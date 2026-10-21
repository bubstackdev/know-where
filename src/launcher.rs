//! Session-scoped harness integration. Saved harness configuration is never edited.
use anyhow::{Context, Result, bail, ensure};
use knowwhere::{
    AppState,
    types::{Model, TIERS},
};
use serde_json::{Value, json};
use std::{
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{process::Command, sync::oneshot, task::JoinHandle};

const SETUP_MODEL: &str = "knowwhere-openrouter";
#[cfg(unix)]
const INTERRUPT_SIGNAL: i32 = libc::SIGINT;
#[cfg(not(unix))]
const INTERRUPT_SIGNAL: i32 = 2;

pub fn validate_setup(args: &[String]) -> Result<Option<String>> {
    match args {
        [] => Ok(None),
        [flag, model] if flag == "--model" => {
            validate_model(model)?;
            Ok(Some(model.clone()))
        }
        [arg] if arg.starts_with("--model=") => {
            let model = arg.trim_start_matches("--model=");
            validate_model(model)?;
            Ok(Some(model.to_owned()))
        }
        _ => bail!("Usage: knowwhere [global options] setup [--model owner/model]"),
    }
}
fn validate_model(model: &str) -> Result<()> {
    ensure!(
        model.len() <= 200
            && model.contains('/')
            && model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._:-".contains(&b)),
        "Use an OpenRouter model ID such as owner/model"
    );
    ensure!(
        !model.starts_with('/') && !model.ends_with('/'),
        "Use an OpenRouter model ID such as owner/model"
    );
    Ok(())
}
pub fn setup(state: &AppState, model: Option<String>) -> Result<()> {
    let model = model.unwrap_or_else(|| "openrouter/auto".into());
    let key = match std::env::var("OPENROUTER_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key.trim().to_owned(),
        _ if io::stdin().is_terminal() => {
            rpassword::prompt_password("OpenRouter API key (hidden): ")?
                .trim()
                .to_owned()
        }
        _ => bail!("Set OPENROUTER_API_KEY for noninteractive setup"),
    };
    ensure!(
        !key.is_empty() && !key.chars().any(char::is_control),
        "OpenRouter API key must be nonempty and contain no control characters"
    );
    let mut catalog = state.store.catalog()?;
    let selected: Model = serde_json::from_value(json!({
        "id": SETUP_MODEL, "provider":"openrouter", "upstreamModelId":model,
        "enabled":true, "supportsTools":true,
        "openRouter":{"maxPromptPrice":1,"maxCompletionPrice":1}
    }))?;
    if let Some(existing) = catalog.models.iter_mut().find(|m| m.id == SETUP_MODEL) {
        ensure!(
            existing.provider == "openrouter",
            "The model ID {SETUP_MODEL} already belongs to another provider; rename it in Settings before setup"
        );
        *existing = selected;
    } else {
        ensure!(
            !catalog
                .models
                .iter()
                .any(|m| m.id.eq_ignore_ascii_case(SETUP_MODEL)),
            "A conflicting model ID already exists; rename it in Settings before setup"
        );
        catalog.models.push(selected);
    }
    // Preserve all other model rows. Setup explicitly selects this provider for
    // every tier so no harness default can accidentally route to a subscription.
    for tier in TIERS {
        catalog.tiers.insert(tier.into(), vec![SETUP_MODEL.into()]);
    }
    catalog.default_tier = "high".into();
    catalog.prices.remove(SETUP_MODEL);
    state
        .store
        .save_catalog_with_secret(&catalog, "openrouter", &key)?;
    state.store.clear_cooldowns("openrouter")?;
    println!(
        "OpenRouter configured: {model}\nProvider price ceiling: $1 per million prompt tokens and $1 per million completion tokens (sent on every request).\nSelected for all four tiers; other model rows retained.\nRun knowwhere run claude or knowwhere run codex.\nThe model/key have been saved locally; no paid request was made."
    );
    Ok(())
}

pub fn validate_run(args: &[String]) -> Result<()> {
    ensure!(
        matches!(args.first().map(String::as_str), Some("claude" | "codex")),
        "Usage: knowwhere [global options] run claude|codex [--] HARNESS_ARGS"
    );
    let forwarded = args[1..]
        .strip_prefix(&["--".to_owned()])
        .unwrap_or(&args[1..]);
    let mut index = 0;
    while index < forwarded.len() {
        let arg = &forwarded[index];
        if arg == "--" {
            break;
        }
        // These modes detach from the process whose lifetime owns the gateway.
        ensure!(
            !matches!(
                arg.as_str(),
                "--background" | "--bg" | "--cloud" | "--remote" | "--oss" | "--local-provider"
            ) && !arg.starts_with("--remote=")
                && !arg.starts_with("--cloud=")
                && !arg.starts_with("--local-provider="),
            "Detached/remote harness modes are not supported by the session launcher"
        );
        if args[0] == "codex" {
            if arg == "-c" || arg == "--config" {
                index += 1;
                let value = forwarded
                    .get(index)
                    .context("Codex --config requires key=value")?;
                validate_codex_config(value)?;
            } else if let Some(value) = arg
                .strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c").filter(|s| !s.is_empty()))
            {
                validate_codex_config(value)?;
            }
        }
        index += 1;
    }
    if args[0] == "claude" {
        claude_settings(forwarded)?;
    }
    Ok(())
}
fn merge_settings(target: &mut Value, source: Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            if let Some(existing) = target.get_mut(key) {
                merge_settings(existing, value.clone());
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = source;
    }
}
fn claude_settings(args: &[String]) -> Result<(Vec<String>, Value)> {
    let (mut forwarded, mut settings) = (Vec::new(), json!({}));
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            forwarded.extend_from_slice(&args[index..]);
            break;
        }
        let supplied = if arg == "--settings" {
            index += 1;
            Some(
                args.get(index)
                    .context("Claude --settings requires a file or JSON object")?
                    .as_str(),
            )
        } else {
            arg.strip_prefix("--settings=")
        };
        if let Some(value) = supplied {
            let parsed = match serde_json::from_str::<Value>(value) {
                Ok(v) => v,
                Err(_) => {
                    let text = std::fs::read_to_string(value)
                        .context("Cannot read Claude --settings file")?;
                    serde_json::from_str(&text)
                        .context("Claude --settings file must contain valid JSON")?
                }
            };
            ensure!(
                parsed.is_object(),
                "Claude --settings must be a JSON object"
            );
            ensure!(
                parsed.get("env").is_none_or(Value::is_object),
                "Claude --settings env must be an object"
            );
            merge_settings(&mut settings, parsed);
        } else {
            forwarded.push(arg.clone());
        }
        index += 1;
    }
    Ok((forwarded, settings))
}
fn validate_codex_config(value: &str) -> Result<()> {
    let (key, _) = value
        .split_once('=')
        .context("Codex --config requires key=value")?;
    // Parse only the key: Codex also accepts unquoted string values. TOML
    // decoding canonicalizes quoted keys, escapes, and whitespace correctly.
    let table = toml::from_str::<toml::Table>(&format!("{key}=false"))
        .map_err(|_| anyhow::anyhow!("Codex --config requires a valid TOML key"))?;
    ensure!(
        !table.keys().any(|key| matches!(
            key.as_str(),
            "model_provider" | "model_providers" | "openai_base_url"
        )),
        "The launcher manages Codex provider settings; change the KnowWhere catalog instead"
    );
    Ok(())
}

struct Gateway {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), std::io::Error>>,
}
impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}
impl Gateway {
    async fn close(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}

struct HarnessCommand {
    command: Command,
    _settings: Option<tempfile::NamedTempFile>,
}
fn command(harness: &str, args: &[String], base: &str, key: &str) -> Result<HarnessCommand> {
    let args = args.strip_prefix(&["--".to_owned()]).unwrap_or(args);
    let mut overlay = None;
    let mut command = Command::new(harness);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command.env_remove("OPENROUTER_API_KEY");
    // Proxy settings must not divert the private gateway connection.
    let no_proxy = [
        std::env::var("NO_PROXY").unwrap_or_default(),
        std::env::var("no_proxy").unwrap_or_default(),
        "localhost,127.0.0.1,::1".into(),
    ]
    .join(",");
    command
        .env("NO_PROXY", &no_proxy)
        .env("no_proxy", &no_proxy);
    if harness == "claude" {
        let (forwarded, mut settings) = claude_settings(args)?;
        let managed = json!({
            "ANTHROPIC_BASE_URL":base,"ANTHROPIC_AUTH_TOKEN":key,"ANTHROPIC_API_KEY":key,
            "ANTHROPIC_MODEL":"knowwhere", "ANTHROPIC_DEFAULT_MODEL":"knowwhere",
            "ANTHROPIC_CUSTOM_MODEL_OPTION":"knowwhere",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME":"KnowWhere",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION":"Uses KnowWhere routing; the upstream model is selected by the gateway",
            "ANTHROPIC_DEFAULT_OPUS_MODEL":"claude-opus-4-6",
            "ANTHROPIC_DEFAULT_FABLE_MODEL":"claude-fable-5-1",
            "ANTHROPIC_DEFAULT_SONNET_MODEL":"claude-sonnet-4-6","ANTHROPIC_DEFAULT_HAIKU_MODEL":"claude-haiku-4-5",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME":"KnowWhere (Fable route)",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME":"KnowWhere (Opus route)",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME":"KnowWhere (Sonnet route)",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME":"KnowWhere (Haiku route)",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION":"Routes through KnowWhere using your catalog aliases",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION":"Routes through KnowWhere using your catalog aliases",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION":"Routes through KnowWhere using your catalog aliases",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION":"Routes through KnowWhere using your catalog aliases",
            "CLAUDE_CODE_USE_BEDROCK":"0","CLAUDE_CODE_USE_VERTEX":"0","CLAUDE_CODE_USE_FOUNDRY":"0",
            "NO_PROXY":no_proxy,"no_proxy":no_proxy,
        });
        merge_settings(&mut settings, json!({"env":managed.clone()}));
        let mut file = tempfile::Builder::new()
            .prefix("knowwhere-claude-")
            .suffix(".json")
            .tempfile()
            .context("Create private session settings")?;
        serde_json::to_writer(file.as_file_mut(), &settings)?;
        file.as_file_mut().flush()?;
        // Put the overlay before a literal '--' so it stays a CLI option while
        // preserving prompts that begin with an option-like string.
        let separator = forwarded
            .iter()
            .position(|a| a == "--")
            .unwrap_or(forwarded.len());
        command
            .args(&forwarded[..separator])
            .arg("--settings")
            .arg(file.path())
            .args(&forwarded[separator..]);
        overlay = Some(file);
        for (name, value) in managed
            .as_object()
            .expect("managed environment is an object")
        {
            command.env(
                name,
                value
                    .as_str()
                    .expect("managed environment values are strings"),
            );
        }
    } else {
        let provider = format!(
            "model_providers.knowwhere={{name=\"KnowWhere\",base_url=\"{base}/v1\",env_key=\"KNOW_WHERE_GATEWAY_KEY\",wire_api=\"responses\",requires_openai_auth=false,supports_websockets=false,capabilities={{remote_compaction=\"unsupported\"}}}}"
        );
        command
            .env("KNOW_WHERE_GATEWAY_KEY", key)
            .env_remove("OPENAI_API_KEY")
            .arg("--no-daemon")
            .args([
                "-c",
                "model_provider=\"knowwhere\"",
                "-c",
                "model=\"knowwhere\"",
                "-c",
                "web_search=\"disabled\"",
                "-c",
                &provider,
            ]);
        command.args(args);
    }
    Ok(HarnessCommand {
        command,
        _settings: overlay,
    })
}

pub async fn run(mut state: AppState, args: &[String]) -> Result<i32> {
    validate_run(args)?;
    let harness = &args[0];
    let catalog = state.store.catalog()?;
    let mut ready = false;
    for model in catalog
        .models
        .iter()
        .filter(|m| m.enabled && catalog.tiers.values().any(|ids| ids.contains(&m.id)))
    {
        ready |= match model.provider.as_str() {
            "local" => true,
            "claude" | "codex" | "openrouter" => {
                state.store.get_secret(&model.provider)?.is_some()
                    || (model.provider == "openrouter"
                        && state
                            .store
                            .get_secret(&format!("model:{}", model.id))?
                            .is_some())
            }
            _ => false,
        };
    }
    ensure!(
        ready,
        "No configured routed provider; run knowwhere setup --model owner/model (or connect a subscription in Settings)"
    );
    // Bind directly: no saved listener settings, certificates, or remote server
    // processes are changed or reused by a session launcher.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    state.config.listen_address = address.ip();
    state.config.port = address.port();
    *state.runtime.listeners.write().await =
        Some(json!({"address":"127.0.0.1","port":address.port(),"httpsPort":null}));
    let key = state.store.gateway_key()?;
    knowwhere::refresh_health(&state).await;
    let app = knowwhere::app(state);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
    });
    let mut gateway = Gateway {
        stop: Some(stop),
        task,
    };
    let base = format!("http://{address}");
    eprintln!("KnowWhere → {harness}; gateway {base} (session only)");
    if let Some(ids) = catalog.tiers.get(&catalog.default_tier) {
        let upstreams = ids
            .iter()
            .filter_map(|id| {
                catalog
                    .models
                    .iter()
                    .find(|model| model.id == *id && model.enabled)
            })
            .map(|model| model.upstream_model_id.as_str())
            .collect::<Vec<_>>();
        eprintln!(
            "KnowWhere default route: {} → {}",
            catalog.default_tier,
            upstreams.join(", ")
        );
    }
    let mut harness_command = command(harness, &args[1..], &base, &key)?;
    let cmd = &mut harness_command.command;
    // Noninteractive processes can have an independent group for reliable tool
    // cleanup. Interactive harnesses must retain the foreground terminal group.
    let own_group = !io::stdin().is_terminal();
    #[cfg(unix)]
    if own_group {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Cannot launch {harness}; install it and ensure it is on PATH"))?;
    let pid = child.id();
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            signal = shutdown_signal() => {
                if !own_group&&signal==INTERRUPT_SIGNAL{
                    // The foreground terminal already delivered Ctrl-C to the
                    // harness. It can cancel a turn and remain interactive.
                    continue;
                }
                #[cfg(unix)]
                if let Some(pid) = pid {
                    // SAFETY: this is our live child PID, or its process group.
                    unsafe { libc::kill(if own_group { -(pid as i32) } else { pid as i32 }, signal); }
                }
                #[cfg(not(unix))]
                let _ = (pid, own_group, signal);
                break match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(status) => status?,
                    Err(_) => {
                        #[cfg(unix)]
                        if own_group && let Some(pid) = pid { unsafe { libc::kill(-(pid as i32), libc::SIGKILL); } }
                        child.kill().await.ok();
                        child.wait().await?
                    }
                }
            }
        }
    };
    #[cfg(unix)]
    if own_group && let Some(pid) = pid {
        // A detached tool must not outlive its noninteractive session.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    gateway.close().await;
    Ok(exit_code(status))
}
async fn shutdown_signal() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install termination handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => libc::SIGINT, _ = term.recv() => libc::SIGTERM }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        2
    }
}
fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(1)
    }
    #[cfg(not(unix))]
    {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn passthrough_preserves_harness_flags_and_keeps_tokens_out_of_arguments() {
        let cmd = command(
            "codex",
            &["exec".into(), "--json".into(), "two words".into()],
            "http://127.0.0.1:32123",
            "fixture-gateway-key",
        )
        .unwrap();
        let args: Vec<_> = cmd.command.as_std().get_args().collect();
        assert_eq!(
            &args[args.len() - 3..],
            &[
                OsStr::new("exec"),
                OsStr::new("--json"),
                OsStr::new("two words")
            ]
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("fixture-gateway-key"))
        );
        let env: Vec<_> = cmd.command.as_std().get_envs().collect();
        assert!(env.contains(&(
            OsStr::new("KNOW_WHERE_GATEWAY_KEY"),
            Some(OsStr::new("fixture-gateway-key"))
        )));
        assert!(env.contains(&(OsStr::new("OPENROUTER_API_KEY"), None)));
        assert!(
            args.iter()
                .any(|arg| arg.to_string_lossy().contains("requires_openai_auth=false"))
        );
    }
    #[test]
    fn claude_overrides_are_process_scoped_and_separator_is_removed() {
        let cmd = command(
            "claude",
            &["--".into(), "-p".into(), "hello".into()],
            "http://127.0.0.1:1",
            "gateway",
        )
        .unwrap();
        let args = cmd.command.as_std().get_args().collect::<Vec<_>>();
        assert_eq!(&args[..2], [OsStr::new("-p"), OsStr::new("hello")]);
        assert_eq!(args[2], OsStr::new("--settings"));
        assert!(std::path::Path::new(args[3]).is_file());
        let env: Vec<_> = cmd.command.as_std().get_envs().collect();
        assert!(env.contains(&(
            OsStr::new("ANTHROPIC_AUTH_TOKEN"),
            Some(OsStr::new("gateway"))
        )));
        assert!(env.contains(&(OsStr::new("ANTHROPIC_API_KEY"), Some(OsStr::new("gateway")))));
    }
    #[test]
    fn private_claude_overlay_preserves_preferences_and_overrides_routing_env() {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        let original = r#"{"permissions":{"allow":["Read"]},"env":{"ANTHROPIC_BASE_URL":"https://elsewhere.invalid","KEEP_USER_PREF":"yes"}}"#;
        source.write_all(original.as_bytes()).unwrap();
        let args=vec!["--settings".into(),source.path().to_string_lossy().into_owned(),"--settings={\"attribution\":{\"commit\":\"keep attribution\"},\"env\":{\"ANTHROPIC_API_KEY\":\"old-secret\"}}".into(),"-p".into(),"hello".into()];
        let cmd = command(
            "claude",
            &args,
            "http://127.0.0.1:1234",
            "private-gateway-key",
        )
        .unwrap();
        let argv = cmd.command.as_std().get_args().collect::<Vec<_>>();
        assert!(
            !argv
                .iter()
                .any(|arg| arg.to_string_lossy().contains("private-gateway-key"))
        );
        let path = std::path::Path::new(argv.last().unwrap());
        let overlay: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(overlay["permissions"]["allow"][0], "Read");
        assert_eq!(overlay["attribution"]["commit"], "keep attribution");
        assert_eq!(overlay["env"]["KEEP_USER_PREF"], "yes");
        assert_eq!(
            overlay["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:1234"
        );
        assert_eq!(overlay["env"]["ANTHROPIC_API_KEY"], "private-gateway-key");
        assert_eq!(overlay["env"]["ANTHROPIC_MODEL"], "knowwhere");
        assert_eq!(overlay["env"]["ANTHROPIC_CUSTOM_MODEL_OPTION"], "knowwhere");
        assert_eq!(
            overlay["env"]["ANTHROPIC_CUSTOM_MODEL_OPTION_NAME"],
            "KnowWhere"
        );
        for (name, value) in overlay["env"].as_object().unwrap() {
            if name == "KEEP_USER_PREF" {
                continue;
            }
            assert!(
                cmd.command
                    .as_std()
                    .get_envs()
                    .any(|(key, env)| key == OsStr::new(name)
                        && env == value.as_str().map(OsStr::new))
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let overlay_path = path.to_owned();
        drop(cmd);
        assert!(!overlay_path.exists());
        assert_eq!(std::fs::read_to_string(source.path()).unwrap(), original);
        for settings in ["[]", "null", r#"{"env":[]}"#] {
            assert!(
                validate_run(&["claude".into(), "--settings".into(), settings.into()]).is_err()
            );
        }
    }
    #[test]
    fn detached_modes_and_provider_bypass_are_rejected() {
        for args in [
            ["claude", "--bg"],
            ["codex", "--remote=ws://example"],
            ["codex", "-cmodel_provider=other"],
        ] {
            assert!(validate_run(&args.map(String::from)).is_err());
        }
        assert!(validate_run(&["codex".into(), "exec".into(), "--json".into()]).is_ok());
    }
    #[test]
    fn provider_config_keys_are_checked_only_in_actual_config_options() {
        for option in [
            "model_provider=other",
            "model_provider\t = \"other\"",
            "\"model_provider\"=\"other\"",
            "'model_providers'.'knowwhere'.base_url=\"https://elsewhere.invalid\"",
            "\"\\u006dodel_provider\"=\"other\"",
            "openai_base_url = \"https://elsewhere.invalid\"",
        ] {
            for form in [
                vec!["-c".into(), option.into()],
                vec!["--config".into(), option.into()],
                vec![format!("-c{option}")],
                vec![format!("--config={option}")],
            ] {
                let mut args = vec!["codex".into(), "exec".into()];
                args.extend(form);
                assert!(validate_run(&args).is_err(), "{args:?}");
            }
        }
        for args in [
            vec!["codex", "exec", "model_provider=other"],
            vec!["codex", "exec", "--", "-cmodel_provider=other"],
            vec!["codex", "--", "exec", "-c", "reasoning_effort=high"],
            vec!["claude", "--", "-p", "--", "--bg"],
        ] {
            assert!(validate_run(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_ok());
        }
    }
    #[test]
    fn setup_rejects_secrets_as_arguments_and_invalid_model_ids() {
        assert!(validate_setup(&["--key=secret".into()]).is_err());
        assert!(validate_setup(&["--model".into(), "vendor/model\nsecret".into()]).is_err());
        assert_eq!(
            validate_setup(&["--model=vendor/model:free".into()]).unwrap(),
            Some("vendor/model:free".into())
        );
    }
}

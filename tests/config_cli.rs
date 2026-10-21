//! Process-level CLI and startup regressions. All credentials and databases are fixtures.
use knowwhere::types::{Catalog, Pin};
use knowwhere::{AppState, Config, Store};
use serde_json::{Value, json};
#[cfg(unix)]
use std::path::Path;
use std::{
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Fixture {
    directory: tempfile::TempDir,
    config: Config,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let seed = directory.path().join("seed.json");
        std::fs::write(&seed, seed_value().to_string()).unwrap();
        let config = Config {
            data_dir: directory.path().join("data"),
            seed_file: Some(seed),
            ..Config::default()
        };
        Self { directory, config }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_knowwhere"));
        command
            .env_clear()
            .args(args)
            .arg("--data-dir")
            .arg(&self.config.data_dir)
            .arg("--seed-file")
            .arg(self.config.seed_file.as_ref().unwrap());
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn override_file(&self, value: &Value) {
        std::fs::create_dir_all(&self.config.data_dir).unwrap();
        std::fs::write(
            self.config.data_dir.join("seed.local.json"),
            value.to_string(),
        )
        .unwrap();
    }
}
fn seed_value() -> Value {
    json!({"defaultTier":"high","models":[{"id":"local","provider":"local","upstreamModelId":"original","baseUrl":"http://127.0.0.1:1/v1","enabled":false,"contextWindow":8192,"supportsTools":true,"apiKey":"seed-key"}],"tiers":{"high":["local"]},"aliases":[]})
}
fn key(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find(|s| s.starts_with("kw_"))
        .unwrap()
        .to_owned()
}
#[cfg(unix)]
fn catalog(config: &Config) -> Catalog {
    Store::open(&config.data_dir.join("knowwhere.db"))
        .unwrap()
        .catalog()
        .unwrap()
}

#[test]
fn cli_gateway_key_survives_processes_and_rotation_replaces_it() {
    let f = Fixture::new();
    let first = key(f.run(&["key", "show"]));
    assert_eq!(first, key(f.run(&["key", "show"])));
    let rotated = key(f.run(&["key", "rotate"]));
    assert_ne!(first, rotated);
    assert_eq!(rotated, key(f.run(&["key", "show"])));
    assert_eq!(
        Store::open(&f.config.data_dir.join("knowwhere.db"))
            .unwrap()
            .gateway_key()
            .unwrap(),
        rotated
    );
}

#[test]
fn cli_invalid_commands_and_options_fail_without_initializing_a_database() {
    for args in [
        &["unknown"][..],
        &["key", "missing"][..],
        &["--unknown-option=value"][..],
        &["key", "show", "--port=-1"][..],
    ] {
        let f = Fixture::new();
        let output = f.run(args);
        assert!(!output.status.success(), "unexpected success for {args:?}");
        assert!(
            !f.config.data_dir.exists(),
            "invalid command initialized data: {args:?}"
        );
    }
}

#[test]
fn cli_help_and_version_do_not_require_seed_or_data_files() {
    for args in [&["--help"][..], &["--version"][..]] {
        let f = Fixture::new();
        std::fs::remove_file(f.config.seed_file.as_ref().unwrap()).unwrap();
        let output = f.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!f.config.data_dir.exists());
    }
}

#[test]
fn command_line_wins_over_environment_even_when_the_shadowed_value_is_invalid() {
    let f = Fixture::new();
    let ignored = f.directory.path().join("environment-data");
    let mut command = f.command(&["key", "show", "--port=0"]);
    command
        .env("SIMPLEROUTER__PORT", "not-a-number")
        .env("KnowWhere__DataDirectory", &ignored)
        .env("KNOW_WHERE__Port", "also-invalid");
    key(command.output().unwrap());
    assert!(f.config.data_dir.exists());
    assert!(!ignored.exists());
}

#[test]
fn environment_prefix_precedence_and_case_insensitivity_match_documented_aliases() {
    let f = Fixture::new();
    let one = f.directory.path().join("first");
    let two = f.directory.path().join("second");
    let three = f.directory.path().join("third");
    let output = Command::new(env!("CARGO_BIN_EXE_knowwhere"))
        .env_clear()
        .args(["key", "show"])
        .arg("--seed-file")
        .arg(f.config.seed_file.as_ref().unwrap())
        .env("SIMPLEROUTER__DATADIRECTORY", &one)
        .env("knowwhere__datadirectory", &two)
        .env("KNOW_WHERE__DataDirectory", &three)
        .output()
        .unwrap();
    key(output);
    assert!(!one.exists());
    assert!(!two.exists());
    assert!(three.join("knowwhere.db").exists());
}

#[test]
fn default_data_directory_uses_userprofile_when_home_is_absent() {
    let f = Fixture::new();
    let profile = f.directory.path().join("windows-user");
    let output = Command::new(env!("CARGO_BIN_EXE_knowwhere"))
        .env_clear()
        .env("USERPROFILE", &profile)
        .args(["key", "show"])
        .arg("--seed-file")
        .arg(f.config.seed_file.as_ref().unwrap())
        .output()
        .unwrap();
    key(output);
    assert!(profile.join(".knowwhere/knowwhere.db").exists());
    assert!(!f.config.data_dir.exists());
}

#[test]
fn reference_defaults_and_invalid_configuration_are_checked_without_panics() {
    let c = Config::default();
    assert_eq!(c.health_interval, Duration::from_secs(300));
    assert_eq!(c.port, 8790);
    assert_eq!(c.https_port, 8791);
    let mut c = c;
    c.set("listen-address", " 127.0.0.1 ").unwrap();
    assert!(c.listen_address.is_loopback());
    for (field, value) in [
        ("listen-address", "192.168.1"),
        ("listen-address", "example.com"),
        ("port", "65536"),
        ("Local:StreamIdleTimeout", "00:00:NaN"),
        (
            "Local:StreamIdleTimeout",
            "999999999999999999999999999999:00:00",
        ),
    ] {
        assert!(
            c.set(field, value).is_err(),
            "accepted invalid {field}={value}"
        );
    }
    c.set("Local:StreamIdleTimeout", " 00:00:00.5 ").unwrap();
    assert_eq!(c.local_stream_idle, Duration::from_millis(500));
    c.set("Local:StreamIdleTimeout", "1.02:03:04.5").unwrap();
    assert_eq!(c.local_stream_idle, Duration::from_millis(93_784_500));
    assert!(c.set("Local:StreamIdleTimeout", "00:90:00").is_err());
}

#[tokio::test]
async fn unchanged_override_preserves_settings_edits_changed_content_reapplies() {
    let f = Fixture::new();
    f.override_file(&json!({"models":[{"id":"local","baseUrl":"http://127.0.0.1:2/v1","enabled":true,"apiKey":"override-key"}]}));
    let first = AppState::open(f.config.clone()).unwrap();
    let original_hash = first
        .store
        .get_setting("seed_override_hash")
        .unwrap()
        .unwrap();
    let mut edited = first.store.catalog().unwrap();
    edited.models[0].base_url = Some("http://127.0.0.1:3/v1".into());
    first.store.save_catalog(&edited).unwrap();
    drop(first);
    let second = AppState::open(f.config.clone()).unwrap();
    assert_eq!(
        second.store.catalog().unwrap().models[0]
            .base_url
            .as_deref(),
        Some("http://127.0.0.1:3/v1")
    );
    assert_eq!(
        second.store.get_secret("model:local").unwrap().as_deref(),
        Some("override-key")
    );
    drop(second);
    f.override_file(&json!({"models":[{"id":"local","baseUrl":"http://127.0.0.1:4/v1","apiKey":"changed-key"}]}));
    let third = AppState::open(f.config.clone()).unwrap();
    assert_eq!(
        third.store.catalog().unwrap().models[0].base_url.as_deref(),
        Some("http://127.0.0.1:4/v1")
    );
    assert_ne!(
        third
            .store
            .get_setting("seed_override_hash")
            .unwrap()
            .as_deref(),
        Some(original_hash.as_str())
    );
    assert_eq!(
        third.store.get_secret("model:local").unwrap().as_deref(),
        Some("changed-key")
    );
}

#[tokio::test]
async fn renamed_models_are_not_recreated_by_changed_overrides_for_old_ids() {
    let f = Fixture::new();
    f.override_file(&json!({"models":[{"id":"local","baseUrl":"http://127.0.0.1:2/v1"}]}));
    let first = AppState::open(f.config.clone()).unwrap();
    first
        .store
        .assign_pin(
            &Pin {
                session_key: "pin".into(),
                tier: "high".into(),
                model_id: "local".into(),
                client_session_id: None,
                created_at: 1,
            },
            true,
        )
        .unwrap();
    first.store.rename_model("local", "renamed").unwrap();
    drop(first);
    f.override_file(
        &json!({"models":[{"id":"local","enabled":true},{"id":"unknown","enabled":true}]}),
    );
    let second = AppState::open(f.config.clone()).unwrap();
    let c = second.store.catalog().unwrap();
    assert_eq!(c.models.len(), 1);
    assert_eq!(c.models[0].id, "renamed");
    assert!(!c.models[0].enabled);
    assert_eq!(
        c.models[0].base_url.as_deref(),
        Some("http://127.0.0.1:2/v1")
    );
    assert_eq!(second.store.pins().unwrap()[0].model_id, "renamed");
    assert_eq!(
        second.store.get_secret("model:renamed").unwrap().as_deref(),
        Some("seed-key")
    );
}

#[tokio::test]
async fn null_override_fields_preserve_values_and_unknown_fields_do_not_change_provider() {
    let f = Fixture::new();
    f.override_file(&json!({"models":[{"id":"local","apiKey":null,"baseUrl":null,"contextWindow":null,"supportsTools":null,"effort":null,"provider":"codex","unknown":true}]}));
    let state = AppState::open(f.config.clone()).unwrap();
    let c = state.store.catalog().unwrap();
    assert_eq!(c.models[0].provider, "local");
    assert_eq!(c.models[0].context_window, Some(8192));
    assert!(c.models[0].supports_tools);
    assert_eq!(
        state.store.get_secret("model:local").unwrap().as_deref(),
        Some("seed-key")
    );
}

#[tokio::test]
async fn bom_prefixed_seed_and_override_are_accepted() {
    let f = Fixture::new();
    std::fs::write(
        f.config.seed_file.as_ref().unwrap(),
        format!("\u{feff}{}", seed_value()),
    )
    .unwrap();
    f.override_file(&json!({"models":[]}));
    std::fs::write(
        f.config.data_dir.join("seed.local.json"),
        "\u{feff}{\"models\":[{\"id\":\"local\",\"enabled\":true}]}",
    )
    .unwrap();
    let state = AppState::open(f.config.clone()).unwrap();
    assert!(state.store.catalog().unwrap().models[0].enabled);
}

#[tokio::test]
async fn invalid_override_does_not_replace_catalog_secret_or_applied_content_hash() {
    let f = Fixture::new();
    f.override_file(&json!({"models":[{"id":"local","apiKey":"good-key"}]}));
    let first = AppState::open(f.config.clone()).unwrap();
    let hash = first.store.get_setting("seed_override_hash").unwrap();
    drop(first);
    f.override_file(
        &json!({"models":[{"id":"local","contextWindow":0,"apiKey":"must-not-write"}]}),
    );
    assert!(AppState::open(f.config.clone()).is_err());
    let store = Store::open(&f.config.data_dir.join("knowwhere.db")).unwrap();
    assert_eq!(store.get_setting("seed_override_hash").unwrap(), hash);
    assert_eq!(
        store.get_secret("model:local").unwrap().as_deref(),
        Some("good-key")
    );
    assert_eq!(
        store.catalog().unwrap().models[0].context_window,
        Some(8192)
    );
}

#[test]
fn catalog_context_tool_and_effort_validation_rejects_invalid_seed_shapes() {
    for effort in [
        json!({"mode":"unknown"}),
        json!({"levels":"high"}),
        json!({"levels":["huge"]}),
        json!({"default":"huge"}),
        json!({"mode":"chat_template_kwargs","levels":["low","high"],"kwargs":{"low":{}}}),
        json!({"mode":"chat_template_kwargs","levels":["high"],"kwargs":{"high":"not-object"}}),
    ] {
        let mut seed = seed_value();
        seed["models"][0]["effort"] = effort.clone();
        let c: Catalog = serde_json::from_value(seed).unwrap();
        assert!(
            knowwhere::routing::validate_catalog(&c).is_err(),
            "accepted {effort}"
        );
    }
    for field in ["contextWindow", "maxPromptTokens", "maxConversations"] {
        let mut seed = seed_value();
        seed["models"][0][field] = json!(0);
        let c: Catalog = serde_json::from_value(seed).unwrap();
        assert!(
            knowwhere::routing::validate_catalog(&c).is_err(),
            "accepted zero {field}"
        );
    }
    let mut valid = seed_value();
    valid["models"][0]["supportsTools"] = json!(false);
    valid["models"][0]["effort"] = json!({"mode":"chat_template_kwargs","levels":["low","high"],"kwargs":{"low":{"thinking":false},"high":{"thinking":true}},"default":"low"});
    let c: Catalog = serde_json::from_value(valid).unwrap();
    knowwhere::routing::validate_catalog(&c).unwrap();
    assert!(!c.models[0].supports_tools);
}

#[tokio::test]
async fn zero_refresh_interval_never_contacts_even_configured_logged_in_providers() {
    use axum::{Json, Router, routing::get};
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(get(move || {
                let recorded = recorded.clone();
                async move {
                    recorded.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"data":[]}))
                }
            })),
        )
        .await
        .unwrap()
    });
    let f = Fixture::new();
    let mut config = f.config.clone();
    config.model_refresh_interval = Duration::ZERO;
    config.health_interval = Duration::ZERO;
    config.openrouter_api_base = format!("http://{address}/v1");
    let state = AppState::open(config).unwrap();
    state.store.set_secret("openrouter", "fixture-key").unwrap();
    knowwhere::start_background(state);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[cfg(unix)]
#[test]
fn cli_existing_world_readable_data_is_restricted() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    std::fs::create_dir_all(&f.config.data_dir).unwrap();
    std::fs::set_permissions(&f.config.data_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    key(f.run(&["key", "show"]));
    for (path, mode) in [
        (f.config.data_dir.as_path(), 0o700),
        (Path::new(&f.config.data_dir.join("knowwhere.db")), 0o600),
    ] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    assert_eq!(catalog(&f.config).models.len(), 1);
}

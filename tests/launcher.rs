//! Reversibility and failure cleanup at the executable boundary; fixture keys only.
use knowwhere::{Store, types::TIERS};
use std::{path::Path, process::Command};

fn command(data: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knowwhere"));
    cmd.env_remove("OPENROUTER_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("OPENAI_API_KEY")
        .arg("--data-dir")
        .arg(data);
    cmd
}
fn setup(data: &Path, model: &str) {
    let output = command(data)
        .args(["setup", "--model", model])
        .env("OPENROUTER_API_KEY", "fixture-upstream-key")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-upstream-key"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-upstream-key"));
}

#[test]
fn setup_encrypts_key_selects_all_tiers_and_can_be_repeated() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    setup(&data, "vendor/first");
    setup(&data, "vendor/second");
    let store = Store::open(&data.join("knowwhere.db")).unwrap();
    assert_eq!(
        store.get_secret("openrouter").unwrap().as_deref(),
        Some("fixture-upstream-key")
    );
    let catalog = store.catalog().unwrap();
    for tier in TIERS {
        assert_eq!(catalog.tiers[tier], ["knowwhere-openrouter"]);
    }
    let models: Vec<_> = catalog
        .models
        .iter()
        .filter(|m| m.id == "knowwhere-openrouter")
        .collect();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].upstream_model_id, "vendor/second");
    assert!(
        catalog.models.len() > 1,
        "setup preserves previous model rows"
    );
    assert_eq!(
        models[0].open_router.as_ref().unwrap(),
        &serde_json::json!({"maxPromptPrice":1,"maxCompletionPrice":1})
    );
    assert!(models[0].api_key.is_none());
    let bytes = std::fs::read(data.join("knowwhere.db")).unwrap();
    assert!(
        !bytes
            .windows(b"fixture-upstream-key".len())
            .any(|b| b == b"fixture-upstream-key")
    );
}

#[test]
fn setup_does_not_replace_a_reserved_id_owned_by_another_provider() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    setup(&data, "vendor/original");
    let store = Store::open(&data.join("knowwhere.db")).unwrap();
    let mut catalog = store.catalog().unwrap();
    let model = catalog
        .models
        .iter_mut()
        .find(|m| m.id == "knowwhere-openrouter")
        .unwrap();
    model.provider = "local".into();
    model.base_url = Some("http://127.0.0.1:9/v1".into());
    model.upstream_model_id = "user-local-model".into();
    store.save_catalog(&catalog).unwrap();
    let output = command(&data)
        .args(["setup", "--model", "vendor/replacement"])
        .env("OPENROUTER_API_KEY", "replacement-fixture-key")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already belongs to another provider")
    );
    assert_eq!(
        serde_json::to_value(store.catalog().unwrap()).unwrap(),
        serde_json::to_value(catalog).unwrap()
    );
    assert_eq!(
        store.get_secret("openrouter").unwrap().as_deref(),
        Some("fixture-upstream-key")
    );
}

#[test]
fn invalid_setup_and_launcher_arguments_fail_before_database_creation() {
    for args in [
        &["setup", "--model", "invalid"][..],
        &["setup", "--key=secret"][..],
        &["run", "unknown"][..],
        &["run", "claude", "--bg"][..],
        &["run", "claude", "--settings", "[]"][..],
        &["run", "claude", "--settings", "{\"env\":[]}"][..],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let output = command(&data).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(!data.exists());
    }
}

#[test]
fn unconfigured_launcher_explains_setup_instead_of_starting_a_harness() {
    let temp = tempfile::tempdir().unwrap();
    let output = command(&temp.path().join("data"))
        .args(["run", "claude"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("run knowwhere setup"));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        net::{SocketAddr, TcpStream},
        os::unix::fs::PermissionsExt,
        process::Stdio,
        time::{Duration, Instant},
    };

    fn harness(path: &Path, script: &str) {
        std::fs::write(path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn address(file: &Path) -> SocketAddr {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(file)
                && let Ok(address) = text.trim().trim_start_matches("http://").parse()
            {
                return address;
            }
            assert!(
                Instant::now() < end,
                "harness did not report a gateway address"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn child_exit_code_is_preserved_gateway_closes_and_settings_are_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        setup(&data, "vendor/model");
        let settings = temp.path().join("settings.json");
        std::fs::write(&settings, "sentinel saved harness settings").unwrap();
        harness(
            &temp.path().join("claude"),
            r#"
test "$ANTHROPIC_API_KEY" = "$ANTHROPIC_AUTH_TOKEN"
test "${OPENROUTER_API_KEY-unset}" = unset
test "$ANTHROPIC_API_KEY" != real-harness-key
printf '%s' "$ANTHROPIC_BASE_URL" > "$FIXTURE_ADDRESS"
printf '%s' "$*" > "$FIXTURE_ARGS"
exit 23
"#,
        );
        let file = temp.path().join("address");
        let args = temp.path().join("args");
        let output = command(&data)
            .env("PATH", temp.path())
            .env("FIXTURE_ADDRESS", &file)
            .env("FIXTURE_ARGS", &args)
            .env("OPENROUTER_API_KEY", "fixture-upstream-key")
            .env("ANTHROPIC_API_KEY", "real-harness-key")
            .args(["run", "claude", "--", "-p", "two words"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(23),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::fs::read_to_string(args)
                .unwrap()
                .starts_with("-p two words --settings ")
        );
        assert!(TcpStream::connect_timeout(&address(&file), Duration::from_millis(100)).is_err());
        assert_eq!(
            std::fs::read_to_string(settings).unwrap(),
            "sentinel saved harness settings"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-upstream-key"));
    }
    #[test]
    fn sigterm_stops_harness_tools_and_private_listener() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        setup(&data, "vendor/model");
        harness(
            &temp.path().join("claude"),
            r#"
printf '%s' "$ANTHROPIC_BASE_URL" > "$FIXTURE_ADDRESS"
/bin/sleep 60 &
printf '%s' "$!" > "$FIXTURE_TOOL_PID"
wait
"#,
        );
        let file = temp.path().join("address");
        let pid_file = temp.path().join("tool-pid");
        let mut child = command(&data)
            .env("PATH", temp.path())
            .env("FIXTURE_ADDRESS", &file)
            .env("FIXTURE_TOOL_PID", &pid_file)
            .args(["run", "claude"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let address = address(&file);
        assert!(TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_ok());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pid_file.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        let tool_pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("launcher did not stop after SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
        // A killed orphan can briefly remain a zombie until its parent reaps it;
        // it cannot execute or retain a gateway connection.
        let result = unsafe { libc::kill(tool_pid, 0) };
        if result == 0 {
            let output = Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &tool_pid.to_string()])
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z'),
                "tool remained alive"
            );
        }
    }
    #[test]
    fn interactive_ctrl_c_leaves_a_cancelled_harness_and_gateway_running() {
        use std::{
            fs::File,
            os::{fd::FromRawFd, unix::process::CommandExt},
        };
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        setup(&data, "vendor/model");
        harness(
            &temp.path().join("claude"),
            r#"
trap 'printf cancelled > "$FIXTURE_CANCELLED"' INT
printf '%s' "$ANTHROPIC_BASE_URL" > "$FIXTURE_ADDRESS"
while :; do /bin/sleep 1 || :; done
"#,
        );
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let _master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let address_file = temp.path().join("address");
        let cancelled = temp.path().join("cancelled");
        let mut command = command(&data);
        command
            .env("PATH", temp.path())
            .env("FIXTURE_ADDRESS", &address_file)
            .env("FIXTURE_CANCELLED", &cancelled)
            .args(["run", "claude"])
            .stdin(Stdio::from(slave))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        struct Cleanup(u32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(-(self.0 as i32), libc::SIGKILL);
                }
            }
        }
        let _cleanup = Cleanup(child.id());
        let address = address(&address_file);
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGINT);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cancelled.exists() {
            assert!(
                Instant::now() < deadline,
                "foreground harness did not receive Ctrl-C"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // The former launcher killed an otherwise healthy interactive child
        // after its five-second shutdown timer elapsed.
        std::thread::sleep(Duration::from_millis(5300));
        assert!(
            child.try_wait().unwrap().is_none(),
            "Ctrl-C must cancel the harness turn without ending its session"
        );
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok());
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        while child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "SIGTERM did not shut down interactive launcher"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
    }
}

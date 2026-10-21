//! Real TLS handshakes and certificate interoperability. No system trust is changed.
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use knowwhere::{AppState, Config, tls};
use serde_json::Value;
use std::{
    io::{BufRead, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tower::ServiceExt;

fn setup() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let c = Config {
        data_dir: dir.path().into(),
        listen_address: "127.0.0.1".parse().unwrap(),
        ..Config::default()
    };
    let state = AppState::open(c).unwrap();
    (dir, state)
}
struct Server {
    address: SocketAddr,
    handle: axum_server::Handle,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.handle.shutdown();
        self.task.abort();
    }
}
async fn serve(state: AppState) -> Server {
    let config = tls::load(&state).await.unwrap().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = axum_server::Handle::new();
    let h = handle.clone();
    let task = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, config)
            .handle(h)
            .serve(
                knowwhere::app(state)
                    .layer(axum::Extension(knowwhere::Https))
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
    });
    let address = tokio::time::timeout(Duration::from_secs(5), handle.listening())
        .await
        .unwrap()
        .unwrap();
    Server {
        address,
        handle,
        task,
    }
}
fn client(ca: &[u8], name: &str, address: SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .resolve(name, address)
        .add_root_certificate(reqwest::Certificate::from_pem(ca).unwrap())
        .build()
        .unwrap()
}
fn cert_path(state: &AppState, name: &str) -> PathBuf {
    state.config.data_dir.join("certs").join(name)
}

#[tokio::test]
async fn trusted_generated_ca_serves_https_and_still_requires_gateway_key() {
    let (_dir, s) = setup();
    tls::create(&s).unwrap();
    let ca = std::fs::read(cert_path(&s, "router.crt")).unwrap();
    let key = s.store.gateway_key().unwrap();
    let server = serve(s).await;
    for name in ["localhost", "127.0.0.1"] {
        let c = client(&ca, name, server.address);
        let url = format!("https://{name}:{}/v1/models", server.address.port());
        assert_eq!(
            c.get(&url).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let response = c.get(&url).header("x-api-key", &key).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.json::<Value>().await.unwrap()["data"].is_array());
    }
    let c = client(&ca, "localhost", server.address);
    assert_eq!(
        c.get(format!("https://localhost:{}/ui", server.address.port()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn untrusted_authority_and_unlisted_dns_name_fail_the_handshake() {
    let (_dir, s) = setup();
    tls::create(&s).unwrap();
    let ca = std::fs::read(cert_path(&s, "router.crt")).unwrap();
    let (_other_dir, other) = setup();
    tls::create(&other).unwrap();
    let other_ca = std::fs::read(cert_path(&other, "router.crt")).unwrap();
    let server = serve(s).await;
    let wrong_ca = client(&other_ca, "localhost", server.address);
    assert!(
        wrong_ca
            .get(format!(
                "https://localhost:{}/v1/models",
                server.address.port()
            ))
            .send()
            .await
            .is_err()
    );
    let wrong_name = client(&ca, "unlisted.invalid", server.address);
    assert!(
        wrong_name
            .get(format!(
                "https://unlisted.invalid:{}/v1/models",
                server.address.port()
            ))
            .send()
            .await
            .is_err()
    );
}

fn supplied_pem(path: &Path) -> Vec<u8> {
    let mut p =
        rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    p.is_ca = rcgen::IsCa::ExplicitNoCa;
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = p.self_signed(&key).unwrap();
    std::fs::write(path, format!("{}{}", cert.pem(), key.serialize_pem())).unwrap();
    cert.pem().into_bytes()
}

#[tokio::test]
async fn supplied_combined_pem_overrides_the_generated_identity() {
    let (dir, mut s) = setup();
    tls::create(&s).unwrap();
    let generated = std::fs::read(cert_path(&s, "router.crt")).unwrap();
    let path = dir.path().join("provided.pem");
    let supplied = supplied_pem(&path);
    s.config.certificate_path = Some(path);
    tls::load(&s).await.unwrap().unwrap();
    let active: Value =
        serde_json::from_str(&s.store.get_setting("tls:active_metadata").unwrap().unwrap())
            .unwrap();
    assert_eq!(active["source"], "supplied");
    assert_eq!(
        active["names"],
        serde_json::json!(["localhost", "127.0.0.1"])
    );
    assert!(active["expiresAt"].as_i64().unwrap() > knowwhere::types::now());
    let key = s.store.gateway_key().unwrap();
    let server = serve(s).await;
    let c = client(&supplied, "localhost", server.address);
    assert_eq!(
        c.get(format!(
            "https://localhost:{}/v1/models",
            server.address.port()
        ))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
    let c = client(&generated, "localhost", server.address);
    assert!(
        c.get(format!(
            "https://localhost:{}/v1/models",
            server.address.port()
        ))
        .send()
        .await
        .is_err()
    );
}

fn openssl_available() -> bool {
    Command::new("openssl")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}
fn openssl(args: &[&str]) -> String {
    let out = Command::new("openssl").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "OpenSSL failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn supplied_password_protected_pkcs12_is_imported_and_wrong_password_is_sanitized() {
    if !openssl_available() {
        eprintln!("Skipping PKCS#12 interoperability: openssl is not installed.");
        return;
    }
    let (dir, mut s) = setup();
    let pem_path = dir.path().join("provided.pem");
    let ca = supplied_pem(&pem_path);
    let p12 = dir.path().join("provided.p12");
    let password = "qa-certificate-passphrase";
    let out = Command::new("openssl")
        .args(["pkcs12", "-export", "-in"])
        .arg(&pem_path)
        .arg("-out")
        .arg(&p12)
        .args(["-passout", "env:KNOW_WHERE_TLS_TEST_PASSWORD"])
        .env("KNOW_WHERE_TLS_TEST_PASSWORD", password)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "OpenSSL could not export the test fixture"
    );
    s.config.certificate_path = Some(p12);
    s.config.certificate_password = Some("incorrect-secret-password".into());
    let error = tls::load(&s)
        .await
        .expect_err("wrong password must fail")
        .to_string();
    assert!(!error.contains("incorrect-secret-password"));
    assert!(error.contains("PKCS#12"));
    s.config.certificate_password = Some(password.into());
    let server = serve(s).await;
    let c = client(&ca, "localhost", server.address);
    assert_eq!(
        c.get(format!(
            "https://localhost:{}/v1/models",
            server.address.port()
        ))
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn exported_ca_has_constrained_signing_purpose_and_private_bundle_matches_only_leaf() {
    if !openssl_available() {
        eprintln!("Skipping certificate inspection: openssl is not installed.");
        return;
    }
    let (dir, s) = setup();
    let metadata = tls::create(&s).unwrap();
    let ca = cert_path(&s, "router.crt");
    let leaf = cert_path(&s, "router-leaf.crt");
    let ca_text = openssl(&["x509", "-in", ca.to_str().unwrap(), "-text", "-noout"]);
    assert!(ca_text.contains("CA:TRUE, pathlen:0"));
    assert!(ca_text.contains("Name Constraints: critical"));
    assert!(ca_text.contains("DNS:localhost"));
    assert!(ca_text.contains("DNS:.local"));
    assert!(ca_text.contains("Certificate Sign, CRL Sign"));
    let leaf_text = openssl(&["x509", "-in", leaf.to_str().unwrap(), "-text", "-noout"]);
    assert!(leaf_text.contains("CA:FALSE"));
    assert!(leaf_text.contains("TLS Web Server Authentication"));
    assert!(leaf_text.contains("DNS:localhost"));
    assert!(leaf_text.contains("IP Address:127.0.0.1"));
    assert!(
        openssl(&[
            "verify",
            "-CAfile",
            ca.to_str().unwrap(),
            leaf.to_str().unwrap()
        ])
        .contains(": OK")
    );
    let remaining = metadata["expiresAt"].as_i64().unwrap() - knowwhere::types::now();
    assert!((390 * 86400..=398 * 86400).contains(&remaining));
    let bundle: Value =
        serde_json::from_str(&s.store.get_secret("tls:bundle").unwrap().unwrap()).unwrap();
    let key = bundle["key"].as_str().unwrap();
    assert_eq!(key.matches("BEGIN PRIVATE KEY").count(), 1);
    let mut cmd = Command::new("openssl")
        .args(["pkey", "-pubout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    cmd.stdin.take().unwrap().write_all(key.as_bytes()).unwrap();
    let public = cmd.wait_with_output().unwrap();
    assert!(public.status.success());
    let leaf_public = openssl(&["x509", "-in", leaf.to_str().unwrap(), "-pubkey", "-noout"]);
    let ca_public = openssl(&["x509", "-in", ca.to_str().unwrap(), "-pubkey", "-noout"]);
    assert_eq!(String::from_utf8(public.stdout).unwrap(), leaf_public);
    assert_ne!(leaf_public, ca_public);
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let data = std::fs::read(path).unwrap();
            assert!(
                !data
                    .windows(b"PRIVATE KEY".len())
                    .any(|w| w == b"PRIVATE KEY")
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("certs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn cli_ephemeral_ports_are_reported_in_snapshot_and_certificate_lan_url() {
    let (dir, s) = setup();
    tls::create(&s).unwrap();
    drop(s);
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_knowwhere"))
            .arg("--data-dir")
            .arg(dir.path())
            .args([
                "--listen-address",
                "0.0.0.0",
                "--port",
                "0",
                "--https-port",
                "0",
                "--openrouter-api-base-url",
                "http://127.0.0.1:9",
                "--model-list-refresh-interval",
                "0",
            ])
            .env("RUST_LOG", "knowwhere=info")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let addresses = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let (mut http, mut https) = (None, None);
        while std::time::Instant::now() < deadline && (http.is_none() || https.is_none()) {
            if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
                let address = line
                    .split("address=")
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse::<SocketAddr>().ok());
                if line.contains("Listening on HTTP ") {
                    http = address;
                }
                if line.contains("Listening on HTTPS") {
                    https = address;
                }
            }
        }
        (http, https)
    })
    .await
    .unwrap();
    let http = addresses
        .0
        .expect("HTTP startup log must include its actual socket address");
    let https = addresses
        .1
        .expect("HTTPS startup log must include its actual socket address");
    assert!(http.ip().is_loopback());
    assert_eq!(https.ip(), "0.0.0.0".parse::<std::net::IpAddr>().unwrap());
    assert_ne!(http.port(), 0);
    assert_ne!(https.port(), 0);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("http://{http}/ui/api/snapshot");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let snapshot = loop {
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            break response.json::<Value>().await.unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "HTTP snapshot must become available after startup"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(snapshot["listening"]["address"], "127.0.0.1");
    assert_eq!(snapshot["listening"]["port"], http.port());
    assert_eq!(snapshot["listening"]["httpsPort"], https.port());
    let lan = url::Url::parse(
        snapshot["tls"]["lanUrl"]
            .as_str()
            .expect("active certificate must expose its LAN URL"),
    )
    .unwrap();
    assert_eq!(lan.scheme(), "https");
    assert_eq!(lan.port(), Some(https.port()));
    let _ = child.0.kill();
    let _ = child.0.wait();
    reader.join().unwrap();
}
#[test]
fn cli_with_certificate_keeps_plain_http_on_loopback() {
    let (dir, s) = setup();
    tls::create(&s).unwrap();
    drop(s);
    let mut command = Command::new(env!("CARGO_BIN_EXE_knowwhere"));
    command
        .arg("--data-dir")
        .arg(dir.path())
        .args([
            "--listen-address",
            "0.0.0.0",
            "--port",
            "0",
            "--https-port",
            "0",
            "--openrouter-api-base-url",
            "http://127.0.0.1:9",
            "--model-list-refresh-interval",
            "0",
        ])
        .env("RUST_LOG", "knowwhere=info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard(command.spawn().unwrap());
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut http = false;
    let mut https = false;
    while std::time::Instant::now() < deadline && !(http && https) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            if line.contains("Listening on HTTP ") {
                assert!(
                    line.contains("127.0.0.1:"),
                    "plain HTTP was not loopback-bound: {line}"
                );
                http = true;
            }
            if line.contains("Listening on HTTPS") {
                assert!(line.contains("0.0.0.0:"));
                https = true;
            }
        } else if child.0.try_wait().unwrap().is_some() {
            break;
        }
    }
    let _ = child.0.kill();
    let _ = child.0.wait();
    drop(rx);
    reader.join().unwrap();
    assert!(http && https, "both HTTP and HTTPS listeners must start");
}

#[tokio::test]
async fn dashboard_boundary_accepts_mapped_loopback_and_rejects_origin_port_mismatch() {
    let (_dir, s) = setup();
    for (remote, host, origin, expected) in [
        ("[::ffff:127.0.0.1]:55", "localhost:8790", None, 200),
        ("[::1]:55", "[::1]:8790", Some("http://[::1]:8790"), 200),
        (
            "127.0.0.1:55",
            "localhost:8790",
            Some("http://localhost:8791"),
            400,
        ),
        ("127.0.0.1:55", "localhost:8790", Some("null"), 400),
    ] {
        let mut builder = Request::builder().uri("/ui").header("host", host);
        if let Some(origin) = origin {
            builder = builder.header("origin", origin)
        }
        let mut req = builder.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(remote.parse::<SocketAddr>().unwrap()));
        assert_eq!(
            knowwhere::app(s.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status()
                .as_u16(),
            expected,
            "remote={remote}, origin={origin:?}"
        );
    }
}

#[tokio::test]
async fn http2_authority_and_transport_default_ports_use_the_same_local_boundary() {
    let (_dir, s) = setup();
    for (uri, host, origin, https, expected) in [
        (
            "https://localhost:8791/ui",
            None,
            Some("https://localhost:8791"),
            true,
            200,
        ),
        (
            "https://localhost:8791/ui",
            Some("localhost:8790"),
            None,
            true,
            400,
        ),
        (
            "https://localhost:8791/ui",
            Some("evil.example:8791"),
            None,
            true,
            400,
        ),
        (
            "/ui",
            Some("localhost"),
            Some("https://localhost"),
            false,
            400,
        ),
        (
            "/ui",
            Some("localhost"),
            Some("http://localhost"),
            false,
            200,
        ),
        (
            "/ui",
            Some("localhost"),
            Some("https://localhost"),
            true,
            200,
        ),
        (
            "/ui",
            Some("localhost"),
            Some("http://localhost"),
            true,
            400,
        ),
    ] {
        let mut builder = Request::builder().uri(uri);
        if uri.starts_with("https://") {
            builder = builder.version(axum::http::Version::HTTP_2);
        }
        if let Some(host) = host {
            builder = builder.header("host", host)
        }
        if let Some(origin) = origin {
            builder = builder.header("origin", origin)
        }
        let mut req = builder.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:55".parse::<SocketAddr>().unwrap()));
        if https {
            req.extensions_mut().insert(knowwhere::Https);
        }
        assert_eq!(
            knowwhere::app(s.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status()
                .as_u16(),
            expected,
            "uri={uri}, host={host:?}, origin={origin:?}, https={https}"
        );
    }
}

#[tokio::test]
async fn every_non_api_path_is_local_only_even_with_forwarded_loopback_and_a_key() {
    let (_dir, s) = setup();
    let key = s.store.gateway_key().unwrap();
    for path in [
        "/",
        "/ui",
        "/ui/settings",
        "/ui/assets/app.js",
        "/ui/api/gateway-key",
        "/v10/models",
        "/v1evil",
        "/unknown",
    ] {
        let mut req = Request::builder()
            .uri(path)
            .header("host", "localhost:8790")
            .header("origin", "http://localhost:8790")
            .header("x-forwarded-for", "127.0.0.1")
            .header("forwarded", "for=127.0.0.1;host=localhost")
            .header("x-api-key", &key)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(
            "192.168.1.50:55".parse::<SocketAddr>().unwrap(),
        ));
        assert_eq!(
            knowwhere::app(s.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "LAN access must not bypass boundary for {path}"
        );
    }
}

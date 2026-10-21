mod launcher;
use anyhow::{Result, bail};
use knowwhere::{AppState, Config};
use std::{
    io::{self, Write},
    net::{IpAddr, SocketAddr},
};
#[tokio::main]
async fn main() {
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("knowwhere: {e}");
            std::process::exit(1)
        }
    }
}
async fn run() -> Result<i32> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "knowwhere=info".into()),
        )
        .with_target(false)
        .init();
    let (config, args) = Config::load(std::env::args().skip(1))?;
    if args.first().is_some_and(|a| a == "--help" || a == "help") {
        println!(
            "KnowWhere — local model gateway\n\nCommands:\n  serve                  Run the server (default)\n  setup [--model ID]      Configure OpenRouter (key hidden or OPENROUTER_API_KEY)\n  run claude|codex [--] ARGS  Run a harness through a private session gateway\n  login claude|codex     Connect a subscription\n  key show|rotate        Display or replace the gateway key\n  models refresh         Refresh provider model catalogs\n  cert create            Generate a LAN HTTPS certificate\n\nOptions:\n  --data-dir PATH        Data directory (default ~/.knowwhere)\n  --listen-address IP    Bind address (default 0.0.0.0)\n  --port PORT            HTTP port (default 8790)\n  --https-port PORT      HTTPS port (default 8791)\n  --seed-file PATH       Initial catalog\n\nReference-compatible --SimpleRouter:Section:Option=value and\nKnowWhere__Section__Option environment overrides are also accepted."
        );
        return Ok(0);
    }
    if args.first().is_some_and(|a| a == "--version") {
        println!("knowwhere {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if args.first().is_some_and(|a| a == "run") {
        launcher::validate_run(&args[1..])?;
        return launcher::run(AppState::open(config)?, &args[1..]).await;
    }
    if args.first().is_some_and(|a| a == "setup") {
        let model = launcher::validate_setup(&args[1..])?;
        launcher::setup(&AppState::open(config)?, model)?;
        return Ok(0);
    }
    let command = args.iter().map(String::as_str).collect::<Vec<_>>();
    if !matches!(
        command.as_slice(),
        [] | ["serve"]
            | ["key", "show"]
            | ["key", "rotate"]
            | ["models", "refresh"]
            | ["cert", "create"]
            | ["login", "claude"]
            | ["login", "codex"]
    ) {
        bail!("Unknown command. Run knowwhere --help")
    }
    let state = AppState::open(config)?;
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => serve(state).await.map(|_| 0),
        ["key", "show"] => {
            println!("{}", state.store.gateway_key()?);
            Ok(0)
        }
        ["key", "rotate"] => {
            println!("{}", state.store.rotate_gateway_key()?);
            Ok(0)
        }
        ["models", "refresh"] => {
            println!(
                "{}",
                serde_json::to_string_pretty(&knowwhere::providers::refresh(&state).await?)?
            );
            Ok(0)
        }
        ["cert", "create"] => {
            let info = knowwhere::tls::create(&state)?;
            println!(
                "Certificate created.\nCA certificate to trust: {}\nSHA-256: {}\nLAN URL: {}\nExpires: {}\nRestart the server after certificate renewal.",
                info["caPath"].as_str().unwrap_or(""),
                info["fingerprint"].as_str().unwrap_or(""),
                info["lanUrl"].as_str().unwrap_or(""),
                chrono::DateTime::from_timestamp(info["expiresAt"].as_i64().unwrap_or(0), 0)
                    .map(|n| n.to_rfc3339())
                    .unwrap_or_default()
            );
            Ok(0)
        }
        ["login", "claude"] => {
            let login = knowwhere::providers::begin_claude(&state)?;
            println!("Open this URL and sign in:\n{}", login.authorize_url);
            print!("Paste code#state: ");
            io::stdout().flush()?;
            let code = tokio::task::spawn_blocking(|| {
                let mut code = String::new();
                io::stdin().read_line(&mut code).map(|_| code)
            })
            .await??;
            knowwhere::providers::finish_claude(&state, &login, &code).await?;
            state.store.clear_cooldowns("claude")?;
            let _ = knowwhere::providers::refresh(&state).await;
            println!("Claude connected.");
            Ok(0)
        }
        ["login", "codex"] => {
            let device = knowwhere::providers::start_codex(&state).await?;
            println!(
                "Open {} and enter {}",
                device.verification_url, device.user_code
            );
            let deadline = tokio::time::Instant::now() + state.config.device_code_timeout;
            loop {
                if tokio::time::Instant::now() >= deadline {
                    bail!("Device login timed out")
                };
                tokio::time::sleep(std::time::Duration::from_secs(device.interval)).await;
                if knowwhere::providers::poll_codex(&state, &device).await? {
                    break;
                }
            }
            state.store.clear_cooldowns("codex")?;
            let _ = knowwhere::providers::refresh(&state).await;
            println!("Codex connected.");
            Ok(0)
        }
        _ => bail!("Unknown command. Run knowwhere --help"),
    }
}
async fn serve(mut state: AppState) -> Result<()> {
    let tls = knowwhere::tls::load(&state).await?;
    let listen = state.config.listen_address;
    let http_ip = if tls.is_some() && !listen.is_loopback() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        listen
    };
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(http_ip, state.config.port)).await?;
    let actual = listener.local_addr()?;
    state.config.port = actual.port();
    let https_listener = if tls.is_some() {
        let listener =
            std::net::TcpListener::bind(SocketAddr::new(listen, state.config.https_port))?;
        listener.set_nonblocking(true)?;
        state.config.https_port = listener.local_addr()?.port();
        if let Some(mut info) = state
            .store
            .get_setting("tls:active_metadata")?
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        {
            if let Some(mut url) = info["lanUrl"]
                .as_str()
                .and_then(|url| url::Url::parse(url).ok())
            {
                let _ = url.set_port(Some(state.config.https_port));
                info["lanUrl"] = serde_json::json!(url.to_string().trim_end_matches('/'));
            }
            state
                .store
                .set_setting("tls:active_metadata", &info.to_string())?;
        }
        Some(listener)
    } else {
        None
    };
    *state.runtime.listeners.write().await = Some(
        serde_json::json!({"address":http_ip.to_string(),"port":actual.port(),"httpsPort":https_listener.as_ref().map(|_|state.config.https_port)}),
    );
    tracing::info!(address=%actual,"Listening on HTTP");
    tracing::info!(url=%format!("http://localhost:{}/ui",actual.port()),"Dashboard");
    let _ = state.store.gateway_key()?;
    // Probe before accepting requests so a healthy local model is eligible immediately.
    knowwhere::refresh_health(&state).await;
    knowwhere::start_background(state.clone());
    let app = knowwhere::app(state.clone());
    let http_app = app.clone();
    let http = async move {
        axum::serve(
            listener,
            http_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(anyhow::Error::from)
    };
    if let Some(tls) = tls {
        let address = SocketAddr::new(listen, state.config.https_port);
        tracing::info!(%address,"Listening on HTTPS");
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown().await;
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
        let https =
            axum_server::from_tcp_rustls(https_listener.expect("TLS listener was bound"), tls)
                .handle(handle)
                .serve(
                    app.layer(axum::Extension(knowwhere::Https))
                        .into_make_service_with_connect_info::<SocketAddr>(),
                );
        tokio::select! {r=http=>r,r=https=>r.map_err(anyhow::Error::from)}
    } else {
        http.await
    }
}
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

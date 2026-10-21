pub mod config;
pub mod dashboard;
pub mod pricing;
pub mod providers;
pub mod responses;
pub mod routing;
pub mod store;
pub mod systemone;
pub mod tls;
pub mod types;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
pub use config::Config;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
pub use store::Store;
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use types::*;
pub type ConversationSlot = Arc<Mutex<(usize, Instant)>>;
pub type ConversationHolds = HashMap<(String, String), ConversationSlot>;
#[derive(Clone, Copy)]
pub struct Https;
#[derive(Default)]
pub struct Runtime {
    pub health: RwLock<HashMap<String, bool>>,
    pub listeners: RwLock<Option<Value>>,
    pub assignment: AsyncMutex<()>,
    pub conversations: Mutex<ConversationHolds>,
    pub renamed: Mutex<HashMap<String, String>>,
}
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub config: Config,
    pub client: reqwest::Client,
    pub runtime: Arc<Runtime>,
}
impl AppState {
    pub fn open(config: Config) -> anyhow::Result<Self> {
        let store = Store::open(&config.data_dir.join("knowwhere.db"))?;
        let needs_seed = store.catalog().map_or(true, |catalog| {
            catalog.models.is_empty() || catalog.prices.is_empty()
        });
        if needs_seed {
            let seed = match &config.seed_file {
                Some(path) => std::fs::read_to_string(path)?,
                None => include_str!("../seed.json").into(),
            };
            let catalog: Catalog = serde_json::from_str(seed.trim_start_matches('\u{feff}'))?;
            routing::validate_catalog(&catalog)?;
            store.seed(&catalog)?;
        }
        let path = config
            .seed_override_file
            .clone()
            .unwrap_or_else(|| config.data_dir.join("seed.local.json"));
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            routing::apply_override(&store, &text)?;
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            store,
            config,
            client,
            runtime: Arc::default(),
        })
    }
    pub async fn monitoring(&self) -> anyhow::Result<Value> {
        let mut catalog = self.store.catalog()?;
        for m in &mut catalog.models {
            m.api_key = None;
        }
        let health = self.runtime.health.read().await.clone();
        let cooldowns = self.store.cooldowns()?;
        let mut statuses = Vec::new();
        for m in &catalog.models {
            statuses.push(json!({"id":m.id,"provider":m.provider,"status":routing::status(self,m).await?,"healthy":health.get(&m.id).copied(),"apiKeySet":self.store.get_secret(&format!("model:{}",m.id))?.is_some()}));
        }
        let listening=self.runtime.listeners.read().await.clone().unwrap_or_else(||json!({"address":self.config.listen_address.to_string(),"port":self.config.port,"httpsPort":self.config.https_port}));
        Ok(
            json!({"catalog":catalog,"statuses":statuses,"pins":self.store.pins()?,"cooldowns":cooldowns,"requests":self.store.usages(50,None)?,"subscriptions":routing::subscription_usage(self).await?,"listening":listening}),
        )
    }
}
pub fn error(status: u16, kind: &str, message: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({"type":"error","error":{"type":kind,"message":message}})),
    )
        .into_response()
}
pub fn app(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/messages", post(routing::messages))
        .route("/v1/responses", post(responses::create))
        .route("/v1/responses/compact", post(responses::compact))
        .route("/v1/messages/count_tokens", post(routing::count_tokens))
        .route("/v1/models", get(routing::models))
        .route("/v1/subscriptions/usage", get(routing::subscriptions))
        .route("/v1/sessions/{session_id}/cost", get(routing::session_cost))
        .merge(systemone::routes());
    api.merge(dashboard::routes())
        .route("/", get(|| async { axum::response::Redirect::to("/ui") }))
        .fallback(|| async { error(404, "not_found_error", "Unknown route") })
        // Messages/Responses buffer through Bytes. Decision endpoints consume
        // Request directly and enforce their separately configured body limit.
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), boundary))
        .with_state(state)
}
pub fn is_loopback(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
        }
    }
}
fn local_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost") || ["127.0.0.1", "::1"].contains(&host)
}
fn local_authority(authority: &str) -> bool {
    authority
        .parse::<axum::http::uri::Authority>()
        .ok()
        .is_some_and(|a| local_host(a.host()))
}
async fn boundary(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let api = req.uri().path() == "/v1" || req.uri().path().starts_with("/v1/");
    if api {
        let expected = match state.store.gateway_key() {
            Ok(v) => v,
            Err(_) => return error(500, "api_error", "Gateway key is unavailable"),
        };
        let mut keys = Vec::new();
        for name in ["x-weave-router-key", "x-api-key"] {
            keys.extend(
                req.headers()
                    .get_all(name)
                    .iter()
                    .filter_map(|v| v.to_str().ok()),
            );
        }
        keys.extend(
            req.headers()
                .get_all("authorization")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .filter_map(|v| {
                    v.get(..7)
                        .filter(|p| p.eq_ignore_ascii_case("Bearer "))
                        .map(|_| v[7..].trim())
                }),
        );
        use subtle::ConstantTimeEq;
        if !keys
            .into_iter()
            .any(|k| bool::from(k.as_bytes().ct_eq(expected.as_bytes())))
        {
            return error(
                401,
                "authentication_error",
                "A valid gateway key is required",
            );
        }
    } else {
        let remote = req
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip());
        if !remote.is_some_and(is_loopback) {
            return error(
                403,
                "permission_error",
                "The dashboard is available only on this machine",
            );
        }
        let uri_authority = req.uri().authority().map(|a| a.as_str());
        let host = req.headers().get("host").and_then(|v| v.to_str().ok());
        if host
            .zip(uri_authority)
            .is_some_and(|(h, a)| !h.eq_ignore_ascii_case(a))
        {
            return error(
                400,
                "invalid_request_error",
                "Conflicting Host and URI authority",
            );
        }
        let authority = host.or(uri_authority).unwrap_or("");
        if !local_authority(authority) {
            return error(400, "invalid_request_error", "A loopback Host is required");
        }
        let default_port = if req.extensions().get::<Https>().is_some() {
            443
        } else {
            80
        };
        if let Some(origin) = req.headers().get("origin") {
            let ok = origin
                .to_str()
                .ok()
                .and_then(|s| url::Url::parse(s).ok())
                .is_some_and(|u| {
                    let host_ok = local_host(u.host_str().unwrap_or(""));
                    let a = authority.parse::<axum::http::uri::Authority>().ok();
                    let port = a
                        .as_ref()
                        .and_then(|a| a.port_u16())
                        .unwrap_or(default_port);
                    host_ok
                        && matches!(u.scheme(), "http" | "https")
                        && u.port_or_known_default() == Some(port)
                });
            if !ok {
                return error(
                    400,
                    "invalid_request_error",
                    "A same-port loopback Origin is required",
                );
            }
        }
    }
    let response = next.run(req).await;
    if api
        && response.status().is_client_error()
        && !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.starts_with("application/json"))
    {
        let status = response.status().as_u16();
        return error(
            status,
            if status == 413 {
                "request_too_large"
            } else {
                "invalid_request_error"
            },
            if status == 413 {
                "Request body exceeds the allowed size"
            } else {
                "Invalid request parameters"
            },
        );
    }
    response
}
pub async fn refresh_health(state: &AppState) {
    if let Ok(catalog) = state.store.catalog() {
        for m in catalog
            .models
            .into_iter()
            .filter(|m| m.enabled && matches!(m.provider.as_str(), "local" | "systemone"))
        {
            let mut req = state
                .client
                .get(format!(
                    "{}/models",
                    m.base_url.as_deref().unwrap_or("").trim_end_matches('/')
                ))
                .timeout(std::time::Duration::from_secs(5));
            if let Ok(Some(key)) = state.store.get_secret(&format!("model:{}", m.id)) {
                req = req.bearer_auth(key);
            }
            let healthy = req.send().await.is_ok_and(|r| r.status().is_success());
            state.runtime.health.write().await.insert(m.id, healthy);
        }
    }
}
pub fn start_background(state: AppState) {
    let s = state.clone();
    tokio::spawn(async move {
        if s.config.health_interval.is_zero() {
            return;
        }
        loop {
            tokio::time::sleep(s.config.health_interval).await;
            refresh_health(&s).await;
        }
    });
    tokio::spawn(async move {
        let interval = state.config.model_refresh_interval;
        if interval.is_zero() {
            return;
        }
        loop {
            let mut delay = interval;
            for provider in ["claude", "codex", "openrouter"] {
                if !state.store.get_secret(provider).is_ok_and(|v| v.is_some()) {
                    continue;
                }
                let last = state
                    .store
                    .get_setting(&format!("models:{provider}"))
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .and_then(|v| v["fetchedAt"].as_i64());
                let remaining = last.map(|at| {
                    interval.saturating_sub(std::time::Duration::from_secs(
                        now().saturating_sub(at).max(0) as u64,
                    ))
                });
                if remaining.is_some_and(|d| !d.is_zero()) {
                    delay = delay.min(remaining.unwrap());
                } else {
                    let _ = providers::refresh_provider(&state, provider).await;
                }
            }
            tokio::time::sleep(delay.max(std::time::Duration::from_secs(1))).await;
        }
    });
}

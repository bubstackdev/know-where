//! Explicit SystemOne decision calls, also exposed as stateless MCP tools.
use crate::{
    AppState, Store, error,
    types::{Model, Usage, now},
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use std::{fmt, time::Instant};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/systemone/models", get(models))
        .route("/v1/systemone", post(http_decide))
        .route(
            "/v1/mcp",
            post(mcp)
                .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                .delete(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
}

const QUESTIONS: [&str; 4] = ["choice", "noul", "score", "box"];
fn supports(model: &Model, flag: &str) -> bool {
    match &model.decisions {
        Value::Object(m) => m.get(flag).and_then(Value::as_bool).unwrap_or(false),
        Value::String(s) => s
            .split(',')
            .any(|s| s.trim().eq_ignore_ascii_case(flag) || s.trim().eq_ignore_ascii_case("all")),
        Value::Number(n) => {
            let bit = match flag {
                "choice" => 1,
                "noul" => 2,
                "score" => 4,
                "box" => 8,
                "images" => 16,
                "videos" => 32,
                _ => 0,
            };
            n.as_u64().is_some_and(|v| v & bit != 0)
        }
        _ => false,
    }
}
fn unsupported(model: &Model, request: &Value) -> Option<String> {
    if let Some(questions) = request["questions"].as_object() {
        for (name, q) in questions {
            if let Some(kind) = q["type"].as_str()
                && QUESTIONS.contains(&kind)
                && !supports(model, kind)
            {
                return Some(format!("{kind} questions (question '{name}')"));
            }
        }
    }
    let mut stack = vec![request];
    let mut image = false;
    let mut video = false;
    while let Some(v) = stack.pop() {
        match v {
            Value::Object(map) => {
                image |= v["type"] == "image_url";
                video |= v["type"] == "video_url";
                stack.extend(map.values());
            }
            Value::Array(array) => stack.extend(array),
            _ => {}
        }
    }
    image |= request["images"].as_array().is_some_and(|v| !v.is_empty());
    video |= request["videos"].as_array().is_some_and(|v| !v.is_empty());
    if image && !supports(model, "images") {
        Some("images".into())
    } else if video && !supports(model, "videos") {
        Some("videos".into())
    } else {
        None
    }
}
async fn model_listing(state: &AppState) -> anyhow::Result<Value> {
    let mut models: Vec<_> = state
        .store
        .catalog()?
        .models
        .into_iter()
        .filter(|m| m.enabled && m.provider.eq_ignore_ascii_case("systemone"))
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    let health = state.runtime.health.read().await;
    Ok(
        json!({"data":models.iter().map(|m|json!({"id":m.id,"healthy":health.get(&m.id).copied().unwrap_or(false),"question_types":QUESTIONS.iter().filter(|q|supports(m,q)).collect::<Vec<_>>(),"images":supports(m,"images"),"videos":supports(m,"videos")})).collect::<Vec<_>>()}),
    )
}
async fn models(State(state): State<AppState>) -> Response {
    match model_listing(&state).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::warn!(error=%e,"SystemOne model listing failed");
            error(500, "api_error", "The catalog is unavailable")
        }
    }
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    content_type: Option<String>,
    router_error: Option<(String, String)>,
}
impl Reply {
    fn error(status: u16, kind: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: Vec::new(),
            content_type: None,
            router_error: Some((kind.into(), message.into())),
        }
    }
    fn into_response(self) -> Response {
        if let Some((kind, message)) = self.router_error {
            return error(self.status, &kind, &message);
        }
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() =
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_GATEWAY);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            self.content_type
                .as_deref()
                .unwrap_or("application/json")
                .parse()
                .unwrap_or_else(|_| header::HeaderValue::from_static("application/json")),
        );
        response
    }
}

/// Dropping an in-flight handler still records spent work when a caller cancels.
struct UsageGuard {
    store: Store,
    row: Usage,
    started: Instant,
}
impl Drop for UsageGuard {
    fn drop(&mut self) {
        self.row.latency_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if let Err(e) = self.store.record_usage(&self.row) {
            tracing::warn!(error=%e,"Could not record SystemOne usage");
        }
    }
}

async fn decide(state: &AppState, mut request: Value, session: Option<String>) -> Reply {
    let id = request["model"].as_str();
    let catalog = match state.store.catalog() {
        Ok(c) => c,
        Err(_) => return Reply::error(500, "api_error", "The catalog is unavailable"),
    };
    let model = match catalog.models.iter().find(|m| {
        Some(m.id.as_str()) == id && m.enabled && m.provider.eq_ignore_ascii_case("systemone")
    }) {
        Some(m) => m,
        None => {
            return Reply::error(
                404,
                "not_found_error",
                match id {
                    Some(id) => format!("No enabled SystemOne model is named '{id}'."),
                    None => "model is required: the ID of a SystemOne model.".into(),
                },
            );
        }
    };
    let mut guard = UsageGuard {
        store: state.store.clone(),
        started: Instant::now(),
        row: Usage {
            at: now(),
            model_id: model.id.clone(),
            provider: "systemone".into(),
            client_session_id: session,
            status: 499,
            upstream_model_id: Some(model.upstream_model_id.clone()),
            is_streaming: Some(false),
            outcome: Some("client_cancelled".into()),
            ..Default::default()
        },
    };
    let reply = send(state, model, &mut request).await;
    guard.row.status = reply.status;
    guard.row.outcome = Some(
        if (200..300).contains(&reply.status) {
            "completed"
        } else {
            "failed"
        }
        .into(),
    );
    if let Some((kind, message)) = &reply.router_error {
        guard.row.failure_kind = Some(kind.clone());
        guard.row.failure_detail = Some(message.clone());
    }
    if reply.router_error.is_none()
        && (200..300).contains(&reply.status)
        && let Ok(body) = serde_json::from_slice::<Value>(&reply.body)
    {
        let u = &body["usage"];
        guard.row.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
        guard.row.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
        guard.row.cache_read_tokens = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        guard.row.cache_write_tokens = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        guard.row.cache_write1h_tokens = u
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
    guard.row.cost_source = Some(
        if let Some(p) = catalog.prices.get(&model.id) {
            guard.row.cost_usd_micros = crate::pricing::estimated(p, &guard.row);
            "estimated"
        } else {
            "unknown"
        }
        .into(),
    );
    reply
}
async fn send(state: &AppState, model: &Model, request: &mut Value) -> Reply {
    if let Some(reason) = unsupported(model, request) {
        return Reply::error(
            400,
            "invalid_request_error",
            format!("SystemOne model {} does not support {reason}.", model.id),
        );
    }
    if !state
        .runtime
        .health
        .read()
        .await
        .get(&model.id)
        .copied()
        .unwrap_or(false)
    {
        return Reply::error(
            503,
            "overloaded_error",
            format!(
                "SystemOne model {} is unavailable: its endpoint failed its last health check.",
                model.id
            ),
        );
    }
    request["model"] = Value::String(model.upstream_model_id.clone());
    let Some(base) = model.base_url.as_deref() else {
        return Reply::error(503, "overloaded_error", "SystemOne model has no endpoint");
    };
    let mut builder = state
        .client
        .post(format!("{}/systemone", base.trim_end_matches('/')))
        .json(request)
        .timeout(state.config.systemone_timeout);
    match state.store.get_secret(&format!("model:{}", model.id)) {
        Ok(Some(key)) => builder = builder.bearer_auth(key),
        Ok(None) => {}
        Err(_) => return Reply::error(500, "api_error", "The model credential is unavailable"),
    }
    let sent = async {
        let response = builder.send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await?.to_vec();
        Ok::<_, reqwest::Error>(Reply {
            status,
            body,
            content_type,
            router_error: None,
        })
    };
    // Covers headers AND body, even when the upstream sends headers immediately.
    match tokio::time::timeout(state.config.systemone_timeout, sent).await {
        Ok(Ok(reply)) => reply,
        Err(_) => Reply::error(
            504,
            "timeout_error",
            format!(
                "SystemOne model {} did not answer within {} seconds.",
                model.id,
                state.config.systemone_timeout.as_secs_f64()
            ),
        ),
        Ok(Err(e)) if e.is_timeout() => Reply::error(
            504,
            "timeout_error",
            format!(
                "SystemOne model {} did not answer within {} seconds.",
                model.id,
                state.config.systemone_timeout.as_secs_f64()
            ),
        ),
        Ok(Err(_)) => {
            state
                .runtime
                .health
                .write()
                .await
                .insert(model.id.clone(), false);
            Reply::error(
                503,
                "overloaded_error",
                format!("SystemOne model {} is unreachable.", model.id),
            )
        }
    }
}
fn session(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
struct InputError {
    status: u16,
    kind: &'static str,
    message: String,
}
impl IntoResponse for InputError {
    fn into_response(self) -> Response {
        error(self.status, self.kind, &self.message)
    }
}
fn too_large(max: usize) -> InputError {
    InputError {
        status: 413,
        kind: "request_too_large",
        message: format!("The body is larger than {max} bytes."),
    }
}
async fn read_body(req: Request, max: usize) -> Result<Value, InputError> {
    let bytes = read_bytes(req, max).await?;
    parse_strict(&bytes).map_err(|_| InputError {
        status: 400,
        kind: "invalid_request_error",
        message: "The body must be a JSON object with no duplicate keys.".into(),
    })
}
async fn read_bytes(req: Request, max: usize) -> Result<bytes::Bytes, InputError> {
    if req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > max as u64)
    {
        return Err(too_large(max));
    }
    to_bytes(req.into_body(), max)
        .await
        .map_err(|_| too_large(max))
}
async fn http_decide(State(state): State<AppState>, req: Request) -> Response {
    let session = session(req.headers());
    let request = match read_body(req, state.config.systemone_max_body).await {
        Ok(v) if v.is_object() => v,
        Ok(_) => {
            return error(
                400,
                "invalid_request_error",
                "The body must be a JSON object.",
            );
        }
        Err(e) => return e.into_response(),
    };
    decide(&state, request, session).await.into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}
fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
fn invalid_rpc_transport() -> Response {
    let mut response = rpc_error(Value::Null, -32600, "Invalid Request");
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response
}
fn accepts(headers: &HeaderMap, mime: &str) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .any(|part| {
            let mut p = part.split(';');
            let name = p.next().unwrap_or("").trim();
            name.eq_ignore_ascii_case(mime) && !p.any(|s| s.trim() == "q=0")
        })
}
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin
        .to_str()
        .ok()
        .and_then(|s| url::Url::parse(s).ok())
        .ok_or(())
    else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
        || !origin.username().is_empty()
        || origin.password().is_some()
    {
        return false;
    }
    let Some(host) = origin.host_str() else {
        return false;
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    let Some(authority) = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<axum::http::uri::Authority>().ok())
    else {
        return false;
    };
    loopback
        && host.eq_ignore_ascii_case(authority.host())
        && origin.port_or_known_default()
            == Some(
                authority
                    .port_u16()
                    .unwrap_or(if origin.scheme() == "https" { 443 } else { 80 }),
            )
}
async fn mcp(State(state): State<AppState>, req: Request) -> Response {
    if !origin_allowed(req.headers()) {
        return error(
            403,
            "permission_error",
            "MCP requires a matching loopback Origin",
        );
    }
    if !req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return error(
            415,
            "invalid_request_error",
            "MCP requires application/json",
        );
    }
    // Apply the media limit before negotiation, preserving a useful 413 envelope.
    let acceptable =
        accepts(req.headers(), "application/json") && accepts(req.headers(), "text/event-stream");
    if let Some(version) = req.headers().get("mcp-protocol-version")
        && !version
            .to_str()
            .is_ok_and(|v| ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"].contains(&v))
    {
        return error(
            400,
            "invalid_request_error",
            "Unsupported MCP protocol version",
        );
    }
    let session = session(req.headers());
    let bytes = match read_bytes(req, state.config.systemone_max_body).await {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    if !acceptable {
        return error(
            406,
            "invalid_request_error",
            "Accept must include application/json and text/event-stream",
        );
    }
    let request = match parse_strict(&bytes) {
        Ok(v) => v,
        Err(_) => return invalid_rpc_transport(),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if !request.is_object()
        || request["jsonrpc"] != "2.0"
        || !request["method"].is_string()
        || (!id.is_null() && !id.is_string() && !id.is_number())
    {
        return invalid_rpc_transport();
    }
    let method = request["method"].as_str().unwrap_or("");
    if request.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let params = &request["params"];
    match method {
        "initialize" => {
            if !params.is_object()
                || !params["protocolVersion"].is_string()
                || !params["clientInfo"].is_object()
                || !params["capabilities"].is_object()
            {
                return rpc_error(id, -32602, "Invalid initialize parameters");
            }
            let requested = params["protocolVersion"].as_str().unwrap();
            let version =
                if ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"].contains(&requested) {
                    requested
                } else {
                    "2025-11-25"
                };
            rpc_result(
                id,
                json!({"protocolVersion":version,"capabilities":{"logging":{},"tools":{"listChanged":false}},"serverInfo":{"name":"knowwhere","version":env!("CARGO_PKG_VERSION")}}),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "logging/setLevel" => {
            if !params["level"].as_str().is_some_and(|v| {
                [
                    "debug",
                    "info",
                    "notice",
                    "warning",
                    "error",
                    "critical",
                    "alert",
                    "emergency",
                ]
                .contains(&v)
            }) {
                return rpc_error(id, -32602, "Invalid logging level");
            }
            // Stateless transport has no persistent per-client log stream.
            rpc_result(id, json!({}))
        }
        "tools/list" => rpc_result(id, json!({"tools":tools()})),
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return rpc_error(id, -32602, "Tool name is required");
            };
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !args.is_object() {
                return rpc_error(id, -32602, "Tool arguments must be an object");
            }
            let (text, is_error) = match name {
                "systemone_models" => match model_listing(&state).await {
                    Ok(v) => (v.to_string(), false),
                    Err(_) => ("The catalog is unavailable".into(), true),
                },
                "systemone_decide" => {
                    if !args["model"].is_string()
                        || args.get("state").is_none()
                        || !args["questions"].is_object()
                        || ["images", "videos"].iter().any(|key| {
                            args.get(key).is_some_and(|v| {
                                !v.is_null()
                                    && !v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
                            })
                        })
                    {
                        return rpc_error(
                            id,
                            -32602,
                            "model, state and questions are required; media must be string arrays",
                        );
                    }
                    let reply = decide(&state, args, session).await;
                    let failed = !(200..300).contains(&reply.status);
                    match reply.router_error {
                        Some((_, message)) => (message, true),
                        None => {
                            let text = String::from_utf8_lossy(&reply.body).into_owned();
                            (
                                if failed {
                                    format!("HTTP {}: {text}", reply.status)
                                } else {
                                    text
                                },
                                failed,
                            )
                        }
                    }
                }
                _ => return rpc_error(id, -32602, "Unknown tool"),
            };
            rpc_result(
                id,
                json!({"content":[{"type":"text","text":text}],"isError":is_error}),
            )
        }
        _ => rpc_error(id, -32601, "Method not found"),
    }
}
fn tools() -> Value {
    json!([
        {"name":"systemone_models","description":"Lists enabled SystemOne decision models, endpoint health, supported question types and media.","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true,"idempotentHint":true}},
        {"name":"systemone_decide","description":"Answers decision questions about state without generating text. Questions map a name to {type, instructions, criteria}. choice takes an option object, noul is yes/no, score takes ordered labels, box locates an image target without criteria. Use model capabilities from systemone_models.","inputSchema":{"type":"object","properties":{"model":{"type":"string","description":"A SystemOne model ID from systemone_models"},"state":{"description":"Text or any JSON input"},"questions":{"type":"object","description":"Question name to question"},"images":{"type":"array","items":{"type":"string"},"description":"Image URLs or base64 data URLs"},"videos":{"type":"array","items":{"type":"string"},"description":"Video URLs or base64 data URLs"}},"required":["model","state","questions"]},"annotations":{"readOnlyHint":true}}
    ])
}

/// serde_json's ordinary Value accepts duplicate keys. Recursive validation
/// avoids ambiguous model selection in both HTTP and MCP envelopes.
pub(crate) fn parse_strict(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = Strict::deserialize(&mut de)?.0;
    de.end()?;
    Ok(value)
}
struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value with unique object keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Strict, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Strict(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Strict, E> {
                Ok(Strict(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut array = Vec::new();
                while let Some(Strict(v)) = a.next_element()? {
                    array.push(v)
                }
                Ok(Strict(Value::Array(array)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut map = serde_json::Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if map.contains_key(&k) {
                        return Err(de::Error::custom(format!("duplicate key {k}")));
                    }
                    let Strict(v) = a.next_value()?;
                    map.insert(k, v);
                }
                Ok(Strict(Value::Object(map)))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Runtime, types::Catalog};
    use http_body_util::BodyExt;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    };
    use tower::ServiceExt;
    struct Fixture {
        state: AppState,
        requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
        status: Arc<AtomicU16>,
        server: tokio::task::JoinHandle<()>,
        _directory: tempfile::TempDir,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    impl Fixture {
        async fn new() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let status = Arc::new(AtomicU16::new(200));
            let captured = requests.clone();
            let code = status.clone();
            let upstream = Router::new().route(
                "/v1/systemone",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let captured = captured.clone();
                    let code = code.clone();
                    async move {
                        let half = body["state"] == "round-half";
                        captured.lock().unwrap().push((headers, body));
                        let status = StatusCode::from_u16(code.load(Ordering::SeqCst)).unwrap();
                        (
                            status,
                            [(header::CONTENT_TYPE, "application/json")],
                            if status.is_success() && half {
                                "{\"answers\":{\"q\":0.7},\"usage\":{\"input_tokens\":50}}"
                            } else if status.is_success() {
                                "{\"answers\":{\"q\":0.7},\"usage\":{\"input_tokens\":120}}"
                            } else {
                                "{\"error\":\"bad question\"}"
                            },
                        )
                    }
                }),
            );
            let server =
                tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(&directory.path().join("router.db")).unwrap();
            let catalog:Catalog=serde_json::from_value(json!({"models":[{"id":"d3","provider":"systemone","upstreamModelId":"upstream-d3","enabled":true,"baseUrl":format!("http://{address}/v1/"),"apiKey":"model-secret"}],"prices":{"d3":{"input":2.0}}})).unwrap();
            store.seed(&catalog).unwrap();
            let state = AppState {
                store,
                config: Config::default(),
                client: reqwest::Client::new(),
                runtime: Arc::new(Runtime::default()),
            };
            state.runtime.health.write().await.insert("d3".into(), true);
            Self {
                state,
                requests,
                status,
                server,
                _directory: directory,
            }
        }
        async fn http(&self, body: impl Into<Body>) -> Response {
            routes()
                .with_state(self.state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/systemone")
                        .header("x-claude-code-session-id", " session ")
                        .body(body.into())
                        .unwrap(),
                )
                .await
                .unwrap()
        }
        async fn mcp(&self, body: Value) -> Response {
            routes()
                .with_state(self.state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/mcp")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::ACCEPT, "application/json, text/event-stream")
                        .header("x-claude-code-session-id", "mcp-session")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    }
    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
    fn request() -> Value {
        json!({"model":"d3","state":"code","questions":{"q":{"type":"noul"}}})
    }
    fn call(name: &str, args: Value) -> Value {
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":args}})
    }

    #[tokio::test]
    async fn forwards_exact_decision_shape_and_records_priced_untiered_usage() {
        let f = Fixture::new().await;
        let response = f.http(request().to_string()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["usage"]["input_tokens"], 120);
        let requests = f.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0[header::AUTHORIZATION], "Bearer model-secret");
        assert_eq!(requests[0].1["model"], "upstream-d3");
        assert_eq!(requests[0].1["questions"], request()["questions"]);
        drop(requests);
        let rows = f.state.store.usages(10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].client_session_id.as_deref(), Some("session"));
        assert_eq!(rows[0].tier, None);
        assert_eq!(rows[0].input_tokens, 120);
        assert_eq!(rows[0].cost_usd_micros, 240);
    }

    #[tokio::test]
    async fn usage_uses_shared_decimal_half_micro_rounding() {
        let f = Fixture::new().await;
        let mut catalog = f.state.store.catalog().unwrap();
        catalog.prices.get_mut("d3").unwrap().input = 0.29;
        f.state.store.save_catalog(&catalog).unwrap();
        let mut body = request();
        body["state"] = json!("round-half");
        assert_eq!(f.http(body.to_string()).await.status(), StatusCode::OK);
        let rows = f.state.store.usages(10, None).unwrap();
        assert_eq!(rows[0].input_tokens, 50);
        assert_eq!(rows[0].cost_usd_micros, 15);
    }

    #[tokio::test]
    async fn errors_passthrough_and_unknown_models_do_not_record_usage() {
        let f = Fixture::new().await;
        f.status.store(400, Ordering::SeqCst);
        let response = f.http(request().to_string()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await, json!({"error":"bad question"}));
        let response = f.http(json!({"model":"missing"}).to_string()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(f.requests.lock().unwrap().len(), 1);
        assert_eq!(f.state.store.usages(10, None).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn duplicate_keys_and_non_objects_are_refused_before_upstream() {
        let f = Fixture::new().await;
        for body in [
            "[]",
            "{\"model\":\"d3\",\"model\":\"missing\"}",
            "{\"model\":\"d3\",\"questions\":{\"a\":1,\"a\":2}}",
        ] {
            assert_eq!(f.http(body).await.status(), StatusCode::BAD_REQUEST);
        }
        assert!(f.requests.lock().unwrap().is_empty());
        assert!(f.state.store.usages(10, None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn known_question_types_and_nested_media_obey_capabilities() {
        let f = Fixture::new().await;
        let mut catalog = f.state.store.catalog().unwrap();
        catalog.models[0].decisions = json!("Choice, Noul, Box, Images");
        f.state.store.save_catalog(&catalog).unwrap();
        for body in [
            json!({"model":"d3","questions":{"q":{"type":"score"}}}),
            json!({"model":"d3","questions":{"q":{"type":"noul","instructions":[{"type":"video_url"}]}}}),
            json!({"model":"d3","videos":["x"]}),
        ] {
            assert_eq!(
                f.http(body.to_string()).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
        let response = f
            .http(
                json!({"model":"d3","questions":{"q":{"type":"box"}},"images":["x"],"videos":[]})
                    .to_string(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(f.requests.lock().unwrap().len(), 1);
        assert_eq!(f.state.store.usages(10, None).unwrap().len(), 4);
    }

    #[tokio::test]
    async fn unhealthy_and_unprobed_models_are_refused() {
        let f = Fixture::new().await;
        f.state.runtime.health.write().await.clear();
        assert_eq!(
            f.http(request().to_string()).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(f.requests.lock().unwrap().is_empty());
        let models = routes()
            .with_state(f.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/v1/systemone/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_body(models).await["data"][0]["healthy"], false);
    }

    #[tokio::test]
    async fn oversized_media_has_a_structured_413_for_http_and_mcp() {
        let mut f = Fixture::new().await;
        f.state.config.systemone_max_body = 100;
        let response = f
            .http(json!({"model":"d3","state":"x".repeat(200)}).to_string())
            .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            json_body(response).await["error"]["type"],
            "request_too_large"
        );
        assert_eq!(
            f.mcp(call("systemone_decide", request())).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert!(f.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mcp_initialization_tools_and_calls_share_http_behavior() {
        let f = Fixture::new().await;
        let initialized=json_body(f.mcp(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","clientInfo":{"name":"test","version":"1"},"capabilities":{}}})).await).await;
        assert_eq!(initialized["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            f.mcp(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await
                .status(),
            StatusCode::ACCEPTED
        );
        let listed = json_body(
            f.mcp(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
                .await,
        )
        .await;
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 2);
        assert_eq!(
            listed["result"]["tools"][1]["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            5
        );
        let answer = json_body(f.mcp(call("systemone_decide", request())).await).await;
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"]["isError"], false);
        assert!(
            answer["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("answers")
        );
        assert_eq!(
            f.state.store.usages(10, None).unwrap()[0]
                .client_session_id
                .as_deref(),
            Some("mcp-session")
        );
    }

    #[tokio::test]
    async fn mcp_tool_errors_and_protocol_errors_have_distinct_shapes() {
        let f = Fixture::new().await;
        let unknown = json_body(
            f.mcp(call(
                "systemone_decide",
                json!({"model":"missing","state":"x","questions":{}}),
            ))
            .await,
        )
        .await;
        assert_eq!(unknown["result"]["isError"], true);
        f.status.store(400, Ordering::SeqCst);
        let upstream = json_body(f.mcp(call("systemone_decide", request())).await).await;
        assert!(
            upstream["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("HTTP 400:")
        );
        let malformed =
            json_body(f.mcp(call("systemone_decide", json!({"model":"d3"}))).await).await;
        assert_eq!(malformed["error"]["code"], -32602);
        let missing = json_body(
            f.mcp(json!({"jsonrpc":"2.0","id":3,"method":"missing"}))
                .await,
        )
        .await;
        assert_eq!(missing["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn mcp_requires_transport_negotiation_and_rejects_cross_origin_browsers() {
        let f = Fixture::new().await;
        for (accept, origin, status) in [
            ("application/json", None, 406),
            (
                "application/json, text/event-stream",
                Some("https://evil.example"),
                403,
            ),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/v1/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, accept)
                .header(header::HOST, "localhost:8790");
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = routes()
                .with_state(f.state.clone())
                .oneshot(request.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
        }
    }

    #[tokio::test]
    async fn malformed_mcp_transport_and_logging_match_the_reference_sdk() {
        let f = Fixture::new().await;
        let response = routes()
            .with_state(f.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/mcp")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from("{bad"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["error"]["code"], -32600);
        let response=f.mcp(json!({"jsonrpc":"2.0","id":1,"method":"logging/setLevel","params":{"level":"warning"}})).await;
        assert_eq!(json_body(response).await["result"], json!({}));
        let response=f.mcp(json!({"jsonrpc":"2.0","id":2,"method":"logging/setLevel","params":{"level":"invalid"}})).await;
        assert_eq!(json_body(response).await["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn cancellation_records_499_and_timeout_records_504() {
        let mut f = Fixture::new().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let notify = entered.clone();
        let upstream = Router::new().route(
            "/systemone",
            post(move || {
                let notify = notify.clone();
                async move {
                    notify.notify_one();
                    std::future::pending::<()>().await;
                    StatusCode::OK
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let mut catalog = f.state.store.catalog().unwrap();
        catalog.models[0].base_url = Some(format!("http://{address}"));
        f.state.store.save_catalog(&catalog).unwrap();
        let state = f.state.clone();
        let task = tokio::spawn(async move { decide(&state, request(), None).await });
        entered.notified().await;
        task.abort();
        let _ = task.await;
        assert_eq!(f.state.store.usages(10, None).unwrap()[0].status, 499);
        f.state.config.systemone_timeout = std::time::Duration::from_millis(20);
        assert_eq!(
            f.http(request().to_string()).await.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(f.state.store.usages(10, None).unwrap()[0].status, 504);
        server.abort();
    }
}

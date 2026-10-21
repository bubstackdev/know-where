//! Provider transports, credentials and Messages protocol translation.
mod discovery;
mod limits;
mod oauth;
mod stream;
mod translate;
use crate::{AppState, types::Model};
use axum::{
    Json,
    body::Body,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
pub use oauth::{DeviceCode, PkceLogin, begin_claude, finish_claude, poll_codex, start_codex};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub use translate::{chat_request, responses_request};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelFault {
    pub kind: String,
    pub reset_at: Option<i64>,
    pub message: Option<String>,
}
impl ModelFault {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            reset_at: None,
            message: Some(message.into()),
        }
    }
}
pub struct Attempt {
    pub response: Response,
    pub fault: Option<ModelFault>,
}
#[derive(Clone, Debug, Default)]
pub struct StreamDiagnostics {
    pub failure_kind: Option<String>,
    pub fault: Option<ModelFault>,
}
pub fn error(status: u16, message: &str) -> Response {
    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        Json(json!({"type":"error","error":{"type":"api_error","message":message}})),
    )
        .into_response()
}
fn ok(value: Value) -> Attempt {
    Attempt {
        response: Json(value).into_response(),
        fault: None,
    }
}
fn diagnosed_error(kind: &str, message: &str) -> Attempt {
    let mut response = error(502, message);
    response
        .extensions_mut()
        .insert(std::sync::Arc::new(std::sync::Mutex::new(
            StreamDiagnostics {
                failure_kind: Some(kind.into()),
                fault: None,
            },
        )));
    Attempt {
        response,
        fault: None,
    }
}
pub(crate) fn context_overflow(message: &str) -> bool {
    let s = message.to_lowercase();
    ["", " ", "_", "-"].iter().any(|sep| {
        ["length", "window", "size"]
            .iter()
            .any(|word| s.contains(&format!("context{sep}{word}")))
    }) || [
        "maximum context",
        "too many tokens",
        "prompt too long",
        "prompt is too long",
        "context overflow",
        "context the overflow",
    ]
    .iter()
    .any(|pattern| s.contains(pattern))
        || ["exceed", "exceeds"].iter().any(|verb| {
            ["", "model's ", "models "].iter().any(|model| {
                ["max", "context"]
                    .iter()
                    .any(|target| s.contains(&format!("{verb} the {model}{target}")))
            })
        })
}
pub(crate) fn fault(status: u16, body: &Value) -> Option<ModelFault> {
    let msg = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/detail/message").and_then(Value::as_str))
        .or_else(|| body["detail"].as_str())
        .unwrap_or("");
    let code = body
        .pointer("/error/code")
        .or_else(|| body.pointer("/error/type"))
        .or_else(|| body.pointer("/detail/code"))
        .or_else(|| body.pointer("/detail/type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let lower = msg.to_lowercase();
    let kind = if code == "context_length_exceeded"
        || ((status == 400 || status == 413 || status == 422) && context_overflow(msg))
    {
        "context_overflow"
    } else if [
        "usage_not_included",
        "model_not_found",
        "unsupported_model",
        "model_not_available",
    ]
    .contains(&code)
        || lower.contains("not supported when using codex")
    {
        "plan_rejected"
    } else if status == 429
        || [
            "usage_limit_reached",
            "rate_limit_exceeded",
            "rate_limit_error",
            "insufficient_quota",
        ]
        .contains(&code)
    {
        "limited"
    } else if [500, 502, 503, 504, 529].contains(&status)
        || ["overloaded_error", "server_is_overloaded", "overloaded"].contains(&code)
    {
        "overloaded"
    } else if status == 401 || status == 403 {
        "auth_rejected"
    } else if status == 404 {
        "plan_rejected"
    } else {
        return None;
    };
    Some(ModelFault::new(kind, msg))
}
pub async fn send(
    state: &AppState,
    model: &Model,
    request: Value,
    session_key: &str,
    count_tokens: bool,
    estimated_tokens: u64,
) -> Result<Attempt, ModelFault> {
    send_with_headers(
        state,
        model,
        request,
        session_key,
        count_tokens,
        estimated_tokens,
        &axum::http::HeaderMap::new(),
    )
    .await
}
pub async fn send_with_headers(
    state: &AppState,
    model: &Model,
    request: Value,
    session_key: &str,
    count_tokens: bool,
    estimated_tokens: u64,
    inbound: &axum::http::HeaderMap,
) -> Result<Attempt, ModelFault> {
    send_with_headers_and_query(
        state,
        model,
        request,
        session_key,
        count_tokens,
        estimated_tokens,
        inbound,
        None,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
pub async fn send_with_headers_and_query(
    state: &AppState,
    model: &Model,
    mut request: Value,
    session_key: &str,
    count_tokens: bool,
    estimated_tokens: u64,
    inbound: &axum::http::HeaderMap,
    query: Option<&str>,
) -> Result<Attempt, ModelFault> {
    let provider = model.provider.as_str();
    if count_tokens && provider != "claude" {
        return Ok(ok(json!({"input_tokens":estimated_tokens})));
    }
    let streaming = request["stream"].as_bool().unwrap_or(false);
    let body = match provider {
        "claude" => {
            translate::prepare_claude(&mut request, &model.upstream_model_id, !count_tokens);
            request
        }
        "codex" => responses_request(&request, model, session_key),
        "local" | "openrouter" => chat_request(&request, model, estimated_tokens, session_key),
        _ => return Err(ModelFault::new("unreachable", "Unsupported provider")),
    };
    let mut refresh = false;
    let mut rejected_token: Option<String> = None;
    let response = loop {
        let mut builder = match provider {
            "claude" => {
                let token = match if refresh {
                    oauth::access_rejected(state, "claude", rejected_token.as_deref().unwrap_or(""))
                        .await
                } else {
                    oauth::access(state, "claude", false).await
                } {
                    Ok(t) => t,
                    Err(_) if count_tokens => {
                        return Ok(ok(json!({"input_tokens":estimated_tokens})));
                    }
                    Err(e) => return Err(oauth::refresh_fault(&e)),
                };
                rejected_token = Some(token.access_token.clone());
                let mut b = state
                    .client
                    .post(format!(
                        "{}/v1/messages{}{}",
                        state.config.claude_api_base.trim_end_matches('/'),
                        if count_tokens { "/count_tokens" } else { "" },
                        query.map(|q| format!("?{q}")).unwrap_or_default()
                    ))
                    .bearer_auth(token.access_token);
                for (k, v) in inbound {
                    if k.as_str().starts_with("anthropic-") && k != "anthropic-beta"
                        || k == "accept"
                    {
                        b = b.header(k, v);
                    }
                }
                if !inbound.contains_key("anthropic-version") {
                    b = b.header("anthropic-version", "2023-06-01");
                }
                let mut flags = inbound
                    .get("anthropic-beta")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "oauth-2025-04-20")
                    .collect::<Vec<_>>();
                flags.push("oauth-2025-04-20");
                b.header("anthropic-beta", flags.join(","))
            }
            "codex" => {
                let token = if refresh {
                    oauth::access_rejected(state, "codex", rejected_token.as_deref().unwrap_or(""))
                        .await
                } else {
                    oauth::access(state, "codex", false).await
                }
                .map_err(|e| oauth::refresh_fault(&e))?;
                rejected_token = Some(token.access_token.clone());
                state
                    .client
                    .post(format!(
                        "{}/responses",
                        state.config.codex_api_base.trim_end_matches('/')
                    ))
                    .bearer_auth(token.access_token)
                    .header("ChatGPT-Account-ID", token.account_id.unwrap_or_default())
                    .header("originator", "codex_cli_rs")
                    .header("User-Agent", "codex_cli_rs")
                    .header("OpenAI-Beta", "responses=experimental")
                    .header("session_id", session_key)
            }
            _ => {
                let base = if provider == "openrouter" {
                    model
                        .base_url
                        .as_deref()
                        .unwrap_or(&state.config.openrouter_api_base)
                } else {
                    model
                        .base_url
                        .as_deref()
                        .unwrap_or("http://localhost:8080/v1")
                };
                let mut b = state
                    .client
                    .post(format!("{}/chat/completions", base.trim_end_matches('/')));
                let key = if provider == "openrouter" {
                    state.store.get_secret("openrouter").ok().flatten()
                } else {
                    state
                        .store
                        .get_secret(&format!("model:{}", model.id))
                        .ok()
                        .flatten()
                }
                .or_else(|| model.api_key.clone());
                if provider == "openrouter" && key.as_ref().is_none_or(|k| k.trim().is_empty()) {
                    return Err(ModelFault::new(
                        "auth_rejected",
                        "OpenRouter has no API key configured",
                    ));
                }
                if let Some(key) = key.filter(|k| !k.is_empty()) {
                    b = b.bearer_auth(key);
                }
                b
            }
        };
        builder = builder.json(&body);
        let idle = if provider == "codex" {
            state.config.codex_stream_idle
        } else if provider == "openrouter" {
            state.config.openrouter_stream_idle
        } else {
            state.config.local_stream_idle
        };
        let response = match tokio::time::timeout(idle, builder.send()).await {
            Ok(Ok(response)) => response,
            other => {
                if provider == "local" {
                    state
                        .runtime
                        .health
                        .write()
                        .await
                        .insert(model.id.clone(), false);
                }
                return Err(ModelFault::new(
                    "unreachable",
                    if other.is_err() {
                        "Upstream response headers timed out"
                    } else {
                        "Upstream connection failed"
                    },
                ));
            }
        };
        if response.status().as_u16() == 401
            && !refresh
            && (provider == "claude" || provider == "codex")
        {
            refresh = true;
            continue;
        }
        break response;
    };
    let status = response.status();
    let headers = response.headers().clone();
    let header_fault = limits::reading(provider, &headers);
    if !status.is_success() {
        let mut bytes = Vec::new();
        let mut s = response.bytes_stream();
        while let Some(chunk) = s.next().await {
            let chunk = chunk
                .map_err(|_| ModelFault::new("unreachable", "Failed reading upstream error"))?;
            if bytes.len() + chunk.len() > 65536 {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(
            json!({"error":{"message":format!("Upstream returned HTTP {}",status.as_u16())}}),
        );
        if count_tokens && (status.as_u16() == 401 || status.as_u16() == 403) {
            return Ok(ok(json!({"input_tokens":estimated_tokens})));
        }
        let mut classified = fault(status.as_u16(), &value);
        if provider == "local" {
            classified = if status.as_u16() == 429 {
                Some(ModelFault::new("limited", "Local endpoint is rate limited"))
            } else if status.as_u16() >= 500 {
                Some(ModelFault::new(
                    "overloaded",
                    "Local endpoint is unavailable",
                ))
            } else {
                classified.filter(|f| f.kind == "context_overflow")
            };
        }
        if provider == "openrouter" {
            classified = match status.as_u16() {
                402 => Some(ModelFault::new(
                    "auth_rejected",
                    "OpenRouter rejected credentials or payment",
                )),
                408 => Some(ModelFault::new("unreachable", "OpenRouter timed out")),
                n if n >= 500 => Some(ModelFault::new("overloaded", "OpenRouter is unavailable")),
                _ => classified,
            };
        }
        if provider == "codex" && status.as_u16() == 404 {
            let code = value
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("");
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("");
            if ![
                "model_not_found",
                "unsupported_model",
                "model_not_available",
                "usage_not_included",
            ]
            .contains(&code)
                && !msg.contains("not supported when using Codex")
            {
                classified = None;
            }
        }
        if provider == "claude" {
            let typ = value
                .pointer("/error/type")
                .and_then(Value::as_str)
                .unwrap_or("");
            let msg = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            if status.as_u16() == 404 && typ != "not_found_error" {
                classified = None;
            }
            if status.as_u16() == 403
                && typ == "permission_error"
                && ["model", "plan", "access to"]
                    .iter()
                    .any(|s| msg.contains(s))
            {
                classified = Some(ModelFault::new("plan_rejected", msg));
            }
        }
        if let Some(mut f) = classified {
            f.reset_at = header_fault
                .as_ref()
                .and_then(|f| f.reset_at)
                .or_else(|| limits::reset(&headers, &value));
            return Err(f);
        }
        if provider == "claude" {
            return Ok(Attempt {
                response: (status, Json(value)).into_response(),
                fault: None,
            });
        }
        let code = status.as_u16();
        let (client_status, kind) = if provider == "local" {
            match code {
                400 | 422 => (400, "invalid_request_error"),
                413 => (413, "request_too_large"),
                _ => (502, "api_error"),
            }
        } else if provider == "openrouter" {
            match code {
                400 | 422 => (400, "invalid_request_error"),
                413 => (413, "api_error"),
                _ => (502, "api_error"),
            }
        } else {
            (
                code,
                match code {
                    400 | 422 => "invalid_request_error",
                    401 => "authentication_error",
                    403 => "permission_error",
                    404 => "not_found_error",
                    413 => "request_too_large",
                    _ => "api_error",
                },
            )
        };
        let message = if provider == "openrouter" {
            format!("OpenRouter rejected the request (HTTP {code}).")
        } else {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/detail/message").and_then(Value::as_str))
                .unwrap_or("Upstream rejected the request")
                .to_owned()
        };
        return Ok(Attempt {
            response: crate::error(client_status, kind, &message),
            fault: None,
        });
    }
    if count_tokens {
        let v = response
            .json()
            .await
            .map_err(|_| ModelFault::new("unreachable", "Malformed token count"))?;
        return Ok(ok(v));
    }
    if provider == "claude" {
        let mut out = if streaming {
            stream::claude(
                response,
                state.config.codex_stream_idle,
                state.config.keepalive_interval,
            )
        } else {
            Response::new(Body::from_stream(response.bytes_stream()))
        };
        *out.status_mut() = status;
        for (k, v) in headers.iter() {
            if ![
                "connection",
                "keep-alive",
                "proxy-authenticate",
                "proxy-authorization",
                "te",
                "trailer",
                "upgrade",
                "transfer-encoding",
                "content-length",
                "content-encoding",
            ]
            .contains(&k.as_str())
            {
                out.headers_mut().insert(k, v.clone());
            }
        }
        return Ok(Attempt {
            response: out,
            fault: header_fault,
        });
    }
    if !streaming && provider != "codex" {
        let value =
            match tokio::time::timeout(state.config.local_stream_idle, response.json::<Value>())
                .await
            {
                Ok(Ok(v)) => v,
                _ => {
                    if provider == "openrouter" {
                        return Err(ModelFault::new(
                            "overloaded",
                            "OpenRouter returned an unreadable response",
                        ));
                    }
                    return Ok(diagnosed_error(
                        "malformed_response",
                        "Local model returned an unreadable response",
                    ));
                }
            };
        if let Some(e) = value.get("error") {
            if provider == "openrouter" {
                let mut f = stream::stream_fault(e, false);
                f.message = Some("OpenRouter reported an error".into());
                return Err(f);
            }
            return Ok(diagnosed_error(
                "upstream_error",
                "Local model reported an error",
            ));
        }
        if !value
            .pointer("/choices/0/message")
            .is_some_and(Value::is_object)
            || value.pointer("/choices/0/finish_reason") == Some(&json!("error"))
        {
            if provider == "openrouter" {
                return Err(ModelFault::new(
                    "overloaded",
                    "OpenRouter returned an invalid response",
                ));
            }
            return Ok(diagnosed_error(
                "malformed_response",
                "Local model returned an invalid response",
            ));
        }
        let mut result = ok(translate::chat_response(
            &value,
            &model.id,
            estimated_tokens,
            provider == "openrouter",
        ));
        result.fault = header_fault;
        return Ok(result);
    }
    let mut result = stream::serve(state, response, model, streaming, estimated_tokens).await?;
    if result.fault.is_none() {
        result.fault = header_fault;
    }
    Ok(result)
}

/// Refresh provider model lists and OpenRouter metadata.
pub async fn refresh(state: &AppState) -> anyhow::Result<Value> {
    discovery::refresh_selected(state, &["claude", "codex", "openrouter"]).await
}

pub async fn refresh_provider(state: &AppState, provider: &str) -> anyhow::Result<Value> {
    if !["claude", "codex", "openrouter"].contains(&provider) {
        anyhow::bail!("Unknown provider");
    }
    discovery::refresh_selected(state, &[provider]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_overflow_matches_reference_phrasings() {
        for message in [
            "context_length exceeded",
            "context-window exceeded",
            "context size limit",
            "contextsize limit",
            "maximum context reached",
            "too many tokens",
            "prompt too long",
            "prompt is too long",
            "context overflow",
            "context the overflow",
            "exceed the model's max",
            "exceeds the models context",
            "exceeds the context",
        ] {
            assert!(context_overflow(message), "{message}");
            for status in [400, 413, 422] {
                assert_eq!(
                    fault(status, &json!({"error":{"message":message}}))
                        .unwrap()
                        .kind,
                    "context_overflow"
                );
            }
        }
        for message in [
            "missing required field",
            "invalid image",
            "contextual information unavailable",
        ] {
            assert!(!context_overflow(message), "{message}");
            assert!(fault(400, &json!({"error":{"message":message}})).is_none());
        }
    }
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[tokio::test]
    async fn claude_preserves_raw_query_protocol_headers_and_filters_credentials_and_hop_headers() {
        let router = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(
                |axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
                 headers: axum::http::HeaderMap| async move {
                    assert_eq!(uri.query(), Some("beta=true&tag=a%2Fb&tag=a+b&empty="));
                    assert_eq!(headers["anthropic-version"], "custom");
                    assert_eq!(headers["anthropic-extra"], "forwarded");
                    assert_eq!(headers["accept"], "application/json");
                    assert_eq!(headers["authorization"], "Bearer managed-access");
                    for name in [
                        "x-api-key",
                        "x-weave-router-key",
                        "x-know-where-key",
                        "proxy-authorization",
                        "connection",
                        "keep-alive",
                        "upgrade",
                        "te",
                    ] {
                        assert!(
                            !headers.contains_key(name),
                            "unexpected forwarded header {name}"
                        );
                    }
                    (
                        [
                            ("content-type", "application/json"),
                            ("connection", "keep-alive"),
                            ("keep-alive", "timeout=5"),
                            ("proxy-authenticate", "Basic"),
                            ("proxy-authorization", "upstream-secret"),
                            ("te", "trailers"),
                            ("trailer", "x-trailer"),
                            ("upgrade", "websocket"),
                            ("anthropic-request-id", "preserved"),
                            ("x-custom-response", "preserved"),
                        ],
                        "{\"type\":\"message\",\"content\":[]}",
                    )
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: crate::Store::open(&dir.path().join("test.db")).unwrap(),
            config: crate::Config {
                claude_api_base: format!("http://{addr}"),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            runtime: Arc::default(),
        };
        state.store.set_secret("claude",&json!({"access_token":"managed-access","refresh_token":"refresh","expires_at":oauth::now()+600,"account_id":null}).to_string()).unwrap();
        let model: Model = serde_json::from_value(
            json!({"id":"claude","provider":"claude","upstreamModelId":"upstream","enabled":true}),
        )
        .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in [
            ("anthropic-version", "custom"),
            ("anthropic-extra", "forwarded"),
            ("accept", "application/json"),
            ("authorization", "Bearer gateway-token"),
            ("x-api-key", "gateway-token"),
            ("x-weave-router-key", "gateway-token"),
            ("x-know-where-key", "gateway-token"),
            ("proxy-authorization", "proxy-secret"),
            ("connection", "close"),
            ("keep-alive", "timeout=1"),
            ("upgrade", "websocket"),
            ("te", "trailers"),
        ] {
            headers.insert(
                axum::http::HeaderName::from_static(name),
                value.parse().unwrap(),
            );
        }
        let served = send_with_headers_and_query(
            &state,
            &model,
            json!({"messages":[]}),
            "s",
            false,
            0,
            &headers,
            Some("beta=true&tag=a%2Fb&tag=a+b&empty="),
        )
        .await
        .unwrap();
        assert_eq!(served.response.status(), 200);
        for name in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "upgrade",
            "transfer-encoding",
            "content-length",
        ] {
            assert!(
                !served.response.headers().contains_key(name),
                "unexpected downstream hop header {name}"
            );
        }
        assert_eq!(
            served.response.headers()["anthropic-request-id"],
            "preserved"
        );
        assert_eq!(served.response.headers()["x-custom-response"], "preserved");
        let _ = axum::body::to_bytes(served.response.into_body(), 10000)
            .await
            .unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn concurrent_claude_401s_refresh_once_and_forward_protocol_headers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let router=axum::Router::new().route("/token",axum::routing::post(move||{let count=count.clone();async move{count.fetch_add(1,Ordering::SeqCst);axum::Json(json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}))}})).route("/v1/messages",axum::routing::post(|headers:axum::http::HeaderMap,axum::Json(v):axum::Json<Value>|async move{assert_eq!(headers["anthropic-version"],"custom-version");assert_eq!(headers["anthropic-beta"],"custom-beta,oauth-2025-04-20");assert_eq!(v["system"][0]["text"],"You are Claude Code, Anthropic's official CLI for Claude.");if headers["authorization"]=="Bearer old-access"{(axum::http::StatusCode::UNAUTHORIZED,axum::Json(json!({"error":{"type":"authentication_error"}})))}else{assert_eq!(headers["authorization"],"Bearer new-access");(axum::http::StatusCode::OK,axum::Json(json!({"type":"message","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"output_tokens":1}})))}}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            claude_api_base: format!("http://{addr}"),
            claude_token_url: format!("http://{addr}/token"),
            ..Default::default()
        };
        let state = AppState {
            store: crate::Store::open(&dir.path().join("test.db")).unwrap(),
            config,
            client: reqwest::Client::new(),
            runtime: Arc::default(),
        };
        state.store.set_secret("claude",&json!({"access_token":"old-access","refresh_token":"old-refresh","expires_at":oauth::now()+600,"account_id":null}).to_string()).unwrap();
        let model: Model = serde_json::from_value(
            json!({"id":"claude","provider":"claude","upstreamModelId":"upstream","enabled":true}),
        )
        .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("anthropic-version", "custom-version".parse().unwrap());
        headers.insert(
            "anthropic-beta",
            "custom-beta,oauth-2025-04-20".parse().unwrap(),
        );
        let results = futures_util::future::join_all((0..8).map(|_| {
            send_with_headers(
                &state,
                &model,
                json!({"messages":[{"role":"user","content":"hi"}]}),
                "session",
                false,
                1,
                &headers,
            )
        }))
        .await;
        for result in results {
            assert_eq!(result.unwrap().response.status(), 200);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
    #[tokio::test]
    async fn local_connect_failure_marks_endpoint_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: crate::Store::open(&dir.path().join("test.db")).unwrap(),
            config: crate::Config::default(),
            client: reqwest::Client::new(),
            runtime: Arc::default(),
        };
        state
            .runtime
            .health
            .write()
            .await
            .insert("local".into(), true);
        let model:Model=serde_json::from_value(json!({"id":"local","provider":"local","upstreamModelId":"upstream","enabled":true,"baseUrl":format!("http://{addr}")})).unwrap();
        assert!(
            send(&state, &model, json!({"messages":[]}), "s", false, 0)
                .await
                .is_err()
        );
        assert_eq!(state.runtime.health.read().await.get("local"), Some(&false));
    }
}

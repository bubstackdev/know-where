use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, State},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use know_where::{AppState, Config, routing, types::*};
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;
#[derive(Clone, Default)]
struct Fake {
    calls: Arc<Mutex<Vec<Value>>>,
}
struct Upstream {
    base: String,
    fake: Fake,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort()
    }
}
async fn upstream() -> Upstream {
    async fn chat(State(s): State<Fake>, Json(v): Json<Value>) -> Response {
        s.calls.lock().unwrap().push(v.clone());
        let model = v["model"].as_str().unwrap_or("");
        if model.starts_with("limit") {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "120")],
                Json(json!({"error":{"type":"rate_limit_error","message":"quota"}})),
            )
                .into_response();
        }
        if model.starts_with("overflow") {
            return(StatusCode::BAD_REQUEST,Json(json!({"error":{"code":"context_length_exceeded","message":"maximum context length exceeded"}}))).into_response();
        }
        if model == "slow" {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        if v["stream"] == true {
            let chunks = if model == "error-first" {
                vec![json!({"error":{"type":"rate_limit_error","message":"quota"}})]
            } else {
                vec![
                    json!({"id":"gen-test","model":model,"choices":[{"index":0,"delta":{"content":"Hello"}}]}),
                    if model == "error-later" {
                        json!({"error":{"type":"rate_limit_error","message":"quota"}})
                    } else {
                        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}})
                    },
                ]
            };
            let mut s = chunks
                .into_iter()
                .map(|v| format!("data: {v}\n\n"))
                .collect::<String>();
            if model != "error-later" {
                s.push_str("data: [DONE]\n\n")
            }
            return ([("content-type", "text/event-stream")], s).into_response();
        }
        Json(json!({"id":"gen-test","model":model,"choices":[{"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}})).into_response()
    }
    let fake = Fake::default();
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(json!({"data":[{"id":"test"}]})) }),
        )
        .route("/v1/chat/completions", post(chat))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Upstream { base, fake, task }
}
fn setup(base: &str, ids: &[&str]) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().into(),
        listen_address: "127.0.0.1".parse().unwrap(),
        stream_start_grace: Duration::from_millis(30),
        keepalive_interval: Duration::from_millis(20),
        ..Default::default()
    };
    let s = AppState::open(config).unwrap();
    let models=ids.iter().map(|id|serde_json::from_value(json!({"id":id,"upstreamModelId":id,"provider":"local","baseUrl":base,"enabled":true})).unwrap()).collect();
    s.store
        .save_catalog(&Catalog {
            models,
            default_tier: "high".into(),
            tiers: std::collections::BTreeMap::from([(
                "high".into(),
                ids.iter().map(|s| (*s).into()).collect(),
            )]),
            aliases: vec![Alias {
                pattern: "*opus*".into(),
                tier: "high".into(),
            }],
            prices: std::collections::BTreeMap::from([(
                ids[0].into(),
                Price {
                    input: 2.,
                    output: 4.,
                    ..Default::default()
                },
            )]),
        })
        .unwrap();
    for id in ids {
        s.runtime
            .health
            .try_write()
            .unwrap()
            .insert((*id).into(), true);
    }
    let _ = s.store.gateway_key().unwrap();
    (dir, s)
}
// Avoid blocking_write in async tests: populate via an uncontended try_write.
fn request(s: &AppState, path: &str, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder()
        .uri(path)
        .header("host", "localhost:8790")
        .header("x-api-key", s.store.gateway_key().unwrap());
    if body.is_some() {
        b = b.method("POST").header("content-type", "application/json")
    }
    let mut req = b
        .body(
            body.map(|v| Body::from(v.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
    req
}
fn message() -> Value {
    json!({"model":"default","max_tokens":1024,"messages":[{"role":"user","content":"Hi"}]})
}
async fn json_body(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn send(s: &AppState, mut v: Value, session: &str) -> (u16, Value) {
    if v.is_null() {
        v = message()
    }
    let mut r = request(s, "/v1/messages", Some(v));
    r.headers_mut()
        .insert("x-claude-code-session-id", session.parse().unwrap());
    let r = know_where::app(s.clone()).oneshot(r).await.unwrap();
    let code = r.status().as_u16();
    (code, json_body(r).await)
}
#[tokio::test]
async fn round_robin_pins_survive_restart_and_sibling_requests_share_turn() {
    let u = upstream().await;
    let (dir, s) = setup(&u.base, &["a", "b"]);
    assert_eq!(send(&s, message(), "one").await.1["model"], "a");
    let mut side = message();
    side["messages"][0]["content"] = json!("title");
    assert_eq!(send(&s, side, "one").await.1["model"], "a");
    assert_eq!(s.store.cursor("high").unwrap(), 1);
    assert_eq!(send(&s, message(), "two").await.1["model"], "b");
    let mut c = s.config.clone();
    c.data_dir = dir.path().into();
    let restarted = AppState::open(c).unwrap();
    restarted
        .runtime
        .health
        .write()
        .await
        .extend([("a".into(), true), ("b".into(), true)]);
    assert_eq!(send(&restarted, message(), "one").await.1["model"], "a");
    assert_eq!(restarted.store.cursor("high").unwrap(), 2);
}
#[tokio::test]
async fn token_count_does_not_pin_rotate_or_record() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["a", "b"]);
    let r = know_where::app(s.clone())
        .oneshot(request(&s, "/v1/messages/count_tokens", Some(message())))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(json_body(r).await["input_tokens"].as_u64().unwrap() > 0);
    assert!(s.store.pins().unwrap().is_empty());
    assert_eq!(s.store.cursor("high").unwrap(), 0);
    assert!(s.store.usages(10, None).unwrap().is_empty());
    assert!(u.fake.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn limited_model_fails_over_and_stays_cooled() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["limit", "b"]);
    let (status, v) = send(&s, message(), "one").await;
    assert_eq!(status, 200);
    assert_eq!(v["model"], "b");
    let cd = s.store.cooldowns().unwrap();
    assert_eq!(cd[0].model_id, "limit");
    assert!(cd[0].until >= now() + 119);
    assert_eq!(s.store.pins().unwrap()[0].model_id, "b");
    let rows = s.store.usages(10, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model_id, "b");
    assert_eq!(rows[0].outcome.as_deref(), Some("completed"));
}
#[tokio::test]
async fn all_overflow_is_compaction_error_without_cooldown() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["overflow-a", "overflow-b"]);
    let (status, v) = send(&s, message(), "one").await;
    assert_eq!(status, 400);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("prompt is too long")
    );
    assert!(s.store.cooldowns().unwrap().is_empty());
    assert_eq!(
        s.store.usages(10, None).unwrap()[0].outcome.as_deref(),
        Some("failed")
    );
}
#[tokio::test]
async fn mixed_context_and_limit_returns_exhaustion() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["overflow", "limit"]);
    let (status, v) = send(&s, message(), "one").await;
    assert_eq!(status, 429);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("model_class_unavailable")
    );
}
#[tokio::test]
async fn streaming_error_before_content_can_failover() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["error-first", "b"]);
    let mut v = message();
    v["stream"] = json!(true);
    let response = know_where::app(s.clone())
        .oneshot(request(&s, "/v1/messages", Some(v)))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = String::from_utf8(
        to_bytes(response.into_body(), 100000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("Hello"));
    assert!(body.contains("message_stop"));
    assert_eq!(u.fake.calls.lock().unwrap().len(), 2);
    assert_eq!(s.store.usages(10, None).unwrap()[0].model_id, "b");
}
#[tokio::test]
async fn committed_stream_error_never_retries_and_records_failed_outcome() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["error-later", "b"]);
    let mut v = message();
    v["stream"] = json!(true);
    let response = know_where::app(s.clone())
        .oneshot(request(&s, "/v1/messages", Some(v)))
        .await
        .unwrap();
    let body = String::from_utf8(
        to_bytes(response.into_body(), 100000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("Hello"));
    assert!(body.contains("event: error"));
    assert!(!body.contains("event: message_stop"));
    assert_eq!(u.fake.calls.lock().unwrap().len(), 1);
    let rows = s.store.usages(10, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome.as_deref(), Some("failed"));
    assert!(
        s.store
            .cooldowns()
            .unwrap()
            .iter()
            .any(|c| c.model_id == "error-later")
    );
}
#[tokio::test]
async fn successful_usage_is_priced_and_available_by_client_session() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["a"]);
    assert_eq!(send(&s, message(), "cost/a").await.0, 200);
    let rows = s.store.usages(10, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].input_tokens, 10);
    assert_eq!(rows[0].output_tokens, 3);
    assert_eq!(rows[0].cost_usd_micros, 32);
    let response = know_where::app(s.clone())
        .oneshot(request(&s, "/v1/sessions/cost%2Fa/cost", None))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let v = json_body(response).await;
    assert_eq!(v["request_count"], 1);
    assert_eq!(v["actual_cost_usd_micros"], 32);
}
#[tokio::test]
async fn cancellation_before_response_is_499_and_releases_capacity() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["slow"]);
    let app = know_where::app(s.clone());
    let req = request(&s, "/v1/messages", Some(message()));
    let task = tokio::spawn(async move { app.oneshot(req).await });
    for _ in 0..100 {
        if !u.fake.calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    let rows = s.store.usages(10, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 499);
    assert_eq!(rows[0].outcome.as_deref(), Some("client_cancelled"));
    assert!(
        s.runtime
            .conversations
            .lock()
            .unwrap()
            .values()
            .all(|slot| slot.lock().unwrap().0 == 0)
    );
}
#[tokio::test]
async fn gateway_all_key_schemes_and_rotation() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["a"]);
    for name in ["x-api-key", "authorization", "x-weave-router-key"] {
        let mut req = request(&s, "/v1/models", None);
        req.headers_mut().remove("x-api-key");
        let key = s.store.gateway_key().unwrap();
        req.headers_mut().insert(
            name,
            if name == "authorization" {
                format!("Bearer {key}")
            } else {
                key
            }
            .parse()
            .unwrap(),
        );
        assert_eq!(
            know_where::app(s.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status(),
            200
        )
    }
    let req = request(&s, "/v1/models", None);
    s.store.rotate_gateway_key().unwrap();
    assert_eq!(
        know_where::app(s.clone())
            .oneshot(req)
            .await
            .unwrap()
            .status(),
        401
    );
}
#[tokio::test]
async fn dashboard_enforces_tcp_host_origin_and_ignores_forwarded_headers() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["a"]);
    for (remote, host, origin, expected) in [
        ("192.168.1.2:55", "localhost:8790", None, 403),
        ("127.0.0.1:55", "evil.example", None, 400),
        (
            "127.0.0.1:55",
            "localhost:8790",
            Some("http://evil.example:8790"),
            400,
        ),
        (
            "127.0.0.1:55",
            "localhost:8790",
            Some("http://localhost:9999"),
            400,
        ),
        (
            "127.0.0.1:55",
            "localhost:8790",
            Some("http://localhost:8790"),
            200,
        ),
    ] {
        let mut req = request(&s, "/ui", None);
        req.extensions_mut()
            .insert(ConnectInfo(remote.parse::<SocketAddr>().unwrap()));
        req.headers_mut().insert("host", host.parse().unwrap());
        req.headers_mut()
            .insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
        if let Some(origin) = origin {
            req.headers_mut().insert("origin", origin.parse().unwrap());
        }
        assert_eq!(
            know_where::app(s.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status()
                .as_u16(),
            expected
        );
    }
}
#[tokio::test]
async fn malformed_duplicate_keys_and_invalid_surrogates_are_400() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["a"]);
    for text in [
        r#"{"model":"a","model":"b"}"#,
        r#"{"messages":[{"role":"user","content":"\ud800"}]}"#,
        r#"[]"#,
        r#"null"#,
        r#"{"#,
    ] {
        let mut req = request(&s, "/v1/messages", Some(message()));
        *req.body_mut() = Body::from(text);
        let r = know_where::app(s.clone()).oneshot(req).await.unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(r.headers()["x-weave-model-class"], "high");
    }
    assert!(u.fake.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn concurrent_first_requests_share_pin_and_one_cursor_step() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["a", "b"]);
    let mut tasks = vec![];
    for _ in 0..12 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            send(&s, message(), "same").await.1["model"].clone()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), "a")
    }
    assert_eq!(s.store.cursor("high").unwrap(), 1);
    assert_eq!(s.store.pins().unwrap().len(), 1);
}
#[tokio::test]
async fn tool_and_context_capabilities_filter_candidates() {
    let u = upstream().await;
    let (_, s) = setup(&u.base, &["a", "b"]);
    let mut c = s.store.catalog().unwrap();
    c.models[0].supports_tools = false;
    s.store.save_catalog(&c).unwrap();
    let mut v = message();
    v["tools"] = json!([{"name":"x","input_schema":{"type":"object"}}]);
    assert_eq!(send(&s, v, "one").await.1["model"], "b");
    c.models[0].supports_tools = true;
    c.models[0].max_prompt_tokens = Some(1);
    s.store.save_catalog(&c).unwrap();
    assert_eq!(send(&s, message(), "two").await.1["model"], "b");
}
#[test]
fn session_identity_ignores_cache_control_and_extracts_bundled_uuid() {
    let h = HeaderMap::new();
    let mut v = message();
    v["metadata"] =
        json!({"user_id":"user_a_account_b_session_12345678-1234-1234-1234-123456789abc"});
    v["messages"][0]["content"] = json!([{"type":"image","source":{"type":"base64","data":"abc"}}]);
    let (a, id) = routing::session_identity(&h, &v);
    assert_eq!(id.as_deref(), Some("12345678-1234-1234-1234-123456789abc"));
    v["messages"][0]["content"][0]["cache_control"] = json!({"type":"ephemeral"});
    assert_eq!(routing::session_identity(&h, &v).0, a);
}
#[test]
fn images_use_bounded_token_estimate() {
    let mut a = message();
    a["messages"][0]["content"] = json!([{"type":"image","source":{"type":"base64","data":"a"}}]);
    let n = routing::estimate_tokens(&a);
    a["messages"][0]["content"][0]["source"]["data"] = json!("A".repeat(100000));
    assert_eq!(routing::estimate_tokens(&a), n);
    assert!(n >= 1600);
}

#[tokio::test]
async fn active_conversation_lease_releases_after_a_model_is_renamed_back() {
    let u = upstream().await;
    let (_dir, s) = setup(&u.base, &["slow"]);
    let app = know_where::app(s.clone());
    let req = request(&s, "/v1/messages", Some(message()));
    let running = tokio::spawn(async move { app.oneshot(req).await });
    for _ in 0..100 {
        if !u.fake.calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!u.fake.calls.lock().unwrap().is_empty());
    for (old, new) in [("slow", "renamed"), ("renamed", "slow")] {
        let mut model = serde_json::to_value(
            s.store
                .catalog()
                .unwrap()
                .models
                .into_iter()
                .find(|m| m.id == old)
                .unwrap(),
        )
        .unwrap();
        model["id"] = json!(new);
        model["existingId"] = json!(old);
        let response = know_where::app(s.clone())
            .oneshot(request(&s, "/ui/api/models", Some(model)))
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", json_body(response).await);
    }
    running.abort();
    let _ = running.await;
    assert!(
        s.runtime
            .conversations
            .lock()
            .unwrap()
            .values()
            .all(|slot| slot.lock().unwrap().0 == 0)
    );
    assert_eq!(
        s.store.usages(10, None).unwrap()[0].outcome.as_deref(),
        Some("client_cancelled")
    );
}

#[tokio::test]
async fn session_and_chart_totals_saturate_when_recorded_costs_exceed_integer_range() {
    let u = upstream().await;
    let (_dir, s) = setup(&u.base, &["a"]);
    for _ in 0..2 {
        s.store
            .record_usage(&Usage {
                at: now(),
                model_id: "a".into(),
                provider: "local".into(),
                client_session_id: Some("huge".into()),
                status: 200,
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                cost_usd_micros: i64::MAX,
                ..Default::default()
            })
            .unwrap();
    }
    for path in ["/v1/sessions/huge/cost", "/ui/api/charts"] {
        let response = know_where::app(s.clone())
            .oneshot(request(&s, path, None))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let value = json_body(response).await;
        assert_eq!(
            value
                .get("actual_cost_usd_micros")
                .or_else(|| value.get("costUsdMicros"))
                .unwrap(),
            i64::MAX
        );
    }
}

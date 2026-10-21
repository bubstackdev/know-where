use axum::{
    Json, Router,
    body::{Body, Bytes},
    http::Request,
    response::IntoResponse,
    routing::post,
};
use futures_util::StreamExt;
use know_where::{AppState, Config, Store, app, types::Catalog};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

struct Fixture {
    state: AppState,
    captured: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(stream: bool) -> Self {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let records = captured.clone();
        let upstream=Router::new().route("/chat/completions",post(move|Json(body):Json<Value>|{let records=records.clone();async move{
   records.lock().unwrap().push(body.clone());
   let prompt=body["messages"].to_string();
   if prompt.contains("CANCEL_RESPONSE"){let stream=async_stream::stream!{yield Ok::<_,std::io::Error>(Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"));tokio::time::sleep(Duration::from_secs(60)).await;yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));};return axum::response::Response::new(Body::from_stream(stream));}
   if prompt.contains("FAIL_RESPONSE"){return ([("content-type","text/event-stream")],"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\ndata: {\"error\":{\"message\":\"synthetic-provider-secret\"}}\n\n").into_response();}
   let tool=body["tools"][0]["function"]["name"].as_str().unwrap_or("lookup");let custom=body["tools"][0]["function"]["parameters"]["properties"].get("input").is_some();let arguments=if custom{json!({"input":"exact custom input\nsecond line"})}else{json!({"q":"rust"})}.to_string();
   if !stream{return ([("content-type","application/json")],json!({"id":"chat","model":"vendor/model","choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string()).into_response();}
   let second=body["messages"].as_array().is_some_and(|a|a.iter().any(|m|m["role"]=="tool"));let mut chunks=vec![json!({"choices":[{"delta":{"content":if second{"finished"}else{"checking"}}}]})];if !second{
    chunks.push(json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","id":"reasoning-1","index":0,"data":"backend opaque reasoning"}]}}]}));
    chunks.push(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call1","function":{"name":tool,"arguments":&arguments[..arguments.len()/2]}}]}}]}));chunks.push(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":&arguments[arguments.len()/2..]}}]}}]}));}
   chunks.push(json!({"choices":[{"delta":{},"finish_reason":if second{"stop"}else{"tool_calls"}}]}));chunks.push(json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}}));let data=chunks.into_iter().map(|c|format!("data: {c}\n\n")).collect::<String>()+"data: [DONE]\n\n";( [("content-type","text/event-stream")],data).into_response()
  }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let catalog:Catalog=serde_json::from_value(json!({"models":[{"id":"mapped","provider":"openrouter","upstreamModelId":"vendor/model","enabled":true,"supportsTools":true,"supportsVision":true}],"tiers":{"high":["mapped"]},"defaultTier":"high","aliases":[]})).unwrap();
        store.save_catalog(&catalog).unwrap();
        store.set_secret("openrouter", "synthetic-key").unwrap();
        let state = AppState {
            store,
            config: Config {
                openrouter_api_base: format!("http://{addr}"),
                stream_start_grace: Duration::from_millis(5),
                ..Default::default()
            },
            client: reqwest::Client::new(),
            runtime: Arc::default(),
        };
        Self {
            state,
            captured,
            server,
            _dir: dir,
        }
    }
    async fn request(&self, body: Value) -> axum::response::Response {
        app(self.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header(
                        "authorization",
                        format!("Bearer {}", self.state.store.gateway_key().unwrap()),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
}
async fn value(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
async fn events(response: axum::response::Response) -> Vec<Value> {
    assert_eq!(response.status(), 200);
    let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn tools() -> Value {
    json!([{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}},"strict":false}])
}

#[tokio::test]
async fn malformed_request_shapes_and_duplicate_fields_fail_before_upstream() {
    let fixture = Fixture::new(false).await;
    for extra in [
        json!({"stream":"true"}),
        json!({"stream":1}),
        json!({"model":""}),
        json!({"input":[]}),
        json!({"input":[{"content":"missing role"}]}),
        json!({"input":[{"role":"user","content":null}]}),
        json!({"input":[{"role":"","content":"bad"}]}),
        json!({"input":[{"role":"user","content":[]}]}),
        json!({"max_output_tokens":"100"}),
        json!({"reasoning":{"effort":null,"summary":"unknown"}}),
        json!({"parallel_tool_calls":"false"}),
        json!({"unknown_feature":true}),
    ] {
        let mut request = json!({"model":"high","input":"hello"});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = fixture.request(request.clone()).await;
        assert_eq!(response.status(), 400, "{request}");
        assert!(value(response).await["error"]["message"].is_string());
    }
    let response = app(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(
                    "authorization",
                    format!("Bearer {}", fixture.state.store.gateway_key().unwrap()),
                )
                .body(Body::from(r#"{"model":"high","input":"a","input":"b"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(fixture.captured.lock().unwrap().is_empty());
    let response = fixture
        .request(json!({"model":"high","input":"hello","stream":null}))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(value(response).await["object"], "response");
}
#[tokio::test]
async fn encrypted_reasoning_tampering_and_key_rotation_are_rejected() {
    let fixture = Fixture::new(true).await;
    let es = events(
        fixture
            .request(json!({"model":"high","input":"hello","tools":tools(),"stream":true}))
            .await,
    )
    .await;
    let reasoning = es.last().unwrap()["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "reasoning")
        .unwrap()
        .clone();
    let mut tampered = reasoning.clone();
    let mut encoded = reasoning["encrypted_content"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec();
    let i = encoded.len() - 10;
    encoded[i] = if encoded[i] == b'A' { b'B' } else { b'A' };
    tampered["encrypted_content"] = json!(String::from_utf8(encoded).unwrap());
    for item in [tampered, reasoning.clone()] {
        if item == reasoning {
            fixture.state.store.rotate_gateway_key().unwrap();
        }
        let response = fixture
            .request(json!({"model":"high","input":[item,{"role":"user","content":"next"}]}))
            .await;
        assert_eq!(response.status(), 400);
    }
    assert_eq!(fixture.captured.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn committed_failure_emits_response_failed_and_never_relays_raw_provider_error() {
    let fixture = Fixture::new(true).await;
    let es = events(
        fixture
            .request(json!({"model":"high","input":"FAIL_RESPONSE","stream":true}))
            .await,
    )
    .await;
    assert!(es.iter().any(|e| e["type"] == "response.output_text.delta"));
    assert_eq!(es.last().unwrap()["type"], "response.failed");
    assert!(!es.iter().any(|e| e["type"] == "response.completed"));
    assert!(!json!(es).to_string().contains("synthetic-provider-secret"));
    let usage = fixture.state.store.usages(10, None).unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].outcome.as_deref(), Some("failed"));
}
#[tokio::test]
async fn dropping_responses_body_cancels_shared_router_stream_and_records_client_cancellation() {
    let fixture = Fixture::new(true).await;
    let response = fixture
        .request(json!({"model":"high","input":"CANCEL_RESPONSE","stream":true}))
        .await;
    assert_eq!(response.status(), 200);
    let mut body = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        std::str::from_utf8(&first)
            .unwrap()
            .contains("response.created")
    );
    drop(body);
    let usage = fixture.state.store.usages(10, None).unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].outcome.as_deref(), Some("client_cancelled"));
}

#[tokio::test]
async fn nonstream_routes_through_shared_gateway_and_translates_developer_images_and_history() {
    let fixture = Fixture::new(false).await;
    let response=fixture.request(json!({"model":"high","instructions":"system instruction","input":[{"role":"developer","content":[{"type":"input_text","text":"developer instruction"}]},{"type":"function_call","call_id":"old","name":"lookup","arguments":"{\"q\":\"before\"}"},{"type":"function_call_output","call_id":"old","output":"result"},{"role":"user","content":[{"type":"input_text","text":"continue"},{"type":"input_image","image_url":"https://example.com/image.png","detail":"auto"}]}],"tools":tools(),"max_output_tokens":100,"stream":false})).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-weave-model-class"], "high");
    let result = value(response).await;
    assert_eq!(result["object"], "response");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["output"][0]["content"][0]["text"], "hello");
    assert_eq!(result["usage"]["total_tokens"], 12);
    let sent = fixture.captured.lock().unwrap()[0].clone();
    assert_eq!(sent["model"], "vendor/model");
    assert_eq!(sent["max_tokens"], 100);
    assert!(
        sent["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("developer instruction")
    );
    assert_eq!(sent["messages"][2]["role"], "tool");
    assert!(sent["messages"].as_array().unwrap().iter().any(|m| {
        m["content"]
            .as_array()
            .is_some_and(|a| a.iter().any(|b| b["type"] == "image_url"))
    }));
    assert_eq!(fixture.state.store.usages(10, None).unwrap().len(), 1);
}
#[tokio::test]
async fn streaming_functions_and_encrypted_reasoning_round_trip_to_provider() {
    let fixture = Fixture::new(true).await;
    let first=events(fixture.request(json!({"model":"high","input":"look up rust","tools":tools(),"stream":true,"store":false,"include":["reasoning.encrypted_content"]})).await).await;
    for (index, e) in first.iter().enumerate() {
        assert_eq!(e["sequence_number"], index as u64);
    }
    assert_eq!(first[0]["type"], "response.created");
    assert!(
        first
            .iter()
            .any(|e| e["type"] == "response.function_call_arguments.delta")
    );
    let final_response = &first.last().unwrap()["response"];
    assert_eq!(final_response["status"], "completed");
    let output = final_response["output"].as_array().unwrap();
    let call = output
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    assert_eq!(call["call_id"], "call1");
    assert_eq!(call["arguments"], "{\"q\":\"rust\"}");
    let reasoning = output.iter().find(|i| i["type"] == "reasoning").unwrap();
    let opaque = reasoning["encrypted_content"].as_str().unwrap();
    assert!(opaque.starts_with("kwr1."));
    assert!(!opaque.contains("backend opaque"));
    let mut history = vec![json!({"role":"user","content":"look up rust"})];
    history.extend(output.iter().cloned());
    history.push(json!({"type":"function_call_output","call_id":"call1","output":"tool result"}));
    let second = events(
        fixture
            .request(json!({"model":"high","input":history,"tools":tools(),"stream":true}))
            .await,
    )
    .await;
    assert_eq!(second.last().unwrap()["type"], "response.completed");
    let captured = fixture.captured.lock().unwrap();
    let replay = captured[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant" && m.get("reasoning_details").is_some())
        .unwrap();
    assert_eq!(
        replay["reasoning_details"][0]["data"],
        "backend opaque reasoning"
    );
    assert!(
        captured[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["content"] == "tool result")
    );
}
#[tokio::test]
async fn namespaces_restore_native_identity_and_custom_text_tools_restore_raw_input() {
    for custom in [false, true] {
        let fixture = Fixture::new(true).await;
        let tool = if custom {
            json!({"type":"custom","name":"patch","format":{"type":"text"}})
        } else {
            json!({"type":"function","name":"lookup","parameters":{"type":"object"}})
        };
        let es=events(fixture.request(json!({"model":"high","input":"run tool","tools":[{"type":"namespace","name":"workspace","description":"Workspace tools","tools":[tool]}],"stream":true})).await).await;
        let item = es.last().unwrap()["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| {
                i["type"]
                    == if custom {
                        "custom_tool_call"
                    } else {
                        "function_call"
                    }
            })
            .unwrap();
        assert_eq!(item["namespace"], "workspace");
        assert_eq!(item["name"], if custom { "patch" } else { "lookup" });
        if custom {
            assert_eq!(item["input"], "exact custom input\nsecond line");
            assert!(
                es.iter()
                    .any(|e| e["type"] == "response.custom_tool_call_input.done")
            );
        }
        assert!(
            fixture.captured.lock().unwrap()[0]["tools"][0]["function"]["name"]
                .as_str()
                .unwrap()
                .starts_with("kw_ns_")
        );
    }
}
#[tokio::test]
async fn unsupported_platform_features_are_explicit_errors_and_do_not_call_upstream() {
    let fixture = Fixture::new(false).await;
    for extra in [
        json!({"store":true}),
        json!({"background":true}),
        json!({"previous_response_id":"resp_old"}),
        json!({"tools":[{"type":"web_search","name":"search"}]}),
        json!({"tools":[{"type":"function","name":"strict","strict":true}]}),
        json!({"tools":[{"type":"custom","name":"grammar","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}]}),
        json!({"input":[{"type":"item_reference","id":"old"}]}),
        json!({"input":[{"role":"user","content":[{"type":"input_file","file_id":"file1"}]}]}),
        json!({"text":{"format":{"type":"json_schema","schema":{}}}}),
    ] {
        let mut request = json!({"model":"high","input":"hello"});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = fixture.request(request).await;
        assert_eq!(response.status(), 400);
        assert!(value(response).await["error"]["message"].is_string());
    }
    assert!(fixture.captured.lock().unwrap().is_empty());
}
#[tokio::test]
async fn tampered_or_foreign_reasoning_is_rejected_before_upstream() {
    let fixture = Fixture::new(false).await;
    let response=fixture.request(json!({"model":"high","input":[{"type":"reasoning","encrypted_content":"opaque-native-platform-blob"},{"role":"user","content":"hello"}]})).await;
    assert_eq!(response.status(), 400);
    assert!(
        value(response).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("start a new client session")
    );
    assert!(fixture.captured.lock().unwrap().is_empty());
}

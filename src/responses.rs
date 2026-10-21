//! Stateless HTTP Responses compatibility over the shared Messages router.
//!
//! Tool execution remains in the client. Platform-hosted tools, stored response
//! IDs, background jobs and grammar/strict schema enforcement are rejected.
use crate::AppState;
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use futures_util::StreamExt;
use rand::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

const MAX_BODY: usize = 32 * 1024 * 1024;
const REASONING_AAD: &[u8] = b"knowwhere.responses.reasoning.v1";
type Result<T> = std::result::Result<T, String>;
fn string<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{name} must be a nonempty string"))
}
fn id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}
fn error(status: u16, message: &str) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),Json(json!({"error":{"type":if status<500{"invalid_request_error"}else{"server_error"},"code":"unsupported_request","message":message,"param":null}}))).into_response()
}

#[derive(Clone, Debug)]
struct Tool {
    wire_name: String,
    name: String,
    namespace: Option<String>,
    custom: bool,
}
impl Tool {
    fn new(name: &str, namespace: Option<&str>, custom: bool) -> Self {
        let wire_name = if let Some(ns) = namespace {
            let digest = Sha256::digest(format!("{ns}\0{name}").as_bytes());
            format!("kw_ns_{}", hex::encode(&digest[..12]))
        } else {
            name.into()
        };
        Self {
            wire_name,
            name: name.into(),
            namespace: namespace.map(str::to_owned),
            custom,
        }
    }
    fn key(&self) -> (Option<String>, String) {
        (self.namespace.clone(), self.name.clone())
    }
    fn output(&self, call_id: &str, item_id: &str, status: &str) -> Value {
        let mut out = if self.custom {
            json!({"type":"custom_tool_call","id":item_id,"status":status,"call_id":call_id,"name":self.name,"input":""})
        } else {
            json!({"type":"function_call","id":item_id,"status":status,"call_id":call_id,"name":self.name,"arguments":""})
        };
        if let Some(ns) = &self.namespace {
            out["namespace"] = json!(ns);
        }
        out
    }
}
struct Request {
    original: Value,
    message: Value,
    tools: HashMap<String, Tool>,
    key: [u8; 32],
}
fn reasoning_key(state: &AppState) -> Result<[u8; 32]> {
    let gateway = state
        .store
        .gateway_key()
        .map_err(|_| "Gateway key is unavailable".to_owned())?;
    let mut proposed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut proposed);
    let stored = state
        .store
        .get_or_create_secret("responses:reasoning_key", &URL_SAFE_NO_PAD.encode(proposed))
        .map_err(|_| "Reasoning encryption key is unavailable".to_owned())?;
    let secret = URL_SAFE_NO_PAD
        .decode(stored)
        .map_err(|_| "Reasoning encryption key is invalid")?;
    if secret.len() != 32 {
        return Err("Reasoning encryption key is invalid".into());
    }
    let mut digest = Sha256::new();
    digest.update(REASONING_AAD);
    digest.update(secret);
    digest.update(gateway.as_bytes());
    Ok(digest.finalize().into())
}
fn seal(key: &[u8; 32], blocks: &[Value]) -> Result<String> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| "Invalid reasoning key")?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let plain = serde_json::to_vec(&json!({"v":1,"blocks":blocks}))
        .map_err(|_| "Could not encode reasoning")?;
    let encrypted = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &plain,
                aad: REASONING_AAD,
            },
        )
        .map_err(|_| "Could not encrypt reasoning")?;
    let mut data = nonce.to_vec();
    data.extend(encrypted);
    Ok(format!("kwr1.{}", URL_SAFE_NO_PAD.encode(data)))
}
fn unseal(key: &[u8; 32], encoded: &str) -> Result<Vec<Value>> {
    let reject = || {
        "Encrypted reasoning is not from this gateway or its key has changed; start a new client session".to_owned()
    };
    let data = URL_SAFE_NO_PAD
        .decode(encoded.strip_prefix("kwr1.").ok_or_else(reject)?)
        .map_err(|_| reject())?;
    if data.len() < 28 {
        return Err(reject());
    }
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| reject())?;
    let plain = cipher
        .decrypt(
            (&data[..12]).into(),
            Payload {
                msg: &data[12..],
                aad: REASONING_AAD,
            },
        )
        .map_err(|_| reject())?;
    let value: Value = serde_json::from_slice(&plain).map_err(|_| reject())?;
    if value["v"] != 1 {
        return Err(reject());
    }
    value["blocks"].as_array().cloned().ok_or_else(reject)
}

fn tools(definitions: &Value) -> Result<(Vec<Value>, HashMap<String, Tool>)> {
    let mut translated = Vec::new();
    let mut mapped = HashMap::new();
    let mut identities = std::collections::HashSet::new();
    fn add(
        tool: &Value,
        namespace: Option<&str>,
        ns_description: Option<&str>,
        out: &mut Vec<Value>,
        mapped: &mut HashMap<String, Tool>,
        identities: &mut std::collections::HashSet<(Option<String>, String)>,
    ) -> Result<()> {
        let name = string(tool, "name")?;
        if name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(
                "Tool names must contain 1-64 letters, digits, underscores or hyphens".into(),
            );
        }
        let typ = string(tool, "type")?;
        if typ == "namespace" {
            if namespace.is_some() {
                return Err("Nested tool namespaces are not supported".into());
            }
            let nested = tool["tools"]
                .as_array()
                .ok_or("Namespace tools must be an array")?;
            for child in nested {
                add(
                    child,
                    Some(name),
                    tool["description"].as_str(),
                    out,
                    mapped,
                    identities,
                )?;
            }
            return Ok(());
        }
        if !["function", "custom"].contains(&typ) {
            return Err(format!("Hosted or unsupported tool type: {typ}"));
        }
        for option in ["async", "defer_loading"] {
            if tool[option] == true {
                return Err(format!("Tool option {option}=true is unsupported"));
            }
        }
        if tool["strict"] == true {
            return Err("Strict function schema enforcement is unsupported by the translated provider; set strict=false".into());
        }
        if let Some(callers) = tool["allowed_callers"].as_array()
            && callers.iter().any(|v| v != "direct")
        {
            return Err("Programmatic tool callers are unsupported".into());
        }
        let custom = typ == "custom";
        if custom
            && tool
                .get("format")
                .is_some_and(|f| !f.is_null() && f["type"] != "text")
        {
            return Err(
                "Custom tool grammars are unsupported; use an unconstrained text custom tool"
                    .into(),
            );
        }
        let item = Tool::new(name, namespace, custom);
        if !identities.insert(item.key()) {
            return Err("Duplicate tool name in the same namespace".into());
        }
        let mut description = tool["description"].as_str().unwrap_or("").to_owned();
        if let Some(ns) = namespace {
            description = format!(
                "Namespace {ns}: {}\n\n{description}",
                ns_description.unwrap_or("")
            );
        }
        let schema = if custom {
            description
                .push_str("\nProvide the exact custom tool input as the string field input.");
            json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false})
        } else {
            tool.get("parameters")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or(json!({"type":"object","properties":{}}))
        };
        if mapped.contains_key(&item.wire_name) {
            return Err("Tool name collides with an encoded namespace tool".into());
        }
        out.push(json!({"name":item.wire_name,"description":description,"input_schema":schema}));
        mapped.insert(item.wire_name.clone(), item);
        Ok(())
    }
    if !definitions.is_null() {
        for t in definitions.as_array().ok_or("tools must be an array")? {
            add(t, None, None, &mut translated, &mut mapped, &mut identities)?;
        }
    }
    Ok((translated, mapped))
}
fn content(v: &Value) -> Result<Vec<Value>> {
    if let Some(s) = v.as_str() {
        return Ok(vec![json!({"type":"text","text":s})]);
    }
    let mut out = Vec::new();
    for part in v
        .as_array()
        .ok_or("Message content must be a string or array")?
    {
        match part["type"].as_str(){Some("input_text"|"output_text"|"text")=>{let s=part["text"].as_str().ok_or("Text content requires text")?;out.push(json!({"type":"text","text":s}));},Some("refusal")=>out.push(json!({"type":"text","text":part["refusal"].as_str().ok_or("Refusal requires text")?})),Some("input_image")=>{
  if part.get("file_id").is_some_and(|v|!v.is_null()){return Err("Platform file IDs are unsupported; provide image_url or a base64 data URL".into());}
  if part.get("detail").is_some_and(|v|!v.is_null()&&v!="auto"){return Err("Only automatic image detail is supported by the translated provider".into());}
  let url=string(part,"image_url")?;let source=if let Some((header,data))=url.strip_prefix("data:").and_then(|v|v.split_once(',')){let media=header.strip_suffix(";base64").filter(|v|v.starts_with("image/")).ok_or("Image data URL must contain base64 image data")?;STANDARD.decode(data).map_err(|_|"Image data URL contains invalid base64")?;json!({"type":"base64","media_type":media,"data":data})}else{let parsed=url::Url::parse(url).map_err(|_|"Invalid image URL")?;if !["http","https"].contains(&parsed.scheme()){return Err("Image URLs must use HTTP or HTTPS".into());}json!({"type":"url","url":url})};out.push(json!({"type":"image","source":source}));
 },Some(other)=>return Err(format!("Unsupported input content type: {other}")),None=>return Err("Input content type is required".into())}
    }
    Ok(out)
}
fn push(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
        last["content"].as_array_mut().unwrap().extend(blocks);
    } else {
        messages.push(json!({"role":role,"content":blocks}));
    }
}
fn validate_request(v: &Value) -> Result<()> {
    const FIELDS: [&str; 24] = [
        "model",
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "reasoning",
        "store",
        "background",
        "include",
        "text",
        "temperature",
        "top_p",
        "max_output_tokens",
        "metadata",
        "client_metadata",
        "prompt_cache_key",
        "previous_response_id",
        "conversation",
        "prompt",
        "context_management",
        "truncation",
        "service_tier",
        "stream",
    ];
    let object = v.as_object().ok_or("Request body must be a JSON object")?;
    for field in object.keys() {
        if !FIELDS.contains(&field.as_str()) {
            return Err(format!("Unsupported Responses request parameter: {field}"));
        }
    }
    string(v, "model")?;
    for field in ["stream", "store", "background", "parallel_tool_calls"] {
        if v.get(field)
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return Err(format!("{field} must be a boolean or null"));
        }
    }
    if v.get("max_output_tokens")
        .is_some_and(|n| !n.is_null() && n.as_u64().is_none_or(|n| n < 16))
    {
        return Err("max_output_tokens must be an integer of at least16".into());
    }
    for field in ["metadata", "client_metadata", "text", "reasoning"] {
        if v.get(field)
            .is_some_and(|value| !value.is_null() && !value.is_object())
        {
            return Err(format!("{field} must be an object or null"));
        }
    }
    if v.get("prompt_cache_key")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err("prompt_cache_key must be a string or null".into());
    }
    if v.get("tool_choice")
        .is_some_and(|value| !value.is_null() && !value.is_string() && !value.is_object())
    {
        return Err("tool_choice must be a string or object".into());
    }
    if let Some(reasoning) = v["reasoning"].as_object() {
        for (name, value) in reasoning {
            match name.as_str() {
                "effort" => {
                    if !value.is_null()
                        && !value.as_str().is_some_and(|s| {
                            ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                                .contains(&s)
                        })
                    {
                        return Err("Unknown reasoning effort".into());
                    }
                }
                "summary" => {
                    if !value.is_null()
                        && !value
                            .as_str()
                            .is_some_and(|s| ["auto", "concise", "detailed"].contains(&s))
                    {
                        return Err("Unknown reasoning summary style".into());
                    }
                }
                _ => return Err(format!("Unsupported reasoning parameter: {name}")),
            }
        }
    }
    if let Some(text) = v["text"].as_object() {
        for (name, value) in text {
            match name.as_str() {
                "format" => {
                    if !value.is_null() && (!value.is_object() || value["type"] != "text") {
                        return Err("Structured Responses text formats are unsupported".into());
                    }
                }
                "verbosity" => {
                    if !value.is_null()
                        && !value
                            .as_str()
                            .is_some_and(|s| ["low", "medium", "high"].contains(&s))
                    {
                        return Err("Unknown text verbosity".into());
                    }
                }
                _ => return Err(format!("Unsupported text parameter: {name}")),
            }
        }
    }
    Ok(())
}
fn from_request(state: &AppState, original: Value) -> Result<Request> {
    validate_request(&original)?;
    if !original.is_object() {
        return Err("Request body must be a JSON object".into());
    }
    for name in [
        "previous_response_id",
        "conversation",
        "prompt",
        "context_management",
    ] {
        if original.get(name).is_some_and(|v| !v.is_null()) {
            return Err(format!(
                "{name} requires platform state and is unsupported; send the full input history"
            ));
        }
    }
    for name in ["background", "store"] {
        if original[name] == true {
            return Err(format!(
                "{name}=true is unsupported; this gateway serves stateless requests"
            ));
        }
    }
    if original
        .get("truncation")
        .is_some_and(|v| !v.is_null() && v != "disabled")
    {
        return Err("Automatic server-side truncation is unsupported".into());
    }
    if original
        .get("service_tier")
        .is_some_and(|v| !v.is_null() && v != "auto" && v != "default")
    {
        return Err("Requested service_tier is unsupported".into());
    }
    if original.get("text").is_some_and(|v| {
        v.get("format")
            .is_some_and(|f| !f.is_null() && f["type"] != "text")
    }) {
        return Err("Structured Responses text formats are unsupported".into());
    }
    if original.get("include").is_some_and(|v| {
        !v.is_null()
            && v.as_array()
                .is_none_or(|a| a.iter().any(|v| v != "reasoning.encrypted_content"))
    }) {
        return Err("Only include=reasoning.encrypted_content is supported".into());
    }
    let key = reasoning_key(state)?;
    let (translated, tools) = tools(&original["tools"])?;
    let mut system = Vec::new();
    let mut messages = Vec::new();
    if !original["instructions"].is_null() {
        system.extend(content(&original["instructions"])?);
    }
    if system.iter().any(|block| block["type"] != "text") {
        return Err("instructions must contain text only".into());
    }
    if let Some(verbosity) = original.pointer("/text/verbosity").and_then(Value::as_str) {
        system.push(
            json!({"type":"text","text":format!("Requested response verbosity: {verbosity}.")}),
        );
    }
    let input = if let Some(s) = original["input"].as_str() {
        vec![json!({"role":"user","content":s})]
    } else {
        original["input"]
            .as_array()
            .cloned()
            .ok_or("input must be a string or an array")?
    };
    for item in input {
        let typ = item["type"].as_str().unwrap_or("message");
        match typ {
            "message" => {
                let role = string(&item, "role")?;
                let blocks = content(&item["content"])?;
                match role {
                    "system" | "developer" => {
                        if blocks.iter().any(|b| b["type"] != "text") {
                            return Err(
                                "System and developer content must contain text only".into()
                            );
                        }
                        system.extend(blocks);
                    }
                    "user" | "assistant" => push(&mut messages, role, blocks),
                    _ => return Err(format!("Unsupported input role: {role}")),
                }
            }
            "function_call" | "custom_tool_call" => {
                let name = string(&item, "name")?;
                let call_id = string(&item, "call_id")?;
                let namespace = item["namespace"].as_str();
                let custom = typ == "custom_tool_call";
                let tool = Tool::new(name, namespace, custom);
                let input = if custom {
                    json!({"input":item["input"].as_str().ok_or("Custom tool input must be a string")?})
                } else {
                    let parsed: Value = serde_json::from_str(
                        item["arguments"]
                            .as_str()
                            .ok_or("Function arguments must be a JSON string")?,
                    )
                    .map_err(|_| "Function arguments must be valid JSON")?;
                    if !parsed.is_object() {
                        return Err("Function arguments must be a JSON object".into());
                    }
                    parsed
                };
                push(
                    &mut messages,
                    "assistant",
                    vec![
                        json!({"type":"tool_use","id":call_id,"name":tool.wire_name,"input":input}),
                    ],
                );
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = string(&item, "call_id")?;
                let output = content(&item["output"])?;
                push(
                    &mut messages,
                    "user",
                    vec![json!({"type":"tool_result","tool_use_id":call_id,"content":output})],
                );
            }
            "reasoning" => {
                if let Some(encoded) = item["encrypted_content"].as_str() {
                    push(&mut messages, "assistant", unseal(&key, encoded)?);
                } else {
                    let text = item["summary"]
                        .as_array()
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default();
                    if !text.is_empty() {
                        push(
                            &mut messages,
                            "assistant",
                            vec![json!({"type":"thinking","thinking":text,"signature":"local:"})],
                        );
                    }
                }
            }
            other => return Err(format!("Unsupported Responses input item: {other}")),
        }
    }
    if messages.is_empty() {
        return Err("input must contain at least one user or assistant message".into());
    }
    let mut message = json!({"model":original["model"],"messages":messages,"system":system,"stream":original["stream"]==true,"max_tokens":original["max_output_tokens"].as_u64().unwrap_or(16384)});
    if !message["model"].is_string() {
        return Err("model must be a string".into());
    }
    if message["max_tokens"] == 0 {
        return Err("max_output_tokens must be positive".into());
    }
    if !translated.is_empty() {
        message["tools"] = json!(translated);
    }
    for field in ["temperature", "top_p"] {
        if let Some(value) = original.get(field).filter(|v| !v.is_null()) {
            if !value.is_number() {
                return Err(format!("{field} must be a number"));
            }
            message[field] = value.clone();
        }
    }
    if let Some(effort) = original
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
    {
        if !["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort) {
            return Err("Unknown reasoning effort".into());
        }
        message["output_config"] = json!({"effort":effort});
        message["thinking"] = json!({"type":if effort=="none"{"disabled"}else{"adaptive"}});
    } else if !original["reasoning"].is_null() {
        message["thinking"] = json!({"type":"adaptive"});
    }
    let mut choice = match original["tool_choice"].as_str() {
        Some("none") => json!({"type":"none"}),
        Some("auto") | None if !original["tool_choice"].is_object() => json!({"type":"auto"}),
        Some("required") => json!({"type":"any"}),
        Some(other) => return Err(format!("Unsupported tool_choice: {other}")),
        _ => {
            let c = &original["tool_choice"];
            let typ = string(c, "type")?;
            if !["function", "custom"].contains(&typ) {
                return Err("Only function or custom tool choices are supported".into());
            }
            let wanted = (
                c["namespace"].as_str().map(str::to_owned),
                string(c, "name")?.to_owned(),
            );
            let tool = tools
                .values()
                .find(|t| t.key() == wanted)
                .ok_or("tool_choice names a tool absent from tools")?;
            json!({"type":"tool","name":tool.wire_name})
        }
    };
    if original["parallel_tool_calls"] == false {
        choice["disable_parallel_tool_use"] = json!(true);
    }
    if !tools.is_empty() {
        message["tool_choice"] = choice;
    }
    Ok(Request {
        original,
        message,
        tools,
        key,
    })
}

struct OutputBlock {
    item: Value,
    original: Value,
    args: String,
    closed: bool,
    deferred_invalid: bool,
}
struct Output {
    request: Request,
    id: String,
    created: i64,
    sequence: u64,
    started: bool,
    ended: bool,
    blocks: Vec<OutputBlock>,
    indices: BTreeMap<u64, usize>,
    usage: Value,
    stop: String,
}
impl Output {
    fn new(request: Request) -> Self {
        Self {
            request,
            id: id("resp"),
            created: crate::types::now(),
            sequence: 0,
            started: false,
            ended: false,
            blocks: Vec::new(),
            indices: BTreeMap::new(),
            usage: json!({}),
            stop: "end_turn".into(),
        }
    }
    fn event(&mut self, mut value: Value, out: &mut Vec<Value>) {
        value["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        out.push(value);
    }
    fn snapshot(&self, status: &str) -> Value {
        let input = self.usage["input_tokens"].as_u64().unwrap_or(0);
        let cached = self.usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let written = self.usage["cache_creation_input_tokens"]
            .as_u64()
            .unwrap_or(0);
        let output = self.usage["output_tokens"].as_u64().unwrap_or(0);
        let total_input = input.saturating_add(cached).saturating_add(written);
        let r = &self.request.original;
        json!({"id":self.id,"object":"response","created_at":self.created,"status":status,"background":false,"error":null,"incomplete_details":if status=="incomplete"{json!({"reason":"max_output_tokens"})}else{Value::Null},"instructions":r["instructions"],"model":r["model"],"output":self.blocks.iter().map(|b|b.item.clone()).collect::<Vec<_>>(),"parallel_tool_calls":r["parallel_tool_calls"].as_bool().unwrap_or(true),"reasoning":r.get("reasoning").cloned().unwrap_or(json!({"effort":null,"summary":null})),"store":false,"temperature":r.get("temperature").cloned().unwrap_or(json!(1.0)),"top_p":r.get("top_p").cloned().unwrap_or(json!(1.0)),"text":r.get("text").cloned().unwrap_or(json!({"format":{"type":"text"}})),"tool_choice":r.get("tool_choice").cloned().unwrap_or(json!("auto")),"tools":r.get("tools").cloned().unwrap_or(json!([])),"truncation":"disabled","max_output_tokens":r["max_output_tokens"],"metadata":r.get("metadata").cloned().unwrap_or(json!({})),"usage":if status=="in_progress"{Value::Null}else{json!({"input_tokens":total_input,"input_tokens_details":{"cached_tokens":cached},"output_tokens":output,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":total_input.saturating_add(output)})}})
    }
    fn start(&mut self, out: &mut Vec<Value>) {
        if self.started {
            return;
        }
        self.started = true;
        self.event(
            json!({"type":"response.created","response":self.snapshot("in_progress")}),
            out,
        );
        self.event(
            json!({"type":"response.in_progress","response":self.snapshot("in_progress")}),
            out,
        );
    }
    fn usage(&mut self, value: &Value) {
        if let Some(o) = value.as_object() {
            for (k, v) in o {
                self.usage[k] = v.clone();
            }
        }
    }
    fn block(&mut self, index: u64, block: Value, out: &mut Vec<Value>) -> Result<()> {
        if self.indices.contains_key(&index) {
            return Err("Duplicate upstream content block".into());
        }
        self.start(out);
        let n = self.blocks.len();
        let typ = string(&block, "type")?;
        let item = match typ {
            "text" => {
                json!({"id":id("msg"),"type":"message","status":"in_progress","role":"assistant","content":[{"type":"output_text","text":"","annotations":[],"logprobs":[]}]})
            }
            "thinking" | "redacted_thinking" => {
                json!({"id":id("rs"),"type":"reasoning","summary":if typ=="thinking"{json!([{"type":"summary_text","text":""}])}else{json!([])}})
            }
            "tool_use" => {
                let name = string(&block, "name")?;
                let tool = self
                    .request
                    .tools
                    .get(name)
                    .ok_or_else(|| "Upstream invoked an unknown tool".to_owned())?;
                tool.output(
                    string(&block, "id")?,
                    &id(if tool.custom { "ctc" } else { "fc" }),
                    "in_progress",
                )
            }
            _ => return Err("Unsupported upstream content block".to_owned()),
        };
        self.indices.insert(index, n);
        self.blocks.push(OutputBlock {
            item: item.clone(),
            original: block.clone(),
            args: String::new(),
            closed: false,
            deferred_invalid: false,
        });
        self.event(
            json!({"type":"response.output_item.added","output_index":n,"item":item}),
            out,
        );
        match typ {
            "text" => {
                self.event(json!({"type":"response.content_part.added","item_id":item["id"],"output_index":n,"content_index":0,"part":item["content"][0]}),out);
                if let Some(s) = block["text"].as_str().filter(|s| !s.is_empty()) {
                    self.delta(index, &json!({"type":"text_delta","text":s}), out)?;
                }
            }
            "thinking" => {
                self.event(json!({"type":"response.reasoning_summary_part.added","item_id":item["id"],"output_index":n,"summary_index":0,"part":item["summary"][0]}),out);
                if let Some(s) = block["thinking"].as_str().filter(|s| !s.is_empty()) {
                    self.delta(index, &json!({"type":"thinking_delta","thinking":s}), out)?;
                }
            }
            "tool_use" if block["input"].as_object().is_some_and(|v| !v.is_empty()) => {
                self.delta(
                    index,
                    &json!({"type":"input_json_delta","partial_json":block["input"].to_string()}),
                    out,
                )?;
            }
            _ => {}
        }
        Ok(())
    }
    fn delta(&mut self, index: u64, delta: &Value, out: &mut Vec<Value>) -> Result<()> {
        let n = *self
            .indices
            .get(&index)
            .ok_or("Upstream delta references an unknown block")?;
        let b = &mut self.blocks[n];
        if b.closed {
            return Err("Upstream delta references a closed block".into());
        }
        let item_id = b.item["id"].clone();
        let event = match delta["type"].as_str() {
            Some("text_delta") if b.item["type"] == "message" => {
                let value = delta["text"].as_str().ok_or("Invalid text delta")?;
                let mut text = b.item["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                text.push_str(value);
                b.item["content"][0]["text"] = json!(text);
                b.original["text"] = json!(text);
                Some(
                    json!({"type":"response.output_text.delta","item_id":item_id,"output_index":n,"content_index":0,"delta":value,"logprobs":[]}),
                )
            }
            Some("thinking_delta") if b.item["type"] == "reasoning" => {
                let value = delta["thinking"]
                    .as_str()
                    .ok_or("Invalid reasoning delta")?;
                let mut text = b.item["summary"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                text.push_str(value);
                b.item["summary"][0]["text"] = json!(text);
                b.original["thinking"] = json!(text);
                Some(
                    json!({"type":"response.reasoning_summary_text.delta","item_id":item_id,"output_index":n,"summary_index":0,"delta":value}),
                )
            }
            Some("signature_delta") if b.item["type"] == "reasoning" => {
                let value = delta["signature"]
                    .as_str()
                    .ok_or("Invalid reasoning signature")?;
                let mut signature = b.original["signature"].as_str().unwrap_or("").to_owned();
                signature.push_str(value);
                b.original["signature"] = json!(signature);
                None
            }
            Some("input_json_delta") if b.original["type"] == "tool_use" => {
                let value = delta["partial_json"]
                    .as_str()
                    .ok_or("Invalid tool arguments delta")?;
                b.args.push_str(value);
                if b.item["type"] == "function_call" {
                    b.item["arguments"] = json!(b.args);
                    Some(
                        json!({"type":"response.function_call_arguments.delta","item_id":item_id,"output_index":n,"delta":value}),
                    )
                } else {
                    None
                }
            }
            Some(_) => return Err("Unsupported upstream delta".to_owned()),
            None => return Err("Upstream delta type is missing".into()),
        };
        if let Some(event) = event {
            self.event(event, out);
        }
        Ok(())
    }
    fn close(&mut self, index: u64, out: &mut Vec<Value>) -> Result<()> {
        let n = *self
            .indices
            .get(&index)
            .ok_or("Upstream stop references an unknown block")?;
        if self.blocks[n].closed {
            return Err("Upstream block stopped twice".into());
        }
        let item_id = self.blocks[n].item["id"].clone();
        let typ = self.blocks[n].item["type"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        match typ.as_str() {
            "message" => {
                let part = self.blocks[n].item["content"][0].clone();
                self.event(json!({"type":"response.output_text.done","item_id":item_id,"output_index":n,"content_index":0,"text":part["text"],"logprobs":[]}),out);
                self.event(json!({"type":"response.content_part.done","item_id":item_id,"output_index":n,"content_index":0,"part":part}),out);
            }
            "reasoning" => {
                if let Some(part) = self.blocks[n].item["summary"]
                    .as_array()
                    .and_then(|v| v.first())
                    .cloned()
                {
                    self.event(json!({"type":"response.reasoning_summary_text.done","item_id":item_id,"output_index":n,"summary_index":0,"text":part["text"]}),out);
                    self.event(json!({"type":"response.reasoning_summary_part.done","item_id":item_id,"output_index":n,"summary_index":0,"part":part}),out);
                }
                self.blocks[n].item["encrypted_content"] =
                    json!(seal(&self.request.key, &[self.blocks[n].original.clone()])?);
            }
            "function_call" | "custom_tool_call" => {
                let args = if self.blocks[n].args.is_empty() {
                    self.blocks[n]
                        .original
                        .get("input")
                        .cloned()
                        .unwrap_or(json!({}))
                } else {
                    serde_json::from_str::<Value>(&self.blocks[n].args).unwrap_or(Value::Null)
                };
                if !args.is_object() || typ == "custom_tool_call" && !args["input"].is_string() {
                    self.blocks[n].closed = true;
                    self.blocks[n].deferred_invalid = true;
                    self.blocks[n].item["status"] = json!("incomplete");
                    return Ok(());
                }
                if typ == "custom_tool_call" {
                    let value = args["input"]
                        .as_str()
                        .ok_or("Upstream custom tool did not return the input string")?
                        .to_owned();
                    self.blocks[n].item["input"] = json!(value);
                    self.event(json!({"type":"response.custom_tool_call_input.delta","item_id":item_id,"output_index":n,"delta":value}),out);
                    self.event(json!({"type":"response.custom_tool_call_input.done","item_id":item_id,"output_index":n,"input":value}),out);
                } else {
                    let arguments = args.to_string();
                    self.blocks[n].item["arguments"] = json!(arguments);
                    self.event(json!({"type":"response.function_call_arguments.done","item_id":item_id,"output_index":n,"arguments":arguments}),out);
                }
            }
            _ => return Err("Unsupported upstream output item".to_owned()),
        }
        self.blocks[n].closed = true;
        if self.blocks[n].item.get("status").is_some() {
            self.blocks[n].item["status"] = json!("completed");
        }
        self.event(
            json!({"type":"response.output_item.done","output_index":n,"item":self.blocks[n].item}),
            out,
        );
        Ok(())
    }
    fn feed(&mut self, value: Value) -> Result<Vec<Value>> {
        if self.ended {
            return Err("Upstream sent data after its terminal event".into());
        }
        let mut out = Vec::new();
        match value["type"].as_str() {
            Some("message_start") => {
                self.usage(&value["message"]["usage"]);
                self.start(&mut out);
            }
            Some("content_block_start") => self.block(
                value["index"]
                    .as_u64()
                    .ok_or("Upstream block index is invalid")?,
                value["content_block"].clone(),
                &mut out,
            )?,
            Some("content_block_delta") => self.delta(
                value["index"]
                    .as_u64()
                    .ok_or("Upstream delta index is invalid")?,
                &value["delta"],
                &mut out,
            )?,
            Some("content_block_stop") => self.close(
                value["index"]
                    .as_u64()
                    .ok_or("Upstream stop index is invalid")?,
                &mut out,
            )?,
            Some("message_delta") => {
                self.usage(&value["usage"]);
                if let Some(stop) = value["delta"]["stop_reason"].as_str() {
                    self.stop = stop.into();
                }
            }
            Some("message_stop") => {
                if self.blocks.iter().any(|b| !b.closed) {
                    return Err("Upstream completed with an unfinished content block".into());
                }
                if self.stop != "max_tokens" && self.blocks.iter().any(|b| b.deferred_invalid) {
                    return Err("Upstream returned invalid tool arguments".into());
                }
                self.start(&mut out);
                self.ended = true;
                let status = if self.stop == "max_tokens" {
                    "incomplete"
                } else {
                    "completed"
                };
                self.event(
                    json!({"type":format!("response.{status}"),"response":self.snapshot(status)}),
                    &mut out,
                );
            }
            Some("error") => {
                let kind = value["error"]["type"].as_str().unwrap_or("api_error");
                let code = match kind {
                    "rate_limit_error" => "rate_limit_exceeded",
                    "invalid_request_error" => "invalid_request_error",
                    _ => "server_error",
                };
                out.extend(self.fail(
                    code,
                    match code {
                        "rate_limit_exceeded" => "The upstream provider is rate limited",
                        "invalid_request_error" => "The upstream provider rejected the request",
                        _ => "The upstream provider could not complete the response",
                    },
                ));
            }
            Some("ping") => {}
            Some(_) => return Err("Unsupported upstream event".into()),
            None => return Err("Upstream event type is missing".into()),
        }
        Ok(out)
    }
    fn fail(&mut self, code: &str, message: &str) -> Vec<Value> {
        if self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.start(&mut out);
        for block in &mut self.blocks {
            if !block.closed && block.item.get("status").is_some() {
                block.item["status"] = json!("incomplete");
            }
        }
        self.ended = true;
        let mut response = self.snapshot("failed");
        response["error"] = json!({"code":code,"message":message});
        self.event(
            json!({"type":"response.failed","response":response}),
            &mut out,
        );
        out
    }
    fn message(&mut self, message: Value) -> Result<Value> {
        self.usage(&message["usage"]);
        self.stop = message["stop_reason"].as_str().unwrap_or("end_turn").into();
        let blocks = message["content"]
            .as_array()
            .ok_or("Upstream message content is invalid")?;
        let mut events = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            self.block(index as u64, block.clone(), &mut events)?;
            self.close(index as u64, &mut events)?;
        }
        if self.stop != "max_tokens" && self.blocks.iter().any(|b| b.deferred_invalid) {
            return Err("Upstream returned invalid tool arguments".into());
        }
        self.ended = true;
        Ok(self.snapshot(if self.stop == "max_tokens" {
            "incomplete"
        } else {
            "completed"
        }))
    }
}

#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
    data: Vec<String>,
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.pending.iter().position(|b| *b == b'\n') {
            let line = self.pending.drain(..=i).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)
                .map_err(|_| "Upstream SSE contains invalid UTF-8")?
                .trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    let data = std::mem::take(&mut self.data).join("\n");
                    out.push(
                        serde_json::from_str(&data)
                            .map_err(|_| "Upstream SSE contains invalid JSON")?,
                    );
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).into());
            }
            if self.pending.len() + self.data.iter().map(String::len).sum::<usize>()
                > 4 * 1024 * 1024
            {
                return Err("Upstream SSE event exceeds 4 MB".into());
            }
        }
        if self.pending.len() > 4 * 1024 * 1024 {
            return Err("Upstream SSE event exceeds 4 MB".into());
        }
        Ok(out)
    }
}
fn frame(events: Vec<Value>) -> Bytes {
    Bytes::from(
        events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap_or("error")
                )
            })
            .collect::<String>(),
    )
}

pub async fn create(
    State(state): State<AppState>,
    mut headers: HeaderMap,
    uri: OriginalUri,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY {
        return error(413, "Request body exceeds32 MB");
    }
    let original = match crate::systemone::parse_strict(&body) {
        Ok(v) => v,
        Err(_) => {
            return error(
                400,
                "Request body must be well-formed JSON without duplicate fields",
            );
        }
    };
    let request = match from_request(&state, original) {
        Ok(r) => r,
        Err(message) => return error(400, &message),
    };
    if !headers.contains_key("x-claude-code-session-id")
        && let Some(session) = request
            .original
            .pointer("/client_metadata/session_id")
            .and_then(Value::as_str)
            .or_else(|| request.original["prompt_cache_key"].as_str())
        && let Ok(value) = session.parse()
    {
        headers.insert("x-claude-code-session-id", value);
    }
    let streaming = request.message["stream"] == true;
    let body = match serde_json::to_vec(&request.message) {
        Ok(body) => Bytes::from(body),
        Err(_) => return error(500, "Could not encode translated request"),
    };
    let response = crate::routing::messages(State(state), headers, uri, body).await;
    let (mut parts, body) = response.into_parts();
    parts.headers.remove("content-length");
    let mut output = Output::new(request);
    if !parts.status.is_success() {
        let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => return error(502, "Upstream returned an unreadable error"),
        };
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let message = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("The gateway could not serve this request");
        let code = if crate::providers::context_overflow(message) {
            "context_length_exceeded"
        } else {
            match parts.status.as_u16() {
                401 => "invalid_api_key",
                429 => "rate_limit_exceeded",
                400 | 413 | 422 => "invalid_request_error",
                _ => "server_error",
            }
        };
        parts
            .headers
            .insert("content-type", "application/json".parse().unwrap());
        let message = match code {
            "context_length_exceeded" => "Request exceeds the model's context window",
            "invalid_api_key" => "Gateway authentication failed",
            "rate_limit_exceeded" => "The gateway is rate limited",
            "invalid_request_error" => "The gateway rejected the request",
            _ => "The gateway could not serve this request",
        };
        return Response::from_parts(parts,Body::from(json!({"error":{"type":if code=="invalid_api_key"{"authentication_error"}else if code=="server_error"{"server_error"}else{"invalid_request_error"},"code":code,"message":message,"param":null}}).to_string()));
    }
    if !streaming {
        let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
            Ok(bytes) => bytes,
            Err(_) => return error(502, "Upstream message exceeds the supported size"),
        };
        let message: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => return error(502, "Upstream returned an unreadable message"),
        };
        match output.message(message) {
            Ok(value) => {
                parts
                    .headers
                    .insert("content-type", "application/json".parse().unwrap());
                Response::from_parts(parts, Body::from(value.to_string()))
            }
            Err(e) => error(502, &e),
        }
    } else {
        parts
            .headers
            .insert("content-type", "text/event-stream".parse().unwrap());
        parts
            .headers
            .insert("cache-control", "no-cache".parse().unwrap());
        let stream = async_stream::stream! {
            let mut source=body.into_data_stream();
            let mut decoder=Decoder::default();
            while let Some(chunk)=source.next().await {
                let decoded=match chunk {
                    Ok(bytes)=>decoder.feed(&bytes),
                    Err(_)=>Err("Upstream stream was interrupted".into()),
                };
                let events=match decoded {
                    Ok(events)=>events,
                    Err(message)=>{
                        yield Ok::<Bytes,std::io::Error>(frame(output.fail("server_error",&message)));
                        break;
                    }
                };
                for event in events {
                    if event["type"]=="ping" {
                        yield Ok(Bytes::from_static(b": ping\n\n"));
                        continue;
                    }
                    match output.feed(event) {
                        Ok(events)=>{
                            // Finish the underlying usage observer before sending a
                            // terminal Responses event that makes clients stop reading.
                            if output.ended {
                                while source.next().await.is_some() {}
                            }
                            if !events.is_empty() {
                                yield Ok(frame(events));
                            }
                        }
                        Err(message)=>{
                            yield Ok(frame(output.fail("server_error",&message)));
                            break;
                        }
                    }
                    if output.ended {break;}
                }
                if output.ended {break;}
            }
            if !output.ended {
                yield Ok(frame(output.fail("server_error","Upstream stream ended before completion")));
            }
        };
        Response::from_parts(parts, Body::from_stream(stream))
    }
}
pub async fn compact() -> Response {
    error(
        501,
        "Server-side Responses compaction is unsupported; send client-managed conversation history",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state(dir: &tempfile::TempDir) -> AppState {
        AppState {
            store: crate::Store::open(&dir.path().join("test.db")).unwrap(),
            config: crate::Config::default(),
            client: reqwest::Client::new(),
            runtime: std::sync::Arc::default(),
        }
    }
    #[test]
    fn reasoning_key_is_persistent_server_only_and_bound_to_gateway_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let first = reasoning_key(&state).unwrap();
        assert_eq!(first, reasoning_key(&state).unwrap());
        let known: [u8; 32] = Sha256::digest(state.store.gateway_key().unwrap().as_bytes()).into();
        assert_ne!(first, known);
        let encoded = seal(
            &first,
            &[json!({"type":"thinking","thinking":"private","signature":"local:"})],
        )
        .unwrap();
        assert!(unseal(&known, &encoded).is_err());
        assert_eq!(unseal(&first, &encoded).unwrap()[0]["thinking"], "private");
        state.store.rotate_gateway_key().unwrap();
        assert!(unseal(&reasoning_key(&state).unwrap(), &encoded).is_err());
    }
    #[test]
    fn truncated_tool_arguments_produce_incomplete_instead_of_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let request=from_request(&state,json!({"model":"high","input":"hello","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]})).unwrap();
        let mut output = Output::new(request);
        for event in [
            json!({"type":"message_start","message":{"usage":{}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"lookup","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":10}}),
        ] {
            output.feed(event).unwrap();
        }
        let events = output.feed(json!({"type":"message_stop"})).unwrap();
        assert_eq!(events.last().unwrap()["type"], "response.incomplete");
        assert_eq!(
            events.last().unwrap()["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert_eq!(
            events.last().unwrap()["response"]["output"][0]["status"],
            "incomplete"
        );
    }
}

use super::StreamDiagnostics;
use super::{Attempt, ModelFault, translate};
use crate::{AppState, types::Model};
use axum::{
    Json,
    body::{Body, Bytes},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::{collections::BTreeMap, pin::Pin, time::Duration};
fn diagnose(shared: &Arc<Mutex<StreamDiagnostics>>, kind: &str, fault: Option<&ModelFault>) {
    if let Ok(mut state) = shared.lock() {
        state.failure_kind = Some(kind.into());
        state.fault = fault
            .filter(|f| {
                [
                    "limited",
                    "overloaded",
                    "context_overflow",
                    "plan_rejected",
                    "auth_rejected",
                ]
                .contains(&f.kind.as_str())
            })
            .cloned();
    }
}
fn reader_failure(message: &str) -> &'static str {
    if message.contains("idle timeout") {
        "upstream_timeout"
    } else if message.contains("interrupted") {
        "transport_error"
    } else {
        "malformed_response"
    }
}
type Input = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;
pub(super) fn stream_fault(error: &Value, codex: bool) -> ModelFault {
    let code = error["code"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .unwrap_or("");
    let message = error["message"]
        .as_str()
        .unwrap_or("Upstream stream failed");
    if codex && code == "usage_not_included" {
        return ModelFault::new("limited", message);
    }
    let status = error["code"]
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .or_else(|| code.parse().ok())
        .unwrap_or(0);
    if !codex
        && let Some(f) =
            super::fault(400, &json!({"error":error})).filter(|f| f.kind == "context_overflow")
    {
        return f;
    }
    if ["invalid_prompt", "invalid_request_error", "BadRequestError"].contains(&code)
        || matches!(status, 400 | 422)
    {
        return ModelFault::new("invalid_request_error", message);
    }
    let mut f = super::fault(status, &json!({"error":error})).unwrap_or_else(|| {
        ModelFault::new(
            if codex {
                "upstream_error"
            } else {
                "overloaded"
            },
            message,
        )
    });
    f.reset_at = super::limits::reset(&axum::http::HeaderMap::new(), &json!({"error":error}));
    f
}
fn served_error(f: &ModelFault) -> Response {
    crate::error(
        if f.kind == "invalid_request_error" {
            400
        } else {
            502
        },
        if f.kind == "invalid_request_error" {
            "invalid_request_error"
        } else {
            "api_error"
        },
        f.message.as_deref().unwrap_or("Upstream failed"),
    )
}
#[derive(Default)]
struct ThinkFilter {
    state: u8,
    pending: String,
}
impl ThinkFilter {
    fn feed(&mut self, text: &str) -> (String, String) {
        let mut s = std::mem::take(&mut self.pending);
        s.push_str(text);
        let mut thinking = String::new();
        loop {
            match self.state {
                0 => {
                    let trimmed = s.trim_start();
                    if let Some(rest) = trimmed.strip_prefix("<think>") {
                        s = rest.into();
                        self.state = 1;
                        continue;
                    }
                    if "<think>".starts_with(trimmed) {
                        self.pending = s;
                        return (thinking, String::new());
                    }
                    self.state = 3;
                    return (thinking, s);
                }
                1 => {
                    if let Some((a, b)) = s.split_once("</think>") {
                        thinking.push_str(a);
                        s = b.into();
                        self.state = 2;
                        continue;
                    }
                    let held = (1..="</think>".len().min(s.len()))
                        .rev()
                        .find(|n| s.ends_with(&"</think>"[..*n]))
                        .unwrap_or(0);
                    let split = s.len() - held;
                    thinking.push_str(&s[..split]);
                    self.pending = s[split..].into();
                    return (thinking, String::new());
                }
                2 => {
                    s = s.trim_start().into();
                    if !s.is_empty() {
                        self.state = 3;
                    }
                    return (thinking, s);
                }
                _ => return (thinking, s),
            }
        }
    }
    fn flush(&mut self) -> (String, String) {
        let s = std::mem::take(&mut self.pending);
        if self.state == 1 {
            (s, String::new())
        } else {
            (String::new(), s)
        }
    }
}
struct Reader {
    input: Input,
    pending: Vec<u8>,
    data: Vec<String>,
    eof: bool,
    last_activity: tokio::time::Instant,
}
impl Reader {
    async fn next(&mut self, idle: Duration) -> Result<Option<Value>, String> {
        loop {
            while let Some(i) = self.pending.iter().position(|b| *b == b'\n') {
                let line = self.pending.drain(..=i).collect::<Vec<_>>();
                let line = std::str::from_utf8(&line)
                    .map_err(|_| "Invalid UTF-8 stream")?
                    .trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    if !self.data.is_empty() {
                        let data = std::mem::take(&mut self.data).join("\n");
                        if data.trim() == "[DONE]" {
                            return Ok(Some(json!({"_done":true})));
                        }
                        return serde_json::from_str(&data)
                            .map(Some)
                            .map_err(|_| "Malformed stream event".into());
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    self.data
                        .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
                }
            }
            if self.pending.len() > 4 * 1024 * 1024 {
                return Err("Upstream stream event exceeds size limit".into());
            }
            if self.eof {
                if !self.pending.is_empty() {
                    self.pending.push(b'\n');
                    continue;
                }
                if !self.data.is_empty() {
                    self.pending.push(b'\n');
                    continue;
                }
                return Ok(None);
            }
            match tokio::time::timeout_at(self.last_activity + idle, self.input.next()).await {
                Ok(Some(Ok(b))) => {
                    self.last_activity = tokio::time::Instant::now();
                    self.pending.extend_from_slice(&b);
                }
                Ok(Some(Err(_))) => return Err("Upstream stream interrupted".into()),
                Ok(None) => self.eof = true,
                Err(_) => return Err("Upstream stream idle timeout".into()),
            }
        }
    }
}
#[derive(Default)]
struct Block {
    index: usize,
    kind: String,
    text: String,
    args: String,
    id: String,
    name: String,
    signature: String,
    stopped: bool,
}
struct Translator {
    codex: bool,
    openrouter: bool,
    model: String,
    id: String,
    estimated: u64,
    blocks: BTreeMap<String, Block>,
    order: Vec<String>,
    ended: bool,
    usage: Value,
    stop: String,
    actual_model: Option<Value>,
    generation: Option<Value>,
    think: ThinkFilter,
    reasoning_field: bool,
    details: Vec<Value>,
    calls: BTreeMap<String, Value>,
    chat_finished: bool,
    summary_indices: BTreeMap<String, u64>,
    last_call_key: Option<String>,
}
impl Translator {
    fn close_open(&mut self, events: &mut Vec<Value>) {
        for b in self.blocks.values_mut().filter(|b| !b.stopped) {
            if b.kind == "thinking" && b.signature.is_empty() && !self.codex {
                b.signature = "local:".into();
                events.push(json!({"type":"content_block_delta","index":b.index,"delta":{"type":"signature_delta","signature":"local:"}}));
            }
            events.push(json!({"type":"content_block_stop","index":b.index}));
            b.stopped = true;
        }
    }
    fn new(model: &Model, estimated: u64) -> Self {
        Self {
            codex: model.provider == "codex",
            openrouter: model.provider == "openrouter",
            model: model.id.clone(),
            id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            estimated,
            blocks: BTreeMap::new(),
            order: vec![],
            ended: false,
            usage: json!({"input_tokens":estimated,"output_tokens":0}),
            stop: "end_turn".into(),
            actual_model: None,
            generation: None,
            think: ThinkFilter::default(),
            reasoning_field: false,
            details: vec![],
            calls: BTreeMap::new(),
            chat_finished: false,
            summary_indices: BTreeMap::new(),
            last_call_key: None,
        }
    }
    fn start(&self) -> Value {
        json!({"type":"message_start","message":{"id":self.id,"type":"message","role":"assistant","model":self.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":self.estimated,"output_tokens":0}}})
    }
    fn block(
        &mut self,
        key: String,
        kind: &str,
        id: &str,
        name: &str,
        events: &mut Vec<Value>,
    ) -> usize {
        if self.blocks.get(&key).is_some_and(|b| b.stopped) {
            let archived = self.blocks.remove(&key).unwrap();
            let archive_key = format!("{key}#{}", archived.index);
            self.order[archived.index] = archive_key.clone();
            self.blocks.insert(archive_key, archived);
        }
        if !self.blocks.contains_key(&key) {
            self.close_open(events);
            let index = self.order.len();
            self.order.push(key.clone());
            let content = match kind {
                "thinking" => json!({"type":"thinking","thinking":"","signature":""}),
                "tool_use" => {
                    json!({"type":"tool_use","id":translate::tool_id(id),"name":name,"input":{}})
                }
                _ => json!({"type":"text","text":""}),
            };
            events
                .push(json!({"type":"content_block_start","index":index,"content_block":content}));
            self.blocks.insert(
                key.clone(),
                Block {
                    index,
                    kind: kind.into(),
                    id: content["id"].as_str().unwrap_or("").into(),
                    name: name.into(),
                    ..Block::default()
                },
            );
        }
        self.blocks[&key].index
    }
    fn delta(&mut self, key: String, kind: &str, value: &str, events: &mut Vec<Value>) {
        if value.is_empty() {
            return;
        }
        let i = self.block(
            key.clone(),
            if kind == "thinking_delta" || kind == "signature_delta" {
                "thinking"
            } else {
                "text"
            },
            "",
            "",
            events,
        );
        let b = self.blocks.get_mut(&key).unwrap();
        let field = match kind {
            "thinking_delta" => {
                b.text.push_str(value);
                "thinking"
            }
            "signature_delta" => {
                b.signature.push_str(value);
                "signature"
            }
            "input_json_delta" => {
                b.args.push_str(value);
                "partial_json"
            }
            _ => {
                b.text.push_str(value);
                "text"
            }
        };
        let mut d = json!({"type":kind});
        d[field] = json!(value);
        events.push(json!({"type":"content_block_delta","index":i,"delta":d}));
    }
    fn finish(&mut self, events: &mut Vec<Value>) {
        if self.ended {
            return;
        }
        let (thinking, text) = self.think.flush();
        if !self.reasoning_field {
            self.delta("reasoning".into(), "thinking_delta", &thinking, events);
        }
        self.delta("text".into(), "text_delta", &text, events);
        if !self.codex {
            for (key, call) in std::mem::take(&mut self.calls) {
                self.item_done(key, &call, true, events);
            }
        }
        for (i, detail) in self.details.clone().iter().enumerate() {
            let sig = format!("openrouter:{}", STANDARD.encode(detail.to_string()));
            let key = format!("detail:{i}");
            if detail["type"] == "reasoning.encrypted" {
                self.close_open(events);
                let index = self.order.len();
                self.order.push(key.clone());
                self.blocks.insert(
                    key,
                    Block {
                        index,
                        kind: "redacted_thinking".into(),
                        signature: sig.clone(),
                        ..Block::default()
                    },
                );
                events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"redacted_thinking","data":sig}}));
            } else {
                self.delta(
                    key.clone(),
                    "thinking_delta",
                    detail["text"]
                        .as_str()
                        .or_else(|| detail["summary"].as_str())
                        .unwrap_or(""),
                    events,
                );
                self.delta(key, "signature_delta", &sig, events);
            }
        }
        self.ended = true;
        if (self.codex || self.stop != "max_tokens")
            && self.blocks.values().any(|b| b.kind == "tool_use")
        {
            self.stop = "tool_use".into();
        }
        self.close_open(events);
        let mut event = json!({"type":"message_delta","delta":{"stop_reason":self.stop,"stop_sequence":null},"usage":self.usage});
        if let Some(m) = &self.actual_model {
            event["resolved_model"] = m.clone();
        }
        if let Some(id) = &self.generation {
            event["generation_id"] = id.clone();
        }
        events.push(event);
        events.push(json!({"type":"message_stop"}));
    }
    fn feed(&mut self, v: Value) -> Result<Vec<Value>, ModelFault> {
        let mut events = vec![];
        if v["_done"] == true || v["_eof"] == true {
            if self.codex || !self.chat_finished {
                return Err(ModelFault::new(
                    "truncated_stream",
                    "Upstream stream ended before completion",
                ));
            }
            self.finish(&mut events);
            return Ok(events);
        }
        if !v.is_object() || self.codex && !v["type"].is_string() {
            return Err(ModelFault::new(
                "malformed_response",
                "Upstream sent an invalid stream event",
            ));
        }
        if let Some(error) = v.get("error") {
            let mut f = stream_fault(error, self.codex);
            if self.openrouter {
                f.message = Some("OpenRouter reported a stream error".into());
            }
            return Err(f);
        }
        if self.codex {
            let typ = v["type"].as_str().unwrap_or("");
            let key = format!("item:{}", v["output_index"].as_u64().unwrap_or(0));
            match typ {
                "response.output_item.added" => {
                    let item = &v["item"];
                    if item["type"] == "function_call" {
                        self.calls.insert(key, item.clone());
                    }
                }
                "response.refusal.delta" => {
                    self.stop = "refusal".into();
                    self.delta(
                        key,
                        "text_delta",
                        v["delta"].as_str().unwrap_or(""),
                        &mut events,
                    );
                }
                "response.output_text.delta" => self.delta(
                    key,
                    "text_delta",
                    v["delta"].as_str().unwrap_or(""),
                    &mut events,
                ),
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    let mut value = v["delta"].as_str().unwrap_or("").to_owned();
                    if typ == "response.reasoning_summary_text.delta" {
                        let index = v["summary_index"].as_u64().unwrap_or(0);
                        if self
                            .summary_indices
                            .insert(key.clone(), index)
                            .is_some_and(|old| old != index)
                        {
                            value = format!("\n\n{value}");
                        }
                    }
                    self.delta(key, "thinking_delta", &value, &mut events);
                }
                "response.function_call_arguments.delta" => {
                    let call = self.calls.entry(key).or_insert(json!({}));
                    let mut args = call["arguments"].as_str().unwrap_or("").to_owned();
                    args.push_str(v["delta"].as_str().unwrap_or(""));
                    call["arguments"] = json!(args);
                }
                "response.function_call_arguments.done" => {
                    let call = self.calls.entry(key).or_insert(json!({}));
                    call["arguments"] = v["arguments"].clone();
                }
                "response.output_item.done" => {
                    self.item_done(key, &v["item"], true, &mut events);
                }
                "response.completed" | "response.incomplete" => {
                    let r = &v["response"];
                    if !r.is_object() {
                        return Err(ModelFault::new(
                            "upstream_error",
                            "Malformed completion event",
                        ));
                    }
                    for (index, item) in r["output"].as_array().into_iter().flatten().enumerate() {
                        self.item_done(
                            format!("item:{index}"),
                            item,
                            typ == "response.completed",
                            &mut events,
                        );
                    }
                    if typ == "response.incomplete" {
                        self.calls.clear();
                        self.stop = if r["incomplete_details"]["reason"] == "content_filter" {
                            "refusal".into()
                        } else {
                            "max_tokens".into()
                        };
                    }
                    self.usage = json!({"input_tokens":r["usage"]["input_tokens"].as_u64().unwrap_or(self.estimated),"output_tokens":r["usage"]["output_tokens"].as_u64().unwrap_or(0),"cache_read_input_tokens":r["usage"]["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0)});
                    let cached = self.usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    self.usage["input_tokens"] = json!(
                        self.usage["input_tokens"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_sub(cached)
                    );
                    if r["incomplete_details"]["reason"] == "max_output_tokens" {
                        self.stop = "max_tokens".into();
                    }
                    self.finish(&mut events);
                }
                "response.failed" | "error" => {
                    let e = v
                        .pointer("/response/error")
                        .or_else(|| v.get("error"))
                        .unwrap_or(&v);
                    return Err(stream_fault(e, true));
                }
                _ => {}
            }
        } else {
            if !v["choices"].is_array()
                || v["choices"].as_array().is_some_and(|v| v.is_empty()) && !v["usage"].is_object()
            {
                return Err(ModelFault::new(
                    "malformed_response",
                    "Upstream sent an invalid stream chunk",
                ));
            }
            if let Some(model) = v.get("model") {
                self.actual_model = Some(model.clone());
            }
            if let Some(id) = v.get("id") {
                self.generation = Some(id.clone());
            }
            if let Some(usage) = v.get("usage").filter(|v| v.is_object()) {
                self.usage = translate::usage(usage, self.estimated, self.openrouter);
                if let Some(cost) = usage.get("cost") {
                    self.usage["cost"] = cost.clone();
                }
            }
            for c in v["choices"].as_array().into_iter().flatten() {
                if c["index"].as_u64().unwrap_or(0) != 0 {
                    continue;
                }
                let d = &c["delta"];
                if let Some(parts) = d["reasoning_details"].as_array() {
                    self.reasoning_field |= !parts.is_empty();
                    for fragment in parts {
                        if !fragment.is_object() {
                            continue;
                        }
                        let index = fragment.get("index");
                        let id = fragment.get("id");
                        let existing = self.details.iter_mut().find(|e| {
                            (index.is_some() && e.get("index") == index
                                || index.is_none() && id.is_some() && e.get("id") == id)
                                && (fragment.get("type").is_none()
                                    || e.get("type").is_none()
                                    || fragment.get("type") == e.get("type"))
                                && (id.is_none() || e.get("id").is_none() || e.get("id") == id)
                        });
                        if let Some(existing) = existing {
                            for (k, v) in fragment.as_object().unwrap() {
                                if ["text", "summary", "data", "signature"].contains(&k.as_str())
                                    && let (Some(a), Some(b)) = (existing[k].as_str(), v.as_str())
                                {
                                    existing[k] = json!(format!("{a}{b}"));
                                    continue;
                                }
                                existing[k] = v.clone();
                            }
                        } else {
                            self.details.push(fragment.clone());
                        }
                    }
                }
                if let Some(s) = d["reasoning_content"]
                    .as_str()
                    .or_else(|| d["reasoning"].as_str())
                {
                    self.reasoning_field = true;
                    if self.details.is_empty() {
                        self.delta("reasoning".into(), "thinking_delta", s, &mut events);
                    }
                }
                if let Some(s) = d["content"].as_str() {
                    let (thinking, text) = self.think.feed(s);
                    if !self.reasoning_field {
                        self.delta("reasoning".into(), "thinking_delta", &thinking, &mut events);
                    }
                    self.delta("text".into(), "text_delta", &text, &mut events);
                }
                for call in d["tool_calls"].as_array().into_iter().flatten() {
                    let id = call["id"].as_str().filter(|id| !id.is_empty());
                    let key = if let Some(index) = call["index"].as_u64() {
                        format!("tool:{index:020}")
                    } else if let Some(last) = self.last_call_key.as_ref().filter(|last| {
                        id.is_none()
                            || self.calls.get(*last).and_then(|v| v["call_id"].as_str()) == id
                    }) {
                        last.clone()
                    } else {
                        format!("tool:{:020}", self.calls.len())
                    };
                    self.last_call_key = Some(key.clone());
                    let entry = self
                        .calls
                        .entry(key)
                        .or_insert(json!({"type":"function_call","name":"","arguments":""}));
                    if let Some(id) = id.filter(|_| entry["call_id"].is_null()) {
                        entry["call_id"] = json!(id);
                    }
                    if let Some(name) = call["function"]["name"].as_str() {
                        let mut full = entry["name"].as_str().unwrap_or("").to_owned();
                        full.push_str(name);
                        entry["name"] = json!(full);
                    }
                    if let Some(args) = call["function"]["arguments"].as_str() {
                        let mut full = entry["arguments"].as_str().unwrap_or("").to_owned();
                        full.push_str(args);
                        entry["arguments"] = json!(full);
                    }
                }
                if let Some(reason) = c["finish_reason"].as_str() {
                    if reason == "error" {
                        return Err(ModelFault::new(
                            "overloaded",
                            "Upstream reported a stream error",
                        ));
                    }
                    self.chat_finished = true;
                    self.stop = match reason {
                        "length" => "max_tokens",
                        "content_filter" => "refusal",
                        _ => "end_turn",
                    }
                    .into(); /* usage often follows the finish chunk */
                }
            }
        }
        Ok(events)
    }
    fn item_done(&mut self, key: String, item: &Value, allow_calls: bool, events: &mut Vec<Value>) {
        match item["type"].as_str() {
            Some("function_call") if allow_calls && !self.blocks.contains_key(&key) => {
                let mut call = self.calls.remove(&key).unwrap_or(json!({}));
                for (k, v) in item.as_object().into_iter().flatten() {
                    if !v.is_null() && v.as_str() != Some("") {
                        call[k] = v.clone();
                    }
                }
                if let Some(name) = call["name"].as_str().filter(|s| !s.is_empty()) {
                    self.block(
                        key.clone(),
                        "tool_use",
                        call["call_id"].as_str().unwrap_or(""),
                        name,
                        events,
                    );
                    let args = call["arguments"].as_str().unwrap_or("{}");
                    let args = serde_json::from_str::<Value>(args)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or(json!({}))
                        .to_string();
                    self.delta(key, "input_json_delta", &args, events);
                }
            }
            Some("message") if !self.blocks.contains_key(&key) => {
                for part in item["content"].as_array().into_iter().flatten() {
                    if part["type"] == "refusal" {
                        self.stop = "refusal".into();
                    }
                    let text = part["text"]
                        .as_str()
                        .or_else(|| part["refusal"].as_str())
                        .unwrap_or("");
                    self.delta(key.clone(), "text_delta", text, events);
                }
            }
            Some("reasoning") => {
                if !self.blocks.contains_key(&key) {
                    let summary = item["summary"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default();
                    self.delta(key.clone(), "thinking_delta", &summary, events);
                }
                if self.blocks.get(&key).is_none_or(|b| b.signature.is_empty())
                    && let Some(sig) = item["encrypted_content"].as_str()
                {
                    self.delta(key, "signature_delta", &format!("codex:{sig}"), events);
                }
            }
            _ => {}
        }
    }
    fn message(&self) -> Value {
        let content=self.order.iter().map(|k|{let b=&self.blocks[k];match b.kind.as_str(){"tool_use"=>json!({"type":"tool_use","id":b.id,"name":b.name,"input":serde_json::from_str::<Value>(&b.args).ok().filter(Value::is_object).unwrap_or(json!({}))}),"thinking"=>json!({"type":"thinking","thinking":b.text,"signature":if b.signature.is_empty(){"local:"}else{&b.signature}}),"redacted_thinking"=>json!({"type":"redacted_thinking","data":b.signature}),_=>json!({"type":"text","text":b.text})}}).collect::<Vec<_>>();
        json!({"id":self.id,"type":"message","role":"assistant","model":self.model,"content":content,"stop_reason":self.stop,"stop_sequence":null,"usage":self.usage,"resolved_model":self.actual_model,"generation_id":self.generation})
    }
}
fn format_events(events: &[Value]) -> Bytes {
    Bytes::from(
        events
            .iter()
            .map(|v| {
                format!(
                    "event: {}\ndata: {}\n\n",
                    v["type"].as_str().unwrap_or("message"),
                    v
                )
            })
            .collect::<String>(),
    )
}
fn error_event(f: &ModelFault) -> Value {
    json!({"type":"error","error":{"type":match f.kind.as_str(){"limited"=>"rate_limit_error","overloaded"=>"overloaded_error",_=>"api_error"},"message":f.message.as_deref().unwrap_or("Upstream stream failed")}})
}
pub fn claude(response: reqwest::Response, idle: Duration, keepalive: Duration) -> Response {
    let mut reader = Reader {
        input: Box::pin(response.bytes_stream()),
        pending: vec![],
        data: vec![],
        eof: false,
        last_activity: tokio::time::Instant::now(),
    };
    let diagnostics = Arc::new(Mutex::new(StreamDiagnostics::default()));
    let shared = diagnostics.clone();
    let output = async_stream::stream! {loop{
     let read=reader.next(idle);tokio::pin!(read);let result=loop{tokio::select!{r=&mut read=>break r,_=tokio::time::sleep(keepalive),if !keepalive.is_zero()=>yield Ok::<Bytes,std::io::Error>(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"))}};
     match result{Ok(Some(v))=>{let ended=v["type"]=="message_stop"||v["type"]=="error";if v["type"]=="error"{let f=stream_fault(&v["error"],false);diagnose(&shared,"upstream_error",Some(&f));}yield Ok(format_events(&[v]));if ended{break;}},other=>{let(kind,message)=match other{Err(e)=>(reader_failure(&e),e),_=>("truncated_stream","Claude stream ended before completion".into())};diagnose(&shared,kind,None);yield Ok(format_events(&[error_event(&ModelFault::new("unreachable",message))]));break;}}
    }};
    let mut out = Response::new(Body::from_stream(output));
    out.extensions_mut().insert(diagnostics);
    out.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    out.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-cache"),
    );
    out
}

fn before_commit(
    translator: &mut Translator,
    diagnostics: &Arc<Mutex<StreamDiagnostics>>,
    mut f: ModelFault,
) -> Result<Vec<Value>, ModelFault> {
    let category = if [
        "malformed_response",
        "truncated_stream",
        "upstream_timeout",
        "transport_error",
    ]
    .contains(&f.kind.as_str())
    {
        f.kind.clone()
    } else {
        "upstream_error".into()
    };
    if translator.openrouter && category != "upstream_error" {
        f.kind = if category == "upstream_timeout" || category == "transport_error" {
            "unreachable"
        } else {
            "overloaded"
        }
        .into();
        return Err(f);
    }
    if [
        "limited",
        "context_overflow",
        "overloaded",
        "plan_rejected",
        "auth_rejected",
    ]
    .contains(&f.kind.as_str())
    {
        return Err(f);
    }
    translator.ended = true;
    diagnose(diagnostics, &category, None);
    Ok(vec![error_event(&f)])
}
pub async fn serve(
    state: &AppState,
    response: reqwest::Response,
    model: &Model,
    streaming: bool,
    estimated: u64,
) -> Result<Attempt, ModelFault> {
    let mut reader = Reader {
        input: Box::pin(response.bytes_stream()),
        pending: vec![],
        data: vec![],
        eof: false,
        last_activity: tokio::time::Instant::now(),
    };
    let mut translator = Translator::new(model, estimated);
    let idle = if model.provider == "codex" {
        state.config.codex_stream_idle
    } else if model.provider == "openrouter" {
        state.config.openrouter_stream_idle
    } else {
        state.config.local_stream_idle
    };
    let diagnostics = Arc::new(Mutex::new(StreamDiagnostics::default()));
    if !streaming {
        while !translator.ended {
            let next = match reader.next(idle).await {
                Ok(Some(v)) => translator.feed(v),
                Ok(None) => translator.feed(json!({"_eof":true})),
                Err(e) => Err(ModelFault::new(reader_failure(&e), e)),
            };
            if let Err(f) = next {
                if [
                    "limited",
                    "context_overflow",
                    "overloaded",
                    "plan_rejected",
                    "auth_rejected",
                ]
                .contains(&f.kind.as_str())
                {
                    return Err(f);
                }
                let category = if [
                    "malformed_response",
                    "truncated_stream",
                    "upstream_timeout",
                    "transport_error",
                ]
                .contains(&f.kind.as_str())
                {
                    f.kind.as_str()
                } else {
                    "upstream_error"
                };
                diagnose(&diagnostics, category, None);
                let mut response = served_error(&f);
                response.extensions_mut().insert(diagnostics);
                return Ok(Attempt {
                    response,
                    fault: None,
                });
            }
        }
        return Ok(Attempt {
            response: Json(translator.message()).into_response(),
            fault: None,
        });
    }
    let deadline = tokio::time::Instant::now() + state.config.stream_start_grace;
    let mut prelude = vec![translator.start()];
    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            break;
        }
        let next = match tokio::time::timeout(remain, reader.next(idle)).await {
            Ok(Ok(Some(v))) => translator.feed(v),
            Ok(Ok(None)) => translator.feed(json!({"_eof":true})),
            Ok(Err(e)) => Err(ModelFault::new(reader_failure(&e), e)),

            Err(_) => break,
        };
        let events = match next {
            Ok(events) => events,
            Err(f) => {
                if !translator.codex && f.kind == "invalid_request_error" {
                    diagnose(&diagnostics, "upstream_error", None);
                    let mut response = served_error(&f);
                    response.extensions_mut().insert(diagnostics);
                    return Ok(Attempt {
                        response,
                        fault: None,
                    });
                }
                before_commit(&mut translator, &diagnostics, f)?
            }
        };
        if !events.is_empty() {
            prelude.extend(events);
            break;
        }
    }
    let keepalive = state.config.keepalive_interval;
    let shared = diagnostics.clone();
    let output = async_stream::stream! {
     yield Ok::<Bytes,std::io::Error>(format_events(&prelude));while !translator.ended{
      let read=reader.next(idle);tokio::pin!(read);let result=loop{tokio::select!{r=&mut read=>break r,_=tokio::time::sleep(keepalive),if !keepalive.is_zero()=>yield Ok(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"))}};
      let (next,category)=match result{Ok(Some(v))=>{let result=translator.feed(v);let category=result.as_ref().err().filter(|f|["malformed_response","truncated_stream"].contains(&f.kind.as_str())).map(|f|f.kind.clone()).unwrap_or("upstream_error".into());(result,category)},Ok(None)=>(translator.feed(json!({"_eof":true})),"truncated_stream".into()),Err(e)=>{let category=reader_failure(&e).to_owned();(Err(ModelFault::new(&category,e)),category)}};
      match next{Ok(events)=>{if !events.is_empty(){yield Ok(format_events(&events));}},Err(f)=>{diagnose(&shared,&category,if translator.codex{Some(&f)}else{None});yield Ok(format_events(&[error_event(&f)]));break;}}
     }
    };
    let mut response = Response::new(Body::from_stream(output));
    response.extensions_mut().insert(diagnostics);
    response.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-cache"),
    );
    Ok(Attempt {
        response,
        fault: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn delayed_response(
        parts: Vec<(Duration, Vec<u8>)>,
    ) -> (reqwest::Response, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router=axum::Router::new().route("/",axum::routing::get(move||{let parts=parts.clone();async move{let stream=async_stream::stream!{for(delay,part)in parts{tokio::time::sleep(delay).await;yield Ok::<_,std::io::Error>(Bytes::from(part));}};Response::new(Body::from_stream(stream))}}));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let response = reqwest::get(format!("http://{addr}/")).await.unwrap();
        (response, task)
    }
    #[tokio::test]
    async fn upstream_comments_and_fragmented_events_reset_idle_and_zero_keepalive_disables_pings()
    {
        let completion=b"data: {\"choices\":[{\"delta\":{\"content\":\"alive\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        for comments in [true, false] {
            let parts = if comments {
                let mut p = vec![(Duration::from_millis(10), b": upstream ping\n\n".to_vec()); 10];
                p.push((Duration::from_millis(10), completion.to_vec()));
                p
            } else {
                completion
                    .chunks(8)
                    .map(|part| (Duration::from_millis(10), part.to_vec()))
                    .collect()
            };
            let (response, task) = delayed_response(parts).await;
            let dir = tempfile::tempdir().unwrap();
            let mut state = state(&dir);
            state.config.local_stream_idle = Duration::from_millis(60);
            state.config.stream_start_grace = Duration::from_millis(5);
            state.config.keepalive_interval = Duration::ZERO;
            let served = serve(&state, response, &model("local"), true, 1)
                .await
                .unwrap();
            let diagnostics = served
                .response
                .extensions()
                .get::<Arc<Mutex<StreamDiagnostics>>>()
                .unwrap()
                .clone();
            let bytes = axum::body::to_bytes(served.response.into_body(), 100000)
                .await
                .unwrap();
            let s = std::str::from_utf8(&bytes).unwrap();
            assert!(s.contains("alive") && s.contains("message_stop"), "{s}");
            assert!(
                !s.contains("event: ping"),
                "zero keepalive must disable generated pings"
            );
            assert!(diagnostics.lock().unwrap().failure_kind.is_none());
            task.abort();
        }
    }
    #[tokio::test]
    async fn truly_silent_upstream_times_out_and_nonzero_keepalive_uses_configured_interval() {
        let (response, task) = delayed_response(vec![
            (Duration::ZERO, b": initial\n\n".to_vec()),
            (Duration::from_millis(300), b": too late\n\n".to_vec()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(&dir);
        state.config.local_stream_idle = Duration::from_millis(50);
        state.config.stream_start_grace = Duration::from_millis(1);
        state.config.keepalive_interval = Duration::from_millis(10);
        let served = serve(&state, response, &model("local"), true, 1)
            .await
            .unwrap();
        let diagnostics = served
            .response
            .extensions()
            .get::<Arc<Mutex<StreamDiagnostics>>>()
            .unwrap()
            .clone();
        let bytes = axum::body::to_bytes(served.response.into_body(), 100000)
            .await
            .unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.contains("event: ping"),
            "10ms keepalive should fire before50ms idle timeout"
        );
        assert!(s.contains("idle timeout"));
        assert!(!s.contains("message_stop"));
        assert_eq!(
            diagnostics.lock().unwrap().failure_kind.as_deref(),
            Some("upstream_timeout")
        );
        task.abort();
    }
    #[test]
    fn finish_reason_error_aborts_local_stream() {
        let mut t = Translator::new(&model("local"), 1);
        t.feed(json!({"choices":[{"delta":{"content":"partial"}}]}))
            .unwrap();
        assert!(
            t.feed(json!({"choices":[{"delta":{},"finish_reason":"error"}]}))
                .is_err()
        );
        assert!(!t.ended);
    }
    #[test]
    fn completed_codex_tool_call_survives_later_incomplete_terminal() {
        let mut t = Translator::new(&model("codex"), 1);
        t.feed(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","name":"f","arguments":"{}"}})).unwrap();
        t.feed(json!({"type":"response.incomplete","response":{"output":[],"incomplete_details":{"reason":"max_output_tokens"}}})).unwrap();
        assert_eq!(t.message()["stop_reason"], "tool_use");
    }
    #[test]
    fn openrouter_reasoning_same_index_with_distinct_ids_stays_separate() {
        let mut t = Translator::new(&model("openrouter"), 1);
        for (id, data) in [("a", "AA"), ("b", "BB")] {
            t.feed(json!({"choices":[{"delta":{"reasoning_details":[{"index":0,"id":id,"type":"reasoning.encrypted","data":data}]}}]})).unwrap();
        }
        t.feed(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}))
            .unwrap();
        t.feed(json!({"_done":true})).unwrap();
        assert_eq!(t.message()["content"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn indexless_tool_calls_follow_repeated_ids_and_missing_id_fragments() {
        let mut t = Translator::new(&model("local"), 1);
        for call in [
            json!({"id":"call_a","function":{"name":"Read","arguments":"{\"path\":"}}),
            json!({"id":"call_a","function":{"arguments":"\"a.txt\"}"}}),
            json!({"id":"call_b","function":{"name":"Grep","arguments":"{\"pattern\":"}}),
            json!({"function":{"arguments":"\"TODO\"}"}}),
        ] {
            t.feed(json!({"choices":[{"delta":{"tool_calls":[call]}}]}))
                .unwrap();
        }
        t.feed(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}))
            .unwrap();
        t.feed(json!({"_done":true})).unwrap();
        let m = t.message();
        assert_eq!(m["content"].as_array().unwrap().len(), 2);
        assert_eq!(m["content"][0]["id"], "call_a");
        assert_eq!(m["content"][0]["name"], "Read");
        assert_eq!(m["content"][0]["input"], json!({"path":"a.txt"}));
        assert_eq!(m["content"][1]["id"], "call_b");
        assert_eq!(m["content"][1]["name"], "Grep");
        assert_eq!(m["content"][1]["input"], json!({"pattern":"TODO"}));
    }
    #[test]
    fn chat_requires_finish_reason_but_not_done_marker() {
        let mut t = Translator::new(&model("local"), 1);
        t.feed(json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}))
            .unwrap();
        assert!(
            t.feed(json!({"_eof":true}))
                .unwrap()
                .iter()
                .any(|v| v["type"] == "message_stop")
        );
        let mut t = Translator::new(&model("local"), 1);
        t.feed(json!({"choices":[{"delta":{"content":"partial"}}]}))
            .unwrap();
        assert_eq!(
            t.feed(json!({"_done":true})).unwrap_err().kind,
            "truncated_stream"
        );
    }
    #[test]
    fn codex_summary_parts_have_paragraph_breaks() {
        let mut t = Translator::new(&model("codex"), 1);
        for (index, text) in [(0, "one"), (0, " more"), (1, "two")] {
            t.feed(json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":index,"delta":text})).unwrap();
        }
        t.feed(json!({"type":"response.completed","response":{}}))
            .unwrap();
        assert_eq!(t.message()["content"][0]["thinking"], "one more\n\ntwo");
    }
    #[tokio::test]
    async fn empty_stream_is_served_error_for_codex_and_local_but_openrouter_can_failover() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        for provider in ["codex", "local", "openrouter"] {
            let (r, task) = response(String::new()).await;
            let result = serve(&state, r, &model(provider), true, 1).await;
            if provider == "openrouter" {
                assert!(matches!(result,Err(f)if f.kind=="overloaded"));
            } else {
                let served = result.unwrap();
                let diagnostics = served
                    .response
                    .extensions()
                    .get::<Arc<Mutex<StreamDiagnostics>>>()
                    .unwrap()
                    .clone();
                let body = axum::body::to_bytes(served.response.into_body(), 100000)
                    .await
                    .unwrap();
                assert!(
                    std::str::from_utf8(&body)
                        .unwrap()
                        .contains("ended before completion")
                );
                assert_eq!(
                    diagnostics.lock().unwrap().failure_kind.as_deref(),
                    Some("truncated_stream")
                );
            }
            task.abort();
        }
    }
    fn model(provider: &str) -> Model {
        serde_json::from_value(json!({"id":"m","provider":provider,"upstreamModelId":"u","enabled":true,"supportsTools":true,"supportsVision":false})).unwrap()
    }
    #[test]
    fn fragmented_tool_arguments_remain_one_block() {
        let mut t = Translator::new(&model("local"), 20);
        t.feed(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"test","arguments":"{\"x\":"}}]}}]})).unwrap();
        t.feed(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]})).unwrap();
        t.feed(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}))
            .unwrap();
        t.feed(json!({"_done":true})).unwrap();
        assert_eq!(t.message()["content"][0]["input"], json!({"x":1}));
        assert_eq!(t.message()["stop_reason"], "tool_use");
    }
    #[test]
    fn codex_rate_limit_is_failover_fault() {
        let mut t = Translator::new(&model("codex"), 20);
        let f=t.feed(json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"limited"}}})).unwrap_err();
        assert_eq!(f.kind, "limited");
    }
    #[test]
    fn terminal_codex_output_synthesizes_text_tools_and_encrypted_reasoning() {
        let mut t = Translator::new(&model("codex"), 20);
        let events=t.feed(json!({"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"text":"first"},{"text":"second"}],"encrypted_content":"opaque"},{"type":"message","content":[{"type":"output_text","text":"answer"}]},{"type":"function_call","call_id":"call1","name":"test","arguments":"{\"x\":1}"}],"usage":{"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":30}}}})).unwrap();
        let m = t.message();
        assert_eq!(m["content"][0]["thinking"], "first\n\nsecond");
        assert_eq!(m["content"][0]["signature"], "codex:opaque");
        assert_eq!(m["content"][1]["text"], "answer");
        assert_eq!(m["content"][2]["input"], json!({"x":1}));
        assert_eq!(m["usage"]["input_tokens"], 70);
        assert_eq!(m["stop_reason"], "tool_use");
        let mut open = None;
        for e in events {
            match e["type"].as_str() {
                Some("content_block_start") => {
                    assert!(open.is_none(), "blocks must not interleave");
                    open = e["index"].as_u64();
                }
                Some("content_block_stop") => {
                    assert_eq!(open, e["index"].as_u64());
                    open = None;
                }
                _ => {}
            }
        }
        assert!(open.is_none());
    }
    #[test]
    fn codex_calls_are_buffered_until_done_and_terminal_does_not_duplicate() {
        let mut t = Translator::new(&model("codex"), 20);
        assert!(t.feed(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c","name":"f"}})).unwrap().is_empty());
        assert!(t.feed(json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"a\":"})).unwrap().is_empty());
        t.feed(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","name":"f","arguments":"{\"a\":2}"}})).unwrap();
        t.feed(json!({"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"c","name":"f","arguments":"{\"a\":2}"}]}})).unwrap();
        assert_eq!(t.message()["content"].as_array().unwrap().len(), 1);
        assert_eq!(t.message()["content"][0]["input"], json!({"a":2}));
    }
    #[test]
    fn incomplete_codex_drops_unfinished_call() {
        let mut t = Translator::new(&model("codex"), 20);
        t.feed(json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c","name":"f"}})).unwrap();
        t.feed(json!({"type":"response.incomplete","response":{"output":[{"type":"function_call","name":"f","arguments":"{\"a\":"}],"incomplete_details":{"reason":"max_output_tokens"}}})).unwrap();
        assert!(t.message()["content"].as_array().unwrap().is_empty());
        assert_eq!(t.message()["stop_reason"], "max_tokens");
    }
    #[test]
    fn split_think_tags_preserve_reasoning_without_leaking_tags() {
        let mut t = Translator::new(&model("local"), 20);
        for text in [" \n<th", "ink>secret</th", "ink>\nanswer"] {
            t.feed(json!({"choices":[{"delta":{"content":text}}]}))
                .unwrap();
        }
        t.feed(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}))
            .unwrap();
        t.feed(json!({"_done":true})).unwrap();
        let m = t.message();
        assert_eq!(m["content"][0]["thinking"], "secret");
        assert_eq!(m["content"][1]["text"], "answer");
    }
    #[test]
    fn opaque_openrouter_reasoning_fragments_round_trip() {
        let mut t = Translator::new(&model("openrouter"), 20);
        for data in ["AAA", "BBB"] {
            t.feed(json!({"choices":[{"delta":{"reasoning_details":[{"index":0,"id":"r","type":"reasoning.encrypted","data":data}]}}]})).unwrap();
        }
        t.feed(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}))
            .unwrap();
        t.feed(json!({"_done":true})).unwrap();
        let m = t.message();
        let encoded = m["content"][0]["data"]
            .as_str()
            .unwrap()
            .strip_prefix("openrouter:")
            .unwrap();
        let detail: Value = serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
        assert_eq!(detail["data"], "AAABBB");
        let request = translate::chat_request(
            &json!({"messages":[{"role":"assistant","content":m["content"]}]}),
            &model("openrouter"),
            0,
            "s",
        );
        assert_eq!(request["messages"][0]["reasoning_details"][0], detail);
    }
    async fn response(body: String) -> (reqwest::Response, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let body = body.clone();
                async move { body }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let response = reqwest::get(format!("http://{addr}/")).await.unwrap();
        (response, task)
    }
    fn state(dir: &tempfile::TempDir) -> AppState {
        AppState {
            store: crate::Store::open(&dir.path().join("test.db")).unwrap(),
            config: crate::Config::default(),
            client: reqwest::Client::new(),
            runtime: std::sync::Arc::default(),
        }
    }
    #[tokio::test]
    async fn first_event_limit_fails_over_but_limit_after_content_is_committed_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let failure = json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"limited"}}});
        let (r, task) = response(format!("data: {failure}\n\n")).await;
        let result = serve(&state, r, &model("codex"), true, 20).await;
        assert!(matches!(result,Err(f)if f.kind=="limited"));
        task.abort();
        let text = json!({"type":"response.output_text.delta","delta":"hello"});
        let (r, task) = response(format!("data: {text}\n\ndata: {failure}\n\n")).await;
        let served = serve(&state, r, &model("codex"), true, 20).await.unwrap();
        assert!(served.fault.is_none());
        let bytes = axum::body::to_bytes(served.response.into_body(), 1_000_000)
            .await
            .unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("message_start"));
        assert!(s.contains("hello"));
        assert!(s.contains("rate_limit_error"));
        assert!(!s.contains("message_stop"));
        task.abort();
    }
    #[tokio::test]
    async fn truncated_stream_emits_error_after_content() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let text = json!({"choices":[{"delta":{"content":"hello"}}]});
        let (r, task) = response(format!("data: {text}\n\n")).await;
        let served = serve(&state, r, &model("local"), true, 20).await.unwrap();
        let diagnostics = served
            .response
            .extensions()
            .get::<Arc<Mutex<StreamDiagnostics>>>()
            .unwrap()
            .clone();
        let bytes = axum::body::to_bytes(served.response.into_body(), 1_000_000)
            .await
            .unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("hello"));
        assert!(s.contains("ended before completion"));
        assert!(!s.contains("message_stop"));
        assert_eq!(
            diagnostics.lock().unwrap().failure_kind.as_deref(),
            Some("truncated_stream")
        );
        assert!(diagnostics.lock().unwrap().fault.is_none());
        task.abort();
    }
    #[tokio::test]
    async fn grace_expiry_commits_start_before_delayed_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router=axum::Router::new().route("/",axum::routing::get(||async{let body=async_stream::stream!{tokio::time::sleep(Duration::from_millis(80)).await;yield Ok::<_,std::io::Error>(Bytes::from_static(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"limited\"}}}\n\n"));};Response::new(Body::from_stream(body))}));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(&dir);
        state.config.stream_start_grace = Duration::from_millis(5);
        let response = reqwest::get(format!("http://{addr}/")).await.unwrap();
        let served = serve(&state, response, &model("codex"), true, 20)
            .await
            .unwrap();
        let diagnostics = served
            .response
            .extensions()
            .get::<Arc<Mutex<StreamDiagnostics>>>()
            .unwrap()
            .clone();
        let mut body = served.response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_millis(40), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            std::str::from_utf8(&first)
                .unwrap()
                .contains("message_start")
        );
        let mut rest = Vec::new();
        while let Some(chunk) = body.next().await {
            rest.extend_from_slice(&chunk.unwrap());
        }
        assert!(
            std::str::from_utf8(&rest)
                .unwrap()
                .contains("rate_limit_error")
        );
        let d = diagnostics.lock().unwrap();
        assert_eq!(d.failure_kind.as_deref(), Some("upstream_error"));
        assert_eq!(d.fault.as_ref().unwrap().kind, "limited");
        task.abort();
    }
}

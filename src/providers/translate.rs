use crate::types::Model;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
pub fn text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.into()
    } else {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}
fn system_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        s.into()
    } else {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default()
    }
}
fn blocks(v: &Value) -> Vec<Value> {
    v.as_array()
        .cloned()
        .unwrap_or_else(|| vec![json!({"type":"text","text":v.as_str().unwrap_or("")})])
}
fn image(b: &Value) -> Option<String> {
    let s = &b["source"];
    if s["type"] == "base64" {
        Some(format!(
            "data:{};base64,{}",
            s["media_type"].as_str().unwrap_or("image/png"),
            s["data"].as_str()?
        ))
    } else {
        s["url"].as_str().map(str::to_owned)
    }
}
fn tool_output(b: &Value) -> String {
    let s = text(&b["content"]);
    if b["is_error"] == true {
        format!("Error: {s}")
    } else {
        s
    }
}
fn effort(request: &Value, model: &Model) -> Option<String> {
    const LEVELS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    let mapping = model.effort.as_ref();
    let explicit = request
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
        .and_then(|s| LEVELS.iter().position(|v| v.eq_ignore_ascii_case(s.trim())));
    let requested = explicit
        .or_else(
            || match request.pointer("/thinking/type").and_then(Value::as_str) {
                Some("disabled") => Some(0),
                Some("enabled") => request
                    .pointer("/thinking/budget_tokens")
                    .and_then(Value::as_u64)
                    .map(|n| match n {
                        0..4096 => 2,
                        4096..16384 => 3,
                        16384..32768 => 4,
                        32768..65536 => 5,
                        _ => 6,
                    }),
                _ => None,
            },
        )
        .or_else(|| {
            if model.provider != "codex" {
                mapping
                    .and_then(|m| m["default"].as_str())
                    .and_then(|s| LEVELS.iter().position(|v| *v == s))
            } else {
                None
            }
        })?;
    if model.provider == "local" && mapping.is_none_or(|m| m["mode"] == "none") {
        return None;
    }
    let supported = mapping
        .and_then(|m| m["levels"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().and_then(|s| LEVELS.iter().position(|l| *l == s)))
                .collect::<Vec<_>>()
        })
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| {
            if model.provider == "codex" {
                vec![2, 3, 4, 5]
            } else if model.provider == "openrouter" {
                vec![requested]
            } else {
                vec![]
            }
        });
    supported
        .into_iter()
        .min_by_key(|n| (n.abs_diff(requested), *n))
        .map(|n| LEVELS[n].into())
}
pub fn chat_request(request: &Value, model: &Model, estimated: u64, session: &str) -> Value {
    let openrouter = model.provider == "openrouter";
    let mut messages = vec![];
    let system = system_text(&request["system"]);
    if !system.is_empty() {
        messages.push(json!({"role":"system","content":system}));
    }
    for m in request["messages"].as_array().into_iter().flatten() {
        let bs = blocks(&m["content"]);
        if m["role"] == "assistant" {
            let mut texts = vec![];
            let mut reasoning = vec![];
            let mut calls = vec![];
            let mut details = vec![];
            for b in bs {
                match b["type"].as_str().unwrap_or("") {
                    "text" => texts.push(b["text"].as_str().unwrap_or("").to_owned()),
                    "tool_use" => {
                        if model.supports_tools {
                            calls.push(json!({"id":b["id"],"type":"function","function":{"name":b["name"],"arguments":b["input"].to_string()}}));
                        } else {
                            texts.push(format!(
                                "[called tool {} with {}]",
                                b["name"].as_str().unwrap_or(""),
                                b["input"]
                            ));
                        }
                    }
                    "thinking" | "redacted_thinking" => {
                        let sig = b["signature"]
                            .as_str()
                            .or_else(|| b["data"].as_str())
                            .unwrap_or("");
                        if sig.starts_with("local:") {
                            reasoning.push(b["thinking"].as_str().unwrap_or("").to_owned());
                        }
                        if openrouter
                            && let Some(data) = sig
                                .strip_prefix("openrouter:")
                                .and_then(|s| STANDARD.decode(s).ok())
                                .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
                        {
                            details.push(data);
                        }
                    }
                    _ => {}
                }
            }
            let mut out = json!({"role":"assistant","content":if texts.is_empty()&&!calls.is_empty(){Value::Null}else{json!(texts.join("\n"))}});
            if !calls.is_empty() {
                out["tool_calls"] = json!(calls);
            }
            if !reasoning.is_empty() {
                out[if openrouter {
                    "reasoning"
                } else {
                    "reasoning_content"
                }] = json!(reasoning.join("\n"));
            }
            if !details.is_empty() {
                out["reasoning_details"] = json!(details);
            }
            messages.push(out);
        } else {
            let mut parts = vec![];
            let mut images = vec![];
            for b in bs {
                match b["type"].as_str().unwrap_or("") {
                    "text" => parts.push(json!({"type":"text","text":b["text"]})),
                    "image" => parts.push(chat_image(&b, model.supports_vision)),
                    "tool_result" => {
                        let mut lines = Vec::new();
                        if let Some(bs) = b["content"].as_array() {
                            for part in bs {
                                match part["type"].as_str() {
                                    Some("text") => {
                                        lines.push(part["text"].as_str().unwrap_or("").to_owned())
                                    }
                                    Some("image") if model.supports_vision => {
                                        images.push(chat_image(part, true))
                                    }
                                    Some("image") => lines.push(
                                        "[image omitted: this model does not accept images]".into(),
                                    ),
                                    _ => {}
                                }
                            }
                        } else if let Some(text) = b["content"].as_str() {
                            lines.push(text.into());
                        }
                        let output = lines.join("\n");
                        let output = if b["is_error"] == true {
                            format!("Error: {output}")
                        } else {
                            output
                        };
                        if model.supports_tools {
                            messages.push(json!({"role":"tool","tool_call_id":b["tool_use_id"],"content":output}));
                        } else {
                            parts.push(json!({"type":"text","text":format!("[result of tool call {}]: {}",b["tool_use_id"].as_str().unwrap_or(""),output)}));
                        }
                    }
                    _ => {}
                }
            }
            images.extend(parts);
            if !images.is_empty() {
                let content = if images.iter().all(|v| v["type"] == "text") {
                    json!(
                        images
                            .iter()
                            .filter_map(|v| v["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                } else {
                    json!(images)
                };
                messages.push(json!({"role":"user","content":content}));
            }
        }
    }
    let mut out = json!({"model":model.upstream_model_id,"messages":messages});
    for (a, b) in [
        ("max_tokens", "max_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop_sequences", "stop"),
    ] {
        if let Some(v) = request.get(a) {
            out[b] = v.clone();
        }
    }
    if !openrouter
        && let (Some(window), Some(max)) = (model.context_window, out["max_tokens"].as_u64())
    {
        out["max_tokens"] = json!(max.min(window.saturating_sub(estimated)).max(1));
    }
    if request["stream"] == true {
        out["stream"] = json!(true);
        out["stream_options"] = json!({"include_usage":true});
    }
    if model.supports_tools {
        add_tools(request, &mut out, false);
    }
    if let Some(level) = effort(request, model) {
        if openrouter {
            out["reasoning"] = json!({"effort":level,"exclude":false});
        } else if let Some(mapping) = &model.effort {
            if mapping["mode"] == "chatTemplateKwargs" || mapping["mode"] == "chat_template_kwargs"
            {
                if let Some(kwargs) = mapping.get("kwargs").and_then(|v| v.get(&level)) {
                    out["chat_template_kwargs"] = kwargs.clone();
                }
            } else {
                out["reasoning_effort"] = json!(level);
            }
        }
    }
    if openrouter {
        out["session_id"] = json!(session);
        if request.pointer("/thinking/type") == Some(&json!("adaptive"))
            && out.get("reasoning").is_none()
        {
            out["reasoning"] = json!({"enabled":true,"exclude":false});
        }
        if let Some(r) = &model.open_router {
            if model.upstream_model_id == "openrouter/auto"
                || model.upstream_model_id == "openrouter/auto-beta"
            {
                let mut plugin = json!({"id":if model.upstream_model_id.ends_with("-beta"){"auto-beta-router"}else{"auto-router"}});
                for (a, b) in [
                    ("costTier", "cost_tier"),
                    ("allowedModels", "allowed_models"),
                    ("excludedModels", "excluded_models"),
                ] {
                    if let Some(v) = r.get(a) {
                        plugin[b] = v.clone();
                    }
                }
                out["plugins"] = json!([plugin]);
            }
            let mut prices = json!({});
            for (a, b) in [
                ("maxPromptPrice", "prompt"),
                ("maxCompletionPrice", "completion"),
            ] {
                if let Some(v) = r.get(a) {
                    prices[b] = v.clone();
                }
            }
            if prices.as_object().is_some_and(|v| !v.is_empty()) {
                out["provider"] = json!({"max_price":prices});
            }
        }
    }
    out
}
fn chat_image(b: &Value, vision: bool) -> Value {
    if let Some(url) = image(b).filter(|_| vision) {
        json!({"type":"image_url","image_url":{"url":url}})
    } else {
        json!({"type":"text","text":"[image omitted: this model does not accept images]"})
    }
}
fn add_tools(request: &Value, out: &mut Value, responses: bool) {
    let mut tools = vec![];
    let mut names = vec![];
    for t in request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["input_schema"].is_object())
    {
        let mut f = json!({"name":t["name"],"parameters":t["input_schema"]});
        if let Some(d) = t.get("description") {
            f["description"] = d.clone();
        }
        names.push(t["name"].clone());
        if responses {
            f["type"] = json!("function");
            f["strict"] = json!(false);
            tools.push(f);
        } else {
            tools.push(json!({"type":"function","function":f}));
        }
    }
    if tools.is_empty() {
        return;
    }
    out["tools"] = json!(tools);
    let choice = &request["tool_choice"];
    out["tool_choice"] = match choice["type"].as_str() {
        Some("any") => json!("required"),
        Some("none") => json!("none"),
        Some("tool") if names.contains(&choice["name"]) => {
            if responses {
                json!({"type":"function","name":choice["name"]})
            } else {
                json!({"type":"function","function":{"name":choice["name"]}})
            }
        }
        _ => json!("auto"),
    };
    if responses || choice["disable_parallel_tool_use"] == true {
        out["parallel_tool_calls"] = json!(choice["disable_parallel_tool_use"] != true);
    }
}
pub fn responses_request(request: &Value, model: &Model, session: &str) -> Value {
    let mut out = json!({"model":model.upstream_model_id,"stream":true,"store":false,"reasoning":{"summary":"auto"},"include":["reasoning.encrypted_content"],"instructions":system_text(&request["system"]),"prompt_cache_key":session});
    if let Some(e) = effort(request, model) {
        out["reasoning"]["effort"] = json!(e);
    }
    let mut input = vec![];
    for m in request["messages"].as_array().into_iter().flatten() {
        let assistant = m["role"] == "assistant";
        let mut parts = vec![];
        let mut tool_images = Vec::new();
        let flush = |parts: &mut Vec<Value>, input: &mut Vec<Value>| {
            if !parts.is_empty() {
                input.push(json!({"type":"message","role":if assistant{"assistant"}else{"user"},"content":std::mem::take(parts)}));
            }
        };
        for b in blocks(&m["content"]) {
            if b["type"] != "tool_result" {
                parts.append(&mut tool_images);
            }
            match b["type"].as_str().unwrap_or("") {
                "text" => parts.push(
                    json!({"type":if assistant{"output_text"}else{"input_text"},"text":b["text"]}),
                ),
                "image" => {
                    if let Some(url) = image(&b) {
                        parts.push(json!({"type":"input_image","image_url":url}));
                    }
                }
                "tool_use" if assistant => {
                    flush(&mut parts, &mut input);
                    input.push(json!({"type":"function_call","call_id":b["id"],"name":b["name"],"arguments":b["input"].to_string()}));
                }
                "tool_result" => {
                    flush(&mut parts, &mut input);
                    input.push(json!({"type":"function_call_output","call_id":b["tool_use_id"],"output":tool_output(&b)}));
                    for img in b["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "image")
                    {
                        if let Some(url) = image(img) {
                            tool_images.push(json!({"type":"input_image","image_url":url}));
                        }
                    }
                }
                "thinking" => {
                    if let Some(sig) = b["signature"]
                        .as_str()
                        .and_then(|s| s.strip_prefix("codex:"))
                    {
                        flush(&mut parts, &mut input);
                        input
                            .push(json!({"type":"reasoning","encrypted_content":sig,"summary":[]}));
                    }
                }
                _ => {}
            }
        }
        parts.append(&mut tool_images);
        flush(&mut parts, &mut input);
    }
    out["input"] = json!(input);
    add_tools(request, &mut out, true);
    out
}
pub fn prepare_claude(request: &mut Value, model: &str, identity: bool) {
    request["model"] = json!(model);
    let mut tool_loop = false;
    if let Some(ms) = request["messages"].as_array_mut() {
        for m in ms {
            let assistant = m["role"] == "assistant";
            if let Some(bs) = m["content"].as_array_mut() {
                bs.retain(|b| {
                    let sig = b["signature"]
                        .as_str()
                        .or_else(|| b["data"].as_str())
                        .unwrap_or("");
                    !(b["type"] == "thinking"
                        && (sig.is_empty()
                            || ["codex:", "local:", "openrouter:"]
                                .iter()
                                .any(|p| sig.starts_with(p)))
                        || b["type"] == "redacted_thinking" && sig.starts_with("openrouter:"))
                });
                if bs.is_empty() {
                    bs.push(json!({"type":"text","text":"[reasoning omitted]"}));
                }
                if assistant {
                    tool_loop = bs.iter().any(|b| b["type"] == "tool_use")
                        && !bs.first().is_some_and(|b| {
                            b["type"] == "thinking" || b["type"] == "redacted_thinking"
                        });
                }
            }
        }
    }
    if tool_loop {
        request.as_object_mut().unwrap().remove("thinking");
    }
    if identity {
        const ID: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
        let mut bs = blocks(&request["system"]);
        if !bs
            .first()
            .and_then(|b| b["text"].as_str())
            .is_some_and(|s| s.trim_start().starts_with(ID))
        {
            bs.insert(0, json!({"type":"text","text":ID}));
        }
        bs.retain(|b| b["text"].as_str() != Some(""));
        request["system"] = json!(bs);
    }
}
pub fn tool_id(id: &str) -> String {
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        id.into()
    } else {
        format!("toolu_{}", uuid::Uuid::new_v4().simple())
    }
}
pub fn usage(v: &Value, estimated: u64, openrouter: bool) -> Value {
    let input = v["prompt_tokens"].as_u64().unwrap_or(estimated);
    let mut out =
        json!({"input_tokens":input,"output_tokens":v["completion_tokens"].as_u64().unwrap_or(0)});
    if openrouter {
        let cached = v
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(input);
        let created = v
            .pointer("/prompt_tokens_details/cache_write_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(input - cached);
        out["input_tokens"] = json!(input - cached - created);
        out["cache_read_input_tokens"] = json!(cached);
        out["cache_creation_input_tokens"] = json!(created);
    }
    out
}
pub fn chat_response(chat: &Value, model: &str, estimated: u64, openrouter: bool) -> Value {
    let m = &chat["choices"][0]["message"];
    let mut content = vec![];
    let mut text = m["content"].as_str().unwrap_or("").to_owned();
    let mut reasoning = m["reasoning_content"]
        .as_str()
        .or_else(|| m["reasoning"].as_str())
        .unwrap_or("")
        .to_owned();
    if text.trim_start().starts_with("<think>")
        && let Some((thinking, rest)) = text.trim_start()[7..].split_once("</think>")
    {
        if reasoning.is_empty() {
            reasoning = thinking.to_owned();
        }
        text = rest.trim_start_matches('\n').to_owned();
    }
    if openrouter {
        for detail in m["reasoning_details"].as_array().into_iter().flatten() {
            let sig = format!("openrouter:{}", STANDARD.encode(detail.to_string()));
            content.push(if detail["type"]=="reasoning.encrypted"{json!({"type":"redacted_thinking","data":sig})}else{json!({"type":"thinking","thinking":detail["text"].as_str().or_else(||detail["summary"].as_str()).unwrap_or(""),"signature":sig})});
        }
    }
    if content.is_empty() && !reasoning.is_empty() {
        content.push(json!({"type":"thinking","thinking":reasoning,"signature":"local:"}));
    }
    if !text.is_empty() {
        content.push(json!({"type":"text","text":text}));
    }
    for call in m["tool_calls"].as_array().into_iter().flatten() {
        if let Some(name) = call["function"]["name"].as_str().filter(|s| !s.is_empty()) {
            let args = serde_json::from_str::<Value>(
                call["function"]["arguments"].as_str().unwrap_or("{}"),
            )
            .ok()
            .filter(Value::is_object)
            .unwrap_or(json!({}));
            content.push(json!({"type":"tool_use","id":tool_id(call["id"].as_str().unwrap_or("")),"name":name,"input":args}));
        }
    }
    let stop = match chat["choices"][0]["finish_reason"].as_str() {
        Some("length") => "max_tokens",
        _ if content.iter().any(|b| b["type"] == "tool_use") => "tool_use",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    };
    json!({"id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":stop,"stop_sequence":null,"usage":({let mut u=usage(&chat["usage"],estimated,openrouter);if let Some(cost)=chat["usage"].get("cost"){u["cost"]=cost.clone();}u}),"resolved_model":chat["model"],"generation_id":chat["id"]})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model() -> Model {
        serde_json::from_value(json!({"id":"local","provider":"local","upstreamModelId":"upstream","enabled":true,"supportsTools":true,"supportsVision":false})).unwrap()
    }
    #[test]
    fn tool_results_precede_user_text() {
        let m = model();
        let r = chat_request(
            &json!({"messages":[{"role":"user","content":[{"type":"text","text":"next"},{"type":"tool_result","tool_use_id":"c1","content":"answer"}]}]}),
            &m,
            20,
            "s",
        );
        assert_eq!(r["messages"][0]["role"], "tool");
        assert_eq!(r["messages"][1]["content"], "next");
    }
    #[test]
    fn codex_preserves_encrypted_reasoning_and_tool_order() {
        let r = responses_request(
            &json!({"messages":[{"role":"assistant","content":[{"type":"text","text":"before"},{"type":"thinking","signature":"codex:encrypted"},{"type":"tool_use","id":"c","name":"f","input":{"x":1}}]}]}),
            &model(),
            "s",
        );
        assert_eq!(r["input"][0]["type"], "message");
        assert_eq!(r["input"][1]["encrypted_content"], "encrypted");
        assert_eq!(r["input"][2]["type"], "function_call");
        assert!(r.get("max_tokens").is_none());
    }
    #[test]
    fn length_wins_over_tool_calls_and_invalid_args_degrade() {
        let r = chat_response(
            &json!({"choices":[{"message":{"tool_calls":[{"id":"bad.id","function":{"name":"f","arguments":"[]"}}]},"finish_reason":"length"}]}),
            "m",
            50,
            false,
        );
        assert_eq!(r["stop_reason"], "max_tokens");
        assert_eq!(r["content"][0]["input"], json!({}));
        assert!(!r["content"][0]["id"].as_str().unwrap().contains('.'));
    }
}

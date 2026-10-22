use crate::{
    AppState, Store, error,
    providers::{self, ModelFault},
    types::*,
};
use anyhow::{Result, bail};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{OriginalUri, Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Instant};

pub fn validate_catalog(c: &Catalog) -> Result<()> {
    if !TIERS.contains(&c.default_tier.as_str()) {
        bail!("Default tier must be max, high, mid or low")
    }
    let mut ids = HashSet::new();
    for m in &c.models {
        if m.id.trim().is_empty() || !ids.insert(m.id.to_ascii_lowercase()) {
            bail!("Model IDs must be nonempty and unique, ignoring case")
        }
        if !["claude", "codex", "local", "openrouter", "systemone"].contains(&m.provider.as_str()) {
            bail!("Unknown provider")
        }
        if m.upstream_model_id.trim().is_empty() {
            bail!("An upstream model ID is required")
        }
        if matches!(m.provider.as_str(), "local" | "systemone") {
            let u = url::Url::parse(m.base_url.as_deref().unwrap_or(""))?;
            if !matches!(u.scheme(), "http" | "https")
                || u.host_str().is_none()
                || !u.username().is_empty()
                || u.password().is_some()
                || u.query().is_some()
                || u.fragment().is_some()
            {
                bail!("Model endpoint must be an HTTP(S) URL without embedded credentials")
            }
        }
        if m.context_window == Some(0)
            || m.max_prompt_tokens == Some(0)
            || m.max_conversations == Some(0)
        {
            bail!("Model limits must be positive")
        }
        if let Some(e) = &m.effort {
            validate_effort(e)?;
        }
        if let Some(options) = &m.open_router {
            validate_openrouter(options)?;
        }
    }
    for (tier, members) in &c.tiers {
        if !TIERS.contains(&tier.as_str()) {
            bail!("Unknown tier")
        }
        let mut seen = HashSet::new();
        for id in members {
            let Some(m) = c.models.iter().find(|m| m.id == *id) else {
                bail!("Tier contains an unknown model")
            };
            if m.provider == "systemone" {
                bail!("Decision models cannot belong to a tier")
            }
            if !seen.insert(id) {
                bail!("A tier cannot contain a model twice")
            }
        }
    }
    for a in &c.aliases {
        if !TIERS.contains(&a.tier.as_str()) {
            bail!("Unknown alias tier")
        }
    }
    for p in c.prices.values() {
        if [
            p.input,
            p.output,
            p.cache_read,
            p.cache_write,
            p.cache_write1h.unwrap_or(p.cache_write),
        ]
        .iter()
        .any(|p| !p.is_finite() || *p < 0.)
        {
            bail!("Prices must be finite and nonnegative")
        }
    }
    Ok(())
}
pub fn apply_override(store: &Store, text: &str) -> Result<()> {
    let hash = hex::encode(Sha256::digest(text.as_bytes()));
    if store.get_setting("seed_override_hash")?.as_deref() == Some(&hash) {
        return Ok(());
    }
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
    if !v.is_object() {
        bail!("Seed override must be an object")
    }
    if v.get("models")
        .is_some_and(|models| !models.is_null() && !models.is_array())
    {
        bail!("Override models must be an array")
    }
    let mut catalog = store.catalog()?;
    for patch in v["models"].as_array().into_iter().flatten() {
        let Some(id) = patch["id"].as_str() else {
            bail!("Override model needs id")
        };
        if let Some(e) = patch.get("effort").filter(|v| !v.is_null()) {
            validate_effort(e)?;
        }
        if let Some(options) = patch.get("openRouter").filter(|v| !v.is_null()) {
            validate_openrouter(options)?;
        }
        if patch.get("maxConversations") == Some(&json!(0)) {
            bail!("Model conversation limit must be positive")
        }
        if let Some(m) = catalog.models.iter_mut().find(|m| m.id == id) {
            let mut data = serde_json::to_value(&*m)?;
            let Some(fields) = patch.as_object() else {
                bail!("Model override must be an object")
            };
            for (k, v) in fields {
                if !v.is_null()
                    && [
                        "enabled",
                        "baseUrl",
                        "upstreamModelId",
                        "apiKey",
                        "contextWindow",
                        "maxPromptTokens",
                        "maxConversations",
                        "supportsTools",
                        "supportsVision",
                        "effort",
                        "openRouter",
                        "decisions",
                    ]
                    .contains(&k.as_str())
                {
                    data[k] = v.clone();
                }
            }
            *m = serde_json::from_value(data)?
        } else {
            tracing::warn!(model_id = id, "Skipping override for unknown model");
        }
    }
    validate_catalog(&catalog)?;
    store.save_catalog(&catalog)?;
    store.set_setting("seed_override_hash", &hash)?;
    Ok(())
}
fn hash(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}
fn utf16_len(s: &str) -> u64 {
    s.encode_utf16().count() as u64
}
pub fn estimate_tokens(v: &Value) -> u64 {
    fn count(v: &Value) -> u64 {
        match v {
            Value::Object(m) if m.get("type").and_then(Value::as_str) == Some("base64") => 6400,
            Value::Object(m) => m.iter().map(|(k, v)| utf16_len(k) + count(v)).sum(),
            Value::Array(a) => a.iter().map(count).sum(),
            Value::String(s) => utf16_len(s),
            Value::Null => 0,
            _ => v.to_string().len() as u64,
        }
    }
    (count(&v["system"]) + count(&v["messages"]) + count(&v["tools"]))
        .div_ceil(4)
        .clamp(1, i32::MAX as u64)
}
fn content_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|b| {
                if let Some(t) = b["text"].as_str() {
                    t.to_owned()
                } else {
                    let mut b = b.clone();
                    if let Some(m) = b.as_object_mut() {
                        m.remove("cache_control");
                    }
                    b.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => v.to_string(),
    }
}
pub fn session_identity(h: &HeaderMap, v: &Value) -> (String, Option<String>) {
    let normalize = |s: &str| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_owned())
        }
    };
    let id = h
        .get("x-claude-code-session-id")
        .and_then(|s| s.to_str().ok())
        .and_then(normalize)
        .or_else(|| {
            let raw = v["metadata"]["user_id"].as_str()?;
            if raw.trim_start().starts_with('{') {
                return serde_json::from_str::<Value>(raw)
                    .ok()?
                    .get("session_id")?
                    .as_str()
                    .and_then(normalize);
            }
            let lower = raw.to_ascii_lowercase();
            let i = lower.find("session_")?;
            let candidate = lower.get(i + 8..i + 44)?;
            uuid::Uuid::parse_str(candidate).ok().map(|v| v.to_string())
        });
    let source = id
        .as_ref()
        .map(|id| format!("client:{id}"))
        .unwrap_or_else(|| format!("system:{}", hash(&content_text(&v["system"]))));
    let first = v["messages"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["role"] == "user"))
        .map(|m| &m["content"])
        .unwrap_or(&Value::Null);
    (
        hash(&format!(
            "{source}\nfirst-user:{}",
            hash(&content_text(first))
        )),
        id,
    )
}
fn glob(pattern: &str, text: &str) -> bool {
    let p = pattern.to_lowercase().chars().collect::<Vec<_>>();
    let t = text.to_lowercase().chars().collect::<Vec<_>>();
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(s) = star {
            retry += 1;
            j = retry;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1
    }
    i == p.len()
}
fn tier(c: &Catalog, h: &HeaderMap, v: &Value) -> Result<String> {
    if let Some(header) = h.get("x-weave-model-class") {
        let value = header.to_str()?.trim().to_ascii_lowercase();
        if !TIERS.contains(&value.as_str()) {
            bail!("x-weave-model-class must be max, high, mid or low")
        };
        return Ok(value);
    }
    let model = v["model"].as_str().unwrap_or("");
    Ok(c.aliases
        .iter()
        .find(|a| glob(&a.pattern, model))
        .map(|a| a.tier.clone())
        .unwrap_or_else(|| c.default_tier.clone()))
}
pub async fn status(state: &AppState, m: &Model) -> Result<String> {
    if !m.enabled {
        return Ok("disabled".into());
    }
    if matches!(m.provider.as_str(), "claude" | "codex" | "openrouter")
        && state.store.get_secret(&m.provider)?.is_none()
    {
        return Ok("not_logged_in".into());
    }
    let lists = state
        .store
        .get_setting(&format!("models:{}", m.provider))?
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if let Some(ids) = lists.as_ref().and_then(|v| v["modelIds"].as_array()) {
        let matches = ids.iter().filter_map(Value::as_str).any(|id| {
            if m.provider == "openrouter" {
                id == m.upstream_model_id
            } else {
                id.eq_ignore_ascii_case(&m.upstream_model_id)
                    || snapshot_of(id, &m.upstream_model_id)
                    || snapshot_of(&m.upstream_model_id, id)
            }
        });
        if !matches
            && !(m.provider == "openrouter"
                && ["openrouter/auto", "openrouter/auto-beta"]
                    .contains(&m.upstream_model_id.as_str()))
        {
            return Ok("unknown_model".into());
        }
    }
    if let Some(c) = state
        .store
        .cooldowns()?
        .into_iter()
        .find(|c| c.model_id == m.id && c.until > now())
    {
        return Ok(if c.reason == "auth_rejected" {
            "auth_rejected"
        } else {
            "cooling"
        }
        .into());
    }
    if matches!(m.provider.as_str(), "local" | "systemone")
        && !state
            .runtime
            .health
            .read()
            .await
            .get(&m.id)
            .copied()
            .unwrap_or(false)
    {
        return Ok("unhealthy".into());
    }
    Ok("ok".into())
}
fn fits(m: &Model, v: &Value, estimate: u64) -> bool {
    let output = v["max_tokens"].as_u64().unwrap_or(0).min(1024);
    m.context_window
        .is_none_or(|n| estimate.saturating_add(output) <= n)
        && m.max_prompt_tokens.is_none_or(|n| estimate <= n)
        && (m.supports_tools || v["tools"].as_array().is_none_or(Vec::is_empty))
}
struct Hold {
    slot: crate::ConversationSlot,
}
impl Drop for Hold {
    fn drop(&mut self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.0 = slot.0.saturating_sub(1);
        slot.1 = Instant::now();
    }
}
fn admits(state: &AppState, m: &Model, key: &str) -> bool {
    if m.provider != "local" || m.max_conversations.is_none() {
        return true;
    }
    let mut holds = state
        .runtime
        .conversations
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    holds.retain(|_, slot| {
        let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.0 > 0 || slot.1.elapsed() < state.config.conversation_idle
    });
    if holds.contains_key(&(m.id.clone(), key.into())) {
        return true;
    }
    holds.keys().filter(|(id, _)| id == &m.id).count() < (m.max_conversations.unwrap() as usize)
}
// A tier is a preference: exhaust it before descending to lower tiers.
fn fallback_tiers(tier: &str) -> &'static [&'static str] {
    &TIERS[TIERS.iter().position(|t| *t == tier).unwrap_or(TIERS.len())..]
}
struct SelectionRequest<'a> {
    catalog: &'a Catalog,
    tier: &'a str,
    key: &'a str,
    client: &'a Option<String>,
    body: &'a Value,
    estimate: u64,
}
async fn select(
    state: &AppState,
    request: &SelectionRequest<'_>,
    tried: &HashSet<String>,
    peek: bool,
) -> Result<Option<(Model, Option<Hold>)>> {
    let SelectionRequest {
        catalog: c,
        tier,
        key,
        client,
        body: v,
        estimate,
    } = *request;
    let _gate = state.runtime.assignment.lock().await;
    let mut pool = Vec::new();
    let mut serving_tier = tier;
    for candidate_tier in fallback_tiers(tier) {
        for id in c.tiers.get(*candidate_tier).into_iter().flatten() {
            if let Some(m) = c.models.iter().find(|m| m.id == *id)
                && !tried.contains(id)
                && fits(m, v, estimate)
                && status(state, m).await? == "ok"
            {
                pool.push(m.clone())
            }
        }
        if !pool.is_empty() {
            serving_tier = candidate_tier;
            break;
        }
    }
    if pool.is_empty() {
        return Ok(None);
    }
    let open = pool
        .iter()
        .filter(|m| admits(state, m, key))
        .cloned()
        .collect::<Vec<_>>();
    if !open.is_empty() {
        pool = open
    }
    let pin = state.store.pin(key, tier)?;
    let pinned = pin
        .as_ref()
        .and_then(|p| pool.iter().find(|m| m.id == p.model_id));
    let sibling = if pin.is_none() && client.is_some() {
        let mut pins = state.store.pins()?;
        pins.sort_by_key(|p| p.created_at);
        pins.into_iter()
            .filter(|p| p.client_session_id == *client && p.tier == tier)
            .find_map(|p| pool.iter().find(|m| m.id == p.model_id))
    } else {
        None
    };
    let advance = pinned.is_none() && sibling.is_none();
    let chosen = if let Some(m) = pinned.or(sibling) {
        m.clone()
    } else {
        let locals = pool
            .iter()
            .filter(|m| m.provider == "local")
            .cloned()
            .collect::<Vec<_>>();
        if matches!(serving_tier, "mid" | "low") && !locals.is_empty() {
            pool = locals
        }
        pool[(state.store.cursor(tier)? % (pool.len() as u64)) as usize].clone()
    };
    if peek {
        return Ok(Some((chosen, None)));
    }
    let new_pin = Pin {
        session_key: key.into(),
        tier: tier.into(),
        model_id: chosen.id.clone(),
        client_session_id: client.clone(),
        created_at: pin.as_ref().map(|p| p.created_at).unwrap_or_else(now),
    };
    state.store.assign_pin(&new_pin, advance)?;
    let mut holds = state
        .runtime
        .conversations
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(p) = pin
        && p.model_id != chosen.id
        && holds
            .get(&(p.model_id.clone(), key.into()))
            .is_some_and(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).0 == 0)
    {
        holds.remove(&(p.model_id, key.into()));
    }
    if chosen.provider != "local" {
        return Ok(Some((chosen, None)));
    }
    let slot = holds
        .entry((chosen.id.clone(), key.into()))
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new((0, Instant::now()))))
        .clone();
    slot.lock().unwrap_or_else(|e| e.into_inner()).0 += 1;
    Ok(Some((chosen, Some(Hold { slot }))))
}

pub fn record_fault(state: &AppState, model: &Model, fault: &ModelFault) {
    let fallback = match fault.kind.as_str() {
        "limited" => state.config.limit_cooldown,
        "plan_rejected" => state.config.plan_rejection_cooldown,
        "overloaded" => state.config.overloaded_cooldown,
        "unreachable" if model.provider != "local" => state.config.overloaded_cooldown,
        "auth_rejected" => state.config.auth_rejection_cooldown,
        _ => return,
    };
    let n = now();
    let until = fault
        .reset_at
        .filter(|r| *r > n)
        .unwrap_or(n + fallback.as_secs().min(8 * 86400) as i64)
        .min(n + 8 * 86400);
    let models = if fault.kind == "auth_rejected" {
        state
            .store
            .catalog()
            .map(|c| {
                c.models
                    .into_iter()
                    .filter(|m| m.provider == model.provider)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![model.clone()]
    };
    for m in models {
        if state
            .store
            .set_cooldown(&Cooldown {
                model_id: m.id,
                until,
                reason: fault.kind.clone(),
            })
            .is_err()
        {
            tracing::warn!("Could not persist model cooldown")
        }
    }
}
const EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
pub fn sent_effort(v: &Value, m: &Model) -> Option<String> {
    let requested = v["output_config"]["effort"]
        .as_str()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|s| EFFORTS.contains(&s.as_str()))
        .or_else(|| match v["thinking"]["type"].as_str() {
            Some("disabled") => Some("none".into()),
            Some("enabled") => v["thinking"]["budget_tokens"].as_i64().map(|n| {
                match n {
                    ..4096 => "low",
                    4096..16384 => "medium",
                    16384..32768 => "high",
                    32768..65536 => "xhigh",
                    _ => "max",
                }
                .into()
            }),
            _ => None,
        });
    if m.provider == "claude" {
        return requested;
    }
    let e = m.effort.as_ref();
    let requested = if m.provider == "codex" {
        requested
    } else {
        requested.or_else(|| e.and_then(|e| e["default"].as_str()).map(str::to_owned))
    }?;
    let levels = e
        .and_then(|e| e["levels"].as_array())
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| {
            if m.provider == "codex" {
                vec!["low", "medium", "high", "xhigh"]
            } else if m.provider == "openrouter" {
                EFFORTS.to_vec()
            } else {
                vec![]
            }
        });
    if m.provider == "local" && e.and_then(|e| e["mode"].as_str()).unwrap_or("none") == "none" {
        return None;
    }
    let n = EFFORTS.iter().position(|l| *l == requested)?;
    levels
        .into_iter()
        .filter_map(|l| {
            EFFORTS
                .iter()
                .position(|v| *v == l)
                .map(|p| (p.abs_diff(n), p, l))
        })
        .min_by_key(|(dist, p, _)| (*dist, *p))
        .map(|(_, _, l)| l.into())
}
pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    route(state, headers, body, false, uri.query()).await
}
pub async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    route(state, headers, body, true, uri.query()).await
}
async fn route(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    count: bool,
    query: Option<&str>,
) -> Response {
    let c = match state.store.catalog() {
        Ok(c) => c,
        Err(_) => return error(500, "api_error", "Routing catalog unavailable"),
    };
    if body.len() > 32 * 1024 * 1024 {
        return error(413, "request_too_large", "Request body exceeds 32 MB");
    }
    let v = match crate::systemone::parse_strict(&body) {
        Ok(v) if v.is_object() => v,
        _ => {
            let mut r = error(
                400,
                "invalid_request_error",
                "Request body must be a well-formed JSON object",
            );
            if let Ok(t) = tier(&c, &headers, &Value::Null) {
                r.headers_mut()
                    .insert("x-weave-model-class", t.parse().unwrap());
            }
            return r;
        }
    };
    let tier = match tier(&c, &headers, &v) {
        Ok(t) => t,
        Err(_) => {
            return error(
                400,
                "invalid_request_error",
                "x-weave-model-class must be max, high, mid or low",
            );
        }
    };
    let (key, client) = session_identity(&headers, &v);
    let estimate = estimate_tokens(&v);
    let streaming = v["stream"] == true;
    let started = Instant::now();
    let mut tried = HashSet::new();
    let mut only_overflow = true;
    let mut overflow = None;
    let mut last = None;
    loop {
        let selected = match select(
            &state,
            &SelectionRequest {
                catalog: &c,
                tier: &tier,
                key: &key,
                client: &client,
                body: &v,
                estimate,
            },
            &tried,
            count,
        )
        .await
        {
            Ok(s) => s,
            Err(_) => return error(500, "api_error", "Could not assign a model"),
        };
        let Some((m, hold)) = selected else { break };
        tried.insert(m.id.clone());
        last = Some(m.clone());
        let u = Usage {
            at: now(),
            client_session_id: client.clone(),
            model_id: m.id.clone(),
            provider: m.provider.clone(),
            tier: Some(tier.clone()),
            status: 200,
            latency_ms: 0,
            effort: sent_effort(&v, &m),
            session_key: Some(key.clone()),
            upstream_model_id: Some(m.upstream_model_id.clone()),
            is_streaming: Some(streaming),
            ..Default::default()
        };
        let mut pending = Pending {
            state: state.clone(),
            usage: if count { None } else { Some(u) },
            started,
            hold,
        };
        let result = providers::send_with_headers_and_query(
            &state,
            &m,
            v.clone(),
            &key,
            count,
            estimate,
            &headers,
            query,
        )
        .await;
        let row = pending.usage.take();
        match result {
            Ok(attempt) => {
                if let Some(f) = attempt.fault {
                    record_fault(&state, &m, &f)
                }
                let mut response = attempt.response;
                response
                    .headers_mut()
                    .insert("x-weave-model-class", tier.parse().unwrap());
                if count {
                    return response;
                }
                let mut u = row.expect("usage exists for a Messages request");
                u.status = response.status().as_u16();
                return tap_response(state, response, m, u, started, pending.hold.take());
            }
            Err(f) => {
                pending.hold.take();
                record_fault(&state, &m, &f);
                only_overflow &= f.kind == "context_overflow";
                if f.kind == "context_overflow" {
                    overflow = f.message.or(Some("prompt is too long".into()))
                }
            }
        }
    }
    let mut response = if let Some(message) = overflow.filter(|_| only_overflow) {
        error(
            400,
            "invalid_request_error",
            &if message.to_lowercase().contains("prompt is too long") {
                message
            } else {
                format!("prompt is too long: {message}")
            },
        )
    } else {
        exhausted(&state, &c, &tier, &v, estimate).await
    };
    response
        .headers_mut()
        .insert("x-weave-model-class", tier.parse().unwrap());
    if !count && let Some(m) = last {
        let mut u = Usage {
            at: now(),
            client_session_id: client,
            model_id: m.id.clone(),
            provider: m.provider.clone(),
            tier: Some(tier),
            status: response.status().as_u16(),
            latency_ms: started.elapsed().as_millis() as u64,
            session_key: Some(key),
            upstream_model_id: Some(m.upstream_model_id.clone()),
            is_streaming: Some(streaming),
            outcome: Some("failed".into()),
            failure_kind: Some(
                if only_overflow {
                    "context_overflow"
                } else {
                    "tier_exhausted"
                }
                .into(),
            ),
            ..Default::default()
        };
        price_usage(&mut u, &c, &m, None);
        u.failure_detail = u
            .failure_kind
            .as_deref()
            .and_then(explanation)
            .map(str::to_owned);
        let _ = state.store.record_usage(&u);
    }
    response
}
async fn exhausted(
    state: &AppState,
    c: &Catalog,
    tier: &str,
    v: &Value,
    estimate: u64,
) -> Response {
    let cooldowns = state.store.cooldowns().unwrap_or_default();
    let n = now();
    let ids = fallback_tiers(tier)
        .iter()
        .flat_map(|tier| c.tiers.get(*tier).into_iter().flatten())
        .collect::<HashSet<_>>();
    let next = cooldowns
        .iter()
        .filter(|cd| {
            cd.until > n
                && ids.contains(&cd.model_id)
                && c.models
                    .iter()
                    .any(|m| m.id == cd.model_id && m.enabled && fits(m, v, estimate))
        })
        .map(|c| c.until)
        .min();
    let wait = next
        .map(|v| v - n)
        .unwrap_or(state.config.limit_cooldown.as_secs() as i64)
        .max(1);
    let mut missing = HashSet::new();
    for m in c
        .models
        .iter()
        .filter(|m| m.enabled && ids.contains(&m.id) && fits(m, v, estimate))
    {
        if status(state, m)
            .await
            .is_ok_and(|s| s == "not_logged_in" || s == "auth_rejected")
        {
            missing.insert(m.provider.as_str());
        }
    }
    let mut hints = missing
        .into_iter()
        .map(|p| {
            if p == "openrouter" {
                "set the OpenRouter API key in /ui/settings".into()
            } else {
                format!("knowwhere login {p}")
            }
        })
        .collect::<Vec<String>>();
    hints.sort();
    let hint = if hints.is_empty() {
        String::new()
    } else {
        format!(" Run {}.", hints.join(" or "))
    };
    let mut r = error(
        429,
        "rate_limit_error",
        &format!(
            "model_class_unavailable: no model in the {tier} tier or its lower-tier fallbacks can serve this request now (usage limit, login or availability); retry in {wait} s.{hint}"
        ),
    );
    r.headers_mut()
        .insert("retry-after", wait.to_string().parse().unwrap());
    r
}

struct Pending {
    state: AppState,
    usage: Option<Usage>,
    started: Instant,
    hold: Option<Hold>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(mut u) = self.usage.take() {
            u.status = 499;
            u.outcome = Some("client_cancelled".into());
            u.failure_kind = Some("client_cancelled".into());
            u.failure_detail = explanation("client_cancelled").map(str::to_owned);
            if let Ok(c) = self.state.store.catalog()
                && let Some(m) = c.models.iter().find(|m| m.id == u.model_id)
            {
                price_usage(&mut u, &c, m, None)
            }
            u.latency_ms = self.started.elapsed().as_millis() as u64;
            let _ = self.state.store.record_usage(&u);
        }
    }
}

struct Tap {
    state: AppState,
    model: Model,
    usage: Usage,
    started: Instant,
    hold: Option<Hold>,
    pending: Vec<u8>,
    data: Vec<u8>,
    json: Vec<u8>,
    stream: bool,
    finished: bool,
    bytes: usize,
    reported_cost: Option<f64>,
    message_started: bool,
    message_stopped: bool,
    message: bool,
    error: bool,
    malformed: bool,
    unreadable: bool,
    line_overflow: bool,
    event_overflow: bool,
    diagnostics: Option<std::sync::Arc<std::sync::Mutex<providers::StreamDiagnostics>>>,
}
impl Tap {
    fn feed(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len();
        if !self.stream {
            if !self.unreadable {
                if self.json.len() + bytes.len() > 8 * 1024 * 1024 {
                    self.json.clear();
                    self.unreadable = true;
                } else {
                    self.json.extend_from_slice(bytes);
                }
            }
            return;
        }
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            let newline = part.last() == Some(&b'\n');
            let payload = if newline {
                &part[..part.len() - 1]
            } else {
                part
            };
            if !self.line_overflow && self.pending.len() + payload.len() <= 256 * 1024 {
                self.pending.extend_from_slice(payload);
            } else {
                self.line_overflow = true;
                self.unreadable = true;
            }
            if newline {
                self.end_line();
            }
        }
    }
    fn end_line(&mut self) {
        while self.pending.last() == Some(&b'\r') {
            self.pending.pop();
        }
        let line = std::mem::take(&mut self.pending);
        if self.line_overflow {
            self.event_overflow = true;
        } else if line.is_empty() {
            self.dispatch();
        } else if let Some(event) = line.strip_prefix(b"event:") {
            if event.trim_ascii() == b"error" {
                self.error = true;
            }
        } else if let Some(data) = line.strip_prefix(b"data:") {
            if self.data.len() + data.len() < 256 * 1024 {
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(data);
            } else {
                self.event_overflow = true;
                self.unreadable = true;
            }
        }
        self.line_overflow = false;
    }
    fn dispatch(&mut self) {
        let data = std::mem::take(&mut self.data);
        if !self.event_overflow && !data.is_empty() {
            self.apply(&data);
        }
        self.event_overflow = false;
    }
    fn apply(&mut self, bytes: &[u8]) {
        let v = match serde_json::from_slice::<Value>(bytes) {
            Ok(v) if v.is_object() => v,
            _ => {
                self.malformed = true;
                return;
            }
        };
        if v["type"] == "error" || v.get("error").is_some_and(|v| !v.is_null()) {
            self.error = true;
        }
        if self.stream {
            if v["type"] == "message_start" {
                self.message_started = v["message"].is_object();
                if !self.message_started {
                    self.malformed = true;
                }
                self.tokens(&v["message"]["usage"]);
                self.metadata(&v["message"]);
            }
            if v["type"] == "message_delta" {
                self.tokens(&v["usage"]);
                self.metadata(&v);
                if let Some(reason) = v["delta"]["stop_reason"].as_str().and_then(safe_stop) {
                    self.usage.stop_reason = Some(reason);
                }
            }
            if v["type"] == "message_stop" {
                self.message_stopped = true;
            }
            if v["type"] == "error" {
                let fault = match v["error"]["type"].as_str() {
                    Some("rate_limit_error") => Some("limited"),
                    Some("overloaded_error") => Some("overloaded"),
                    _ => None,
                };
                if let Some(kind) = fault {
                    record_fault(
                        &self.state,
                        &self.model,
                        &ModelFault {
                            kind: kind.into(),
                            reset_at: None,
                            message: None,
                        },
                    );
                }
            }
        } else {
            self.message = v["type"] == "message" && v["content"].is_array();
            self.tokens(&v["usage"]);
            self.metadata(&v);
            self.usage.stop_reason = v["stop_reason"].as_str().and_then(safe_stop);
        }
    }
    fn metadata(&mut self, v: &Value) {
        if let Some(s) = v["resolved_model"].as_str() {
            self.usage.resolved_model_id = Some(s.into());
        }
        if let Some(s) = v["generation_id"].as_str() {
            self.usage.generation_id = Some(s.into());
        }
    }
    fn tokens(&mut self, v: &Value) {
        macro_rules! read {
            ($field:ident,$key:literal) => {
                if let Some(n) = v[$key].as_u64().filter(|n| *n <= i64::MAX as u64) {
                    self.usage.$field = n;
                }
            };
        }
        read!(input_tokens, "input_tokens");
        read!(output_tokens, "output_tokens");
        read!(cache_read_tokens, "cache_read_input_tokens");
        read!(cache_write_tokens, "cache_creation_input_tokens");
        if let Some(n) = v["cache_creation"]["ephemeral_1h_input_tokens"]
            .as_u64()
            .filter(|n| *n <= i64::MAX as u64)
        {
            self.usage.cache_write1h_tokens = n;
        }
        if let Some(cost) = v["cost"].as_f64().filter(|n| n.is_finite() && *n >= 0.) {
            self.reported_cost = Some(cost);
        }
    }
    fn finish(&mut self) {
        self.finished = true;
        if self.stream {
            self.end_line();
            self.dispatch();
        } else if !self.unreadable {
            let bytes = std::mem::take(&mut self.json);
            self.apply(&bytes);
        }
    }
    fn interrupted(&mut self) {
        self.usage.failure_kind = Some("transport_error".into());
    }
}
impl Drop for Tap {
    fn drop(&mut self) {
        if let Some(diagnostics) = &self.diagnostics
            && let Ok(d) = diagnostics.lock()
        {
            if self.finished
                && let Some(kind) = d.failure_kind.as_deref()
            {
                self.usage.failure_kind = Some(
                    if explanation(kind).is_some() {
                        kind
                    } else {
                        "upstream_error"
                    }
                    .into(),
                );
            }
            if let Some(fault) = &d.fault {
                record_fault(&self.state, &self.model, fault);
            }
        }
        let failure = if !self.finished {
            if self.usage.failure_kind.is_none() {
                Some("client_cancelled")
            } else {
                None
            }
        } else if self.usage.failure_kind.is_some() {
            None
        } else if self.error {
            Some("upstream_error")
        } else if !(200..300).contains(&self.usage.status) {
            Some("http_error")
        } else if self.malformed {
            Some("malformed_response")
        } else if self.stream && !self.message_stopped {
            Some("truncated_stream")
        } else if self.unreadable {
            None
        } else if self.stream && !self.message_started {
            Some("truncated_stream")
        } else if !self.stream && !self.message {
            Some("malformed_response")
        } else {
            None
        };
        if let Some(kind) = failure {
            self.usage.failure_kind = Some(kind.into());
        }
        if let Some(kind) = self.usage.failure_kind.as_deref() {
            self.usage.outcome = Some(
                if kind == "client_cancelled" {
                    "client_cancelled"
                } else {
                    "failed"
                }
                .into(),
            );
            self.usage.failure_detail = explanation(kind).map(str::to_owned);
            if kind == "client_cancelled" && self.bytes == 0 {
                self.usage.status = 499;
            }
        } else {
            self.usage.outcome = Some(
                if self.unreadable {
                    "unknown"
                } else {
                    "completed"
                }
                .into(),
            );
        }
        self.usage.latency_ms = self.started.elapsed().as_millis() as u64;
        if let Ok(catalog) = self.state.store.catalog() {
            price_usage(&mut self.usage, &catalog, &self.model, self.reported_cost);
        }
        if self.state.store.record_usage(&self.usage).is_err() {
            tracing::warn!("Could not persist request usage");
        }
        self.hold.take();
    }
}
fn tap_response(
    state: AppState,
    response: Response,
    model: Model,
    mut usage: Usage,
    started: Instant,
    hold: Option<Hold>,
) -> Response {
    let (mut parts, body) = response.into_parts();
    let diagnostics = parts
        .extensions
        .get::<std::sync::Arc<std::sync::Mutex<providers::StreamDiagnostics>>>()
        .cloned();
    for (key, target) in [
        ("x-knowwhere-resolved-model", &mut usage.resolved_model_id),
        ("x-knowwhere-generation-id", &mut usage.generation_id),
    ] {
        *target = parts
            .headers
            .remove(key)
            .and_then(|v| v.to_str().ok().map(str::to_owned));
    }
    let reported_cost = parts
        .headers
        .remove("x-knowwhere-reported-cost")
        .and_then(|v| v.to_str().ok().and_then(|v| v.parse::<f64>().ok()))
        .filter(|v| v.is_finite() && *v >= 0.);
    let stream = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_ascii_lowercase().contains("text/event-stream"));
    usage.is_streaming = Some(stream);
    let mut tap = Tap {
        state,
        model,
        stream,
        usage,
        started,
        hold,
        pending: vec![],
        data: vec![],
        json: vec![],
        finished: false,
        bytes: 0,
        reported_cost,
        message_started: false,
        message_stopped: false,
        message: false,
        error: false,
        malformed: false,
        unreadable: false,
        line_overflow: false,
        event_overflow: false,
        diagnostics,
    };
    let stream = async_stream::stream! {
        let mut upstream=body.into_data_stream();
        while let Some(chunk)=upstream.next().await {
            match chunk {
                Ok(bytes)=>{tap.feed(&bytes);yield Ok::<_,std::io::Error>(bytes);},
                Err(_)=>{tap.interrupted();yield Err(std::io::Error::other("The upstream response was interrupted"));break;}
            }
        }
        tap.finish();drop(tap);
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

#[derive(serde::Deserialize)]
pub struct ModelQuery {
    limit: Option<usize>,
    after_id: Option<String>,
    before_id: Option<String>,
}
pub async fn models(
    State(state): State<AppState>,
    query: std::result::Result<Query<ModelQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let q = match query {
        Ok(Query(q)) => q,
        Err(_) => {
            return error(
                400,
                "invalid_request_error",
                "limit must be between 1 and 1000",
            );
        }
    };
    let c = match state.store.catalog() {
        Ok(c) => c,
        Err(_) => return error(500, "api_error", "Catalog unavailable"),
    };
    let limit = q.limit.unwrap_or(20);
    if !(1..=1000).contains(&limit) {
        return error(
            400,
            "invalid_request_error",
            "limit must be between 1 and 1000",
        );
    }
    let mut entries = Vec::new();
    for tier in TIERS {
        let members = c.tiers.get(tier).cloned().unwrap_or_default();
        if !members
            .iter()
            .any(|id| c.models.iter().any(|m| m.id == *id && m.enabled))
        {
            continue;
        }
        let mut claude = c
            .models
            .iter()
            .filter(|m| m.provider == "claude")
            .collect::<Vec<_>>();
        claude.sort_by(|a, b| a.id.cmp(&b.id));
        for m in claude {
            if c.aliases
                .iter()
                .find(|a| glob(&a.pattern, &m.id))
                .is_some_and(|a| a.tier == tier)
            {
                entries.push(json!({"id":m.id,"type":"model","display_name":m.id,"created_at":"1970-01-01T00:00:00+00:00"}))
            }
        }
    }
    if let Some(id) = q.after_id
        && let Some(pos) = entries.iter().position(|e| e["id"] == id)
    {
        entries = entries.split_off(pos + 1)
    }
    if let Some(id) = q.before_id
        && let Some(pos) = entries.iter().position(|e| e["id"] == id)
    {
        entries.truncate(pos)
    }
    let more = entries.len() > limit;
    entries.truncate(limit);
    Json(json!({"first_id":entries.first().map(|v|&v["id"]),"last_id":entries.last().map(|v|&v["id"]),"has_more":more,"data":entries})).into_response()
}
pub async fn subscription_usage(state: &AppState) -> Result<Value> {
    let c = state.store.catalog()?;
    let mut credentials = Vec::new();
    let mut counted = Vec::new();
    let cooldowns = state.store.cooldowns()?;
    for provider in ["claude", "codex"] {
        if state.store.get_secret(provider)?.is_none() {
            continue;
        }
        let mut statuses = Vec::new();
        for m in c.models.iter().filter(|m| {
            m.provider == provider && m.enabled && c.tiers.values().any(|ids| ids.contains(&m.id))
        }) {
            let status = status(state, m).await?;
            if ["ok", "cooling", "auth_rejected"].contains(&status.as_str()) {
                statuses.push(status.clone());
                counted.push((m.id.clone(), status));
            }
        }
        let value = if statuses.iter().any(|s| s == "ok") {
            "ok"
        } else if statuses.iter().any(|s| s == "auth_rejected") {
            "auth_rejected"
        } else if !statuses.is_empty() {
            "cooling"
        } else {
            "unknown"
        };
        credentials.push(json!({"provider":provider,"source":"managed","enabled":true,"routable":value!="auth_rejected","state":value}));
    }
    let all = !counted.is_empty() && counted.iter().all(|(_, s)| s != "ok");
    let resumes_at = if all {
        cooldowns
            .into_iter()
            .filter(|cd| cd.until > now() && counted.iter().any(|(id, _)| id == &cd.model_id))
            .map(|cd| cd.until)
            .min()
            .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
            .map(|n| n.to_rfc3339())
    } else {
        None
    };
    Ok(
        json!({"as_of":chrono::Utc::now().to_rfc3339(),"all_exhausted":all,"resumes_at":resumes_at,"known_credentials":credentials.len(),"credentials":credentials}),
    )
}
pub async fn subscriptions(State(state): State<AppState>) -> Response {
    match subscription_usage(&state).await {
        Ok(v) => Json(v).into_response(),
        Err(_) => error(500, "api_error", "Usage unavailable"),
    }
}
pub async fn session_cost(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.session_usages(&id){Ok(rows)if !rows.is_empty()=>Json(json!({"session_id":id,"request_count":rows.len(),"actual_cost_usd_micros":rows.iter().fold(0i64,|total,u|total.saturating_add(u.cost_usd_micros)),"unknown_cost_request_count":rows.iter().filter(|u|u.cost_source.as_deref()==Some("unknown")).count(),"estimated_cost_request_count":rows.iter().filter(|u|u.cost_source.as_deref().is_none_or(|s|s=="estimated")).count()})).into_response(),Ok(_)=>error(404,"not_found_error","No requests recorded for this session"),Err(_)=>error(500,"api_error","Usage unavailable")}
}

fn safe_stop(reason: &str) -> Option<String> {
    [
        "end_turn",
        "max_tokens",
        "stop_sequence",
        "tool_use",
        "pause_turn",
        "refusal",
        "model_context_window_exceeded",
    ]
    .contains(&reason)
    .then(|| reason.into())
}
fn explanation(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "upstream_timeout" => "The upstream did not respond before the timeout.",
        "upstream_unreachable" => "The upstream could not be reached.",
        "truncated_stream" => "The stream ended without a message_stop completion event.",
        "malformed_response" => "The upstream response could not be read as a valid message.",
        "upstream_error" => "The upstream reported an error. Upstream error text is not retained.",
        "http_error" => "The request ended with an unsuccessful HTTP status.",
        "client_cancelled" => {
            "The client disconnected or cancelled the request before it completed."
        }
        "transport_error" => "The response was interrupted by a transport failure.",
        "tier_exhausted" => "No eligible model in this tier could serve the request.",
        "context_overflow" => "The request exceeded the available models' context windows.",
        _ => return None,
    })
}

fn snapshot_of(dated: &str, alias: &str) -> bool {
    dated
        .to_ascii_lowercase()
        .strip_prefix(&format!("{}-", alias.to_ascii_lowercase()))
        .is_some_and(|suffix| suffix.len() == 8 && suffix.bytes().all(|c| c.is_ascii_digit()))
}
fn is_auto(id: &str) -> bool {
    ["openrouter/auto", "openrouter/auto-beta"].contains(&id)
}
fn price_usage(u: &mut Usage, c: &Catalog, m: &Model, reported: Option<f64>) {
    if let Some(cost) = reported {
        u.cost_usd_micros = crate::pricing::reported(cost);
        u.cost_source = Some("reported".into());
    } else {
        let price_id = if m.provider == "openrouter" {
            match u.resolved_model_id.as_deref() {
                Some(resolved) if resolved != m.upstream_model_id => c
                    .models
                    .iter()
                    .find(|other| {
                        other.provider == "openrouter" && other.upstream_model_id == resolved
                    })
                    .map(|m| m.id.as_str()),
                _ => Some(m.id.as_str()),
            }
        } else {
            Some(m.id.as_str())
        };
        let price = if m.provider == "openrouter"
            && is_auto(&m.upstream_model_id)
            && u.resolved_model_id.as_deref().is_none_or(is_auto)
        {
            None
        } else {
            price_id.and_then(|id| c.prices.get(id))
        };
        u.cost_source = Some(
            if price.is_some() {
                "estimated"
            } else {
                "unknown"
            }
            .into(),
        );
        if let Some(p) = price {
            u.cost_usd_micros = crate::pricing::estimated(p, u);
        }
    }
    if m.provider == "openrouter" && u.resolved_model_id.as_deref().is_some_and(is_auto) {
        u.resolved_model_id = None
    }
}

fn validate_effort(e: &Value) -> Result<()> {
    if !e.is_object() {
        bail!("Effort mapping must be an object")
    }
    let mode = match e.get("mode") {
        None => "none",
        Some(v) => v
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Effort mode must be a string"))?,
    };
    if !["none", "reasoning_effort", "chat_template_kwargs"].contains(&mode) {
        bail!("Unknown effort mode")
    }
    if let Some(levels) = e.get("levels").filter(|v| !v.is_null()) {
        let levels = levels
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Effort levels must be an array"))?;
        for l in levels {
            if !EFFORTS.contains(&l.as_str().unwrap_or("")) {
                bail!("Unknown effort level")
            }
        }
    }
    if let Some(default) = e.get("default").filter(|v| !v.is_null())
        && !EFFORTS.contains(&default.as_str().unwrap_or(""))
    {
        bail!("Unknown default effort level")
    }
    if let Some(kwargs) = e.get("kwargs").filter(|v| !v.is_null()) {
        let kwargs = kwargs
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Effort kwargs must be an object"))?;
        if kwargs.values().any(|v| !v.is_object()) {
            bail!("Every effort kwargs entry must be an object")
        }
    }
    if mode == "chat_template_kwargs" {
        for level in e["levels"].as_array().into_iter().flatten() {
            if !e["kwargs"][level.as_str().unwrap_or("")].is_object() {
                bail!("Every effort level needs chat template kwargs")
            }
        }
    }
    Ok(())
}
fn validate_openrouter(v: &Value) -> Result<()> {
    if !v.is_object() {
        bail!("OpenRouter options must be an object")
    }
    if let Some(tier) = v.get("costTier").filter(|v| !v.is_null())
        && !["low", "medium", "high", "xhigh", "max"].contains(&tier.as_str().unwrap_or(""))
    {
        bail!("Unknown OpenRouter cost tier")
    }
    for name in ["allowedModels", "excludedModels"] {
        if let Some(models) = v.get(name).filter(|v| !v.is_null())
            && !models.as_array().is_some_and(|a| {
                a.iter().all(|v| {
                    v.as_str().is_some_and(|s| {
                        !s.trim().is_empty() && !s.chars().any(char::is_whitespace)
                    })
                })
            })
        {
            bail!("OpenRouter model constraints must be arrays of nonempty strings")
        }
    }
    for name in ["maxPromptPrice", "maxCompletionPrice"] {
        if let Some(price) = v.get(name).filter(|v| !v.is_null())
            && !price.as_f64().is_some_and(|n| n.is_finite() && n >= 0.)
        {
            bail!("OpenRouter price ceilings must be nonnegative")
        }
    }
    Ok(())
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    async fn observe(bytes: Vec<u8>, stream: bool, chunk: usize, status: u16) -> Usage {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        let state = AppState::open(config).unwrap();
        let model = state
            .store
            .catalog()
            .unwrap()
            .models
            .into_iter()
            .find(|m| m.provider == "local")
            .unwrap();
        let pieces = bytes
            .chunks(chunk)
            .map(|s| Ok::<_, std::io::Error>(Bytes::copy_from_slice(s)))
            .collect::<Vec<_>>();
        let mut response = Response::new(Body::from_stream(futures_util::stream::iter(pieces)));
        response.headers_mut().insert(
            "content-type",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
            .parse()
            .unwrap(),
        );
        *response.status_mut() = axum::http::StatusCode::from_u16(status).unwrap();
        let row = Usage {
            at: now(),
            model_id: model.id.clone(),
            provider: model.provider.clone(),
            status,
            is_streaming: Some(stream),
            ..Default::default()
        };
        let response = tap_response(state.clone(), response, model, row, Instant::now(), None);
        axum::body::to_bytes(response.into_body(), 20 * 1024 * 1024)
            .await
            .unwrap();
        state.store.usages(1, None).unwrap().remove(0)
    }
    const START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n";
    const STOP: &str = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    #[tokio::test]
    async fn chunked_sse_requires_completion_and_merges_usage() {
        for chunk in [1, 9, 8192] {
            let u = observe(format!("{START}{STOP}").into_bytes(), true, chunk, 200).await;
            assert_eq!(u.outcome.as_deref(), Some("completed"));
            assert_eq!((u.input_tokens, u.output_tokens), (7, 3));
            assert_eq!(u.stop_reason.as_deref(), Some("end_turn"));
            assert!(u.failure_kind.is_none());
        }
    }
    #[tokio::test]
    async fn truncated_stream_keeps_tokens_and_sanitized_explanation() {
        let u = observe(START.as_bytes().into(), true, 7, 200).await;
        assert_eq!(u.outcome.as_deref(), Some("failed"));
        assert_eq!(u.failure_kind.as_deref(), Some("truncated_stream"));
        assert_eq!(u.input_tokens, 7);
        assert!(u.failure_detail.unwrap().contains("message_stop"));
    }
    #[tokio::test]
    async fn error_even_with_later_stop_never_retains_provider_text() {
        let body = format!(
            "{START}event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"secret-error-code\",\"message\":\"secret-prompt-token\"}}}}\n\n{STOP}"
        );
        let u = observe(body.into_bytes(), true, 13, 200).await;
        assert_eq!(u.outcome.as_deref(), Some("failed"));
        assert_eq!(u.failure_kind.as_deref(), Some("upstream_error"));
        assert!(!serde_json::to_string(&u).unwrap().contains("secret"));
    }
    #[tokio::test]
    async fn error_event_name_counts_even_with_bad_or_missing_data() {
        for tail in ["event: error\n\n", "event: error\ndata: invalid\n\n"] {
            let u = observe(format!("{START}{tail}{STOP}").into_bytes(), true, 2, 200).await;
            assert_eq!(u.failure_kind.as_deref(), Some("upstream_error"));
        }
    }
    #[tokio::test]
    async fn stop_reason_is_allowlisted() {
        let u = observe(
            br#"{"type":"message","content":[],"stop_reason":"secret-prompt"}"#.to_vec(),
            false,
            7,
            200,
        )
        .await;
        assert_eq!(u.outcome.as_deref(), Some("completed"));
        assert_eq!(u.stop_reason, None);
    }
    #[tokio::test]
    async fn multiline_crlf_and_final_unterminated_event_are_observed() {
        let data = format!(
            "{START}event: message_delta\r\ndata: {{\"type\":\"message_delta\",\r\ndata: \"usage\":{{\"output_tokens\":9}}}}\r\n\r\ndata: {{\"type\":\"message_stop\"}}"
        );
        let u = observe(data.into_bytes(), true, 3, 200).await;
        assert_eq!(u.outcome.as_deref(), Some("completed"));
        assert_eq!(u.output_tokens, 9);
    }
    #[tokio::test]
    async fn oversized_nonstream_is_unknown() {
        let body=json!({"type":"message","content":[{"type":"text","text":"x".repeat(9*1024*1024)}],"usage":{"input_tokens":999}}).to_string().into_bytes();
        let u = observe(body, false, 65536, 200).await;
        assert_eq!(u.outcome.as_deref(), Some("unknown"));
        assert_eq!(u.input_tokens, 0);
    }
    #[tokio::test]
    async fn successful_http_with_invalid_body_is_failed() {
        for body in [
            "null",
            "{}",
            "not json",
            r#"{"type":"message","content":"wrong"}"#,
        ] {
            let u = observe(body.as_bytes().into(), false, 8, 200).await;
            assert_eq!(u.failure_kind.as_deref(), Some("malformed_response"));
        }
    }
    #[test]
    fn auto_router_prices_configured_resolved_entry_or_reports_unknown() {
        let mut c: Catalog = serde_json::from_str(include_str!("../seed.json")).unwrap();
        let model:Model=serde_json::from_value(json!({"id":"auto","provider":"openrouter","upstreamModelId":"openrouter/auto","enabled":true})).unwrap();
        let fixed:Model=serde_json::from_value(json!({"id":"my-fixed","provider":"openrouter","upstreamModelId":"vendor/model","enabled":true})).unwrap();
        c.models.extend([model.clone(), fixed]);
        c.prices.insert(
            "my-fixed".into(),
            Price {
                input: 2.,
                output: 3.,
                ..Default::default()
            },
        );
        c.prices.insert(
            "auto".into(),
            Price {
                input: 99.,
                ..Default::default()
            },
        );
        let mut u = Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        price_usage(&mut u, &c, &model, None);
        assert_eq!(u.cost_source.as_deref(), Some("unknown"));
        u.resolved_model_id = Some("vendor/model".into());
        price_usage(&mut u, &c, &model, None);
        assert_eq!(u.cost_usd_micros, 35);
        assert_eq!(u.cost_source.as_deref(), Some("estimated"));
        price_usage(&mut u, &c, &model, Some(0.003));
        assert_eq!(u.cost_usd_micros, 3000);
        assert_eq!(u.cost_source.as_deref(), Some("reported"));
    }
    #[tokio::test]
    async fn unknown_discovered_models_do_not_count_as_subscription_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        let state = AppState::open(config).unwrap();
        state.store.set_secret("claude", "opaque").unwrap();
        state
            .store
            .set_setting(
                "models:claude",
                &json!({"modelIds":["unconfigured"]}).to_string(),
            )
            .unwrap();
        let report = subscription_usage(&state).await.unwrap();
        assert_eq!(report["all_exhausted"], false);
        assert_eq!(report["credentials"][0]["state"], "unknown");
        assert_eq!(report["credentials"][0]["enabled"], true);
        assert_eq!(report["credentials"][0]["routable"], true);
    }
}

//! Local operator console. Network/origin access checks are applied by the parent router.
use crate::{
    AppState,
    types::{Catalog, Model, TIERS, now},
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::header,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Mutex, OnceLock},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ui", get(index))
        .route("/ui/", get(index))
        .route("/ui/settings", get(index))
        .route("/ui/requests/{id}", get(index))
        .route("/ui/pins", get(index))
        .route("/ui/assets/style.css", get(css))
        .route("/ui/assets/app.js", get(js))
        .route("/ui/api/snapshot", get(snapshot))
        .route("/ui/api/settings", get(settings))
        .route("/ui/api/settings/{section}", put(save_section))
        .route("/ui/api/catalog", put(save_catalog))
        .route("/ui/api/models", post(save_model))
        .route("/ui/api/models/{id}", delete(remove_model))
        .route("/ui/api/test-connection", post(test_connection))
        .route("/ui/api/refresh", post(refresh))
        .route("/ui/api/gateway-key", get(gateway_key))
        .route("/ui/api/gateway-key/rotate", post(rotate_key))
        .route("/ui/api/requests", get(requests))
        .route("/ui/api/requests/{id}", get(request_detail))
        .route("/ui/api/pins", get(pin_history).delete(unpin))
        .route("/ui/api/charts", get(charts))
        .route(
            "/ui/api/accounts/{provider}",
            put(save_account).delete(delete_account),
        )
        .route("/ui/api/accounts/{provider}/connect", post(connect))
        .route("/ui/api/accounts/{provider}/finish", post(finish))
        .layer(axum::middleware::from_fn(console_headers))
}
async fn console_headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert(header::CONTENT_SECURITY_POLICY,"default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    h.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
async fn index() -> Html<&'static str> {
    Html(include_str!("../assets/dashboard.html"))
}
async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/style.css"),
    )
}
async fn js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/app.js"),
    )
}
fn result(r: anyhow::Result<Value>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => crate::error(400, "settings_error", &e.to_string()),
    }
}
fn account_rows(s: &AppState) -> anyhow::Result<(Vec<Value>, Value)> {
    let mut accounts = Vec::new();
    let mut discovered = serde_json::Map::new();
    for provider in ["claude", "codex", "openrouter"] {
        let list = s
            .store
            .get_setting(&format!("models:{provider}"))?
            .and_then(|v| serde_json::from_str::<Value>(&v).ok())
            .unwrap_or(json!({}));
        let connected = s.store.get_secret(provider)?.is_some();
        accounts.push(json!({"provider":provider,"connected":connected,"loggedIn":connected,"listFetchedAt":list["fetchedAt"],"listCheckedAt":list["checkedAt"],"listedModels":list["modelIds"].as_array().map(Vec::len),"listError":list["lastError"]}));
        discovered.insert(
            provider.into(),
            list["modelIds"]
                .as_array()
                .cloned()
                .map(Value::Array)
                .unwrap_or(json!([])),
        );
    }
    Ok((accounts, Value::Object(discovered)))
}
async fn snapshot(State(s): State<AppState>) -> Response {
    result(
        async {
            let mut value = s.monitoring().await?;
            let (mut accounts, _) = account_rows(&s)?;
            for a in &mut accounts {
                if let Some(credential) = value
                    .pointer("/subscriptions/credentials")
                    .and_then(Value::as_array)
                    .and_then(|rows| rows.iter().find(|r| r["provider"] == a["provider"]))
                {
                    a["state"] = credential["state"].clone();
                }
            }
            value["accounts"] = json!(accounts);
            let tls = match s.store.get_setting("tls:active_metadata")? {
                Some(active) => Some(active),
                None => s.store.get_setting("tls:metadata")?,
            };
            value["tls"] = tls
                .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                .unwrap_or(Value::Null);
            Ok(value)
        }
        .await,
    )
}
async fn settings(State(s): State<AppState>) -> Response {
    result((|| {
        let catalog = s.store.catalog()?;
        let mut models = Vec::new();
        for model in &catalog.models {
            let mut v = serde_json::to_value(model)?;
            v.as_object_mut().unwrap().remove("apiKey");
            v["hasApiKey"] = json!(
                s.store
                    .get_secret(&format!("model:{}", model.id))?
                    .is_some()
            );
            models.push(v);
        }
        let (accounts, discovered) = account_rows(&s)?;
        Ok(
            json!({"models":models,"tiers":catalog.tiers,"aliases":catalog.aliases,"defaultTier":catalog.default_tier,"prices":catalog.prices,"accounts":accounts,"discovered":discovered,"server":{"listenAddress":s.config.listen_address.to_string(),"port":s.config.port,"httpsPort":s.config.https_port}}),
        )
    })())
}
fn validate(c: &Catalog) -> anyhow::Result<()> {
    crate::routing::validate_catalog(c)?;
    anyhow::ensure!(
        TIERS.contains(&c.default_tier.as_str()),
        "Choose a valid default tier."
    );
    let mut ids = HashSet::new();
    for m in &c.models {
        anyhow::ensure!(
            valid_id(&m.id),
            "Model ID must start with a letter or digit and contain at most 200 letters, digits or . _ : / @ -."
        );
        anyhow::ensure!(
            ids.insert(m.id.to_lowercase()),
            "Model IDs must be unique, ignoring case."
        );
        anyhow::ensure!(
            ["claude", "codex", "local", "openrouter", "systemone"].contains(&m.provider.as_str()),
            "Unknown provider."
        );
        anyhow::ensure!(
            !m.upstream_model_id.trim().is_empty(),
            "Upstream model ID is required."
        );
        if m.provider == "local" || m.provider == "systemone" {
            valid_url(m.base_url.as_deref().unwrap_or(""))?;
        }
        for n in [m.context_window, m.max_prompt_tokens, m.max_conversations] {
            anyhow::ensure!(
                n != Some(0),
                "Token and conversation limits must be positive or empty."
            );
        }
    }
    for (tier, members) in &c.tiers {
        anyhow::ensure!(TIERS.contains(&tier.as_str()), "Unknown tier.");
        let mut seen = HashSet::new();
        for id in members {
            let m = c
                .models
                .iter()
                .find(|m| &m.id == id)
                .ok_or_else(|| anyhow::anyhow!("Unknown tier member {id}."))?;
            anyhow::ensure!(
                m.provider != "systemone",
                "SystemOne decision models cannot join message tiers."
            );
            anyhow::ensure!(seen.insert(id), "A tier contains a duplicate model.");
        }
    }
    let mut patterns = HashSet::new();
    for a in &c.aliases {
        anyhow::ensure!(
            !a.pattern.trim().is_empty()
                && a.pattern.len() <= 200
                && a.pattern.matches('*').count() <= 4,
            "Alias patterns require 1–200 characters and at most four wildcards."
        );
        anyhow::ensure!(
            patterns.insert(&a.pattern),
            "An alias pattern is listed twice."
        );
        anyhow::ensure!(
            TIERS.contains(&a.tier.as_str()),
            "Alias names an unknown tier."
        );
    }
    for p in c.prices.values() {
        for v in [
            p.input,
            p.output,
            p.cache_read,
            p.cache_write,
            p.cache_write1h.unwrap_or(p.cache_write),
        ] {
            anyhow::ensure!(
                v.is_finite() && (0.0..=1_000_000.0).contains(&v),
                "Prices must be between 0 and 1,000,000 USD per million tokens."
            );
        }
    }
    Ok(())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@-".contains(&b))
}
fn valid_url(value: &str) -> anyhow::Result<url::Url> {
    let u = url::Url::parse(value)
        .map_err(|_| anyhow::anyhow!("Base URL must be an absolute http:// or https:// URL."))?;
    anyhow::ensure!(
        ["http", "https"].contains(&u.scheme())
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "Base URL must be HTTP(S) without credentials, query or fragment."
    );
    Ok(u)
}
async fn save_catalog(State(s): State<AppState>, Json(c): Json<Catalog>) -> Response {
    let assignment = s.runtime.assignment.lock().await;
    let r = (|| {
        validate(&c)?;
        let old = s.store.catalog()?;
        anyhow::ensure!(
            c.models.iter().all(|m| m.api_key.is_none()),
            "Use the model editor to change credentials."
        );
        anyhow::ensure!(
            old.models.iter().all(|m| c
                .models
                .iter()
                .any(|n| n.id == m.id && n.provider == m.provider)),
            "Use the model editor to remove, rename or change models."
        );
        s.store.save_catalog(&c)?;
        Ok(json!({"saved":true}))
    })();
    drop(assignment);
    if r.is_ok() {
        crate::refresh_health(&s).await;
    }
    result(r)
}
/// Section edits merge with the latest catalog under the same gate as assignment
/// and model edits, so stale browser forms cannot revert unrelated settings.
async fn save_section(
    State(s): State<AppState>,
    Path(section): Path<String>,
    Json(input): Json<Value>,
) -> Response {
    let assignment = s.runtime.assignment.lock().await;
    let r = (|| {
        let mut catalog = s.store.catalog()?;
        match section.as_str() {
            "tiers" => {
                if let Some(tiers) = input.get("tiers") {
                    catalog.tiers = serde_json::from_value(tiers.clone())?;
                }
                if let Some(tier) = input.get("defaultTier") {
                    catalog.default_tier = tier
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Default tier must be a string."))?
                        .into();
                }
            }
            "aliases" => {
                catalog.aliases = serde_json::from_value(
                    input
                        .get("aliases")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Alias rules are required."))?,
                )?;
                for alias in &mut catalog.aliases {
                    alias.pattern = alias.pattern.trim().into();
                }
            }
            "prices" => {
                let prices = input["prices"].as_object().ok_or_else(|| {
                    anyhow::anyhow!("Prices must be an object keyed by model ID.")
                })?;
                for (id, value) in prices {
                    anyhow::ensure!(
                        catalog.models.iter().any(|m| m.id == *id),
                        "Unknown model {id}. Reload settings before editing prices."
                    );
                    if value.is_null() {
                        catalog.prices.remove(id);
                    } else {
                        catalog
                            .prices
                            .insert(id.clone(), serde_json::from_value(value.clone())?);
                    }
                }
            }
            "model-enabled" => {
                let id = input["id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Model ID is required."))?;
                let enabled = input["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("Enabled must be true or false."))?;
                catalog
                    .models
                    .iter_mut()
                    .find(|m| m.id == id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown model {id}."))?
                    .enabled = enabled;
            }
            _ => anyhow::bail!("Unknown settings section."),
        }
        validate(&catalog)?;
        s.store.save_catalog(&catalog)?;
        Ok(json!({"saved":true}))
    })();
    drop(assignment);
    if r.is_ok() && section == "model-enabled" {
        crate::refresh_health(&s).await;
    }
    result(r)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelEdit {
    #[serde(flatten)]
    model: Model,
    existing_id: Option<String>,
    new_api_key: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
}
async fn save_model(State(s): State<AppState>, Json(mut edit): Json<ModelEdit>) -> Response {
    let assignment = s.runtime.assignment.lock().await;
    edit.model.id = edit.model.id.trim().into();
    let rename = edit
        .existing_id
        .as_ref()
        .filter(|id| **id != edit.model.id)
        .map(|id| (id.clone(), edit.model.id.clone()));
    let r = (|| {
        let mut c = s.store.catalog()?;
        edit.model.id = edit.model.id.trim().into();
        edit.model.upstream_model_id = edit.model.upstream_model_id.trim().into();
        if let Some(id) = edit.existing_id.as_deref() {
            let old = c
                .models
                .iter()
                .find(|m| m.id == id)
                .ok_or_else(|| anyhow::anyhow!("Model no longer exists."))?;
            anyhow::ensure!(
                old.provider == edit.model.provider,
                "A model's provider cannot be changed."
            );
            anyhow::ensure!(
                id == edit.model.id
                    || ["local", "systemone", "openrouter"].contains(&old.provider.as_str()),
                "Subscription models cannot be renamed."
            );
            for m in &mut c.models {
                if m.id == id {
                    *m = edit.model.clone();
                }
            }
            if id != edit.model.id {
                for members in c.tiers.values_mut() {
                    for member in members {
                        if member == id {
                            *member = edit.model.id.clone();
                        }
                    }
                }
                if let Some(p) = c.prices.remove(id) {
                    c.prices.insert(edit.model.id.clone(), p);
                }
            }
        } else {
            anyhow::ensure!(
                !c.models
                    .iter()
                    .any(|m| m.id.eq_ignore_ascii_case(&edit.model.id)),
                "That model ID already exists."
            );
            c.models.push(edit.model.clone());
        }
        if edit.model.provider == "systemone" {
            for members in c.tiers.values_mut() {
                members.retain(|id| id != &edit.model.id);
            }
        }
        validate(&c)?;
        if let Some(id) = edit.existing_id.as_deref()
            && id != edit.model.id
        {
            s.store.rename_model(id, &edit.model.id)?;
        }
        let saved = c.models.iter_mut().find(|m| m.id == edit.model.id).unwrap();
        saved.api_key = if let Some(k) = edit.new_api_key.filter(|k| !k.is_empty()) {
            Some(k)
        } else if edit.clear_api_key {
            Some(String::new())
        } else {
            None
        };
        s.store.save_catalog(&c)?;
        Ok(json!({"saved":true}))
    })();
    if r.is_ok()
        && let Some((old, new)) = rename
    {
        {
            // Hold::drop locks in this same order. Move counters and their alias
            // together so an in-flight response cannot release between the two.
            let mut conversations = s
                .runtime
                .conversations
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut renamed = s.runtime.renamed.lock().unwrap_or_else(|e| e.into_inner());
            for target in renamed.values_mut() {
                if *target == old {
                    *target = new.clone();
                }
            }
            renamed.insert(old.clone(), new.clone());
            renamed.retain(|source, target| source != target);
            let keys = conversations
                .keys()
                .filter(|(model, _)| model == &old)
                .cloned()
                .collect::<Vec<_>>();
            for key in keys {
                if let Some(value) = conversations.remove(&key) {
                    conversations.insert((new.clone(), key.1), value);
                }
            }
        }
        {
            let mut health = s.runtime.health.write().await;
            if let Some(value) = health.remove(&old) {
                health.insert(new.clone(), value);
            }
        }
    }
    drop(assignment);
    if r.is_ok() {
        crate::refresh_health(&s).await;
    }
    result(r)
}
async fn remove_model(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let assignment = s.runtime.assignment.lock().await;
    let r = (|| {
        let mut c = s.store.catalog()?;
        let m = c
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| anyhow::anyhow!("Unknown model."))?;
        anyhow::ensure!(
            ["local", "systemone", "openrouter"].contains(&m.provider.as_str()),
            "Subscription models can be disabled or removed from tiers."
        );
        c.models.retain(|m| m.id != id);
        for members in c.tiers.values_mut() {
            members.retain(|m| m != &id);
        }
        c.prices.remove(&id);
        s.store.save_catalog(&c)?;
        s.store.delete_secret(&format!("model:{id}"))?;
        Ok(json!({"removed":true}))
    })();
    drop(assignment);
    if r.is_ok() {
        crate::refresh_health(&s).await;
    }
    result(r)
}
async fn gateway_key(State(s): State<AppState>) -> Response {
    result(s.store.gateway_key().map(|key| json!({"key":key})))
}
async fn rotate_key(State(s): State<AppState>) -> Response {
    result(s.store.rotate_gateway_key().map(|key| json!({"key":key})))
}
async fn save_account(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(v): Json<Value>,
) -> Response {
    result((|| {
        anyhow::ensure!(
            provider == "openrouter",
            "Use Connect to log into this provider."
        );
        let key = v["key"].as_str().unwrap_or("").trim();
        anyhow::ensure!(!key.is_empty(), "Enter an OpenRouter API key.");
        s.store.set_secret(&provider, key)?;
        s.store.clear_cooldowns(&provider)?;
        Ok(json!({"saved":true}))
    })())
}
async fn delete_account(State(s): State<AppState>, Path(provider): Path<String>) -> Response {
    result((|| {
        anyhow::ensure!(
            ["claude", "codex", "openrouter"].contains(&provider.as_str()),
            "Unknown account provider."
        );
        s.store.delete_secret(&provider)?;
        Ok(json!({"disconnected":true}))
    })())
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionTest {
    base_url: String,
    new_api_key: Option<String>,
    existing_id: Option<String>,
    #[serde(default)]
    clear_api_key: bool,
}
async fn test_connection(State(s): State<AppState>, Json(input): Json<ConnectionTest>) -> Response {
    let r = async {
        valid_url(&input.base_url)?;
        let key = match input.new_api_key.filter(|v| !v.is_empty()) {
            Some(k) => Some(k),
            None if !input.clear_api_key => input
                .existing_id
                .and_then(|id| s.store.get_secret(&format!("model:{id}")).ok().flatten()),
            _ => None,
        };
        let mut req = s
            .client
            .get(format!("{}/models", input.base_url.trim_end_matches('/')))
            .timeout(std::time::Duration::from_secs(10));
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        let response = req.send().await.map_err(|_| {
            anyhow::anyhow!(
                "Endpoint did not respond. Check its URL and whether the model server is running."
            )
        })?;
        anyhow::ensure!(
            response.status().is_success(),
            "Endpoint returned HTTP {}.",
            response.status().as_u16()
        );
        Ok(json!({"ok":true,"message":"Connection succeeded."}))
    }
    .await;
    result(r)
}
async fn refresh(State(s): State<AppState>) -> Response {
    let r = crate::providers::refresh(&s).await;
    crate::refresh_health(&s).await;
    result(r)
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    before: Option<i64>,
    session_key: Option<String>,
    tier: Option<String>,
    range: Option<String>,
}
async fn requests(State(s): State<AppState>, Query(q): Query<Page>) -> Response {
    result(s.store.usages(51, q.before).map(|mut rows| {
        let more = rows.len() > 50;
        rows.truncate(50);
        json!({"nextBeforeId":if more{rows.last().map(|r|r.id)}else{None},"rows":rows})
    }))
}
async fn request_detail(State(s): State<AppState>, Path(id): Path<i64>) -> Response {
    match s.store.usage(id) {
        Ok(Some(u)) => Json(u).into_response(),
        Ok(None) => crate::error(404, "not_found", "Request not found."),
        Err(_) => crate::error(500, "storage_error", "Could not load request."),
    }
}
async fn pin_history(State(s): State<AppState>, Query(q): Query<Page>) -> Response {
    result((|| {
        let key = q
            .session_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Session key is required."))?;
        let tier = q
            .tier
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Tier is required."))?;
        let mut rows = s.store.pin_usages(key, tier, q.before, 51)?;
        let more = rows.len() > 50;
        rows.truncate(50);
        Ok(
            json!({"pin":s.store.pin(key,tier)?,"nextBeforeId":if more{rows.last().map(|r|r.id)}else{None},"rows":rows}),
        )
    })())
}
async fn unpin(State(s): State<AppState>, Query(q): Query<Page>) -> Response {
    result((|| {
        s.store.delete_pin(
            q.session_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Session key is required."))?,
            q.tier
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Tier is required."))?,
        )?;
        Ok(json!({"removed":true}))
    })())
}
async fn charts(State(s): State<AppState>, Query(q): Query<Page>) -> Response {
    result((|| {
        let (step, count) = match q.range.as_deref() {
            Some("week") => (86400, 7),
            Some("month") => (86400, 30),
            _ => (3600, 24),
        };
        let end = now() / step * step + step;
        let start = end - step * count;
        let mut series: BTreeMap<String, (Vec<u64>, Vec<i64>)> = BTreeMap::new();
        let (mut requests, mut tokens, mut cost, mut unknown) = (0u64, 0u64, 0i64, 0u64);
        for u in s.store.usage_since(start)? {
            if u.at >= end {
                continue;
            }
            let i = ((u.at - start) / step) as usize;
            let entry = series
                .entry(u.model_id)
                .or_insert_with(|| (vec![0; count as usize], vec![0; count as usize]));
            entry.0[i] = entry.0[i].saturating_add(1);
            entry.1[i] = entry.1[i].saturating_add(u.cost_usd_micros);
            requests = requests.saturating_add(1);
            tokens = tokens
                .saturating_add(u.input_tokens)
                .saturating_add(u.output_tokens);
            cost = cost.saturating_add(u.cost_usd_micros);
            if u.cost_source.as_deref() == Some("unknown") {
                unknown += 1;
            }
        }
        Ok(
            json!({"buckets":(0..count).map(|i|start+i*step).collect::<Vec<_>>(),"series":series.into_iter().map(|(id,(requests,cost))|json!({"modelId":id,"requests":requests,"costUsdMicros":cost})).collect::<Vec<_>>(),"requests":requests,"tokens":tokens,"costUsdMicros":cost,"unknownCostRequests":unknown}),
        )
    })())
}
type PendingLogins = HashMap<String, (i64, String, Value)>;
fn pending() -> &'static Mutex<PendingLogins> {
    static PENDING: OnceLock<Mutex<PendingLogins>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}
async fn connect(State(s): State<AppState>, Path(provider): Path<String>) -> Response {
    let r=async{let id=uuid::Uuid::new_v4().to_string();let(secret,public)=match provider.as_str(){"claude"=>{let p=crate::providers::begin_claude(&s)?;let v=serde_json::to_value(p)?;(v.clone(),json!({"authorizeUrl":v["authorizeUrl"]}))},"codex"=>{let p=crate::providers::start_codex(&s).await?;let v=serde_json::to_value(p)?;(v.clone(),json!({"verificationUrl":v["verificationUrl"],"userCode":v["userCode"],"interval":v["interval"]}))},_=>anyhow::bail!("Unknown login provider.")};let mut map=pending().lock().map_err(|_|anyhow::anyhow!("Login state unavailable."))?;map.retain(|_,(at,_,_)|*at+900>now());anyhow::ensure!(map.len()<32,"Too many pending logins. Try again later.");map.insert(id.clone(),(now(),provider,secret));let mut out=public;out["loginId"]=json!(id);Ok(out)}.await;
    result(r)
}
async fn finish(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(v): Json<Value>,
) -> Response {
    let r = async {
        let id = v["loginId"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Login ID is required."))?;
        let data = {
            let map = pending()
                .lock()
                .map_err(|_| anyhow::anyhow!("Login state unavailable."))?;
            let (at, p, data) = map
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("Login expired. Start again."))?;
            anyhow::ensure!(
                *at + 900 > now() && p == &provider,
                "Login expired. Start again."
            );
            data.clone()
        };
        let connected = match provider.as_str() {
            "claude" => {
                let login = serde_json::from_value(data)?;
                crate::providers::finish_claude(&s, &login, v["code"].as_str().unwrap_or(""))
                    .await?;
                true
            }
            "codex" => {
                let login = serde_json::from_value(data)?;
                crate::providers::poll_codex(&s, &login).await?
            }
            _ => anyhow::bail!("Unknown provider."),
        };
        if connected {
            pending()
                .lock()
                .map_err(|_| anyhow::anyhow!("Login state unavailable."))?
                .remove(id);
            s.store.clear_cooldowns(&provider)?;
            // Credentials remain connected when discovery fails; its persisted
            // fetch status tells the settings page that a retry is needed.
            let _ = crate::providers::refresh_provider(&s, &provider).await;
        }
        Ok(json!({"connected":connected}))
    }
    .await;
    result(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::Config {
            data_dir: dir.path().into(),
            ..crate::Config::default()
        };
        let s = AppState::open(c).unwrap();
        (dir, s)
    }
    #[tokio::test]
    async fn partial_settings_preserve_unrelated_edits_and_missing_price_is_explicit() {
        let (_dir, s) = state();
        let original = s.store.catalog().unwrap();
        assert_eq!(
            call(
                &s,
                "PUT",
                "/ui/api/settings/model-enabled",
                Some(json!({"id":"claude-opus-5-5","enabled":false}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &s,
                "PUT",
                "/ui/api/settings/aliases",
                Some(json!({"aliases":[{"pattern":"*custom*","tier":"low"}]}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &s,
                "PUT",
                "/ui/api/settings/prices",
                Some(json!({"prices":{"claude-opus-5-5":null}}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &s,
                "PUT",
                "/ui/api/settings/tiers",
                Some(json!({"defaultTier":"mid"}))
            )
            .await
            .0,
            StatusCode::OK
        );
        let updated = s.store.catalog().unwrap();
        assert!(
            !updated
                .models
                .iter()
                .find(|m| m.id == "claude-opus-5-5")
                .unwrap()
                .enabled
        );
        assert_eq!(updated.aliases[0].pattern, "*custom*");
        assert_eq!(updated.default_tier, "mid");
        assert_eq!(updated.tiers, original.tiers);
        assert!(!updated.prices.contains_key("claude-opus-5-5"));
        assert_eq!(
            serde_json::to_value(&updated.prices["gpt-6.1-sol"]).unwrap(),
            serde_json::to_value(&original.prices["gpt-6.1-sol"]).unwrap()
        );
    }
    #[tokio::test]
    async fn concurrent_section_mutations_do_not_overwrite_each_other() {
        let (_dir, s) = state();
        let (a, b) = tokio::join!(
            call(
                &s,
                "PUT",
                "/ui/api/settings/model-enabled",
                Some(json!({"id":"claude-opus-5-5","enabled":false}))
            ),
            call(
                &s,
                "PUT",
                "/ui/api/settings/aliases",
                Some(json!({"aliases":[{"pattern":"*new*","tier":"low"}]}))
            )
        );
        assert_eq!(a.0, StatusCode::OK);
        assert_eq!(b.0, StatusCode::OK);
        let c = s.store.catalog().unwrap();
        assert!(
            !c.models
                .iter()
                .find(|m| m.id == "claude-opus-5-5")
                .unwrap()
                .enabled
        );
        assert_eq!(c.aliases[0].pattern, "*new*");
    }
    #[tokio::test]
    async fn model_save_rejects_invalid_effort_and_openrouter_configuration() {
        let (_dir, s) = state();
        let original = s.store.catalog().unwrap();
        let model = original.models.iter().find(|m| m.id == "mimo-2.6").unwrap();
        for (field, value) in [
            ("effort", json!({"mode":"unknown-mode"})),
            ("openRouter", json!({"maxPromptPrice":-1})),
        ] {
            let mut edit = serde_json::to_value(model).unwrap();
            edit["existingId"] = json!(model.id);
            edit[field] = value;
            assert_eq!(
                call(&s, "POST", "/ui/api/models", Some(edit)).await.0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            serde_json::to_value(s.store.catalog().unwrap()).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }
    #[tokio::test]
    async fn active_null_certificate_metadata_does_not_show_a_saved_unused_certificate() {
        let (_dir, s) = state();
        s.store
            .set_setting("tls:metadata", r#"{"lanUrl":"https://stale.local"}"#)
            .unwrap();
        s.store.set_setting("tls:active_metadata", "null").unwrap();
        let (code, v) = call(&s, "GET", "/ui/api/snapshot", None).await;
        assert_eq!(code, StatusCode::OK);
        assert!(v["tls"].is_null());
    }
    #[tokio::test]
    async fn successful_login_immediately_discovers_only_the_connected_provider() {
        let (_dir, mut s) = state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let server=Router::new().route("/token",post(||async{Json(json!({"access_token":"test-access","refresh_token":"test-refresh","expires_in":3600}))})).route("/v1/models",get(move||{let count=count.clone();async move{count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);Json(json!({"data":[{"id":"discovered-claude"}],"has_more":false}))}}));
        let task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        s.config.claude_token_url = format!("{base}/token");
        s.config.claude_api_base = base;
        s.config.codex_api_base = "http://127.0.0.1:9/should-not-fetch".into();
        let login = crate::providers::begin_claude(&s).unwrap();
        let login_id = uuid::Uuid::new_v4().to_string();
        pending().lock().unwrap().insert(
            login_id.clone(),
            (
                now(),
                "claude".into(),
                serde_json::to_value(&login).unwrap(),
            ),
        );
        let (code, v) = call(
            &s,
            "POST",
            "/ui/api/accounts/claude/finish",
            Some(json!({"loginId":login_id,"code":format!("approved#{}",login.state)})),
        )
        .await;
        task.abort();
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["connected"], true);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let list: Value =
            serde_json::from_str(&s.store.get_setting("models:claude").unwrap().unwrap()).unwrap();
        assert_eq!(list["modelIds"][0], "discovered-claude");
        assert!(s.store.get_setting("models:codex").unwrap().is_none());
    }
    async fn call(
        s: &AppState,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        let b = body.map(|v| v.to_string()).unwrap_or_default();
        let res = routes()
            .with_state(s.clone())
            .oneshot(req.body(Body::from(b)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 8 * 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    #[tokio::test]
    async fn settings_never_exposes_provider_or_model_secrets() {
        let (_dir, s) = state();
        s.store.set_secret("claude", "private-oauth-token").unwrap();
        s.store
            .set_secret("model:mimo-2.6", "private-local-key")
            .unwrap();
        let (status, v) = call(&s, "GET", "/ui/api/settings", None).await;
        assert_eq!(status, StatusCode::OK);
        let text = v.to_string();
        assert!(!text.contains("private-oauth-token"));
        assert!(!text.contains("private-local-key"));
        assert!(
            v["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == "mimo-2.6")
                .unwrap()["hasApiKey"]
                .as_bool()
                .unwrap()
        );
        assert!(
            v["models"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m.get("apiKey").is_none())
        );
    }
    #[tokio::test]
    async fn editable_model_rename_keeps_pin_and_secret() {
        let (_dir, s) = state();
        s.runtime
            .health
            .write()
            .await
            .insert("mimo-2.6".into(), true);
        s.runtime.conversations.lock().unwrap().insert(
            ("mimo-2.6".into(), "routing-key".into()),
            std::sync::Arc::new(Mutex::new((3, std::time::Instant::now()))),
        );
        s.store
            .set_secret("model:mimo-2.6", "preserve-key")
            .unwrap();
        s.store
            .assign_pin(
                &crate::types::Pin {
                    session_key: "routing-key".into(),
                    tier: "mid".into(),
                    model_id: "mimo-2.6".into(),
                    client_session_id: None,
                    created_at: now(),
                },
                false,
            )
            .unwrap();
        let mut v = serde_json::to_value(
            s.store
                .catalog()
                .unwrap()
                .models
                .into_iter()
                .find(|m| m.id == "mimo-2.6")
                .unwrap(),
        )
        .unwrap();
        v["existingId"] = json!("mimo-2.6");
        v["id"] = json!("local-renamed");
        let (status, _) = call(&s, "POST", "/ui/api/models", Some(v)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            s.store
                .get_secret("model:local-renamed")
                .unwrap()
                .as_deref(),
            Some("preserve-key")
        );
        assert_eq!(
            s.store.pin("routing-key", "mid").unwrap().unwrap().model_id,
            "local-renamed"
        );
        assert!(s.store.catalog().unwrap().tiers["mid"].contains(&"local-renamed".into()));
        assert_eq!(
            s.runtime.health.read().await.get("local-renamed"),
            Some(&true)
        );
        let conversations = s.runtime.conversations.lock().unwrap();
        assert_eq!(
            conversations
                .get(&("local-renamed".into(), "routing-key".into()))
                .unwrap()
                .lock()
                .unwrap()
                .0,
            3
        );
        assert!(!conversations.contains_key(&("mimo-2.6".into(), "routing-key".into())));
    }
    #[tokio::test]
    async fn rejected_rename_does_not_mutate_catalog() {
        let (_dir, s) = state();
        let mut v = serde_json::to_value(
            s.store
                .catalog()
                .unwrap()
                .models
                .into_iter()
                .find(|m| m.id == "mimo-2.6")
                .unwrap(),
        )
        .unwrap();
        v["existingId"] = json!("mimo-2.6");
        v["id"] = json!("claude-opus-5-5");
        assert_eq!(
            call(&s, "POST", "/ui/api/models", Some(v)).await.0,
            StatusCode::BAD_REQUEST
        );
        assert!(
            s.store
                .catalog()
                .unwrap()
                .models
                .iter()
                .any(|m| m.id == "mimo-2.6")
        );
    }
    #[tokio::test]
    async fn pin_history_uses_routing_key_and_tier() {
        let (_dir, s) = state();
        for (key, tier) in [("target", "mid"), ("target", "low"), ("another", "mid")] {
            s.store
                .record_usage(&crate::types::Usage {
                    at: now(),
                    session_key: Some(key.into()),
                    tier: Some(tier.into()),
                    client_session_id: Some("same-client".into()),
                    model_id: "mimo-2.6".into(),
                    ..Default::default()
                })
                .unwrap();
        }
        let (status, v) = call(&s, "GET", "/ui/api/pins?sessionKey=target&tier=mid", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["rows"].as_array().unwrap().len(), 1);
        assert_eq!(v["rows"][0]["tier"], "mid");
    }
    #[tokio::test]
    async fn request_history_has_stable_id_cursor() {
        let (_dir, s) = state();
        for _ in 0..52 {
            s.store
                .record_usage(&crate::types::Usage {
                    at: now(),
                    ..Default::default()
                })
                .unwrap();
        }
        let (_, v) = call(&s, "GET", "/ui/api/requests", None).await;
        assert_eq!(v["rows"].as_array().unwrap().len(), 50);
        assert_eq!(v["nextBeforeId"], 3);
        let (_, v) = call(&s, "GET", "/ui/api/requests?before=3", None).await;
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);
        assert!(v["nextBeforeId"].is_null());
    }
    #[tokio::test]
    async fn charts_preserve_unknown_cost_and_ignore_old_rows() {
        let (_dir, s) = state();
        for at in [now() - 10, now() - 40 * 86400] {
            s.store
                .record_usage(&crate::types::Usage {
                    at,
                    model_id: "test".into(),
                    input_tokens: 7,
                    output_tokens: 3,
                    cost_source: Some("unknown".into()),
                    ..Default::default()
                })
                .unwrap();
        }
        let (_, v) = call(&s, "GET", "/ui/api/charts?range=month", None).await;
        assert_eq!(v["buckets"].as_array().unwrap().len(), 30);
        assert_eq!(v["requests"], 1);
        assert_eq!(v["tokens"], 10);
        assert_eq!(v["unknownCostRequests"], 1);
    }
    #[tokio::test]
    async fn oauth_connect_returns_opaque_handle_without_verifier() {
        let (_dir, s) = state();
        let (status, v) = call(&s, "POST", "/ui/api/accounts/claude/connect", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(v["loginId"].is_string());
        assert!(v["authorizeUrl"].as_str().unwrap().starts_with("https://"));
        assert!(v.get("codeVerifier").is_none());
        let id = v["loginId"].as_str().unwrap();
        assert!(pending().lock().unwrap().remove(id).is_some());
    }
    #[tokio::test]
    async fn secrets_have_no_store_and_csp_headers() {
        let (_dir, s) = state();
        let res = routes()
            .with_state(s)
            .oneshot(
                Request::builder()
                    .uri("/ui/api/gateway-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            res.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
    }
    #[test]
    fn ids_are_bounded_and_path_safe() {
        assert!(valid_id("local:qwen/32b"));
        assert!(!valid_id(""));
        assert!(!valid_id("../x"));
        assert!(!valid_id("<script>"));
        assert!(!valid_id(&"a".repeat(201)));
    }
    #[test]
    fn endpoint_urls_reject_embedded_credentials() {
        assert!(valid_url("http://127.0.0.1:1234/v1").is_ok());
        assert!(valid_url("file:///etc/passwd").is_err());
        assert!(valid_url("https://key@example.com").is_err());
        assert!(valid_url("http://localhost?api_key=secret").is_err());
    }
}

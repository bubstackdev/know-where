use super::oauth;
use crate::{AppState, types::Price};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

async fn body(response: reqwest::Response) -> Result<Value> {
    let response = response.error_for_status()?;
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(part) = chunks.next().await {
        let part = part?;
        if bytes.len() + part.len() > 16 * 1024 * 1024 {
            bail!("Model list exceeds size limit");
        }
        bytes.extend_from_slice(&part);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
async fn fetch(state: &AppState, provider: &str) -> Result<Value> {
    let mut all = Vec::new();
    let mut after = String::new();
    let mut retried = false;
    let mut token = if provider == "openrouter" {
        None
    } else {
        Some(oauth::access(state, provider, false).await?)
    };
    for page in 0..20 {
        let base = match provider {
            "claude" => format!(
                "{}/v1/models",
                state.config.claude_api_base.trim_end_matches('/')
            ),
            "codex" => format!(
                "{}/models",
                state.config.codex_api_base.trim_end_matches('/')
            ),
            _ => format!(
                "{}/models",
                state.config.openrouter_api_base.trim_end_matches('/')
            ),
        };
        let mut req = state.client.get(base).timeout(Duration::from_secs(30));
        if let Some(token) = &token {
            req = req.bearer_auth(&token.access_token);
            if provider == "claude" {
                req = req
                    .header("anthropic-version", "2023-06-01")
                    .header("anthropic-beta", "oauth-2025-04-20")
                    .query(&[("limit", "1000"), ("after_id", after.as_str())]);
            } else {
                req = req
                    .header("originator", "codex_cli_rs")
                    .header("User-Agent", "codex_cli_rs")
                    .header(
                        "ChatGPT-Account-ID",
                        token.account_id.as_deref().unwrap_or(""),
                    )
                    .query(&[("client_version", state.config.codex_client_version.as_str())]);
            }
        } else {
            let key = state
                .store
                .get_secret("openrouter")?
                .filter(|k| !k.trim().is_empty())
                .context("No OpenRouter key configured")?;
            req = req.bearer_auth(key);
        }
        let mut response = req.send().await?;
        if response.status().as_u16() == 401 && !retried && provider == "codex" {
            token = Some(
                oauth::access_rejected(state, provider, &token.as_ref().unwrap().access_token)
                    .await?,
            );
            retried = true;
            let token = token.as_ref().unwrap();
            response = state
                .client
                .get(format!(
                    "{}/models",
                    state.config.codex_api_base.trim_end_matches('/')
                ))
                .timeout(Duration::from_secs(30))
                .bearer_auth(&token.access_token)
                .header(
                    "ChatGPT-Account-ID",
                    token.account_id.as_deref().unwrap_or(""),
                )
                .header("originator", "codex_cli_rs")
                .header("User-Agent", "codex_cli_rs")
                .query(&[("client_version", state.config.codex_client_version.as_str())])
                .send()
                .await?;
        }
        let v = body(response).await?;
        let list = v
            .get(if provider == "codex" {
                "models"
            } else {
                "data"
            })
            .and_then(Value::as_array)
            .context("Malformed model list")?;
        for m in list {
            let id = m
                .get(if provider == "codex" { "slug" } else { "id" })
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .context("Model list contains an invalid entry")?;
            let mut m = m.clone();
            m["id"] = json!(id);
            all.push(m);
        }
        if provider != "claude" || v["has_more"] != true {
            if all.is_empty() {
                bail!("Upstream model list was empty");
            }
            return Ok(json!({"data":all}));
        }
        let cursor = v["last_id"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != after)
            .context("Model list pagination did not advance")?;
        after = cursor.into();
        if page == 19 {
            bail!("Model list exceeded pagination limit");
        }
    }
    unreachable!()
}
pub async fn refresh_selected(state: &AppState, providers: &[&str]) -> Result<Value> {
    let mut result = json!({});
    for &provider in providers {
        if state.store.get_secret(provider)?.is_none() {
            result[provider] = json!({"skipped":true});
            continue;
        }
        let key = format!("models:{provider}");
        let mut cache = state
            .store
            .get_setting(&key)?
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(json!({"modelIds":null,"fetchedAt":null}));
        cache["checkedAt"] = json!(oauth::now());
        match fetch(state, provider).await {
            Ok(v) => {
                let ids = v["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|m| m["id"].as_str())
                    .collect::<Vec<_>>();
                cache["modelIds"] = json!(ids);
                cache["fetchedAt"] = json!(oauth::now());
                cache["lastError"] = Value::Null;
                if provider == "openrouter" {
                    update_openrouter(state, &v).await?;
                }
                result[provider] = v;
            }
            Err(_) => {
                let message = format!(
                    "{provider} model list could not be read; retaining the last successful list"
                );
                cache["lastError"] = json!(message);
                result[provider] = json!({"error":message});
            }
        }
        state.store.set_setting(&key, &cache.to_string())?;
    }
    Ok(result)
}
fn price(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v * 1_000_000.0)
}
async fn update_openrouter(state: &AppState, value: &Value) -> Result<()> {
    let _guard = state.runtime.assignment.lock().await;
    let mut catalog = state.store.catalog()?;

    for model in catalog
        .models
        .iter_mut()
        .filter(|m| m.provider == "openrouter")
    {
        let Some(info) = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == model.upstream_model_id)
        else {
            continue;
        };
        if model.upstream_model_id == "openrouter/auto"
            || model.upstream_model_id == "openrouter/auto-beta"
        {
            continue;
        }
        if let Some(n) = info["context_length"].as_u64().filter(|n| *n > 0) {
            model.context_window = Some(n);
        }
        if let Some(p) = info["supported_parameters"].as_array() {
            model.supports_tools = p.iter().any(|v| v == "tools");
        }
        if let Some(m) = info
            .pointer("/architecture/input_modalities")
            .and_then(Value::as_array)
        {
            model.supports_vision = m.iter().any(|v| v == "image");
        }
        if let Some(levels) = info
            .pointer("/reasoning/supported_efforts")
            .and_then(Value::as_array)
        {
            let levels = levels
                .iter()
                .filter(|v| {
                    v.as_str().is_some_and(|s| {
                        ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&s)
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            if !levels.is_empty() && model.effort.is_none() {
                let default = info
                    .pointer("/reasoning/default_effort")
                    .filter(|v| {
                        levels.contains(v)
                            && info.pointer("/reasoning/default_enabled") != Some(&json!(false))
                    })
                    .cloned();
                model.effort =
                    Some(json!({"mode":"reasoning_effort","levels":levels,"default":default}));
            }
        }
        let p = &info["pricing"];
        let input = price(&p["prompt"]);
        let output = price(&p["completion"]);
        if let (Some(input), Some(output)) = (input, output) {
            let previous = catalog.prices.get(&model.id).cloned().unwrap_or_default();
            catalog.prices.insert(
                model.id.clone(),
                Price {
                    input,
                    output,
                    cache_read: price(&p["input_cache_read"]).unwrap_or(previous.cache_read),
                    cache_write: price(&p["input_cache_write"]).unwrap_or(previous.cache_write),
                    cache_write1h: price(&p["input_cache_write_1h"]).or(previous.cache_write1h),
                },
            );
        } else {
            catalog.prices.remove(&model.id);
        }
    }
    state.store.save_catalog(&catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    fn state(dir: &tempfile::TempDir) -> AppState {
        AppState {
            store: crate::Store::open(&dir.path().join("test.db")).unwrap(),
            config: crate::Config::default(),
            client: reqwest::Client::new(),
            runtime: Arc::default(),
        }
    }
    #[tokio::test]
    async fn claude_pagination_and_failed_refresh_retain_last_good_list() {
        let calls = Arc::new(AtomicUsize::new(0));
        let call_count = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router=axum::Router::new().route("/v1/models",axum::routing::get(move|axum::extract::Query(q):axum::extract::Query<std::collections::HashMap<String,String>>|{let calls=call_count.clone();async move{let n=calls.fetch_add(1,Ordering::SeqCst);if n>1{return (axum::http::StatusCode::BAD_GATEWAY,axum::Json(json!({"error":"bad"})));}let second=q.get("after_id").is_some_and(|v|v=="model-a");(axum::http::StatusCode::OK,axum::Json(if second{json!({"data":[{"id":"model-b"}],"has_more":false})}else{json!({"data":[{"id":"model-a"}],"has_more":true,"last_id":"model-a"})}))}}));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(&dir);
        state.config.claude_api_base = format!("http://{addr}");
        state.store.set_secret("claude",&json!({"access_token":"test","refresh_token":"test","expires_at":oauth::now()+600,"account_id":null}).to_string()).unwrap();
        refresh_selected(&state, &["claude"]).await.unwrap();
        let first: Value =
            serde_json::from_str(&state.store.get_setting("models:claude").unwrap().unwrap())
                .unwrap();
        assert_eq!(first["modelIds"], json!(["model-a", "model-b"]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        refresh_selected(&state, &["claude"]).await.unwrap();
        let second: Value =
            serde_json::from_str(&state.store.get_setting("models:claude").unwrap().unwrap())
                .unwrap();
        assert_eq!(second["modelIds"], first["modelIds"]);
        assert_eq!(second["fetchedAt"], first["fetchedAt"]);
        assert!(second["lastError"].is_string());
        task.abort();
    }
    #[tokio::test]
    async fn openrouter_discovery_updates_configured_capabilities_and_upstream_prices() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let catalog:crate::types::Catalog=serde_json::from_value(json!({"models":[{"id":"mapped","provider":"openrouter","upstreamModelId":"vendor/model","enabled":true}],"tiers":{"high":["mapped"]},"aliases":[],"defaultTier":"high","prices":{}})).unwrap();
        state.store.save_catalog(&catalog).unwrap();
        update_openrouter(&state,&json!({"data":[{"id":"vendor/model","context_length":100000,"supported_parameters":["tools"],"architecture":{"input_modalities":["text","image"]},"pricing":{"prompt":"0.000002","completion":"0.00001","input_cache_read":"0.0000002"},"reasoning":{"supported_efforts":["low","high"],"default_enabled":true,"default_effort":"high"}}]})).await.unwrap();
        let c = state.store.catalog().unwrap();
        assert_eq!(c.models[0].context_window, Some(100000));
        assert!(c.models[0].supports_tools && c.models[0].supports_vision);
        assert_eq!(c.models[0].effort.as_ref().unwrap()["default"], "high");
        assert!(!c.prices.contains_key("vendor/model"));
        assert_eq!(c.prices["mapped"].input, 2.0);
        assert_eq!(c.prices["mapped"].output, 10.0);
    }
}

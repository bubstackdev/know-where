//! Protected deployment policy for Moe. Never supplied by model-visible input.
use crate::{Config, types::Model};
use anyhow::{Result, bail};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub model: String,
    pub upstream_model: String,
    pub provider: String,
    pub account: String,
    pub profile: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub routes: Vec<Route>,
    pub max_in_flight: Option<usize>,
}

impl Policy {
    pub fn validate(&self, config: &Config) -> Result<()> {
        if !config.listen_address.is_loopback()
            || !config.data_dir.is_absolute()
            || config.data_dir == Config::default().data_dir
            || self.routes.is_empty()
            || self.max_in_flight == Some(0)
        {
            bail!(
                "Moe requires loopback, an explicit absolute owned data directory, routes and a positive cap"
            )
        }
        let mut ids = std::collections::HashSet::new();
        for r in &self.routes {
            if !safe_label(&r.model)
                || !safe_label(&r.upstream_model)
                || !safe_label(&r.profile)
                || !ids.insert(&r.model)
                || !matches!(
                    (r.provider.as_str(), r.account.as_str()),
                    ("claude" | "codex", "subscription" | "api_key") | ("local", "local_lease")
                )
            {
                bail!(
                    "Moe routes must uniquely pin claude, codex or local and an approved account/profile"
                )
            }
        }
        Ok(())
    }

    pub fn route(&self, model: &Model) -> Option<&Route> {
        self.routes.iter().find(|r| {
            r.model == model.id
                && r.provider == model.provider
                && r.upstream_model == model.upstream_model_id
        })
    }

    pub fn binding<'a>(
        &'a self,
        config: &'a Config,
        headers: &'a HeaderMap,
    ) -> Option<(&'a str, &'a Route)> {
        let header = |name| headers.get(name).and_then(|h| h.to_str().ok());
        let run = header("x-moe-run-id").or(config.moe_run_id.as_deref())?;
        let model = header("x-moe-model").or(config.moe_model.as_deref())?;
        if run.is_empty()
            || run.len() > 200
            || !run
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        {
            return None;
        }
        // A launcher binding cannot be overridden by a harness request.
        if config.moe_run_id.as_deref().is_some_and(|v| v != run)
            || config.moe_model.as_deref().is_some_and(|v| v != model)
        {
            return None;
        }
        let route = self.routes.iter().find(|r| r.model == model)?;
        if header("x-moe-account").is_some_and(|v| v != route.account)
            || header("x-moe-profile").is_some_and(|v| v != route.profile)
        {
            return None;
        }
        Some((run, route))
    }
}

fn safe_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c))
}

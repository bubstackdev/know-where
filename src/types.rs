use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

fn yes() -> bool {
    true
}
fn high() -> String {
    "high".into()
}
fn all_decisions() -> Value {
    serde_json::json!({"choice":true,"noul":true,"score":true,"box":true,"images":true,"videos":true})
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub upstream_model_id: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_prompt_tokens: Option<u64>,
    #[serde(default)]
    pub max_conversations: Option<u64>,
    #[serde(default = "yes")]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub effort: Option<Value>,
    #[serde(default)]
    pub open_router: Option<Value>,
    #[serde(default = "all_decisions")]
    pub decisions: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alias {
    pub pattern: String,
    pub tier: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Price {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub cache_write1h: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub tiers: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    #[serde(default = "high")]
    pub default_tier: String,
    #[serde(default)]
    pub prices: BTreeMap<String, Price>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub session_key: String,
    pub tier: String,
    pub model_id: String,
    pub client_session_id: Option<String>,
    pub created_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cooldown {
    pub model_id: String,
    pub until: i64,
    pub reason: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Usage {
    pub id: i64,
    pub at: i64,
    pub client_session_id: Option<String>,
    pub model_id: String,
    pub provider: String,
    pub tier: Option<String>,
    pub status: u16,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write1h_tokens: u64,
    pub cost_usd_micros: i64,
    pub latency_ms: u64,
    pub effort: Option<String>,
    pub resolved_model_id: Option<String>,
    pub generation_id: Option<String>,
    pub cost_source: Option<String>,
    pub session_key: Option<String>,
    pub upstream_model_id: Option<String>,
    pub is_streaming: Option<bool>,
    pub outcome: Option<String>,
    pub failure_kind: Option<String>,
    pub failure_detail: Option<String>,
    pub stop_reason: Option<String>,
}
pub const TIERS: [&str; 4] = ["max", "high", "mid", "low"];
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

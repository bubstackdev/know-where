use crate::AppState;
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
static CLAUDE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static CODEX_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PkceLogin {
    pub authorize_url: String,
    pub state: String,
    pub code_verifier: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: u64,
    pub verification_url: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub account_id: Option<String>,
}
pub fn begin_claude(state: &AppState) -> Result<PkceLogin> {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let nonce = uuid::Uuid::new_v4().to_string();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let c = &state.config;
    let mut url = url::Url::parse(&c.claude_authorize_url)?;
    url.query_pairs_mut().extend_pairs([
        ("code", "true"),
        ("client_id", &c.claude_client_id),
        ("response_type", "code"),
        ("redirect_uri", &c.claude_redirect_uri),
        ("scope", &c.claude_scope),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &nonce),
    ]);
    Ok(PkceLogin {
        authorize_url: url.into(),
        state: nonce,
        code_verifier: verifier,
    })
}
pub async fn finish_claude(state: &AppState, login: &PkceLogin, code: &str) -> Result<()> {
    let code = code.trim();
    let (code, returned_state) = code
        .split_once('#')
        .map(|(a, b)| (a, Some(b)))
        .unwrap_or((code, None));
    if returned_state.is_some_and(|s| s != login.state) {
        bail!("OAuth state does not match this login");
    }
    let c = &state.config;
    let body = json!({"grant_type":"authorization_code","code":code,"state":login.state,"client_id":c.claude_client_id,"redirect_uri":c.claude_redirect_uri,"code_verifier":login.code_verifier});
    let response = state
        .client
        .post(&c.claude_token_url)
        .header("User-Agent", "know-where/0.1")
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?;
    save(state, "claude", response.json().await?, None)?;
    Ok(())
}
pub async fn start_codex(state: &AppState) -> Result<DeviceCode> {
    let c = &state.config;
    let body: Value = state
        .client
        .post(format!(
            "{}/api/accounts/deviceauth/usercode",
            c.codex_issuer.trim_end_matches('/')
        ))
        .header("User-Agent", "know-where/0.1")
        .json(&json!({"client_id":c.codex_client_id}))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let id = body["device_auth_id"]
        .as_str()
        .context("Missing device authorization ID")?;
    let code = body["user_code"]
        .as_str()
        .or_else(|| body["usercode"].as_str())
        .context("Missing user code")?;
    let interval = body["interval"]
        .as_u64()
        .or_else(|| body["interval"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(5);
    let interval = if interval == 0 { 5 } else { interval.min(30) };
    Ok(DeviceCode {
        device_auth_id: id.into(),
        user_code: code.into(),
        interval,
        verification_url: format!("{}/codex/device", c.codex_issuer.trim_end_matches('/')),
    })
}
pub async fn poll_codex(state: &AppState, device: &DeviceCode) -> Result<bool> {
    let c = &state.config;
    let response = state
        .client
        .post(format!(
            "{}/api/accounts/deviceauth/token",
            c.codex_issuer.trim_end_matches('/')
        ))
        .json(&json!({"device_auth_id":device.device_auth_id,"user_code":device.user_code}))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;
    if matches!(response.status().as_u16(), 403 | 404) {
        return Ok(false);
    }
    let body: Value = response.error_for_status()?.json().await?;
    let code = body["authorization_code"]
        .as_str()
        .context("Missing authorization code")?;
    let verifier = body["code_verifier"].as_str().context("Missing verifier")?;
    let token: Value = state
        .client
        .post(format!(
            "{}/oauth/token",
            c.codex_issuer.trim_end_matches('/')
        ))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", c.codex_client_id.as_str()),
            ("code", code),
            ("code_verifier", verifier),
            (
                "redirect_uri",
                &format!(
                    "{}/deviceauth/callback",
                    c.codex_issuer.trim_end_matches('/')
                ),
            ),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    save(state, "codex", token, None)?;
    Ok(true)
}
fn claims(jwt: &str) -> Option<Value> {
    let data = jwt.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(data).ok()?).ok()
}
fn save(
    state: &AppState,
    provider: &str,
    value: Value,
    previous: Option<&Tokens>,
) -> Result<Tokens> {
    let access = value["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Token response omitted access token")?;
    let refresh = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| previous.map(|p| p.refresh_token.as_str()))
        .context("Token response omitted refresh token")?;
    let identity = claims(value["id_token"].as_str().unwrap_or(access)).or_else(|| claims(access));
    let account = identity
        .as_ref()
        .and_then(|c| {
            c.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .or_else(|| c.get("chatgpt_account_id"))
                .or_else(|| c.pointer("/organizations/0/id"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous.and_then(|p| p.account_id.clone()));
    if provider == "codex" && account.is_none() {
        bail!("Codex token response omitted account ID");
    }
    let expires = value["expires_in"]
        .as_i64()
        .filter(|v| *v > 0)
        .map(|v| now() + v)
        .or_else(|| claims(access).and_then(|c| c["exp"].as_i64()))
        .unwrap_or(now() + 3600);
    let token = Tokens {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at: expires,
        account_id: account,
    };
    state
        .store
        .set_secret(provider, &serde_json::to_string(&token)?)?;
    Ok(token)
}
pub(crate) async fn access(state: &AppState, provider: &str, force: bool) -> Result<Tokens> {
    access_inner(state, provider, None, force).await
}
pub(crate) async fn access_rejected(
    state: &AppState,
    provider: &str,
    rejected: &str,
) -> Result<Tokens> {
    access_inner(state, provider, Some(rejected), false).await
}
async fn access_inner(
    state: &AppState,
    provider: &str,
    rejected: Option<&str>,
    force: bool,
) -> Result<Tokens> {
    let lock = if provider == "claude" {
        &CLAUDE_LOCK
    } else {
        &CODEX_LOCK
    };
    let guard = lock.lock().await;
    let secret = state
        .store
        .get_secret(provider)?
        .context(format!("Run know-where login {provider} first"))?;
    let token: Tokens =
        serde_json::from_str(&secret).context("Stored credentials are malformed")?;
    if !force && rejected != Some(token.access_token.as_str()) && token.expires_at > now() + 300 {
        return Ok(token);
    }
    let state = state.clone();
    let provider = provider.to_owned();
    // Rotating refresh tokens must be saved even when the requesting client disconnects.
    tokio::spawn(async move {
        let _guard = guard;
        refresh_tokens(&state, &provider, token).await
    })
    .await
    .context("Token refresh task failed")?
}
async fn refresh_tokens(state: &AppState, provider: &str, token: Tokens) -> Result<Tokens> {
    let c = &state.config;
    let response = if provider == "claude" {
        state.client.post(&c.claude_token_url).header("User-Agent","know-where/0.1").json(&json!({"grant_type":"refresh_token","refresh_token":token.refresh_token,"client_id":c.claude_client_id})).timeout(std::time::Duration::from_secs(30)).send().await?
    } else {
        state
            .client
            .post(format!(
                "{}/oauth/token",
                c.codex_issuer.trim_end_matches('/')
            ))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", token.refresh_token.as_str()),
                ("client_id", c.codex_client_id.as_str()),
            ])
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?
    };
    save(
        state,
        provider,
        response.error_for_status()?.json().await?,
        Some(&token),
    )
}
pub(crate) fn refresh_fault(error: &anyhow::Error) -> super::ModelFault {
    let request_error = error.downcast_ref::<reqwest::Error>();
    let kind = if request_error
        .is_none_or(|e| e.is_decode() || e.status().is_some_and(|s| s.as_u16() < 500))
    {
        "auth_rejected"
    } else {
        "unreachable"
    };
    super::ModelFault::new(
        kind,
        if kind == "auth_rejected" {
            "Subscription credentials expired or were revoked; log in again"
        } else {
            "Subscription token could not be refreshed"
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_refresh_persists_rotation_and_old_rejections_reuse_new_token() {
        let sent = Arc::new(tokio::sync::Notify::new());
        let notify = sent.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router=axum::Router::new().route("/token",axum::routing::post(move||{let notify=notify.clone();let count=count.clone();async move{count.fetch_add(1,Ordering::SeqCst);notify.notify_one();tokio::time::sleep(std::time::Duration::from_millis(30)).await;axum::Json(json!({"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600}))}}));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(&dir);
        state.config.claude_token_url = format!("http://{addr}/token");
        state.store.set_secret("claude",&json!({"access_token":"old-access","refresh_token":"old-refresh","expires_at":0,"account_id":null}).to_string()).unwrap();
        let caller_state = state.clone();
        let caller = tokio::spawn(async move { access(&caller_state, "claude", false).await });
        sent.notified().await;
        caller.abort();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let saved: Tokens =
                    serde_json::from_str(&state.store.get_secret("claude").unwrap().unwrap())
                        .unwrap();
                if saved.refresh_token == "rotated-refresh" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        for result in futures_util::future::join_all(
            (0..8).map(|_| access_rejected(&state, "claude", "old-access")),
        )
        .await
        {
            assert_eq!(result.unwrap().access_token, "new-access");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
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
    #[test]
    fn pkce_challenge_matches_verifier_and_state_is_random() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let a = begin_claude(&state).unwrap();
        let b = begin_claude(&state).unwrap();
        let u = url::Url::parse(&a.authorize_url).unwrap();
        let params = u.query_pairs().collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            params["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(a.code_verifier.as_bytes()))
        );
        assert_eq!(params["state"], a.state);
        assert_ne!(a.state, b.state);
        assert_ne!(a.code_verifier, b.code_verifier);
    }
    #[tokio::test]
    async fn concurrent_claude_refresh_rotates_once_and_preserves_omitted_refresh_token() {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new().route(
            "/token",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let count = calls.clone();
                async move {
                    assert_eq!(body["grant_type"], "refresh_token");
                    assert_eq!(body["refresh_token"], "old-refresh");
                    count.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({"access_token":"new-access","expires_in":3600}))
                }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(&dir);
        state.config.claude_token_url = format!("http://{addr}/token");
        state.store.set_secret("claude",&json!({"access_token":"old-access","refresh_token":"old-refresh","expires_at":0,"account_id":null}).to_string()).unwrap();
        let tokens =
            futures_util::future::join_all((0..8).map(|_| access(&state, "claude", false))).await;
        for token in tokens {
            let token = token.unwrap();
            assert_eq!(token.access_token, "new-access");
            assert_eq!(token.refresh_token, "old-refresh");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        task.abort();
    }
    #[tokio::test]
    async fn claude_login_rejects_mismatched_state_before_token_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(&dir);
        let login = begin_claude(&state).unwrap();
        assert!(
            finish_claude(&state, &login, "code#wrong-state")
                .await
                .is_err()
        );
        assert!(state.store.get_secret("claude").unwrap().is_none());
    }
    #[tokio::test]
    async fn codex_device_flow_handles_pending_interval_and_account_claim() {
        let polls = Arc::new(AtomicUsize::new(0));
        let calls = polls.clone();
        let jwt = format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(
                json!({"https://api.openai.com/auth":{"chatgpt_account_id":"account1"}})
                    .to_string()
            )
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router=axum::Router::new().route("/api/accounts/deviceauth/usercode",axum::routing::post(||async{axum::Json(json!({"device_auth_id":"device1","user_code":"USER-CODE","interval":"999"}))})).route("/api/accounts/deviceauth/token",axum::routing::post(move|axum::Json(v):axum::Json<Value>|{let count=calls.clone();async move{assert_eq!(v["device_auth_id"],"device1");if count.fetch_add(1,Ordering::SeqCst)==0{(axum::http::StatusCode::FORBIDDEN,axum::Json(json!({})))}else{(axum::http::StatusCode::OK,axum::Json(json!({"authorization_code":"authorization","code_verifier":"verifier"})))}}})).route("/oauth/token",axum::routing::post(move|axum::Form(form):axum::Form<std::collections::HashMap<String,String>>|{let jwt=jwt.clone();async move{assert_eq!(form["grant_type"],"authorization_code");assert_eq!(form["code_verifier"],"verifier");axum::Json(json!({"access_token":jwt,"refresh_token":"refresh","expires_in":3600}))}}));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(&dir);
        state.config.codex_issuer = format!("http://{addr}");
        let code = start_codex(&state).await.unwrap();
        assert_eq!(code.interval, 30);
        assert!(!poll_codex(&state, &code).await.unwrap());
        assert!(poll_codex(&state, &code).await.unwrap());
        let saved: Tokens =
            serde_json::from_str(&state.store.get_secret("codex").unwrap().unwrap()).unwrap();
        assert_eq!(saved.account_id.as_deref(), Some("account1"));
        task.abort();
    }
}

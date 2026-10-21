use anyhow::{Context, Result, bail};
use std::{net::IpAddr, path::PathBuf, time::Duration};
#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    pub seed_file: Option<PathBuf>,
    pub seed_override_file: Option<PathBuf>,
    pub listen_address: IpAddr,
    pub port: u16,
    pub https_port: u16,
    pub certificate_path: Option<PathBuf>,
    pub certificate_password: Option<String>,
    pub claude_api_base: String,
    pub claude_authorize_url: String,
    pub claude_token_url: String,
    pub claude_client_id: String,
    pub claude_redirect_uri: String,
    pub claude_scope: String,
    pub codex_issuer: String,
    pub codex_client_id: String,
    pub codex_api_base: String,
    pub codex_client_version: String,
    pub openrouter_api_base: String,
    pub stream_start_grace: Duration,
    pub keepalive_interval: Duration,
    pub local_stream_idle: Duration,
    pub codex_stream_idle: Duration,
    pub openrouter_stream_idle: Duration,
    pub model_refresh_interval: Duration,
    pub health_interval: Duration,
    pub conversation_idle: Duration,
    pub device_code_timeout: Duration,
    pub limit_cooldown: Duration,
    pub plan_rejection_cooldown: Duration,
    pub overloaded_cooldown: Duration,
    pub auth_rejection_cooldown: Duration,
    pub systemone_timeout: Duration,
    pub systemone_max_body: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".know-where"),
            seed_file: None,
            seed_override_file: None,
            listen_address: "0.0.0.0".parse().unwrap(),
            port: 8790,
            https_port: 8791,
            certificate_path: None,
            certificate_password: None,
            claude_api_base: "https://api.anthropic.com".into(),
            claude_authorize_url: "https://claude.ai/oauth/authorize".into(),
            claude_token_url: "https://console.anthropic.com/v1/oauth/token".into(),
            claude_client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into(),
            claude_redirect_uri: "https://console.anthropic.com/oauth/code/callback".into(),
            claude_scope: "org:create_api_key user:profile user:inference".into(),
            codex_issuer: "https://auth.openai.com".into(),
            codex_client_id: "app_EMoamEEZ73f0CkXaXp7hrann".into(),
            codex_api_base: "https://chatgpt.com/backend-api/codex".into(),
            codex_client_version: "99.0.0".into(),
            openrouter_api_base: "https://openrouter.ai/api/v1".into(),
            stream_start_grace: Duration::from_secs(15),
            keepalive_interval: Duration::from_secs(15),
            local_stream_idle: Duration::from_secs(120),
            codex_stream_idle: Duration::from_secs(300),
            openrouter_stream_idle: Duration::from_secs(120),
            model_refresh_interval: Duration::from_secs(21600),
            health_interval: Duration::from_secs(300),
            conversation_idle: Duration::from_secs(300),
            device_code_timeout: Duration::from_secs(900),
            limit_cooldown: Duration::from_secs(60),
            plan_rejection_cooldown: Duration::from_secs(21600),
            overloaded_cooldown: Duration::from_secs(30),
            auth_rejection_cooldown: Duration::from_secs(300),
            systemone_timeout: Duration::from_secs(60),
            systemone_max_body: 100 * 1024 * 1024,
        }
    }
}
impl Config {
    pub fn load(args: impl IntoIterator<Item = String>) -> Result<(Self, Vec<String>)> {
        let mut c = Self::default();
        let mut values = Vec::new();
        for prefix in ["SimpleRouter__", "KnowWhere__", "KNOW_WHERE__"] {
            for (k, v) in std::env::vars().filter(|(k, _)| {
                k.get(..prefix.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(prefix))
            }) {
                values.push((k[prefix.len()..].to_owned(), v));
            }
        }
        let mut rest = Vec::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            // The launcher owns everything after its command, including flags
            // belonging to the harness. Global gateway flags precede run/setup.
            if arg == "run" || arg == "setup" {
                rest.push(arg);
                rest.extend(args);
                break;
            }
            if let Some(k) = arg.strip_prefix("--") {
                if k == "help" || k == "version" {
                    rest.push(arg);
                    continue;
                }
                let (key, value) = match k.split_once('=') {
                    Some((k, v)) => (k.to_owned(), v.to_owned()),
                    None => (
                        k.to_owned(),
                        args.next().context(format!("Missing value for --{k}"))?,
                    ),
                };
                values.push((key, value));
            } else {
                rest.push(arg);
            }
        }
        // Merge providers before parsing: a valid command-line override must
        // supersede even an invalid environment value, as .NET configuration does.
        let mut seen = std::collections::HashSet::new();
        for (key, value) in values.into_iter().rev() {
            if seen.insert(normalize_key(&key)) {
                c.set(&key, &value)?;
            }
        }
        Ok((c, rest))
    }
    pub fn set(&mut self, key: &str, v: &str) -> Result<()> {
        let normalized = normalize_key(key);
        match normalized.as_str() {
            "datadirectory" | "datadir" => self.data_dir = v.into(),
            "seedfile" => self.seed_file = Some(v.into()),
            "seedoverridefile" => self.seed_override_file = Some(v.into()),
            "listenaddress" => {
                self.listen_address = v
                    .trim()
                    .parse()
                    .context("Listen address must be a full IP address")?
            }
            "port" => self.port = v.parse()?,
            "httpsport" => self.https_port = v.parse()?,
            "httpscertificatepath" => self.certificate_path = Some(v.into()),
            "httpspassword" => self.certificate_password = Some(v.into()),
            "claudeapibaseurl" => self.claude_api_base = v.into(),
            "claudeauthorizeurl" => self.claude_authorize_url = v.into(),
            "claudetokenurl" => self.claude_token_url = v.into(),
            "claudeclientid" => self.claude_client_id = v.into(),
            "clauderedirecturi" => self.claude_redirect_uri = v.into(),
            "claudescope" => self.claude_scope = v.into(),
            "codexissuer" => self.codex_issuer = v.into(),
            "codexclientid" => self.codex_client_id = v.into(),
            "codexapibaseurl" => self.codex_api_base = v.into(),
            "codexclientversion" => self.codex_client_version = v.into(),
            "openrouterapibaseurl" => self.openrouter_api_base = v.into(),
            "limitsstreamstartgrace" => self.stream_start_grace = duration(v)?,
            "limitskeepaliveinterval" => self.keepalive_interval = duration(v)?,
            "localstreamidletimeout" => self.local_stream_idle = duration(v)?,
            "codexstreamidletimeout" => self.codex_stream_idle = duration(v)?,
            "openrouterstreamidletimeout" => self.openrouter_stream_idle = duration(v)?,
            "modellistrefreshinterval" => self.model_refresh_interval = duration(v)?,
            "localhealthcheckintervalseconds" => {
                self.health_interval = Duration::from_secs(v.parse()?)
            }
            "localconversationidle" => self.conversation_idle = duration(v)?,
            "codexdevicecodetimeout" => self.device_code_timeout = duration(v)?,
            "limitslimitcooldown" => self.limit_cooldown = duration(v)?,
            "limitsplanrejectioncooldown" => self.plan_rejection_cooldown = duration(v)?,
            "limitsoverloadedcooldown" => self.overloaded_cooldown = duration(v)?,
            "limitsauthrejectioncooldown" => self.auth_rejection_cooldown = duration(v)?,
            "systemonetimeoutseconds" => self.systemone_timeout = Duration::from_secs(v.parse()?),
            "systemonemaxbodybytes" => self.systemone_max_body = v.parse()?,
            _ => bail!("Unknown configuration option: {key}"),
        }
        Ok(())
    }
}
fn normalize_key(key: &str) -> String {
    let key = key
        .to_ascii_lowercase()
        .replace("simplerouter:", "")
        .replace("knowwhere:", "")
        .replace([':', '_', '-'], "");
    if key == "datadir" {
        "datadirectory".into()
    } else {
        key
    }
}
fn duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.contains(':') {
        let x = s.split(':').collect::<Vec<_>>();
        if x.len() != 3 {
            bail!("Duration must be HH:MM:SS")
        }
        let (days, hours) = match x[0].split_once('.') {
            Some((days, hours)) => (days.parse::<u64>()?, hours.parse::<u64>()?),
            None => (0, x[0].parse::<u64>()?),
        };
        let minutes = x[1].parse::<u8>()?;
        let seconds = x[2].parse::<f64>()?;
        if minutes >= 60 || !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
            bail!("Duration minutes and seconds must be between zero and sixty")
        }
        let secs = days as f64 * 86400. + hours as f64 * 3600. + minutes as f64 * 60. + seconds;
        Duration::try_from_secs_f64(secs).context("Duration is outside the supported range")
    } else {
        Ok(Duration::from_secs(s.parse()?))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_reference_configuration() {
        let (c, args) = Config::load([
            "--SimpleRouter:Port=0".into(),
            "--SimpleRouter:Limits:StreamStartGrace=00:00:00.1".into(),
            "key".into(),
            "show".into(),
        ])
        .unwrap();
        assert_eq!(c.port, 0);
        assert_eq!(c.stream_start_grace, Duration::from_millis(100));
        assert_eq!(args, ["key", "show"]);
    }
    #[test]
    fn rejects_short_ipv4() {
        let mut c = Config::default();
        assert!(c.set("listen-address", "192.168.1").is_err());
    }
}

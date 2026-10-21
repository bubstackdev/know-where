use crate::{AppState, types::now};
use anyhow::{Context, Result, bail};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa,
    KeyPair, KeyUsagePurpose, NameConstraints,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{net::IpAddr, path::Path, process::Command};

pub fn identity() -> (Vec<String>, Vec<IpAddr>) {
    let full = hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "localhost".into())
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let short = full.split('.').next().unwrap_or("localhost").to_owned();
    let bonjour = if cfg!(target_os = "macos") {
        Command::new("/usr/sbin/scutil")
            .args(["--get", "LocalHostName"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_ascii_lowercase())
    } else {
        None
    }
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| short.clone());
    let mut names = vec![format!("{bonjour}.local"), short, full, "localhost".into()];
    let mut seen = std::collections::HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    let mut ips = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| i.ip())
        .filter(|ip| !ip.is_loopback() && !matches!(ip,IpAddr::V6(ip)if ip.is_unicast_link_local()))
        .collect::<Vec<_>>();
    ips.extend([
        "127.0.0.1".parse::<IpAddr>().unwrap(),
        "::1".parse::<IpAddr>().unwrap(),
    ]);
    ips.sort();
    ips.dedup();
    (names, ips)
}
pub fn create(state: &AppState) -> Result<Value> {
    let (names, ips) = identity();
    let mut sans = names.clone();
    sans.extend(ips.iter().map(ToString::to_string));
    let mut ca = CertificateParams::default();
    ca.distinguished_name
        .push(DnType::CommonName, "know-where CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let start = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    let end = time::OffsetDateTime::now_utc() + time::Duration::days(397);
    ca.not_before = start;
    ca.not_after = end;
    let mut subtrees = vec!["localhost".to_owned(), ".local".to_owned()];
    subtrees.extend(
        names
            .iter()
            .filter(|n| *n != "localhost" && !n.ends_with(".local"))
            .cloned(),
    );
    subtrees.sort();
    subtrees.dedup();
    ca.name_constraints = Some(NameConstraints {
        permitted_subtrees: subtrees.into_iter().map(GeneralSubtree::DnsName).collect(),
        excluded_subtrees: vec![],
    });
    let ca_key = KeyPair::generate()?;
    let ca = ca.self_signed(&ca_key)?;
    let mut leaf = CertificateParams::new(sans.clone())?;
    leaf.distinguished_name
        .push(DnType::CommonName, "know-where");
    leaf.is_ca = IsCa::ExplicitNoCa;
    leaf.not_before = start;
    leaf.not_after = end;
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    leaf.use_authority_key_identifier_extension = true;
    let key = KeyPair::generate()?;
    let leaf = leaf.signed_by(&key, &ca, &ca_key)?;
    // Persist one authenticated encrypted bundle, so certificate renewal cannot pair
    // a previous chain with a newly written key after a crash. The CA key is discarded.
    state.store.set_secret(
        "tls:bundle",
        &json!({"chain":format!("{}{}",leaf.pem(),ca.pem()),"key":key.serialize_pem()}).to_string(),
    )?;
    let dir = state.config.data_dir.join("certs");
    std::fs::create_dir_all(&dir)?;
    private_dir(&dir)?;
    std::fs::write(dir.join("router.crt"), ca.pem())?;
    std::fs::write(dir.join("router-leaf.crt"), leaf.pem())?;
    let fingerprint = hex::encode(Sha256::digest(ca.der()));
    let expires = end.unix_timestamp();
    let metadata = json!({"names":sans,"expiresAt":expires,"fingerprint":fingerprint,"lanUrl":format!("https://{}:{}",names[0],state.config.https_port),"caPath":dir.join("router.crt")});
    state
        .store
        .set_setting("tls:metadata", &metadata.to_string())?;
    Ok(metadata)
}
fn private_dir(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub async fn load(state: &AppState) -> Result<Option<axum_server::tls_rustls::RustlsConfig>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (chain, key) = if let Some(path) = &state.config.certificate_path {
        let bytes = std::fs::read(path)
            .context("Reading SimpleRouter:Https:CertificatePath supplied TLS certificate")?;
        if bytes.starts_with(b"-----BEGIN") {
            (bytes.clone(), bytes)
        } else {
            // Use the platform OpenSSL importer for PKCS#12. Keys remain in memory; the
            // password travels through a child-only environment variable, never argv.
            let out = Command::new("openssl")
                .args(["pkcs12", "-in"])
                .arg(path)
                .args(["-nodes", "-passin", "env:KNOW_WHERE_P12_PASSWORD"])
                .env(
                    "KNOW_WHERE_P12_PASSWORD",
                    state.config.certificate_password.as_deref().unwrap_or(""),
                )
                .output()
                .context("PKCS#12 import requires openssl")?;
            if !out.status.success() {
                bail!(
                    "SimpleRouter:Https:CertificatePath: the supplied PKCS#12 certificate could not be decrypted"
                )
            }
            (out.stdout.clone(), out.stdout)
        }
    } else if let Some(bundle) = state.store.get_secret("tls:bundle")? {
        let v: Value = serde_json::from_str(&bundle)?;
        (
            v["chain"]
                .as_str()
                .context("Missing TLS chain")?
                .as_bytes()
                .to_vec(),
            v["key"]
                .as_str()
                .context("Missing TLS key")?
                .as_bytes()
                .to_vec(),
        )
    } else {
        state.store.set_setting("tls:active_metadata", "null")?;
        return Ok(None);
    };
    let info = certificate_metadata(
        &chain,
        state.config.https_port,
        state.config.certificate_path.is_some(),
    )?;
    {
        if info["expiresAt"]
            .as_i64()
            .is_some_and(|at| at < now() + 30 * 86400)
        {
            tracing::warn!(
                "TLS certificate expires soon or has expired; replace the supplied certificate or run know-where cert create for a generated one"
            )
        }
        let (names, ips) = identity();
        let expected = names
            .into_iter()
            .chain(ips.into_iter().map(|i| i.to_string()));
        if expected.into_iter().any(|n| {
            !info["names"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == &n))
        }) {
            tracing::warn!(
                "TLS certificate does not cover every current host name/address; replace the supplied certificate or regenerate the router certificate"
            )
        }
    }
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(chain, key).await?;
    state
        .store
        .set_setting("tls:active_metadata", &info.to_string())?;
    Ok(Some(config))
}
fn certificate_metadata(chain: &[u8], port: u16, supplied: bool) -> Result<Value> {
    let certificates = x509_parser::pem::Pem::iter_from_buffer(chain)
        .filter_map(|p| p.ok())
        .filter(|p| p.label == "CERTIFICATE")
        .collect::<Vec<_>>();
    let leaf = certificates
        .first()
        .context("TLS chain contains no server certificate")?;
    let (_, cert) = x509_parser::parse_x509_certificate(&leaf.contents)
        .map_err(|_| anyhow::anyhow!("Invalid TLS server certificate"))?;
    let mut names = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::DNSName(name) => {
                    names.push((*name).to_owned())
                }
                x509_parser::extensions::GeneralName::IPAddress(bytes) => {
                    let ip = match bytes.len() {
                        4 => Some(IpAddr::from(<[u8; 4]>::try_from(*bytes).unwrap())),
                        16 => Some(IpAddr::from(<[u8; 16]>::try_from(*bytes).unwrap())),
                        _ => None,
                    };
                    if let Some(ip) = ip {
                        names.push(ip.to_string())
                    }
                }
                _ => {}
            }
        }
    }
    let host = names.first().cloned().unwrap_or_else(|| "localhost".into());
    let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host
    };
    let root = certificates.last().unwrap();
    Ok(
        json!({"names":names,"expiresAt":cert.validity().not_after.timestamp(),"fingerprint":hex::encode(Sha256::digest(&root.contents)),"lanUrl":format!("https://{host}:{port}"),"source":if supplied{"supplied"}else{"generated"}}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn generated_certificate_loads_and_private_key_is_only_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::Config {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        let s = AppState::open(c).unwrap();
        let meta = create(&s).unwrap();
        assert_eq!(meta["fingerprint"].as_str().unwrap().len(), 64);
        assert!(load(&s).await.unwrap().is_some());
        let stored = std::fs::read(dir.path().join("know-where.db")).unwrap();
        assert!(!stored.windows(17).any(|w| w == b"BEGIN PRIVATE KEY"));
        assert!(!dir.path().join("certs/router.key").exists());
    }
}

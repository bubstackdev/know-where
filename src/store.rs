//! Durable routing state and authenticated encryption for credentials.
use crate::types::{Catalog, Cooldown, Pin, Usage};
use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    key: Arc<[u8; 32]>,
}

impl Store {
    /// Opens a SQLite database file. The adjacent key file must travel with backups.
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).context("create data directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let key = load_key(&path.with_extension("key"))?;
        let connection = Connection::open(path).context("open database")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS secrets(key TEXT PRIMARY KEY, value BLOB NOT NULL);
          CREATE TABLE IF NOT EXISTS pins(session_key TEXT NOT NULL,tier TEXT NOT NULL,model_id TEXT NOT NULL,client_session_id TEXT,created_at INTEGER NOT NULL,PRIMARY KEY(session_key,tier));
          CREATE INDEX IF NOT EXISTS pins_client ON pins(client_session_id,tier,created_at);
          CREATE TABLE IF NOT EXISTS cursors(tier TEXT PRIMARY KEY,next INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS cooldowns(model_id TEXT PRIMARY KEY,until_at INTEGER NOT NULL,reason TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS usages(id INTEGER PRIMARY KEY AUTOINCREMENT,client_session_id TEXT,session_key TEXT,body TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS usages_client ON usages(client_session_id,id);
          CREATE INDEX IF NOT EXISTS usages_session ON usages(session_key,id);")?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            key: Arc::new(key),
        })
    }

    fn db(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("database lock poisoned"))
    }

    fn encrypt(&self, name: &str, value: &str) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new(self.key.as_ref().into());
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("encrypt secret"))?;
        Ok([nonce.to_vec(), encrypted].concat())
    }

    fn decrypt(&self, name: &str, value: &[u8]) -> Result<String> {
        if value.len() < 28 {
            bail!("invalid encrypted secret {name}");
        }
        let cipher = ChaCha20Poly1305::new(self.key.as_ref().into());
        let clear = cipher
            .decrypt(
                Nonce::from_slice(&value[..12]),
                Payload {
                    msg: &value[12..],
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| {
                anyhow!("cannot decrypt secret {name}; key missing, changed or data corrupt")
            })?;
        String::from_utf8(clear).context("secret is not UTF-8")
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db()?
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db()?.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value])?;
        Ok(())
    }
    pub fn get_secret(&self, key: &str) -> Result<Option<String>> {
        let data: Option<Vec<u8>> = self
            .db()?
            .query_row("SELECT value FROM secrets WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        data.map(|b| self.decrypt(key, &b)).transpose()
    }
    pub fn set_secret(&self, key: &str, value: &str) -> Result<()> {
        let data = self.encrypt(key, value)?;
        self.db()?.execute("INSERT INTO secrets(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,data])?;
        Ok(())
    }
    /// Persist a generated secret once, without rotating it when separate
    /// requests/processes initialize concurrently. Existing data must decrypt.
    pub fn get_or_create_secret(&self, key: &str, proposed: &str) -> Result<String> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let old: Option<Vec<u8>> = tx
            .query_row("SELECT value FROM secrets WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(value) = old {
            return self.decrypt(key, &value);
        }
        tx.execute(
            "INSERT INTO secrets(key,value) VALUES(?1,?2)",
            params![key, self.encrypt(key, proposed)?],
        )?;
        tx.commit()?;
        Ok(proposed.to_owned())
    }
    pub fn delete_secret(&self, key: &str) -> Result<()> {
        self.db()?
            .execute("DELETE FROM secrets WHERE key=?1", [key])?;
        Ok(())
    }

    pub fn gateway_key(&self) -> Result<String> {
        // Serialize create-if-absent across both cloned handles and independent CLI processes.
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let old: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value FROM secrets WHERE key='gateway_key'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(value) = old {
            return self.decrypt("gateway_key", &value);
        }
        let key = new_gateway_key();
        tx.execute(
            "INSERT INTO secrets(key,value) VALUES('gateway_key',?1)",
            [self.encrypt("gateway_key", &key)?],
        )?;
        tx.commit()?;
        Ok(key)
    }
    pub fn rotate_gateway_key(&self) -> Result<String> {
        let key = new_gateway_key();
        self.set_secret("gateway_key", &key)?;
        Ok(key)
    }

    pub fn catalog(&self) -> Result<Catalog> {
        let data = self
            .get_setting("catalog")?
            .ok_or_else(|| anyhow!("catalog has not been seeded"))?;
        let mut catalog: Catalog = serde_json::from_str(&data).context("read catalog")?;
        // Never return secrets in configuration responses or exports.
        for model in &mut catalog.models {
            model.api_key = None;
        }
        Ok(catalog)
    }
    pub fn save_catalog(&self, catalog: &Catalog) -> Result<()> {
        self.write_catalog(catalog, false, None).map(|_| ())
    }
    /// Commit setup's provider credential and selected catalog together.
    pub fn save_catalog_with_secret(
        &self,
        catalog: &Catalog,
        name: &str,
        secret: &str,
    ) -> Result<()> {
        self.write_catalog(catalog, false, Some((name, secret)))
            .map(|_| ())
    }
    fn write_catalog(
        &self,
        catalog: &Catalog,
        seed_only: bool,
        secret: Option<(&str, &str)>,
    ) -> Result<bool> {
        validate_catalog(catalog)?;
        let mut clean = catalog.clone();
        let mut updates = Vec::new();
        if let Some((name, value)) = secret {
            updates.push((name.to_owned(), Some(self.encrypt(name, value)?)));
        }
        for model in &mut clean.models {
            if let Some(key) = model.api_key.take() {
                let name = format!("model:{}", model.id);
                updates.push((
                    name.clone(),
                    if key.is_empty() {
                        None
                    } else {
                        Some(self.encrypt(&name, &key)?)
                    },
                ));
            }
        }
        let body = serde_json::to_string(&clean)?;
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if seed_only
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM settings WHERE key='catalog')",
                [],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Ok(false);
        }
        tx.execute("INSERT INTO settings(key,value) VALUES('catalog',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&body])?;
        tx.execute("INSERT INTO settings(key,value) VALUES('default_tier',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&catalog.default_tier])?;
        for (name, value) in updates {
            match value {
                Some(data) => {
                    tx.execute("INSERT INTO secrets(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![name,data])?;
                }
                None => {
                    tx.execute("DELETE FROM secrets WHERE key=?1", [name])?;
                }
            }
        }
        tx.commit()?;
        Ok(true)
    }
    /// Seed only an empty catalog; dashboard edits survive restarts.
    pub fn seed(&self, catalog: &Catalog) -> Result<()> {
        if !self.write_catalog(catalog, true, None)? {
            // Price seeding is independent, as in the reference: upgrading an
            // unpriced installation must not replace its edited models.
            let mut current = self.catalog()?;
            if current.prices.is_empty() && !catalog.prices.is_empty() {
                current.prices = catalog.prices.clone();
                self.save_catalog(&current)?;
            }
        }
        Ok(())
    }

    pub fn pins(&self) -> Result<Vec<Pin>> {
        let db = self.db()?;
        // Timestamps use seconds in the public Rust contract; rowid retains
        // first-assignment order when sibling pins arrive within one second.
        let mut s=db.prepare("SELECT session_key,tier,model_id,client_session_id,created_at FROM pins ORDER BY created_at,rowid")?;
        Ok(s.query_map([], read_pin)?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn pin(&self, session_key: &str, tier: &str) -> Result<Option<Pin>> {
        Ok(self.db()?.query_row("SELECT session_key,tier,model_id,client_session_id,created_at FROM pins WHERE session_key=?1 AND tier=?2", params![session_key,tier],read_pin).optional()?)
    }
    pub fn assign_pin(&self, pin: &Pin, advance: bool) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction()?;
        // Re-pinning keeps the original creation time and client identity.
        tx.execute("INSERT INTO pins(session_key,tier,model_id,client_session_id,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session_key,tier) DO UPDATE SET model_id=excluded.model_id", params![pin.session_key,pin.tier,pin.model_id,pin.client_session_id,pin.created_at])?;
        if advance {
            tx.execute("INSERT INTO cursors(tier,next) VALUES(?1,1) ON CONFLICT(tier) DO UPDATE SET next=(next+1) % 9223372036854775807", [&pin.tier])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn cursor(&self, tier: &str) -> Result<u64> {
        let cursor: Option<u64> = self
            .db()?
            .query_row("SELECT next FROM cursors WHERE tier=?1", [tier], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(cursor.unwrap_or(0))
    }
    pub fn delete_pin(&self, session_key: &str, tier: &str) -> Result<()> {
        self.db()?.execute(
            "DELETE FROM pins WHERE session_key=?1 AND tier=?2",
            params![session_key, tier],
        )?;
        Ok(())
    }
    pub fn cooldowns(&self) -> Result<Vec<Cooldown>> {
        let db = self.db()?;
        let mut s =
            db.prepare("SELECT model_id,until_at,reason FROM cooldowns ORDER BY until_at")?;
        Ok(s.query_map([], |r| {
            Ok(Cooldown {
                model_id: r.get(0)?,
                until: r.get(1)?,
                reason: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
    }
    pub fn set_cooldown(&self, cooldown: &Cooldown) -> Result<()> {
        self.db()?.execute("INSERT INTO cooldowns(model_id,until_at,reason) VALUES(?1,?2,?3) ON CONFLICT(model_id) DO UPDATE SET until_at=excluded.until_at,reason=excluded.reason",params![cooldown.model_id,cooldown.until,cooldown.reason])?;
        Ok(())
    }
    pub fn clear_cooldowns(&self, provider: &str) -> Result<()> {
        let catalog = self.catalog()?;
        let mut db = self.db()?;
        let tx = db.transaction()?;
        for m in catalog
            .models
            .iter()
            .filter(|m| m.provider.eq_ignore_ascii_case(provider))
        {
            tx.execute("DELETE FROM cooldowns WHERE model_id=?1", [&m.id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_usage(&self, usage: &Usage) -> Result<i64> {
        let body = serde_json::to_string(usage)?;
        let db = self.db()?;
        db.execute(
            "INSERT INTO usages(client_session_id,session_key,body) VALUES(?1,?2,?3)",
            params![usage.client_session_id, usage.session_key, body],
        )?;
        Ok(db.last_insert_rowid())
    }
    pub fn usages(&self, limit: usize, before: Option<i64>) -> Result<Vec<Usage>> {
        let db = self.db()?;
        let mut s = db.prepare(
            "SELECT id,body FROM usages WHERE (?1 IS NULL OR id<?1) ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = s.query_map(
            params![before, i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (id, body) = row?;
            decode_usage(id, &body)
        })
        .collect()
    }
    pub fn usage(&self, id: i64) -> Result<Option<Usage>> {
        let data: Option<String> = self
            .db()?
            .query_row("SELECT body FROM usages WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        data.map(|body| decode_usage(id, &body)).transpose()
    }
    pub fn session_usages(&self, id: &str) -> Result<Vec<Usage>> {
        let db = self.db()?;
        let mut s =
            db.prepare("SELECT id,body FROM usages WHERE client_session_id=?1 ORDER BY id DESC")?;
        let rows = s.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, body) = row?;
            decode_usage(id, &body)
        })
        .collect()
    }
    pub fn usage_since(&self, at: i64) -> Result<Vec<Usage>> {
        let db = self.db()?;
        let mut s = db.prepare(
            "SELECT id,body FROM usages WHERE json_extract(body,'$.at')>=?1 ORDER BY id DESC",
        )?;
        let rows = s.query_map([at], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, body) = row?;
            decode_usage(id, &body)
        })
        .collect()
    }
    pub fn pin_usages(
        &self,
        session_key: &str,
        tier: &str,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Usage>> {
        let db = self.db()?;
        let mut s=db.prepare("SELECT id,body FROM usages WHERE session_key=?1 AND json_extract(body,'$.tier')=?2 AND (?3 IS NULL OR id<?3) ORDER BY id DESC LIMIT ?4")?;
        let rows = s.query_map(
            params![
                session_key,
                tier,
                before,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (id, body) = row?;
            decode_usage(id, &body)
        })
        .collect()
    }

    /// Rename an editable catalog identity and all references in a single transaction.
    pub fn rename_model(&self, old: &str, new: &str) -> Result<()> {
        if new.trim().is_empty() {
            bail!("model ID must not be empty");
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let data: String =
            tx.query_row("SELECT value FROM settings WHERE key='catalog'", [], |r| {
                r.get(0)
            })?;
        let mut catalog: Catalog = serde_json::from_str(&data)?;
        if old == new {
            return Ok(());
        }
        if catalog.models.iter().any(|m| m.id == new) {
            bail!("model {new} already exists");
        }
        let model = catalog
            .models
            .iter_mut()
            .find(|m| m.id == old)
            .ok_or_else(|| anyhow!("unknown model {old}"))?;
        model.id = new.into();
        for members in catalog.tiers.values_mut() {
            for member in members {
                if member == old {
                    *member = new.into();
                }
            }
        }
        if let Some(price) = catalog.prices.remove(old) {
            catalog.prices.insert(new.into(), price);
        }
        let old_key = format!("model:{old}");
        let new_key = format!("model:{new}");
        let secret: Option<Vec<u8>> = tx
            .query_row("SELECT value FROM secrets WHERE key=?1", [&old_key], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(secret) = secret {
            let encrypted = self.encrypt(&new_key, &self.decrypt(&old_key, &secret)?)?;
            tx.execute("INSERT INTO secrets(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![new_key,encrypted])?;
            tx.execute("DELETE FROM secrets WHERE key=?1", [old_key])?;
        }
        tx.execute(
            "UPDATE pins SET model_id=?1 WHERE model_id=?2",
            params![new, old],
        )?;
        tx.execute(
            "UPDATE cooldowns SET model_id=?1 WHERE model_id=?2",
            params![new, old],
        )?;
        tx.execute("UPDATE usages SET body=json_set(body,'$.modelId',?1) WHERE json_extract(body,'$.modelId')=?2",params![new,old])?;
        tx.execute(
            "UPDATE settings SET value=?1 WHERE key='catalog'",
            [serde_json::to_string(&catalog)?],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn read_pin(r: &rusqlite::Row<'_>) -> rusqlite::Result<Pin> {
    Ok(Pin {
        session_key: r.get(0)?,
        tier: r.get(1)?,
        model_id: r.get(2)?,
        client_session_id: r.get(3)?,
        created_at: r.get(4)?,
    })
}
fn decode_usage(id: i64, body: &str) -> Result<Usage> {
    let mut usage: Usage = serde_json::from_str(body)?;
    usage.id = id;
    Ok(usage)
}
fn new_gateway_key() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!(
        "kw_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn load_key(path: &Path) -> Result<[u8; 32]> {
    if !path.exists() {
        // Publish only a fully written key, so another server/CLI cannot read
        // a partially written create_new file during simultaneous startup.
        let mut suffix = [0u8; 16];
        OsRng.fill_bytes(&mut suffix);
        let temp = path.with_extension(format!("key-{}", hex::encode(suffix)));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> Result<()> {
            let mut f = options.open(&temp)?;
            let mut key = [0u8; 32];
            OsRng.fill_bytes(&mut key);
            f.write_all(&key)?;
            f.sync_all()?;
            match fs::hard_link(&temp, path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e).context("publish encryption key"),
            }
        })();
        let _ = fs::remove_file(&temp);
        result?;
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        bail!("encryption key must be a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?.read_to_end(&mut bytes)?;
    bytes.try_into().map_err(|_| {
        anyhow!(
            "encryption key must be exactly 32 bytes: {}",
            path.display()
        )
    })
}

fn validate_catalog(catalog: &Catalog) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for model in &catalog.models {
        if model.id.trim().is_empty() || !ids.insert(model.id.as_str()) {
            bail!("model IDs must be nonempty and unique: {}", model.id);
        }
        if model.max_conversations == Some(0) {
            bail!("model {}: maxConversations must be positive", model.id);
        }
    }
    for (tier, members) in &catalog.tiers {
        let mut seen = std::collections::HashSet::new();
        for member in members {
            if !ids.contains(member.as_str()) {
                bail!("tier {tier} refers to unknown model {member}");
            }
            if !seen.insert(member) {
                bail!("tier {tier} contains duplicate model {member}");
            }
        }
    }
    for (id, price) in &catalog.prices {
        for v in [
            price.input,
            price.output,
            price.cache_read,
            price.cache_write,
            price.cache_write1h.unwrap_or(price.cache_write),
        ] {
            if !v.is_finite() || v < 0.0 {
                bail!("model {id}: prices must be finite and nonnegative");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> Catalog {
        serde_json::from_value(json!({"defaultTier":"high","models":[{"id":"local","provider":"local","upstreamModelId":"test","enabled":true,"apiKey":"never-plaintext"}],"tiers":{"high":["local"]},"aliases":[],"prices":{"local":{"input":1.5}}})).unwrap()
    }
    fn pin(model: &str) -> Pin {
        Pin {
            session_key: "key".into(),
            tier: "high".into(),
            model_id: model.into(),
            client_session_id: Some("client".into()),
            created_at: 123,
        }
    }

    #[test]
    fn create_secret_is_atomic_across_independent_database_handles() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("router.db");
        let main = Store::open(&path).unwrap();
        let handles: Vec<_> = (0..8).map(|_| Store::open(&path).unwrap()).collect();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let jobs: Vec<_> = handles
            .into_iter()
            .enumerate()
            .map(|(index, store)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .get_or_create_secret(
                            "responses:reasoning-key",
                            &format!("candidate-{index}"),
                        )
                        .unwrap()
                })
            })
            .collect();
        let values: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
        assert!(values.iter().all(|v| v == &values[0]));
        assert_eq!(
            main.get_secret("responses:reasoning-key")
                .unwrap()
                .as_deref(),
            Some(values[0].as_str())
        );
        assert_eq!(
            main.get_or_create_secret("responses:reasoning-key", "do-not-rotate")
                .unwrap(),
            values[0]
        );
    }

    #[test]
    fn secrets_survive_reopen_but_catalog_exports_are_redacted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("router.db");
        {
            let store = Store::open(&path).unwrap();
            store.seed(&fixture()).unwrap();
            store.set_secret("claude", "oauth-secret").unwrap();
            assert!(store.catalog().unwrap().models[0].api_key.is_none());
            let db = store.db().unwrap();
            let body: String = db
                .query_row("SELECT value FROM settings WHERE key='catalog'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(!body.contains("never-plaintext"));
            let ciphertext: Vec<u8> = db
                .query_row("SELECT value FROM secrets WHERE key='claude'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(!ciphertext.windows(12).any(|w| w == b"oauth-secret"));
        }
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.get_secret("model:local").unwrap().as_deref(),
            Some("never-plaintext")
        );
        assert_eq!(
            reopened.get_secret("claude").unwrap().as_deref(),
            Some("oauth-secret")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path.with_extension("key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn authenticated_ciphertext_cannot_move_between_names_or_be_modified() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("router.db")).unwrap();
        store.set_secret("claude", "secret").unwrap();
        store
            .db()
            .unwrap()
            .execute(
                "INSERT INTO secrets SELECT 'codex',value FROM secrets WHERE key='claude'",
                [],
            )
            .unwrap();
        assert!(store.get_secret("codex").is_err());
        store
            .db()
            .unwrap()
            .execute(
                "UPDATE secrets SET value=zeroblob(40) WHERE key='claude'",
                [],
            )
            .unwrap();
        assert!(store.get_secret("claude").is_err());
    }

    #[test]
    fn saving_redacted_catalog_retains_credentials_and_empty_key_clears_them() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("router.db")).unwrap();
        store.seed(&fixture()).unwrap();
        let mut catalog = store.catalog().unwrap();
        catalog.models[0].enabled = false;
        store.save_catalog(&catalog).unwrap();
        store.seed(&fixture()).unwrap();
        assert!(!store.catalog().unwrap().models[0].enabled);
        assert!(store.get_secret("model:local").unwrap().is_some());
        catalog.models[0].api_key = Some(String::new());
        store.save_catalog(&catalog).unwrap();
        assert!(store.get_secret("model:local").unwrap().is_none());
    }

    #[test]
    fn invalid_catalog_does_not_replace_persisted_configuration_or_secret() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("router.db")).unwrap();
        store.seed(&fixture()).unwrap();
        let mut catalog = store.catalog().unwrap();
        catalog.models[0].api_key = Some("new-key".into());
        catalog
            .tiers
            .get_mut("high")
            .unwrap()
            .push("missing".into());
        assert!(store.save_catalog(&catalog).is_err());
        assert_eq!(store.catalog().unwrap().tiers["high"], vec!["local"]);
        assert_eq!(
            store.get_secret("model:local").unwrap().as_deref(),
            Some("never-plaintext")
        );
    }

    #[test]
    fn gateway_key_is_durable_and_rotation_is_visible_to_other_handles() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("router.db");
        let one = Store::open(&path).unwrap();
        let two = Store::open(&path).unwrap();
        let first = one.gateway_key().unwrap();
        assert!(first.starts_with("kw_"));
        assert_eq!(two.gateway_key().unwrap(), first);
        let second = two.rotate_gateway_key().unwrap();
        assert_ne!(first, second);
        assert_eq!(one.gateway_key().unwrap(), second);
    }

    #[test]
    fn pin_and_cursor_persist_together_and_repin_keeps_creation_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("router.db");
        let store = Store::open(&path).unwrap();
        store.assign_pin(&pin("first"), true).unwrap();
        let mut changed = pin("second");
        changed.created_at = 999;
        changed.client_session_id = None;
        store.assign_pin(&changed, false).unwrap();
        let second = Store::open(&path).unwrap();
        assert_eq!(second.cursor("high").unwrap(), 1);
        let saved = second.pin("key", "high").unwrap().unwrap();
        assert_eq!(saved.model_id, "second");
        assert_eq!(saved.created_at, 123);
        assert_eq!(saved.client_session_id.as_deref(), Some("client"));
        second.delete_pin("key", "high").unwrap();
        assert!(store.pin("key", "high").unwrap().is_none());
    }

    #[test]
    fn usage_pagination_and_session_history_are_complete() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("router.db")).unwrap();
        for at in 100..104 {
            store
                .record_usage(&Usage {
                    at,
                    client_session_id: Some(if at % 2 == 0 { "a" } else { "b" }.into()),
                    model_id: "local".into(),
                    ..Default::default()
                })
                .unwrap();
        }
        let first = store.usages(2, None).unwrap();
        assert_eq!(first.iter().map(|u| u.id).collect::<Vec<_>>(), vec![4, 3]);
        assert_eq!(
            store
                .usages(2, Some(3))
                .unwrap()
                .iter()
                .map(|u| u.id)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(store.session_usages("a").unwrap().len(), 2);
        assert_eq!(store.usage_since(102).unwrap().len(), 2);
        assert_eq!(store.usage(2).unwrap().unwrap().at, 101);
        assert!(store.usage(99).unwrap().is_none());
    }

    #[test]
    fn rename_preserves_catalog_secret_and_related_state_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("router.db")).unwrap();
        store.seed(&fixture()).unwrap();
        store.assign_pin(&pin("local"), true).unwrap();
        store
            .set_cooldown(&Cooldown {
                model_id: "local".into(),
                until: 500,
                reason: "rate_limit".into(),
            })
            .unwrap();
        let id = store
            .record_usage(&Usage {
                model_id: "local".into(),
                ..Default::default()
            })
            .unwrap();
        store.rename_model("local", "renamed").unwrap();
        let catalog = store.catalog().unwrap();
        assert_eq!(catalog.models[0].id, "renamed");
        assert_eq!(catalog.tiers["high"], vec!["renamed"]);
        assert!(catalog.prices.contains_key("renamed"));
        assert!(store.get_secret("model:local").unwrap().is_none());
        assert_eq!(
            store.get_secret("model:renamed").unwrap().as_deref(),
            Some("never-plaintext")
        );
        assert_eq!(store.pins().unwrap()[0].model_id, "renamed");
        assert_eq!(store.cooldowns().unwrap()[0].model_id, "renamed");
        assert_eq!(store.usage(id).unwrap().unwrap().model_id, "renamed");
        store.clear_cooldowns("LOCAL").unwrap();
        assert!(store.cooldowns().unwrap().is_empty());
    }

    #[test]
    fn parallel_initialization_uses_one_key_and_one_initial_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("router.db");
        // Exercise publication independently of SQLite's initial WAL setup.
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                let key_path = path.with_extension("key");
                std::thread::spawn(move || {
                    barrier.wait();
                    load_key(&key_path).unwrap()
                })
            })
            .collect();
        let keys: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
        assert!(keys.iter().all(|key| key == &keys[0]));
        let first = Store::open(&path).unwrap();
        let second = Store::open(&path).unwrap();
        let first_catalog = fixture();
        let mut second_catalog = fixture();
        second_catalog.models[0].enabled = false;
        let a = std::thread::spawn(move || first.seed(&first_catalog).unwrap());
        let b = std::thread::spawn(move || second.seed(&second_catalog).unwrap());
        a.join().unwrap();
        b.join().unwrap();
        let store = Store::open(&path).unwrap();
        let before = store.catalog().unwrap().models[0].enabled;
        let mut different = fixture();
        different.models[0].enabled = !before;
        store.seed(&different).unwrap();
        assert_eq!(store.catalog().unwrap().models[0].enabled, before);
    }

    #[test]
    fn price_backfill_preserves_existing_model_settings_and_pin_history_is_tier_specific() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("router.db")).unwrap();
        let mut original = fixture();
        original.prices.clear();
        original.models[0].enabled = false;
        store.seed(&original).unwrap();
        store.seed(&fixture()).unwrap();
        assert!(!store.catalog().unwrap().models[0].enabled);
        assert_eq!(store.catalog().unwrap().prices["local"].input, 1.5);
        for tier in ["high", "mid", "high"] {
            store
                .record_usage(&Usage {
                    session_key: Some("key".into()),
                    client_session_id: Some("client".into()),
                    tier: Some(tier.into()),
                    ..Default::default()
                })
                .unwrap();
        }
        let latest = store.pin_usages("key", "high", None, 1).unwrap();
        assert_eq!(latest[0].id, 3);
        let older = store.pin_usages("key", "high", Some(3), 10).unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].id, 1);
        assert!(
            store
                .pin_usages("other", "high", None, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reopening_tightens_existing_directory_database_and_key_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("data");
        let path = directory.join("router.db");
        drop(Store::open(&path).unwrap());
        for (file, mode) in [
            (&directory, 0o755),
            (&path, 0o644),
            (&path.with_extension("key"), 0o644),
        ] {
            fs::set_permissions(file, fs::Permissions::from_mode(mode)).unwrap();
        }
        let store = Store::open(&path).unwrap();
        store.gateway_key().unwrap();
        for (file, expected) in [
            (&directory, 0o700),
            (&path, 0o600),
            (&path.with_extension("key"), 0o600),
        ] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                expected
            );
        }
    }

    #[test]
    fn pin_timestamp_ties_keep_assignment_order_and_new_cooldowns_replace_old_deadlines() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("router.db")).unwrap();
        let mut first = pin("first");
        first.session_key = "z".into();
        store.assign_pin(&first, true).unwrap();
        let mut second = pin("second");
        second.session_key = "a".into();
        store.assign_pin(&second, true).unwrap();
        assert_eq!(
            store
                .pins()
                .unwrap()
                .iter()
                .map(|p| p.session_key.as_str())
                .collect::<Vec<_>>(),
            vec!["z", "a"]
        );
        for (until, reason) in [(999, "limited"), (500, "overloaded")] {
            store
                .set_cooldown(&Cooldown {
                    model_id: "m".into(),
                    until,
                    reason: reason.into(),
                })
                .unwrap();
        }
        let cooldown = store.cooldowns().unwrap().pop().unwrap();
        assert_eq!(cooldown.until, 500);
        assert_eq!(cooldown.reason, "overloaded");
    }
}

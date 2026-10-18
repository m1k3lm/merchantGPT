//! Encrypted SQLite (SQLCipher) persistence for shops, credentials, LLM
//! settings, conversations, memory notes, scheduled tasks and the audit log.
//!
//! The API is synchronous: every query is a small local operation, so async
//! callers use it directly. Types are primitive (strings, JSON) so this crate
//! stays independent of connectors and providers.

mod key;
mod repo;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;

pub use key::{DbKey, InMemoryKeyStore, KeyStore, KeyringKeyStore};
pub use repo::*;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_llm_options.sql"),
    include_str!("../migrations/0003_app_settings.sql"),
    include_str!("../migrations/0004_task_permissions.sql"),
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("key error: {0}")]
    Key(String),
    #[error("the database exists but its key is missing from the OS keyring; refusing to create a new key over existing data")]
    KeyLost,
    #[error("could not open the database: wrong key or not a merchantGPT database")]
    WrongKey,
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Opens (or creates) the database at `path`, taking the key from
    /// `keys`. A new key is only generated when no database exists yet, so a
    /// lost keyring entry never silently orphans existing data.
    pub fn open_with_keystore(path: &Path, keys: &dyn KeyStore) -> Result<Self> {
        let has_data = std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        let key = match keys.load()? {
            Some(k) => k,
            None if has_data => return Err(StoreError::KeyLost),
            None => {
                let k = DbKey::generate()?;
                keys.save(&k)?;
                k
            }
        };
        Self::open(path, &key)
    }

    pub fn open(path: &Path, key: &DbKey) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        }
        Self::init(Connection::open(path)?, key)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, &DbKey::generate()?)
    }

    fn init(conn: Connection, key: &DbKey) -> Result<Self> {
        // Raw-key syntax skips SQLCipher's KDF; the key is already 256 random bits.
        conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", key.to_hex()))?;
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|_| StoreError::WrongKey)?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn();
        let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<usize> {
        let v: i64 = self
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(v as usize)
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_opened_with_wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let store = Store::open(&path, &DbKey::generate().unwrap()).unwrap();
        store.upsert_shop("https://a.test", None).unwrap();
        drop(store);
        let err = Store::open(&path, &DbKey::generate().unwrap())
            .err()
            .unwrap();
        assert!(matches!(err, StoreError::WrongKey), "{err}");
    }

    #[test]
    fn migrations_apply_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let key = DbKey::generate().unwrap();
        let s = Store::open(&path, &key).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len());
        s.upsert_shop("https://a.test", None).unwrap();
        drop(s);
        let s = Store::open(&path, &key).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len());
        assert_eq!(s.list_shops().unwrap().len(), 1);
    }

    #[test]
    fn data_on_disk_is_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let s = Store::open(&path, &DbKey::generate().unwrap()).unwrap();
        s.save_llm_settings(&LlmSettingsRow {
            options: Default::default(),
            provider: "openai".into(),
            api_key: "sk-very-secret-value".into(),
            base_url: None,
            model: "m".into(),
            active: true,
        })
        .unwrap();
        s.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(s);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"));
        let needle = b"sk-very-secret-value";
        assert!(!bytes.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn keystore_generates_key_once_and_refuses_lost_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let keys = InMemoryKeyStore::default();
        Store::open_with_keystore(&path, &keys)
            .unwrap()
            .upsert_shop("https://a.test", None)
            .unwrap();
        assert!(keys.load().unwrap().is_some());
        assert_eq!(
            Store::open_with_keystore(&path, &keys)
                .unwrap()
                .list_shops()
                .unwrap()
                .len(),
            1
        );
        let empty = InMemoryKeyStore::default();
        assert!(matches!(
            Store::open_with_keystore(&path, &empty),
            Err(StoreError::KeyLost)
        ));
        assert!(empty.load().unwrap().is_none(), "no key written on refusal");
    }
}

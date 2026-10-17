//! The SQLCipher key: 32 random bytes kept in the OS keyring, never on disk.

use std::sync::Mutex;

use crate::StoreError;

#[derive(Clone, PartialEq, Eq)]
pub struct DbKey([u8; 32]);

impl DbKey {
    pub fn generate() -> Result<Self, StoreError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| StoreError::Key(e.to_string()))?;
        Ok(Self(bytes))
    }

    pub fn from_hex(hex: &str) -> Result<Self, StoreError> {
        let hex = hex.trim();
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::Key("stored key is malformed".into()));
        }
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|e| StoreError::Key(e.to_string()))?;
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for DbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbKey(<redacted>)")
    }
}

pub trait KeyStore: Send + Sync {
    fn load(&self) -> Result<Option<DbKey>, StoreError>;
    fn save(&self, key: &DbKey) -> Result<(), StoreError>;
}

/// macOS Keychain, Windows Credential Manager or the Linux Secret Service.
/// Not reachable from the dev container; verified manually on a host.
pub struct KeyringKeyStore {
    service: String,
    account: String,
}

impl KeyringKeyStore {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| StoreError::Key(format!("OS keyring unavailable: {e}")))
    }
}

impl Default for KeyringKeyStore {
    fn default() -> Self {
        Self::new("merchantgpt", "db-key")
    }
}

impl KeyStore for KeyringKeyStore {
    fn load(&self) -> Result<Option<DbKey>, StoreError> {
        match self.entry()?.get_password() {
            Ok(hex) => DbKey::from_hex(&hex).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Key(format!("could not read OS keyring: {e}"))),
        }
    }

    fn save(&self, key: &DbKey) -> Result<(), StoreError> {
        self.entry()?
            .set_password(&key.to_hex())
            .map_err(|e| StoreError::Key(format!("could not write OS keyring: {e}")))
    }
}

#[derive(Default)]
pub struct InMemoryKeyStore(Mutex<Option<DbKey>>);

impl KeyStore for InMemoryKeyStore {
    fn load(&self) -> Result<Option<DbKey>, StoreError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn save(&self, key: &DbKey) -> Result<(), StoreError> {
        *self.0.lock().unwrap() = Some(key.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_debug_is_redacted() {
        let k = DbKey::generate().unwrap();
        assert_eq!(DbKey::from_hex(&k.to_hex()).unwrap(), k);
        assert_eq!(format!("{k:?}"), "DbKey(<redacted>)");
        assert!(DbKey::from_hex("zz").is_err());
    }
}

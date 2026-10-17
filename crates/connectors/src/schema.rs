use std::collections::BTreeMap;

/// Credentials as entered by the user, keyed by `CredentialField::key`.
pub type Credentials = BTreeMap<String, String>;

/// A single input the credential form must collect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialField {
    pub key: &'static str,
    pub label: &'static str,
    pub secret: bool,
}

impl CredentialField {
    pub fn text(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            secret: false,
        }
    }

    pub fn secret(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            secret: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSchema {
    fields: Vec<CredentialField>,
}

impl CredentialSchema {
    pub fn new(fields: Vec<CredentialField>) -> Self {
        Self { fields }
    }

    pub fn fields(&self) -> &[CredentialField] {
        &self.fields
    }

    /// Labels of required fields that are absent or blank.
    pub fn missing(&self, creds: &Credentials) -> Vec<&'static str> {
        self.fields
            .iter()
            .filter(|f| creds.get(f.key).is_none_or(|v| v.trim().is_empty()))
            .map(|f| f.label)
            .collect()
    }
}

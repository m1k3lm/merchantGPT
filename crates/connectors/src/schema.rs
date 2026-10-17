use std::collections::BTreeMap;

/// Credentials as entered by the user, keyed by `CredentialField::key`.
pub type Credentials = BTreeMap<String, String>;

/// A single input the credential form must collect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialField {
    pub key: &'static str,
    pub label: &'static str,
    pub secret: bool,
    pub required: bool,
}

impl CredentialField {
    pub fn text(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            secret: false,
            required: true,
        }
    }

    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    pub fn secret(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            secret: true,
            required: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSchema {
    fields: Vec<CredentialField>,
    help: Option<&'static str>,
}

impl CredentialSchema {
    pub fn new(fields: Vec<CredentialField>) -> Self {
        Self { fields, help: None }
    }

    /// Where the merchant finds these credentials, shown under the form.
    pub fn with_help(mut self, help: &'static str) -> Self {
        self.help = Some(help);
        self
    }

    pub fn help(&self) -> Option<&'static str> {
        self.help
    }

    pub fn fields(&self) -> &[CredentialField] {
        &self.fields
    }

    /// Labels of required fields that are absent or blank.
    pub fn missing(&self, creds: &Credentials) -> Vec<&'static str> {
        self.fields
            .iter()
            .filter(|f| f.required && creds.get(f.key).is_none_or(|v| v.trim().is_empty()))
            .map(|f| f.label)
            .collect()
    }
}

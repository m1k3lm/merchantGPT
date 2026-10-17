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
}

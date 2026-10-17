//! Scrubs known secrets (API keys, shop secrets) from text before it reaches
//! the model, the audit log or the UI. Providers and shops sometimes echo
//! credentials back in error bodies.

use std::sync::Arc;

#[derive(Clone, Default, Debug)]
pub struct Redactor(Arc<Vec<String>>);

/// Shorter values are too likely to match ordinary text.
const MIN_SECRET_LEN: usize = 6;
pub const MASK: &str = "[redacted]";

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut v: Vec<String> = secrets
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() >= MIN_SECRET_LEN)
            .collect();
        // Longest first so a secret containing another is masked whole.
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        v.dedup();
        Self(Arc::new(v))
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in self.0.iter() {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), MASK);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secrets_longest_first_and_ignores_short_values() {
        let r = Redactor::new([
            "sk-abc123".to_string(),
            "sk-abc123-long".into(),
            "ck".into(),
        ]);
        assert_eq!(
            r.apply("bad key sk-abc123-long and sk-abc123, ck stays"),
            "bad key [redacted] and [redacted], ck stays"
        );
    }
}

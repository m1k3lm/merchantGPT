//! Leaf crate: tool definitions, calls and results shared by connectors (producers)
//! and the LLM layer (consumer). Must not depend on any other workspace crate.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use tokio_util::sync::CancellationToken;

/// Whether running a tool can change state outside the app. Mutating tools are
/// always confirmed by the user and never run unattended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mutability {
    Read,
    Mutate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema (type "object") describing the arguments.
    pub parameters: Value,
    pub mutability: Mutability,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: content.into(),
            is_error: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub shop_id: Option<String>,
    pub unattended: bool,
    pub cancel: CancellationToken,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("{0}")]
    Failed(String),
    #[error("cancelled")]
    Cancelled,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<String, ToolError>;
}

/// The common subset accepted by OpenAI, Anthropic, Google and DeepSeek.
pub fn is_valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Deserialises tool arguments into a typed struct, mapping failures to
/// `InvalidArguments` so the model sees a useful error and can retry.
pub fn parse_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool, replacing any previous tool with the same name.
    ///
    /// Panics on an invalid name: tool names are fixed in code, so this is a
    /// programming error, not a runtime condition.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name;
        assert!(is_valid_tool_name(&name), "invalid tool name: {name:?}");
        self.tools.insert(name, tool);
    }

    pub fn extend(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) {
        for t in tools {
            self.register(t);
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.tools.keys()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".into(),
                description: "Echo".into(),
                parameters: json!({"type": "object"}),
                mutability: Mutability::Read,
            }
        }
        async fn call(&self, args: Value, _ctx: &ToolContext) -> Result<String, ToolError> {
            Ok(args.to_string())
        }
    }

    #[test]
    fn tool_name_validation_matches_provider_rules() {
        assert!(is_valid_tool_name("list_products"));
        assert!(is_valid_tool_name("a-b_C9"));
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name("has space"));
        assert!(!is_valid_tool_name("dot.name"));
        assert!(!is_valid_tool_name(&"x".repeat(65)));
    }

    #[tokio::test]
    async fn registry_looks_up_and_calls_tools() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Echo));
        assert_eq!(reg.definitions().len(), 1);
        let out = reg
            .get("echo")
            .unwrap()
            .call(json!({"a": 1}), &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out, r#"{"a":1}"#);
    }

    #[test]
    fn parse_args_reports_invalid_arguments() {
        #[derive(Deserialize, Debug)]
        struct A {
            id: String,
        }
        assert_eq!(parse_args::<A>(json!({"id": "7"})).unwrap().id, "7");
        let err = parse_args::<A>(json!({})).unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}

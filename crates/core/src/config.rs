//! `config.toml` in the platform config dir. Holds defaults only; API keys
//! are rejected here and live in the encrypted database instead.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::AgentConfig;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub llm: LlmDefaults,
    pub agent: AgentSection,
    pub skills: SkillsSection,
    pub data: DataSection,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmDefaults {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSection {
    pub max_iterations: u32,
    pub max_tokens: u32,
    pub max_tool_result_chars: usize,
    pub turn_timeout_secs: u64,
}

impl Default for AgentSection {
    fn default() -> Self {
        let d = AgentConfig::new("");
        Self {
            max_iterations: d.max_iterations,
            max_tokens: d.max_tokens,
            max_tool_result_chars: d.max_tool_result_chars,
            turn_timeout_secs: d.turn_timeout.as_secs(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsSection {
    /// Extra skill roots, searched after the default one in the config dir.
    pub dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataSection {
    pub db_path: Option<PathBuf>,
}

pub const TEMPLATE: &str = r#"# merchantGPT configuration. API keys are NOT read from this file: enter them
# in Settings inside the app; they are stored in the encrypted database.

[llm]
# Default provider and model pre-filled in Settings: openai, anthropic, google, deepseek
# provider = "anthropic"
# model = "claude-sonnet-5-5"
# base_url = ""

[agent]
max_iterations = 10
max_tokens = 4096
max_tool_result_chars = 20000
turn_timeout_secs = 300

[skills]
# Additional skill directories (each skill is a folder with a SKILL.md)
dirs = []

[data]
# db_path = "/path/to/merchantgpt.db"
"#;

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "seQura", "merchantGPT")
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        project_dirs().map(|d| d.config_dir().join("config.toml"))
    }

    /// Loads the file, or the defaults when it does not exist.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("api_key") {
                "API keys are not allowed in config.toml; enter them in Settings".to_string()
            } else {
                msg
            }
        })
    }

    /// Writes the commented template on first run so merchants can find it.
    pub fn write_template_if_missing(path: &Path) -> std::io::Result<()> {
        if path.exists() {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, TEMPLATE)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data.db_path.clone().unwrap_or_else(|| {
            project_dirs()
                .map(|d| d.data_dir().join("merchantgpt.db"))
                .unwrap_or_else(|| PathBuf::from("merchantgpt.db"))
        })
    }

    pub fn skill_dirs(&self) -> Vec<PathBuf> {
        project_dirs()
            .map(|d| d.config_dir().join("skills"))
            .into_iter()
            .chain(self.skills.dirs.iter().cloned())
            .collect()
    }

    pub fn agent_config(&self) -> AgentConfig {
        AgentConfig {
            model: String::new(),
            max_iterations: self.agent.max_iterations.max(1),
            max_tokens: self.agent.max_tokens,
            max_tool_result_chars: self.agent.max_tool_result_chars,
            turn_timeout: Duration::from_secs(self.agent.turn_timeout_secs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        assert_eq!(Config::parse(TEMPLATE).unwrap(), Config::default());
    }

    #[test]
    fn api_keys_are_rejected_with_a_clear_message() {
        let err = Config::parse("[llm]\napi_key = \"sk\"\n").unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c = Config::parse("[agent]\nmax_iterations = 3\n").unwrap();
        assert_eq!(c.agent.max_iterations, 3);
        assert_eq!(c.agent.max_tokens, 4096);
        assert_eq!(c.agent_config().max_iterations, 3);
    }

    #[test]
    fn missing_file_loads_defaults_and_template_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        Config::write_template_if_missing(&path).unwrap();
        std::fs::write(&path, "[agent]\nmax_tokens = 1\n").unwrap();
        Config::write_template_if_missing(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().agent.max_tokens, 1);
    }
}

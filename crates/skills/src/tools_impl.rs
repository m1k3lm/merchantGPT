use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tools::{parse_args, Mutability, Tool, ToolContext, ToolDefinition, ToolError};

use crate::runner::{run_script, ScriptLimits};
use crate::Skill;

/// `load_skill` and, when any skill ships scripts, `run_skill_script`.
pub fn skill_tools(skills: Arc<Vec<Skill>>, limits: ScriptLimits) -> Vec<Arc<dyn Tool>> {
    if skills.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Arc<dyn Tool>> = vec![Arc::new(LoadSkill(skills.clone()))];
    if skills.iter().any(|s| !s.scripts.is_empty()) {
        out.push(Arc::new(RunSkillScript { skills, limits }));
    }
    out
}

struct LoadSkill(Arc<Vec<Skill>>);

#[derive(Deserialize)]
struct LoadArgs {
    name: String,
}

#[async_trait]
impl Tool for LoadSkill {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "load_skill".into(),
            description: "Load a skill's full instructions. Call this when a listed skill is relevant to the merchant's request, before acting on it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "Skill name from the list in the system prompt"}},
                "required": ["name"]
            }),
            mutability: Mutability::Read,
        }
    }

    async fn call(&self, args: Value, _: &ToolContext) -> Result<String, ToolError> {
        let a: LoadArgs = parse_args(args)?;
        let skill = self
            .0
            .iter()
            .find(|s| s.name == a.name)
            .ok_or_else(|| ToolError::Failed(format!("no skill named {:?}", a.name)))?;
        let mut out = skill.body.clone();
        if !skill.scripts.is_empty() {
            out.push_str(&format!(
                "\n\nScripts (run with run_skill_script): {}",
                skill.scripts.join(", ")
            ));
        }
        Ok(out)
    }
}

struct RunSkillScript {
    skills: Arc<Vec<Skill>>,
    limits: ScriptLimits,
}

#[derive(Deserialize)]
struct RunArgs {
    skill: String,
    script: String,
    #[serde(default)]
    args: Vec<String>,
}

#[async_trait]
impl Tool for RunSkillScript {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "run_skill_script".into(),
            description: "Run one of a skill's bundled scripts on the merchant's computer. Only scripts listed by load_skill can run; the merchant confirms every run.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "skill": {"type": "string"},
                    "script": {"type": "string", "description": "Script file name, e.g. report.sh"},
                    "args": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["skill", "script"]
            }),
            mutability: Mutability::Mutate,
        }
    }

    async fn call(&self, args: Value, _: &ToolContext) -> Result<String, ToolError> {
        let a: RunArgs = parse_args(args)?;
        let skill = self
            .skills
            .iter()
            .find(|s| s.name == a.skill)
            .ok_or_else(|| ToolError::Failed(format!("no skill named {:?}", a.skill)))?;
        // Matching against the listed names rules out paths like "../x".
        if !skill.scripts.contains(&a.script) {
            return Err(ToolError::InvalidArguments(format!(
                "{:?} is not a script of {}; available: {}",
                a.script,
                skill.name,
                skill.scripts.join(", ")
            )));
        }
        let path = skill.dir.join("scripts").join(&a.script);
        let out = run_script(&path, &a.args, &skill.dir, self.limits)
            .await
            .map_err(|e| ToolError::Failed(format!("could not start {}: {e}", a.script)))?;
        if out.timed_out {
            return Err(ToolError::Failed(format!(
                "{} timed out after {:?}",
                a.script, self.limits.timeout
            )));
        }
        let result = json!({
            "exit_code": out.status,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "truncated": out.truncated,
        });
        if out.status == Some(0) {
            Ok(result.to_string())
        } else {
            Err(ToolError::Failed(result.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skills(dir: &std::path::Path) -> Arc<Vec<Skill>> {
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/hi.sh"), "echo hi $1").unwrap();
        Arc::new(vec![
            Skill {
                name: "greet".into(),
                description: "d".into(),
                body: "Say hi.".into(),
                dir: dir.to_path_buf(),
                scripts: vec!["hi.sh".into()],
            },
            Skill {
                name: "plain".into(),
                description: "d".into(),
                body: "No scripts.".into(),
                dir: PathBuf::new(),
                scripts: vec![],
            },
        ])
    }

    #[tokio::test]
    async fn load_and_run_only_listed_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let tools = skill_tools(skills(dir.path()), ScriptLimits::default());
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[1].definition().mutability, Mutability::Mutate);
        let ctx = ToolContext::default();
        let body = tools[0].call(json!({"name": "greet"}), &ctx).await.unwrap();
        assert!(body.contains("Say hi.") && body.contains("hi.sh"));

        let out = tools[1]
            .call(
                json!({"skill": "greet", "script": "hi.sh", "args": ["Ana"]}),
                &ctx,
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["stdout"], "hi Ana\n");

        let escape = tools[1]
            .call(json!({"skill": "greet", "script": "../../../bin/sh"}), &ctx)
            .await;
        assert!(matches!(escape, Err(ToolError::InvalidArguments(_))));
    }

    #[test]
    fn no_skills_means_no_tools() {
        assert!(skill_tools(Arc::new(vec![]), ScriptLimits::default()).is_empty());
    }
}

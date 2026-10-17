//! Skill discovery: each skill is a folder holding a `SKILL.md` (frontmatter
//! with `name` and `description`, then instructions) and optional scripts in
//! a `scripts/` subfolder. Only names and descriptions go into the system
//! prompt; the agent loads a body on demand.

mod runner;
mod tools_impl;

use std::path::{Path, PathBuf};

pub use runner::{run_script, ScriptLimits, ScriptOutput};
pub use tools_impl::skill_tools;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub dir: PathBuf,
    /// File names inside `dir/scripts`.
    pub scripts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    #[error("{0}: SKILL.md must start with a --- frontmatter block")]
    NoFrontmatter(PathBuf),
    #[error("{0}: frontmatter is missing `{1}`")]
    MissingField(PathBuf, &'static str),
    #[error("{0}: skill name {1:?} must be lowercase letters, digits, - or _")]
    BadName(PathBuf, String),
    #[error("{0}: duplicate skill name {1:?}; the first one found is used")]
    Duplicate(PathBuf, String),
    #[error("{0}: {1}")]
    Io(PathBuf, String),
}

pub fn is_valid_skill_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner.to_string();
        }
    }
    v.to_string()
}

/// Splits `SKILL.md` into (name, description, body).
pub fn parse_skill_md(path: &Path, text: &str) -> Result<(String, String, String), SkillError> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(SkillError::NoFrontmatter(path.to_path_buf()));
    }
    let mut name = None;
    let mut description = None;
    let mut closed = false;
    let mut consumed = 1;
    for line in lines.by_ref() {
        consumed += 1;
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "name" => name = Some(unquote(v)),
                "description" => description = Some(unquote(v)),
                _ => {}
            }
        }
    }
    if !closed {
        return Err(SkillError::NoFrontmatter(path.to_path_buf()));
    }
    let name = name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| SkillError::MissingField(path.to_path_buf(), "name"))?;
    let description = description
        .filter(|d| !d.is_empty())
        .ok_or_else(|| SkillError::MissingField(path.to_path_buf(), "description"))?;
    if !is_valid_skill_name(&name) {
        return Err(SkillError::BadName(path.to_path_buf(), name));
    }
    let body = text.lines().skip(consumed).collect::<Vec<_>>().join("\n");
    Ok((name, description, body.trim().to_string()))
}

fn list_scripts(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir.join("scripts")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Loads every skill under the given roots. Missing roots are skipped;
/// invalid skills are reported but do not stop the others from loading.
pub fn load_skills(roots: &[PathBuf]) -> (Vec<Skill>, Vec<SkillError>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut errors = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("SKILL.md").is_file())
            .collect();
        dirs.sort();
        for dir in dirs {
            let md = dir.join("SKILL.md");
            let text = match std::fs::read_to_string(&md) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(SkillError::Io(md, e.to_string()));
                    continue;
                }
            };
            match parse_skill_md(&md, &text) {
                Ok((name, description, body)) => {
                    if skills.iter().any(|s| s.name == name) {
                        errors.push(SkillError::Duplicate(md, name));
                        continue;
                    }
                    skills.push(Skill {
                        scripts: list_scripts(&dir),
                        name,
                        description,
                        body,
                        dir,
                    });
                }
                Err(e) => errors.push(e),
            }
        }
    }
    (skills, errors)
}

/// Turns `/skill-name rest of message` into a message that tells the agent
/// to follow that skill. Returns `None` when the text is not such a command.
pub fn expand_command(text: &str, skills: &[Skill]) -> Option<String> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, input) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let skill = skills.iter().find(|s| s.name == name)?;
    let mut msg = format!(
        "Run the \"{}\" skill now. Its instructions:\n\n{}\n",
        skill.name, skill.body
    );
    if !skill.scripts.is_empty() {
        msg.push_str(&format!(
            "\nScripts available to run_skill_script: {}\n",
            skill.scripts.join(", ")
        ));
    }
    let input = input.trim();
    if !input.is_empty() {
        msg.push_str(&format!("\nMerchant input: {input}"));
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn frontmatter_parses_name_and_description() {
        let (n, d, b) = parse_skill_md(
            Path::new("x"),
            "---\nname: daily-sales\ndescription: \"Summarise: yesterday's sales\"\nextra: 1\n---\n\n# Steps\n1. Do it\n",
        )
        .unwrap();
        assert_eq!(n, "daily-sales");
        assert_eq!(d, "Summarise: yesterday's sales");
        assert_eq!(b, "# Steps\n1. Do it");
    }

    #[test]
    fn skill_without_frontmatter_is_rejected() {
        let p = Path::new("s/SKILL.md");
        assert_eq!(
            parse_skill_md(p, "# Just markdown"),
            Err(SkillError::NoFrontmatter(p.into()))
        );
        assert_eq!(
            parse_skill_md(p, "---\nname: a\n"),
            Err(SkillError::NoFrontmatter(p.into()))
        );
        assert_eq!(
            parse_skill_md(p, "---\nname: a\n---\nbody"),
            Err(SkillError::MissingField(p.into(), "description"))
        );
        assert!(matches!(
            parse_skill_md(p, "---\nname: Bad Name\ndescription: d\n---\n"),
            Err(SkillError::BadName(..))
        ));
    }

    #[test]
    fn load_skills_collects_scripts_and_reports_bad_and_duplicate_skills() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write(
            a.path(),
            "sales/SKILL.md",
            "---\nname: sales\ndescription: Sales\n---\nBody",
        );
        write(a.path(), "sales/scripts/report.sh", "echo hi");
        write(a.path(), "sales/scripts/.hidden", "");
        write(a.path(), "broken/SKILL.md", "no frontmatter");
        write(a.path(), "not-a-skill/README.md", "");
        write(
            b.path(),
            "dup/SKILL.md",
            "---\nname: sales\ndescription: Other\n---\n",
        );
        let (skills, errors) =
            load_skills(&[a.path().into(), b.path().into(), "/does/not/exist".into()]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].scripts, ["report.sh"]);
        assert_eq!(skills[0].description, "Sales");
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn slash_commands_expand_to_skill_instructions() {
        let skills = vec![Skill {
            name: "sales".into(),
            description: "d".into(),
            body: "Call shop_sales_report.".into(),
            dir: PathBuf::new(),
            scripts: vec!["x.sh".into()],
        }];
        let msg = expand_command("/sales last week", &skills).unwrap();
        assert!(msg.contains("Call shop_sales_report."));
        assert!(msg.contains("x.sh"));
        assert!(msg.ends_with("Merchant input: last week"));
        assert!(expand_command("/unknown", &skills).is_none());
        assert!(expand_command("hello /sales", &skills).is_none());
    }
}

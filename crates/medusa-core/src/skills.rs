use std::{
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr};

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    body: String,
}

impl SkillRegistry {
    pub fn load(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let dir = workspace.join(".medusa").join("skills");
        if !dir.exists() {
            return Ok(Self::default());
        }

        let mut skills = Vec::new();
        for entry in fs::read_dir(&dir)
            .wrap_err_with(|| format!("failed to read skills directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let skill_path = if path.is_dir() {
                path.join("SKILL.md")
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
                path
            } else {
                continue;
            };

            if !skill_path.is_file() {
                continue;
            }

            let body = fs::read_to_string(&skill_path)
                .wrap_err_with(|| format!("failed to read skill {}", skill_path.display()))?;
            let name_hint = skill_path
                .parent()
                .and_then(Path::file_name)
                .or_else(|| skill_path.file_stem())
                .and_then(|name| name.to_str())
                .unwrap_or("skill")
                .to_string();
            skills.push(parse_skill(&name_hint, skill_path, body));
        }

        skills.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self { skills })
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn list_text(&self) -> String {
        if self.skills.is_empty() {
            return "skills\nNo workspace skills found in .medusa/skills.".to_string();
        }

        let mut lines = vec!["skills".to_string()];
        for skill in &self.skills {
            if skill.description.is_empty() {
                lines.push(format!("${}", skill.name));
            } else {
                lines.push(format!("${:<18} {}", skill.name, skill.description));
            }
        }
        lines.join("\n")
    }

    pub fn prompt_context(&self, prompt: &str) -> Option<String> {
        let selected = self
            .skills
            .iter()
            .filter(|skill| prompt_references_skill(prompt, &skill.name))
            .take(3)
            .collect::<Vec<_>>();

        if selected.is_empty() {
            return None;
        }

        let mut context = String::from(
            "Active Medusa skills. Follow these reusable workspace instructions for this turn:\n",
        );
        for skill in selected {
            context.push_str("\n<skill name=\"");
            context.push_str(&skill.name);
            context.push_str("\" path=\"");
            context.push_str(&skill.path.display().to_string());
            context.push_str("\">\n");
            context.push_str(&compact(&skill.body, 12_000));
            context.push_str("\n</skill>\n");
        }

        Some(compact(&context, 24_000))
    }
}

fn parse_skill(name_hint: &str, path: PathBuf, body: String) -> Skill {
    let mut name = sanitize_skill_name(name_hint);
    let mut description = String::new();

    for raw_line in body.lines().take(32) {
        let line = raw_line.trim();
        if let Some(value) = metadata_value(line, "name") {
            name = sanitize_skill_name(value);
        } else if let Some(value) = metadata_value(line, "description") {
            description = value.trim().to_string();
        } else if description.is_empty() && line.starts_with('#') {
            description = line.trim_start_matches('#').trim().to_string();
        }
    }

    Skill {
        name,
        description,
        path,
        body,
    }
}

fn metadata_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)
        .or_else(|| line.strip_prefix(&key.to_ascii_uppercase()))
        .and_then(|rest| rest.strip_prefix(':'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn sanitize_skill_name(value: &str) -> String {
    let name = value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    name.trim_matches('-').to_string()
}

fn prompt_references_skill(prompt: &str, name: &str) -> bool {
    let prompt = prompt.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    prompt.contains(&format!("${name}"))
        || prompt.contains(&format!("skill:{name}"))
        || prompt.contains(&format!("skill {name}"))
}

fn compact(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let compact = chars.by_ref().take(max_chars).collect::<String>();

    if chars.next().is_some() {
        format!("{compact}...")
    } else {
        compact
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn missing_skills_directory_is_empty() {
        let registry = SkillRegistry::load(temp_workspace()).unwrap();

        assert!(registry.is_empty());
        assert!(registry.list_text().contains("No workspace skills"));
    }

    #[test]
    fn loads_skills_from_workspace_directory() {
        let workspace = temp_workspace();
        let dir = workspace.join(".medusa/skills/repo-map");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "name: repo-map\ndescription: Map the repository\n\nRead files carefully.",
        )
        .unwrap();

        let registry = SkillRegistry::load(&workspace).unwrap();

        assert_eq!(registry.skills.len(), 1);
        assert_eq!(registry.skills[0].name, "repo-map");
        assert!(registry.list_text().contains("$repo-map"));
    }

    #[test]
    fn explicit_skill_reference_adds_prompt_context() {
        let workspace = temp_workspace();
        let dir = workspace.join(".medusa/skills/review");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "description: Review code\n\nAlways lead with findings.",
        )
        .unwrap();
        let registry = SkillRegistry::load(&workspace).unwrap();

        let context = registry
            .prompt_context("use $review on this diff")
            .expect("skill should match");

        assert!(context.contains("Active Medusa skills"));
        assert!(context.contains("Always lead with findings."));
    }

    #[test]
    fn unrelated_prompt_does_not_load_skills() {
        let workspace = temp_workspace();
        let dir = workspace.join(".medusa/skills/review");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "description: Review code").unwrap();
        let registry = SkillRegistry::load(&workspace).unwrap();

        assert_eq!(registry.prompt_context("hello"), None);
    }

    fn temp_workspace() -> PathBuf {
        // Timestamp alone collides when parallel tests start in the same
        // instant; the counter makes each workspace unique per process.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "medusa-skills-test-{}-{suffix}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }
}

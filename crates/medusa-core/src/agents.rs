//! User-defined named agents loaded from `.medusa/agents/*.md`.
//!
//! Each file defines one agent: frontmatter-ish header lines (`name:`,
//! `description:`, `tools: read|shell|edit|verify`), then a blank line, then
//! the body used as the agent's system prompt addition. Workflow scripts
//! reference agents by name through the `agentType` field of an agent spec.

use std::{
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, bail};

use crate::workflow::SubagentToolPolicy;

/// Hard cap on loaded agents so a runaway directory cannot bloat prompts.
const MAX_AGENTS: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: Vec<AgentDefinition>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub tool_policy: SubagentToolPolicy,
    pub path: PathBuf,
    pub prompt: String,
}

impl AgentRegistry {
    /// Load all agents from `.medusa/agents/*.md`. Malformed or unreadable
    /// files are skipped and recorded in `warnings()` instead of failing the
    /// whole load, mirroring the `SkillRegistry` resilience contract.
    pub fn load(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let dir = workspace.join(".medusa").join("agents");
        if !dir.exists() {
            return Ok(Self::default());
        }

        let mut paths = fs::read_dir(&dir)
            .wrap_err_with(|| format!("failed to read agents directory {}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().and_then(|extension| extension.to_str()) == Some("md")
                    && path.is_file()
            })
            .collect::<Vec<_>>();
        paths.sort();

        let mut agents: Vec<AgentDefinition> = Vec::new();
        let mut warnings = Vec::new();
        for path in paths {
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) => {
                    warnings.push(format!("skipped {}: {error}", path.display()));
                    continue;
                }
            };
            match parse_agent(&path, &contents) {
                Ok(agent) => {
                    if agents.iter().any(|existing| existing.name == agent.name) {
                        warnings.push(format!(
                            "skipped {}: duplicate agent name {:?}",
                            path.display(),
                            agent.name
                        ));
                    } else if agents.len() >= MAX_AGENTS {
                        warnings.push(format!(
                            "skipped {}: agent limit of {MAX_AGENTS} reached",
                            path.display()
                        ));
                    } else {
                        agents.push(agent);
                    }
                }
                Err(error) => warnings.push(format!("skipped {}: {error}", path.display())),
            }
        }

        agents.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(Self { agents, warnings })
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn names(&self) -> Vec<String> {
        self.agents.iter().map(|agent| agent.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        let name = sanitize_agent_name(name);
        self.agents.iter().find(|agent| agent.name == name)
    }
}

fn parse_agent(path: &Path, contents: &str) -> Result<AgentDefinition> {
    let mut name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(sanitize_agent_name)
        .unwrap_or_default();
    let mut description = String::new();
    let mut tool_policy = SubagentToolPolicy::ShellRead;

    let mut lines = contents.lines();
    for raw_line in lines.by_ref() {
        let line = raw_line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = header_value(line, "name") {
            name = sanitize_agent_name(value);
        } else if let Some(value) = header_value(line, "description") {
            description = value.to_string();
        } else if let Some(value) = header_value(line, "tools") {
            tool_policy = crate::workflow::script::parse_tool_policy(value)?;
        } else {
            bail!("unrecognized header line {line:?} (expected name:, description:, or tools:)");
        }
    }

    let prompt = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    if prompt.is_empty() {
        bail!("agent body (the system prompt after the blank line) is empty");
    }
    if name.is_empty() {
        bail!("agent name is empty");
    }

    Ok(AgentDefinition {
        name,
        description,
        tool_policy,
        path: path.to_path_buf(),
        prompt,
    })
}

fn header_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)
        .or_else(|| line.strip_prefix(&key.to_ascii_uppercase()))
        .and_then(|rest| rest.strip_prefix(':'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn sanitize_agent_name(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn missing_agents_directory_is_empty() {
        let registry = AgentRegistry::load(temp_workspace()).unwrap();

        assert!(registry.is_empty());
        assert!(registry.warnings().is_empty());
    }

    #[test]
    fn loads_well_formed_agent_with_all_headers() {
        let workspace = temp_workspace();
        write_agent(
            &workspace,
            "reviewer.md",
            "name: reviewer\ndescription: Review diffs harshly\ntools: read\n\nAlways lead with findings.\nBe specific.",
        );

        let registry = AgentRegistry::load(&workspace).unwrap();

        assert_eq!(registry.agents().len(), 1);
        let agent = &registry.agents()[0];
        assert_eq!(agent.name, "reviewer");
        assert_eq!(agent.description, "Review diffs harshly");
        assert_eq!(agent.tool_policy, SubagentToolPolicy::ReadOnly);
        assert_eq!(agent.prompt, "Always lead with findings.\nBe specific.");
        assert!(registry.warnings().is_empty());
        assert_eq!(registry.get("reviewer").unwrap().name, "reviewer");
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn tools_header_defaults_to_shell_read_and_name_defaults_to_file_stem() {
        let workspace = temp_workspace();
        write_agent(
            &workspace,
            "Doc Writer.md",
            "description: Writes docs\n\nWrite clear prose.",
        );

        let registry = AgentRegistry::load(&workspace).unwrap();

        assert_eq!(registry.agents().len(), 1);
        assert_eq!(registry.agents()[0].name, "doc-writer");
        assert_eq!(
            registry.agents()[0].tool_policy,
            SubagentToolPolicy::ShellRead
        );
    }

    #[test]
    fn malformed_agents_are_skipped_with_warnings() {
        let workspace = temp_workspace();
        write_agent(&workspace, "good.md", "name: good\n\nDo good work.");
        write_agent(
            &workspace,
            "bad-tools.md",
            "name: bad-tools\ntools: sudo\n\nBody here.",
        );
        write_agent(&workspace, "no-body.md", "name: no-body\ndescription: x\n");
        write_agent(&workspace, "bad-header.md", "wat: nope\n\nBody here.");
        write_agent(&workspace, "notes.txt", "not an agent file");

        let registry = AgentRegistry::load(&workspace).unwrap();

        assert_eq!(registry.names(), vec!["good".to_string()]);
        assert_eq!(registry.warnings().len(), 3);
        assert!(
            registry
                .warnings()
                .iter()
                .any(|warning| warning.contains("bad-tools.md"))
        );
        assert!(
            registry
                .warnings()
                .iter()
                .any(|warning| warning.contains("no-body.md"))
        );
        assert!(
            registry
                .warnings()
                .iter()
                .any(|warning| warning.contains("bad-header.md"))
        );
    }

    #[test]
    fn duplicate_names_and_overflow_are_capped_with_warnings() {
        let workspace = temp_workspace();
        write_agent(&workspace, "a.md", "name: same\n\nFirst body.");
        write_agent(&workspace, "b.md", "name: same\n\nSecond body.");
        for index in 0..MAX_AGENTS + 3 {
            write_agent(
                &workspace,
                &format!("bulk-{index:03}.md"),
                &format!("name: bulk-{index:03}\n\nBody {index}."),
            );
        }

        let registry = AgentRegistry::load(&workspace).unwrap();

        assert_eq!(registry.agents().len(), MAX_AGENTS);
        assert!(
            registry
                .warnings()
                .iter()
                .any(|warning| warning.contains("duplicate agent name"))
        );
        assert!(
            registry
                .warnings()
                .iter()
                .any(|warning| warning.contains("agent limit"))
        );
    }

    fn write_agent(workspace: &Path, file: &str, contents: &str) {
        let dir = workspace.join(".medusa/agents");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), contents).unwrap();
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
            "medusa-agents-test-{}-{suffix}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }
}

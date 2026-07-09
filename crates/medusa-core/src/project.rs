use std::fs;
use std::path::Path;

const PROJECT_INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md", "MEDUSA.md"];
const PROJECT_INSTRUCTIONS_MAX_CHARS: usize = 16_000;

pub fn project_instructions_context(workspace: &Path) -> Option<String> {
    for name in PROJECT_INSTRUCTION_FILES {
        let path = workspace.join(name);
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut body = trimmed
            .chars()
            .take(PROJECT_INSTRUCTIONS_MAX_CHARS)
            .collect::<String>();
        if trimmed
            .chars()
            .nth(PROJECT_INSTRUCTIONS_MAX_CHARS)
            .is_some()
        {
            body.push_str("\n[project instructions truncated]");
        }

        return Some(format!(
            "Project instructions from {name} (user-maintained; follow its conventions, commands, and constraints for this workspace):\n{body}"
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "medusa-project-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_yields_no_context() {
        let workspace = temp_workspace("missing");
        assert_eq!(project_instructions_context(&workspace), None);
    }

    #[test]
    fn agents_md_is_loaded_and_labeled() {
        let workspace = temp_workspace("agents");
        fs::write(
            workspace.join("AGENTS.md"),
            "Run `cargo test` before commits.",
        )
        .unwrap();

        let context = project_instructions_context(&workspace).unwrap();
        assert!(context.contains("Project instructions from AGENTS.md"));
        assert!(context.contains("Run `cargo test` before commits."));
    }

    #[test]
    fn agents_md_takes_priority_over_claude_md() {
        let workspace = temp_workspace("priority");
        fs::write(workspace.join("CLAUDE.md"), "claude instructions").unwrap();
        fs::write(workspace.join("AGENTS.md"), "agents instructions").unwrap();

        let context = project_instructions_context(&workspace).unwrap();
        assert!(context.contains("agents instructions"));
        assert!(!context.contains("claude instructions"));
    }

    #[test]
    fn empty_file_falls_through_to_next_candidate() {
        let workspace = temp_workspace("empty");
        fs::write(workspace.join("AGENTS.md"), "  \n").unwrap();
        fs::write(workspace.join("CLAUDE.md"), "claude instructions").unwrap();

        let context = project_instructions_context(&workspace).unwrap();
        assert!(context.contains("Project instructions from CLAUDE.md"));
    }

    #[test]
    fn oversized_instructions_are_truncated() {
        let workspace = temp_workspace("truncate");
        fs::write(workspace.join("AGENTS.md"), "x".repeat(20_000)).unwrap();

        let context = project_instructions_context(&workspace).unwrap();
        assert!(context.contains("[project instructions truncated]"));
        assert!(context.chars().count() < 17_000);
    }
}

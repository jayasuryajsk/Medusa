use std::{
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    config: PermissionConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Open,
    Guarded,
    Readonly,
}

impl PermissionMode {
    pub fn all() -> &'static [Self] {
        &[Self::Open, Self::Guarded, Self::Readonly]
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "open" | "auto" | "trusted" => Some(Self::Open),
            "guarded" | "safe" | "default" => Some(Self::Guarded),
            "readonly" | "read-only" | "read_only" | "ro" => Some(Self::Readonly),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Guarded => "guarded",
            Self::Readonly => "readonly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Guarded => "Guarded",
            Self::Readonly => "Read-only",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Open => "Let workspace tools run with the normal Medusa workspace boundary.",
            Self::Guarded => {
                "Block destructive shell fragments and protected Medusa/Git paths by default."
            }
            Self::Readonly => "Allow common inspection commands and block file edits/patches.",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PermissionConfig {
    #[serde(default)]
    terminal: TerminalPermissionConfig,
    #[serde(default)]
    patch: PatchPermissionConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct TerminalPermissionConfig {
    #[serde(default)]
    allow_prefixes: Vec<String>,
    #[serde(default)]
    deny_contains: Vec<String>,
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PatchPermissionConfig {
    #[serde(default)]
    allow_prefixes: Vec<String>,
    #[serde(default)]
    deny_prefixes: Vec<String>,
}

impl PermissionPolicy {
    pub fn load(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let path = workspace.join(".medusa").join("permissions.json");
        let config = if path.exists() {
            let text = fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str(&text)
                .wrap_err_with(|| format!("failed to parse {}", path.display()))?
        } else {
            PermissionConfig::default()
        };

        Ok(Self { config })
    }

    pub fn write_mode(workspace: impl AsRef<Path>, mode: PermissionMode) -> Result<()> {
        let workspace = workspace.as_ref();
        let path = workspace.join(".medusa").join("permissions.json");
        let config = config_for_mode(mode);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }

        let json =
            serde_json::to_string_pretty(&config).wrap_err("failed to encode permissions")?;
        fs::write(&path, json).wrap_err_with(|| format!("failed to write {}", path.display()))
    }

    pub fn check_terminal_command(&self, command: &str) -> Result<()> {
        let command = command.trim_start();

        if let Some(denied) = first_matching_contains(command, &self.config.terminal.deny_contains)
        {
            bail!("terminal.exec denied by permissions: command contains `{denied}`");
        }

        let allow_prefixes = normalized_nonempty(&self.config.terminal.allow_prefixes);
        if !allow_prefixes.is_empty()
            && !allow_prefixes
                .iter()
                .any(|prefix| command_matches_allow_prefix(command, prefix))
        {
            bail!(
                "terminal.exec denied by permissions: command does not match an allow_prefixes entry"
            );
        }

        if self.config.terminal.read_only {
            check_read_only_terminal_command(command)?;
        }

        Ok(())
    }

    pub fn check_patch_paths(&self, paths: &[String]) -> Result<()> {
        let deny_prefixes = normalized_nonempty(&self.config.patch.deny_prefixes);
        let allow_prefixes = normalized_nonempty(&self.config.patch.allow_prefixes);

        for path in paths {
            if let Some(denied) = first_matching_prefix(path, &deny_prefixes) {
                bail!("file.patch denied by permissions: `{path}` matches `{denied}`");
            }

            if !allow_prefixes.is_empty()
                && !allow_prefixes
                    .iter()
                    .any(|prefix| path.starts_with(prefix.as_str()))
            {
                bail!(
                    "file.patch denied by permissions: `{path}` does not match an allow_prefixes entry"
                );
            }
        }

        Ok(())
    }
}

fn config_for_mode(mode: PermissionMode) -> PermissionConfig {
    match mode {
        PermissionMode::Open => PermissionConfig::default(),
        PermissionMode::Guarded => PermissionConfig {
            terminal: TerminalPermissionConfig {
                allow_prefixes: Vec::new(),
                deny_contains: vec![
                    "rm -rf".to_string(),
                    "mkfs".to_string(),
                    "dd if=".to_string(),
                    ":(){".to_string(),
                    "chmod -R 777".to_string(),
                    "chown -R".to_string(),
                ],
                read_only: false,
            },
            patch: PatchPermissionConfig {
                allow_prefixes: Vec::new(),
                deny_prefixes: vec![
                    ".git/".to_string(),
                    ".medusa/sessions/".to_string(),
                    ".medusa/permissions.json".to_string(),
                ],
            },
        },
        PermissionMode::Readonly => PermissionConfig {
            terminal: TerminalPermissionConfig {
                allow_prefixes: vec![
                    "pwd".to_string(),
                    "ls".to_string(),
                    "find".to_string(),
                    "rg".to_string(),
                    "grep".to_string(),
                    "cat".to_string(),
                    "sed".to_string(),
                    "head".to_string(),
                    "tail".to_string(),
                    "wc".to_string(),
                    "git status".to_string(),
                    "git diff".to_string(),
                    "git log".to_string(),
                ],
                deny_contains: Vec::new(),
                read_only: true,
            },
            patch: PatchPermissionConfig {
                allow_prefixes: vec!["__medusa_readonly_no_write_paths__".to_string()],
                deny_prefixes: Vec::new(),
            },
        },
    }
}

fn normalized_nonempty(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn first_matching_contains<'a>(text: &str, patterns: &'a [String]) -> Option<&'a str> {
    patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .find(|pattern| text.contains(*pattern))
}

fn command_matches_allow_prefix(command: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end();
    if prefix.is_empty() {
        return false;
    }

    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn check_read_only_terminal_command(command: &str) -> Result<()> {
    const DENIED_SHELL_TOKENS: &[&str] = &[
        "\n", "\r", ";", "&&", "||", "|", "&", ">", "<", "`", "$(", "${",
    ];

    for token in DENIED_SHELL_TOKENS {
        if command.contains(token) {
            bail!(
                "terminal.exec denied by readonly permissions: command contains shell token `{token}`"
            );
        }
    }

    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or_default();
    let args = words.collect::<Vec<_>>();

    match program {
        "sed" => {
            if args
                .iter()
                .any(|arg| *arg == "--in-place" || arg.starts_with("-i"))
            {
                bail!("terminal.exec denied by readonly permissions: sed in-place editing");
            }
        }
        "find" => {
            if let Some(arg) = args.iter().find(|arg| {
                matches!(
                    **arg,
                    "-delete"
                        | "-exec"
                        | "-execdir"
                        | "-ok"
                        | "-okdir"
                        | "-fprint"
                        | "-fprintf"
                        | "-fls"
                )
            }) {
                bail!("terminal.exec denied by readonly permissions: find action `{arg}`");
            }
        }
        _ => {}
    }

    Ok(())
}

fn first_matching_prefix<'a>(text: &str, patterns: &'a [String]) -> Option<&'a str> {
    patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .find(|pattern| text.starts_with(*pattern))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn missing_permissions_allow_by_default() {
        let policy = PermissionPolicy::load(temp_workspace()).unwrap();

        policy.check_terminal_command("rm -rf target").unwrap();
        policy
            .check_patch_paths(&["src/main.rs".to_string()])
            .unwrap();
    }

    #[test]
    fn terminal_denies_configured_substrings() {
        let workspace = temp_workspace();
        write_permissions(
            &workspace,
            r#"{"terminal":{"deny_contains":["rm -rf","git reset --hard"]}}"#,
        );
        let policy = PermissionPolicy::load(&workspace).unwrap();

        let error = policy
            .check_terminal_command("printf ok && rm -rf target")
            .unwrap_err();

        assert!(error.to_string().contains("rm -rf"));
    }

    #[test]
    fn terminal_allow_prefixes_are_enforced_when_present() {
        let workspace = temp_workspace();
        write_permissions(
            &workspace,
            r#"{"terminal":{"allow_prefixes":["cargo ","rg "]}}"#,
        );
        let policy = PermissionPolicy::load(&workspace).unwrap();

        policy.check_terminal_command("cargo test").unwrap();
        assert!(policy.check_terminal_command("git status").is_err());
    }

    #[test]
    fn readonly_terminal_blocks_shell_write_forms() {
        let workspace = temp_workspace();
        PermissionPolicy::write_mode(&workspace, PermissionMode::Readonly).unwrap();
        let policy = PermissionPolicy::load(&workspace).unwrap();

        policy.check_terminal_command("cat README.md").unwrap();
        policy.check_terminal_command("git status --short").unwrap();

        for command in [
            "cat > notes.txt",
            "sed -i '' s/old/new/g README.md",
            "find . -delete",
            "pwd && touch notes.txt",
            "grep medusa README.md | tee notes.txt",
        ] {
            let error = policy.check_terminal_command(command).unwrap_err();
            assert!(
                error.to_string().contains("readonly permissions"),
                "{command}: {error:?}"
            );
        }
    }

    #[test]
    fn patch_prefix_policy_is_enforced() {
        let workspace = temp_workspace();
        write_permissions(
            &workspace,
            r#"{"patch":{"allow_prefixes":["crates/"],"deny_prefixes":["crates/private/"]}}"#,
        );
        let policy = PermissionPolicy::load(&workspace).unwrap();

        policy
            .check_patch_paths(&["crates/medusa-core/src/lib.rs".to_string()])
            .unwrap();
        assert!(
            policy
                .check_patch_paths(&["README.md".to_string()])
                .is_err()
        );
        assert!(
            policy
                .check_patch_paths(&["crates/private/key.txt".to_string()])
                .is_err()
        );
    }

    fn write_permissions(workspace: &std::path::Path, json: &str) {
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(workspace.join(".medusa/permissions.json"), json).unwrap();
    }

    fn temp_workspace() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("medusa-permissions-test-{suffix}"));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }
}

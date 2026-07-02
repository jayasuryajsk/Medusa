use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct HookRuntime {
    workspace: PathBuf,
    config: HookConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HookConfig {
    #[serde(default)]
    hooks: BTreeMap<String, Vec<HookCommandConfig>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum HookCommandConfig {
    Shell(String),
    Object {
        command: String,
        cwd: Option<PathBuf>,
        #[serde(default)]
        fail_on_error: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEventKind {
    TurnStart,
    TurnEnd,
    PreTool,
    PostTool,
}

#[derive(Debug, Clone)]
pub struct HookEvent<'a> {
    kind: HookEventKind,
    turn_mode: &'a str,
    prompt: Option<&'a str>,
    tool_name: Option<&'a str>,
    tool_summary: Option<&'a str>,
    tool_status: Option<&'a str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookReport {
    pub runs: Vec<HookRun>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRun {
    pub event: String,
    pub command: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub failed: bool,
    pub fail_on_error: bool,
}

impl HookRuntime {
    pub fn load(workspace: impl Into<PathBuf>) -> Result<Self> {
        let workspace = workspace.into();
        let config_path = workspace.join(".medusa").join("hooks.json");
        let config = if config_path.exists() {
            let text = fs::read_to_string(&config_path)
                .wrap_err_with(|| format!("failed to read {}", config_path.display()))?;
            serde_json::from_str(&text)
                .wrap_err_with(|| format!("failed to parse {}", config_path.display()))?
        } else {
            HookConfig::default()
        };

        Ok(Self { workspace, config })
    }

    pub fn is_configured(&self) -> bool {
        self.config
            .hooks
            .values()
            .any(|commands| !commands.is_empty())
    }

    pub fn run(&self, event: HookEvent<'_>) -> HookReport {
        let Some(commands) = self.config.hooks.get(event.name()) else {
            return HookReport::default();
        };

        let runs = commands
            .iter()
            .filter(|command| !command.command().trim().is_empty())
            .map(|command| self.run_command(event.clone(), command))
            .collect();

        HookReport { runs }
    }

    fn run_command(&self, event: HookEvent<'_>, command: &HookCommandConfig) -> HookRun {
        let event_name = event.name().to_string();
        let command_text = command.command().to_string();
        let fail_on_error = command.fail_on_error();
        let cwd = match command.cwd() {
            Some(cwd) => self.resolve_workspace_path(cwd),
            None => Ok(self.workspace.clone()),
        };

        let cwd = match cwd {
            Ok(cwd) => cwd,
            Err(error) => {
                return HookRun {
                    event: event_name,
                    command: command_text,
                    code: None,
                    stdout: String::new(),
                    stderr: error.to_string(),
                    failed: true,
                    fail_on_error,
                };
            }
        };

        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsStr::new("sh").to_os_string());
        match Command::new(shell)
            .arg("-lc")
            .arg(&command_text)
            .current_dir(cwd)
            .env("MEDUSA_HOOK_EVENT", event.name())
            .env("MEDUSA_WORKSPACE", &self.workspace)
            .env("MEDUSA_TURN_MODE", event.turn_mode)
            .env("MEDUSA_PROMPT", event.prompt.unwrap_or_default())
            .env("MEDUSA_TOOL_NAME", event.tool_name.unwrap_or_default())
            .env(
                "MEDUSA_TOOL_SUMMARY",
                event.tool_summary.unwrap_or_default(),
            )
            .env("MEDUSA_TOOL_STATUS", event.tool_status.unwrap_or_default())
            .output()
        {
            Ok(output) => HookRun {
                event: event_name,
                command: command_text,
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                failed: !output.status.success(),
                fail_on_error,
            },
            Err(error) => HookRun {
                event: event_name,
                command: command_text,
                code: None,
                stdout: String::new(),
                stderr: error.to_string(),
                failed: true,
                fail_on_error,
            },
        }
    }

    fn resolve_workspace_path(&self, path: &Path) -> Result<PathBuf> {
        if path.is_absolute() {
            bail!("hook cwd must be workspace-relative: {}", path.display());
        }

        for component in path.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("hook cwd escapes workspace: {}", path.display());
                }
            }
        }

        let candidate = self.workspace.join(path);
        let canonical = candidate
            .canonicalize()
            .wrap_err_with(|| format!("hook cwd does not exist: {}", candidate.display()))?;

        if !canonical.starts_with(&self.workspace) {
            bail!(
                "hook cwd escapes workspace: {} is outside {}",
                canonical.display(),
                self.workspace.display()
            );
        }

        Ok(canonical)
    }
}

impl<'a> HookEvent<'a> {
    pub fn turn_start(turn_mode: &'a str, prompt: &'a str) -> Self {
        Self {
            kind: HookEventKind::TurnStart,
            turn_mode,
            prompt: Some(prompt),
            tool_name: None,
            tool_summary: None,
            tool_status: None,
        }
    }

    pub fn turn_end(turn_mode: &'a str, status: &'a str) -> Self {
        Self {
            kind: HookEventKind::TurnEnd,
            turn_mode,
            prompt: None,
            tool_name: None,
            tool_summary: None,
            tool_status: Some(status),
        }
    }

    pub fn pre_tool(turn_mode: &'a str, tool_name: &'a str, tool_summary: &'a str) -> Self {
        Self {
            kind: HookEventKind::PreTool,
            turn_mode,
            prompt: None,
            tool_name: Some(tool_name),
            tool_summary: Some(tool_summary),
            tool_status: None,
        }
    }

    pub fn post_tool(
        turn_mode: &'a str,
        tool_name: &'a str,
        tool_summary: &'a str,
        tool_status: &'a str,
    ) -> Self {
        Self {
            kind: HookEventKind::PostTool,
            turn_mode,
            prompt: None,
            tool_name: Some(tool_name),
            tool_summary: Some(tool_summary),
            tool_status: Some(tool_status),
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            HookEventKind::TurnStart => "turn_start",
            HookEventKind::TurnEnd => "turn_end",
            HookEventKind::PreTool => "pre_tool",
            HookEventKind::PostTool => "post_tool",
        }
    }
}

impl HookReport {
    pub fn blocking_failure_summary(&self) -> Option<String> {
        self.runs
            .iter()
            .find(|run| run.failed && run.fail_on_error)
            .map(HookRun::summary)
    }
}

impl HookRun {
    fn summary(&self) -> String {
        let detail = if !self.stderr.trim().is_empty() {
            self.stderr.trim()
        } else if !self.stdout.trim().is_empty() {
            self.stdout.trim()
        } else {
            "no output"
        };

        format!(
            "{} hook `{}` failed with exit {}: {}",
            self.event,
            compact(&self.command, 120),
            self.code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            compact(detail, 240)
        )
    }
}

impl HookCommandConfig {
    fn command(&self) -> &str {
        match self {
            Self::Shell(command) => command,
            Self::Object { command, .. } => command,
        }
    }

    fn cwd(&self) -> Option<&Path> {
        match self {
            Self::Shell(_) => None,
            Self::Object { cwd, .. } => cwd.as_deref(),
        }
    }

    fn fail_on_error(&self) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Object { fail_on_error, .. } => *fail_on_error,
        }
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn missing_config_is_empty() {
        let workspace = temp_workspace();
        let hooks = HookRuntime::load(&workspace).unwrap();

        assert!(!hooks.is_configured());
        assert!(
            hooks
                .run(HookEvent::turn_start("chat", "hi"))
                .runs
                .is_empty()
        );
    }

    #[test]
    fn hook_command_receives_event_environment() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(
            workspace.join(".medusa/hooks.json"),
            r#"{"hooks":{"pre_tool":["printf '%s' \"$MEDUSA_TOOL_NAME\" > hook.out"]}}"#,
        )
        .unwrap();

        let hooks = HookRuntime::load(&workspace).unwrap();
        let report = hooks.run(HookEvent::pre_tool("goal", "terminal.exec", "$ pwd"));

        assert_eq!(report.runs.len(), 1);
        assert!(!report.runs[0].failed);
        assert_eq!(
            fs::read_to_string(workspace.join("hook.out")).unwrap(),
            "terminal.exec"
        );
    }

    #[test]
    fn fail_on_error_surfaces_blocking_failure() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(
            workspace.join(".medusa/hooks.json"),
            r#"{"hooks":{"pre_tool":[{"command":"echo nope >&2; exit 7","fail_on_error":true}]}}"#,
        )
        .unwrap();

        let hooks = HookRuntime::load(&workspace).unwrap();
        let report = hooks.run(HookEvent::pre_tool("goal", "terminal.exec", "$ pwd"));
        let failure = report.blocking_failure_summary().unwrap();

        assert!(failure.contains("pre_tool hook"));
        assert!(failure.contains("exit 7"));
        assert!(failure.contains("nope"));
    }

    #[test]
    fn hook_cwd_cannot_escape_workspace() {
        let workspace = temp_workspace();
        fs::create_dir_all(workspace.join(".medusa")).unwrap();
        fs::write(
            workspace.join(".medusa/hooks.json"),
            r#"{"hooks":{"pre_tool":[{"command":"true","cwd":"..","fail_on_error":true}]}}"#,
        )
        .unwrap();

        let hooks = HookRuntime::load(&workspace).unwrap();
        let report = hooks.run(HookEvent::pre_tool("goal", "terminal.exec", "$ pwd"));

        assert!(report.blocking_failure_summary().is_some());
    }

    fn temp_workspace() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let index = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("medusa-hooks-test-{pid}-{suffix}-{index}"));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }
}

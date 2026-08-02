use std::{
    env, io,
    io::{Read, Write},
    path::Path,
};

use color_eyre::eyre::{Result, WrapErr, bail};
use medusa_core::auth::load_codex_oauth_credentials;
use medusa_core::credentials::CredentialStore;
use medusa_core::mcp::McpRegistry;
use medusa_core::model::{
    ConversationMessage, ModelGateway, ModelStreamEvent,
    provider::{ProviderAuth, ProviderRegistry, canonical_provider_id},
};
use medusa_core::permissions::PermissionMode;
use medusa_core::session::SessionOpenMode;
use medusa_core::tools::ToolRuntime;
use medusa_core::workflow::WorkflowEvent;
use serde::Serialize;

use crate::config::load_app_settings;
use crate::render::{compact_tool_detail, tool_output_failed};
use crate::util::{abbreviate_home, clean_model_error, compact_one_line, permission_context_text};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartupCommand {
    Tui(SessionOpenMode),
    Headless(HeadlessOptions),
    Auth(AuthCommand),
    Print(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthCommand {
    List,
    Set(String),
    Remove(String),
}

pub(crate) const HELP_TEXT: &str = "Usage: medusa [continue [session]]
       medusa run [options] [--] <task>

Commands:
  continue            Resume the last Medusa TUI session in this workspace
  continue <session>  Resume a specific session from .medusa/sessions
  run                 Run one non-interactive headless agent turn
  auth list           Show configured provider credentials
  auth set <provider> Securely store a provider API key
  auth remove <provider>

Run options:
  --model <provider/model>       Override the model for this run
  --permission <open|guarded|readonly>
  --json                         Print a machine-readable JSON result
  --no-stream                    Print only the final answer

If <task> is omitted, medusa run reads the task from stdin.";

pub(crate) const RUN_HELP_TEXT: &str = "Usage: medusa run [options] [--] <task>

Options:
  --model <provider/model>       Override the model for this run
  --permission <open|guarded|readonly>
  --json                         Print a machine-readable JSON result
  --no-stream                    Print only the final answer

If <task> is omitted, medusa run reads the task from stdin.";

pub(crate) const VERSION_TEXT: &str = concat!("medusa ", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadlessOptions {
    pub(crate) task: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) permission_mode: Option<PermissionMode>,
    pub(crate) json: bool,
    pub(crate) stream: bool,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            task: None,
            model: None,
            permission_mode: None,
            json: false,
            stream: true,
        }
    }
}

pub(crate) fn parse_args() -> Result<StartupCommand> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    parse_startup_command(&args)
}

pub(crate) fn parse_startup_command(args: &[String]) -> Result<StartupCommand> {
    match args {
        [] => Ok(StartupCommand::Tui(SessionOpenMode::New)),
        [command] if command == "continue" => {
            Ok(StartupCommand::Tui(SessionOpenMode::ContinueLast))
        }
        [command, session] if command == "continue" => Ok(StartupCommand::Tui(
            SessionOpenMode::ContinueNamed(session.to_string()),
        )),
        [command, rest @ ..] if command == "run" => parse_headless_options(rest),
        [command, rest @ ..] if command == "auth" => parse_auth_command(rest),
        [command] if command == "--help" || command == "-h" => Ok(StartupCommand::Print(HELP_TEXT)),
        [command] if command == "--version" || command == "-V" => {
            Ok(StartupCommand::Print(VERSION_TEXT))
        }
        [command] => bail!("unknown command `{command}` (try `medusa run` or `medusa continue`)"),
        _ => bail!(
            "too many arguments (usage: medusa [continue [session]] | medusa run [options] [--] <task>)"
        ),
    }
}

fn parse_auth_command(args: &[String]) -> Result<StartupCommand> {
    match args {
        [] => Ok(StartupCommand::Auth(AuthCommand::List)),
        [action] if action == "list" => Ok(StartupCommand::Auth(AuthCommand::List)),
        [action, provider] if action == "set" => Ok(StartupCommand::Auth(AuthCommand::Set(
            canonical_provider_id(provider),
        ))),
        [action, provider] if action == "remove" || action == "delete" => Ok(StartupCommand::Auth(
            AuthCommand::Remove(canonical_provider_id(provider)),
        )),
        [action] if action == "--help" || action == "-h" => Ok(StartupCommand::Print(
            "Usage: medusa auth [list | set <provider> | remove <provider>]",
        )),
        _ => bail!("usage: medusa auth [list | set <provider> | remove <provider>]"),
    }
}

pub(crate) fn run_auth(command: AuthCommand) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let registry = ProviderRegistry::load(&cwd)?;
    let store = CredentialStore::load();
    match command {
        AuthCommand::List => {
            for provider in registry.providers() {
                let status = match provider.auth {
                    ProviderAuth::CodexOauth => {
                        if load_codex_oauth_credentials().is_ok() {
                            "ready"
                        } else {
                            "missing"
                        }
                    }
                    ProviderAuth::Bearer => {
                        if store.api_key(provider).is_some() {
                            "ready"
                        } else {
                            "missing"
                        }
                    }
                    ProviderAuth::None => "not required",
                };
                println!("{:<12} {:<18} {status}", provider.id, provider.display_name);
            }
        }
        AuthCommand::Set(provider_id) => {
            let provider = registry.provider(&provider_id).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "unknown provider `{provider_id}`; add it to .medusa/providers.json first"
                )
            })?;
            if provider.auth != ProviderAuth::Bearer {
                bail!(
                    "{} does not use a stored API key ({})",
                    provider.display_name,
                    provider.auth_hint()
                );
            }
            let key = rpassword::prompt_password(format!("{} API key: ", provider.display_name))?;
            if key.trim().is_empty() {
                bail!("API key cannot be empty");
            }
            store.store_api_key(&provider.id, &key)?;
            println!("Stored {} credentials.", provider.display_name);
        }
        AuthCommand::Remove(provider_id) => {
            if store.remove(&provider_id)? {
                println!("Removed credentials for {provider_id}.");
            } else {
                println!("No stored credentials for {provider_id}.");
            }
        }
    }
    Ok(())
}

pub(crate) fn parse_headless_options(args: &[String]) -> Result<StartupCommand> {
    let mut options = HeadlessOptions::default();
    let mut task_parts = Vec::new();
    let mut index = 0usize;
    let mut passthrough = false;

    while index < args.len() {
        let arg = &args[index];
        if passthrough {
            task_parts.push(arg.clone());
            index += 1;
            continue;
        }

        match arg.as_str() {
            "--" => {
                passthrough = true;
                index += 1;
            }
            "--json" => {
                options.json = true;
                options.stream = false;
                index += 1;
            }
            "--no-stream" => {
                options.stream = false;
                index += 1;
            }
            "--stream" => {
                options.stream = true;
                index += 1;
            }
            "--model" | "-m" => {
                let Some(model) = args.get(index + 1) else {
                    bail!("{arg} requires a model name");
                };
                options.model = Some(model.clone());
                index += 2;
            }
            "--permission" | "--permissions" | "-p" => {
                let Some(mode) = args.get(index + 1) else {
                    bail!("{arg} requires open, guarded, or readonly");
                };
                options.permission_mode =
                    Some(PermissionMode::from_name(mode).ok_or_else(|| {
                        color_eyre::eyre::eyre!("unknown permission mode `{mode}`")
                    })?);
                index += 2;
            }
            "--help" | "-h" => return Ok(StartupCommand::Print(RUN_HELP_TEXT)),
            value if value.starts_with('-') && task_parts.is_empty() => {
                bail!("unknown medusa run option `{value}`");
            }
            value => {
                task_parts.push(value.to_string());
                index += 1;
            }
        }
    }

    if !task_parts.is_empty() {
        options.task = Some(task_parts.join(" "));
    }

    Ok(StartupCommand::Headless(options))
}

#[derive(Debug, Serialize)]
pub(crate) struct HeadlessToolEvent {
    name: String,
    summary: String,
    failed: Option<bool>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HeadlessRunResult {
    success: bool,
    model: String,
    permission_mode: String,
    event_count: usize,
    answer: String,
    tools: Vec<HeadlessToolEvent>,
}

pub(crate) fn run_headless(options: HeadlessOptions) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let tools = ToolRuntime::new(&cwd).wrap_err("failed to initialize Medusa tools")?;
    let tools = match McpRegistry::load(tools.workspace()) {
        Ok(registry) => tools.with_mcp(registry),
        Err(error) => {
            eprintln!("warning: MCP config ignored: {error}");
            tools
        }
    };
    let settings = load_app_settings(tools.workspace()).unwrap_or_default();
    let permission_mode = options
        .permission_mode
        .unwrap_or_else(|| settings.permission_mode());
    let tools = tools.with_permission_mode(permission_mode);
    let mut backend =
        ModelGateway::new(tools.workspace().to_path_buf()).wrap_err("HTTP client builds")?;

    let model_override = options.model.clone().or_else(|| {
        if env::var_os("MEDUSA_MODEL").is_none() {
            settings.model()
        } else {
            None
        }
    });
    if let Some(model) = model_override {
        backend
            .try_set_model_name(model)
            .wrap_err("invalid model selection")?;
    }

    let task = read_headless_task(&options)?;
    let prompt = headless_conversation_history(
        &task,
        tools.workspace(),
        backend.model_name(),
        permission_mode,
    );
    let mut answer = String::new();
    let mut tools_seen = Vec::new();
    let mut stdout = io::stdout();

    if !options.json {
        eprintln!(
            "medusa headless · model {} · permission {} · workspace {}",
            backend.model_name(),
            permission_mode.name(),
            abbreviate_home(&tools.workspace().to_string_lossy())
        );
    }

    let result = if permission_mode == PermissionMode::Readonly {
        backend.chat_stream_messages_read_only(&prompt, tools, |event| {
            handle_headless_event(event, &options, &mut answer, &mut tools_seen, &mut stdout)
        })
    } else {
        backend.chat_stream_messages(&prompt, tools, |event| {
            handle_headless_event(event, &options, &mut answer, &mut tools_seen, &mut stdout)
        })
    };

    match result {
        Ok(event_count) => {
            if options.stream && !options.json && !answer.ends_with('\n') {
                println!();
            } else if !options.stream && !options.json {
                println!("{}", answer.trim());
            }

            if options.json {
                let result = HeadlessRunResult {
                    success: true,
                    model: backend.model_name().to_string(),
                    permission_mode: permission_mode.name().to_string(),
                    event_count,
                    answer: answer.trim().to_string(),
                    tools: tools_seen,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Ok(())
        }
        Err(error) => {
            let clean_error = clean_model_error(&error.to_string());
            if options.json {
                let result = HeadlessRunResult {
                    success: false,
                    model: backend.model_name().to_string(),
                    permission_mode: permission_mode.name().to_string(),
                    event_count: 0,
                    answer: clean_error,
                    tools: tools_seen,
                };
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                eprintln!("{clean_error}");
            }
            std::process::exit(1);
        }
    }
}

pub(crate) fn read_headless_task(options: &HeadlessOptions) -> Result<String> {
    if let Some(task) = options
        .task
        .as_deref()
        .map(str::trim)
        .filter(|task| !task.is_empty())
    {
        return Ok(task.to_string());
    }

    let mut task = String::new();
    io::stdin()
        .read_to_string(&mut task)
        .wrap_err("failed to read task from stdin")?;
    let task = task.trim().to_string();
    if task.is_empty() {
        bail!("medusa run requires a task argument or stdin input");
    }
    Ok(task)
}

pub(crate) fn headless_conversation_history(
    task: &str,
    workspace: &Path,
    model: &str,
    permission_mode: PermissionMode,
) -> Vec<ConversationMessage> {
    vec![
        ConversationMessage {
            role: "system".to_string(),
            content: permission_context_text(permission_mode).to_string(),
            attachments: Vec::new(),
        },
        ConversationMessage {
            role: "system".to_string(),
            content: format!(
                "Medusa headless run. Workspace: {}. Model: {model}. Run autonomously in this non-interactive CLI harness. Use tools to inspect, edit, and verify as needed. Return the final answer only after the task is complete or clearly blocked.",
                workspace.display()
            ),
            attachments: Vec::new(),
        },
        ConversationMessage {
            role: "user".to_string(),
            content: task.to_string(),
            attachments: Vec::new(),
        },
    ]
}

pub(crate) fn handle_headless_event(
    event: ModelStreamEvent,
    options: &HeadlessOptions,
    answer: &mut String,
    tools_seen: &mut Vec<HeadlessToolEvent>,
    stdout: &mut io::Stdout,
) -> Result<()> {
    match event {
        ModelStreamEvent::Delta(delta) => {
            answer.push_str(&delta);
            if options.stream && !options.json {
                print!("{delta}");
                stdout.flush().wrap_err("failed to flush stdout")?;
            }
        }
        ModelStreamEvent::ReasoningDelta(delta) => {
            if options.stream && !options.json {
                eprintln!("reasoning: {}", compact_one_line(&delta, 160));
            }
        }
        ModelStreamEvent::CompactionStarted { before_tokens } => {
            eprintln!("compacting context ({before_tokens} estimated tokens)...");
        }
        ModelStreamEvent::CompactionFinished {
            before_tokens,
            after_tokens,
            folded_messages,
        } => {
            eprintln!(
                "context compacted: {before_tokens} -> {after_tokens} estimated tokens ({folded_messages} messages folded)"
            );
        }
        ModelStreamEvent::ToolStart { name, summary, .. } => {
            if !options.json {
                eprintln!("tool start: {name} · {}", compact_one_line(&summary, 180));
            }
            tools_seen.push(HeadlessToolEvent {
                name,
                summary,
                failed: None,
            });
        }
        ModelStreamEvent::ToolResult { name, output, .. } => {
            let failed = tool_output_failed(&output);
            let detail = compact_tool_detail(&output);
            if !options.json {
                let status = if failed { "failed" } else { "done" };
                eprintln!("tool {status}: {name} · {}", compact_one_line(&detail, 220));
            }
            tools_seen.push(HeadlessToolEvent {
                name,
                summary: detail,
                failed: Some(failed),
            });
        }
        ModelStreamEvent::Workflow(event) => {
            if !options.json {
                match &event {
                    WorkflowEvent::RunStarted { title, .. } => {
                        eprintln!("workflow started: {title}");
                    }
                    WorkflowEvent::PhaseStarted { name, .. } => {
                        eprintln!("workflow phase: {name}");
                    }
                    WorkflowEvent::AgentFinished { name, status, .. } => {
                        eprintln!("workflow agent {name}: {status:?}");
                    }
                    WorkflowEvent::Log { message, .. } => {
                        eprintln!("workflow log: {message}");
                    }
                    WorkflowEvent::RunFinished { status, .. } => {
                        eprintln!("workflow finished: {status:?}");
                    }
                    WorkflowEvent::AgentStarted { .. } | WorkflowEvent::PhaseFinished { .. } => {}
                }
            }
        }
        ModelStreamEvent::Usage(_)
        | ModelStreamEvent::Done { .. }
        | ModelStreamEvent::Error(_)
        | ModelStreamEvent::Cancelled => {}
    }
    Ok(())
}

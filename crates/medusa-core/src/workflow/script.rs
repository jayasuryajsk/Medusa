use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    path::Path,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::Sender,
    },
    thread,
};

use color_eyre::eyre::{Result, bail, eyre};
use rquickjs::{Context, Ctx, Exception, Function, Runtime as JsRuntime, Value as JsValue};
use serde_json::Value;

use super::{
    SubagentReport, SubagentToolPolicy, WorkflowEvent, WorkflowPhaseReport, WorkflowRunReport,
    WorkflowRuntime, WorkflowStatus, compact, phase_status_from_reports, tool_result_failed,
    workflow_run_id,
};
use crate::{
    agents::AgentRegistry,
    model::{ConversationMessage, DirectCodexBackend, ModelStreamEvent},
    tools::ToolRuntime,
};

const DEFAULT_MAX_SCRIPT_AGENTS: usize = 200;
const DEFAULT_MAX_PARALLEL_AGENTS: usize = 8;
const AGENT_RESULT_MAX_CHARS: usize = 24_000;
const WORKFLOW_SCRIPT_DIR: &str = ".medusa/workflows";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowScript {
    pub name: String,
    pub source: String,
}

impl WorkflowScript {
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
        }
    }

    pub fn load(workspace: &Path, name: &str) -> Result<Self> {
        let name = name.trim().trim_end_matches(".js");
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("invalid workflow script name {name:?}");
        }

        let path = workspace
            .join(WORKFLOW_SCRIPT_DIR)
            .join(format!("{name}.js"));
        let source = fs::read_to_string(&path)
            .map_err(|error| eyre!("failed to read workflow script {}: {error}", path.display()))?;
        Ok(Self::new(name, source))
    }

    pub fn list(workspace: &Path) -> Vec<String> {
        let Ok(entries) = fs::read_dir(workspace.join(WORKFLOW_SCRIPT_DIR)) else {
            return Vec::new();
        };
        let mut names = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.strip_suffix(".js").map(str::to_string)
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScriptAgentSpec {
    pub(crate) prompt: String,
    pub(crate) label: String,
    pub(crate) role: String,
    pub(crate) tool_policy: SubagentToolPolicy,
    pub(crate) schema: Option<Value>,
}

impl ScriptAgentSpec {
    fn parse(value: &Value, default_index: usize, agents: &AgentRegistry) -> Result<Self> {
        let default_label = format!("agent-{}", default_index + 1);
        match value {
            Value::String(prompt) if !prompt.trim().is_empty() => Ok(Self {
                prompt: prompt.clone(),
                label: default_label.clone(),
                role: default_label,
                tool_policy: SubagentToolPolicy::ShellRead,
                schema: None,
            }),
            Value::Object(map) => {
                let named = map
                    .get("agentType")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(|name| resolve_named_agent(agents, name))
                    .transpose()?;
                let mut prompt = map
                    .get("prompt")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|prompt| !prompt.is_empty())
                    .ok_or_else(|| eyre!("agent spec requires a non-empty `prompt`"))?
                    .to_string();
                if let Some(agent) = named {
                    prompt = format!("{}\n\n{prompt}", agent.prompt.trim());
                }
                let label = map
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        named
                            .map(|agent| agent.name.clone())
                            .unwrap_or(default_label)
                    });
                let role = map
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|role| !role.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| label.clone());
                let tool_policy = match map.get("tools").and_then(Value::as_str) {
                    // The named agent's policy is only the default; explicit
                    // `tools` in the spec always wins.
                    None => named
                        .map(|agent| agent.tool_policy)
                        .unwrap_or(SubagentToolPolicy::ShellRead),
                    Some(tools) => parse_tool_policy(tools)?,
                };
                let schema = map
                    .get("schema")
                    .filter(|schema| !schema.is_null())
                    .cloned();
                Ok(Self {
                    prompt,
                    label,
                    role,
                    tool_policy,
                    schema,
                })
            }
            _ => bail!(
                "agent spec must be a prompt string or {{prompt, agentType?, label?, tools?, schema?}} object"
            ),
        }
    }
}

fn resolve_named_agent<'registry>(
    agents: &'registry AgentRegistry,
    name: &str,
) -> Result<&'registry crate::agents::AgentDefinition> {
    agents.get(name).ok_or_else(|| {
        let known = agents.names();
        if known.is_empty() {
            eyre!("unknown agentType {name:?}: no agents are defined in .medusa/agents")
        } else {
            eyre!(
                "unknown agentType {name:?}; known agents: {}",
                known.join(", ")
            )
        }
    })
}

pub(crate) fn parse_tool_policy(tools: &str) -> Result<SubagentToolPolicy> {
    match tools.trim().to_ascii_lowercase().as_str() {
        "read" | "read-only" | "readonly" => Ok(SubagentToolPolicy::ReadOnly),
        "shell" | "shell-read" => Ok(SubagentToolPolicy::ShellRead),
        "edit" | "write" => Ok(SubagentToolPolicy::Edit),
        "verify" => Ok(SubagentToolPolicy::Verify),
        other => bail!("unknown agent tools policy {other:?} (use read, shell, edit, or verify)"),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentRunOutcome {
    pub(crate) output: String,
    pub(crate) tool_counts: BTreeMap<String, usize>,
    pub(crate) failed_tools: bool,
}

pub(crate) type AgentRunner =
    Arc<dyn Fn(&ScriptAgentSpec) -> Result<AgentRunOutcome> + Send + Sync>;

type AgentSlot = (SubagentReport, Result<Value>);

struct ScriptHost {
    run_id: String,
    events: Sender<WorkflowEvent>,
    runner: AgentRunner,
    agents: AgentRegistry,
    max_agents: usize,
    max_parallel: usize,
    total_agents: usize,
    phase_open: bool,
    phase_name: String,
    phase_agents: Vec<SubagentReport>,
    finished_phases: Vec<WorkflowPhaseReport>,
}

impl ScriptHost {
    fn emit(&self, event: WorkflowEvent) {
        let _ = self.events.send(event);
    }

    fn log(&self, message: &str) {
        self.emit(WorkflowEvent::Log {
            run_id: self.run_id.clone(),
            message: compact(message, 400),
        });
    }

    fn ensure_phase(&mut self) {
        if !self.phase_open {
            self.open_phase("main");
        }
    }

    fn open_phase(&mut self, name: &str) {
        let name = name.trim();
        self.phase_name = if name.is_empty() { "phase" } else { name }.to_string();
        self.phase_open = true;
        self.emit(WorkflowEvent::PhaseStarted {
            run_id: self.run_id.clone(),
            phase_index: self.finished_phases.len(),
            name: self.phase_name.clone(),
            agent_count: 0,
        });
    }

    fn start_phase(&mut self, name: &str) {
        self.close_phase();
        self.open_phase(name);
    }

    fn close_phase(&mut self) {
        if !self.phase_open {
            return;
        }
        let agents = std::mem::take(&mut self.phase_agents);
        let status = if agents.is_empty() {
            WorkflowStatus::Succeeded
        } else {
            phase_status_from_reports(&agents)
        };
        self.emit(WorkflowEvent::PhaseFinished {
            run_id: self.run_id.clone(),
            phase_index: self.finished_phases.len(),
            name: self.phase_name.clone(),
            status,
        });
        self.finished_phases.push(WorkflowPhaseReport {
            name: std::mem::take(&mut self.phase_name),
            agents,
        });
        self.phase_open = false;
    }

    fn reserve_agents(&mut self, count: usize) -> Result<()> {
        if self.total_agents + count > self.max_agents {
            bail!(
                "workflow agent budget exhausted ({} of {} agents used, {count} more requested); raise MEDUSA_WORKFLOW_MAX_SCRIPT_AGENTS if intentional",
                self.total_agents,
                self.max_agents
            );
        }
        self.total_agents += count;
        Ok(())
    }

    fn run_single(&mut self, spec_value: &Value) -> Result<Value> {
        let spec = ScriptAgentSpec::parse(spec_value, self.total_agents, &self.agents)?;
        self.reserve_agents(1)?;
        self.ensure_phase();

        let phase_index = self.finished_phases.len();
        let agent_index = self.phase_agents.len();
        let (report, value) = execute_script_agent(
            &self.runner,
            &self.events,
            &self.run_id,
            phase_index,
            agent_index,
            &spec,
        );
        self.phase_agents.push(report);
        value
    }

    fn run_parallel(&mut self, specs_value: &Value) -> Result<Vec<Value>> {
        let Value::Array(items) = specs_value else {
            bail!("parallel() expects an array of agent specs");
        };
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let mut specs = Vec::with_capacity(items.len());
        for (offset, item) in items.iter().enumerate() {
            specs.push(ScriptAgentSpec::parse(
                item,
                self.total_agents + offset,
                &self.agents,
            )?);
        }
        self.reserve_agents(specs.len())?;
        self.ensure_phase();

        let phase_index = self.finished_phases.len();
        let base_index = self.phase_agents.len();
        let worker_count = self.max_parallel.clamp(1, specs.len());
        let specs = Arc::new(specs);
        let next = Arc::new(AtomicUsize::new(0));
        let slots: Arc<Mutex<Vec<Option<AgentSlot>>>> =
            Arc::new(Mutex::new((0..specs.len()).map(|_| None).collect()));

        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let specs = Arc::clone(&specs);
            let next = Arc::clone(&next);
            let slots = Arc::clone(&slots);
            let runner = Arc::clone(&self.runner);
            let events = self.events.clone();
            let run_id = self.run_id.clone();
            handles.push(thread::spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(spec) = specs.get(index) else {
                        break;
                    };
                    let outcome = execute_script_agent(
                        &runner,
                        &events,
                        &run_id,
                        phase_index,
                        base_index + index,
                        spec,
                    );
                    if let Ok(mut slots) = slots.lock() {
                        slots[index] = Some(outcome);
                    }
                }
            }));
        }
        for handle in handles {
            if handle.join().is_err() {
                bail!("workflow parallel agent worker panicked");
            }
        }

        let slots = Arc::try_unwrap(slots)
            .map_err(|_| eyre!("parallel agent results still borrowed"))?
            .into_inner()
            .map_err(|_| eyre!("parallel agent results poisoned"))?;

        let mut values = Vec::with_capacity(slots.len());
        for (index, slot) in slots.into_iter().enumerate() {
            let Some((report, value)) = slot else {
                let spec = &specs[index];
                self.phase_agents.push(SubagentReport {
                    name: spec.label.clone(),
                    role: spec.role.clone(),
                    status: WorkflowStatus::Failed,
                    output: "agent ended without a result".to_string(),
                    tool_counts: BTreeMap::new(),
                });
                values.push(Value::Null);
                continue;
            };
            self.phase_agents.push(report);
            values.push(value.unwrap_or(Value::Null));
        }
        Ok(values)
    }
}

fn execute_script_agent(
    runner: &AgentRunner,
    events: &Sender<WorkflowEvent>,
    run_id: &str,
    phase_index: usize,
    agent_index: usize,
    spec: &ScriptAgentSpec,
) -> (SubagentReport, Result<Value>) {
    let _ = events.send(WorkflowEvent::AgentStarted {
        run_id: run_id.to_string(),
        phase_index,
        agent_index,
        name: spec.label.clone(),
        role: spec.role.clone(),
        tool_policy: spec.tool_policy,
    });

    let (report, value) = match run_agent_with_schema(runner, spec) {
        Ok((outcome, value)) => {
            let status = if outcome.failed_tools {
                WorkflowStatus::PartiallySucceeded
            } else {
                WorkflowStatus::Succeeded
            };
            (
                SubagentReport {
                    name: spec.label.clone(),
                    role: spec.role.clone(),
                    status,
                    output: compact(&outcome.output, 2_000),
                    tool_counts: outcome.tool_counts,
                },
                Ok(value),
            )
        }
        Err(error) => (
            SubagentReport {
                name: spec.label.clone(),
                role: spec.role.clone(),
                status: WorkflowStatus::Failed,
                output: format!("agent failed: {error}"),
                tool_counts: BTreeMap::new(),
            },
            Err(error),
        ),
    };

    let _ = events.send(WorkflowEvent::AgentFinished {
        run_id: run_id.to_string(),
        phase_index,
        agent_index,
        name: report.name.clone(),
        status: report.status,
        output: report.output.clone(),
        tool_counts: report.tool_counts.clone(),
    });
    (report, value)
}

fn run_agent_with_schema(
    runner: &AgentRunner,
    spec: &ScriptAgentSpec,
) -> Result<(AgentRunOutcome, Value)> {
    let outcome = runner(spec)?;
    let Some(schema) = &spec.schema else {
        let value = Value::String(compact(&outcome.output, AGENT_RESULT_MAX_CHARS));
        return Ok((outcome, value));
    };

    match parse_json_output(&outcome.output) {
        Ok(value) => Ok((outcome, value)),
        Err(parse_error) => {
            let mut repair_spec = spec.clone();
            repair_spec.prompt = format!(
                "{}\n\nYour previous reply was not valid JSON ({parse_error}). Previous reply:\n{}\n\nReturn ONLY corrected JSON matching the schema:\n{}",
                spec.prompt,
                compact(&outcome.output, 2_000),
                serde_json::to_string(schema).unwrap_or_default(),
            );
            let repaired = runner(&repair_spec)?;
            let value = parse_json_output(&repaired.output)
                .map_err(|error| eyre!("agent did not return valid JSON after retry: {error}"))?;
            Ok((repaired, value))
        }
    }
}

fn parse_json_output(output: &str) -> Result<Value> {
    let trimmed = output.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }

    if let Some(fenced) = extract_fenced_block(trimmed)
        && let Ok(value) = serde_json::from_str(fenced.trim())
    {
        return Ok(value);
    }

    let start = trimmed
        .char_indices()
        .find(|(_, c)| *c == '{' || *c == '[')
        .map(|(index, _)| index);
    let end = trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| *c == '}' || *c == ']')
        .map(|(index, c)| index + c.len_utf8());
    if let (Some(start), Some(end)) = (start, end)
        && start < end
        && let Ok(value) = serde_json::from_str(&trimmed[start..end])
    {
        return Ok(value);
    }

    serde_json::from_str(trimmed).map_err(|error| eyre!(error))
}

fn extract_fenced_block(text: &str) -> Option<&str> {
    let start = text.find("```")?;
    let after_fence = &text[start + 3..];
    let body_start = after_fence.find('\n')? + 1;
    let body = &after_fence[body_start..];
    let end = body.find("```")?;
    Some(&body[..end])
}

impl WorkflowRuntime {
    pub fn run_script<F>(
        &self,
        script: &WorkflowScript,
        args: Option<Value>,
        backend: DirectCodexBackend,
        tools: ToolRuntime,
        mut emit: F,
    ) -> Result<WorkflowRunReport>
    where
        F: FnMut(WorkflowEvent) -> Result<()>,
    {
        let runner = self.script_agent_runner(backend, tools);
        self.run_script_with_runner(script, args, runner, &mut emit)
    }

    fn script_agent_runner(&self, backend: DirectCodexBackend, tools: ToolRuntime) -> AgentRunner {
        let workspace = self.workspace.clone();
        let memory_context = self.memory_context.clone();
        Arc::new(move |spec: &ScriptAgentSpec| {
            let prompt = script_agent_prompt(&workspace, memory_context.as_deref(), spec);
            let messages = [ConversationMessage {
                role: "user".to_string(),
                content: prompt,
                attachments: Vec::new(),
            }];

            let mut output = String::new();
            let mut tool_counts = BTreeMap::new();
            let mut failed_tools = false;
            let handle_event = |event| {
                match event {
                    ModelStreamEvent::Delta(delta) => output.push_str(&delta),
                    ModelStreamEvent::ToolStart { name, .. } => {
                        *tool_counts.entry(name).or_insert(0) += 1;
                    }
                    ModelStreamEvent::ToolResult { output, .. } => {
                        if tool_result_failed(&output) {
                            failed_tools = true;
                        }
                    }
                    ModelStreamEvent::ReasoningDelta(_)
                    | ModelStreamEvent::Workflow(_)
                    | ModelStreamEvent::Usage(_)
                    | ModelStreamEvent::Done { .. }
                    | ModelStreamEvent::Error(_)
                    | ModelStreamEvent::Cancelled => {}
                }
                Ok(())
            };

            backend.chat_stream_messages_subagent(
                &messages,
                tools.clone(),
                spec.tool_policy.allows_mutation(),
                handle_event,
            )?;

            Ok(AgentRunOutcome {
                output: output.trim().to_string(),
                tool_counts,
                failed_tools,
            })
        })
    }

    pub(crate) fn run_script_with_runner<F>(
        &self,
        script: &WorkflowScript,
        args: Option<Value>,
        runner: AgentRunner,
        emit: &mut F,
    ) -> Result<WorkflowRunReport>
    where
        F: FnMut(WorkflowEvent) -> Result<()>,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        let script = script.clone();
        let agents = self.agent_registry.clone();
        let max_agents = max_script_agents();
        let max_parallel = max_parallel_agents();

        let worker = thread::spawn(move || {
            run_script_thread(
                script,
                args,
                runner,
                agents,
                sender,
                max_agents,
                max_parallel,
            )
        });

        for event in receiver {
            emit(event)?;
        }

        worker
            .join()
            .map_err(|_| eyre!("workflow script thread panicked"))?
    }
}

fn max_script_agents() -> usize {
    std::env::var("MEDUSA_WORKFLOW_MAX_SCRIPT_AGENTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_SCRIPT_AGENTS)
}

fn max_parallel_agents() -> usize {
    std::env::var("MEDUSA_WORKFLOW_MAX_PARALLEL")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_PARALLEL_AGENTS)
}

fn run_script_thread(
    script: WorkflowScript,
    args: Option<Value>,
    runner: AgentRunner,
    agents: AgentRegistry,
    events: Sender<WorkflowEvent>,
    max_agents: usize,
    max_parallel: usize,
) -> Result<WorkflowRunReport> {
    let run_id = workflow_run_id();
    let title = format!("script:{}", script.name);
    let task = args
        .as_ref()
        .map(|args| format!("{} {}", script.name, compact(&args.to_string(), 120)))
        .unwrap_or_else(|| script.name.clone());
    let _ = events.send(WorkflowEvent::RunStarted {
        run_id: run_id.clone(),
        title: title.clone(),
        task: task.clone(),
        phases: Vec::new(),
    });

    let host = Rc::new(RefCell::new(ScriptHost {
        run_id: run_id.clone(),
        events: events.clone(),
        runner,
        agents,
        max_agents,
        max_parallel,
        total_agents: 0,
        phase_open: false,
        phase_name: String::new(),
        phase_agents: Vec::new(),
        finished_phases: Vec::new(),
    }));

    let eval_result = eval_workflow_script(&script.source, args, Rc::clone(&host));

    let (finished_phases, total_agents) = {
        let mut host = host.borrow_mut();
        host.close_phase();
        (std::mem::take(&mut host.finished_phases), host.total_agents)
    };

    // Unlike fixed-plan phases, script phases may legitimately hold zero agents
    // (log-only or bookkeeping phases), so empty phases don't count as failures.
    let mut agent_statuses = finished_phases
        .iter()
        .filter(|phase| !phase.agents.is_empty())
        .map(|phase| phase_status_from_reports(&phase.agents));
    let phase_status = agent_statuses
        .next()
        .map(|first| agent_statuses.fold(first, WorkflowStatus::combine))
        .unwrap_or(WorkflowStatus::Succeeded);

    let (status, summary) = match &eval_result {
        Ok(return_value) => {
            let rendered = return_value
                .as_ref()
                .map(render_return_value)
                .filter(|rendered| !rendered.trim().is_empty());
            let headline = format!(
                "workflow script `{}` completed: {total_agents} agents across {} phases",
                script.name,
                finished_phases.len().max(1)
            );
            let summary = match rendered {
                Some(rendered) => format!("{headline}\n\n{}", compact(&rendered, 1_600)),
                None => headline,
            };
            (phase_status, summary)
        }
        Err(error) => (
            WorkflowStatus::Failed,
            format!(
                "workflow script `{}` failed after {total_agents} agents: {error}",
                script.name
            ),
        ),
    };

    let _ = events.send(WorkflowEvent::RunFinished {
        run_id: run_id.clone(),
        status,
        summary: summary.clone(),
    });

    Ok(WorkflowRunReport {
        run_id,
        title,
        task,
        phases: finished_phases,
        summary,
        status,
    })
}

fn render_return_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn eval_workflow_script(
    source: &str,
    args: Option<Value>,
    host: Rc<RefCell<ScriptHost>>,
) -> Result<Option<Value>> {
    let js_runtime =
        JsRuntime::new().map_err(|error| eyre!("failed to start JS runtime: {error}"))?;
    let context =
        Context::full(&js_runtime).map_err(|error| eyre!("failed to build JS context: {error}"))?;

    context.with(|ctx| -> Result<Option<Value>> {
        register_host_functions(&ctx, host).map_err(|error| eyre!("{error}"))?;

        let args_value = json_to_js(&ctx, &args.unwrap_or(Value::Null))
            .map_err(|error| eyre!("failed to convert workflow args: {error}"))?;
        ctx.globals()
            .set("args", args_value)
            .map_err(|error| eyre!("failed to set workflow args: {error}"))?;

        let wrapped = format!("(function() {{\n{source}\n}})()");
        match ctx.eval::<JsValue, _>(wrapped.into_bytes()) {
            Ok(value) => js_to_json(&ctx, value)
                .map_err(|error| eyre!("invalid script return value: {error}")),
            Err(rquickjs::Error::Exception) => Err(eyre!(format_js_exception(&ctx))),
            Err(error) => Err(eyre!("workflow script error: {error}")),
        }
    })
}

fn format_js_exception(ctx: &Ctx<'_>) -> String {
    let caught = ctx.catch();
    if let Some(exception) = caught.as_exception() {
        let message = exception
            .message()
            .unwrap_or_else(|| "unknown error".to_string());
        match exception.stack() {
            Some(stack) if !stack.trim().is_empty() => {
                format!("script exception: {message}\n{}", compact(&stack, 600))
            }
            _ => format!("script exception: {message}"),
        }
    } else if let Some(text) = caught.as_string().and_then(|text| text.to_string().ok()) {
        format!("script exception: {text}")
    } else {
        "script exception: unknown error".to_string()
    }
}

fn register_host_functions<'js>(
    ctx: &Ctx<'js>,
    host: Rc<RefCell<ScriptHost>>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    {
        let host = Rc::clone(&host);
        globals.set(
            "phase",
            Function::new(ctx.clone(), move |title: String| {
                host.borrow_mut().start_phase(&title);
            })?,
        )?;
    }

    {
        let host = Rc::clone(&host);
        globals.set(
            "log",
            Function::new(ctx.clone(), move |message: String| {
                host.borrow().log(&message);
            })?,
        )?;
    }

    {
        let host = Rc::clone(&host);
        globals.set(
            "agent",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, spec: JsValue<'js>| -> rquickjs::Result<JsValue<'js>> {
                    let spec = js_to_json(&ctx, spec)?
                        .ok_or_else(|| Exception::throw_message(&ctx, "agent() requires a spec"))?;
                    let result = host.borrow_mut().run_single(&spec);
                    match result {
                        Ok(value) => json_to_js(&ctx, &value),
                        Err(error) => Err(Exception::throw_message(&ctx, &error.to_string())),
                    }
                },
            )?,
        )?;
    }

    {
        let host = Rc::clone(&host);
        globals.set(
            "parallel",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, specs: JsValue<'js>| -> rquickjs::Result<JsValue<'js>> {
                    let specs = js_to_json(&ctx, specs)?.ok_or_else(|| {
                        Exception::throw_message(&ctx, "parallel() requires an array of specs")
                    })?;
                    let result = host.borrow_mut().run_parallel(&specs);
                    match result {
                        Ok(values) => json_to_js(&ctx, &Value::Array(values)),
                        Err(error) => Err(Exception::throw_message(&ctx, &error.to_string())),
                    }
                },
            )?,
        )?;
    }

    Ok(())
}

fn js_to_json<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> rquickjs::Result<Option<Value>> {
    if value.is_undefined() {
        return Ok(None);
    }
    let Some(text) = ctx.json_stringify(value)? else {
        return Ok(None);
    };
    let text = text.to_string()?;
    Ok(serde_json::from_str(&text).ok())
}

fn json_to_js<'js>(ctx: &Ctx<'js>, value: &Value) -> rquickjs::Result<JsValue<'js>> {
    ctx.json_parse(serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()))
}

fn script_agent_prompt(
    workspace: &Path,
    memory_context: Option<&str>,
    spec: &ScriptAgentSpec,
) -> String {
    let mut prompt = format!(
        "You are a Medusa workflow subagent named {}.\n\
Workspace: {}\n\
Role: {}\n\
Tool policy: {} — {}\n\n",
        spec.label,
        workspace.display(),
        spec.role,
        spec.tool_policy.label(),
        spec.tool_policy.instructions()
    );

    if let Some(memory_context) = memory_context {
        prompt.push_str("Parent session memory:\n");
        prompt.push_str(memory_context.trim());
        prompt.push_str("\n\n");
    }

    prompt.push_str("Task from the orchestrating workflow script:\n");
    prompt.push_str(spec.prompt.trim());
    prompt.push_str(
        "\n\nYour final message is returned to the orchestrating script as data, not shown to the user. Return exactly what the task asks for, compact and free of filler.",
    );

    if let Some(schema) = &spec.schema {
        prompt.push_str(&format!(
            "\nReturn ONLY valid JSON matching this JSON Schema, with no prose and no code fences:\n{}",
            serde_json::to_string(schema).unwrap_or_default()
        ));
    }

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    fn runtime() -> WorkflowRuntime {
        WorkflowRuntime::new(PathBuf::from("/tmp/project"))
    }

    fn echo_runner() -> AgentRunner {
        Arc::new(|spec: &ScriptAgentSpec| {
            Ok(AgentRunOutcome {
                output: format!("echo:{}", spec.prompt),
                tool_counts: BTreeMap::new(),
                failed_tools: false,
            })
        })
    }

    fn run(
        script_source: &str,
        args: Option<Value>,
        runner: AgentRunner,
    ) -> Result<(WorkflowRunReport, Vec<WorkflowEvent>)> {
        run_with_runtime(runtime(), script_source, args, runner)
    }

    fn run_with_runtime(
        runtime: WorkflowRuntime,
        script_source: &str,
        args: Option<Value>,
        runner: AgentRunner,
    ) -> Result<(WorkflowRunReport, Vec<WorkflowEvent>)> {
        let script = WorkflowScript::new("test", script_source);
        let mut events = Vec::new();
        let report = runtime.run_script_with_runner(&script, args, runner, &mut |event| {
            events.push(event);
            Ok(())
        })?;
        Ok((report, events))
    }

    /// Registry with one read-only `reviewer` agent, built from a real
    /// temp workspace so the loader path is exercised too.
    fn reviewer_registry() -> AgentRegistry {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let workspace = std::env::temp_dir().join(format!(
            "medusa-script-agents-test-{}-{unique}",
            std::process::id()
        ));
        let dir = workspace.join(".medusa/agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reviewer.md"),
            "name: reviewer\ndescription: Harsh diff reviewer\ntools: read\n\nAlways lead with findings.",
        )
        .unwrap();
        AgentRegistry::load(&workspace).unwrap()
    }

    /// Echoes the effective tool policy and prompt so tests can observe how
    /// agentType resolution rewrote the spec.
    fn policy_echo_runner() -> AgentRunner {
        Arc::new(|spec: &ScriptAgentSpec| {
            Ok(AgentRunOutcome {
                output: format!("{}|{}", spec.tool_policy.label(), spec.prompt),
                tool_counts: BTreeMap::new(),
                failed_tools: false,
            })
        })
    }

    #[test]
    fn script_returns_value_and_reports_phases() {
        let source = r#"
            phase("gather");
            const a = agent("first probe");
            const b = agent({ prompt: "second probe", label: "prober" });
            phase("wrap");
            log("wrapping up");
            return { a, b };
        "#;

        let (report, events) = run(source, None, echo_runner()).unwrap();

        assert_eq!(report.status, WorkflowStatus::Succeeded);
        assert_eq!(report.phases.len(), 2);
        assert_eq!(report.phases[0].name, "gather");
        assert_eq!(report.phases[0].agents.len(), 2);
        assert_eq!(report.phases[0].agents[1].name, "prober");
        assert!(report.summary.contains("echo:first probe"));
        assert!(events.iter().any(
            |event| matches!(event, WorkflowEvent::Log { message, .. } if message == "wrapping up")
        ));
    }

    #[test]
    fn parallel_runs_all_specs_and_preserves_order() {
        let source = r#"
            const results = parallel([
                "alpha",
                { prompt: "beta" },
                "gamma",
            ]);
            return results;
        "#;

        let (report, _) = run(source, None, echo_runner()).unwrap();

        assert_eq!(report.status, WorkflowStatus::Succeeded);
        assert_eq!(report.phases.len(), 1);
        assert_eq!(report.phases[0].name, "main");
        assert_eq!(report.phases[0].agents.len(), 3);
        assert!(report.summary.contains("echo:alpha"));
        assert!(report.summary.contains("echo:beta"));
        assert!(report.summary.contains("echo:gamma"));
    }

    #[test]
    fn loops_and_args_drive_agent_counts() {
        let source = r#"
            let outputs = [];
            for (let i = 0; i < args.rounds; i++) {
                outputs.push(agent(`round ${i}`));
            }
            return outputs.length;
        "#;

        let (report, _) = run(
            source,
            Some(serde_json::json!({ "rounds": 3 })),
            echo_runner(),
        )
        .unwrap();

        assert_eq!(report.phases[0].agents.len(), 3);
        assert!(report.summary.contains('3'));
    }

    #[test]
    fn schema_parses_json_and_retries_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_runner = Arc::clone(&calls);
        let runner: AgentRunner = Arc::new(move |_spec| {
            let call = calls_in_runner.fetch_add(1, Ordering::SeqCst);
            Ok(AgentRunOutcome {
                output: if call == 0 {
                    "not json at all".to_string()
                } else {
                    r#"{"bugs": ["off-by-one"]}"#.to_string()
                },
                tool_counts: BTreeMap::new(),
                failed_tools: false,
            })
        });

        let source = r#"
            const found = agent({
                prompt: "find bugs",
                schema: { type: "object", required: ["bugs"] },
            });
            return found.bugs[0];
        "#;

        let (report, _) = run(source, None, runner).unwrap();

        assert_eq!(report.status, WorkflowStatus::Succeeded);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(report.summary.contains("off-by-one"));
    }

    #[test]
    fn failed_agents_become_null_in_parallel_and_throw_in_agent() {
        let runner: AgentRunner = Arc::new(|spec: &ScriptAgentSpec| {
            if spec.prompt.contains("boom") {
                bail!("backend unavailable")
            }
            Ok(AgentRunOutcome {
                output: "ok".to_string(),
                tool_counts: BTreeMap::new(),
                failed_tools: false,
            })
        });

        let source = r#"
            const results = parallel(["fine", "boom now"]);
            const nulls = results.filter((r) => r === null).length;
            let threw = false;
            try {
                agent("boom again");
            } catch (error) {
                threw = true;
            }
            return { nulls, threw };
        "#;

        let (report, _) = run(source, None, runner).unwrap();

        assert_eq!(report.status, WorkflowStatus::PartiallySucceeded);
        assert!(report.summary.contains("\"nulls\": 1"));
        assert!(report.summary.contains("\"threw\": true"));
    }

    #[test]
    fn script_exception_fails_run_with_message() {
        let source = r#"throw new Error("intentional failure");"#;

        let (report, _) = run(source, None, echo_runner()).unwrap();

        assert_eq!(report.status, WorkflowStatus::Failed);
        assert!(report.summary.contains("intentional failure"));
    }

    #[test]
    fn scripts_without_return_still_report_agent_runs() {
        let (report, _) = run(
            "for (let i = 0; i < 5; i++) agent(`a${i}`);",
            None,
            echo_runner(),
        )
        .unwrap();

        assert_eq!(report.status, WorkflowStatus::Succeeded);
        assert_eq!(report.phases[0].agents.len(), 5);
        assert!(report.summary.contains("5 agents"));
    }

    #[test]
    fn agent_type_prepends_prompt_and_defaults_label_and_policy() {
        let runtime = runtime().with_agent_registry(reviewer_registry());
        let source = r#"return agent({ agentType: "reviewer", prompt: "check the diff" });"#;

        let (report, _) = run_with_runtime(runtime, source, None, policy_echo_runner()).unwrap();

        assert_eq!(report.status, WorkflowStatus::Succeeded);
        // Named agent's policy (read) became the default and its stored
        // prompt was prepended before the spec prompt.
        assert!(
            report
                .summary
                .contains("read-only|Always lead with findings.\n\ncheck the diff")
        );
        assert_eq!(report.phases[0].agents[0].name, "reviewer");
    }

    #[test]
    fn explicit_spec_tools_override_named_agent_policy() {
        let runtime = runtime().with_agent_registry(reviewer_registry());
        let source = r#"return agent({ agentType: "reviewer", prompt: "fix it", tools: "edit" });"#;

        let (report, _) = run_with_runtime(runtime, source, None, policy_echo_runner()).unwrap();

        assert_eq!(report.status, WorkflowStatus::Succeeded);
        assert!(report.summary.contains("edit|Always lead with findings."));
    }

    #[test]
    fn unknown_agent_type_fails_with_known_agent_names() {
        let runtime = runtime().with_agent_registry(reviewer_registry());
        let source = r#"return agent({ agentType: "nope", prompt: "check" });"#;

        let (report, _) = run_with_runtime(runtime, source, None, policy_echo_runner()).unwrap();

        assert_eq!(report.status, WorkflowStatus::Failed);
        assert!(report.summary.contains("unknown agentType \"nope\""));
        assert!(report.summary.contains("known agents: reviewer"));
    }

    #[test]
    fn unknown_agent_type_with_empty_registry_points_at_agents_directory() {
        let runtime = runtime().with_agent_registry(AgentRegistry::default());
        let source = r#"return agent({ agentType: "nope", prompt: "check" });"#;

        let (report, _) = run_with_runtime(runtime, source, None, policy_echo_runner()).unwrap();

        assert_eq!(report.status, WorkflowStatus::Failed);
        assert!(
            report
                .summary
                .contains("no agents are defined in .medusa/agents")
        );
    }

    #[test]
    fn load_rejects_traversal_names() {
        let workspace = PathBuf::from("/tmp/project");
        assert!(WorkflowScript::load(&workspace, "../evil").is_err());
        assert!(WorkflowScript::load(&workspace, "").is_err());
        assert!(WorkflowScript::load(&workspace, "a/b").is_err());
    }

    #[test]
    fn parse_json_output_handles_fences_and_prose() {
        assert_eq!(
            parse_json_output("{\"a\": 1}").unwrap(),
            serde_json::json!({"a": 1})
        );
        assert_eq!(
            parse_json_output("```json\n{\"a\": 1}\n```").unwrap(),
            serde_json::json!({"a": 1})
        );
        assert_eq!(
            parse_json_output("Here you go:\n{\"a\": 1}\nDone.").unwrap(),
            serde_json::json!({"a": 1})
        );
        assert!(parse_json_output("no json here").is_err());
    }
}

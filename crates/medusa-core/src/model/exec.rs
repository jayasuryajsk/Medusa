use std::path::PathBuf;

use serde_json::{Value, json};

use crate::harness::HarnessPolicy;
use crate::hooks::HookEvent;
use crate::model::types::*;
use crate::tools::{
    DecisionQuestionRequest, DecisionRequest, DecisionResult, ExploreBatchRequest,
    ExploreBatchResult, ExploreProbe, ExploreProbeKind, FileEditRequest, FileGlobRequest,
    FilePatchRequest, FileReadRequest, FileSearchRequest, FsListRequest, PlanUpdateItem,
    PlanUpdateRequest, PlanUpdateResult, QuestionRequest, TaskUpdateRequest, TerminalExecRequest,
    ToolRuntime,
};

/// Tools that neither mutate the workspace nor consult [`ToolLoopState`] —
/// safe to execute concurrently within one turn.
pub(crate) fn tool_call_is_read_only(name: &str) -> bool {
    matches!(
        name,
        "file_read" | "file_search" | "file_glob" | "fs_list" | "explore_batch"
    )
}

pub(crate) fn tool_call_is_file_mutation(name: &str) -> bool {
    matches!(name, "file_edit" | "file_patch")
}

/// Workspace-relative paths a successful file_edit/file_patch touched,
/// parsed from its raw output.
pub(crate) fn mutation_changed_files(output: &str) -> Vec<String> {
    if let Some(rest) = output.strip_prefix("edited files:\n") {
        return rest
            .lines()
            .take_while(|line| !line.starts_with("replacements:"))
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
    }
    if let Some(rest) = output.strip_prefix("patched files:\n") {
        return rest
            .lines()
            .take_while(|line| !line.starts_with("verify:"))
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
    }
    Vec::new()
}

/// Split a mutation output into its base and the trailing `verify:` block
/// appended by post-edit verification, if any.
pub(crate) fn split_verify_section(output: &str) -> (&str, Option<&str>) {
    match output.find("\nverify: ") {
        Some(position) => (&output[..position], Some(&output[position + 1..])),
        None => (output, None),
    }
}

pub(crate) fn execute_tool_call_with_hooks(
    tools: &ToolRuntime,
    call: &ToolCall,
    state: &ToolLoopState,
    policy: HarnessPolicy,
    tool_policy: ToolLoopPolicy,
) -> ToolExecution {
    let summary = summarize_tool_call(call);
    let tool_name = display_tool_name(&call.name);
    let turn_mode = policy.mode_label();

    if let Some(error) = tools
        .hooks()
        .run(HookEvent::pre_tool(turn_mode, tool_name, &summary))
        .blocking_failure_summary()
    {
        return ToolExecution {
            failed: true,
            output: format!("error: pre_tool hook blocked {tool_name}: {error}"),
        };
    }

    let mut execution = execute_tool_call(tools, call, state, tool_policy);
    let status = if execution.failed {
        "failed"
    } else {
        "succeeded"
    };

    if let Some(error) = tools
        .hooks()
        .run(HookEvent::post_tool(turn_mode, tool_name, &summary, status))
        .blocking_failure_summary()
    {
        if !execution.output.ends_with('\n') && !execution.output.is_empty() {
            execution.output.push('\n');
        }
        execution.output.push_str(&format!(
            "error: post_tool hook failed for {tool_name}: {error}"
        ));
        execution.failed = true;
    }

    execution
}

pub(crate) fn execute_workflow_run_with_hooks<F>(
    tools: &ToolRuntime,
    call: &ToolCall,
    policy: HarnessPolicy,
    tool_policy: ToolLoopPolicy,
    backend: &crate::model::types::DirectCodexBackend,
    on_event: &mut F,
) -> ToolExecution
where
    F: FnMut(ModelStreamEvent) -> color_eyre::eyre::Result<()>,
{
    let summary = summarize_tool_call(call);
    let tool_name = display_tool_name(&call.name);
    let turn_mode = policy.mode_label();

    if let Some(error) = tools
        .hooks()
        .run(HookEvent::pre_tool(turn_mode, tool_name, &summary))
        .blocking_failure_summary()
    {
        return ToolExecution {
            failed: true,
            output: format!("error: pre_tool hook blocked {tool_name}: {error}"),
        };
    }

    let mut execution = execute_workflow_run(tools, call, tool_policy, backend, on_event);
    let status = if execution.failed {
        "failed"
    } else {
        "succeeded"
    };

    if let Some(error) = tools
        .hooks()
        .run(HookEvent::post_tool(turn_mode, tool_name, &summary, status))
        .blocking_failure_summary()
    {
        if !execution.output.ends_with('\n') && !execution.output.is_empty() {
            execution.output.push('\n');
        }
        execution.output.push_str(&format!(
            "error: post_tool hook failed for {tool_name}: {error}"
        ));
        execution.failed = true;
    }

    execution
}

fn execute_workflow_run<F>(
    tools: &ToolRuntime,
    call: &ToolCall,
    tool_policy: ToolLoopPolicy,
    backend: &crate::model::types::DirectCodexBackend,
    on_event: &mut F,
) -> ToolExecution
where
    F: FnMut(ModelStreamEvent) -> color_eyre::eyre::Result<()>,
{
    if !tool_policy.allow_workflows() {
        return ToolExecution {
            failed: true,
            output: "error: workflow_run is unavailable here: workflow subagents cannot launch nested workflows".to_string(),
        };
    }

    let args = match serde_json::from_str::<Value>(&call.arguments) {
        Ok(args) => args,
        Err(error) => {
            return ToolExecution {
                failed: true,
                output: format!("error: invalid tool arguments: {error}"),
            };
        }
    };

    let Some(script_source) = args.get("script").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: workflow_run.script is required".to_string(),
        };
    };
    if script_source.trim().is_empty() {
        return ToolExecution {
            failed: true,
            output: "error: workflow_run.script cannot be empty".to_string(),
        };
    }

    let goal = args
        .get("goal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|goal| !goal.is_empty())
        .unwrap_or("model-workflow");
    let workflow_args = args.get("args").filter(|value| !value.is_null()).cloned();

    let script = crate::workflow::WorkflowScript::new(goal, script_source);
    let runtime = crate::workflow::WorkflowRuntime::new(tools.workspace().to_path_buf());

    match runtime.run_script(
        &script,
        workflow_args,
        backend.clone(),
        tools.clone(),
        |event| on_event(ModelStreamEvent::Workflow(event)),
    ) {
        Ok(report) => ToolExecution {
            failed: report.status == crate::workflow::WorkflowStatus::Failed,
            output: report.summary,
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: workflow failed to run: {error}"),
        },
    }
}

pub(crate) fn execute_tool_call(
    tools: &ToolRuntime,
    call: &ToolCall,
    state: &ToolLoopState,
    tool_policy: ToolLoopPolicy,
) -> ToolExecution {
    if crate::model::types::is_mutation_tool(&call.name) && !tool_policy.allow_mutation() {
        return ToolExecution {
            failed: true,
            output: format!(
                "error: {} is unavailable for this read-only workflow subagent",
                display_tool_name(&call.name)
            ),
        };
    }

    if call.name == "file_patch" && state.patch_requires_context {
        return ToolExecution {
            failed: true,
            output: "file.patch recovery active: the previous patch failed. Use file_read, file_search, fs_list, or terminal_exec first to inspect the target file/context, then submit a fresh unified diff. Blind malformed patch retries are paused until context is refreshed.".to_string(),
        };
    }

    let args = match serde_json::from_str::<Value>(&call.arguments) {
        Ok(args) => args,
        Err(error) => {
            return ToolExecution {
                failed: true,
                output: format!("error: invalid tool arguments: {error}"),
            };
        }
    };

    match call.name.as_str() {
        "file_read" => execute_file_read(tools, &args),
        "file_search" => execute_file_search(tools, &args),
        "file_glob" => execute_file_glob(tools, &args),
        "fs_list" => execute_fs_list(tools, &args),
        "explore_batch" => execute_explore_batch(tools, &args),
        "terminal_exec" => execute_terminal_exec(tools, &args),
        "file_edit" => execute_file_edit(tools, &args),
        "file_patch" => execute_file_patch(tools, &args),
        "task_update" => execute_task_update(tools, &args),
        "plan_update" => execute_plan_update(tools, &args),
        "decision_request" => execute_decision_request(tools, &args),
        "question" => execute_question(tools, &args),
        other => ToolExecution {
            failed: true,
            output: format!("error: unknown Medusa tool: {other}"),
        },
    }
}

fn execute_file_read(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(paths) = paths_arg(args) else {
        return ToolExecution {
            failed: true,
            output: "error: file_read.paths is required".to_string(),
        };
    };

    let request = FileReadRequest {
        paths,
        start_line: optional_usize(args, "start_line"),
        end_line: optional_usize(args, "end_line"),
    };

    match tools.file_read(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format_file_read_result(&result),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_file_search(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(query) = args.get("query").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: file_search.query is required".to_string(),
        };
    };

    let request = FileSearchRequest {
        query: query.to_string(),
        path: optional_path(args, "path"),
        depth: optional_usize(args, "depth"),
        max_results: optional_usize(args, "max_results"),
        case_sensitive: optional_bool(args, "case_sensitive"),
        include: args
            .get("include")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
    };

    match tools.file_search(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format_file_search_result(&result),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_file_glob(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: file_glob.pattern is required".to_string(),
        };
    };

    let request = FileGlobRequest {
        pattern: pattern.to_string(),
        path: optional_path(args, "path"),
        max_results: optional_usize(args, "max_results"),
    };

    match tools.file_glob(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format_file_glob_result(&result),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_fs_list(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let request = FsListRequest {
        path: optional_path(args, "path"),
        depth: optional_usize(args, "depth"),
        max_entries: optional_usize(args, "max_entries"),
    };

    match tools.fs_list(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format_fs_list_result(&result),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_explore_batch(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(probes) = args.get("probes").and_then(Value::as_array) else {
        return ToolExecution {
            failed: true,
            output: "error: explore_batch.probes is required".to_string(),
        };
    };

    let mut parsed = Vec::new();
    for (index, probe) in probes.iter().enumerate().take(12) {
        let Some(kind) = probe
            .get("kind")
            .or_else(|| probe.get("tool"))
            .and_then(Value::as_str)
            .and_then(ExploreProbeKind::from_name)
        else {
            return ToolExecution {
                failed: true,
                output: format!("error: explore_batch.probes[{index}].kind is required"),
            };
        };

        let paths = paths_arg(probe).unwrap_or_default();
        parsed.push(ExploreProbe {
            kind,
            query: probe
                .get("query")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToString::to_string),
            path: optional_path(probe, "path"),
            paths,
            command: probe
                .get("command")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToString::to_string),
            cwd: optional_path(probe, "cwd"),
            start_line: optional_usize(probe, "start_line"),
            end_line: optional_usize(probe, "end_line"),
            depth: optional_usize(probe, "depth"),
            max_results: optional_usize(probe, "max_results"),
            max_entries: optional_usize(probe, "max_entries"),
            case_sensitive: optional_bool(probe, "case_sensitive"),
        });
    }

    let request = ExploreBatchRequest {
        goal: args
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or("explore workspace context")
            .to_string(),
        probes: parsed,
    };

    match tools.explore_batch(request) {
        Ok(result) => ToolExecution {
            failed: result.failed == result.probes.len(),
            output: format_explore_batch_result(&result),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_file_edit(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: file_edit.path is required".to_string(),
        };
    };
    let Some(old_string) = string_arg(args, "oldString", "old_string") else {
        return ToolExecution {
            failed: true,
            output: "error: file_edit.oldString is required".to_string(),
        };
    };
    let Some(new_string) = string_arg(args, "newString", "new_string") else {
        return ToolExecution {
            failed: true,
            output: "error: file_edit.newString is required".to_string(),
        };
    };

    let request = FileEditRequest {
        path: PathBuf::from(path),
        old_string: old_string.to_string(),
        new_string: new_string.to_string(),
        replace_all: bool_arg(args, "replaceAll", "replace_all").unwrap_or(false),
    };

    match tools.file_edit(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format!(
                "edited files:\n{}\nreplacements: {}",
                result.path, result.replacements
            ),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!(
                "error: {error}\nrecovery: inspect the current file/context with file_read or file_search before retrying file_edit."
            ),
        },
    }
}

fn execute_terminal_exec(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: terminal_exec.command is required".to_string(),
        };
    };

    let request = TerminalExecRequest {
        command: command.to_string(),
        cwd: optional_path(args, "cwd"),
        background: bool_arg(args, "background", "background").unwrap_or(false),
    };

    match tools.terminal_exec(request) {
        Ok(result) => {
            if result.background {
                return ToolExecution {
                    failed: false,
                    output: format!(
                        "background: started\npid: {}\ncommand: {}",
                        result.pid.unwrap_or(0),
                        result.command
                    ),
                };
            }
            let mut output = format!("exit: {}\n", result.code.unwrap_or(-1));
            if result.stdout.is_empty() {
                output.push_str("stdout: <empty>\n");
            } else {
                output.push_str("stdout:\n");
                output.push_str(&result.stdout);
                if !result.stdout.ends_with('\n') {
                    output.push('\n');
                }
            }
            if !result.stderr.is_empty() {
                output.push_str("stderr:\n");
                output.push_str(&result.stderr);
                if !result.stderr.ends_with('\n') {
                    output.push('\n');
                }
            }
            ToolExecution {
                failed: result.code != Some(0),
                output,
            }
        }
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_file_patch(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(diff) = args.get("diff").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: file_patch.diff is required".to_string(),
        };
    };

    let request = FilePatchRequest {
        diff: diff.to_string(),
        cwd: optional_path(args, "cwd"),
        description: args
            .get("description")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    };

    match tools.file_patch(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format!("patched files:\n{}", result.changed_files.join("\n")),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!(
                "error: {error}\nrecovery: inspect the current file/context with file_read, file_search, fs_list, or terminal_exec before retrying file_patch. Use a fresh unified diff with exact hunk context."
            ),
        },
    }
}

fn execute_task_update(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(status) = args.get("status").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: task_update.status is required".to_string(),
        };
    };

    match tools.task_update(TaskUpdateRequest::new(status)) {
        Ok(result) => ToolExecution {
            failed: false,
            output: result.status,
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_plan_update(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(items) = args.get("items").and_then(Value::as_array) else {
        return ToolExecution {
            failed: true,
            output: "error: plan_update.items is required".to_string(),
        };
    };

    let mut parsed_items = Vec::new();
    for (index, item) in items.iter().enumerate().take(24) {
        let Some(text) = item.get("text").and_then(Value::as_str) else {
            return ToolExecution {
                failed: true,
                output: format!("error: plan_update.items[{index}].text is required"),
            };
        };
        let Some(status) = item.get("status").and_then(Value::as_str) else {
            return ToolExecution {
                failed: true,
                output: format!("error: plan_update.items[{index}].status is required"),
            };
        };
        let evidence = item
            .get("evidence")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        parsed_items.push(PlanUpdateItem {
            text: text.to_string(),
            status: status.to_string(),
            evidence,
        });
    }

    let request = PlanUpdateRequest {
        summary: args
            .get("summary")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string),
        items: parsed_items,
    };

    match tools.plan_update(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format_plan_update_result(&result),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_decision_request(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(questions) = args.get("questions").and_then(Value::as_array) else {
        return ToolExecution {
            failed: true,
            output: "error: decision_request.questions is required".to_string(),
        };
    };

    let mut parsed_questions = Vec::new();
    for (index, question) in questions.iter().enumerate().take(8) {
        let Some(prompt) = question.get("prompt").and_then(Value::as_str) else {
            return ToolExecution {
                failed: true,
                output: format!("error: decision_request.questions[{index}].prompt is required"),
            };
        };

        let options = question
            .get("options")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        parsed_questions.push(DecisionQuestionRequest {
            id: question
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            prompt: prompt.to_string(),
            kind: question
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("choice")
                .to_string(),
            options,
            recommended: question
                .get("recommended")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            required: question
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }

    let assumptions = args
        .get("assumptions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let request = DecisionRequest {
        title: args
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string),
        reason: args
            .get("reason")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string),
        questions: parsed_questions,
        assumptions,
    };

    match tools.decision_request(request) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format_decision_result(&result),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_question(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(question) = args.get("question").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: question.question is required".to_string(),
        };
    };

    match tools.question(QuestionRequest::new(question)) {
        Ok(result) => ToolExecution {
            failed: false,
            output: format!("question for user: {}", result.question),
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn format_file_read_result(result: &crate::tools::FileReadResult) -> String {
    let mut output = format!("read files: {}\n", result.files.len());
    for file in &result.files {
        output.push_str(&format!(
            "{}:{}-{} / {} lines{}\n",
            file.path,
            file.start_line,
            file.end_line,
            file.total_lines,
            if file.truncated { " (truncated)" } else { "" }
        ));
        for line in &file.lines {
            output.push_str(&format!("{:>5} | {}\n", line.number, line.text));
        }
    }
    output
}

fn format_file_search_result(result: &crate::tools::FileSearchResult) -> String {
    let mut output = format!(
        "query: {}\nmode: {}\nsearched files: {}\nmatches: {}{}\n",
        result.query,
        if result.regex { "regex" } else { "literal" },
        result.searched_files,
        result.matches.len(),
        if result.truncated { " (truncated)" } else { "" }
    );
    for hit in &result.matches {
        output.push_str(&format!("{}:{}: {}\n", hit.path, hit.line, hit.text));
    }
    output
}

fn format_file_glob_result(result: &crate::tools::FileGlobResult) -> String {
    let mut output = format!(
        "pattern: {}\nroot: {}\nmatches: {}{}\n",
        result.pattern,
        result.root,
        result.paths.len(),
        if result.truncated { " (truncated)" } else { "" }
    );
    for path in &result.paths {
        output.push_str(path);
        output.push('\n');
    }
    output
}

fn format_fs_list_result(result: &crate::tools::FsListResult) -> String {
    let mut output = format!(
        "root: {}{}\n",
        result.root,
        if result.truncated { " (truncated)" } else { "" }
    );
    for entry in &result.entries {
        output.push_str(&format!(
            "{}{} {}\n",
            "  ".repeat(entry.depth),
            entry.kind,
            entry.path
        ));
    }
    output
}

fn format_explore_batch_result(result: &ExploreBatchResult) -> String {
    let mut output = format!(
        "Evidence Board\n\
goal: {}\n\
probes: {} · failed {} · elapsed {}ms\n",
        result.goal,
        result.probes.len(),
        result.failed,
        result.elapsed_ms
    );

    for probe in &result.probes {
        output.push_str(&format!(
            "\n{}. {} · {} · {}ms{}\n",
            probe.index + 1,
            probe.kind,
            probe.label,
            probe.elapsed_ms,
            if probe.failed { " · failed" } else { "" }
        ));
        for line in compact(&probe.output, 2400).lines() {
            output.push_str("   ");
            output.push_str(line);
            output.push('\n');
        }
    }

    compact(&output, 20_000)
}

fn format_plan_update_result(result: &PlanUpdateResult) -> String {
    json!({
        "summary": result.summary,
        "items": result
            .items
            .iter()
            .map(|item| {
                json!({
                    "text": item.text,
                    "status": item.status,
                    "evidence": item.evidence,
                })
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}

fn format_decision_result(result: &DecisionResult) -> String {
    json!({
        "title": result.title,
        "reason": result.reason,
        "questions": result
            .questions
            .iter()
            .map(|question| {
                json!({
                    "id": question.id,
                    "prompt": question.prompt,
                    "kind": question.kind,
                    "options": question.options,
                    "recommended": question.recommended,
                    "required": question.required,
                })
            })
            .collect::<Vec<_>>(),
        "assumptions": result.assumptions,
    })
    .to_string()
}

pub(crate) fn update_tool_loop_state(
    state: &mut ToolLoopState,
    call: &ToolCall,
    execution: &ToolExecution,
) {
    match call.name.as_str() {
        "file_edit" | "file_patch" if execution.failed => {
            state.patch_requires_context = true;
        }
        "file_edit" | "file_patch" => {
            state.patch_requires_context = false;
        }
        "file_read" | "file_search" | "file_glob" | "fs_list" | "explore_batch"
        | "terminal_exec" | "workflow_run" => {
            state.patch_requires_context = false;
        }
        _ => {}
    }
}

fn paths_arg(args: &Value) -> Option<Vec<PathBuf>> {
    if let Some(values) = args.get("paths").and_then(Value::as_array) {
        let paths = values
            .iter()
            .filter_map(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        return (!paths.is_empty()).then_some(paths);
    }

    args.get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| vec![PathBuf::from(value)])
}

fn optional_path(args: &Value, key: &str) -> Option<PathBuf> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

fn optional_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn optional_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn string_arg<'a>(args: &'a Value, primary: &str, fallback: &str) -> Option<&'a str> {
    args.get(primary)
        .or_else(|| args.get(fallback))
        .and_then(Value::as_str)
}

fn bool_arg(args: &Value, primary: &str, fallback: &str) -> Option<bool> {
    args.get(primary)
        .or_else(|| args.get(fallback))
        .and_then(Value::as_bool)
}

pub(crate) fn summarize_tool_call(call: &ToolCall) -> String {
    let args = serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
    match call.name.as_str() {
        "file_read" => paths_arg(&args)
            .map(|paths| {
                if paths.len() == 1 {
                    format!("read {}", paths[0].display())
                } else {
                    format!("read {} files", paths.len())
                }
            })
            .unwrap_or_else(|| "read files".to_string()),
        "file_search" => args
            .get("query")
            .and_then(Value::as_str)
            .map(|query| format!("search {query:?}"))
            .unwrap_or_else(|| "search files".to_string()),
        "file_glob" => args
            .get("pattern")
            .and_then(Value::as_str)
            .map(|pattern| format!("glob {pattern}"))
            .unwrap_or_else(|| "glob files".to_string()),
        "fs_list" => args
            .get("path")
            .and_then(Value::as_str)
            .map(|path| format!("list {path}"))
            .unwrap_or_else(|| "list workspace".to_string()),
        "explore_batch" => {
            let goal = args
                .get("goal")
                .and_then(Value::as_str)
                .map(|goal| compact(goal, 80))
                .unwrap_or_else(|| "workspace context".to_string());
            let count = args
                .get("probes")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("explore {goal} · {count} probes")
        }
        "terminal_exec" => args
            .get("command")
            .and_then(Value::as_str)
            .map(|command| format!("$ {command}"))
            .unwrap_or_else(|| "run command".to_string()),
        "workflow_run" => args
            .get("goal")
            .and_then(Value::as_str)
            .map(|goal| format!("workflow: {}", compact(goal, 80)))
            .unwrap_or_else(|| "run workflow script".to_string()),
        "file_edit" => args
            .get("path")
            .and_then(Value::as_str)
            .map(|path| format!("edit {path}"))
            .unwrap_or_else(|| "edit file".to_string()),
        "file_patch" => {
            let description = args
                .get("description")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let target = args
                .get("diff")
                .and_then(Value::as_str)
                .and_then(first_patch_path);

            match (target, description) {
                (Some(target), Some(description)) => format!("{target} - {description}"),
                (Some(target), None) => target,
                (None, Some(description)) => description.to_string(),
                (None, None) => "apply patch".to_string(),
            }
        }
        "task_update" => args
            .get("status")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| "update status".to_string()),
        "plan_update" => {
            let count = args
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let summary = args
                .get("summary")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(|value| compact(value, 80));
            match summary {
                Some(summary) => format!("plan {count} steps · {summary}"),
                None => format!("plan {count} steps"),
            }
        }
        "decision_request" => {
            let count = args
                .get("questions")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let title = args
                .get("title")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(|value| compact(value, 80));
            match title {
                Some(title) => format!("decision queue · {count} question(s) · {title}"),
                None => format!("decision queue · {count} question(s)"),
            }
        }
        "question" => args
            .get("question")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| "ask user".to_string()),
        other => other.to_string(),
    }
}

fn first_patch_path(diff: &str) -> Option<String> {
    for line in diff.lines() {
        if let Some(path) = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
        {
            return Some(path.trim().to_string());
        }

        if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut parts = rest.split_whitespace();
            let _old = parts.next();
            if let Some(new) = parts.next() {
                return Some(new.trim_start_matches("b/").to_string());
            }
        }

        if let Some(rest) = line.strip_prefix("+++ b/") {
            return Some(rest.to_string());
        }
    }

    None
}

pub(crate) fn compact_tool_context_output(call: &ToolCall, execution: &ToolExecution) -> String {
    if execution.failed {
        return compact(&execution.output, 6000);
    }

    match call.name.as_str() {
        "file_read" | "file_search" | "file_glob" | "fs_list" | "explore_batch" => {
            compact(&execution.output, 20_000)
        }
        "terminal_exec" => compact_terminal_context_output(&execution.output, 6000),
        "workflow_run" => compact(&execution.output, 8000),
        // Short confirmations only: the model already produced the edit content,
        // so echoing a diff back would just duplicate tokens in context. The
        // verify: block DOES go back — breakage feedback is the whole point.
        "file_edit" => {
            let (base, verify) = split_verify_section(&execution.output);
            let mut context = summarize_file_edit_output(base);
            if let Some(block) = verify {
                context.push('\n');
                context.push_str(block);
            }
            context
        }
        "file_patch" => {
            let (base, verify) = split_verify_section(&execution.output);
            let mut context = summarize_file_patch_output(base);
            if let Some(block) = verify {
                context.push('\n');
                context.push_str(block);
            }
            context
        }
        "task_update" => execution.output.clone(),
        "plan_update" => execution.output.clone(),
        "decision_request" => execution.output.clone(),
        "question" => execution.output.clone(),
        _ => compact(&execution.output, 4000),
    }
}

pub(crate) fn summarize_tool_result(call: &ToolCall, execution: &ToolExecution) -> String {
    if execution.failed {
        return format!("failed • {}", compact(&execution.output, 500));
    }

    match call.name.as_str() {
        "file_read" => summarize_file_read_output(&execution.output),
        "file_search" => summarize_file_search_output(&execution.output),
        "file_glob" => summarize_file_glob_output(&execution.output),
        "fs_list" => summarize_fs_list_output(&execution.output),
        "explore_batch" => summarize_explore_batch_output(&execution.output),
        "terminal_exec" => summarize_terminal_output(&execution.output),
        "file_edit" => {
            let (base, verify) = split_verify_section(&execution.output);
            compose_mutation_summary(
                summarize_file_edit_output(base),
                verify,
                file_edit_display_diff(call),
            )
        }
        "file_patch" => {
            let (base, verify) = split_verify_section(&execution.output);
            compose_mutation_summary(
                summarize_file_patch_output(base),
                verify,
                file_patch_display_diff(call),
            )
        }
        "workflow_run" => execution
            .output
            .lines()
            .next()
            .unwrap_or("workflow completed")
            .to_string(),
        "task_update" => execution.output.clone(),
        "plan_update" => execution.output.clone(),
        "decision_request" => execution.output.clone(),
        "question" => execution.output.clone(),
        _ => compact(&execution.output, 500),
    }
}

/// Transcript layout for a mutation: verify status rides the headline,
/// failure details come before the diff (breakage first), diff last.
fn compose_mutation_summary(
    base_summary: String,
    verify: Option<&str>,
    diff: Option<String>,
) -> String {
    let mut summary = base_summary;
    let mut details = None;
    if let Some(block) = verify {
        let mut lines = block.lines();
        if let Some(status) = lines.next() {
            summary.push_str(" · ");
            summary.push_str(status);
        }
        let rest = lines.collect::<Vec<_>>().join("\n");
        if !rest.trim().is_empty() {
            details = Some(rest);
        }
    }
    if let Some(details) = details {
        summary.push('\n');
        summary.push_str(&details);
    }
    if let Some(diff) = diff {
        summary.push('\n');
        summary.push_str(&diff);
    }
    summary
}

fn summarize_file_edit_output(output: &str) -> String {
    output
        .strip_prefix("edited files:\n")
        .map(|rest| {
            let path = rest.lines().next().unwrap_or("file");
            let replacements = rest
                .lines()
                .find_map(|line| line.strip_prefix("replacements: "))
                .unwrap_or("1");
            format!(
                "edited {path} ({replacements} replacement{})",
                if replacements == "1" { "" } else { "s" }
            )
        })
        .unwrap_or_else(|| compact(output, 500))
}

fn summarize_file_patch_output(output: &str) -> String {
    output
        .strip_prefix("patched files:\n")
        .map(|files| format!("patched {}", files.lines().collect::<Vec<_>>().join(", ")))
        .unwrap_or_else(|| compact(output, 500))
}

/// Maximum diff lines shown in the transcript before folding the rest.
const DISPLAY_DIFF_MAX_LINES: usize = 60;

/// Unified diff of a file_edit's old/new strings, for the transcript only —
/// never sent back to the model.
fn file_edit_display_diff(call: &ToolCall) -> Option<String> {
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let old = string_arg(&args, "oldString", "old_string").unwrap_or_default();
    let new = string_arg(&args, "newString", "new_string")?;
    let diff = similar::TextDiff::from_lines(old, new);

    let mut lines = Vec::new();
    for hunk in diff.unified_diff().context_radius(2).iter_hunks() {
        for change in hunk.iter_changes() {
            let sign = match change.tag() {
                similar::ChangeTag::Insert => '+',
                similar::ChangeTag::Delete => '-',
                similar::ChangeTag::Equal => ' ',
            };
            lines.push(format!(
                "{sign} {}",
                change.value().trim_end_matches('\n')
            ));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(cap_display_diff(lines))
}

/// The patch body a file_patch call applied, cleaned up for the transcript.
fn file_patch_display_diff(call: &ToolCall) -> Option<String> {
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    let diff = args.get("diff").and_then(Value::as_str)?;
    let lines: Vec<String> = diff
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.is_empty()
                && !trimmed.starts_with("```")
                && !trimmed.starts_with("diff --git")
                && !trimmed.starts_with("index ")
        })
        .map(|line| {
            // Match file_edit diff formatting: a space after the sign column.
            match line.as_bytes().first() {
                Some(b'+') if !line.starts_with("+++") => format!("+ {}", &line[1..]),
                Some(b'-') if !line.starts_with("---") => format!("- {}", &line[1..]),
                _ => line.to_string(),
            }
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(cap_display_diff(lines))
}

fn cap_display_diff(mut lines: Vec<String>) -> String {
    if lines.len() > DISPLAY_DIFF_MAX_LINES {
        let hidden = lines.len() - DISPLAY_DIFF_MAX_LINES;
        lines.truncate(DISPLAY_DIFF_MAX_LINES);
        lines.push(format!("… +{hidden} more diff lines"));
    }
    lines.join("\n")
}

fn summarize_file_read_output(output: &str) -> String {
    let count = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("read files: "))
        .unwrap_or("0");
    let first_path = output
        .lines()
        .nth(1)
        .and_then(|line| line.split(':').next())
        .unwrap_or("files");
    format!("read {count} • {}", compact(first_path, 160))
}

fn summarize_file_search_output(output: &str) -> String {
    let query = output
        .lines()
        .find_map(|line| line.strip_prefix("query: "))
        .unwrap_or("");
    let matches = output
        .lines()
        .find_map(|line| line.strip_prefix("matches: "))
        .unwrap_or("0");
    if query.is_empty() {
        format!("matches {matches}")
    } else {
        format!("matches {matches} • {query:?}")
    }
}

fn summarize_file_glob_output(output: &str) -> String {
    let pattern = output
        .lines()
        .find_map(|line| line.strip_prefix("pattern: "))
        .unwrap_or("");
    let matches = output
        .lines()
        .find_map(|line| line.strip_prefix("matches: "))
        .unwrap_or("0");
    if pattern.is_empty() {
        format!("matched {matches}")
    } else {
        format!("matched {matches} • {pattern}")
    }
}

fn summarize_fs_list_output(output: &str) -> String {
    let root = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("root: "))
        .unwrap_or(".");
    let entries = output
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .count();
    format!("listed {entries} • {}", compact(root, 160))
}

fn summarize_explore_batch_output(output: &str) -> String {
    let probes = output
        .lines()
        .find_map(|line| line.strip_prefix("probes: "))
        .unwrap_or("completed");
    format!("evidence • {}", compact(probes, 180))
}

fn summarize_terminal_output(output: &str) -> String {
    let exit = output
        .lines()
        .next()
        .unwrap_or("exit: unknown")
        .trim()
        .to_string();

    let stdout = section_after(output, "stdout:", Some("stderr:")).unwrap_or_default();
    let stderr = section_after(output, "stderr:", None).unwrap_or_default();
    let combined = if stderr.trim().is_empty() {
        &stdout
    } else {
        &stderr
    };

    let preview = summarize_command_text(&strip_ansi(combined))
        .unwrap_or_else(|| "no output".to_string());
    let mut result = format!("{exit} • {}", compact(&preview, 240));

    // Tail of the raw output for the transcript's expanded view. ANSI codes
    // stay intact here — the UI renders them as colors; the model context
    // path strips them instead.
    let tail = terminal_tail_lines(combined, 40);
    if tail.len() > 1 || tail.first().map(String::as_str) != Some(preview.as_str()) {
        for line in tail {
            result.push('\n');
            result.push_str(&line);
        }
    }
    result
}

/// The last `limit` non-empty output lines, preserving indentation.
fn terminal_tail_lines(text: &str, limit: usize) -> Vec<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && *line != "<empty>")
        .collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(limit))
        .map(ToString::to_string)
        .collect()
}

/// Remove ANSI escape sequences (colors, cursor movement, OSC titles).
pub(crate) fn strip_ansi(text: &str) -> String {
    static ANSI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = ANSI.get_or_init(|| {
        regex::Regex::new(r"\x1b(?:\[[0-9;?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])")
            .expect("ANSI regex compiles")
    });
    re.replace_all(text, "").into_owned()
}

fn compact_terminal_context_output(output: &str, max_chars: usize) -> String {
    let exit = output.lines().next().unwrap_or("exit: unknown").trim();
    let stdout = section_after(output, "stdout:", Some("stderr:")).unwrap_or_default();
    let stderr = section_after(output, "stderr:", None).unwrap_or_default();
    let source = if stderr.trim().is_empty() {
        &stdout
    } else {
        &stderr
    };

    let mut result = String::new();
    result.push_str(exit);
    if let Some(summary) = summarize_command_text(source) {
        result.push_str("\nsummary: ");
        result.push_str(&summary);
    }

    let important = important_output_lines(source, 80);
    if !important.is_empty() {
        result.push_str("\npreview:\n");
        result.push_str(&important.join("\n"));
    }

    // Escape codes are pure token waste in model context.
    compact(&strip_ansi(&result), max_chars)
}

fn section_after(output: &str, marker: &str, until: Option<&str>) -> Option<String> {
    let mut in_section = false;
    let mut lines = Vec::new();
    for line in output.lines() {
        if line == marker {
            in_section = true;
            continue;
        }
        if in_section && until.is_some_and(|end| line == end) {
            break;
        }
        if in_section {
            lines.push(line);
        }
    }

    if lines.is_empty() || lines == ["<empty>"] {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn summarize_command_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "<empty>" {
        return None;
    }

    for line in trimmed
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.contains(" passed") && (line.contains("test result:") || line.contains("passed;")) {
            return Some(line.to_string());
        }
        if line.contains("error") || line.contains("failed") || line.contains("panicked") {
            return Some(line.to_string());
        }
    }

    let count = trimmed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    match count {
        0 => None,
        1 => Some(
            trimmed
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
        _ => Some(format!("{count} lines")),
    }
}

fn important_output_lines(text: &str, limit: usize) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "<empty>")
        .filter(|line| {
            line.starts_with("error")
                || line.starts_with("warning")
                || line.contains("failed")
                || line.contains("panicked")
                || line.contains("test result:")
                || line.contains(" passed")
        })
        .take(8)
        .map(|line| compact(line, limit))
        .collect()
}

pub(crate) fn display_tool_name(name: &str) -> &str {
    match name {
        "file_read" => "file.read",
        "file_search" => "file.search",
        "file_glob" => "file.glob",
        "fs_list" => "fs.list",
        "explore_batch" => "explore.batch",
        "terminal_exec" => "terminal.exec",
        "file_edit" => "file.edit",
        "file_patch" => "file.patch",
        "workflow_run" => "workflow.run",
        "task_update" => "task.update",
        "plan_update" => "plan.update",
        "decision_request" => "decision.request",
        "question" => "question",
        other => other,
    }
}

pub(crate) fn compact(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let compacted = chars.by_ref().take(max_chars).collect::<String>();

    if chars.next().is_some() {
        format!("{compacted}...")
    } else {
        compacted
    }
}

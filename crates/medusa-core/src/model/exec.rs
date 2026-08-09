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
    ToolRuntime, validate_read_only_terminal_command,
};

/// Tools that neither mutate the workspace nor consult [`ToolLoopState`] —
/// safe to execute concurrently within one turn.
pub(crate) fn tool_call_is_read_only(name: &str) -> bool {
    matches!(
        name,
        "file_read"
            | "file_search"
            | "file_glob"
            | "fs_list"
            | "explore_batch"
            | "web_fetch"
            | "web_search"
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
        "web_fetch" => "web.fetch",
        "web_search" => "web.search",
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

mod calls;
mod dispatch;
mod lifecycle;
mod output;

pub(crate) use calls::*;
pub(crate) use dispatch::*;
pub(crate) use lifecycle::*;
pub(crate) use output::*;

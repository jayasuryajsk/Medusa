use super::*;

pub(crate) fn update_tool_loop_state(
    state: &mut ToolLoopState,
    call: &ToolCall,
    execution: &ToolExecution,
) {
    let changed_files = if execution.failed {
        Vec::new()
    } else {
        mutation_changed_files(&execution.output)
    };
    state.orchestrator.record_execution(
        &call.name,
        &summarize_tool_call(call),
        &execution.output,
        execution.failed,
        &changed_files,
    );

    match call.name.as_str() {
        "file_edit" | "file_patch" if execution.failed => {
            state.patch_requires_context = true;
        }
        "file_edit" | "file_patch" => {
            state.patch_requires_context = false;
        }
        "file_read" | "file_search" | "semantic_search" | "file_glob" | "fs_list"
        | "explore_batch" | "terminal_exec" | "workflow_run" => {
            state.patch_requires_context = false;
        }
        _ => {}
    }
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
        "semantic_search" => args
            .get("query")
            .and_then(Value::as_str)
            .map(|query| format!("find code about {query:?}"))
            .unwrap_or_else(|| "search code by meaning".to_string()),
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
        "terminal_exec" => {
            let unsandboxed = args.get("sandbox").and_then(Value::as_bool) == Some(false);
            args.get("command")
                .and_then(Value::as_str)
                .map(|command| {
                    if unsandboxed {
                        format!("$ {command} · unsandboxed")
                    } else {
                        format!("$ {command}")
                    }
                })
                .unwrap_or_else(|| "run command".to_string())
        }
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
        "web_fetch" => args
            .get("url")
            .and_then(Value::as_str)
            .map(|url| format!("fetch {}", compact(url, 120)))
            .unwrap_or_else(|| "fetch url".to_string()),
        "web_search" => args
            .get("query")
            .and_then(Value::as_str)
            .map(|query| format!("web {query:?}"))
            .unwrap_or_else(|| "search the web".to_string()),
        // Namespaced MCP tools (dynamic names; prefix check is display-only —
        // dispatch resolves through the registry map).
        other if other.starts_with("mcp_") => {
            let preview = compact(call.arguments.trim(), 100);
            if preview.is_empty() || preview == "{}" {
                other.to_string()
            } else {
                format!("{other} {preview}")
            }
        }
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

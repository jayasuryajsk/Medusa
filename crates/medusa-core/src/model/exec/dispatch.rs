use super::*;

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

    if call.name == "terminal_exec"
        && (!tool_policy.allow_mutation() || !state.native_mutation_allowed())
    {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Err(error) = validate_read_only_terminal_command(command, tools.workspace()) {
            return ToolExecution {
                failed: true,
                output: format!(
                    "error: terminal.exec is restricted to read-only inspection and verification \
commands until the current harness route permits mutation: {error}"
                ),
            };
        }
    }

    // MCP tools are resolved through the registry's full-name map, never by
    // parsing the namespaced string. They may have side effects, so they run
    // as serial barriers (deliberately absent from tool_call_is_read_only)
    // and are unavailable to read-only turns unless the user marked the
    // server "readOnly": true.
    if let Some((server, tool)) = tools.mcp_lookup(&call.name) {
        return execute_mcp_tool(tools, &call.name, &server, &tool, &args, tool_policy);
    }

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
        "web_fetch" => execute_web_fetch(tools, &args),
        "web_search" => execute_web_search(tools, &args),
        other => ToolExecution {
            failed: true,
            output: format!("error: unknown Medusa tool: {other}"),
        },
    }
}

fn execute_mcp_tool(
    tools: &ToolRuntime,
    namespaced: &str,
    server: &str,
    tool: &str,
    args: &Value,
    tool_policy: ToolLoopPolicy,
) -> ToolExecution {
    let read_only_server = tools
        .mcp()
        .is_some_and(|registry| registry.server_marked_read_only(server));
    if !tool_policy.allow_mutation() && !read_only_server {
        return ToolExecution {
            failed: true,
            output: format!(
                "error: {namespaced} is unavailable here: MCP tools may have side effects and are disabled for read-only turns (mark the server \"readOnly\": true in .medusa/mcp.json if it is safe)"
            ),
        };
    }

    match tools.mcp_call(namespaced, args) {
        Ok(outcome) if outcome.is_error => ToolExecution {
            failed: true,
            output: format!(
                "error: MCP tool {server}:{tool} reported an error:\n{}",
                outcome.text
            ),
        },
        Ok(outcome) => ToolExecution {
            failed: false,
            output: outcome.text,
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
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
        unsandboxed: args.get("sandbox").and_then(Value::as_bool) == Some(false),
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
            let failed = result.code != Some(0);
            let mut output = format!("exit: {}\n", result.code.unwrap_or(-1));
            // Harness notes live between the exit line and the stdout marker
            // so the stdout/stderr sections stay pure command output.
            if result.sandboxed && failed {
                output.push_str(crate::sandbox::execution_note());
                if crate::sandbox::looks_sandbox_denied(&result.stderr, result.code) {
                    output.push_str(
                        "hint: if the sandbox caused this failure, retry with \"sandbox\": false and explain why; the user must approve every unsandboxed run\n",
                    );
                }
            }
            if !result.sandboxed
                && tools.sandbox_policy().should_sandbox()
                && let Some(notice) = crate::sandbox::take_unavailability_notice()
            {
                output.push_str(&format!("note: {notice}\n"));
            }
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
            ToolExecution { failed, output }
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

fn execute_web_fetch(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(url) = args.get("url").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: web_fetch.url is required".to_string(),
        };
    };

    let request = crate::web::WebFetchRequest {
        url: url.to_string(),
        max_chars: optional_usize(args, "max_chars"),
    };

    // Routed through the runtime so outbound egress passes the permission gate
    // (auto-allowed in Open, approval-gated in Guarded/Ask/Readonly).
    match tools.web_fetch(request) {
        Ok(output) => ToolExecution {
            failed: false,
            output,
        },
        Err(error) => ToolExecution {
            failed: true,
            output: format!("error: {error}"),
        },
    }
}

fn execute_web_search(tools: &ToolRuntime, args: &Value) -> ToolExecution {
    let Some(query) = args.get("query").and_then(Value::as_str) else {
        return ToolExecution {
            failed: true,
            output: "error: web_search.query is required".to_string(),
        };
    };

    let request = crate::web::WebSearchRequest {
        query: query.to_string(),
        count: optional_usize(args, "count"),
    };

    match tools.web_search(request) {
        Ok(output) => ToolExecution {
            failed: false,
            output,
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

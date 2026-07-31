use super::*;

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

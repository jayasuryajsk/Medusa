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

fn empty_host() -> Rc<RefCell<ScriptHost>> {
    let (events, _receiver) = std::sync::mpsc::channel();
    Rc::new(RefCell::new(ScriptHost {
        run_id: workflow_run_id(),
        events,
        runner: echo_runner(),
        agents: AgentRegistry::default(),
        max_agents: 4,
        max_parallel: 2,
        total_agents: 0,
        phase_open: false,
        phase_name: String::new(),
        phase_agents: Vec::new(),
        finished_phases: Vec::new(),
    }))
}

#[test]
fn runaway_javascript_is_interrupted_by_deadline() {
    let started = Instant::now();
    let error = eval_workflow_script_with_limits(
        "while (true) {}",
        None,
        empty_host(),
        CancelToken::default(),
        Some(Duration::from_millis(50)),
        16 * 1024 * 1024,
    )
    .unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(error.to_string().contains("execution deadline"));
}

#[test]
fn runaway_javascript_observes_turn_cancellation() {
    let cancel = CancelToken::new();
    cancel.cancel();
    let error = eval_workflow_script_with_limits(
        "while (true) {}",
        None,
        empty_host(),
        cancel,
        None,
        16 * 1024 * 1024,
    )
    .unwrap_err();

    assert!(error.to_string().contains("interrupted by user"));
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
fn parallel_rejects_mutating_agents_to_enforce_single_writer() {
    let source = r#"
            return parallel([
                { prompt: "inspect", tools: "read" },
                { prompt: "edit the file", tools: "edit" },
            ]);
        "#;

    let (report, _) = run(source, None, echo_runner()).unwrap();

    assert_eq!(report.status, WorkflowStatus::Failed);
    assert!(report.summary.contains("single writer"));
    assert!(report.summary.contains("sequentially"));
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

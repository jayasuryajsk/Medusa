use super::*;

#[test]
fn task_update_trims_status() {
    let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

    let result = runtime
        .task_update(TaskUpdateRequest::new("  running tests  "))
        .unwrap();

    assert_eq!(result.status, "running tests");
}

#[test]
fn plan_update_normalizes_status_and_rejects_multiple_active_steps() {
    let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

    let result = runtime
        .plan_update(PlanUpdateRequest {
            summary: Some("  Ship plan view  ".to_string()),
            items: vec![
                PlanUpdateItem {
                    text: "  inspect current TUI  ".to_string(),
                    status: "completed".to_string(),
                    evidence: vec![" main.rs ".to_string()],
                },
                PlanUpdateItem {
                    text: "render plan modal".to_string(),
                    status: "in-progress".to_string(),
                    evidence: Vec::new(),
                },
            ],
        })
        .unwrap();

    assert_eq!(result.summary, "Ship plan view");
    assert_eq!(result.items[0].status, "done");
    assert_eq!(result.items[1].status, "active");
    assert_eq!(result.items[0].evidence, vec!["main.rs"]);

    let error = runtime
        .plan_update(PlanUpdateRequest {
            summary: None,
            items: vec![
                PlanUpdateItem {
                    text: "one".to_string(),
                    status: "active".to_string(),
                    evidence: Vec::new(),
                },
                PlanUpdateItem {
                    text: "two".to_string(),
                    status: "doing".to_string(),
                    evidence: Vec::new(),
                },
            ],
        })
        .unwrap_err();

    assert!(error.to_string().contains("at most one"));
}

#[test]
fn question_trims_and_limits_text() {
    let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

    let result = runtime
        .question(QuestionRequest::new("  Which branch should I keep?  "))
        .unwrap();

    assert_eq!(result.question, "Which branch should I keep?");
}

#[test]
fn decision_request_normalizes_questions_and_requires_choice_options() {
    let runtime = ToolRuntime::new(std::env::current_dir().unwrap()).unwrap();

    let result = runtime
        .decision_request(DecisionRequest {
            title: Some("  Choose storage  ".to_string()),
            reason: Some("  Plan changes persistence.  ".to_string()),
            questions: vec![
                DecisionQuestionRequest {
                    id: "Storage Model".to_string(),
                    prompt: "Where should plans live?".to_string(),
                    kind: "single_choice".to_string(),
                    options: vec![" transcript ".to_string(), " plan file ".to_string()],
                    recommended: Some(" transcript ".to_string()),
                    required: true,
                },
                DecisionQuestionRequest {
                    id: "Storage Model".to_string(),
                    prompt: "Any naming note?".to_string(),
                    kind: "free text".to_string(),
                    options: Vec::new(),
                    recommended: None,
                    required: false,
                },
            ],
            assumptions: vec!["  Default to transcript.  ".to_string()],
        })
        .unwrap();

    assert_eq!(result.title, "Choose storage");
    assert_eq!(result.reason, "Plan changes persistence.");
    assert_eq!(result.questions[0].id, "storage_model");
    assert_eq!(result.questions[1].id, "storage_model_2");
    assert_eq!(result.questions[0].kind, "choice");
    assert_eq!(result.questions[1].kind, "text");
    assert_eq!(result.questions[0].options, vec!["transcript", "plan file"]);
    assert_eq!(
        result.questions[0].recommended.as_deref(),
        Some("transcript")
    );
    assert_eq!(result.assumptions, vec!["Default to transcript."]);

    let error = runtime
        .decision_request(DecisionRequest {
            title: None,
            reason: None,
            questions: vec![DecisionQuestionRequest {
                id: "missing".to_string(),
                prompt: "Choose?".to_string(),
                kind: "choice".to_string(),
                options: Vec::new(),
                recommended: None,
                required: true,
            }],
            assumptions: Vec::new(),
        })
        .unwrap_err();

    assert!(error.to_string().contains("options is required"));
}

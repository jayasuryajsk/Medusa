use super::*;

#[test]
fn mutation_tools_gated_by_policy_not_turn_mode() {
    let mutation_tools = schema::medusa_tools(true, true, &[]);
    let read_only_tools = schema::medusa_tools(false, false, &[]);

    assert!(
        mutation_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("question")))
    );
    assert!(
        mutation_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_patch")))
    );
    assert!(
        mutation_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_edit")))
    );
    assert!(
        !read_only_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_patch")))
    );
    assert!(
        !read_only_tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("file_edit")))
    );
}

#[test]
fn medusa_instructions_include_explicit_skill_context() {
    let instructions = schema::medusa_instructions(
        Path::new("/workspace"),
        &types::ToolLoopState::default(),
        HarnessPolicy::for_user_prompt("use $review"),
        Some("Active Medusa skills.\n<skill name=\"review\">lead with findings</skill>"),
        false,
    );

    assert!(instructions.contains("Active Medusa skills"));
    assert!(instructions.contains("lead with findings"));
}

#[test]
fn medusa_instructions_mention_mcp_only_when_mcp_tools_are_active() {
    let base = |mcp_active| {
        schema::medusa_instructions(
            Path::new("/workspace"),
            &types::ToolLoopState::default(),
            HarnessPolicy::for_user_prompt("hello"),
            None,
            mcp_active,
        )
    };

    assert!(base(true).contains("mcp_<server>_<tool>"));
    assert!(!base(false).contains("mcp_<server>_<tool>"));
}

#[test]
fn context_compaction_keeps_recent_messages() {
    let messages = vec![
        types::ConversationMessage {
            role: "user".to_string(),
            content: "old old old old old".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "assistant".to_string(),
            content: "middle middle middle".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "user".to_string(),
            content: "latest".to_string(),
            attachments: Vec::new(),
        },
    ];

    let compacted = wire::compact_conversation_context(&messages, 10);

    assert!(compacted[0].content.contains("context compaction omitted"));
    assert_eq!(compacted.last().unwrap().content, "latest");
    assert!(
        compacted
            .iter()
            .all(|message| message.content != "old old old old old")
    );
}

#[test]
fn context_compaction_preserves_system_state_messages() {
    let messages = vec![
        types::ConversationMessage {
            role: "system".to_string(),
            content: "permissions".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "system".to_string(),
            content: "rolling session state".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "user".to_string(),
            content: "old old old old old".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "assistant".to_string(),
            content: "middle middle middle".to_string(),
            attachments: Vec::new(),
        },
        types::ConversationMessage {
            role: "user".to_string(),
            content: "latest".to_string(),
            attachments: Vec::new(),
        },
    ];

    let compacted = wire::compact_conversation_context(&messages, 10);

    assert_eq!(compacted[0].content, "permissions");
    assert_eq!(compacted[1].content, "rolling session state");
    assert!(
        compacted
            .iter()
            .any(|message| message.content.contains("context compaction omitted"))
    );
    assert_eq!(compacted.last().unwrap().content, "latest");
}

#[test]
fn question_tool_returns_user_facing_question() {
    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "question".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"question":"Which branch should I keep?"}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(!execution.failed);
    assert!(execution.output.contains("Which branch should I keep?"));
}

#[test]
fn decision_request_tool_returns_structured_queue() {
    let workspace = temp_workspace();
    let tools = ToolRuntime::new(&workspace).unwrap();
    let call = types::ToolCall {
        name: "decision_request".to_string(),
        call_id: "call_test".to_string(),
        arguments: r#"{"title":"Choose UI","reason":"The layout affects planning flow.","questions":[{"id":"rail","prompt":"Show decisions in the right rail?","kind":"choice","options":["yes","no"],"recommended":"yes","required":true}],"assumptions":["Use the right rail if unanswered."]}"#.to_string(),
        reasoning_content: None,
    };

    let execution = exec::execute_tool_call(
        &tools,
        &call,
        &types::ToolLoopState::default(),
        types::ToolLoopPolicy::mutation_allowed(),
    );

    assert!(!execution.failed);
    assert!(execution.output.contains("\"title\":\"Choose UI\""));
    assert!(execution.output.contains("\"id\":\"rail\""));
    assert!(execution.output.contains("\"recommended\":\"yes\""));
}

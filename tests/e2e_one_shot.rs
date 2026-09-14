//! End-to-end one-shot acceptance test: a mocked Anthropic server drives the
//! full pipeline (system prompt assembly → streaming → tool execution →
//! persistence-free one-shot run), mirroring BEHAVIOR.md §1/§3/§11/§12.

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use nanocode::core::types::{
    PermissionDecision, PermissionGate, PermissionMode,
};

const TEXT_THEN_TOOL_SSE: &str = concat!(
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me check the file."}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_e2e1","name":"Read"}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\": \"target.txt\"}"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":1}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":25}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

const FINAL_TEXT_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"usage":{"input_tokens":1234,"cache_read_input_tokens":100}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"The file says hello."}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

async fn setup_server(first_body: String, rest_body: String) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    // Phase 2 (mounted first; LIFO matched last): requests whose message
    // history contains a tool_result → final text response.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains("tool_result"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(rest_body),
        )
        .mount(&server)
        .await;
    // Phase 1 (LIFO priority): the initial request → tool_use response.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(first_body),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn one_shot_tool_round_trip_end_to_end() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("target.txt"), "hello\n").unwrap();
    let server = setup_server(TEXT_THEN_TOOL_SSE.into(), FINAL_TEXT_SSE.into()).await;

    // Point the client at the mock server.
    std::env::set_var("ANTHROPIC_BASE_URL", server.uri());
    std::env::set_var("ANTHROPIC_API_KEY", "e2e-key");

    let mut state = nanocode::cli::session::SessionState::new(
        work.path().to_path_buf(),
        "sonnet",
        "e2e-key",
        nanocode::core::api::ModelProvider::Anthropic,
        PermissionMode::BypassPermissions,
        false, // headless → allow-all gate
        false, // no persistence
    );
    state.session_id = "e2e-test".into();
    state
        .messages
        .lock()
        .unwrap()
        .push(nanocode::core::types::Message::user_text("read target.txt"));

    state.run_agent().await;

    // Pipeline assertions (BEHAVIOR §3, §7 Read, §12):
    let msgs = state.messages.lock().unwrap();
    // user → assistant(text+tool_use) → user(tool_result) → assistant(final)
    assert_eq!(msgs.len(), 4, "message history: {msgs:?}");

    // cost tracker accumulated usage from both calls
    assert!(state.cost_tracker().turns >= 1);

    // Read tool actually read the file through the full stack
    match &msgs[2].content[0] {
        nanocode::core::types::ContentBlock::ToolResult { content, is_error, .. } => {
            assert_eq!(*is_error, Some(false));
            match content {
                nanocode::core::types::ToolResultContent::Text(t) => {
                    assert!(t.contains("1\thello"), "read output: {t}");
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }

    // Final assistant message text
    assert_eq!(msgs[3].assistant_text(), "The file says hello.");

    // File-state cache populated by Read (read-before-edit chain)
    assert!(state
        .tool_context
        .file_state
        .lock()
        .unwrap()
        .has(&work.path().join("target.txt").to_string_lossy()));
}

#[tokio::test]
async fn e2e_permission_denial_round_trip() {
    let work = tempfile::tempdir().unwrap();
    let server = wiremock::MockServer::start().await;
    std::env::set_var("ANTHROPIC_BASE_URL", server.uri());
    std::env::set_var("ANTHROPIC_API_KEY", "e2e-key");

    // Turn 1: Bash tool_use; Turn 2: final
    let tool_turn = format!(
        "{}{}",
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t9","name":"Bash"}}"#,
    );
    let _ = tool_turn;
    // Use a scripted caller directly (headless, non-interactive):
    let sse_tool = concat!(
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t9","name":"Bash"}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"rm -rf /\"}"}}"#,
        "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
        "\n\n",
    );
    let sse_final = concat!(
        "event: content_block_start\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Denied, stopping."}}"#,
        "\n\n",
        "event: content_block_stop\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        "\n\n",
    );
    let _server = setup_server(sse_tool.into(), sse_final.into()).await;

    let mut state = nanocode::cli::session::SessionState::new(
        work.path().to_path_buf(),
        "sonnet",
        "e2e-key",
        nanocode::core::api::ModelProvider::Anthropic,
        PermissionMode::Default,
        false, // headless → AllowAllGate... but we want denial; use bypass=false
        false,
    );
    // Swap in a deny gate to simulate "user says no"
    struct DenyGate;
    #[async_trait]
    impl PermissionGate for DenyGate {
        async fn decide(&self, _: &str, _: &serde_json::Value, _: &str) -> PermissionDecision {
            PermissionDecision::deny("User denied")
        }
    }
    state.tool_context.permission_gate = Arc::new(DenyGate);
    state.session_id = "e2e-deny".into();
    state
        .messages
        .lock()
        .unwrap()
        .push(nanocode::core::types::Message::user_text("rm everything"));

    state.run_agent().await;

    let msgs = state.messages.lock().unwrap();
    assert_eq!(msgs.len(), 4);
    match &msgs[2].content[0] {
        nanocode::core::types::ContentBlock::ToolResult { is_error, .. } => {
            assert_eq!(*is_error, Some(true), "denied tool result is an error");
        }
        other => panic!("{other:?}"),
    }
}

// Silence unused import when std::io::Write unused in some cfgs
#[allow(dead_code)]
fn _w(_: &mut dyn Write) {}

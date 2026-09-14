//! Live E2E against the Zhipu BigModel Anthropic-compatible endpoint.
//! Skipped unless ZHIPU_LIVE_E2E=1 + key present — opt-in to avoid burning
//! quota on every `cargo test`.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use nanocode::core::types::PermissionMode;

fn live_enabled() -> bool {
    std::env::var("ZHIPU_LIVE_E2E").ok().as_deref() == Some("1")
}

fn zhipu_key() -> Option<String> {
    std::env::var("ZHIPU_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
}

async fn make_session(workdir: &std::path::Path, model: &str) -> nanocode::cli::session::SessionState {
    std::env::set_var("ANTHROPIC_BASE_URL", "https://open.bigmodel.cn/api/anthropic");
    let mut state = nanocode::cli::session::SessionState::new(
        workdir.to_path_buf(),
        model,
        &zhipu_key().unwrap_or_default(),
        nanocode::core::api::ModelProvider::Anthropic,
        PermissionMode::BypassPermissions,
        false,
        false,
    );
    state.session_id = "zhipu-live-e2e".into();
    state
}

#[tokio::test]
async fn zhipu_glm_flash_answers_directly() {
    if !live_enabled() || zhipu_key().is_none() {
        eprintln!("skipping: set ZHIPU_LIVE_E2E=1 + ZHIPU_API_KEY to run");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let mut state = make_session(work.path(), "GLM-5.3-Flash").await;
    state
        .messages
        .lock()
        .unwrap()
        .push(nanocode::core::types::Message::user_text(
            "用一句话回答：1+1等于几？不要调用任何工具。",
        ));

    state.run_agent().await;

    let msgs = state.messages.lock().unwrap();
    assert!(msgs.len() >= 2, "history: {msgs:?}");
    let final_text = msgs.last().unwrap().assistant_text();
    assert!(!final_text.is_empty(), "no assistant text: {msgs:?}");
    assert!(final_text.contains('2'), "answer should mention 2: {final_text}");
    assert!(state.cost_tracker().turns >= 1, "usage tracked");
}

#[tokio::test]
async fn zhipu_glm_flash_tool_round_trip() {
    if !live_enabled() || zhipu_key().is_none() {
        eprintln!("skipping: set ZHIPU_LIVE_E2E=1 + ZHIPU_API_KEY to run");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    // Seed a file the model must read via the Read tool
    let target = work.path().join("secret.txt");
    std::fs::write(&target, "MANGO-42\n").unwrap();

    let mut state = make_session(work.path(), "GLM-5.3-Flash").await;
    state
        .messages
        .lock()
        .unwrap()
        .push(nanocode::core::types::Message::user_text(format!(
            "使用 Read 工具读取文件 {} 并原样告诉我里面的内容（只回答内容本身）。",
            target.display()
        )));

    state.run_agent().await;

    let msgs = state.messages.lock().unwrap();
    // Expect: user → assistant(tool_use) → user(tool_result) → assistant(final)
    assert!(msgs.len() >= 4, "history: {msgs:?}");

    let has_read_call = msgs.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, nanocode::core::types::ContentBlock::ToolUse { name, .. } if name == "Read")
        })
    });
    assert!(has_read_call, "model should call Read tool");

    let tool_result_ok = msgs.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, nanocode::core::types::ContentBlock::ToolResult { content, is_error, .. }
                if *is_error != Some(true)
                    && match content {
                        nanocode::core::types::ToolResultContent::Text(t) => t.contains("MANGO-42"),
                        _ => false,
                    })
        })
    });
    assert!(tool_result_ok, "Read result should contain MANGO-42");

    let final_text = msgs.last().unwrap().assistant_text();
    assert!(
        final_text.to_uppercase().contains("MANGO"),
        "final answer should relay file content: {final_text}"
    );

    // Read-through populated the file-state cache (read-before-edit chain)
    assert!(state
        .tool_context
        .file_state
        .lock()
        .unwrap()
        .has(&target.to_string_lossy()));

    let _ = PathBuf::new();
    let _ = Arc::new(RwLock::new(0u8));
}

#[tokio::test]
async fn zhipu_error_classification_on_bad_key() {
    if !live_enabled() {
        return;
    }
    // Deliberately invalid key → 401 from the real endpoint
    std::env::set_var("ANTHROPIC_BASE_URL", "https://open.bigmodel.cn/api/anthropic");
    std::env::set_var("ANTHROPIC_API_KEY", "invalid-key-for-classification-test");

    let work = tempfile::tempdir().unwrap();
    let mut state = nanocode::cli::session::SessionState::new(
        work.path().to_path_buf(),
        "GLM-5.3-Flash",
        "invalid-key-for-classification-test",
        nanocode::core::api::ModelProvider::Anthropic,
        PermissionMode::BypassPermissions,
        false,
        false,
    );
    state.session_id = "zhipu-401".into();
    state
        .messages
        .lock()
        .unwrap()
        .push(nanocode::core::types::Message::user_text("hi"));

    // Should not hang or panic: error classified, loop returns promptly.
    state.run_agent().await;
    let msgs = state.messages.lock().unwrap();
    assert!(msgs.len() == 1, "no assistant message on auth failure: {msgs:?}");
}

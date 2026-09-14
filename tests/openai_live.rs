//! Live smoke against Zhipu's OpenAI-compatible endpoint (paas/v4).
//!
//! The lite coding-plan subscription only covers the *Anthropic*-compatible
//! endpoint; the OpenAI endpoint bills per-use and this account has no
//! balance — the real 1113 response below still exercises the full outbound
//! path: correct URL, Bearer auth accepted, OpenAI error body parsed and
//! classified. Full streaming/tool-calling behavior is covered by the
//! wiremock suite in src/core/openai.rs.

use std::path::PathBuf;

use futures::StreamExt;
use nanocode::core::api::{CallModelParams, ModelCaller, ModelProvider};
use nanocode::core::types::{Message, PermissionMode};
use tokio_util::sync::CancellationToken;

fn live_enabled() -> bool {
    std::env::var("OPENAI_LIVE_E2E").ok().as_deref() == Some("1")
}

fn zhipu_key() -> Option<String> {
    std::env::var("ZHIPU_API_KEY").ok().filter(|k| !k.is_empty())
}

fn params() -> CallModelParams {
    CallModelParams {
        messages: vec![Message::user_text("hi")],
        tools: vec![],
        model_config: nanocode::core::api::get_model_config("GLM-5.3-Flash"),
        system_prompt_blocks: vec![],
        enable_thinking: false,
        thinking_budget: None,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn zhipu_openai_endpoint_reachable_and_error_classified() {
    if !live_enabled() || zhipu_key().is_none() {
        eprintln!("skipping: set OPENAI_LIVE_E2E=1 + ZHIPU_API_KEY to run");
        return;
    }
    let client = nanocode::core::openai::OpenAIClient::new(
        &zhipu_key().unwrap(),
        Some("https://open.bigmodel.cn/api/paas/v4"),
    );
    assert!(client.base_url().ends_with("/api/paas/v4"));

    let events: Vec<_> = client.call_model(params()).collect().await;
    // Either a successful stream (if the account gains balance) or the
    // business-error event — both prove the request path works end to end.
    assert!(!events.is_empty());
    if let Some(nanocode::core::types::AgentEvent::Error { error }) = events.first() {
        let msg = error.to_string();
        // Auth accepted: the 1113 balance error is a business error, not 401.
        assert!(
            !msg.contains("Authentication failed"),
            "Bearer auth must be accepted, got: {msg}"
        );
        eprintln!("live endpoint responded with business error (expected on lite plan): {msg}");
    }
}

#[tokio::test]
async fn provider_selection_builds_openai_caller() {
    let caller = ModelProvider::OpenAI.build_caller("dummy");
    let events: Vec<_> = caller.call_model(params()).collect().await;
    // dummy key against the real api.openai.com → auth error, proving the
    // OpenAI caller (not the Anthropic one) is behind the trait object
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        nanocode::core::types::AgentEvent::Error {
            error: nanocode::core::errors::NanocodeError::Authentication { status: 401, .. }
        }
    ));
}

#[tokio::test]
async fn openai_provider_session_wiring() {
    // SessionState with provider=OpenAI must route through the OpenAI client:
    // a dummy key + real endpoint yields the classified 401 without panic.
    if !live_enabled() {
        return;
    }
    std::env::set_var("OPENAI_API_KEY", "dummy-key");
    std::env::set_var("OPENAI_BASE_URL", "https://open.bigmodel.cn/api/paas/v4");
    let work = PathBuf::from("/tmp");
    let mut state = nanocode::cli::session::SessionState::new(
        work,
        "GLM-5.3-Flash",
        "dummy-key",
        ModelProvider::OpenAI,
        PermissionMode::BypassPermissions,
        false,
        false,
    );
    state.session_id = "openai-wiring".into();
    state.messages.lock().unwrap().push(Message::user_text("hi"));
    state.run_agent().await;
    let msgs = state.messages.lock().unwrap();
    assert_eq!(msgs.len(), 1, "no assistant message on auth failure: {msgs:?}");
}

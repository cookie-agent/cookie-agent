//! An agent's questions to the user in the TUI (ACP elicitation,
//! `docs/site/specs/acp-agents.md` section 15): the docked question panel,
//! its keyboard, and the answers it sends.

use cookie_agent_client_state::elicitation::QuestionState;
use cookie_agent_protocol::{ElicitationId, ElicitationRequest, RunId, SessionId};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::*;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn form_request() -> ElicitationRequest {
    serde_json::from_value(serde_json::json!({
        "elicitation_id": ElicitationId::new_v7(),
        "agent": "claude",
        "message": "How should I go?",
        "form": {"fields": [
            {"name": "plan", "title": "Plan", "required": true, "input": {"type": "choice", "choices": [
                {"value": "safe", "title": "Safe", "description": "Small steps"},
                {"value": "fast", "title": "Fast"}]}},
            {"name": "note", "title": "Other", "description": "Your own answer",
             "input": {"type": "text", "max_length": 20}},
            {"name": "tags", "title": "Tags", "input": {"type": "multi_choice", "choices": [
                {"value": "x", "title": "X"}, {"value": "y", "title": "Y"}]}},
            {"name": "ok", "title": "Proceed", "input": {"type": "boolean"}}
        ]}
    }))
    .expect("a form question")
}

fn url_request() -> ElicitationRequest {
    serde_json::from_value(serde_json::json!({
        "elicitation_id": ElicitationId::new_v7(),
        "agent": "claude",
        "message": "Sign in to Example.",
        "url": "https://auth.example.com/connect?id=1"
    }))
    .expect("a URL question")
}

/// An app viewing a session with `request` waiting, recording what it
/// sends, past the answer grace.
async fn question_app(
    request: ElicitationRequest,
) -> (
    App,
    SessionId,
    std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    impl Sized,
) {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.store
        .sessions
        .entry(session)
        .or_default()
        .questions
        .push(QuestionState {
            session_id: session,
            run_id: Some(RunId::new_v7()),
            request,
        });
    rendered_frame(&mut app, 100, 40);
    app.arm_question_for_test();
    (app, session, recorded, incoming_guard)
}

#[tokio::test]
async fn a_form_question_docks_with_its_fields_and_takes_the_keyboard() {
    let (mut app, _, _, _guard) = question_app(form_request()).await;
    let rendered = rendered_frame(&mut app, 100, 40);
    for needle in [
        "claude asks · waiting on you",
        "How should I go?",
        "› Plan  required",
        "( ) Safe — Small steps",
        "( ) Fast",
        "Other  at most 20 characters",
        "Your own answer",
        "Tags  choose any",
        "[ ] X",
        "[ ] no",
        "✓ Submit",
        "✗ Decline",
        "Cancel (esc)",
    ] {
        assert!(rendered.contains(needle), "{needle}:\n{rendered}");
    }
    assert!(!app.composer_focused());
}

#[tokio::test]
async fn the_keyboard_fills_in_the_form_and_submit_sends_its_values() {
    let (mut app, session, recorded, _guard) = question_app(form_request()).await;
    // Plan: down to Fast and pick it, which moves on to Other.
    app.handle_key(key(KeyCode::Down)).await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.question_panel.focus, Some(QuestionFocus::Field(1)));
    for character in "soon".chars() {
        app.handle_key(key(KeyCode::Char(character))).await;
    }
    // Tags: tick Y. Proceed: flip it.
    app.handle_key(key(KeyCode::Tab)).await;
    app.handle_key(key(KeyCode::Down)).await;
    app.handle_key(key(KeyCode::Char(' '))).await;
    app.handle_key(key(KeyCode::Tab)).await;
    app.handle_key(key(KeyCode::Char(' '))).await;
    let rendered = rendered_frame(&mut app, 100, 40);
    for needle in ["(•) Fast", "[ soon ]", "[x] Y", "[x] yes"] {
        assert!(rendered.contains(needle), "{needle}:\n{rendered}");
    }
    app.handle_key(key(KeyCode::Tab)).await;
    assert_eq!(app.question_panel.focus, Some(QuestionFocus::Button(0)));
    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_method(&recorded, "elicitation.respond", 1).await;
    let params = last_request_params(&recorded, "elicitation.respond");
    assert_eq!(params["session_id"], session.to_string());
    assert_eq!(params["action"], "accept");
    assert_eq!(
        params["content"],
        serde_json::json!({"plan": "fast", "note": "soon", "tags": ["y"], "ok": true})
    );
}

#[tokio::test]
async fn a_required_field_left_empty_is_named_and_nothing_is_sent() {
    let (mut app, _, recorded, _guard) = question_app(form_request()).await;
    app.question_panel.focus = Some(QuestionFocus::Button(0));
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.question_panel.focus, Some(QuestionFocus::Field(0)));
    let rendered = rendered_frame(&mut app, 100, 40);
    assert!(rendered.contains("✗ Plan is required"), "{rendered}");
    assert_eq!(recorded_method_count(&recorded, "elicitation.respond"), 0);
}

#[tokio::test]
async fn a_url_question_shows_its_host_and_declines_or_cancels() {
    let (mut app, _, recorded, _guard) = question_app(url_request()).await;
    let rendered = rendered_frame(&mut app, 100, 40);
    for needle in [
        "Sign in to Example.",
        "Opens  auth.example.com",
        "https://auth.example.com/connect?id=1",
        "⧉ Copy link (c)",
        "✓ Done",
    ] {
        assert!(rendered.contains(needle), "{needle}:\n{rendered}");
    }
    // Focus starts on Copy link, never on the answer: Enter copies.
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.question_panel.notice.as_deref(), Some("link copied"));
    assert_eq!(recorded_method_count(&recorded, "elicitation.respond"), 0);
    app.handle_key(key(KeyCode::Right)).await;
    app.handle_key(key(KeyCode::Right)).await;
    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_method(&recorded, "elicitation.respond", 1).await;
    assert_eq!(
        last_request_params(&recorded, "elicitation.respond")["action"],
        "decline"
    );
}

#[tokio::test]
async fn esc_cancels_once_the_question_has_been_up_a_moment() {
    let mut app = test_app().await;
    let (client, recorded, _guard) = live_recording_client();
    app.client = client;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.store
        .sessions
        .entry(session)
        .or_default()
        .questions
        .push(QuestionState {
            session_id: session,
            run_id: None,
            request: url_request(),
        });
    rendered_frame(&mut app, 100, 40);
    // Just drawn: Esc meant for something else does not answer it.
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(
        app.question_panel.notice.as_deref(),
        Some("just appeared · press again")
    );
    assert_eq!(recorded_method_count(&recorded, "elicitation.respond"), 0);
    app.arm_question_for_test();
    app.handle_key(key(KeyCode::Esc)).await;
    wait_for_method(&recorded, "elicitation.respond", 1).await;
    assert_eq!(
        last_request_params(&recorded, "elicitation.respond")["action"],
        "cancel"
    );
}

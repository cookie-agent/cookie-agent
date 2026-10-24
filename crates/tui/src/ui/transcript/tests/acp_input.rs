//! ACP agents' model and effort in the TUI (`docs/site/specs/acp-agents.md`
//! section 15, input and pickers): the title shows them as the model and
//! variant, the model picker and the variant segment choose them, and a new
//! session's draft sends its choices with `session.create`.

use cookie_agent_protocol::{AcpConfigOption, AcpConfigValue, MessageFrame};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::*;
use crate::ui::slash::SlashCommand;

use super::acp::{acp_descriptor, acp_session_app};
use super::support::*;

fn select(id: &str, category: &str, current: &str, values: &[&str]) -> AcpConfigOption {
    AcpConfigOption {
        kind: Default::default(),
        id: id.into(),
        name: id.to_uppercase(),
        description: None,
        category: Some(category.into()),
        current_value: current.into(),
        values: values
            .iter()
            .map(|value| AcpConfigValue {
                value: (*value).into(),
                name: format!("{}!", value),
                description: None,
            })
            .collect(),
    }
}

fn model_and_effort() -> Vec<AcpConfigOption> {
    vec![
        select("mode", "mode", "auto", &["auto", "plan"]),
        select("model", "model", "sonnet", &["sonnet", "opus"]),
        select("effort", "thought_level", "low", &["low", "high"]),
    ]
}

#[tokio::test]
async fn a_new_session_draft_picks_the_agents_model_and_effort_and_sends_them() {
    let (mut app, _) = acp_session_app().await;
    let mut claude = acp_descriptor("claude", "local", true);
    claude.runtimes[0].options = model_and_effort();
    claude.runtime = claude.runtimes.first().cloned();
    app.agents[1] = claude;
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;
    app.run_command(SlashCommand::New).await;
    let index = app
        .filtered_agent_picker_candidates()
        .iter()
        .position(|agent| agent.id.as_str() == "claude")
        .unwrap();
    app.choose_picker_entry(index).await;
    assert_eq!(
        app.new_session_draft.as_ref().unwrap().model.model.as_str(),
        "acp/claude"
    );
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("claude • sonnet![low!]"), "{rendered}");

    // The model picker lists the model option's values.
    app.open_selection_modal(Modal::Models);
    assert_eq!(app.modal, Modal::AgentOptions);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("MODEL — claude"), "{rendered}");
    assert!(rendered.contains("sonnet! (current)"), "{rendered}");
    app.choose_picker_entry(1).await;
    assert_eq!(app.modal, Modal::None);
    // The variant segment cycles the effort option.
    app.cycle_draft_variant();
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("claude • opus![high!]"), "{rendered}");

    let recorded_for_response = recorded.clone();
    let response = tokio::spawn(async move {
        let id = wait_for_recorded_request(&recorded_for_response, "session.create", 1).await;
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32000, "message": "stop after capture"}
            })))
            .expect("script create response");
    });
    type_input(&mut app, "first message").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    response.await.expect("create response");
    let params = last_request_params(&recorded, "session.create");
    assert_eq!(params["selection"]["agent"], "claude");
    assert_eq!(
        params["agent_options"],
        serde_json::json!([
            {"option_id": "model", "value": "opus"},
            {"option_id": "effort", "value": "high"},
        ])
    );
}

#[tokio::test]
async fn a_draft_of_an_agent_that_never_started_says_where_its_options_come_from() {
    let (mut app, _) = acp_session_app().await;
    app.run_command(SlashCommand::New).await;
    let index = app
        .filtered_agent_picker_candidates()
        .iter()
        .position(|agent| agent.id.as_str() == "claude")
        .unwrap();
    app.choose_picker_entry(index).await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("claude • agent's model"), "{rendered}");
    app.open_selection_modal(Modal::Models);
    assert_eq!(app.modal, Modal::None);
    assert!(
        app.status.contains("once it has started a session"),
        "{}",
        app.status
    );
}

#[tokio::test]
async fn a_sessions_model_picker_sets_the_agents_model_option() {
    let (mut app, session) = acp_session_app().await;
    app.store.sessions.get_mut(&session).unwrap().acp.options = model_and_effort();
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    let index = app
        .filtered_agent_picker_candidates()
        .iter()
        .position(|agent| agent.id.as_str() == "claude")
        .unwrap();
    app.open_selection_modal(Modal::Agents);
    app.choose_picker_entry(index).await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("claude • sonnet![low!]"), "{rendered}");
    app.open_selection_modal(Modal::Models);
    app.choose_picker_entry(1).await;
    wait_for_method(&recorded, "session.agent_option.set", 1).await;
    let params = last_request_params(&recorded, "session.agent_option.set");
    assert_eq!(params["session_id"], session.to_string());
    assert_eq!(params["option_id"], "model");
    assert_eq!(params["value"], "opus");
    drop(incoming_guard);
}

#[tokio::test]
async fn an_acp_draft_attaches_what_its_agent_takes() {
    let (mut app, _) = acp_session_app().await;
    let mut claude = acp_descriptor("claude", "local", true);
    claude.runtimes[0].input_media = vec![
        serde_json::from_value(serde_json::json!({
            "kind": "text", "mime_types": ["text/plain"], "max_bytes": 1000, "max_count": 4,
        }))
        .unwrap(),
    ];
    claude.runtime = claude.runtimes.first().cloned();
    app.agents[1] = claude;
    app.run_command(SlashCommand::New).await;
    let index = app
        .filtered_agent_picker_candidates()
        .iter()
        .position(|agent| agent.id.as_str() == "claude")
        .unwrap();
    app.choose_picker_entry(index).await;
    let accepted = app.accepted_media().expect("what the agent takes");
    assert_eq!(accepted.model, "claude");
    assert_eq!(accepted.kinds(), "text");
    assert!(app.attachments_offered());
    assert!(
        accepted
            .check("notes.md", "text", "text/plain", 10, None)
            .is_ok()
    );
    assert!(
        accepted
            .check("a.png", "image", "image/png", 10, None)
            .is_err()
    );
    // An agent that never started there takes nothing yet.
    app.agents[1].runtimes[0].input_media.clear();
    assert!(!app.attachments_offered());
}

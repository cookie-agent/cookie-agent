//! ACP agents in the TUI (`docs/site/specs/acp-agents.md` section 13,
//! phase 4): runtime agents in the agent picker, the agent's commands in
//! the palette, its options in `/options`, the plan, and its tool calls.

use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AcpCommand, AcpConfigOption, AcpConfigValue, AgentRuntime, DisplayBlock, Open, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, RuntimeId, SessionTree, ToolHint, ToolSource,
};

use crate::markdown::PlainHighlighter;
use crate::ui::app::*;

use super::support::*;

fn acp_descriptor(
    id: &str,
    runtime: &str,
    available: bool,
) -> cookie_agent_protocol::AgentDescriptor {
    let mut agent = descriptor(id, available);
    agent.mode = cookie_agent_protocol::AgentMode::All;
    agent.resolved_fallback = vec![cookie_agent_protocol::acp_model_selection(&agent.id)];
    agent.runtime = Some(AgentRuntime {
        runtime_id: RuntimeId::new(runtime).unwrap(),
        agent: AgentId::new("claude").unwrap(),
        available,
    });
    agent
}

/// An app viewing root session `session`, whose ACP agent offers commands
/// and a mode option.
async fn acp_session_app() -> (App, SessionId) {
    let mut app = test_app().await;
    app.agents = vec![
        descriptor("primary", true),
        acp_descriptor("local-claude", "local", true),
        acp_descriptor("lab-codex", "lab", false),
    ];
    app.models = vec![model_descriptor()];
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.tree = Some(SessionTree {
        session: session_meta(session),
        children: Vec::new(),
    });
    let state = app.store.sessions.entry(session).or_default();
    state.acp.agent = Some(AgentId::new("local-claude").unwrap());
    state.acp.runtime_id = Some(RuntimeId::new("local").unwrap());
    state.acp.commands = vec![
        AcpCommand {
            name: "review".into(),
            description: "Review the changes".into(),
            input_hint: Some("what to review".into()),
        },
        AcpCommand {
            name: "context".into(),
            description: "Show the context".into(),
            input_hint: None,
        },
    ];
    state.acp.options = vec![AcpConfigOption {
        id: "mode".into(),
        name: "Mode".into(),
        description: None,
        category: Some("mode".into()),
        current_value: "auto".into(),
        values: ["auto", "plan"]
            .into_iter()
            .map(|value| AcpConfigValue {
                value: value.into(),
                name: value.to_uppercase(),
                description: None,
            })
            .collect(),
    }];
    (app, session)
}

#[tokio::test]
async fn runtime_agents_show_their_runtime_and_unavailable_ones_cannot_be_picked() {
    let (mut app, _) = acp_session_app().await;
    app.draft = app.default_draft_selection();
    app.open_selection_modal(Modal::Agents);
    assert_eq!(app.modal, Modal::Agents);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("local-claude [on local]"), "{rendered}");
    assert!(
        rendered.contains("lab-codex [on lab · unavailable]"),
        "{rendered}"
    );
    let ids = app
        .filtered_agent_picker_candidates()
        .iter()
        .map(|agent| agent.id.to_string())
        .collect::<Vec<_>>();
    let away = ids.iter().position(|id| id == "lab-codex").unwrap();
    app.choose_picker_entry(away).await;
    assert!(
        app.status.contains("runtime `lab` is not connected"),
        "{}",
        app.status
    );
    assert_eq!(app.modal, Modal::Agents);
    assert_eq!(app.draft.as_ref().unwrap().agent.as_str(), "primary");

    // An available one runs its synthetic model, which no model picker
    // lists: its model is one of its options.
    let local = ids.iter().position(|id| id == "local-claude").unwrap();
    app.choose_picker_entry(local).await;
    let draft = app.draft.clone().expect("draft");
    assert_eq!(draft.agent.as_str(), "local-claude");
    assert_eq!(draft.model.model.as_str(), "acp/local-claude");
    app.revalidate_draft();
    assert_eq!(
        app.draft.as_ref().unwrap().model.model.as_str(),
        "acp/local-claude"
    );
    app.open_selection_modal(Modal::Models);
    assert_eq!(app.modal, Modal::AgentOptions);
}

#[tokio::test]
async fn the_agents_commands_are_in_the_palette_and_send_as_prompts() {
    let (mut app, _) = acp_session_app().await;
    app.open_command_palette();
    type_input(&mut app, "rev").await;
    let labels = app.palette_list_labels();
    assert!(
        labels
            .iter()
            .any(|label| label == "/review what to review — (agent) Review the changes"),
        "{labels:?}"
    );
    // A command that takes input asks for it.
    let index = labels
        .iter()
        .position(|label| label.starts_with("/review"))
        .unwrap();
    app.activate_palette_row(index).await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("/review (the agent's)"), "{rendered}");
    assert!(rendered.contains("Input: what to review"), "{rendered}");
    assert_eq!(
        crate::ui::app::agent_command_prompt_for_test("review", " the parser "),
        "/review the parser"
    );
    assert_eq!(
        crate::ui::app::agent_command_prompt_for_test("context", ""),
        "/context"
    );
    // Cookie's own commands still come first, and another session's agent
    // offers nothing here.
    app.close_command_palette();
    app.selected = Some(SessionId::new_v7());
    app.tree_root = app.selected;
    app.open_command_palette();
    type_input(&mut app, "rev").await;
    assert!(
        !app.palette_list_labels()
            .iter()
            .any(|label| label.contains("(agent)"))
    );
}

#[tokio::test]
async fn options_lists_the_agents_options_and_sets_one() {
    let (mut app, session) = acp_session_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    run_palette_command(&mut app, "options").await;
    assert_eq!(app.modal, Modal::AgentOptions);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(
        rendered.contains("Agent options — local-claude"),
        "{rendered}"
    );
    assert!(rendered.contains("Mode: AUTO"), "{rendered}");
    app.choose_picker_entry(0).await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("AUTO (current)"), "{rendered}");
    app.choose_picker_entry(1).await;
    assert_eq!(app.modal, Modal::None);
    wait_for_method(&recorded, "session.agent_option.set", 1).await;
    let params = last_request_params(&recorded, "session.agent_option.set");
    assert_eq!(params["session_id"], session.to_string());
    assert_eq!(params["option_id"], "mode");
    assert_eq!(params["value"], "plan");
    drop(incoming_guard);
}

#[test]
fn a_plan_renders_as_a_checklist_with_statuses() {
    let entries = vec![
        PlanEntry {
            content: "Read the file".into(),
            status: PlanEntryStatus::Completed,
            priority: None,
        },
        PlanEntry {
            content: "Edit the file".into(),
            status: PlanEntryStatus::InProgress,
            priority: Some(PlanEntryPriority::High),
        },
        PlanEntry {
            content: "Run the tests".into(),
            status: PlanEntryStatus::Pending,
            priority: Some(PlanEntryPriority::Low),
        },
    ];
    let mut state = SessionState::default();
    state.transcript.push(TranscriptItem::Plan {
        id: 1,
        version: 0,
        run: None,
        entries: entries.clone(),
    });
    let text = snapshot_lines(
        &transcript_layout_with(&state, None, 60, &Theme::default(), &PlainHighlighter).lines,
    );
    for line in [
        "◇─ Plan",
        "│ [x] Read the file",
        "│ [~] Edit the file · high",
        "│ [ ] Run the tests · low",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    // The same checklist inside a tool's display.
    let state = display_tool_state(
        hinted_presentation("Probe", ToolHint::GENERIC, None),
        ToolStatus::Completed,
        vec![DisplayBlock::Plan { entries }],
    );
    let call_id = read_tool_id(&state);
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    let text = snapshot_lines(
        &transcript_layout_with(
            &state,
            Some(&expanded),
            60,
            &Theme::default(),
            &PlainHighlighter,
        )
        .lines,
    );
    assert!(text.contains("[~] Edit the file · high"), "{text}");
}

#[test]
fn an_acp_agents_tool_call_names_the_agent() {
    let mut presentation = hinted_presentation("Read notes.txt", ToolHint::FILE_READ, None);
    presentation.tool.source = Some(Open::Known(ToolSource::Acp {
        agent: AgentId::new("local-claude").unwrap(),
        runtime_id: RuntimeId::new("local").unwrap(),
    }));
    let state = display_tool_state(presentation, ToolStatus::Completed, Vec::new());
    let text = snapshot_lines(
        &transcript_layout_with(&state, None, 80, &Theme::default(), &PlainHighlighter).lines,
    );
    assert!(
        text.contains("Read notes.txt · local-claude on local"),
        "{text}"
    );
}

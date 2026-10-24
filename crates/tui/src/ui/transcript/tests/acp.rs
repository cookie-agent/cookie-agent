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

pub(super) fn acp_descriptor(
    id: &str,
    runtime: &str,
    available: bool,
) -> cookie_agent_protocol::AgentDescriptor {
    let mut agent = descriptor(id, available);
    agent.mode = cookie_agent_protocol::AgentMode::All;
    agent.resolved_fallback = vec![cookie_agent_protocol::acp_model_selection(&agent.id)];
    agent.runtime = Some(AgentRuntime {
        runtime_id: RuntimeId::new(runtime).unwrap(),
        agent: agent.id.clone(),
        available,
        options: Vec::new(),
        input_media: Vec::new(),
    });
    agent.runtimes = agent.runtime.iter().cloned().collect();
    agent
}

/// An app viewing root session `session`, whose ACP agent offers commands
/// and a mode option.
pub(super) async fn acp_session_app() -> (App, SessionId) {
    let mut app = test_app().await;
    app.agents = vec![
        descriptor("primary", true),
        acp_descriptor("claude", "local", true),
        acp_descriptor("codex", "lab", false),
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
    state.runtime_attachments = local_workspace_attachments();
    state.acp.agent = Some(AgentId::new("claude").unwrap());
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
        kind: Default::default(),
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
async fn acp_agents_show_where_they_run_and_ones_that_cannot_run_cannot_be_picked() {
    let (mut app, _) = acp_session_app().await;
    app.draft = app.default_draft_selection();
    app.open_selection_modal(Modal::Agents);
    assert_eq!(app.modal, Modal::Agents);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("claude [on local]"), "{rendered}");
    // No runtime this session attaches offers it.
    assert!(
        rendered.contains("codex [needs lab attached]"),
        "{rendered}"
    );
    let ids = app
        .filtered_agent_picker_candidates()
        .iter()
        .map(|agent| agent.id.to_string())
        .collect::<Vec<_>>();
    let away = ids.iter().position(|id| id == "codex").unwrap();
    app.choose_picker_entry(away).await;
    assert_eq!(
        app.status,
        "`codex` needs a runtime that offers it attached: lab"
    );
    assert_eq!(app.modal, Modal::Agents);
    assert_eq!(app.draft.as_ref().unwrap().agent.as_str(), "primary");

    // An available one runs its synthetic model, which no model picker
    // lists: its model is one of its options.
    let local = ids.iter().position(|id| id == "claude").unwrap();
    app.choose_picker_entry(local).await;
    let draft = app.draft.clone().expect("draft");
    assert_eq!(draft.agent.as_str(), "claude");
    assert_eq!(draft.model.model.as_str(), "acp/claude");
    app.revalidate_draft();
    assert_eq!(
        app.draft.as_ref().unwrap().model.model.as_str(),
        "acp/claude"
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
    assert!(rendered.contains("Agent options — claude"), "{rendered}");
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
        agent: AgentId::new("claude").unwrap(),
        runtime_id: RuntimeId::new("local").unwrap(),
    }));
    let state = display_tool_state(presentation, ToolStatus::Completed, Vec::new());
    let text = snapshot_lines(
        &transcript_layout_with(&state, None, 80, &Theme::default(), &PlainHighlighter).lines,
    );
    assert!(text.contains("Read notes.txt · claude on local"), "{text}");
}

#[test]
fn a_notice_and_a_steered_message_render() {
    let mut state = SessionState::default();
    state.transcript.push(TranscriptItem::User {
        id: 1,
        version: 0,
        markdown: crate::markdown::MarkdownDocument::with_hard_breaks("look at notes.txt".into())
            .finished(),
        seq: 7,
    });
    state.acp.steered.insert(7);
    state.transcript.push(TranscriptItem::AcpNotice {
        id: 2,
        notice: cookie_agent_protocol::AcpNotice {
            severity: cookie_agent_protocol::AcpNoticeSeverity::Info,
            title: "The agent compacted its conversation".into(),
            description: Some("We read notes.txt.".into()),
        },
    });
    let text = snapshot_lines(
        &transcript_layout_with(&state, None, 60, &Theme::default(), &PlainHighlighter).lines,
    );
    for line in [
        "│ look at notes.txt",
        "│ ↪ steered into the agent's running turn",
        "◇─ Notice",
        "│ The agent compacted its conversation",
        "│ We read notes.txt.",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
}

#[tokio::test]
async fn the_status_line_shows_the_agents_context_of_its_own_window() {
    let (mut app, session) = acp_session_app().await;
    app.draft = Some(cookie_agent_protocol::RunSelection {
        agent: AgentId::new("claude").unwrap(),
        model: cookie_agent_protocol::acp_model_selection(&AgentId::new("claude").unwrap()),
        preset: None,
    });
    let state = app.store.sessions.get_mut(&session).unwrap();
    state.context_tokens = Some(30_000);
    state.context_window = Some(120_000);
    let bar = rendered_row(&mut app, 100, 24, 23);
    assert!(bar.contains("ctx 30.0K (25%)"), "{bar}");
}

#[tokio::test]
async fn options_toggles_a_boolean_option_in_place() {
    let (mut app, session) = acp_session_app().await;
    app.store
        .sessions
        .get_mut(&session)
        .unwrap()
        .acp
        .options
        .push(AcpConfigOption {
            id: "fast".into(),
            name: "Fast mode".into(),
            kind: cookie_agent_protocol::AcpConfigOptionKind::Boolean,
            description: None,
            category: Some("model_config".into()),
            current_value: "false".into(),
            values: [("true", "On"), ("false", "Off")]
                .into_iter()
                .map(|(value, name)| AcpConfigValue {
                    value: value.into(),
                    name: name.into(),
                    description: None,
                })
                .collect(),
        });
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    run_palette_command(&mut app, "options").await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(
        rendered.contains("Fast mode: Off (model_config)"),
        "{rendered}"
    );
    app.choose_picker_entry(1).await;
    // No values step: the toggle is sent, and the options stay open.
    assert_eq!(app.modal, Modal::AgentOptions);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Agent options — claude"), "{rendered}");
    wait_for_method(&recorded, "session.agent_option.set", 1).await;
    let params = last_request_params(&recorded, "session.agent_option.set");
    assert_eq!(params["option_id"], "fast");
    assert_eq!(params["value"], "true");
    drop(incoming_guard);
}

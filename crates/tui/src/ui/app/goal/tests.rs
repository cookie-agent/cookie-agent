use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    GoalId, GoalItem, MessageFrame, MessageStream, SessionMeta, SessionOrigin, TransportError,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::*;
use crate::{client::Client, config::TuiConfig, state::SessionState, theme::Theme};

struct ScriptedStream {
    incoming: mpsc::UnboundedReceiver<MessageFrame>,
    sent: mpsc::UnboundedSender<MessageFrame>,
}

#[async_trait]
impl MessageStream for ScriptedStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.sent.send(frame).map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

async fn app_with_replies(
    replies: Vec<(&'static str, Result<Value, &'static str>)>,
) -> (App, Arc<Mutex<Vec<Value>>>) {
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    let mut replies = VecDeque::from(replies);
    tokio::spawn(async move {
        while let Some(frame) = sent_rx.recv().await {
            let request: Value = match frame {
                MessageFrame::Value(value) => value,
                MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON request"),
            };
            let method = request["method"].as_str().unwrap_or_default();
            let response = if method.starts_with("session.goal.") {
                let (expected, result) = replies.pop_front().expect("expected goal request");
                assert_eq!(method, expected);
                result
            } else if method == "session.list" {
                Ok(json!({"sessions": []}))
            } else {
                Err("unavailable in test")
            };
            sink.lock().expect("requests").push(request.clone());
            let mut reply = json!({"jsonrpc": "2.0", "id": request["id"]});
            match response {
                Ok(result) => reply["result"] = result,
                Err(message) => reply["error"] = json!({"code": -32000, "message": message}),
            }
            if incoming.send(MessageFrame::Value(reply)).is_err() {
                break;
            }
        }
    });
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut app = App::new_with_config(client, false, TuiConfig::default(), Theme::default())
        .await
        .expect("app");
    install_draft_catalog(&mut app);
    recorded.lock().expect("requests").clear();
    (app, recorded)
}

fn draft_selection(
    model: &str,
    variant: Option<&str>,
    preset: Option<&str>,
) -> cookie_agent_protocol::RunSelection {
    serde_json::from_value(json!({
        "agent": "primary", "model": { "model": model, "variant": variant }, "preset": preset,
    }))
    .unwrap()
}

fn install_draft_catalog(app: &mut App) {
    use cookie_agent_protocol::{AgentDescriptor, AgentMode, Sha256Digest};

    let selection = draft_selection("test/model", None, None);
    app.agents = [None, Some("review".to_owned())]
        .into_iter()
        .map(|preset| AgentDescriptor {
            id: selection.agent.clone(),
            preset,
            description: "Primary test agent".into(),
            mode: AgentMode::Primary,
            enabled: true,
            runnable_as_root: true,
            resolved_fallback: vec![selection.model.clone()],
            delegation_targets: Vec::new(),
        })
        .collect();
    app.models = ["test/model", "test/model-b"]
        .into_iter()
        .map(|model| {
            serde_json::from_value(json!({
                "key": model, "display_name": model,
                "capabilities": {
                    "input": ["text"], "output": ["text"], "context_tokens": 8192,
                    "output_tokens": 2048, "tool_calling": true, "parallel_tool_calls": true,
                    "structured_output": false, "reasoning": true, "temperature": true,
                    "top_p": true, "seed": true,
                    "native_replay": cookie_agent_protocol::ReplayCapability::Optional,
                    "cancellation": cookie_agent_protocol::CancellationCapability::LocalOnly,
                    "media": {},
                },
                "variants": [{
                    "id": "high", "display_name": "High",
                    "origin": cookie_agent_protocol::VariantOrigin::Explicit,
                    "behavior_fingerprint": Sha256Digest::of_bytes(b"high"),
                }],
                "variant_order": ["high"], "default_variant": "high",
                "behavior_fingerprint": Sha256Digest::of_bytes(model.as_bytes()),
            }))
            .unwrap()
        })
        .collect();
    app.draft = Some(selection);
}

fn goal(status: GoalStatus) -> GoalState {
    GoalState {
        goal_id: GoalId::new_v7(),
        objective: "finish  the parser".into(),
        status,
        items: vec![
            GoalItem {
                description: "Parse commands".into(),
                finished: true,
            },
            GoalItem {
                description: "Verify behavior".into(),
                finished: false,
            },
        ],
        revision: 7,
    }
}

fn meta(session_id: SessionId, origin: SessionOrigin) -> SessionMeta {
    let revision = format!("sha256:{}", "1".repeat(64));
    serde_json::from_value(json!({
            "session_id": session_id,
            "origin": origin,
            "cwd_identity": "/workspace",
            "creation_selection": {"agent": "primary", "model": {"model": "test/model", "variant": null}, "preset": null},
            "runtime_revision": revision, "catalog_revision": revision, "provider_state_revision": revision,
            "model_revision": revision, "agent_revision": revision, "recipe_registry_revision": revision, "manifest_revision": revision,
            "title": null, "title_updated_seq": 0, "last_event_seq": 1,
            "last_activity": "2026-08-06T12:00:00Z", "status": "idle", "skipped_events": []
        })).expect("session metadata")
}

/// Run a goal command written the way the tests read it: `/goal pause`,
/// `/goal resume`, `/goal cancel`, or `/goal <objective>`. The palette only
/// sets objectives; lifecycle controls come from the goal bar, and both
/// reach the same `run_goal_command`.
async fn dispatch(app: &mut App, input: &str) {
    let command = match input.strip_prefix("/goal ").expect("goal command") {
        "pause" => GoalCommand::Pause,
        "resume" => GoalCommand::Resume,
        "cancel" => GoalCommand::Cancel,
        objective => GoalCommand::Objective(objective.to_owned()),
    };
    // Synchronous: the RPC runs in the background and reports back below.
    app.run_goal_command(command);
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("goal response")
        .expect("update");
    assert!(matches!(update, RpcUpdate::GoalFinished { .. }));
    app.handle_rpc_update(update);
}

async fn finish_rpc(app: &mut App) {
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("goal response")
        .expect("update");
    assert!(matches!(update, RpcUpdate::GoalFinished { .. }));
    app.handle_rpc_update(update);
}

fn mount_goal(app: &mut App, session_id: SessionId, goal: GoalState) {
    app.selected = Some(session_id);
    app.sessions.push(meta(session_id, SessionOrigin::Root));
    app.store.sessions.insert(
        session_id,
        SessionState {
            goal: Some(goal),
            ..Default::default()
        },
    );
}

#[tokio::test]
async fn goal_activation_action_is_event_owned_regardless_of_rpc_response_order() {
    for response_first in [false, true] {
        let (mut app, requests) = app_with_replies(Vec::new()).await;
        let session_id = SessionId::new_v7();
        let current = goal(GoalStatus::Active);
        app.selected = Some(session_id);
        if response_first {
            app.finish_goal_command(session_id, Ok(Some(current.clone())));
        }
        assert!(app.store.apply_event(cookie_agent_protocol::StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: None,
            seq: 1,
            timestamp: jiff::Timestamp::new(1, 0).unwrap(),
            payload: cookie_agent_protocol::EventPayload::GoalActivated {
                goal_id: current.goal_id,
                objective: current.objective.clone(),
                revision: current.revision,
                selection: None,
            },
        }));
        app.finish_goal_command(session_id, Ok(Some(current)));
        let state = &app.store.sessions[&session_id];
        assert!(matches!(
            state.transcript.as_slice(),
            [crate::state::TranscriptItem::Goal {
                activation: true,
                ..
            }]
        ));
        assert!(state.pending_inputs.is_empty());
        assert!(app.goal_bar_visible());
        assert!(!app.goal_notices.contains_key(&session_id));
        assert!(requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn goal_activation_uses_exact_set_request_without_starting_a_run() {
    let expected = goal(GoalStatus::Active);
    let (mut app, requests) =
        app_with_replies(vec![("session.goal.set", Ok(json!({"goal": expected})))]).await;
    let session_id = SessionId::new_v7();
    app.selected = Some(session_id);
    app.sessions.push(meta(session_id, SessionOrigin::Root));
    let selection = app.draft.clone().unwrap();
    dispatch(&mut app, "/goal finish  the parser").await;
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "session.goal.set");
    assert_eq!(
        requests[0]["params"],
        json!({"session_id": session_id, "objective": "finish  the parser", "selection": selection})
    );
    assert!(!app.goal_notices.contains_key(&session_id));
}

#[tokio::test]
async fn palette_goal_step_prompts_for_the_objective_and_leaves_the_draft_alone() {
    use crossterm::event::KeyModifiers;

    let key = |code| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    let expected = goal(GoalStatus::Active);
    let (mut app, requests) =
        app_with_replies(vec![("session.goal.set", Ok(json!({"goal": expected})))]).await;
    let session_id = SessionId::new_v7();
    app.selected = Some(session_id);
    app.sessions.push(meta(session_id, SessionOrigin::Root));
    app.input.set_buffer("half-written message".into());
    let selection = app.draft.clone().unwrap();
    let type_text = async |app: &mut App, text: &str| {
        for character in text.chars() {
            app.handle_key(key(KeyCode::Char(character))).await;
        }
    };

    app.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
    ))
    .await;
    type_text(&mut app, "goal").await;
    app.handle_key(key(KeyCode::Enter)).await;
    let in_goal_step = |app: &App| {
        matches!(
            app.palette
                .as_ref()
                .and_then(|palette| palette.steps.last()),
            Some(crate::ui::slash::PaletteStep::Text {
                target: crate::ui::slash::TextTarget::GoalObjective,
                ..
            })
        )
    };
    assert!(in_goal_step(&app));

    // An empty objective is refused in place.
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(in_goal_step(&app));
    assert!(app.status.contains("must not be empty"), "{}", app.status);

    // Esc steps back to the command list with the search intact.
    type_text(&mut app, "abandoned").await;
    app.handle_key(key(KeyCode::Esc)).await;
    let palette = app.palette.as_ref().expect("palette still open");
    assert!(palette.steps.is_empty());
    assert_eq!(palette.search.as_str(), "goal");

    app.handle_key(key(KeyCode::Enter)).await;
    assert!(in_goal_step(&app));
    type_text(&mut app, "finish the parser").await;
    app.handle_key(key(KeyCode::Enter)).await;
    assert!(app.palette.is_none());
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("goal response")
        .expect("update");
    app.handle_rpc_update(update);

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["params"],
        json!({"session_id": session_id, "objective": "finish the parser", "selection": selection})
    );
    assert_eq!(app.input.as_str(), "half-written message");
}

#[tokio::test]
async fn palette_goal_entry_refuses_before_prompting_outside_a_root_session() {
    let (mut app, requests) = app_with_replies(Vec::new()).await;
    app.selected = None;
    app.open_command_palette();
    let goal = crate::ui::slash::COMMANDS
        .iter()
        .find(|spec| spec.name == "goal")
        .expect("goal command");
    app.choose_palette_command(goal).await;
    assert!(
        app.palette
            .as_ref()
            .is_some_and(|palette| palette.steps.is_empty())
    );
    assert!(
        app.status.contains("select a root session"),
        "{}",
        app.status
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn lifecycle_buttons_fetch_fresh_identity_and_revision() {
    use ratatui::{Terminal, backend::TestBackend};

    for (projected_status, button, action, changed_status) in [
        (
            GoalStatus::Active,
            GoalBarAction::Pause,
            "pause",
            GoalStatus::Paused,
        ),
        (
            GoalStatus::Paused,
            GoalBarAction::Resume,
            "resume",
            GoalStatus::Active,
        ),
        (
            GoalStatus::Active,
            GoalBarAction::Cancel,
            "cancel",
            GoalStatus::Cancelled,
        ),
    ] {
        let current = goal(projected_status);
        let changed = GoalState {
            status: changed_status,
            revision: 8,
            ..current.clone()
        };
        let (mut app, requests) = app_with_replies(vec![
            ("session.goal.get", Ok(json!({"goal": current}))),
            ("session.goal.lifecycle", Ok(json!({"goal": changed}))),
        ])
        .await;
        let session_id = SessionId::new_v7();
        let mut projected = goal(projected_status);
        projected.revision = 2;
        mount_goal(&mut app, session_id, projected);
        let selection = app.draft.clone().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                app.render_goal_bar(frame, area);
            })
            .unwrap();
        let (rect, _) = app
            .hit_map
            .goal_actions
            .iter()
            .find(|(_, action)| *action == button)
            .copied()
            .expect("lifecycle button");
        assert_eq!(
            app.hover_target_at(rect.x, rect.y),
            Some(super::super::HoverTarget::GoalAction(button))
        );
        app.handle_click(rect.x, rect.y).await;
        finish_rpc(&mut app).await;
        assert_eq!(app.status, format!("goal {}", status_name(changed_status)));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["method"], "session.goal.get");
        assert_eq!(requests[1]["method"], "session.goal.lifecycle");
        let mut params = json!({
            "session_id": session_id, "goal_id": current.goal_id,
            "expected_revision": 7, "action": action,
        });
        if action == "resume" {
            params["selection"] = json!(selection);
        }
        assert_eq!(requests[1]["params"], params);
    }
}

#[tokio::test]
async fn activation_and_resume_send_current_draft_without_changing_running_attribution() {
    use crate::state::{FrozenAssistantAttribution, TranscriptItem};
    use cookie_agent_protocol::{AdaptorId, ResolvedModelRef, RunId, Sha256Digest};

    for command in ["/goal Use the selected model", "/goal resume"] {
        let current_goal = goal(GoalStatus::Paused);
        let replies = if command == "/goal resume" {
            vec![
                ("session.goal.get", Ok(json!({"goal": current_goal}))),
                ("session.goal.lifecycle", Ok(json!({"goal": current_goal}))),
            ]
        } else {
            vec![("session.goal.set", Ok(json!({"goal": current_goal})))]
        };
        let (mut app, requests) = app_with_replies(replies).await;
        let session_id = SessionId::new_v7();
        mount_goal(&mut app, session_id, current_goal);
        let previous = app.sessions[0].creation_selection.clone();
        let selected = draft_selection("test/model-b", Some("high"), Some("review"));
        app.draft = Some(selected.clone());
        let run = RunId::new_v7();
        let attribution = FrozenAssistantAttribution {
            agent: previous.agent.clone(),
            resolved_model: ResolvedModelRef {
                selection: previous.model.clone(),
                provider_id: previous.model.model.provider_id(),
                model_id: previous.model.model.model_id(),
                adapter_id: AdaptorId::OpenaiCompatible,
                selection_fingerprint: Sha256Digest::of_bytes(b"running-model-a"),
            },
        };
        let original_header = attribution.header();
        let state = app.store.sessions.get_mut(&session_id).unwrap();
        if command != "/goal resume" {
            state.goal = None;
        }
        state.active_run = Some(run);
        state.run_agent = Some(previous.agent.clone());
        state.transcript.push(TranscriptItem::Assistant {
            id: 1,
            version: 0,
            attribution,
            committed_turn_seq: Some(1),
            children: Vec::new(),
        });
        let title_before = app.message_title_spans();
        dispatch(&mut app, command).await;
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.last().unwrap()["params"]["selection"],
            json!(selected)
        );
        assert!(requests.iter().all(
                |request| request["method"] != "run.start" && request["method"] != "run.steer"
            ));
        assert_eq!(app.draft.as_ref(), Some(&selected));
        assert_eq!(app.message_title_spans(), title_before);
        assert_eq!(app.sessions[0].creation_selection, previous);
        let state = &app.store.sessions[&session_id];
        assert_eq!(state.active_run, Some(run));
        let TranscriptItem::Assistant { attribution, .. } = &state.transcript[0] else {
            panic!("assistant")
        };
        assert_eq!(attribution.header(), original_header);
    }
}

#[tokio::test]
async fn goal_selection_uses_normal_draft_validation_and_pause_cancel_do_not_normalize() {
    let current_goal = goal(GoalStatus::Paused);
    let (mut app, _) = app_with_replies(vec![(
        "session.goal.set",
        Ok(json!({"goal": current_goal})),
    )])
    .await;
    let session_id = SessionId::new_v7();
    mount_goal(&mut app, session_id, current_goal.clone());
    let invalid = draft_selection("test/model-b", Some("removed-variant"), Some("review"));
    app.draft = Some(invalid.clone());
    let normalized = app
        .validated_draft_selection()
        .expect("normal submission draft");
    assert_eq!(normalized.model.variant.as_ref().unwrap().as_str(), "high");
    app.draft = Some(invalid.clone());
    dispatch(&mut app, "/goal Normalize the selected draft").await;
    assert_eq!(app.draft, Some(normalized));

    for command in ["/goal pause", "/goal cancel"] {
        let (mut app, requests) = app_with_replies(vec![
            ("session.goal.get", Ok(json!({"goal": current_goal}))),
            ("session.goal.lifecycle", Ok(json!({"goal": current_goal}))),
        ])
        .await;
        mount_goal(&mut app, session_id, current_goal.clone());
        app.draft = Some(invalid.clone());
        dispatch(&mut app, command).await;
        assert_eq!(app.draft.as_ref(), Some(&invalid));
        assert!(
            requests.lock().unwrap().last().unwrap()["params"]
                .get("selection")
                .is_none()
        );
    }
    let (mut app, requests) = app_with_replies(Vec::new()).await;
    mount_goal(&mut app, session_id, current_goal);
    app.draft = None;
    for command in [
        GoalCommand::Resume,
        GoalCommand::Objective("Need a model".into()),
    ] {
        app.run_goal_command(command);
        assert!(app.status.contains("select a draft agent/model"));
    }
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn goal_errors_are_visible_and_do_not_mutate_projection_or_composer() {
    for (command, replies, error) in [
        (
            "/goal pause",
            vec![("session.goal.get", Ok(json!({"goal": null})))],
            "no goal is set",
        ),
        (
            "/goal new objective",
            vec![("session.goal.set", Err("goal is already active"))],
            "goal is already active",
        ),
        (
            "/goal cancel",
            vec![
                (
                    "session.goal.get",
                    Ok(json!({"goal": goal(GoalStatus::Active)})),
                ),
                ("session.goal.lifecycle", Err("stale goal revision")),
            ],
            "stale goal revision",
        ),
    ] {
        let (mut app, _) = app_with_replies(replies).await;
        let session_id = SessionId::new_v7();
        let current = goal(GoalStatus::Paused);
        app.selected = Some(session_id);
        app.store.sessions.insert(
            session_id,
            SessionState {
                goal: Some(current.clone()),
                ..Default::default()
            },
        );
        app.input.set_buffer("user draft".into());
        dispatch(&mut app, command).await;
        assert!(app.status.contains(error), "{}", app.status);
        assert!(
            app.goal_notices[&session_id]
                .last()
                .unwrap()
                .contains(error)
        );
        assert_eq!(app.store.sessions[&session_id].goal, Some(current));
        assert_eq!(app.input.as_str(), "user draft");
    }
}

#[tokio::test]
async fn goal_guards_reject_missing_child_read_only_and_empty_activation() {
    let (mut app, requests) = app_with_replies(Vec::new()).await;
    app.run_goal_command(GoalCommand::Pause);
    assert!(app.status.contains("select a root session"));
    let session_id = SessionId::new_v7();
    app.selected = Some(session_id);
    app.sessions.push(meta(
        session_id,
        SessionOrigin::Delegated {
            root_session_id: SessionId::new_v7(),
            parent_session_id: SessionId::new_v7(),
            parent_run_id: cookie_agent_protocol::RunId::new_v7(),
            parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
            depth: 1,
        },
    ));
    app.store.sessions.insert(
        session_id,
        SessionState {
            goal: Some(goal(GoalStatus::Active)),
            ..Default::default()
        },
    );
    assert!(!app.goal_bar_visible());
    app.open_goal_detail();
    assert_eq!(app.modal, Modal::None);
    for action in [
        GoalBarAction::Pause,
        GoalBarAction::Resume,
        GoalBarAction::Cancel,
    ] {
        app.activate_goal_action(action);
        assert!(app.status.contains("only available in root sessions"));
    }
    for command in [
        GoalCommand::Pause,
        GoalCommand::Resume,
        GoalCommand::Cancel,
        GoalCommand::Objective("test".into()),
    ] {
        app.run_goal_command(command);
        assert!(app.status.contains("only available in root sessions"));
    }
    app.sessions[0].origin = SessionOrigin::Root;
    app.read_only_sessions.insert(session_id);
    app.run_goal_command(GoalCommand::Pause);
    assert!(app.status.contains("read-only"));
    app.read_only_sessions.clear();
    app.run_goal_command(GoalCommand::Objective(" \t ".into()));
    assert!(app.status.contains("must not be empty"));
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn goal_responses_stay_with_their_session_and_do_not_replace_newer_events() {
    let (mut app, _) = app_with_replies(Vec::new()).await;
    let first = SessionId::new_v7();
    let second = SessionId::new_v7();
    let current = goal(GoalStatus::Completed);
    app.store.sessions.insert(
        first,
        SessionState {
            goal: Some(current.clone()),
            ..Default::default()
        },
    );
    app.selected = Some(second);
    app.status = "second session".into();
    app.finish_goal_command(first, Ok(Some(goal(GoalStatus::Active))));
    assert_eq!(app.status, "second session");
    assert!(!app.goal_notices.contains_key(&second));
    assert_eq!(app.store.sessions[&first].goal, Some(current));
}

#[tokio::test]
async fn goal_bar_formats_each_status_without_progress() {
    use ratatui::{Terminal, backend::TestBackend};

    let (mut app, _) = app_with_replies(Vec::new()).await;
    let session = SessionId::new_v7();
    for (status, suffix) in [
        (GoalStatus::Active, "[Pause] [Cancel]"),
        (GoalStatus::Paused, "[Resume] [Cancel]"),
        (GoalStatus::Completed, " · completed"),
        (GoalStatus::Cancelled, " · cancelled"),
    ] {
        let mut goal = goal(status);
        goal.objective = "finish\n the parser".into();
        mount_goal(&mut app, session, goal);
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| app.render_goal_bar(frame, frame.area()))
            .unwrap();
        // Skip the continuation cell occupied by the double-width emoji.
        let line = (0..80)
            .filter(|&x| x != 1)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(line.starts_with("🎯: finish the parser"), "{line}");
        assert!(line.trim_end().ends_with(suffix), "{line}");
        assert!(!line.contains('/'));
        for x in 0..21 {
            assert!(
                !terminal.backend().buffer()[(x, 0)]
                    .modifier
                    .contains(ratatui::style::Modifier::UNDERLINED)
            );
        }
    }
}

#[tokio::test]
async fn goal_bar_is_bounded_and_has_non_overlapping_hits_at_tiny_widths() {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    let (mut app, _) = app_with_replies(Vec::new()).await;
    let session_id = SessionId::new_v7();
    let mut current = goal(GoalStatus::Paused);
    current.objective = "Long objective\nwith\ttabs and a long sequence of work ".repeat(10);
    mount_goal(&mut app, session_id, current);
    for width in [1, 3, 7, 8, 11, 12, 13, 20, 25, 26, 40, 80] {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| app.render_goal_bar(frame, Rect::new(0, 0, width, 1)))
            .unwrap();
        // Skip the emoji continuation cell only while the prefix is visible.
        let line = (0..width)
            .filter(|&x| x != 1 || width < 12)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(UnicodeWidthStr::width(line.as_str()) <= usize::from(width));
        assert!(!line.contains(['\n', '\t']));
        if width >= 12 {
            assert!(line.starts_with("🎯: Lon"), "{line}");
        }
        assert_eq!(line.starts_with("🎯: "), width >= 12, "{line}");
        assert!(
            app.hit_map
                .goal_actions
                .iter()
                .any(|(_, action)| *action == GoalBarAction::Details)
        );
        for (index, (rect, action)) in app.hit_map.goal_actions.iter().enumerate() {
            assert!(rect.right() <= width, "{width}: {rect:?}");
            if *action != GoalBarAction::Details {
                let label = (rect.x..rect.right())
                    .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                    .collect::<String>();
                let expected = match (action, width >= 26) {
                    (GoalBarAction::Resume, true) => " [Resume]",
                    (GoalBarAction::Resume, false) => " >",
                    (GoalBarAction::Cancel, true) => " [Cancel]",
                    (GoalBarAction::Cancel, false) => " x",
                    _ => unreachable!(),
                };
                assert_eq!(label, expected, "width {width}");
            }
            for (other, _) in app.hit_map.goal_actions.iter().skip(index + 1) {
                assert!(
                    rect.intersection(*other).is_empty(),
                    "{width}: {rect:?} {other:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn terminal_and_read_only_goals_expose_no_lifecycle_buttons() {
    let (mut app, _) = app_with_replies(Vec::new()).await;
    let session_id = SessionId::new_v7();
    mount_goal(&mut app, session_id, goal(GoalStatus::Completed));
    assert_eq!(app.allowed_goal_actions(), vec![GoalBarAction::Details]);
    app.cycle_goal_focus(false);
    assert_eq!(app.goal_focus, Some(GoalBarAction::Details));
    app.store.sessions.get_mut(&session_id).unwrap().goal = Some(goal(GoalStatus::Active));
    app.read_only_sessions.insert(session_id);
    assert_eq!(app.allowed_goal_actions(), vec![GoalBarAction::Details]);
    app.cycle_goal_focus(false);
    assert_eq!(app.goal_focus, Some(GoalBarAction::Details));
    app.activate_goal_action(GoalBarAction::Pause);
    assert!(app.status.contains("read-only"));
    app.read_only_sessions.clear();
    assert_eq!(
        app.allowed_goal_actions(),
        vec![
            GoalBarAction::Details,
            GoalBarAction::Pause,
            GoalBarAction::Cancel
        ]
    );
    app.store.sessions.get_mut(&session_id).unwrap().goal = Some(goal(GoalStatus::Paused));
    assert_eq!(
        app.allowed_goal_actions(),
        vec![
            GoalBarAction::Details,
            GoalBarAction::Resume,
            GoalBarAction::Cancel
        ]
    );
}

#[tokio::test]
async fn detail_modal_opens_scrolls_and_closes_when_session_changes() {
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    let (mut app, _) = app_with_replies(Vec::new()).await;
    let session_id = SessionId::new_v7();
    let mut current = goal(GoalStatus::Active);
    current.items = (0..20)
        .map(|index| GoalItem {
            description: format!("ordered checklist item {index}"),
            finished: index % 2 == 0,
        })
        .collect();
    mount_goal(&mut app, session_id, current);
    app.open_goal_detail();
    assert_eq!(app.modal, Modal::GoalDetail);
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
    terminal
        .draw(|frame| app.render_goal_detail(frame))
        .unwrap();
    assert!(app.goal_detail.max_scroll > 0);
    app.handle_goal_detail_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert!(app.goal_detail.scroll > 0);
    app.handle_goal_detail_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.goal_detail.scroll, 0);
    for width in [1, 3, 7, 8, 20, 40, 80] {
        let mut narrow = Terminal::new(TestBackend::new(width, 10)).unwrap();
        narrow.draw(|frame| app.render_goal_detail(frame)).unwrap();
        if let Some(close) = app.hit_map.goal_close {
            assert!(close.right() <= width);
            assert!(close.bottom() <= 10);
        }
        app.handle_goal_detail_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        app.handle_goal_detail_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.goal_detail.scroll, app.goal_detail.max_scroll);
        app.handle_goal_detail_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    }

    app.handle_goal_detail_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.modal, Modal::None);
    app.open_goal_detail();

    app.selected = Some(SessionId::new_v7());
    terminal
        .draw(|frame| app.render_goal_detail(frame))
        .unwrap();
    assert_eq!(app.modal, Modal::None);
    assert_eq!(app.goal_detail.session_id, None);
}

#[tokio::test]
async fn producer_reminders_never_restore_into_the_user_composer() {
    use cookie_agent_protocol::{
        EventPayload, GoalReminderIdentity, ProducerDeliveryMode, ProducerIdempotencyKey,
        ProducerMessageId, ProducerOwner, RunId, StoredEvent,
    };

    let (mut app, _) = app_with_replies(Vec::new()).await;
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let goal_id = GoalId::new_v7();
    app.selected = Some(session_id);
    for (index, payload) in [
        EventPayload::ProducerMessageAccepted {
            description: Default::default(),
            message_id: ProducerMessageId::new_v7(),
            producer_owner: ProducerOwner::Goal { goal_id },
            mode: ProducerDeliveryMode::Queue,
            idempotency_key: ProducerIdempotencyKey::new("reminder").unwrap(),
            body: "INTERNAL REMINDER".into(),
            reminder: Some(GoalReminderIdentity {
                goal_id,
                revision: 1,
                kind: cookie_agent_protocol::GoalReminderKind::Continuation,
            }),
            agent_hop: None,
        },
        EventPayload::UserInputAdmitted {
            input: "user pending text".into(),
        },
        EventPayload::RunInterrupted { reason: None },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(app.store.apply_event(StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: Some(run_id),
            seq: index as u64 + 1,
            timestamp: "2026-08-06T12:00:00Z".parse().unwrap(),
            payload,
        }));
    }
    app.restore_voided_inputs();
    assert_eq!(app.input.as_str(), "user pending text");
    assert!(app.store.sessions[&session_id].voided_inputs.is_empty());
}

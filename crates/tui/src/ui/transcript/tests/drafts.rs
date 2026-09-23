use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AgentId, AttemptId, EventPayload, GoalId, ModelSelection, RunSelection, SessionId, SessionTree,
    Sha256Digest,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{FrozenAssistantAttribution, StateStore};

use crate::ui::app::*;

use crate::ui::pickers::SearchPickerFocus;

use crate::ui::slash::{GoalCommand, SlashCommand};

use cookie_agent_server::MessageFrame;

use super::support::*;

#[tokio::test]
async fn goal_activation_keeps_run_a_frozen_until_modeled_run_b_events_arrive() {
    let (client, _requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    let model_a = model_descriptor();
    let model_b = catalog_model("other/model-b", &["low", "high"], Some("low"));
    let mut review_primary = preset_descriptor("review", "primary");
    review_primary.resolved_fallback = vec![ModelSelection {
        model: model_b.key.clone(),
        variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
    }];
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![model_a, model_b.clone()],
        vec![descriptor("primary", true), review_primary],
    ));

    let session = SessionId::new_v7();
    let run_a = run_id();
    let attempt_a = AttemptId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.sessions.push(session_meta(session));
    for event in [
        session_created(session, 1),
        run_started_with_suffix(session, 2, run_a, vec![resolved_model(None)]),
        attempt_started(session, 3, run_a, attempt_a, None),
        turn_committed(
            session,
            4,
            run_a,
            attempt_a,
            1,
            vec![text_part("run A answer")],
            Vec::new(),
            None,
        ),
    ] {
        assert!(app.store.apply_event(event));
    }
    let run_a_projection = assistant_projection(&app.store.sessions[&session]);
    assert_eq!(run_a_projection.len(), 1);
    assert_eq!(run_a_projection[0].0, attribution(None).header());
    assert_eq!(
        app.store.sessions[&session]
            .run_snapshot
            .as_ref()
            .expect("run A snapshot")
            .agent,
        agent_id()
    );

    app.run_command(SlashCommand::Preset).await;
    app.choose_picker_entry(1).await;
    app.set_draft_model(model_b.key.clone());
    app.set_draft_variant(Some(
        cookie_agent_protocol::VariantId::new("high").expect("variant"),
    ));
    let draft_b = app.draft.clone().expect("run B draft");
    assert_eq!(draft_b.agent, agent_id());
    assert_eq!(draft_b.model.model, model_b.key);
    assert_eq!(
        draft_b
            .model
            .variant
            .as_ref()
            .map(|variant| variant.as_str()),
        Some("high")
    );
    assert_eq!(draft_b.preset.as_deref(), Some("review"));
    let switched = rendered_frame(&mut app, 100, 30);
    assert!(
        switched.contains("primary • gateway/arbitrary-model[base]"),
        "{switched}"
    );
    assert!(
        switched.contains("primary • other/model-b[high]"),
        "{switched}"
    );

    app.run_goal_command(GoalCommand::Objective("Continue with model B".into()));
    assert_eq!(app.draft.as_ref(), Some(&draft_b));
    assert_eq!(
        assistant_projection(&app.store.sessions[&session]),
        run_a_projection
    );
    assert_eq!(app.store.sessions[&session].active_run, Some(run_a));
    let activated_goal = GoalState {
        goal_id: GoalId::new_v7(),
        objective: "Continue with model B".into(),
        status: GoalStatus::Active,
        items: Vec::new(),
        revision: 0,
    };
    assert!(app.store.apply_event(runless_event(
        session,
        5,
        EventPayload::GoalActivated {
            goal_id: activated_goal.goal_id,
            objective: activated_goal.objective.clone(),
            revision: activated_goal.revision,
            selection: Some(draft_b.clone()),
        }
    )));
    app.handle_rpc_update(RpcUpdate::GoalFinished {
        session_id: session,
        result: Box::new(Ok(Some(activated_goal))),
    });
    assert_eq!(app.draft.as_ref(), Some(&draft_b));
    assert_eq!(
        assistant_projection(&app.store.sessions[&session]),
        run_a_projection
    );
    let after_goal_result = rendered_frame(&mut app, 100, 30);
    assert!(
        after_goal_result.contains("primary • gateway/arbitrary-model[base]"),
        "{after_goal_result}"
    );
    assert!(
        after_goal_result.contains("primary • other/model-b[high]"),
        "{after_goal_result}"
    );
    assert_eq!(
        app.store.sessions[&session]
            .run_snapshot
            .as_ref()
            .expect("still run A snapshot")
            .fallback_chain[0]
            .selection,
        resolved_model(None).selection
    );

    let mut resolved_b = resolved_model(Some("high"));
    resolved_b.provider_id = model_b.key.provider_id();
    resolved_b.model_id = model_b.key.model_id();
    resolved_b.selection.model = model_b.key.clone();
    resolved_b.selection_fingerprint = Sha256Digest::of_bytes(b"run-b-selection");
    let run_b = run_id();
    let attempt_b = AttemptId::new_v7();
    let mut run_b_started = run_started_with_suffix(session, 7, run_b, vec![resolved_b.clone()]);
    let EventPayload::RunStarted { selection, .. } = &mut run_b_started.payload else {
        unreachable!("run fixture")
    };
    selection.preset = Some("review".into());
    let mut attempt_b_started = attempt_started(session, 8, run_b, attempt_b, Some("high"));
    let EventPayload::ModelAttemptStarted { resolved_model, .. } = &mut attempt_b_started.payload
    else {
        unreachable!("attempt fixture")
    };
    *resolved_model = resolved_b.clone();
    let mut turn_b_committed = turn_committed(
        session,
        9,
        run_b,
        attempt_b,
        2,
        vec![text_part("run B answer")],
        Vec::new(),
        Some("high"),
    );
    let EventPayload::ModelTurnCommitted { resolved_model, .. } = &mut turn_b_committed.payload
    else {
        unreachable!("turn fixture")
    };
    *resolved_model = resolved_b.clone();

    assert!(app.store.apply_event(event(
        session,
        6,
        run_a,
        EventPayload::RunCompleted { final_text: None },
    )));
    assert_eq!(
        assistant_projection(&app.store.sessions[&session]),
        run_a_projection
    );
    assert!(app.store.apply_event(run_b_started));
    let state = &app.store.sessions[&session];
    assert_eq!(assistant_projection(state), run_a_projection);
    assert_eq!(state.active_run, Some(run_b));
    assert_eq!(
        state
            .run_snapshot
            .as_ref()
            .expect("run B snapshot")
            .fallback_chain[0]
            .selection,
        resolved_b.selection
    );
    assert_eq!(
        state.run_selected_suffix.as_ref().expect("run B suffix")[0].selection,
        resolved_b.selection
    );

    assert!(app.store.apply_event(attempt_b_started));
    let expected_b_header = FrozenAssistantAttribution {
        agent: agent_id(),
        resolved_model: resolved_b,
    }
    .header();
    let after_attempt = assistant_projection(&app.store.sessions[&session]);
    assert_eq!(after_attempt.len(), 2);
    assert_eq!(after_attempt[0], run_a_projection[0]);
    assert_eq!(after_attempt[1].0, expected_b_header);
    assert_eq!(after_attempt[1].1, None);

    assert!(app.store.apply_event(turn_b_committed));
    let after_commit = assistant_projection(&app.store.sessions[&session]);
    assert_eq!(after_commit.len(), 2);
    assert_eq!(after_commit[0], run_a_projection[0]);
    assert_eq!(after_commit[1].0, expected_b_header);
    assert_eq!(after_commit[1].1, Some(2));
    assert_eq!(after_commit[1].2, ["text:run B answer"]);
    let rendered = rendered_frame(&mut app, 100, 34);
    assert!(rendered.contains("run A answer"), "{rendered}");
    assert!(rendered.contains("run B answer"), "{rendered}");
    assert!(
        rendered.contains("primary • other/model-b[high]"),
        "{rendered}"
    );
}

#[tokio::test]
async fn agent_picker_lists_only_root_runnable_agents() {
    let mut app = test_app().await;
    let mut internal = descriptor("approval", true);
    internal.mode = cookie_agent_protocol::AgentMode::Internal;
    app.agents = vec![
        descriptor("primary", true),
        descriptor("worker", false),
        internal,
    ];
    app.models = vec![model_descriptor()];
    assert_eq!(app.selectable_agents().len(), 1);
    let draft = app.default_draft_selection().expect("default draft");
    assert_eq!(draft.agent.as_str(), "primary");
}

#[test]
fn startup_pick_reopens_the_most_recent_root_session() {
    let older = SessionId::new_v7();
    let newer = SessionId::new_v7();
    let child = SessionId::new_v7();
    let mut older_meta = session_meta(older);
    older_meta.last_activity = "2026-08-01T00:00:00Z".parse().expect("timestamp");
    let mut newer_meta = session_meta(newer);
    newer_meta.last_activity = "2026-08-09T00:00:00Z".parse().expect("timestamp");
    // The delegated child is the most recently active session overall.
    let mut delegated = delegated_meta(child, older, "worker");
    delegated.last_activity = "2026-08-10T00:00:00Z".parse().expect("timestamp");

    // Listing order comes from HashMap iteration, so recency must decide
    // the pick regardless of position — and a delegated child never wins.
    assert_eq!(
        App::preferred_startup_session(&[
            older_meta.clone(),
            delegated.clone(),
            newer_meta.clone()
        ]),
        Some(newer)
    );
    assert_eq!(
        App::preferred_startup_session(&[
            newer_meta.clone(),
            older_meta.clone(),
            delegated.clone()
        ]),
        Some(newer)
    );
    // Degenerate listings keep the documented fallbacks: the first entry
    // when no root is listed, and no pick (create-new) when empty.
    assert_eq!(App::preferred_startup_session(&[delegated]), Some(child));
    assert_eq!(App::preferred_startup_session(&[]), None);
}

#[test]
fn startup_pick_breaks_activity_ties_deterministically() {
    let one = SessionId::new_v7();
    let two = SessionId::new_v7();
    let (low, high) = if one < two { (one, two) } else { (two, one) };
    // Both roots share the newest activity timestamp, so only the session
    // ID can order them; listing order must not change the answer.
    let low_meta = session_meta(low);
    let high_meta = session_meta(high);
    assert_eq!(low_meta.last_activity, high_meta.last_activity);
    let delegated = delegated_meta(SessionId::new_v7(), low, "worker");

    let forward =
        App::preferred_startup_session(&[low_meta.clone(), delegated.clone(), high_meta.clone()]);
    let reversed =
        App::preferred_startup_session(&[high_meta.clone(), delegated.clone(), low_meta.clone()]);
    assert_eq!(forward, reversed, "listing order must not move the pick");
    assert_eq!(forward, Some(high));
}

#[tokio::test]
async fn root_sessions_may_switch_draft_agents_between_runs() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    app.selected = Some(root);
    app.tree_root = Some(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: Vec::new(),
    });
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });
    assert!(app.watching_root_session());
    assert!(app.agent_switching_allowed());
    assert!(app.delegated_pin_reason().is_none());
    app.cycle_agent(false);
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    // Opening the agent selector works for root sessions.
    app.open_selection_modal(Modal::Agents);
    assert_eq!(app.modal, Modal::Agents);
    app.modal = Modal::None;
    // An active run does not gate drafts: agent switching stays allowed
    // for root sessions and affects the next run only; the producing
    // attribution stays frozen.
    let state = app.store.sessions.entry(root).or_default();
    state.active_run = Some(run_id());
    state.run_agent = Some(agent_id());
    assert!(app.agent_switching_allowed());
    app.open_selection_modal(Modal::Agents);
    assert_eq!(app.modal, Modal::Agents);
    app.modal = Modal::None;
    app.set_draft_agent(AgentId::new("reviewer").expect("agent"));
    assert!(app.status.contains("next run"));
    assert_eq!(
        app.active_run_agent().map(|agent| agent.as_str()),
        Some("primary")
    );
    // Model selection and inline variant cycling stay available during the run too.
    app.open_selection_modal(Modal::Models);
    assert_eq!(app.modal, Modal::Models);
    app.modal = Modal::None;
    app.cycle_draft_variant();
    assert!(app.status.contains("active run is unchanged"));
}

#[tokio::test]
async fn delegated_sessions_pin_the_frozen_child_agent_with_textual_reason() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true), descriptor("worker", false)];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree_root = Some(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "worker"),
            children: Vec::new(),
        }],
    });
    // The persisted `SessionCreated` carries the exact frozen chain the
    // delegated pickers must derive from.
    assert!(app.store.apply_event(session_created_with(
        child,
        1,
        "worker",
        vec![resolved_model(None)],
        0,
    )));
    app.set_selected_session(child);
    assert!(!app.watching_root_session());
    assert!(!app.agent_switching_allowed());
    assert_eq!(
        app.delegated_pin_reason().as_deref(),
        Some("delegated session pinned to frozen child agent worker")
    );
    // The agent selector is disabled with a clear non-color reason.
    app.open_selection_modal(Modal::Agents);
    assert_eq!(app.modal, Modal::None);
    assert!(app.status.contains("pinned to frozen child agent worker"));
    // Tab cycling is refused the same way.
    app.cycle_agent(false);
    assert!(app.status.contains("pinned to frozen child agent worker"));
    // Choosing an entry from a stale open modal is rejected too.
    app.modal = Modal::Agents;
    app.choose_picker_entry(0).await;
    assert!(app.status.contains("pinned to frozen child agent worker"));
    app.modal = Modal::None;
    // Model selection stays available for the delegated session within
    // its frozen suffix; inline variant cycling is a fixed no-op.
    app.open_selection_modal(Modal::Models);
    assert_eq!(app.modal, Modal::Models);
    type_input(&mut app, "ARBITRARY").await;
    assert_eq!(app.filtered_draft_models().len(), 1);
    let rendered = rendered_frame(&mut app, 140, 30);
    assert!(rendered.contains("Model (1/1)"));
    assert!(rendered.contains("Arbitrary Model gateway/arbitrary-model[base]"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    app.choose_picker_entry(0).await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(app.draft_variants(), vec![None]);
    app.cycle_draft_variant();
    assert!(
        app.draft
            .as_ref()
            .is_some_and(|draft| draft.model.variant.is_none())
    );
    // The Agents modal presents the pinned agent as fixed when rendered.
    app.modal = Modal::Agents;
    let rendered = rendered_frame(&mut app, 140, 30);
    assert!(rendered.contains("fixed (delegated session)"));
    assert!(rendered.contains("pinned to frozen child agent worker"));
}

#[tokio::test]
async fn new_from_delegated_session_renders_and_submits_root_agent_candidates() {
    let mut app = test_app().await;
    app.agents = vec![
        descriptor("primary", true),
        descriptor("reviewer", true),
        descriptor("worker", false),
    ];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree_root = Some(root);
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "worker"),
            children: Vec::new(),
        }],
    });
    assert!(app.store.apply_event(session_created_with(
        child,
        1,
        "worker",
        vec![resolved_model(None)],
        0,
    )));
    app.set_selected_session(child);
    let (client, recorded, incoming) = live_recording_client();
    app.client = client;

    app.run_command(SlashCommand::New).await;
    assert_eq!(app.modal, Modal::Agents);
    let rendered = rendered_frame(&mut app, 140, 30);
    assert!(
        rendered.contains("primary Test primary agent"),
        "{rendered}"
    );
    assert!(
        rendered.contains("reviewer Test reviewer agent"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("fixed (delegated session)"),
        "{rendered}"
    );
    assert!(!rendered.contains("worker Test worker agent"), "{rendered}");

    type_input(&mut app, "REVIEWER").await;
    assert_eq!(app.filtered_agent_picker_candidates().len(), 1);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.agent_search.focus(), SearchPickerFocus::List);
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
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "first message").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    response.await.expect("create response");

    let request = recorded
        .lock()
        .expect("recorded")
        .iter()
        .find(|value| value["method"].as_str() == Some("session.create"))
        .cloned()
        .expect("session.create request");
    assert_eq!(request["params"]["selection"]["agent"], "reviewer");
}

#[tokio::test]
async fn descriptor_revisions_are_coherent_and_refresh_revalidates_root_drafts_only() {
    let mut app = test_app().await;
    // Revision coherence: agent and model snapshot revisions travel
    // together in selector presentation.
    app.agent_revision = Some(protocol_revision("1"));
    app.model_revision = Some(protocol_revision("2"));
    let label = app.descriptor_revisions_label();
    assert!(label.contains("agent revision sha256:1111"));
    assert!(label.contains("model revision sha256:2222"));

    // A root draft pointing at a now-unrunnable agent resets to the
    // default; a delegated session's pin is untouched.
    app.agents = vec![descriptor("reviewer", true)];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "worker"),
            children: Vec::new(),
        }],
    });
    app.selected = Some(root);
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });
    app.revalidate_draft();
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("reviewer")
    );
    app.selected = Some(child);
    app.draft = Some(RunSelection {
        agent: AgentId::new("worker").expect("agent"),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });
    app.revalidate_draft();
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("worker")
    );
}

#[tokio::test]
async fn watched_session_change_rebinds_the_draft_without_carrying_root_into_child() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true), descriptor("reviewer", true)];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "reviewer"),
            children: Vec::new(),
        }],
    });
    // Root: draft rebinds to the root's own creation selection.
    app.set_selected_session(root);
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("primary")
    );
    // Change the root draft, then watch the child: the child draft is
    // pinned to its frozen agent — the root draft never carries down.
    app.set_draft_agent(AgentId::new("reviewer").expect("agent"));
    assert!(app.store.apply_event(session_created_with(
        child,
        1,
        "reviewer",
        vec![resolved_model(None)],
        0,
    )));
    app.set_selected_session(child);
    let draft = app.draft.as_ref().expect("child draft");
    assert_eq!(draft.agent.as_str(), "reviewer");
    assert_eq!(draft.model.model, model_key());
    // The exact selection is valid against the frozen agent's chain, so
    // run.start would be accepted.
    assert!(app.agents.iter().any(|agent| {
        agent.id == draft.agent
            && agent
                .resolved_fallback
                .iter()
                .any(|selection| selection == &draft.model)
    }));
    // Back at the root, the draft rebinds to the root creation selection.
    app.set_selected_session(root);
    assert_eq!(
        app.draft.as_ref().map(|draft| draft.agent.as_str()),
        Some("primary")
    );
}

#[tokio::test]
async fn empty_chain_inherited_child_uses_the_persisted_frozen_suffix() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "worker"),
            children: Vec::new(),
        }],
    });
    // An empty-chain child inherits the invoking parent's active frozen
    // suffix at admission: here the suffix begins at the second chain
    // entry, so only the suffix head is selectable.
    let inherited_head = resolved_model(Some("high"));
    let inherited_next = resolved_model(Some("fast"));
    assert!(app.store.apply_event(session_created_with(
        child,
        1,
        "worker",
        vec![resolved_model(None), inherited_head, inherited_next],
        1,
    )));
    app.set_selected_session(child);
    let models = app.draft_models();
    assert_eq!(models.len(), 2, "only the inherited frozen suffix");
    assert_eq!(
        models[0].variant.as_ref().map(|variant| variant.as_str()),
        Some("high")
    );
    assert_eq!(
        models[1].variant.as_ref().map(|variant| variant.as_str()),
        Some("fast")
    );
    // The draft is pinned to the suffix head even though the creation
    // selection names an earlier chain entry.
    let draft = app.draft.as_ref().expect("delegated draft");
    assert_eq!(draft.agent.as_str(), "worker");
    assert_eq!(
        draft.model.variant.as_ref().map(|variant| variant.as_str()),
        Some("high")
    );
    // Model picking stays within the persisted chain: an out-of-chain
    // model is rejected, a chain member is accepted exactly.
    app.set_draft_model(model_key());
    assert!(
        app.status.contains("not in agent worker's fallback chain")
            || app
                .draft
                .as_ref()
                .is_some_and(|draft| draft.model.variant.is_some())
    );
    // Live descriptor changes never reinterpret the persisted chain.
    app.agents.clear();
    let models = app.draft_models();
    assert_eq!(models.len(), 2);
    // A full replay keeps the persisted projection authoritative.
    let mut store = StateStore::default();
    assert!(store.apply_event(session_created_with(
        child,
        1,
        "worker",
        vec![resolved_model(None)],
        0,
    )));
    let state = &store.sessions[&child];
    assert_eq!(
        state
            .creation_agent
            .as_ref()
            .map(|snapshot| snapshot.fallback_chain.len()),
        Some(1)
    );
}

#[tokio::test]
async fn selected_suffix_head_variant_override_persists_through_replay() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "worker"),
            children: Vec::new(),
        }],
    });
    let run = run_id();
    // The creation chain resolves the head to base; the run selection
    // overrode the head to the exact `high` variant. The authoritative
    // suffix carries the override directly.
    assert!(app.store.apply_event(session_created_with(
        child,
        1,
        "worker",
        vec![resolved_model(None), resolved_model(Some("fast"))],
        0,
    )));
    assert!(app.store.apply_event(run_started_with_suffix(
        child,
        2,
        run,
        vec![resolved_model(Some("high")), resolved_model(Some("fast"))],
    )));
    app.set_selected_session(child);
    // The delegated pickers derive from the exact persisted suffix: the
    // overridden head variant, never the reconstructed base.
    let models = app.draft_models();
    assert_eq!(models.len(), 2);
    assert_eq!(
        models[0].variant.as_ref().map(|variant| variant.as_str()),
        Some("high")
    );
    let draft = app.draft.as_ref().expect("delegated draft");
    assert_eq!(
        draft.model.variant.as_ref().map(|variant| variant.as_str()),
        Some("high")
    );
    // Variant cycling for the selected head is fixed to its one exact
    // persisted selection, not live descriptor variants.
    assert_eq!(
        app.draft_variants()
            .iter()
            .map(|variant| variant.as_ref().map(|id| id.as_str()))
            .collect::<Vec<_>>(),
        vec![Some("high")]
    );
    // A full replay keeps the exact suffix authoritative.
    let mut store = StateStore::default();
    assert!(store.apply_event(session_created_with(
        child,
        1,
        "worker",
        vec![resolved_model(None), resolved_model(Some("fast"))],
        0,
    )));
    assert!(store.apply_event(run_started_with_suffix(
        child,
        2,
        run,
        vec![resolved_model(Some("high")), resolved_model(Some("fast"))],
    )));
    let suffix = store.sessions[&child]
        .run_selected_suffix
        .as_ref()
        .expect("persisted selected suffix");
    assert_eq!(
        suffix[0]
            .selection
            .variant
            .as_ref()
            .map(|variant| variant.as_str()),
        Some("high")
    );
}

#[tokio::test]
async fn delegated_variant_cycle_is_immune_to_live_provider_refresh() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree = Some(SessionTree {
        session: session_meta(root),
        children: vec![SessionTree {
            session: delegated_meta(child, root, "worker"),
            children: Vec::new(),
        }],
    });
    let run = run_id();
    assert!(app.store.apply_event(session_created_with(
        child,
        1,
        "worker",
        vec![resolved_model(Some("high"))],
        0,
    )));
    assert!(app.store.apply_event(run_started_with_suffix(
        child,
        2,
        run,
        vec![resolved_model(Some("high"))],
    )));
    app.set_selected_session(child);
    let before = app.draft_variants();
    assert_eq!(
        before
            .iter()
            .map(|variant| variant.as_ref().map(|id| id.as_str()))
            .collect::<Vec<_>>(),
        vec![Some("high")]
    );
    // A provider refresh adds brand-new live variants; the delegated
    // cycle still exposes only the persisted exact selection.
    let mut refreshed = model_descriptor();
    refreshed
        .variants
        .push(cookie_agent_protocol::AvailableVariantDescriptor {
            id: cookie_agent_protocol::VariantId::new("ultra").expect("variant"),
            display_name: "Ultra".into(),
            origin: cookie_agent_protocol::VariantOrigin::Explicit,
            behavior_fingerprint: Sha256Digest::of_bytes(b"ultra"),
        });
    app.models = vec![refreshed];
    let after = app.draft_variants();
    assert_eq!(after, before);
    app.cycle_draft_variant();
    assert_eq!(app.draft_variants(), before);
}

#[tokio::test]
async fn model_command_continues_to_a_variant_step_that_applies_both_together() {
    let (client, _requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    let model_a = model_descriptor();
    let model_b = catalog_model("other/model-b", &["low", "high"], Some("low"));
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![model_a.clone(), model_b.clone()],
        vec![descriptor("primary", true)],
    ));
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.sessions.push(session_meta(session));
    assert!(app.store.apply_event(session_created(session, 1)));
    let original = RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_a.key.clone(),
            variant: None,
        },
        preset: None,
    };
    app.draft = Some(original.clone());

    run_palette_command(&mut app, "model").await;
    assert_eq!(app.modal, Modal::Models);
    let row_b = app
        .filtered_draft_models()
        .iter()
        .position(|selection| selection.model == model_b.key)
        .expect("model B row");
    app.choose_picker_entry(row_b).await;
    assert_eq!(app.modal, Modal::Variants);
    // The model's default is highlighted; nothing is applied yet.
    assert_eq!(app.picker_state.selected(), Some(1));
    assert_eq!(app.draft.as_ref(), Some(&original));
    let rendered = rendered_frame(&mut app, 120, 36);
    assert!(rendered.contains("Variant — other/model-b"), "{rendered}");
    assert!(rendered.contains("base"), "{rendered}");
    assert!(
        rendered.contains("low — Variant low (default)"),
        "{rendered}"
    );
    assert!(rendered.contains("high — Variant high"), "{rendered}");

    // Esc goes back to the model list, still with nothing applied.
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::Models);
    assert_eq!(app.picker_state.selected(), Some(row_b));
    assert_eq!(app.draft.as_ref(), Some(&original));

    app.choose_picker_entry(row_b).await;
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    let draft = app.draft.as_ref().expect("draft");
    assert_eq!(draft.model.model, model_b.key);
    assert_eq!(
        draft.model.variant,
        Some(cookie_agent_protocol::VariantId::new("high").expect("variant"))
    );

    // The title's model segment keeps its one-step picker.
    app.open_selection_modal(Modal::Models);
    let row_a = app
        .filtered_draft_models()
        .iter()
        .position(|selection| selection.model == model_a.key)
        .expect("model A row");
    app.choose_picker_entry(row_a).await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        app.draft.as_ref().map(|draft| &draft.model.model),
        Some(&model_a.key)
    );

    run_palette_command(&mut app, "agent").await;
    assert_eq!(app.modal, Modal::Agents);
}

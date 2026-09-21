use std::{
    fs,
    sync::{Arc, atomic::Ordering},
};

use cookie_agent_protocol::{
    AgentId, ClientRunId, EventPayload, EventSubscriptionMessage, RunStartParams, SessionId,
    SessionStatus,
};

use crate::DelegateInvocation;

use super::support::*;

#[tokio::test]
async fn terminal_child_resume_reuses_identity_refreshes_link_and_notifies_again() {
    let (endpoint, responses, server) = scripted_channel_server(6).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("resume parent");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "create a resumable child",
            scripted_tool_body(
                "resume-fresh",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Original identity",
                    "prompt":"first child task",
                    "background":true
                }),
            ),
        ))
        .expect("fresh tool response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "first child task",
            scripted_text_body("first child result"),
        ))
        .expect("first child response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent after first delegation"),
        ))
        .expect("first parent response");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-terminal-first").expect("run ID"),
                selection: selection.clone(),
                input: "create a resumable child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first parent run");
    await_session_change(
        &fixture.engine,
        parent.session_id,
        "first child completion",
        || {
            (fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .is_some_and(|child| child.status == SessionStatus::Completed)
                && fixture
                    .engine
                    .get_session(parent.session_id)
                    .is_ok_and(|parent| parent.status == SessionStatus::Completed))
            .then_some(())
        },
    )
    .await;
    let child_session_id = fixture
        .engine
        .children(parent.session_id)
        .expect("children")[0]
        .session_id;
    let original_title = fixture
        .engine
        .get_session(child_session_id)
        .expect("original child")
        .title;
    let foreign = fixture
        .engine
        .create_session(
            fixture
                .engine
                .get_session(parent.session_id)
                .expect("parent selection")
                .creation_selection,
        )
        .expect("foreign top-level session");
    let worker = AgentId::new("worker").expect("worker agent");
    let self_error = fixture
        .engine
        .validate_resume_target(parent.session_id, parent.session_id, &worker, None)
        .expect_err("self resume is rejected");
    assert!(self_error.to_string().contains("itself"));
    let foreign_error = fixture
        .engine
        .validate_resume_target(parent.session_id, foreign.session_id, &worker, None)
        .expect_err("foreign resume is rejected");
    assert!(foreign_error.to_string().contains("prior direct child"));
    let missing_id = SessionId::new_v7();
    let missing_error = fixture
        .engine
        .validate_resume_target(parent.session_id, missing_id, &worker, None)
        .expect_err("unknown resume is rejected");
    assert!(missing_error.to_string().contains("was not found"));
    let ancestor_error = fixture
        .engine
        .validate_resume_target(child_session_id, parent.session_id, &worker, None)
        .expect_err("ancestor resume is rejected");
    assert!(ancestor_error.to_string().contains("ancestor"));
    let preset_error = fixture
        .engine
        .validate_resume_target(parent.session_id, child_session_id, &worker, Some("python"))
        .expect_err("cross-preset child resume is rejected");
    assert!(preset_error.to_string().contains("different agent preset"));

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "resume the existing child",
            scripted_tool_body(
                "resume-terminal",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type":"worker",
                    "description":"Do not replace the title",
                    "prompt":"second child task",
                    "background":true,
                    "resume_session_id":child_session_id
                }),
            ),
        ))
        .expect("resume tool response");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "second child task",
            scripted_text_body("second child result"),
        ))
        .expect("second child response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent after resumed delegation"),
        ))
        .expect("second parent response");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-terminal-second").expect("run ID"),
                selection,
                input: "resume the existing child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("second parent run");
    await_projection(
        &fixture.engine,
        parent.session_id,
        "resumed child completion and teaser",
        |parent_projection| {
            let child = fixture
                .engine
                .inner
                .store
                .get(child_session_id)
                .expect("resumed child");
            let notifications = parent_projection
                .log
                .events()
                .iter()
                .filter(|event| {
                    matches!(
                        event.payload,
                        EventPayload::DelegateFinishedV2 {
                            session_id,
                            ..
                        } if session_id == child_session_id
                    )
                })
                .count();
            child.status == SessionStatus::Completed && child.runs.len() == 2 && notifications == 2
        },
    )
    .await;
    assert_eq!(
        fixture
            .engine
            .children(parent.session_id)
            .expect("children")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .engine
            .get_session(child_session_id)
            .expect("resumed metadata")
            .title,
        original_title
    );
    let parent_events = fixture
        .engine
        .inner
        .store
        .get(parent.session_id)
        .expect("linked parent")
        .log
        .events();
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| matches!(
                event.payload,
                EventPayload::ToolCallLinked {
                    child_session_id: linked,
                    ..
                } if linked == child_session_id
            ))
            .count(),
        2
    );
    let entries = fixture.engine.inner.delegation_events.entries();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.started));
    assert!(entries.iter().all(|entry| entry.child_run_id.is_some()));
    let old_parent_run_id = entries[0].reservation.parent_run_id;
    let resumed_child_run_id = entries[1].child_run_id.expect("resumed child run ID");
    let result = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            child_session_id,
            false,
            0,
            20,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("refreshed result link");
    assert!(result.output.contains("second child result"));
    assert_eq!(
        with_watchdog("server fixture completion", server)
            .await
            .expect("resume server")
            .len(),
        6
    );
    let parent_event_path = fixture
        .engine
        .inner
        .store
        .session_dir(parent.session_id)
        .join("events.jsonl");
    let child_event_path = fixture
        .engine
        .inner
        .store
        .session_dir(child_session_id)
        .join("events.jsonl");
    fixture.engine.shutdown().await;

    for (path, run_id, replacement) in [
        (
            parent_event_path,
            old_parent_run_id,
            EventPayload::RunCancelled { reason: None },
        ),
        (
            child_event_path,
            resumed_child_run_id,
            EventPayload::RunInterrupted { reason: None },
        ),
    ] {
        let mut events = fs::read_to_string(&path)
            .expect("rewrite recovery isolation events")
            .lines()
            .map(|line| {
                serde_json::from_str::<cookie_agent_protocol::StoredEvent>(line)
                    .expect("stored recovery isolation event")
            })
            .collect::<Vec<_>>();
        let terminal = events
            .iter_mut()
            .find(|event| {
                event.run_id == Some(run_id)
                    && matches!(event.payload, EventPayload::RunCompleted { .. })
            })
            .expect("terminal event to rewrite");
        terminal.payload = replacement;
        fs::write(
            &path,
            events
                .iter()
                .map(|event| serde_json::to_string(event).expect("serialize rewritten event"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .expect("persist rewritten recovery isolation events");
    }
    let reopened = reopen_engine(&fixture);
    let recovered_child = reopened
        .inner
        .store
        .get(child_session_id)
        .expect("recovered resumed child");
    assert_eq!(
        recovered_child
            .runs
            .get(&resumed_child_run_id)
            .expect("recovered resumed run")
            .status,
        SessionStatus::Interrupted
    );
    assert!(!recovered_child.log.events().iter().any(|event| {
        event.run_id == Some(resumed_child_run_id)
            && matches!(event.payload, EventPayload::RunCancelled { .. })
    }));
    reopened.shutdown().await;
}

#[tokio::test]
async fn terminal_resume_obeys_the_same_background_slot_and_queue_accounting() {
    let (endpoint, resume_id, queued, release, server) = scripted_queued_resume_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_internal_and_concurrency(
        &endpoint,
        "---\ndescription: Queued resume parent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nTest queued resume.\n",
        None,
        None,
        false,
        Some(1),
        None,
    );
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("queued resume parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("queued-resume-first").expect("run ID"),
                selection: selection.clone(),
                input: "create the resume target".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("queued resume first run");
    let resumed_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "terminal resume target",
        || {
            fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .filter(|child| {
                    child.status == SessionStatus::Completed
                        && fixture
                            .engine
                            .get_session(parent.session_id)
                            .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                })
                .map(|child| child.session_id)
        },
    )
    .await;
    resume_id
        .send(resumed_session_id)
        .expect("send queued resume target");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("queued-resume-second").expect("run ID"),
                selection,
                input: "fill the slot then resume the terminal child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("queued resume second run");
    queued.await.expect("resume queued behind slot holder");
    let resumed_entries = fixture
        .engine
        .inner
        .delegation_events
        .entries()
        .into_iter()
        .filter(|entry| entry.reservation.child_session_id == resumed_session_id)
        .collect::<Vec<_>>();
    assert_eq!(
        resumed_entries.len(),
        2,
        "the duplicate queued resume must not reserve or replace an invocation"
    );
    assert!(resumed_entries[1].child_run_id.is_none());
    assert!(resumed_entries[1].terminal_status.is_none());
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(parent.session_id)
            .expect("duplicate resume parent projection")
            .log
            .events()
            .iter()
            .any(|event| matches!(
                &event.payload,
                EventPayload::ToolCallTerminated { termination }
                    if termination.error.as_ref().is_some_and(|error| {
                        error.message.as_str().contains("in-flight delegation")
                    })
            ))
    );
    assert!(
        fixture
            .engine
            .delegation_queue_contains(resumed_session_id)
            .expect("queue state")
    );
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(resumed_session_id)
            .expect("queued resume target projection")
            .runs
            .len(),
        1
    );
    assert_eq!(
        fixture
            .engine
            .children(parent.session_id)
            .expect("children")
            .iter()
            .filter(|child| child.status == SessionStatus::Running)
            .count(),
        1
    );
    let steered = fixture
        .engine
        .steer_subagent(
            parent.session_id,
            resumed_session_id,
            "queued terminal resume correction".into(),
        )
        .await
        .expect("steer queued terminal resume");
    assert_eq!(steered.metadata["status"], "queued");
    release.send(()).expect("release concurrency slot");
    await_projection(
        &fixture.engine,
        resumed_session_id,
        "queued resume completion",
        |child| child.status == SessionStatus::Completed && child.runs.len() == 2,
    )
    .await;
    assert!(
        !fixture
            .engine
            .delegation_queue_contains(resumed_session_id)
            .expect("drained queue state")
    );
    let resumed_events = fixture
        .engine
        .inner
        .store
        .get(resumed_session_id)
        .expect("steered resumed child")
        .log
        .events();
    assert!(resumed_events.iter().any(|event| {
        event.run_id.is_none()
            && matches!(
                &event.payload,
                EventPayload::UserInputAdmitted { input }
                    if input == "queued terminal resume correction"
            )
    }));
    assert!(resumed_events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input }
            if input == "queued terminal resume correction"
    )));
    assert_eq!(fixture.config.runtime.delegation.max_concurrency, Some(1));
    assert_eq!(
        with_watchdog("server fixture completion", server)
            .await
            .expect("queued resume server")
            .len(),
        10
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn queued_terminal_resume_cancel_is_durable_and_does_not_reuse_pending_steers() {
    let (endpoint, resume_id, queued, _release, server) = scripted_queued_resume_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_internal_and_concurrency(
        &endpoint,
        "---\ndescription: Cancel queued resume parent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nTest queued resume cancellation.\n",
        None,
        None,
        false,
        Some(1),
        None,
    );
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("queued cancel parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("queued-cancel-first").expect("run ID"),
                selection: selection.clone(),
                input: "create terminal cancellation target".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("queued cancel first run");
    let resumed_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "terminal cancellation target",
        || {
            fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .filter(|child| {
                    child.status == SessionStatus::Completed
                        && fixture
                            .engine
                            .get_session(parent.session_id)
                            .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                })
                .map(|child| child.session_id)
        },
    )
    .await;
    resume_id
        .send(resumed_session_id)
        .expect("send cancellation resume target");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("queued-cancel-second").expect("run ID"),
                selection,
                input: "queue then cancel the terminal resume".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("queued cancel second run");
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), queued)
        .await
        .expect("terminal resume queue timeout")
        .expect("terminal resume queued for cancellation");
    fixture
        .engine
        .steer_subagent(
            parent.session_id,
            resumed_session_id,
            "must not leak into a later resume".into(),
        )
        .await
        .expect("steer before queued cancellation");
    let cancelled = fixture
        .engine
        .cancel_subagent(
            parent.session_id,
            resumed_session_id,
            Some("cancel queued resumed work".into()),
        )
        .await
        .expect("cancel queued terminal resume");
    assert_eq!(cancelled.metadata["status"], "cancelled");
    assert_eq!(
        fixture
            .engine
            .get_session(resumed_session_id)
            .expect("historical terminal session")
            .status,
        SessionStatus::Completed,
        "pending cancellation must not rewrite the previous run's status"
    );
    assert!(
        !fixture
            .engine
            .delegation_queue_contains(resumed_session_id)
            .expect("cancelled queue state")
    );
    let result = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            resumed_session_id,
            false,
            0,
            20,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("cancelled queued resume result");
    // Liveness, not delegation lifecycle: the session's last completed run is
    // still terminal, so the tool reports that status and its last message.
    assert!(result.output.starts_with("<status>completed</status>"));
    assert!(!result.output.contains("queued resume done"));
    let child_events = fixture
        .engine
        .inner
        .store
        .get(resumed_session_id)
        .expect("cancelled resume projection")
        .log
        .events();
    assert!(child_events.iter().any(|event| {
        event.run_id.is_none()
            && matches!(
                &event.payload,
                EventPayload::UserInputRecalled { input }
                    if input == "must not leak into a later resume"
            )
    }));
    assert_eq!(
        fixture
            .engine
            .inner
            .delegation_events
            .entries()
            .last()
            .and_then(|entry| entry.terminal_status),
        Some(SessionStatus::Cancelled)
    );
    server.abort();
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    assert!(
        !reopened
            .delegation_queue_contains(resumed_session_id)
            .expect("reopened cancelled queue state")
    );
    let (_, terminal_status, _) = reopened
        .delegation_registry_snapshot(resumed_session_id)
        .expect("reopened cancelled resume registry");
    assert_eq!(terminal_status, Some(SessionStatus::Cancelled));
    reopened.shutdown().await;
}

#[tokio::test]
async fn concurrent_running_resume_redelivery_reuses_admission_monitor_and_completion() {
    let (endpoint, ready, resume_id, release, server) = scripted_running_resume_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("running resume parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("running-resume-first").expect("run ID"),
                selection: selection.clone(),
                input: "start the long-running child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first resume parent run");
    with_watchdog("first parent and child requests", ready)
        .await
        .expect("first parent and child requests");
    let child_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "active child and completed first parent run",
        || {
            fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .filter(|child| {
                    child.status == SessionStatus::Running
                        && fixture
                            .engine
                            .get_session(parent.session_id)
                            .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                })
                .map(|child| child.session_id)
        },
    )
    .await;
    let original_run_id = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("running child projection")
        .runs
        .keys()
        .next()
        .copied()
        .expect("original child run");
    let (original_invocation_id, _, original_counts_slot) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("original running delegation record");
    assert!(original_counts_slot);
    fixture
        .engine
        .set_delegation_slot_ownership(child_session_id, false, false)
        .expect("model foreground running slot ownership");
    let (resume_admitted, release_resume_admission) = fixture.engine.install_resume_rollback_hook();
    resume_id
        .send(child_session_id)
        .expect("send running resume ID");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("running-resume-second").expect("run ID"),
                selection,
                input: "attach to the active child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("second resume parent run");
    with_watchdog(
        "first running resume admitted before redelivery",
        resume_admitted,
    )
    .await
    .expect("first running resume admitted before redelivery");
    let resumed_entry = fixture
        .engine
        .inner
        .delegation_events
        .entries()
        .last()
        .expect("running resume reservation event")
        .clone();
    let matching_redelivery = DelegateInvocation {
        parent_session_id: resumed_entry.reservation.parent_session_id,
        parent_run_id: resumed_entry.reservation.parent_run_id,
        parent_tool_call_id: resumed_entry.reservation.parent_tool_call_id,
        agent_type: AgentId::new("worker").expect("worker agent"),
        description: resumed_entry.request.description,
        prompt: resumed_entry.request.prompt,
        background: true,
        resume_session_id: resumed_entry
            .request
            .resume_session_id
            .map(|session_id| session_id.to_string()),
        inherit_context: false,
    };
    let duplicate_engine = fixture.engine.clone();
    let duplicate_invocation = matching_redelivery.clone();
    let duplicate =
        tokio::spawn(async move { duplicate_engine.delegate_invoke(duplicate_invocation).await });
    release_resume_admission.notify_one();
    let duplicate_handle = tokio::time::timeout(std::time::Duration::from_secs(3), duplicate)
        .await
        .expect("concurrent resume redelivery")
        .expect("redelivery task")
        .expect("redelivery handle");
    await_projection(
        &fixture.engine,
        child_session_id,
        "running resume prompt admission",
        |child| {
            let prompt_admitted = child.log.events().iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::UserInputAdmitted { input }
                        if event.run_id == Some(original_run_id)
                            && input == "resume active prompt"
                )
            });
            let registry_handed_off = fixture
                .engine
                .delegation_registry_snapshot(child_session_id)
                .is_ok_and(|(invocation_id, _, _)| invocation_id != original_invocation_id);
            prompt_admitted && registry_handed_off
        },
    )
    .await;
    let (resumed_invocation_id, _, resumed_counts_slot) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("resumed running delegation record");
    assert_ne!(resumed_invocation_id, original_invocation_id);
    assert_eq!(duplicate_handle.invocation_id, resumed_invocation_id);
    assert_eq!(duplicate_handle.child_session_id, child_session_id);
    let mode_conflict = fixture
        .engine
        .delegate_invoke(DelegateInvocation {
            background: false,
            ..matching_redelivery
        })
        .await
        .expect_err("background invocation redelivered as foreground");
    assert!(
        mode_conflict
            .to_string()
            .contains("execution mode conflict")
    );
    assert!(
        mode_conflict
            .to_string()
            .contains("durable invocation is background")
    );
    assert!(
        !resumed_counts_slot,
        "running foreground ownership must not be promoted to a root slot"
    );
    let resume_admissions = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("redelivered running child")
        .log
        .events()
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                EventPayload::UserInputAdmitted { input }
                    if input == "resume active prompt"
            )
        })
        .count();
    assert_eq!(resume_admissions, 1);
    release.send(()).expect("release active child response");
    await_projection(
        &fixture.engine,
        parent.session_id,
        "running resumed child completion",
        |parent_projection| {
            let notifications = parent_projection
                .log
                .events()
                .iter()
                .filter(|event| {
                    matches!(
                        event.payload,
                        EventPayload::DelegateFinishedV2 {
                            session_id,
                            ..
                        } if session_id == child_session_id
                    )
                })
                .count();
            fixture
                .engine
                .get_session(child_session_id)
                .is_ok_and(|child| child.status == SessionStatus::Completed)
                && notifications == 2
        },
    )
    .await;
    let child = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("running resumed projection");
    assert_eq!(child.runs.len(), 1);
    assert!(
        child.log.events().iter().any(|event| matches!(
            &event.payload,
            EventPayload::UserInputAdmitted { input }
                if event.run_id == Some(original_run_id) && input == "resume active prompt"
        )),
        "child events: {:#?}",
        child.log.events()
    );
    assert!(child.log.events().iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input }
            if event.run_id == Some(original_run_id) && input == "resume active prompt"
    )));
    let entries = fixture.engine.inner.delegation_events.entries();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.child_run_id == Some(original_run_id))
    );
    let notification_invocations = fixture
        .engine
        .inner
        .store
        .get(parent.session_id)
        .expect("running resume parent notifications")
        .log
        .events()
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::DelegateFinishedV2 {
                invocation_id,
                session_id,
                ..
            } if session_id == child_session_id => Some(invocation_id),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(notification_invocations.len(), 2);
    assert!(notification_invocations.contains(&original_invocation_id));
    assert!(notification_invocations.contains(&resumed_invocation_id));
    let resumed_notifications = fixture
        .engine
        .inner
        .store
        .get(parent.session_id)
        .expect("redelivery completion notifications")
        .log
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::DelegateFinishedV2 { invocation_id, .. }
                    if invocation_id == resumed_invocation_id
            )
        })
        .count();
    assert_eq!(resumed_notifications, 1);
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("running resume server");
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().any(|request| {
        !request.contains("\"role\":\"tool\"") && request.contains("resume active prompt")
    }));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn running_resume_completion_before_actor_admission_keeps_the_old_owner_terminal() {
    let (endpoint, ready, resume_id, release_child, server) =
        scripted_running_resume_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("handoff race parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-handoff-race-first").expect("run ID"),
                selection: selection.clone(),
                input: "start the race child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("handoff race first run");
    with_watchdog("race child active", ready)
        .await
        .expect("race child active");
    let child_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "race child and first parent completion",
        || {
            fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .filter(|child| {
                    child.status == SessionStatus::Running
                        && fixture
                            .engine
                            .get_session(parent.session_id)
                            .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                })
                .map(|child| child.session_id)
        },
    )
    .await;
    let (old_invocation_id, _, _) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("old race registry owner");
    let (admission_reached, release_admission) = fixture.engine.install_resume_admission_hook();
    resume_id
        .send(child_session_id)
        .expect("send race child ID");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-handoff-race-second").expect("run ID"),
                selection,
                input: "race completion against resume admission".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("handoff race second run");
    with_watchdog(
        "resume paused before child actor admission",
        admission_reached,
    )
    .await
    .expect("resume paused before child actor admission");
    release_child
        .send(())
        .expect("complete child before admission");
    await_projection(
        &fixture.engine,
        child_session_id,
        "child completed while resume admission paused",
        |child| child.status == SessionStatus::Completed,
    )
    .await;
    release_admission.notify_one();
    await_session_change(
        &fixture.engine,
        parent.session_id,
        "rejected resume rollback",
        || {
            let entries = fixture.engine.inner.delegation_events.entries();
            let latest_cancelled = entries.last().is_some_and(|entry| {
                entry.reservation.child_session_id == child_session_id
                    && entry.terminal_status == Some(SessionStatus::Cancelled)
            });
            let registry_terminal = fixture
                .engine
                .delegation_registry_snapshot(child_session_id)
                .is_ok_and(|(invocation_id, terminal_status, _)| {
                    invocation_id == old_invocation_id
                        && terminal_status == Some(SessionStatus::Completed)
                });
            (latest_cancelled && registry_terminal).then_some(())
        },
    )
    .await;
    assert!(
        !fixture
            .engine
            .delegation_queue_contains(child_session_id)
            .expect("race queue state")
    );
    server.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn interleaved_steer_then_running_resume_rollback_recalls_only_resume_prompt() {
    let (endpoint, ready, resume_id, release_child, server) =
        scripted_running_resume_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("cancelled admission parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("cancel-resume-admission-first").expect("run ID"),
                selection: selection.clone(),
                input: "start child for cancelled resume".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("cancelled admission first run");
    with_watchdog("cancelled admission child active", ready)
        .await
        .expect("cancelled admission child active");
    let child_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "cancelled admission running child",
        || {
            fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .filter(|child| {
                    child.status == SessionStatus::Running
                        && fixture
                            .engine
                            .get_session(parent.session_id)
                            .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                })
                .map(|child| child.session_id)
        },
    )
    .await;
    let (old_invocation_id, _, _) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("cancelled admission old owner");
    let old_run_id = fixture
        .engine
        .inner
        .delegation_events
        .get(old_invocation_id)
        .and_then(|entry| entry.child_run_id)
        .expect("cancelled admission old run");
    let (admission_reached, release_admission) = fixture.engine.install_resume_rollback_hook();
    resume_id
        .send(child_session_id)
        .expect("send cancelled admission child ID");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("cancel-resume-admission-second").expect("run ID"),
                selection,
                input: "cancel while resume is admitting".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("cancelled admission second run");
    with_watchdog("resume paused after actor admission", admission_reached)
        .await
        .expect("resume paused after actor admission");
    let resumed_invocation_id = fixture
        .engine
        .inner
        .delegation_events
        .entries()
        .last()
        .expect("cancelled resume reservation event")
        .reservation
        .invocation_id;
    assert!(
        fixture
            .engine
            .steer(
                old_run_id,
                "interleaved direct steer".into(),
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await
            .expect("interleaved direct steer")
            .accepted
    );
    let (_, mut parent_events) = fixture
        .engine
        .subscribe(parent.session_id, None)
        .await
        .expect("subscribe before rollback");
    fixture
        .engine
        .cancel_inflight_delegation_for_test(resumed_invocation_id)
        .expect("cancel delegate future during resume admission");
    release_admission.notify_one();
    await_projection(
        &fixture.engine,
        child_session_id,
        "cancelled resume prompt recall",
        |child| {
            let admitted = child
                .log
                .events()
                .iter()
                .find_map(|event| match &event.payload {
                    EventPayload::UserInputAdmitted { input }
                        if input == "resume active prompt" =>
                    {
                        Some(event.seq)
                    }
                    _ => None,
                });
            let recalled = admitted.is_some_and(|admission_seq| {
                child.log.events().iter().any(|event| {
                    matches!(
                        &event.payload,
                        EventPayload::UserInputRecalledV2 {
                            user_input_seq,
                            input,
                        } if *user_input_seq == admission_seq
                            && input == "resume active prompt"
                    )
                })
            });
            let steer_preserved = child.log.events().iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::UserInputAdmitted { input } if input == "interleaved direct steer"
                )
            });
            recalled && steer_preserved
        },
    )
    .await;
    // Recall is a child event; cancellation is a parent journal event. Require
    // live delivery as well as durable state, without waiting on an idle child.
    with_watchdog("cancelled delegation live event", async {
        loop {
            match parent_events.recv().await.expect("parent subscription open") {
                EventSubscriptionMessage::Event { event } if matches!(
                    event.payload,
                    EventPayload::DelegationFinished { invocation_id, status: SessionStatus::Cancelled, .. }
                        if invocation_id == resumed_invocation_id
                ) => break,
                EventSubscriptionMessage::Gap { .. } => panic!("unexpected parent event gap"),
                _ => {}
            }
        }
    }).await;
    assert_eq!(
        fixture
            .engine
            .inner
            .delegation_events
            .get(resumed_invocation_id)
            .expect("cancelled invocation")
            .terminal_status,
        Some(SessionStatus::Cancelled)
    );
    let (registry_invocation, terminal_status, _) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("cancelled admission registry");
    assert_eq!(registry_invocation, old_invocation_id);
    assert_eq!(terminal_status, None);
    let child = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("running child after cancelled resume");
    assert_eq!(child.status, SessionStatus::Running);
    assert!(!child.log.events().iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input == "resume active prompt"
    )));
    release_child
        .send(())
        .expect("release original child response");
    await_session_change(
        &fixture.engine,
        parent.session_id,
        "interleaved steer promotion and single old completion",
        || {
            let child = fixture
                .engine
                .inner
                .store
                .get(child_session_id)
                .expect("interleaved steer child projection");
            let steer_submitted = child.log.events().iter().any(|event| matches!(
                &event.payload,
                EventPayload::UserInputSubmitted { input } if input == "interleaved direct steer"
            ));
            let old_notifications = fixture
                .engine
                .inner
                .store
                .get(parent.session_id)
                .expect("interleaved steer parent projection")
                .log
                .events()
                .iter()
                .filter(|event| {
                    matches!(
                        &event.payload,
                        EventPayload::DelegateFinishedV2 { invocation_id, .. }
                            if *invocation_id == old_invocation_id
                    )
                })
                .count();
            (steer_submitted && old_notifications == 1).then_some(())
        },
    )
    .await;
    // This negative assertion ensures no duplicate completion is emitted later.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let old_notifications = fixture
        .engine
        .inner
        .store
        .get(parent.session_id)
        .expect("completed interleaved steer parent")
        .log
        .events()
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                EventPayload::DelegateFinishedV2 { invocation_id, .. }
                    if *invocation_id == old_invocation_id
            )
        })
        .count();
    assert_eq!(old_notifications, 1);
    server.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn running_resume_monitor_install_failure_never_admits_the_prompt() {
    let (endpoint, ready, resume_id, _release_child, server) =
        scripted_running_resume_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("monitor failure parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-monitor-failure-first").expect("run ID"),
                selection: selection.clone(),
                input: "start child for monitor failure".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("monitor failure first run");
    with_watchdog("monitor failure child active", ready)
        .await
        .expect("monitor failure child active");
    let child_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "monitor failure running child",
        || {
            fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .filter(|child| {
                    child.status == SessionStatus::Running
                        && fixture
                            .engine
                            .get_session(parent.session_id)
                            .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                })
                .map(|child| child.session_id)
        },
    )
    .await;
    let (old_invocation_id, _, _) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("monitor failure old owner");
    fixture
        .engine
        .inner
        .resume_monitor_failures
        .store(1, Ordering::Release);
    resume_id
        .send(child_session_id)
        .expect("send monitor failure child ID");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-monitor-failure-second").expect("run ID"),
                selection,
                input: "resume with failed monitor installation".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("monitor failure second run");
    await_session_change(
        &fixture.engine,
        parent.session_id,
        "monitor failure terminal event state",
        || {
            fixture
                .engine
                .inner
                .delegation_events
                .entries()
                .last()
                .is_some_and(|entry| {
                    entry.reservation.child_session_id == child_session_id
                        && entry.child_run_id.is_some()
                        && entry.terminal_status == Some(SessionStatus::Cancelled)
                })
                .then_some(())
        },
    )
    .await;
    let child = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("monitor failure child projection");
    assert!(!child.log.events().iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input } if input == "resume active prompt"
    )));
    let (registry_invocation, terminal_status, _) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("monitor failure registry owner");
    assert_eq!(registry_invocation, old_invocation_id);
    assert_eq!(terminal_status, None);
    server.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancellation_between_run_attachment_and_publication_terminalizes_invocation() {
    let (endpoint, ready, resume_id, _release_child, server) =
        scripted_running_resume_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("attachment cancellation parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-attachment-cancel-first").expect("run ID"),
                selection: selection.clone(),
                input: "start child for attachment cancellation".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("attachment cancellation first run");
    with_watchdog("attachment cancellation child active", ready)
        .await
        .expect("attachment cancellation child active");
    let child_session_id = await_session_change(
        &fixture.engine,
        parent.session_id,
        "attachment cancellation running child",
        || {
            fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .first()
                .filter(|child| {
                    child.status == SessionStatus::Running
                        && fixture
                            .engine
                            .get_session(parent.session_id)
                            .is_ok_and(|parent| parent.status == SessionStatus::Completed)
                })
                .map(|child| child.session_id)
        },
    )
    .await;
    let (old_invocation_id, _, _) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("attachment cancellation old owner");
    let (attachment_reached, release_attachment) = fixture.engine.install_resume_attachment_hook();
    resume_id
        .send(child_session_id)
        .expect("send attachment cancellation child ID");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("resume-attachment-cancel-second").expect("run ID"),
                selection,
                input: "cancel after durable run attachment".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("attachment cancellation second run");
    with_watchdog(
        "resume paused after durable run attachment",
        attachment_reached,
    )
    .await
    .expect("resume paused after durable run attachment");
    let invocation_id = fixture
        .engine
        .inner
        .delegation_events
        .entries()
        .last()
        .expect("attached resume event entry")
        .reservation
        .invocation_id;
    fixture
        .engine
        .cancel_inflight_delegation_for_test(invocation_id)
        .expect("cancel attached resume admission");
    release_attachment.notify_one();
    await_session_change(
        &fixture.engine,
        parent.session_id,
        "attached resume terminalization",
        || {
            fixture
                .engine
                .inner
                .delegation_events
                .get(invocation_id)
                .is_some_and(|entry| {
                    entry.run_attached
                        && entry.child_run_id.is_some()
                        && entry.terminal_status == Some(SessionStatus::Cancelled)
                })
                .then_some(())
        },
    )
    .await;
    let child = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("attachment cancellation child projection");
    assert!(!child.log.events().iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input } if input == "resume active prompt"
    )));
    let (registry_invocation, terminal_status, _) = fixture
        .engine
        .delegation_registry_snapshot(child_session_id)
        .expect("attachment cancellation registry owner");
    assert_eq!(registry_invocation, old_invocation_id);
    assert_eq!(terminal_status, None);
    server.abort();
    fixture.engine.shutdown().await;
}

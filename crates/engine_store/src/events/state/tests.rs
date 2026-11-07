//! The folds' rules on logs that reach them, each also compared with its scan
//! at every event.

use std::sync::Arc;

use cookie_agent_protocol::{
    AcpConfigOption, AcpDetachReason, AcpDetached, AcpSessionRecord, AcpSessionStart,
    AcpSessionUpdate, AdaptorId, AgentId, EventOrigin, EventPayload, ModelFinishReason,
    ModelSelection, PersistedModelTurn, ProducerDeliveryMode, ProducerIdempotencyKey,
    ProducerMessageId, ProducerOwner, ResolvedModelRef, RunId, SafeDisplayText, SessionId,
    SessionTitle, SessionTitleChange, Sha256Digest, StoredEvent,
};
use uuid::Uuid;

use super::{DelegatedTitle, LogState, Recorded, TakenInput, reference};
use crate::test_support::{agent_snapshot, run_selection};

fn agent(name: &str) -> AgentId {
    AgentId::new(name).unwrap()
}

/// `payloads` as a log, with its sequence numbers; each with its run, if any.
fn log(payloads: Vec<(Option<RunId>, EventPayload)>) -> Vec<Arc<StoredEvent>> {
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, (run_id, payload))| {
            Arc::new(StoredEvent {
                engine_version: None,
                origin: Some(EventOrigin::new("engine:test").unwrap()),
                session_id: SessionId(Uuid::from_u128(7)),
                run_id,
                seq: index as u64 + 1,
                timestamp: jiff::Timestamp::new(index as i64 + 1, 0).unwrap(),
                payload,
            })
        })
        .collect()
}

fn fold(events: &[Arc<StoredEvent>]) -> LogState {
    reference::assert_every_event(events);
    let mut state = LogState::default();
    for event in events {
        state.observe(event);
    }
    state
}

fn started(name: &str, declaration: &str, session: &str) -> (Option<RunId>, EventPayload) {
    (
        None,
        EventPayload::AcpSessionStarted {
            record: AcpSessionRecord {
                agent: agent(name),
                runtime_id: "local".parse().unwrap(),
                runtime_agent: agent(declaration),
                acp_session_id: session.to_owned(),
                cwd: "/work".to_owned(),
                protocol_version: 2,
                agent_info: serde_json::Value::Null,
                how: AcpSessionStart::New,
                steering: true,
                forked_from: None,
                fork: Some(true),
                fork_point: Some(true),
            },
        },
    )
}

/// [`started`], the way `how` starts a session.
fn started_as(
    how: AcpSessionStart,
    name: &str,
    declaration: &str,
    session: &str,
) -> (Option<RunId>, EventPayload) {
    let (run, mut payload) = started(name, declaration, session);
    if let EventPayload::AcpSessionStarted { record } = &mut payload {
        record.how = how;
    }
    (run, payload)
}

fn updated(update: AcpSessionUpdate) -> (Option<RunId>, EventPayload) {
    (None, EventPayload::AcpSessionUpdated { update })
}

fn options(id: &str, value: &str) -> (Option<RunId>, EventPayload) {
    updated(AcpSessionUpdate {
        options: Some(vec![AcpConfigOption {
            kind: Default::default(),
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            category: None,
            current_value: value.to_owned(),
            values: Vec::new(),
        }]),
        ..AcpSessionUpdate::default()
    })
}

fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(id, value)| ((*id).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn recorded_options_are_the_latest_before_the_session_that_just_started() {
    let state = fold(&log(vec![started("a", "x", "one")]));
    assert_eq!(state.recorded_option_values(&agent("a"), &agent("x")), &[]);

    // What came before the latest session counts; what came after it does
    // not yet.
    let events = log(vec![
        options("before", "0"),
        started("a", "x", "one"),
        options("mode", "1"),
        options("mode", "2"),
        started("a", "x", "two"),
        options("mode", "3"),
    ]);
    let state = fold(&events);
    assert_eq!(
        state.recorded_option_values(&agent("a"), &agent("x")),
        pairs(&[("mode", "2")])
    );
    assert_eq!(state.recorded_option_values(&agent("b"), &agent("x")), &[]);
    assert_eq!(state.recorded_option_values(&agent("a"), &agent("y")), &[]);

    // A session with nothing recorded since passes on the one before it.
    let state = fold(&log(vec![
        started("a", "x", "one"),
        options("mode", "1"),
        started("a", "x", "two"),
        started("a", "x", "three"),
    ]));
    assert_eq!(
        state.recorded_option_values(&agent("a"), &agent("x")),
        pairs(&[("mode", "1")])
    );
}

#[test]
fn recorded_options_stop_at_an_earlier_session_of_another_agent_or_declaration() {
    for (name, declaration) in [("b", "x"), ("a", "y")] {
        let state = fold(&log(vec![
            started("a", "x", "one"),
            options("mode", "1"),
            started(name, declaration, "two"),
            started("a", "x", "three"),
        ]));
        assert_eq!(state.recorded_option_values(&agent("a"), &agent("x")), &[]);
    }
    // Though what the other agent's session recorded, right before this one
    // started, is what the scan meets first.
    let state = fold(&log(vec![
        started("b", "x", "one"),
        options("mode", "1"),
        started("a", "x", "two"),
    ]));
    assert_eq!(
        state.recorded_option_values(&agent("a"), &agent("x")),
        pairs(&[("mode", "1")])
    );
}

fn detached(session: &str, reason: AcpDetachReason) -> (Option<RunId>, EventPayload) {
    updated(AcpSessionUpdate {
        detached: Some(AcpDetached {
            acp_session_id: session.to_owned(),
            reason,
            message_id: Some("message".to_owned()),
        }),
        ..AcpSessionUpdate::default()
    })
}

#[test]
fn each_agent_keeps_its_latest_session_and_what_left_only_that_one_behind() {
    let state = fold(&log(vec![
        started("a", "x", "a-one"),
        started("b", "x", "b-one"),
        detached("a-one", AcpDetachReason::Forked),
        detached("b-one", AcpDetachReason::Reverted),
        detached("nobody", AcpDetachReason::Reverted),
    ]));
    let of = |name: &str| state.acp_agent(&agent(name)).unwrap();
    assert_eq!(of("a").detached.len(), 1);
    assert_eq!(of("a").detached[0].reason, AcpDetachReason::Forked);
    assert_eq!(of("b").detached[0].reason, AcpDetachReason::Reverted);
    assert!(state.acp_agent(&agent("c")).is_none());

    // A new session of the agent leaves nothing behind.
    let state = fold(&log(vec![
        started("a", "x", "a-one"),
        detached("a-one", AcpDetachReason::Forked),
        started("a", "x", "a-two"),
    ]));
    let known = state.acp_agent(&agent("a")).unwrap();
    assert_eq!(known.record.acp_session_id, "a-two");
    assert!(known.detached.is_empty());
}

#[test]
fn prompt_context_is_kept_per_agent() {
    let context = |text: &str| {
        updated(AcpSessionUpdate {
            prompt_context: Some(vec![text.to_owned()]),
            ..AcpSessionUpdate::default()
        })
    };
    let state = fold(&log(vec![
        started("a", "x", "a-one"),
        context("of a"),
        started("b", "x", "b-one"),
        context("of b"),
        // Back to `a`, which composes nothing as it resumes.
        started_as(AcpSessionStart::Resumed, "a", "x", "a-one"),
    ]));
    let of = |name: &str| {
        state
            .acp_agent(&agent(name))
            .unwrap()
            .prompt_context
            .as_deref()
            .cloned()
    };
    assert_eq!(of("a"), Some(vec!["of a".to_owned()]));
    assert_eq!(of("b"), Some(vec!["of b".to_owned()]));
    assert_eq!(state.acp_prompt_context(&agent("a")), ["of a".to_owned()]);
    assert!(state.acp_prompt_context(&agent("stranger")).is_empty());
}

#[test]
fn a_session_that_starts_afresh_has_been_given_no_context() {
    let context = updated(AcpSessionUpdate {
        prompt_context: Some(vec!["of a-one".to_owned()]),
        ..AcpSessionUpdate::default()
    });
    let given = |how| {
        fold(&log(vec![
            started("a", "x", "a-one"),
            context.clone(),
            started_as(how, "a", "x", "a-two"),
        ]))
        .acp_prompt_context(&agent("a"))
        .to_vec()
    };
    assert!(given(AcpSessionStart::New).is_empty());
    assert!(given(AcpSessionStart::Subagent).is_empty());
    for how in [
        AcpSessionStart::Resumed,
        AcpSessionStart::Loaded,
        AcpSessionStart::Forked,
    ] {
        assert_eq!(given(how), ["of a-one".to_owned()], "{how:?}");
    }
}

fn committed(run: RunId, input_through_seq: u64) -> (Option<RunId>, EventPayload) {
    let selection = ModelSelection {
        model: "test/model".parse().unwrap(),
        variant: None,
    };
    (
        Some(run),
        EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: 1,
            input_through_seq,
            resolved_model: ResolvedModelRef {
                model_id: selection.model.model_id().clone(),
                selection,
                provider_id: "test".parse().unwrap(),
                adapter_id: AdaptorId::OPENAI_COMPATIBLE,
                selection_fingerprint: Sha256Digest::of_bytes(b"fixture"),
            },
            turn: PersistedModelTurn {
                content: vec![],
                provider_options: Default::default(),
                finish_reason: ModelFinishReason::Stop,
                usage: Default::default(),
                response_metadata: Default::default(),
                provider_metadata: Default::default(),
                native_replay: None,
            },
            warnings: vec![],
            prefix_fingerprint: None,
        },
    )
}

fn accepted(id: ProducerMessageId, owner: ProducerOwner, key: &str) -> EventPayload {
    EventPayload::ProducerMessageAccepted {
        message_id: id,
        producer_owner: owner,
        mode: ProducerDeliveryMode::Steer,
        idempotency_key: ProducerIdempotencyKey::new(key).unwrap(),
        description: SafeDisplayText::new("message").unwrap(),
        body: key.into(),
        reminder: None,
        agent_hop: None,
        retained_output: None,
    }
}

#[test]
fn input_since_a_boundary_is_what_was_applied_and_admitted_after_it() {
    let run = RunId::new_v7();
    let agent_owner = ProducerOwner::AcpAgent { agent: agent("a") };
    let other_owner = ProducerOwner::AcpAgent { agent: agent("b") };
    let (woken, gone) = (ProducerMessageId::new_v7(), ProducerMessageId::new_v7());
    let events = log(vec![
        (
            Some(run),
            EventPayload::UserInputSubmitted {
                content: "first".into(),
            },
        ),
        (
            Some(run),
            EventPayload::UserInputApplied { user_input_seq: 1 },
        ),
        committed(run, 2),
        (
            Some(run),
            EventPayload::UserInputSubmitted {
                content: "second".into(),
            },
        ),
        (Some(run), accepted(woken, agent_owner, "wake")),
        (Some(run), accepted(gone, other_owner, "gone")),
        (
            Some(run),
            EventPayload::UserInputApplied { user_input_seq: 4 },
        ),
        (
            Some(run),
            EventPayload::ProducerMessageAdmitted { message_id: woken },
        ),
        (
            Some(run),
            EventPayload::ProducerMessageAdmitted { message_id: gone },
        ),
    ]);
    let state = fold(&events);
    assert_eq!(state.input_boundary(), 2);
    let taken = |boundary| {
        state
            .inputs_since(boundary)
            .into_iter()
            .map(|input| match input {
                TakenInput::User { user_input_seq, .. } => user_input_seq,
                TakenInput::Producer { message_id, .. } if message_id == woken => 100,
                TakenInput::Producer { .. } => 200,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(taken(2), [4, 100, 200]);
    assert_eq!(taken(7), [100, 200]);
    assert_eq!(taken(8), [200]);
    assert!(taken(9).is_empty());
    assert!(state.woke_since(2));
    assert!(state.woke_since(7));
    assert!(state.woke_since(8), "`b`'s wake too");
    assert!(!state.woke_since(9));
}

#[test]
fn a_message_counts_as_unadmitted_until_a_run_admits_or_discards_it() {
    let run = RunId::new_v7();
    let message = ProducerMessageId::new_v7();
    let owner = ProducerOwner::AcpAgent { agent: agent("a") };
    let mut payloads = vec![(None, accepted(message, owner.clone(), "wake"))];
    assert!(fold(&log(payloads.clone())).producer_message_unadmitted());
    payloads.push((
        None,
        EventPayload::ProducerMessageDiscarded {
            message_id: message,
            reminder: None,
            producer_owner: owner.clone(),
        },
    ));
    assert!(!fold(&log(payloads.clone())).producer_message_unadmitted());

    let other = ProducerMessageId::new_v7();
    let admitted = vec![
        (None, accepted(other, owner, "other")),
        (
            Some(run),
            EventPayload::ProducerMessageAdmitted { message_id: other },
        ),
    ];
    assert!(!fold(&log(admitted)).producer_message_unadmitted());
}

#[test]
fn once_facts_are_found_by_their_keys() {
    let invocation = cookie_agent_protocol::InvocationId::new_v7();
    let other = cookie_agent_protocol::InvocationId::new_v7();
    let turns = |text: &str| {
        vec![cookie_agent_protocol::DelegatedContextTurn {
            role: cookie_agent_protocol::DelegatedContextRole::User,
            text: text.to_owned(),
        }]
    };
    let title = |text: &str| SessionTitle::new(text).unwrap();
    let delegated = |invocation_id, text: &str| {
        (
            None,
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::DelegatedSet {
                    title: title(text),
                    invocation_id,
                },
                input_through_seq: 0,
            },
        )
    };
    let state = fold(&log(vec![
        (
            None,
            EventPayload::DelegatedContextSeeded {
                invocation_id: invocation,
                turns: turns("seed"),
            },
        ),
        (
            None,
            EventPayload::SkillLoaded {
                name: "deploy".to_owned(),
                rendered_body: "Do it.".to_owned(),
                source_path: "/skills/deploy".to_owned(),
                args: String::new(),
                base_dir: "/skills".to_owned(),
                supporting_files: Vec::new(),
            },
        ),
        delegated(invocation, "Child"),
    ]));
    assert_eq!(state.seed(invocation, &turns("seed")), Recorded::Same);
    assert_eq!(
        state.seed(invocation, &turns("else")),
        Recorded::Conflicting
    );
    assert_eq!(state.seed(other, &turns("seed")), Recorded::Absent);
    assert!(state.skill_loaded("deploy", b"Do it."));
    assert!(!state.skill_loaded("deploy", b"Do it again."));
    assert!(!state.skill_loaded("other", b"Do it."));
    assert_eq!(
        state.delegated_title(invocation, &title("Child")),
        DelegatedTitle::Same
    );
    assert_eq!(
        state.delegated_title(invocation, &title("Else")),
        DelegatedTitle::OtherOrigin
    );
    assert_eq!(
        state.delegated_title(other, &title("Child")),
        DelegatedTitle::OtherTitle
    );

    // A title anything but the delegation or the user set comes first.
    let state = fold(&log(vec![
        (
            None,
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::FallbackSet {
                    title: title("Generated"),
                },
                input_through_seq: 0,
            },
        ),
        delegated(invocation, "Child"),
    ]));
    assert_eq!(
        state.delegated_title(invocation, &title("Child")),
        DelegatedTitle::OtherTitle
    );
    assert_eq!(
        fold(&log(vec![started("a", "x", "one")])).delegated_title(invocation, &title("Child")),
        DelegatedTitle::Absent
    );
}

/// A revert folds what stays again, so what the cut removed is forgotten and
/// what it kept, however far back, is still there.
#[test]
fn a_revert_folds_what_stays_again() {
    let directory = tempfile::tempdir().unwrap();
    let origin = || EventOrigin::new("engine:test").unwrap();
    let session = SessionId(Uuid::from_u128(7));
    let agent_record = agent_snapshot("test", cookie_agent_protocol::AgentMode::Primary);
    let log = crate::events::EventLog::create(
        directory.path().join("events.jsonl"),
        session,
        origin(),
        EventPayload::SessionCreated {
            cron: None,
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            short_id: "test_00000000".into(),
            origin: cookie_agent_protocol::SessionOrigin::Root,
            cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
            creation_selection: run_selection("test"),
            revisions: cookie_agent_protocol::RuntimeRevisions {
                runtime_revision: format!("sha256:{}", "1".repeat(64)).parse().unwrap(),
                catalog_revision: format!("sha256:{}", "2".repeat(64)).parse().unwrap(),
                provider_state_revision: format!("sha256:{}", "3".repeat(64)).parse().unwrap(),
                model_revision: format!("sha256:{}", "4".repeat(64)).parse().unwrap(),
                agent_revision: format!("sha256:{}", "5".repeat(64)).parse().unwrap(),
                recipe_registry_revision: format!("sha256:{}", "6".repeat(64)).parse().unwrap(),
                manifest_revision: crate::test_support::manifest_of(
                    &agent_record.fallback_chain[0],
                ),
            },
            creation_agent: Box::new(agent_record),
        },
    )
    .unwrap();
    let append = |payload: (Option<RunId>, EventPayload)| {
        log.append(payload.0, origin(), payload.1).unwrap();
    };
    append(started("a", "x", "one"));
    append(options("mode", "kept"));
    let cut = log.physical_tip_seq();
    append(started("a", "x", "two"));
    append(options("mode", "removed"));
    append(detached("two", AcpDetachReason::Reverted));
    assert_eq!(
        log.state()
            .acp_agent(&agent("a"))
            .unwrap()
            .record
            .acp_session_id,
        "two"
    );

    let capability = log.write_capability.clone().unwrap();
    log.truncate_after(&capability, cut).unwrap();
    let state = log.state();
    let known = state.acp_agent(&agent("a")).unwrap();
    assert_eq!(known.record.acp_session_id, "one");
    assert!(known.detached.is_empty());
    reference::assert_every_event(&log.event_snapshot());

    // And it goes on from there.
    append(started("a", "x", "three"));
    assert_eq!(
        log.state().recorded_option_values(&agent("a"), &agent("x")),
        pairs(&[("mode", "kept")])
    );
    reference::assert_every_event(&log.event_snapshot());
}

use super::*;

#[test]
fn tool_failures_with_output_stay_inline_on_replay() {
    let session = SessionId::new_v7();
    // No output, partial result, streamed output, display-only, and a normal bash exit 1.
    for case in 0..5 {
        let call = ToolCallId::new_v7();
        let owner = AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new("call").unwrap(),
            provider_item_id: None,
        };
        let start = stored_event(
            session,
            None,
            1,
            EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    tool_call_id: call,
                    owner: owner.clone(),
                    output: Default::default(),
                    presentation: cookie_agent_protocol::ToolCallPresentation {
                        title: cookie_agent_protocol::SafeDisplayText::new(if case == 4 {
                            "bash"
                        } else {
                            "read"
                        })
                        .unwrap(),
                        primary_argument: None,
                    },
                    operation_fingerprint: serde_json::from_value(
                        serde_json::json!({"digest": "1".repeat(64)}),
                    )
                    .unwrap(),
                },
            },
        );
        let event = stored_event(
            session,
            None,
            3,
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id: call,
                    owner,
                    outcome: if case == 4 {
                        ToolTerminationOutcome::Completed
                    } else {
                        ToolTerminationOutcome::Failed
                    },
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        title: cookie_agent_protocol::SafeDisplayText::new("Read").unwrap(),
                        output: if case == 1 {
                            "File missing".into()
                        } else {
                            String::new()
                        },
                        display: match case {
                            1 => Some("Finished".into()),
                            3 => Some("File missing".into()),
                            4 => Some("stdout\nstderr\nExit status: 1".into()),
                            _ => None,
                        },
                        metadata: serde_json::json!({"status": 1, "success": false}),
                        retained_output: None,
                        truncation: None,
                        attachments: vec![],
                        additional_messages: vec![],
                    }),
                    error: (case != 4).then(|| cookie_agent_protocol::SafeToolError {
                        code: cookie_agent_protocol::SafeCode::new("not_found").unwrap(),
                        message: cookie_agent_protocol::SafeErrorMessage::new(
                            "Required file /work/Report.md missing",
                        )
                        .unwrap(),
                    }),
                },
            },
        );
        let mut events = vec![start];
        if case == 2 {
            events.push(stored_event(
                session,
                None,
                2,
                EventPayload::ToolCallProgress {
                    tool_call_id: call,
                    message: cookie_agent_protocol::SafeDisplayText::new("output").unwrap(),
                    display: Some("File missing".into()),
                },
            ));
        }
        events.push(event);
        let state = reduce_session_events(session, 0, &events);
        // Failures render inline on the tool item, like a bash non-zero
        // exit, never as a transcript event row.
        let has_error_event = state.transcript.iter().any(|item| {
            matches!(
                item,
                TranscriptItem::Event {
                    level: EventLevel::Error,
                    ..
                }
            )
        });
        assert!(!has_error_event, "case {case}");
        let tool = &state.tools[&call];
        assert_eq!(
            tool.status,
            if case == 4 {
                ToolStatus::Completed
            } else {
                ToolStatus::Failed
            }
        );
        if case == 4 {
            assert_eq!(tool.detail, "stdout\nstderr\nExit status: 1");
        } else {
            assert!(
                tool.detail
                    .contains("Required file /work/Report.md missing")
            );
            if case != 0 {
                assert!(tool.detail.contains("File missing"));
            }
        }
    }
}

#[test]
fn tool_failure_without_tool_item_surfaces_an_event_row() {
    let session = SessionId::new_v7();
    // A terminated call whose start never reached the transcript has no
    // tool item to carry the failure; keep the event row as a fallback.
    let termination = stored_event(
        session,
        None,
        1,
        EventPayload::ToolCallTerminated {
            termination: cookie_agent_protocol::ToolCallTermination {
                tool_call_id: ToolCallId::new_v7(),
                owner: AssistantToolCallRef {
                    model_turn_seq: 1,
                    content_index: 0,
                    model_call_id: cookie_agent_protocol::ModelCallId::new("call").unwrap(),
                    provider_item_id: None,
                },
                outcome: ToolTerminationOutcome::Failed,
                result: None,
                error: Some(cookie_agent_protocol::SafeToolError {
                    code: cookie_agent_protocol::SafeCode::new("execution_failed").unwrap(),
                    message: cookie_agent_protocol::SafeErrorMessage::new(
                        "oldString was not found",
                    )
                    .unwrap(),
                }),
            },
        },
    );
    let state = reduce_session_events(session, 0, &[termination]);
    let error_rows = state
        .transcript
        .iter()
        .filter(|item| {
            matches!(
                item,
                TranscriptItem::Event {
                    level: EventLevel::Error,
                    ..
                }
            )
        })
        .count();
    assert_eq!(error_rows, 1);
}

#[test]
fn invalid_payload_replay_warnings_dedupe_per_run_and_reason() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let decision = |history_index: u64, reason: &str| cookie_agent_protocol::ReplayDecision {
        history_index,
        disposition: cookie_agent_protocol::ReplayDisposition::DiscardedInvalidPayload {
            reason: cookie_agent_protocol::SafeErrorMessage::new(reason).unwrap(),
        },
    };
    let evaluated = |seq: u64, ordered_decisions: Vec<cookie_agent_protocol::ReplayDecision>| {
        stored_event(
            session,
            Some(run),
            seq,
            EventPayload::ModelReplayEvaluated {
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved_model(),
                ordered_decisions,
            },
        )
    };
    // Every attempt re-evaluates the whole history after a fallback: the
    // same incompatible entries discard again and again. One logical
    // reason warns once per run; a genuinely different reason warns anew.
    let state = reduce_session_events(
        session,
        0,
        &[
            evaluated(
                1,
                vec![
                    decision(2, "unsupported replay format"),
                    decision(4, "unsupported replay format"),
                ],
            ),
            evaluated(
                2,
                vec![
                    decision(2, "unsupported replay format"),
                    decision(4, "unsupported replay format"),
                ],
            ),
            evaluated(
                3,
                vec![
                    decision(2, "unsupported replay format"),
                    decision(6, "payload digest mismatch"),
                ],
            ),
        ],
    );
    let warnings: Vec<&str> = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Event {
                level: EventLevel::Warning,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        warnings
            .iter()
            .filter(|text| text.contains("unsupported replay format"))
            .count(),
        1
    );
    assert_eq!(
        warnings
            .iter()
            .filter(|text| text.contains("payload digest mismatch"))
            .count(),
        1
    );
    assert_eq!(warnings.len(), 2);
}

#[test]
fn repeated_adapter_turn_warnings_dedupe_per_run() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let committed = |seq: u64, model_turn_seq: u64, warning: &str| {
        stored_event(
            session,
            Some(run),
            seq,
            EventPayload::ModelTurnCommitted {
                attempt_id: AttemptId::new_v7(),
                model_turn_seq,
                resolved_model: resolved_model(),
                input_through_seq: seq,
                turn: PersistedModelTurn {
                    content: Vec::new(),
                    provider_options: BTreeMap::new(),
                    finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: vec![cookie_agent_protocol::SafeErrorMessage::new(warning).unwrap()],
            },
        )
    };
    let state = reduce_session_events(
        session,
        0,
        &[
            committed(1, 1, "Responses normalized fallback omitted reasoning"),
            committed(2, 2, "Responses normalized fallback omitted reasoning"),
            committed(3, 3, "Responses normalized fallback omitted reasoning"),
            committed(4, 4, "a different adapter warning"),
        ],
    );
    let warnings: Vec<&str> = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Event {
                level: EventLevel::Warning,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        warnings
            .iter()
            .filter(|text| text.contains("omitted reasoning"))
            .count(),
        1
    );
    assert_eq!(
        warnings
            .iter()
            .filter(|text| text.contains("a different adapter warning"))
            .count(),
        1
    );
    assert_eq!(warnings.len(), 2);
}

#[test]
fn abandoned_attempt_renders_its_model_error() {
    let session = SessionId::new_v7();
    let model_error: ModelErrorSummary = serde_json::from_value(serde_json::json!({"kind":"timeout","message":"model request timed out","retryable":true,"stage":"stream_read","http_status":null,"bytes_received":0,"vendor_code":null,"request_id":null,"retry_after_ms":null})).unwrap();
    let events = vec![stored_event(
        session,
        None,
        1,
        EventPayload::AttemptAbandoned {
            attempt_id: AttemptId::new_v7(),
            model_error: Some(model_error),
        },
    )];
    let state = reduce_session_events(session, 0, &events);
    let row = state
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Event {
                level: EventLevel::Warning,
                text,
                ..
            } => Some(text),
            _ => None,
        })
        .expect("abandon warning row");
    assert!(row.contains("model attempt abandoned"));
    assert!(row.contains("model request timed out"));
    assert!(row.contains("retryable true"));
}

#[test]
fn aborted_attempt_notes_an_interruption_inside_the_assistant_block() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let attempt_id = AttemptId::new_v7();
    let model_error: ModelErrorSummary = serde_json::from_value(serde_json::json!({"kind":"abort","message":"model request aborted","retryable":false,"stage":"stream_read","http_status":null,"bytes_received":0,"vendor_code":null,"request_id":null,"retry_after_ms":null})).unwrap();
    let events = vec![
        stored_event(
            session,
            Some(run),
            1,
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model(),
                prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
            },
        ),
        stored_event(
            session,
            Some(run),
            2,
            EventPayload::TextDelta {
                attempt_id,
                text: "partial".into(),
            },
        ),
        stored_event(
            session,
            Some(run),
            3,
            EventPayload::AttemptAbandoned {
                attempt_id,
                model_error: Some(model_error),
            },
        ),
    ];
    let state = reduce_session_events(session, 0, &events);
    assert!(
        !state.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Event {
                level: EventLevel::Warning,
                ..
            }
        )),
        "an abort must not push an abandon warning row"
    );
    let notice = state
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Assistant { children, .. } => {
                children.iter().find_map(|child| match child {
                    AssistantChild::Notice { text } => Some(text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("assistant interruption notice");
    assert_eq!(notice, "model interrupted");
}

#[test]
fn committed_partial_turn_survives_the_abort_that_follows_it() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let attempt_id = AttemptId::new_v7();
    let model_error: ModelErrorSummary = serde_json::from_value(serde_json::json!({"kind":"abort","message":"model stream was cancelled","retryable":false,"stage":"stream_read","http_status":null,"bytes_received":0,"vendor_code":null,"request_id":null,"retry_after_ms":null})).unwrap();
    let events = vec![
        stored_event(
            session,
            Some(run),
            1,
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model(),
                prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
            },
        ),
        stored_event(
            session,
            Some(run),
            2,
            EventPayload::TextDelta {
                attempt_id,
                text: "partial an".into(),
            },
        ),
        stored_event(
            session,
            Some(run),
            3,
            EventPayload::ModelTurnCommitted {
                attempt_id,
                model_turn_seq: 1,
                resolved_model: resolved_model(),
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                        text: "partial an".into(),
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: cookie_agent_protocol::ModelFinishReason::Aborted,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
        stored_event(
            session,
            Some(run),
            4,
            EventPayload::AttemptAbandoned {
                attempt_id,
                model_error: Some(model_error),
            },
        ),
        stored_event(
            session,
            Some(run),
            5,
            EventPayload::RunCancelled { reason: None },
        ),
    ];
    let state = reduce_session_events(session, 0, &events);
    let TranscriptItem::Assistant { children, .. } = state
        .transcript
        .iter()
        .find(|item| matches!(item, TranscriptItem::Assistant { .. }))
        .expect("assistant item")
    else {
        unreachable!()
    };
    assert!(
        children.iter().any(|child| matches!(
            child,
            AssistantChild::Text { markdown, .. } if markdown.as_str() == "partial an"
        )),
        "the committed partial text survives the abort: {children:?}"
    );
    assert!(
        children.last().is_some_and(|child| matches!(
            child,
            AssistantChild::Notice { text } if text == "model interrupted"
        )),
        "the interruption notice follows the partial text: {children:?}"
    );
    assert!(
        !state.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Event {
                level: EventLevel::Warning,
                ..
            }
        )),
        "an abort must not also push an abandon warning row"
    );
}

#[test]
fn abort_of_an_untracked_attempt_still_shows_an_interruption_row() {
    // The attempt started before this replay window, so there is no block
    // to hold the notice.
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let model_error: ModelErrorSummary = serde_json::from_value(serde_json::json!({"kind":"abort","message":"model stream was cancelled","retryable":false,"stage":"stream_read","http_status":null,"bytes_received":0,"vendor_code":null,"request_id":null,"retry_after_ms":null})).unwrap();
    let events = vec![stored_event(
        session,
        Some(run),
        7,
        EventPayload::AttemptAbandoned {
            attempt_id: AttemptId::new_v7(),
            model_error: Some(model_error),
        },
    )];
    let state = reduce_session_events(session, 6, &events);
    assert!(
        state.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Event {
                level: EventLevel::Warning,
                text,
                ..
            } if text == "model interrupted"
        )),
        "{:?}",
        state.transcript
    );
}

#[test]
fn interrupted_commit_keeps_the_previous_context_estimate() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let (first, second) = (AttemptId::new_v7(), AttemptId::new_v7());
    let started = |seq, attempt_id, ordinal| {
        stored_event(
            session,
            Some(run),
            seq,
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: ordinal,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model(),
                prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
            },
        )
    };
    let committed = |seq, attempt_id, model_turn_seq, finish_reason, usage| {
        stored_event(
            session,
            Some(run),
            seq,
            EventPayload::ModelTurnCommitted {
                attempt_id,
                model_turn_seq,
                resolved_model: resolved_model(),
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                        text: "answer".into(),
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason,
                    usage,
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        )
    };
    let events = vec![
        started(1, first, 1),
        committed(
            2,
            first,
            1,
            cookie_agent_protocol::ModelFinishReason::Stop,
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                ..Usage::default()
            },
        ),
        started(3, second, 2),
        committed(
            4,
            second,
            2,
            cookie_agent_protocol::ModelFinishReason::Aborted,
            Usage::default(),
        ),
    ];
    let state = reduce_session_events(session, 0, &events);
    assert_eq!(state.context_tokens, Some(120));
}

#[test]
fn terminal_and_internal_diagnostics_survive_replay_without_lowercasing() {
    let session = SessionId::new_v7();
    let model_error: ModelErrorSummary = serde_json::from_value(serde_json::json!({"kind":"invalid_request","message":"Invalid request","retryable":false,"stage":"response_body","http_status":400,"bytes_received":12,"vendor_code":"bad_parameter","request_id":"Request-ID","retry_after_ms":null,"response_body":r#"Couldn't decode upstream response: {"message":"Temperature unsupported","password":"review\"secret-tail","items":[{"name":"X-Api-Key","value":"opaque-review-credential"}]}"#})).unwrap();
    let events = vec![
        stored_event(
            session,
            Some(RunId::new_v7()),
            1,
            EventPayload::RunFailed {
                error: cookie_agent_protocol::SafeErrorMessage::new("model failed").unwrap(),
                model_error: Some(model_error.clone()),
                resolved_model: Some(resolved_model()),
            },
        ),
        stored_event(
            session,
            None,
            2,
            EventPayload::InternalAgentFailed {
                invocation_id: cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
                internal_run_id: cookie_agent_protocol::InternalAgentRunId::new_v7(),
                kind: cookie_agent_protocol::InternalAgentKind::ContextCompaction,
                failure: cookie_agent_protocol::InternalAgentFailure {
                    code: cookie_agent_protocol::SafeCode::new("model_failure").unwrap(),
                    message: cookie_agent_protocol::SafeErrorMessage::new("Compaction failed")
                        .unwrap(),
                    retryable: false,
                    model_error: Some(model_error),
                },
            },
        ),
    ];
    let replayed = reduce_session_events(session, 0, &events);
    let errors = replayed
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Event {
                level: EventLevel::Error,
                text,
                ..
            } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 2);
    for text in errors {
        assert!(text.contains("Temperature unsupported"));
        assert!(text.contains("secret-tail"));
        assert!(text.contains("opaque-review-credential"));
        assert!(text.contains("HTTP 400"));
        assert!(text.contains("Request-ID"));
        assert!(text.contains('\n'));
    }
}

fn goal_item(description: &str, finished: bool) -> cookie_agent_protocol::GoalItem {
    cookie_agent_protocol::GoalItem {
        description: description.into(),
        finished,
    }
}

fn resolved_model() -> ResolvedModelRef {
    serde_json::from_value(serde_json::json!({
        "provider_id": "test",
        "model_id": "test",
        "adapter_id": "openai-compatible",
        "selection": {"model": "test/test", "variant": null},
        "selection_fingerprint": "a".repeat(64)
    }))
    .expect("resolved model")
}

fn committed_turn(input_through_seq: u64) -> EventPayload {
    EventPayload::ModelTurnCommitted {
        attempt_id: AttemptId::new_v7(),
        model_turn_seq: 1,
        resolved_model: resolved_model(),
        input_through_seq,
        turn: PersistedModelTurn {
            content: Vec::new(),
            provider_options: BTreeMap::new(),
            finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
            usage: Usage::default(),
            response_metadata: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            native_replay: None,
        },
        warnings: Vec::new(),
    }
}

fn stored_event(
    session_id: SessionId,
    run_id: Option<RunId>,
    seq: u64,
    payload: EventPayload,
) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id,
        seq,
        timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
        payload,
    }
}

fn producer_rows(state: &SessionState) -> Vec<&TranscriptItem> {
    state
        .transcript
        .iter()
        .filter(|item| matches!(item, TranscriptItem::ProducerMessage { .. }))
        .collect()
}

fn accepted_message(
    message_id: ProducerMessageId,
    producer_owner: ProducerOwner,
    key: &str,
    reminder: Option<GoalReminderIdentity>,
) -> EventPayload {
    EventPayload::ProducerMessageAccepted {
        description: Default::default(),
        message_id,
        producer_owner,
        mode: ProducerDeliveryMode::Queue,
        idempotency_key: ProducerIdempotencyKey::new(key).expect("key"),
        body: key.into(),
        reminder,
        agent_hop: None,
    }
}

fn discarded_message(
    message_id: ProducerMessageId,
    producer_owner: Option<ProducerOwner>,
    reminder: Option<GoalReminderIdentity>,
) -> EventPayload {
    EventPayload::ProducerMessageDiscarded {
        message_id,
        reminder,
        producer_owner,
    }
}

#[test]
fn goal_projection_requires_valid_identity_revision_and_lifecycle() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let replacement_id = GoalId::new_v7();
    let mut state = SessionState::default();
    let apply = |state: &mut SessionState, seq, payload| {
        reduce_event(
            state,
            session_id,
            None,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    };

    apply(
        &mut state,
        1,
        EventPayload::GoalActivated {
            goal_id,
            objective: "Ship the reducer".into(),
            revision: 0,
            selection: None,
        },
    );
    apply(
        &mut state,
        2,
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: Vec::new(),
            revision: 1,
        },
    );
    apply(
        &mut state,
        3,
        EventPayload::GoalLifecycleChanged {
            goal_id,
            status: GoalStatus::Completed,
            revision: 2,
            selection: None,
        },
    );
    assert_eq!(
        state.goal.as_ref().expect("goal").status,
        GoalStatus::Active
    );

    apply(
        &mut state,
        4,
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: vec![goal_item("Verify replay", true)],
            revision: 2,
        },
    );
    apply(
        &mut state,
        5,
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: vec![goal_item("stale", false)],
            revision: 2,
        },
    );
    apply(
        &mut state,
        6,
        EventPayload::GoalLifecycleChanged {
            goal_id,
            status: GoalStatus::Completed,
            revision: 3,
            selection: None,
        },
    );
    apply(
        &mut state,
        7,
        EventPayload::GoalActivated {
            goal_id: replacement_id,
            objective: "Replacement".into(),
            revision: 0,
            selection: None,
        },
    );
    apply(
        &mut state,
        8,
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: Vec::new(),
            revision: 4,
        },
    );
    apply(
        &mut state,
        9,
        EventPayload::GoalLifecycleChanged {
            goal_id: replacement_id,
            status: GoalStatus::Cancelled,
            revision: 1,
            selection: None,
        },
    );
    apply(
        &mut state,
        10,
        EventPayload::GoalActivated {
            goal_id,
            objective: "Stale reactivation".into(),
            revision: 100,
            selection: None,
        },
    );

    let goal = state.goal.as_ref().expect("replacement goal");
    assert_eq!(goal.goal_id, replacement_id);
    assert_eq!(goal.status, GoalStatus::Cancelled);
    assert!(goal.items.is_empty());
    let snapshots = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Goal { goal, .. } => Some(goal),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(snapshots.len(), 6);
    assert_eq!(snapshots[1].status, GoalStatus::Active);
    assert!(snapshots[1].items.is_empty());
    assert_eq!(snapshots[3].status, GoalStatus::Completed);
}

#[test]
fn goal_checklist_replacement_preserves_duplicates_and_order() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let replacement = vec![
        goal_item("Repeat verification", false),
        goal_item("Repeat verification", true),
        goal_item("Final review", false),
    ];
    let mut store = StateStore::default();
    for (index, payload) in [
        EventPayload::GoalActivated {
            goal_id,
            objective: "Verify the replacement checklist".into(),
            revision: 0,
            selection: None,
        },
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: vec![goal_item("Old checklist", false)],
            revision: 1,
        },
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: replacement.clone(),
            revision: 2,
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(store.apply_event(stored_event(session_id, None, index as u64 + 1, payload,)));
    }
    let state = &store.sessions[&session_id];
    let goal = state.goal.as_ref().expect("goal");
    assert_eq!(goal.items, replacement);
    assert_eq!(goal.revision, 2);
    assert_eq!(goal.status, GoalStatus::Active);
    assert!(matches!(
        state.transcript.last(),
        Some(TranscriptItem::Goal { goal, .. }) if goal.items == replacement
    ));
}

#[test]
fn goal_projection_rebuilds_for_replay_revert_and_fork_like_history() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let item = goal_item("Keep projection durable", false);
    let events = vec![
        stored_event(
            session_id,
            None,
            1,
            EventPayload::GoalActivated {
                goal_id,
                objective: "Durable goal".into(),
                revision: 0,
                selection: None,
            },
        ),
        stored_event(
            session_id,
            None,
            2,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: vec![item.clone()],
                revision: 1,
            },
        ),
        stored_event(
            session_id,
            None,
            3,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Paused,
                revision: 2,
                selection: None,
            },
        ),
    ];

    let replayed = reduce_session_events(session_id, 4, &events);
    assert_eq!(replayed.generation, 4);
    assert_eq!(
        replayed.goal.as_ref().expect("replayed goal").status,
        GoalStatus::Paused
    );
    assert_eq!(
        replayed.goal.as_ref().expect("replayed goal").items,
        vec![item]
    );

    let forked = reduce_session_events(SessionId::new_v7(), 0, &events);
    assert_eq!(forked.goal, replayed.goal);

    let mut reverted_events = events;
    reverted_events.push(stored_event(
        session_id,
        None,
        4,
        EventPayload::SessionReverted { through_seq: 1 },
    ));
    let reverted = reduce_session_events(session_id, 5, &reverted_events);
    let goal = reverted.goal.expect("reverted goal");
    assert_eq!(goal.status, GoalStatus::Active);
    assert!(goal.items.is_empty());
    assert_eq!(
        reverted
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Goal { .. }))
            .count(),
        1
    );
}

#[test]
fn producer_queue_survives_terminal_and_consumption_updates_one_non_user_row() {
    let session_id = SessionId::new_v7();
    let first_run = RunId::new_v7();
    let second_run = RunId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let owner = ProducerOwner::Plugin {
        plugin: "build-monitor".into(),
    };
    let mut state = SessionState::default();

    for event in [
        stored_event(
            session_id,
            None,
            1,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: owner.clone(),
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("build-1").expect("key"),
                body: "build finished".into(),
                reminder: None,
                agent_hop: None,
            },
        ),
        stored_event(
            session_id,
            None,
            2,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: owner.clone(),
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("build-1").expect("key"),
                body: "build finished".into(),
                reminder: None,
                agent_hop: None,
            },
        ),
        stored_event(
            session_id,
            Some(first_run),
            3,
            EventPayload::RunCompleted { final_text: None },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            event.run_id,
            event.seq,
            event.timestamp,
            event.payload,
        );
    }
    assert!(matches!(
        producer_rows(&state).as_slice(),
        [TranscriptItem::ProducerMessage {
            status: ProducerMessageStatus::Pending,
            ..
        }]
    ));

    for event in [
        stored_event(
            session_id,
            Some(second_run),
            4,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        stored_event(session_id, Some(second_run), 5, committed_turn(4)),
        stored_event(
            session_id,
            Some(second_run),
            6,
            EventPayload::ProducerMessageConsumed {
                message_id,
                run_id: second_run,
            },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            event.run_id,
            event.seq,
            event.timestamp,
            event.payload,
        );
    }

    assert!(matches!(
        producer_rows(&state).as_slice(),
        [TranscriptItem::ProducerMessage {
            status: ProducerMessageStatus::Consumed,
            accepted_at,
            body,
            ..
        }] if body == "build finished"
            && *accepted_at == jiff::Timestamp::new(1, 0).expect("timestamp")
    ));
    assert!(state.pending_inputs.is_empty());
    assert!(state.voided_inputs.is_empty());
    assert!(state.producer_messages[&message_id].consumption_recorded);
    assert!(!state.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::User { .. } | TranscriptItem::Assistant { .. }
    )));
}

#[test]
fn input_rows_follow_application_order_without_changing_queue_identity() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let first = ProducerMessageId::new_v7();
    let second = ProducerMessageId::new_v7();
    let owner = ProducerOwner::Plugin {
        plugin: "test".into(),
    };
    let events = [
        accepted_message(first, owner.clone(), "first", None),
        accepted_message(second, owner, "second", None),
        EventPayload::UserInputSubmitted {
            input: "user input".into(),
        },
        EventPayload::ProducerMessageAdmitted { message_id: second },
        EventPayload::UserInputApplied { user_input_seq: 3 },
        EventPayload::ProducerMessageAdmitted { message_id: first },
        EventPayload::ProducerMessagesClaimed {
            message_ids: vec![second, first],
        },
        committed_turn(7),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, payload)| stored_event(session_id, Some(run_id), index as u64 + 1, payload))
    .collect::<Vec<_>>();
    let state = reduce_session_events(session_id, 0, &events);
    let inputs = state
        .transcript
        .iter()
        .filter(|item| {
            matches!(
                item,
                TranscriptItem::User { .. } | TranscriptItem::ProducerMessage { .. }
            )
        })
        .collect::<Vec<_>>();
    assert!(matches!(inputs.as_slice(), [
            TranscriptItem::ProducerMessage { message_id: a, seq: 2, status: ProducerMessageStatus::Consumed, .. },
            TranscriptItem::User { seq: 3, .. },
            TranscriptItem::ProducerMessage { message_id: b, seq: 1, status: ProducerMessageStatus::Consumed, .. },
        ] if *a == second && *b == first));
    // Rotation changes positions, not IDs, queue ages, or the producer index.
    for message_id in [first, second] {
        let message = &state.producer_messages[&message_id];
        let TranscriptItem::ProducerMessage {
            message_id: row_id,
            accepted_at,
            ..
        } = &state.transcript[message.transcript_index]
        else {
            panic!("producer row")
        };
        assert_eq!(*row_id, message_id);
        assert_eq!(
            *accepted_at,
            jiff::Timestamp::new(message.accepted_seq as i64, 0).unwrap()
        );
    }
    assert_eq!(
        inputs.iter().map(|item| item.id()).collect::<Vec<_>>(),
        [2, 3, 1]
    );
    assert!(state.pending_inputs.is_empty());
    assert!(state.voided_inputs.is_empty());
}

#[test]
fn valid_producer_admission_marks_initial_input_and_promotes_pending_user_input() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let mut state = SessionState::default();
    for (seq, event_run, payload) in [
        (
            1,
            Some(run_id),
            EventPayload::UserInputAdmitted {
                input: "steer after producer start".into(),
            },
        ),
        (
            2,
            None,
            accepted_message(
                message_id,
                ProducerOwner::Plugin {
                    plugin: "scheduler".into(),
                },
                "initial-producer-input",
                None,
            ),
        ),
        (
            3,
            Some(run_id),
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            event_run,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    }

    assert!(state.initial_input_submitted.contains(&run_id));
    assert_eq!(state.pending_inputs.len(), 1);
    reduce_event(
        &mut state,
        session_id,
        Some(run_id),
        4,
        jiff::Timestamp::new(4, 0).expect("timestamp"),
        EventPayload::UserInputSubmitted {
            input: "steer after producer start".into(),
        },
    );
    assert!(state.pending_inputs.is_empty());
}

#[test]
fn invalid_producer_admissions_do_not_mark_initial_input() {
    let session_id = SessionId::new_v7();
    let owner = ProducerOwner::Plugin {
        plugin: "scheduler".into(),
    };
    let mut state = SessionState::default();
    let mut seq = 0;
    let apply =
        |state: &mut SessionState, seq: &mut u64, run_id: Option<RunId>, payload: EventPayload| {
            *seq += 1;
            reduce_event(
                state,
                session_id,
                run_id,
                *seq,
                jiff::Timestamp::new(*seq as i64, 0).expect("timestamp"),
                payload,
            );
        };

    let runless = ProducerMessageId::new_v7();
    apply(
        &mut state,
        &mut seq,
        None,
        accepted_message(runless, owner.clone(), "runless", None),
    );
    apply(
        &mut state,
        &mut seq,
        None,
        EventPayload::ProducerMessageAdmitted {
            message_id: runless,
        },
    );
    assert!(state.initial_input_submitted.is_empty());

    let unknown_run = RunId::new_v7();
    apply(
        &mut state,
        &mut seq,
        Some(unknown_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: ProducerMessageId::new_v7(),
        },
    );
    assert!(!state.initial_input_submitted.contains(&unknown_run));

    let malformed = ProducerMessageId::new_v7();
    let malformed_run = RunId::new_v7();
    apply(
        &mut state,
        &mut seq,
        None,
        accepted_message(
            malformed,
            ProducerOwner::Goal {
                goal_id: GoalId::new_v7(),
            },
            "malformed-goal",
            None,
        ),
    );
    apply(
        &mut state,
        &mut seq,
        Some(malformed_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: malformed,
        },
    );
    assert!(!state.initial_input_submitted.contains(&malformed_run));

    let discarded = ProducerMessageId::new_v7();
    let discarded_run = RunId::new_v7();
    apply(
        &mut state,
        &mut seq,
        None,
        accepted_message(discarded, owner.clone(), "discarded", None),
    );
    apply(
        &mut state,
        &mut seq,
        None,
        discarded_message(discarded, Some(owner.clone()), None),
    );
    apply(
        &mut state,
        &mut seq,
        Some(discarded_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: discarded,
        },
    );
    assert!(!state.initial_input_submitted.contains(&discarded_run));

    let consumed = ProducerMessageId::new_v7();
    let consuming_run = RunId::new_v7();
    let consumed_retry_run = RunId::new_v7();
    apply(
        &mut state,
        &mut seq,
        None,
        accepted_message(consumed, owner.clone(), "consumed", None),
    );
    apply(
        &mut state,
        &mut seq,
        Some(consuming_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: consumed,
        },
    );
    let admission_seq = seq;
    apply(
        &mut state,
        &mut seq,
        Some(consuming_run),
        committed_turn(admission_seq),
    );
    apply(
        &mut state,
        &mut seq,
        Some(consumed_retry_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: consumed,
        },
    );
    assert!(!state.initial_input_submitted.contains(&consumed_retry_run));

    let terminal = ProducerMessageId::new_v7();
    let terminal_run = RunId::new_v7();
    apply(
        &mut state,
        &mut seq,
        None,
        accepted_message(terminal, owner.clone(), "terminal", None),
    );
    apply(
        &mut state,
        &mut seq,
        Some(terminal_run),
        EventPayload::RunCompleted { final_text: None },
    );
    apply(
        &mut state,
        &mut seq,
        Some(terminal_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: terminal,
        },
    );
    assert!(!state.initial_input_submitted.contains(&terminal_run));

    let admitted = ProducerMessageId::new_v7();
    let admitted_run = RunId::new_v7();
    let conflicting_run = RunId::new_v7();
    apply(
        &mut state,
        &mut seq,
        None,
        accepted_message(admitted, owner, "conflicting", None),
    );
    apply(
        &mut state,
        &mut seq,
        Some(admitted_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: admitted,
        },
    );
    apply(
        &mut state,
        &mut seq,
        Some(conflicting_run),
        EventPayload::ProducerMessageAdmitted {
            message_id: admitted,
        },
    );
    assert!(state.initial_input_submitted.contains(&admitted_run));
    assert!(!state.initial_input_submitted.contains(&conflicting_run));
}

#[test]
fn goal_control_messages_survive_terminal_and_reminder_discard() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let body = "Goal paused. Stop pursuing the objective.";
    let events = vec![
        stored_event(
            session_id,
            None,
            1,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: ProducerOwner::GoalControl { goal_id },
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: ProducerIdempotencyKey::new("pause-control").unwrap(),
                body: body.into(),
                reminder: None,
                agent_hop: None,
            },
        ),
        stored_event(
            session_id,
            Some(RunId::new_v7()),
            2,
            EventPayload::RunInterrupted { reason: None },
        ),
        stored_event(
            session_id,
            None,
            3,
            EventPayload::ProducerMessageDiscarded {
                message_id,
                reminder: Some(GoalReminderIdentity {
                    goal_id,
                    revision: 1,
                    kind: cookie_agent_protocol::GoalReminderKind::Continuation,
                }),
                producer_owner: None,
            },
        ),
    ];
    let replayed = reduce_session_events(session_id, 0, &events);
    assert!(matches!(
        producer_rows(&replayed).as_slice(),
        [TranscriptItem::ProducerMessage {
            body: received,
            reminder: None,
            status: ProducerMessageStatus::Pending,
            ..
        }] if received == body
    ));
    assert!(replayed.pending_inputs.is_empty());
    assert!(replayed.voided_inputs.is_empty());
}

#[test]
fn goal_reminder_discard_updates_one_row_without_entering_composer_lanes() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let reminder = GoalReminderIdentity {
        goal_id,
        revision: 7,
        kind: cookie_agent_protocol::GoalReminderKind::Continuation,
    };
    let mut state = SessionState::default();

    for (seq, payload) in [
        (
            1,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: ProducerOwner::Goal { goal_id },
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("goal-reminder-7").expect("key"),
                body: "full internal reminder body".into(),
                reminder: Some(reminder),
                agent_hop: None,
            },
        ),
        (
            2,
            EventPayload::ProducerMessageDiscarded {
                message_id,
                reminder: Some(GoalReminderIdentity {
                    revision: 6,
                    ..reminder
                }),
                producer_owner: None,
            },
        ),
        (
            3,
            EventPayload::ProducerMessageDiscarded {
                message_id,
                reminder: Some(reminder),
                producer_owner: None,
            },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            None,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    }

    assert!(matches!(
        producer_rows(&state).as_slice(),
        [TranscriptItem::ProducerMessage {
            producer_owner: ProducerOwner::Goal { goal_id: row_goal_id },
            status: ProducerMessageStatus::Discarded,
            ..
        }] if *row_goal_id == goal_id
    ));
    assert!(state.pending_inputs.is_empty());
    assert!(state.voided_inputs.is_empty());
    assert_eq!(producer_rows(&state).len(), 1);
}

#[test]
fn discard_inside_committed_input_window_stays_discarded_and_reverts() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let goal_id = GoalId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let reminder = GoalReminderIdentity {
        goal_id,
        revision: 3,
        kind: cookie_agent_protocol::GoalReminderKind::Continuation,
    };
    let events = vec![
        stored_event(
            session_id,
            None,
            1,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: ProducerOwner::Goal { goal_id },
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: ProducerIdempotencyKey::new("reminder-attempt").expect("key"),
                body: "internal reminder".into(),
                reminder: Some(reminder),
                agent_hop: None,
            },
        ),
        stored_event(
            session_id,
            Some(run_id),
            2,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        stored_event(
            session_id,
            None,
            3,
            EventPayload::ProducerMessageDiscarded {
                message_id,
                reminder: Some(reminder),
                producer_owner: None,
            },
        ),
        stored_event(session_id, Some(run_id), 4, committed_turn(3)),
    ];

    let replayed = reduce_session_events(session_id, 0, &events);
    assert!(matches!(
        producer_rows(&replayed).as_slice(),
        [TranscriptItem::ProducerMessage {
            status: ProducerMessageStatus::Discarded,
            ..
        }]
    ));
    assert!(!replayed.producer_messages[&message_id].consumption_recorded);

    let mut reverted_events = events;
    reverted_events.push(stored_event(
        session_id,
        None,
        5,
        EventPayload::SessionReverted { through_seq: 3 },
    ));
    let reverted = reduce_session_events(session_id, 0, &reverted_events);
    assert!(matches!(
        producer_rows(&reverted).as_slice(),
        [TranscriptItem::ProducerMessage {
            status: ProducerMessageStatus::Discarded,
            ..
        }]
    ));
}

#[test]
fn discard_after_committed_input_window_is_consumed_and_cleared() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let goal_id = GoalId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let reminder = GoalReminderIdentity {
        goal_id,
        revision: 4,
        kind: cookie_agent_protocol::GoalReminderKind::Continuation,
    };
    let state = reduce_session_events(
        session_id,
        0,
        &[
            stored_event(
                session_id,
                None,
                1,
                accepted_message(
                    message_id,
                    ProducerOwner::Goal { goal_id },
                    "covered-reminder",
                    Some(reminder),
                ),
            ),
            stored_event(
                session_id,
                Some(run_id),
                2,
                EventPayload::ProducerMessageAdmitted { message_id },
            ),
            stored_event(
                session_id,
                None,
                3,
                discarded_message(message_id, None, Some(reminder)),
            ),
            stored_event(session_id, Some(run_id), 4, committed_turn(2)),
        ],
    );

    assert_eq!(state.producer_messages[&message_id].discarded_seq, None);
    assert!(matches!(
        producer_rows(&state).as_slice(),
        [TranscriptItem::ProducerMessage {
            status: ProducerMessageStatus::Consumed,
            ..
        }]
    ));
}

#[test]
fn claims_validate_atomically_and_support_multiple_tokens_per_message() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let first = ProducerMessageId::new_v7();
    let second = ProducerMessageId::new_v7();
    let unknown = ProducerMessageId::new_v7();
    let mut state = SessionState::default();
    for (seq, payload) in [
        (
            1,
            accepted_message(
                first,
                ProducerOwner::Plugin {
                    plugin: "worker".into(),
                },
                "first-claim",
                None,
            ),
        ),
        (
            2,
            accepted_message(
                second,
                ProducerOwner::Plugin {
                    plugin: "worker".into(),
                },
                "second-claim",
                None,
            ),
        ),
        (
            3,
            EventPayload::ProducerMessageAdmitted { message_id: first },
        ),
        (
            4,
            EventPayload::ProducerMessageAdmitted { message_id: second },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            (seq >= 3).then_some(run_id),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    }

    for (seq, message_ids) in [
        (5, vec![]),
        (6, vec![first, unknown]),
        (7, vec![first, first]),
    ] {
        reduce_event(
            &mut state,
            session_id,
            Some(run_id),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            EventPayload::ProducerMessagesClaimed { message_ids },
        );
    }
    assert!(state.producer_claims.is_empty());
    assert!(
        state
            .producer_messages
            .values()
            .all(|message| message.claims.is_empty()
                && message.status == ProducerMessageStatus::Admitted)
    );

    reduce_event(
        &mut state,
        session_id,
        Some(run_id),
        8,
        jiff::Timestamp::new(8, 0).expect("timestamp"),
        EventPayload::ProducerMessagesClaimed {
            message_ids: vec![first, second],
        },
    );
    reduce_event(
        &mut state,
        session_id,
        Some(run_id),
        9,
        jiff::Timestamp::new(9, 0).expect("timestamp"),
        EventPayload::ProducerMessagesClaimed {
            message_ids: vec![first],
        },
    );

    assert_eq!(
        state.producer_messages[&first].claims,
        HashSet::from([8, 9])
    );
    assert_eq!(state.producer_messages[&second].claims, HashSet::from([8]));
    assert!(producer_rows(&state).iter().all(|row| matches!(
        row,
        TranscriptItem::ProducerMessage {
            status: ProducerMessageStatus::Claimed,
            ..
        }
    )));
}

#[test]
fn release_requires_claim_owner_and_only_last_release_unclaims_message() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let other_run = RunId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let mut state = SessionState::default();
    for (seq, payload) in [
        (
            1,
            accepted_message(
                message_id,
                ProducerOwner::Delegation {
                    invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                },
                "owned-release",
                None,
            ),
        ),
        (2, EventPayload::ProducerMessageAdmitted { message_id }),
        (
            3,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        ),
        (
            4,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            (seq >= 2).then_some(run_id),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    }
    for (seq, release_run, claim_seq) in [
        (5, Some(other_run), 3),
        (6, Some(run_id), 0),
        (7, Some(run_id), 99),
        (8, None, 3),
    ] {
        reduce_event(
            &mut state,
            session_id,
            release_run,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            EventPayload::ProducerMessagesReleased { claim_seq },
        );
    }
    assert_eq!(
        state.producer_messages[&message_id].claims,
        HashSet::from([3, 4])
    );

    for (seq, claim_seq) in [(9, 3), (10, 4)] {
        reduce_event(
            &mut state,
            session_id,
            Some(run_id),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            EventPayload::ProducerMessagesReleased { claim_seq },
        );
        assert_eq!(
            state.producer_messages[&message_id].status,
            if claim_seq == 3 {
                ProducerMessageStatus::Claimed
            } else {
                ProducerMessageStatus::Admitted
            }
        );
    }
    assert!(state.producer_claims.is_empty());
}

#[test]
fn claimed_message_rejects_discard_until_explicit_release() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let owner = ProducerOwner::Plugin {
        plugin: "jobs".into(),
    };
    let message_id = ProducerMessageId::new_v7();
    let mut state = SessionState::default();
    for (seq, event_run, payload) in [
        (
            1,
            None,
            accepted_message(message_id, owner.clone(), "claimed-discard", None),
        ),
        (
            2,
            Some(run_id),
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        (
            3,
            Some(run_id),
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        ),
        (
            4,
            None,
            discarded_message(message_id, Some(owner.clone()), None),
        ),
        (
            5,
            Some(run_id),
            EventPayload::ProducerMessagesReleased { claim_seq: 3 },
        ),
        (6, None, discarded_message(message_id, Some(owner), None)),
    ] {
        reduce_event(
            &mut state,
            session_id,
            event_run,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
        if seq == 4 {
            assert_eq!(state.producer_messages[&message_id].discarded_seq, None);
            assert_eq!(
                state.producer_messages[&message_id].status,
                ProducerMessageStatus::Claimed
            );
        }
    }
    assert_eq!(
        state.producer_messages[&message_id].status,
        ProducerMessageStatus::Discarded
    );
}

#[test]
fn generic_and_legacy_discards_require_exact_durable_identity() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let reminder = GoalReminderIdentity {
        goal_id,
        revision: 8,
        kind: cookie_agent_protocol::GoalReminderKind::Continuation,
    };
    let plugin = ProducerOwner::Plugin {
        plugin: "jobs".into(),
    };
    let delegation = ProducerOwner::Delegation {
        invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
    };
    let control = ProducerOwner::GoalControl { goal_id };
    let goal = ProducerOwner::Goal { goal_id };
    let owners = [
        (plugin, None, "plugin-owner"),
        (delegation, None, "delegation-owner"),
        (control, None, "goal-control-owner"),
        (goal.clone(), Some(reminder), "goal-owner"),
    ];
    let mut state = SessionState::default();
    let mut message_ids = Vec::new();
    for (index, (owner, accepted_reminder, key)) in owners.into_iter().enumerate() {
        let message_id = ProducerMessageId::new_v7();
        message_ids.push(message_id);
        let seq = index as u64 * 2 + 1;
        reduce_event(
            &mut state,
            session_id,
            None,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            accepted_message(message_id, owner.clone(), key, accepted_reminder),
        );
        reduce_event(
            &mut state,
            session_id,
            None,
            seq + 1,
            jiff::Timestamp::new(seq as i64 + 1, 0).expect("timestamp"),
            discarded_message(message_id, Some(owner), accepted_reminder),
        );
    }

    let legacy_id = ProducerMessageId::new_v7();
    reduce_event(
        &mut state,
        session_id,
        None,
        9,
        jiff::Timestamp::new(9, 0).expect("timestamp"),
        accepted_message(legacy_id, goal.clone(), "legacy-goal", Some(reminder)),
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        10,
        jiff::Timestamp::new(10, 0).expect("timestamp"),
        discarded_message(legacy_id, None, Some(reminder)),
    );
    message_ids.push(legacy_id);

    let invalid_goal = ProducerMessageId::new_v7();
    let invalid_control = ProducerMessageId::new_v7();
    reduce_event(
        &mut state,
        session_id,
        None,
        11,
        jiff::Timestamp::new(11, 0).expect("timestamp"),
        accepted_message(invalid_goal, goal, "goal-without-reminder", None),
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        12,
        jiff::Timestamp::new(12, 0).expect("timestamp"),
        accepted_message(
            invalid_control,
            ProducerOwner::GoalControl { goal_id },
            "control-with-reminder",
            Some(reminder),
        ),
    );

    assert!(message_ids.iter().all(|message_id| {
        state.producer_messages[message_id].status == ProducerMessageStatus::Discarded
    }));
    assert!(!state.producer_messages.contains_key(&invalid_goal));
    assert!(!state.producer_messages.contains_key(&invalid_control));
}

#[test]
fn release_does_not_resurrect_consumed_or_discarded_messages() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let owner = ProducerOwner::Plugin {
        plugin: "jobs".into(),
    };
    let consumed = ProducerMessageId::new_v7();
    let discarded = ProducerMessageId::new_v7();
    let mut state = SessionState::default();
    for (seq, event_run, payload) in [
        (
            1,
            None,
            accepted_message(consumed, owner.clone(), "consumed-release", None),
        ),
        (
            2,
            Some(run_id),
            EventPayload::ProducerMessageAdmitted {
                message_id: consumed,
            },
        ),
        (
            3,
            Some(run_id),
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![consumed],
            },
        ),
        (4, Some(run_id), committed_turn(2)),
        (
            5,
            Some(run_id),
            EventPayload::ProducerMessagesReleased { claim_seq: 3 },
        ),
        (
            6,
            None,
            accepted_message(discarded, owner.clone(), "discarded-release", None),
        ),
        (
            7,
            Some(run_id),
            EventPayload::ProducerMessageAdmitted {
                message_id: discarded,
            },
        ),
        (
            8,
            Some(run_id),
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![discarded],
            },
        ),
        (
            9,
            Some(run_id),
            EventPayload::ProducerMessagesReleased { claim_seq: 8 },
        ),
        (10, None, discarded_message(discarded, Some(owner), None)),
        (
            11,
            Some(run_id),
            EventPayload::ProducerMessagesReleased { claim_seq: 8 },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            event_run,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    }
    assert_eq!(
        state.producer_messages[&consumed].status,
        ProducerMessageStatus::Consumed
    );
    assert_eq!(
        state.producer_messages[&discarded].status,
        ProducerMessageStatus::Discarded
    );
}

#[test]
fn replay_and_revert_preserve_terminal_claim_until_explicit_recovery_release() {
    let session_id = SessionId::new_v7();
    let interrupted_run = RunId::new_v7();
    let retry_run = RunId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let mut events = vec![
        stored_event(
            session_id,
            None,
            1,
            accepted_message(
                message_id,
                ProducerOwner::Plugin {
                    plugin: "worker".into(),
                },
                "terminal-recovery",
                None,
            ),
        ),
        stored_event(
            session_id,
            Some(interrupted_run),
            2,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        stored_event(
            session_id,
            Some(interrupted_run),
            3,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        ),
        stored_event(
            session_id,
            Some(interrupted_run),
            4,
            EventPayload::RunInterrupted { reason: None },
        ),
        stored_event(
            session_id,
            Some(interrupted_run),
            5,
            EventPayload::ProducerMessagesReleased { claim_seq: 3 },
        ),
        stored_event(
            session_id,
            Some(retry_run),
            6,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
    ];
    let replayed = reduce_session_events(session_id, 2, &events);
    assert!(replayed.producer_claims.is_empty());
    assert_eq!(
        replayed.producer_messages[&message_id].admission,
        Some((retry_run, 6))
    );
    assert_eq!(
        replayed.producer_messages[&message_id].status,
        ProducerMessageStatus::Admitted
    );

    events.push(stored_event(
        session_id,
        None,
        7,
        EventPayload::SessionReverted { through_seq: 4 },
    ));
    let reverted = reduce_session_events(session_id, 3, &events);
    assert_eq!(reverted.producer_claims[&3].run_id, interrupted_run);
    assert_eq!(
        reverted.producer_messages[&message_id].claims,
        HashSet::from([3])
    );
    assert_eq!(
        reverted.producer_messages[&message_id].status,
        ProducerMessageStatus::Claimed
    );
}

#[test]
fn producer_can_be_readmitted_after_interruption_but_not_to_terminal_run() {
    let session_id = SessionId::new_v7();
    let first_run = RunId::new_v7();
    let second_run = RunId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let mut state = SessionState::default();
    let events = [
        stored_event(
            session_id,
            None,
            1,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: ProducerOwner::Plugin {
                    plugin: "worker".into(),
                },
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("retry").expect("key"),
                body: "result".into(),
                reminder: None,
                agent_hop: None,
            },
        ),
        stored_event(
            session_id,
            Some(first_run),
            2,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        stored_event(
            session_id,
            Some(first_run),
            3,
            EventPayload::RunInterrupted { reason: None },
        ),
        stored_event(
            session_id,
            Some(first_run),
            4,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        stored_event(
            session_id,
            Some(second_run),
            5,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
    ];
    for event in events {
        reduce_event(
            &mut state,
            session_id,
            event.run_id,
            event.seq,
            event.timestamp,
            event.payload,
        );
    }

    assert_eq!(
        state.producer_messages[&message_id]
            .admission
            .map(|(run_id, _)| run_id),
        Some(second_run)
    );
    assert!(matches!(
        producer_rows(&state).as_slice(),
        [TranscriptItem::ProducerMessage {
            status: ProducerMessageStatus::Admitted,
            ..
        }]
    ));
}

#[test]
fn sequenced_projection_rows_use_transcript_allocator_ids() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let mut state = SessionState {
        next_transcript_id: 50,
        transcript: vec![TranscriptItem::Event {
            id: 50,
            version: 0,
            level: EventLevel::Warning,
            text: "old diagnostic".into(),
        }],
        ..SessionState::default()
    };
    for (seq, payload) in [
        (
            1,
            EventPayload::GoalActivated {
                goal_id,
                objective: "Unique rows".into(),
                revision: 0,
                selection: None,
            },
        ),
        (
            2,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id,
                producer_owner: ProducerOwner::Plugin {
                    plugin: "worker".into(),
                },
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("unique-row").expect("key"),
                body: "message".into(),
                reminder: None,
                agent_hop: None,
            },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            None,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    }

    assert_eq!(
        state
            .transcript
            .iter()
            .map(TranscriptItem::id)
            .collect::<Vec<_>>(),
        vec![50, 51, 52]
    );
}

#[test]
fn producer_queue_identity_survives_admission_reordering_timing_pruning_and_replay() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let first_message_id = ProducerMessageId::new_v7();
    let second_message_id = ProducerMessageId::new_v7();
    let events = vec![
        stored_event(
            session_id,
            None,
            1,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id: first_message_id,
                producer_owner: ProducerOwner::Plugin {
                    plugin: "worker".into(),
                },
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("first").expect("key"),
                body: "first".into(),
                reminder: None,
                agent_hop: None,
            },
        ),
        stored_event(
            session_id,
            None,
            2,
            EventPayload::ProducerMessageAccepted {
                description: Default::default(),
                message_id: second_message_id,
                producer_owner: ProducerOwner::Delegation {
                    invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                },
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: ProducerIdempotencyKey::new("second").expect("key"),
                body: "second".into(),
                reminder: None,
                agent_hop: None,
            },
        ),
        stored_event(
            session_id,
            Some(run_id),
            3,
            EventPayload::ProducerMessageAdmitted {
                message_id: first_message_id,
            },
        ),
        stored_event(session_id, Some(run_id), 4, committed_turn(3)),
    ];

    let state = reduce_session_events(session_id, 0, &events);
    assert!(!state.event_timestamps.contains_key(&1));
    assert!(!state.event_timestamps.contains_key(&2));
    assert!(matches!(
        producer_rows(&state).as_slice(),
        [
            TranscriptItem::ProducerMessage {
                summary: None,
                id: 2,
                seq: 2,
                accepted_at: second_accepted_at,
                message_id: row_second_id,
                status: ProducerMessageStatus::Pending,
                ..
            },
            TranscriptItem::ProducerMessage {
                summary: None,
                id: 1,
                seq: 1,
                accepted_at: first_accepted_at,
                message_id: row_first_id,
                status: ProducerMessageStatus::Consumed,
                ..
            }
        ] if *row_first_id == first_message_id
            && *row_second_id == second_message_id
            && *first_accepted_at == jiff::Timestamp::new(1, 0).expect("timestamp")
            && *second_accepted_at == jiff::Timestamp::new(2, 0).expect("timestamp")
    ));
    assert!(state.pending_inputs.is_empty());
    assert!(state.voided_inputs.is_empty());
}

#[test]
fn paused_goal_accepts_empty_checklist_then_resumes_and_completes() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let mut state = SessionState::default();
    for (seq, payload) in [
        (
            1,
            EventPayload::GoalActivated {
                goal_id,
                objective: "Finish lifecycle".into(),
                revision: 0,
                selection: None,
            },
        ),
        (
            2,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Paused,
                revision: 1,
                selection: None,
            },
        ),
        (
            3,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: Vec::new(),
                revision: 2,
            },
        ),
        (
            4,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Active,
                revision: 3,
                selection: None,
            },
        ),
        (
            5,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: vec![goal_item("finish-lifecycle", true)],
                revision: 4,
            },
        ),
        (
            6,
            EventPayload::GoalLifecycleChanged {
                goal_id,
                status: GoalStatus::Completed,
                revision: 5,
                selection: None,
            },
        ),
    ] {
        reduce_event(
            &mut state,
            session_id,
            None,
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    }

    let statuses = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Goal { goal, .. } => Some(goal.status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::Paused,
            GoalStatus::Active,
            GoalStatus::Active,
            GoalStatus::Completed,
        ]
    );
    assert_eq!(
        state.goal.as_ref().expect("goal").status,
        GoalStatus::Completed
    );
}

#[test]
fn thinking_delta_after_committed_child_renumbering_appends_without_duplicate() {
    let item_id = 1;
    let mut state = SessionState {
        transcript: vec![TranscriptItem::Assistant {
            id: item_id,
            version: 0,
            attribution: FrozenAssistantAttribution {
                agent: AgentId::new("test").expect("agent id"),
                resolved_model: serde_json::from_value(serde_json::json!({
                    "provider_id": "test",
                    "model_id": "test",
                    "adapter_id": "openai-compatible",
                    "selection": {"model": "test/test", "variant": null},
                    "selection_fingerprint": "a".repeat(64)
                }))
                .expect("resolved model"),
            },
            committed_turn_seq: None,
            children: Vec::new(),
        }],
        ..SessionState::default()
    };

    append_assistant_delta(
        &mut state,
        item_id,
        10,
        "first".into(),
        AssistantPartKind::Thinking,
        jiff::Timestamp::now(),
    );
    let stale_open = state.open_assistant.expect("open thinking segment");
    let turn = PersistedModelTurn {
        content: vec![cookie_agent_protocol::PersistedAssistantPart::Reasoning {
            text: "first".into(),
            metadata: None,
        }],
        provider_options: BTreeMap::new(),
        finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
        usage: Usage::default(),
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        native_replay: None,
    };
    rebuild_committed_children(&mut state, item_id, 1, 20, 0, &turn);
    state.open_assistant = Some(stale_open);

    append_assistant_delta(
        &mut state,
        item_id,
        21,
        " second".into(),
        AssistantPartKind::Thinking,
        jiff::Timestamp::now(),
    );

    let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
        panic!("assistant item")
    };
    let thinking = children
        .iter()
        .filter_map(|child| match child {
            AssistantChild::Thinking { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(thinking, vec!["first second"]);
}

#[test]
fn tool_termination_clears_streamed_output_and_sets_detail() {
    let call_id = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: cookie_agent_protocol::ModelCallId::new("call").expect("model call id"),
        provider_item_id: None,
    };
    let mut state = SessionState::default();
    state.tools.insert(
        call_id,
        ToolCallState {
            id: call_id,
            owner: owner.clone(),
            presentation: cookie_agent_protocol::ToolCallPresentation {
                title: cookie_agent_protocol::SafeDisplayText::new("Bash")
                    .expect("presentation title"),
                primary_argument: None,
            },
            arguments: "{}".into(),
            status: ToolStatus::Running,
            detail: String::new(),
            has_output_chunks: false,
        },
    );

    reduce_event(
        &mut state,
        SessionId::new_v7(),
        None,
        1,
        jiff::Timestamp::now(),
        EventPayload::ToolCallProgress {
            tool_call_id: call_id,
            message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                .expect("progress message"),
            display: Some("streamed preview".into()),
        },
    );
    assert_eq!(state.tools[&call_id].detail, "streamed preview");
    assert!(state.tools[&call_id].has_output_chunks);

    reduce_event(
        &mut state,
        SessionId::new_v7(),
        None,
        2,
        jiff::Timestamp::now(),
        EventPayload::ToolCallTerminated {
            termination: cookie_agent_protocol::ToolCallTermination {
                tool_call_id: call_id,
                owner,
                outcome: ToolTerminationOutcome::Completed,
                result: Some(cookie_agent_protocol::PersistedToolResult {
                    display: None,
                    retained_output: None,
                    title: cookie_agent_protocol::SafeDisplayText::new("Bash")
                        .expect("result title"),
                    output: "stdout:\nonce\n\nstderr:\n".into(),
                    metadata: serde_json::Value::Null,
                    truncation: None,
                    attachments: Vec::new(),
                    additional_messages: vec![
                        cookie_agent_protocol::ToolEmittedMessage::new(
                            cookie_agent_protocol::ToolEmittedMessageRole::User,
                            vec![
                                cookie_agent_protocol::ToolEmittedContent::Text(
                                    "review this clip".into(),
                                ),
                                cookie_agent_protocol::ToolEmittedContent::File(
                                    cookie_agent_protocol::ToolAttachment {
                                        mime_type: cookie_agent_protocol::MimeType::new(
                                            "video/mp4",
                                        )
                                        .unwrap(),
                                        filename: Some("clip.mp4".into()),
                                        byte_length: 4,
                                        sha256: cookie_agent_protocol::Sha256Digest::of_bytes(
                                            b"clip",
                                        ),
                                        reference: cookie_agent_protocol::ArtifactReference {
                                            uri: format!(
                                                "artifact://sha256/{}",
                                                cookie_agent_protocol::Sha256Digest::of_bytes(
                                                    b"clip"
                                                )
                                            ),
                                        },
                                    },
                                ),
                            ],
                        )
                        .unwrap(),
                    ],
                }),
                error: None,
            },
        },
    );

    assert_eq!(state.tools[&call_id].status, ToolStatus::Completed);
    assert!(!state.tools[&call_id].has_output_chunks);
    assert_eq!(
        state.tools[&call_id].detail,
        format!(
            "Bash\nstdout:\nonce\n\nstderr:\n\nemitted user message:\ntext: review this clip\nfile: video/mp4 · 4 bytes · sha256:{digest} · artifact://sha256/{digest}",
            digest = cookie_agent_protocol::Sha256Digest::of_bytes(b"clip")
        )
    );
}

#[test]
fn tool_display_is_bounded_and_final_replacement_matches_replay() {
    let session = SessionId::new_v7();
    let call = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: cookie_agent_protocol::ModelCallId::new("display-call").unwrap(),
        provider_item_id: None,
    };
    let start = EventPayload::ToolCallStarted {
        start: cookie_agent_protocol::ToolCallStart {
            tool_call_id: call,
            output: Default::default(),
            owner: owner.clone(),
            presentation: cookie_agent_protocol::ToolCallPresentation {
                title: cookie_agent_protocol::SafeDisplayText::new("Test").unwrap(),
                primary_argument: None,
            },
            operation_fingerprint: serde_json::from_value(
                serde_json::json!({"digest": "1".repeat(64)}),
            )
            .unwrap(),
        },
    };
    let terminal = EventPayload::ToolCallTerminated {
        termination: cookie_agent_protocol::ToolCallTermination {
            tool_call_id: call,
            owner,
            outcome: ToolTerminationOutcome::Completed,
            error: None,
            result: Some(cookie_agent_protocol::PersistedToolResult {
                title: cookie_agent_protocol::SafeDisplayText::new("Test").unwrap(),
                output: "model-only authoritative output".into(),
                display: Some("final display\nsecond line".into()),
                retained_output: None,
                metadata: serde_json::json!({"model_only": true}),
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            }),
        },
    };
    let mut live = SessionState::default();
    reduce_event(
        &mut live,
        session,
        None,
        1,
        jiff::Timestamp::now(),
        start.clone(),
    );
    for seq in 2..302 {
        reduce_event(
            &mut live,
            session,
            None,
            seq,
            jiff::Timestamp::now(),
            EventPayload::ToolCallProgress {
                tool_call_id: call,
                message: cookie_agent_protocol::SafeDisplayText::new("status-only").unwrap(),
                display: Some("\u{20ac}".repeat(100)),
            },
        );
    }
    assert!(live.tools[&call].detail.len() <= cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES);
    assert!(live.tools[&call].detail.len() >= cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES - 3);
    reduce_event(
        &mut live,
        session,
        None,
        302,
        jiff::Timestamp::now(),
        terminal.clone(),
    );
    let mut replay = SessionState::default();
    reduce_event(&mut replay, session, None, 1, jiff::Timestamp::now(), start);
    reduce_event(
        &mut replay,
        session,
        None,
        302,
        jiff::Timestamp::now(),
        terminal,
    );
    assert_eq!(live.tools[&call].detail, "final display\nsecond line");
    assert_eq!(replay.tools[&call].detail, live.tools[&call].detail);
    assert!(!live.tools[&call].detail.contains("model-only"));
}

#[test]
fn committed_assistant_text_replaces_streamed_delta() {
    let item_id = 1;
    let mut state = assistant_state_with_item(item_id);
    append_assistant_delta(
        &mut state,
        item_id,
        10,
        "streamed draft".into(),
        AssistantPartKind::Text,
        jiff::Timestamp::now(),
    );
    let turn = PersistedModelTurn {
        content: vec![cookie_agent_protocol::PersistedAssistantPart::Text {
            text: "committed replacement".into(),
            metadata: None,
        }],
        provider_options: BTreeMap::new(),
        finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
        usage: Usage::default(),
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        native_replay: None,
    };

    rebuild_committed_children(&mut state, item_id, 1, 20, 0, &turn);

    let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
        panic!("assistant item")
    };
    assert!(matches!(
        children.as_slice(),
        [AssistantChild::Text { markdown, .. }]
            if markdown.as_str() == "committed replacement"
    ));
}

#[test]
fn replay_with_tool_chunks_ends_at_committed_result_only() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let call_id = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: cookie_agent_protocol::ModelCallId::new("replay-call")
            .expect("model call id"),
        provider_item_id: None,
    };
    let event = |seq, payload| StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: Some(run_id),
        seq,
        timestamp: jiff::Timestamp::now(),
        payload,
    };
    let events = vec![
        event(
            1,
            EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    output: Default::default(),
                    tool_call_id: call_id,
                    owner: owner.clone(),
                    presentation: cookie_agent_protocol::ToolCallPresentation {
                        title: cookie_agent_protocol::SafeDisplayText::new("Bash").expect("title"),
                        primary_argument: None,
                    },
                    operation_fingerprint: serde_json::from_value(serde_json::json!({
                        "digest": "1".repeat(64)
                    }))
                    .expect("fingerprint"),
                },
            },
        ),
        event(
            2,
            EventPayload::ToolCallProgress {
                tool_call_id: call_id,
                message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                    .expect("message"),
                display: Some("historical chunk".into()),
            },
        ),
        event(
            3,
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id: call_id,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        display: None,
                        retained_output: None,
                        title: cookie_agent_protocol::SafeDisplayText::new("Bash").expect("title"),
                        output: "committed replacement".into(),
                        metadata: serde_json::Value::Null,
                        truncation: None,
                        attachments: Vec::new(),
                        additional_messages: Vec::new(),
                    }),
                    error: None,
                },
            },
        ),
    ];

    let replayed = reduce_session_events(session_id, 0, &events);
    assert_eq!(
        replayed.tools[&call_id].detail,
        "Bash\ncommitted replacement"
    );
    assert!(!replayed.tools[&call_id].detail.contains("historical chunk"));
    assert!(!replayed.tools[&call_id].has_output_chunks);
}

#[test]
fn approval_escalation_is_info_while_rejected_and_expired_keep_existing_levels() {
    let session_id = SessionId::new_v7();
    let approval_id = ApprovalId::new_v7();
    let mut state = SessionState::default();

    reduce_event(
        &mut state,
        session_id,
        None,
        1,
        jiff::Timestamp::now(),
        EventPayload::ApprovalEscalated {
            approval_id,
            reason_code: cookie_agent_protocol::ApprovalReasonCode::Escalated,
        },
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        2,
        jiff::Timestamp::now(),
        EventPayload::ApprovalFinalized {
            approval_id,
            decision: cookie_agent_protocol::ApprovalFinalDecision {
                outcome: ApprovalFinalOutcome::Rejected,
                source: cookie_agent_protocol::ApprovalDecisionSource::Policy,
                reason_code: cookie_agent_protocol::ApprovalReasonCode::PolicyDenied,
                feedback: None,
                tree_grant_id: None,
            },
        },
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        3,
        jiff::Timestamp::now(),
        EventPayload::ApprovalFinalized {
            approval_id,
            decision: cookie_agent_protocol::ApprovalFinalDecision {
                outcome: ApprovalFinalOutcome::Expired,
                source: cookie_agent_protocol::ApprovalDecisionSource::System,
                reason_code: cookie_agent_protocol::ApprovalReasonCode::ApprovalExpired,
                feedback: None,
                tree_grant_id: None,
            },
        },
    );

    let levels = state
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::Event { level, .. } => *level,
            _ => panic!("approval lifecycle rows must be events"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        levels,
        vec![EventLevel::Info, EventLevel::Info, EventLevel::Info]
    );
}

fn timestamp(iso: &str) -> jiff::Timestamp {
    iso.parse().expect("timestamp")
}

fn assistant_state_with_item(item_id: u64) -> SessionState {
    SessionState {
        transcript: vec![TranscriptItem::Assistant {
            id: item_id,
            version: 0,
            attribution: FrozenAssistantAttribution {
                agent: AgentId::new("test").expect("agent id"),
                resolved_model: serde_json::from_value(serde_json::json!({
                    "provider_id": "test",
                    "model_id": "test",
                    "adapter_id": "openai-compatible",
                    "selection": {"model": "test/test", "variant": null},
                    "selection_fingerprint": "a".repeat(64)
                }))
                .expect("resolved model"),
            },
            committed_turn_seq: None,
            children: Vec::new(),
        }],
        ..SessionState::default()
    }
}

#[test]
fn thinking_durations_derive_from_durable_event_timestamps() {
    let item_id = 1;
    let mut state = assistant_state_with_item(item_id);
    let opened = timestamp("2026-08-07T10:00:00Z");
    append_assistant_delta(
        &mut state,
        item_id,
        10,
        "hmm".into(),
        AssistantPartKind::Thinking,
        opened,
    );
    assert!(state.has_open_thinking());
    // Continuing the open part keeps its original opening timestamp.
    append_assistant_delta(
        &mut state,
        item_id,
        11,
        "…".into(),
        AssistantPartKind::Thinking,
        timestamp("2026-08-07T10:00:01Z"),
    );
    close_open_assistant(&mut state, timestamp("2026-08-07T10:00:04Z"));
    assert!(!state.has_open_thinking());
    assert_eq!(
        state.thinking_duration(item_id, 10),
        Some(Duration::from_secs(4))
    );

    // Clock-skewed (negative) spans record nothing rather than panic.
    append_assistant_delta(
        &mut state,
        item_id,
        20,
        "again".into(),
        AssistantPartKind::Thinking,
        timestamp("2026-08-07T11:00:00Z"),
    );
    close_open_assistant(&mut state, timestamp("2026-08-07T10:59:00Z"));
    assert_eq!(state.thinking_duration(item_id, 20), None);
}

#[test]
fn thinking_duration_transfers_to_the_committed_child_on_rebuild() {
    let item_id = 1;
    let mut state = assistant_state_with_item(item_id);
    append_assistant_delta(
        &mut state,
        item_id,
        10,
        "streamed".into(),
        AssistantPartKind::Thinking,
        timestamp("2026-08-07T10:00:00Z"),
    );
    close_open_assistant(&mut state, timestamp("2026-08-07T10:00:07Z"));
    assert_eq!(
        state.thinking_duration(item_id, 10),
        Some(Duration::from_secs(7))
    );

    let turn = PersistedModelTurn {
        content: vec![cookie_agent_protocol::PersistedAssistantPart::Reasoning {
            text: "streamed".into(),
            metadata: None,
        }],
        provider_options: BTreeMap::new(),
        finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
        usage: Usage::default(),
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        native_replay: None,
    };
    rebuild_committed_children(&mut state, item_id, 1, 20, 0, &turn);
    // The streamed part id is gone; the committed child carries the time.
    assert_eq!(state.thinking_duration(item_id, 10), None);
    let TranscriptItem::Assistant { children, .. } = &state.transcript[0] else {
        panic!("assistant item")
    };
    let committed_id = children
        .iter()
        .find_map(|child| match child {
            AssistantChild::Thinking { id, .. } => Some(*id),
            _ => None,
        })
        .expect("committed thinking child");
    assert_ne!(committed_id, 10);
    assert_eq!(
        state.thinking_duration(item_id, committed_id),
        Some(Duration::from_secs(7))
    );
}

#[test]
fn session_revert_rebuilds_transcript_across_a_persisted_sequence_gap() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let event = |seq, payload| StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: Some(run_id),
        seq,
        timestamp: jiff::Timestamp::now(),
        payload,
    };
    let mut store = StateStore::default();
    assert!(store.apply_event(event(
        1,
        EventPayload::UserInputSubmitted {
            input: "kept".into(),
        },
    )));
    assert!(store.apply_event(event(
        3,
        EventPayload::UserInputSubmitted {
            input: "removed".into(),
        },
    )));
    let mut reverted = event(4, EventPayload::SessionReverted { through_seq: 1 });
    reverted.run_id = None;
    assert!(store.apply_event(reverted));
    let state = store.sessions.get(&session_id).expect("session state");
    assert_eq!(state.last_seq, 4);
    assert_eq!(state.transcript.len(), 1);
    assert!(matches!(
        &state.transcript[0],
        TranscriptItem::User { text, .. } if text == "kept"
    ));
}

#[test]
fn delegate_tool_starts_update_agent_activity() {
    let session_id = SessionId::new_v7();
    let mut state = SessionState::default();
    let fingerprint = || {
        serde_json::from_value(serde_json::json!({
            "digest": "1".repeat(64)
        }))
        .expect("operation fingerprint")
    };
    let start = |model_turn_seq, model_call_id: &str| {
        let model_call_id =
            cookie_agent_protocol::ModelCallId::new(model_call_id).expect("model call ID");
        cookie_agent_protocol::ToolCallStart {
            output: Default::default(),
            tool_call_id: ToolCallId::new_v7(),
            owner: cookie_agent_protocol::AssistantToolCallRef {
                model_turn_seq,
                content_index: 0,
                model_call_id,
                provider_item_id: None,
            },
            presentation: cookie_agent_protocol::ToolCallPresentation {
                title: cookie_agent_protocol::SafeDisplayText::new("activity tool").expect("title"),
                primary_argument: None,
            },
            operation_fingerprint: fingerprint(),
        }
    };
    state.turn_tool_index.insert(
        (1, "delegate-call".into()),
        IndexedToolCall {
            name: SafeCode::new("delegate_subagent").expect("tool name"),
            arguments: "{}".into(),
        },
    );
    state.turn_tool_index.insert(
        (3, "read-call".into()),
        IndexedToolCall {
            name: SafeCode::new("read").expect("tool name"),
            arguments: "{}".into(),
        },
    );

    let delegated_at = "2026-08-06T11:00:00Z".parse().expect("timestamp");
    reduce_event(
        &mut state,
        session_id,
        None,
        1,
        delegated_at,
        EventPayload::ToolCallStarted {
            start: start(1, "delegate-call"),
        },
    );
    assert_eq!(state.last_agent_activity, Some(delegated_at));

    reduce_event(
        &mut state,
        session_id,
        None,
        3,
        "2026-08-06T13:00:00Z".parse().expect("timestamp"),
        EventPayload::ToolCallStarted {
            start: start(3, "read-call"),
        },
    );
    assert_eq!(state.last_agent_activity, Some(delegated_at));
}

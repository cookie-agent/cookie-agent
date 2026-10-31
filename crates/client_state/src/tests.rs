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
                    background: false,
                    tool_call_id: call,
                    owner: owner.clone(),
                    output: Default::default(),
                    presentation: base_presentation(
                        cookie_agent_protocol::PreparedPresentation::new(
                            cookie_agent_protocol::SafeDisplayText::new(if case == 4 {
                                "bash"
                            } else {
                                "read"
                            })
                            .unwrap(),
                            None,
                        ),
                    ),
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
                            1 => cookie_agent_protocol::text_display("Finished"),
                            3 => cookie_agent_protocol::text_display("File missing"),
                            4 => cookie_agent_protocol::text_display(
                                "stdout\nstderr\nExit status: 1",
                            ),
                            _ => Vec::new(),
                        },
                        metadata: serde_json::json!({"status": 1, "success": false}),
                        background: None,
                        retained_output: None,
                        attachments: vec![],
                        additional_messages: vec![],
                        loaded_tools: Vec::new(),
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
                    display: Some(
                        cookie_agent_protocol::DisplayDelta::append(0, "File missing").into(),
                    ),
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
        let detail = tool.shown_text();
        assert_eq!(
            tool.status,
            if case == 4 {
                ToolStatus::Completed
            } else {
                ToolStatus::Failed
            }
        );
        if case == 4 {
            assert_eq!(detail, "stdout\nstderr\nExit status: 1");
        } else {
            assert!(detail.contains("Required file /work/Report.md missing"));
            // Case 2's output only streamed as live-only progress, which a
            // replay never sees. A result's display is what the call shows,
            // ahead of the output the model reads.
            match case {
                1 => {
                    assert!(detail.contains("Finished"), "{detail}");
                    assert!(!detail.contains("File missing"), "{detail}");
                }
                3 => assert!(detail.contains("File missing"), "{detail}"),
                _ => {}
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
                base_attempt_id: None,
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
fn an_empty_replay_delta_is_not_an_empty_history() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let evaluated = |seq: u64, base_attempt_id: Option<AttemptId>| {
        stored_event(
            session,
            Some(run),
            seq,
            EventPayload::ModelReplayEvaluated {
                base_attempt_id,
                attempt_id: AttemptId::new_v7(),
                resolved_model: resolved_model(),
                ordered_decisions: Vec::new(),
            },
        )
    };
    let empty_history_rows = |events: &[StoredEvent]| {
        reduce_session_events(session, 0, events)
            .transcript
            .iter()
            .filter(|item| {
                matches!(item, TranscriptItem::Event { text, .. } if text.contains("no history entries"))
            })
            .count()
    };
    assert_eq!(empty_history_rows(&[evaluated(1, None)]), 1);
    // Nothing new since the base: the base's rows already stand for it.
    assert_eq!(
        empty_history_rows(&[evaluated(1, Some(AttemptId::new_v7()))]),
        0
    );
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
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Cancelled {
                reason: None,
            }),
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
            AssistantChild::Notice { text } if text == &format!(
                "model interrupted · later requests follow this reply with: {}",
                cookie_agent_protocol::model_text::INTERRUPTED_TURN_MARKER
            )
        )),
        "the interruption notice, quoting the marker later requests carry, follows the \
         partial text: {children:?}"
    );
    // One notice: the abort that follows the commit adds none.
    assert_eq!(
        children
            .iter()
            .filter(|child| matches!(child, AssistantChild::Notice { .. }))
            .count(),
        1,
        "{children:?}"
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
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Failed {
                error: cookie_agent_protocol::SafeErrorMessage::new("model failed").unwrap(),
                model_error: Some(model_error.clone()),
                resolved_model: Some(resolved_model()),
            }),
        ),
        stored_event(
            session,
            None,
            2,
            EventPayload::internal_agent_finished(
                cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
                cookie_agent_protocol::InternalAgentRunId::new_v7(),
                cookie_agent_protocol::InternalAgentKind::ContextCompaction,
                cookie_agent_protocol::InternalAgentOutcome::Failed {
                    failure: cookie_agent_protocol::InternalAgentFailure {
                        code: cookie_agent_protocol::SafeCode::new("model_failure").unwrap(),
                        message: cookie_agent_protocol::SafeErrorMessage::new("Compaction failed")
                            .unwrap(),
                        retryable: false,
                        model_error: Some(model_error),
                    },
                },
            ),
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

/// A record a fork left behind shows only as a debug row naming its type.
#[test]
fn an_omitted_record_is_a_debug_row_and_nothing_else() {
    let session = SessionId::new_v7();
    let events = vec![stored_event(
        session,
        Some(RunId::new_v7()),
        7,
        EventPayload::RecordOmitted {
            omitted_type: cookie_agent_protocol::EventType::new("delegation_finished").unwrap(),
        },
    )];
    let replayed = reduce_session_events(session, 0, &events);
    assert_eq!(replayed.transcript.len(), 1, "{:?}", replayed.transcript);
    assert!(matches!(
        &replayed.transcript[0],
        TranscriptItem::Event { level: EventLevel::Debug, text, .. }
            if text == "event `delegation_finished` (seq 7) was not carried over"
    ));
}

fn model_inputs(state: &SessionState) -> Vec<(crate::model_input::ModelInputKind, &str, &str)> {
    state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ModelInput {
                kind, header, text, ..
            } => Some((*kind, header.as_str(), text.as_str())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_memory_snapshot_shows_the_turn_the_model_reads_when_it_changes() {
    use crate::model_input::ModelInputKind;
    let session = SessionId::new_v7();
    let snapshot = |body: &str| cookie_agent_protocol::MemorySnapshot {
        agent: Some(cookie_agent_protocol::MemorySnapshotAgent {
            agent: cookie_agent_protocol::AgentId::new("reviewer").unwrap(),
            memory: Some(cookie_agent_protocol::MemorySnapshotMemory {
                description: "Reviews".into(),
                keywords: Vec::new(),
                body: body.into(),
                status: cookie_agent_protocol::MemoryDocumentStatus::default(),
            }),
            omitted: false,
            listed: None,
        }),
        ..cookie_agent_protocol::MemorySnapshot::default()
    };
    let loaded = |seq, snapshot| {
        stored_event(
            session,
            Some(RunId::new_v7()),
            seq,
            EventPayload::MemoryLoaded { snapshot },
        )
    };
    let events = vec![
        loaded(1, snapshot("Prefers small diffs.")),
        // The same turn again adds no row.
        loaded(2, snapshot("Prefers small diffs.")),
        loaded(3, snapshot("Prefers small diffs and tests.")),
        loaded(4, cookie_agent_protocol::MemorySnapshot::unavailable()),
    ];
    let replayed = reduce_session_events(session, 0, &events);
    let rows = model_inputs(&replayed);
    assert_eq!(rows.len(), 3, "{rows:?}");
    for (index, body) in [
        (0, "Prefers small diffs."),
        (1, "Prefers small diffs and tests."),
    ] {
        let turn =
            cookie_agent_protocol::model_text::memory_snapshot_turn(&snapshot(body)).unwrap();
        assert_eq!(rows[index].0, ModelInputKind::Memory);
        assert_eq!(rows[index].2, turn);
        assert!(rows[index].1.starts_with("memory · "), "{}", rows[index].1);
    }
    assert_eq!(
        rows[2],
        (
            ModelInputKind::Memory,
            "memory · none in the model's context from here on",
            ""
        )
    );
}

#[test]
fn the_deferred_tools_prompt_and_inherited_context_show_what_the_model_reads() {
    use crate::model_input::ModelInputKind;
    let session = SessionId::new_v7();
    let group = cookie_agent_protocol::AnnouncedToolGroup {
        name: "github".into(),
        source: cookie_agent_protocol::ToolGroupSource::Mcp,
        summary: None,
        untrusted: false,
        tools: vec!["github_issues".into(), "github_pulls".into()],
    };
    let events = vec![
        stored_event(
            session,
            Some(RunId::new_v7()),
            1,
            EventPayload::DelegatedContextSeeded {
                invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
                turns: vec![cookie_agent_protocol::DelegatedContextTurn {
                    role: cookie_agent_protocol::DelegatedContextRole::User,
                    text: "Fix the build.".into(),
                }],
            },
        ),
        stored_event(
            session,
            Some(RunId::new_v7()),
            2,
            EventPayload::DeferredToolsAnnounced {
                added: vec![group.clone()],
                removed: Vec::new(),
            },
        ),
    ];
    let replayed = reduce_session_events(session, 0, &events);
    let announcement =
        cookie_agent_protocol::model_text::deferred_tools_announcement(&[group], &[]);
    // The announcement is the tools row's deferred tools prompt, not a row.
    assert_eq!(
        model_inputs(&replayed),
        [(
            ModelInputKind::InheritedContext,
            "context from the parent · 1 turn",
            "── user\nFix the build."
        )]
    );
    assert_eq!(
        crate::rows::deferred_tools_prompt(&replayed).as_deref(),
        Some(announcement.as_str())
    );
    assert_eq!(
        crate::rows::tool_group_slots(&replayed, &["github_pulls", "other", "github_issues"]),
        [0, 1, 0]
    );
}

#[test]
fn every_announcement_is_kept_and_a_removal_leaves_the_groups() {
    let session = SessionId::new_v7();
    let group = cookie_agent_protocol::AnnouncedToolGroup {
        name: "github".into(),
        source: cookie_agent_protocol::ToolGroupSource::Mcp,
        summary: None,
        untrusted: false,
        tools: vec!["github_issues".into()],
    };
    let announced = |seq| {
        stored_event(
            session,
            Some(RunId::new_v7()),
            seq,
            EventPayload::DeferredToolsAnnounced {
                added: vec![group.clone()],
                removed: Vec::new(),
            },
        )
    };
    let removed = stored_event(
        session,
        Some(RunId::new_v7()),
        2,
        EventPayload::DeferredToolsAnnounced {
            added: Vec::new(),
            removed: vec![group.clone()],
        },
    );
    let both = reduce_session_events(session, 0, &[announced(1), removed]);
    assert_eq!(both.deferred_announcements.len(), 2);
    assert!(both.deferred_tool_groups.is_empty(), "the removal folds");
    let state = reduce_session_events(session, 0, &[announced(1)]);
    assert_eq!(state.deferred_tool_groups["github_issues"], "github");
}

#[test]
fn a_changed_instructions_announcement_is_a_row_with_the_models_text() {
    use crate::model_input::ModelInputKind;

    let session = SessionId::new_v7();
    let blocks = vec![
        cookie_agent_protocol::AnnouncedPromptBlock {
            key: "runtimes".into(),
            text: Some("Working directory: /srv/api".into()),
        },
        cookie_agent_protocol::AnnouncedPromptBlock {
            key: "skills".into(),
            text: None,
        },
    ];
    let events = vec![stored_event(
        session,
        Some(RunId::new_v7()),
        1,
        EventPayload::PromptBlocksAnnounced {
            blocks: blocks.clone(),
        },
    )];
    let replayed = reduce_session_events(session, 0, &events);
    let text = cookie_agent_protocol::model_text::prompt_blocks_announcement(&blocks);
    assert!(
        text.starts_with("[cookie: instructions changed]\n<instructions_changed>\n<block name=\"runtimes\">\nWorking directory: /srv/api\n</block>\n<block name=\"skills\" removed=\"true\"/>\n</instructions_changed>\n"),
        "{text}"
    );
    assert_eq!(
        model_inputs(&replayed),
        [(
            ModelInputKind::PromptBlocks,
            "instructions changed · runtimes, skills removed",
            text.as_str()
        )]
    );
}

#[test]
fn an_acp_agents_first_prompt_context_is_a_row() {
    let session = SessionId::new_v7();
    let events = vec![stored_event(
        session,
        Some(RunId::new_v7()),
        1,
        EventPayload::AcpSessionUpdated {
            update: cookie_agent_protocol::AcpSessionUpdate {
                prompt_context: Some(vec!["<tool_instructions>".into(), "You review.".into()]),
                ..cookie_agent_protocol::AcpSessionUpdate::default()
            },
        },
    )];
    let replayed = reduce_session_events(session, 0, &events);
    assert_eq!(
        model_inputs(&replayed),
        [(
            crate::model_input::ModelInputKind::AgentContext,
            "context sent to the agent · 3 lines",
            "<tool_instructions>\n\nYou review."
        )]
    );
}

#[test]
fn a_tool_row_shows_what_its_result_sent_the_model_beyond_its_output() {
    let session = SessionId::new_v7();
    let call = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: cookie_agent_protocol::ModelCallId::new("call").unwrap(),
        provider_item_id: None,
    };
    let warning = format!(
        "{}: you have made …\n</system-reminder>",
        cookie_agent_protocol::model_text::LOOP_WARNING_MARKER
    );
    let events = vec![
        stored_event(
            session,
            None,
            1,
            EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    background: false,
                    tool_call_id: call,
                    owner: owner.clone(),
                    output: Default::default(),
                    presentation: base_presentation(
                        cookie_agent_protocol::PreparedPresentation::new(
                            cookie_agent_protocol::SafeDisplayText::new("bash").unwrap(),
                            None,
                        ),
                    ),
                    operation_fingerprint: serde_json::from_value(
                        serde_json::json!({"digest": "1".repeat(64)}),
                    )
                    .unwrap(),
                },
            },
        ),
        stored_event(
            session,
            None,
            2,
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id: call,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        title: cookie_agent_protocol::SafeDisplayText::new("ls").unwrap(),
                        output: format!("a.txt{warning}"),
                        display: cookie_agent_protocol::text_display("a.txt"),
                        metadata: serde_json::Value::Null,
                        background: None,
                        retained_output: None,
                        attachments: vec![],
                        additional_messages: vec![cookie_agent_protocol::ToolEmittedMessage {
                            role: cookie_agent_protocol::ToolEmittedMessageRole::User,
                            content: vec![cookie_agent_protocol::ToolEmittedContent::Text(
                                "Mind the cwd.".into(),
                            )],
                        }],
                        loaded_tools: Vec::new(),
                    }),
                    error: None,
                },
            },
        ),
        stored_event(
            session,
            None,
            3,
            EventPayload::ToolOutputElided {
                tool_call_id: call,
                original_bytes: 5,
                retained: cookie_agent_protocol::ArtifactReference {
                    uri: format!("artifact://sha256/{}", "a".repeat(64)),
                },
            },
        ),
    ];
    let state = reduce_session_events(session, 0, &events);
    let shown = state.tools[&call].shown_text();
    assert!(shown.starts_with("a.txt\n"), "{shown}");
    assert!(
        shown.contains("Sent to the model with this result:\n<system-reminder>\nRepeated tool calls detected: you have made …"),
        "{shown}"
    );
    assert!(
        shown.contains("Sent to the model after this result, as a user message:\nMind the cwd."),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "Left the model's context; the model reads instead:\n[tool output elided; 5 bytes retained; read it with read_artifact(artifact=\"{}\")] 1 tool-emitted message(s)",
            "a".repeat(64)
        )),
        "{shown}"
    );
}

#[test]
fn a_memory_change_without_its_call_is_a_row_of_its_own_that_no_level_hides() {
    use cookie_agent_protocol::MemoryChangeKind;
    let session = SessionId::new_v7();
    let run = Some(RunId::new_v7());
    let changed = |seq, change, url: &str| {
        stored_event(
            session,
            run,
            seq,
            EventPayload::MemoryChanged {
                change,
                url: url.into(),
            },
        )
    };
    let events = vec![
        changed(1, MemoryChangeKind::Saved, "project://cookie-agent/release"),
        changed(2, MemoryChangeKind::Edited, "user://preferences"),
        changed(3, MemoryChangeKind::Deleted, "agent://reviewer"),
        changed(4, MemoryChangeKind::Unknown, "machine://labpc/gpu"),
    ];
    let replayed = reduce_session_events(session, 0, &events);
    let rows = replayed
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::MemoryChange {
                seq, change, url, ..
            } => (*seq, format!("{} {url}", change.verb())),
            other => panic!("unexpected item {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            (1, "Saved project://cookie-agent/release".to_owned()),
            (2, "Edited user://preferences".to_owned()),
            (3, "Deleted agent://reviewer".to_owned()),
            (4, "Changed machine://labpc/gpu".to_owned()),
        ]
    );
}

/// A memory tool call as a daemon before 2026-10-24 recorded it: its
/// start, its termination, and a `memory_changed` for the URL it names.
struct OldMemoryCall {
    session: SessionId,
    run: Option<RunId>,
    call: ToolCallId,
    url: &'static str,
}

impl OldMemoryCall {
    fn new(session: SessionId, run: Option<RunId>, url: &'static str) -> Self {
        Self {
            session,
            run,
            call: ToolCallId::new_v7(),
            url,
        }
    }

    fn owner(&self) -> AssistantToolCallRef {
        AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new("call").unwrap(),
            provider_item_id: None,
        }
    }

    fn started(&self, seq: u64) -> StoredEvent {
        stored_event(
            self.session,
            self.run,
            seq,
            EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    background: false,
                    tool_call_id: self.call,
                    owner: self.owner(),
                    output: Default::default(),
                    presentation: base_presentation(
                        cookie_agent_protocol::PreparedPresentation::new(
                            cookie_agent_protocol::SafeDisplayText::new(format!(
                                "Edit memory {}",
                                self.url
                            ))
                            .unwrap(),
                            Some(cookie_agent_protocol::BoundedDisplayText::new(self.url).unwrap()),
                        ),
                    ),
                    operation_fingerprint: serde_json::from_value(
                        serde_json::json!({"digest": "1".repeat(64)}),
                    )
                    .unwrap(),
                },
            },
        )
    }

    fn terminated(&self, seq: u64) -> StoredEvent {
        stored_event(
            self.session,
            self.run,
            seq,
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id: self.call,
                    owner: self.owner(),
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        title: cookie_agent_protocol::SafeDisplayText::new("Edited memory")
                            .unwrap(),
                        output: format!("Edited {}.", self.url),
                        display: vec![cookie_agent_protocol::Open::Known(
                            cookie_agent_protocol::DisplayBlock::key_value([(
                                "url",
                                self.url.to_owned(),
                            )]),
                        )],
                        metadata: serde_json::json!({"url": self.url}),
                        background: None,
                        retained_output: None,
                        attachments: vec![],
                        additional_messages: vec![],
                        loaded_tools: Vec::new(),
                    }),
                    error: None,
                },
            },
        )
    }

    fn changed(&self, seq: u64, url: &str) -> StoredEvent {
        stored_event(
            self.session,
            self.run,
            seq,
            EventPayload::MemoryChanged {
                change: cookie_agent_protocol::MemoryChangeKind::Edited,
                url: url.into(),
            },
        )
    }
}

/// The memory change rows of `state`, as `<verb> <url>`.
fn memory_change_rows(state: &SessionState) -> Vec<String> {
    state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::MemoryChange { change, url, .. } => {
                Some(format!("{} {url}", change.verb()))
            }
            _ => None,
        })
        .collect()
}

/// An older daemon recorded a memory tool's change as its own event while
/// the call ran; it folds into the call's row, so one call is one row.
#[test]
fn a_memory_change_folds_into_the_tool_call_it_follows() {
    let session = SessionId::new_v7();
    let run = Some(RunId::new_v7());
    let edit = OldMemoryCall::new(session, run, "agent://builder");
    // As the engine wrote it: the change while the call ran.
    let state = reduce_session_events(
        session,
        0,
        &[
            edit.started(1),
            edit.changed(2, edit.url),
            edit.terminated(3),
        ],
    );
    assert!(
        memory_change_rows(&state).is_empty(),
        "{:?}",
        state.transcript
    );
    assert_eq!(state.tools[&edit.call].status, ToolStatus::Completed);
    // Right after the call's termination, it folds as well.
    let state = reduce_session_events(
        session,
        0,
        &[
            edit.started(1),
            edit.terminated(2),
            edit.changed(3, edit.url),
        ],
    );
    assert!(
        memory_change_rows(&state).is_empty(),
        "{:?}",
        state.transcript
    );
}

/// A change the reducer cannot tie to the call it follows keeps its own
/// row: another URL, another run, or something between the two.
#[test]
fn a_memory_change_that_follows_no_call_of_its_url_keeps_its_row() {
    let session = SessionId::new_v7();
    let run = Some(RunId::new_v7());
    let edit = OldMemoryCall::new(session, run, "agent://builder");
    let state = reduce_session_events(
        session,
        0,
        &[
            edit.started(1),
            edit.changed(2, "user://preferences"),
            edit.terminated(3),
        ],
    );
    assert_eq!(memory_change_rows(&state), ["Edited user://preferences"]);
    let other_run = OldMemoryCall {
        run: Some(RunId::new_v7()),
        ..OldMemoryCall::new(session, run, "agent://builder")
    };
    let state = reduce_session_events(
        session,
        0,
        &[
            other_run.started(1),
            other_run.terminated(2),
            edit.changed(3, edit.url),
        ],
    );
    assert_eq!(memory_change_rows(&state), ["Edited agent://builder"]);
    let state = reduce_session_events(
        session,
        0,
        &[
            edit.started(1),
            edit.terminated(2),
            edit.changed(3, "user://preferences"),
            edit.changed(4, edit.url),
        ],
    );
    assert_eq!(
        memory_change_rows(&state),
        ["Edited user://preferences", "Edited agent://builder"]
    );
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
        retained_output: None,
    }
}

fn discarded_message(
    message_id: ProducerMessageId,
    producer_owner: ProducerOwner,
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

    // A revert to sequence 1 cuts the log after it.
    let reverted_events = &events[..1];
    let reverted = reduce_session_events(session_id, 5, reverted_events);
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
    let owner = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("build-monitor").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
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
                retained_output: None,
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
                retained_output: None,
            },
        ),
        stored_event(
            session_id,
            Some(first_run),
            3,
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Completed {
                final_text: None,
            }),
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
    let owner = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("test").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
    let events = [
        accepted_message(first, owner.clone(), "first", None),
        accepted_message(second, owner, "second", None),
        EventPayload::UserInputSubmitted {
            content: "user input".into(),
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
                content: "steer after producer start".into(),
            },
        ),
        (
            2,
            None,
            accepted_message(
                message_id,
                ProducerOwner::external(
                    cookie_agent_protocol::ProducerSource::plugin("scheduler").unwrap(),
                    cookie_agent_protocol::ProducerName::default_name(),
                ),
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
            content: "steer after producer start".into(),
        },
    );
    assert!(state.pending_inputs.is_empty());
}

#[test]
fn invalid_producer_admissions_do_not_mark_initial_input() {
    let session_id = SessionId::new_v7();
    let owner = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("scheduler").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
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
        discarded_message(discarded, owner.clone(), None),
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
        EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Completed {
            final_text: None,
        }),
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
                retained_output: None,
            },
        ),
        stored_event(
            session_id,
            Some(RunId::new_v7()),
            2,
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Interrupted {
                reason: None,
            }),
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
                producer_owner: ProducerOwner::Goal { goal_id },
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
                retained_output: None,
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
                producer_owner: ProducerOwner::Goal {
                    goal_id: reminder.goal_id,
                },
            },
        ),
        (
            3,
            EventPayload::ProducerMessageDiscarded {
                message_id,
                reminder: Some(reminder),
                producer_owner: ProducerOwner::Goal {
                    goal_id: reminder.goal_id,
                },
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
                retained_output: None,
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
                producer_owner: ProducerOwner::Goal {
                    goal_id: reminder.goal_id,
                },
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

    // A revert to sequence 3 cuts the log after it.
    let reverted_events = &events[..3];
    let reverted = reduce_session_events(session_id, 0, reverted_events);
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
                discarded_message(message_id, ProducerOwner::Goal { goal_id }, Some(reminder)),
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
                ProducerOwner::external(
                    cookie_agent_protocol::ProducerSource::plugin("worker").unwrap(),
                    cookie_agent_protocol::ProducerName::default_name(),
                ),
                "first-claim",
                None,
            ),
        ),
        (
            2,
            accepted_message(
                second,
                ProducerOwner::external(
                    cookie_agent_protocol::ProducerSource::plugin("worker").unwrap(),
                    cookie_agent_protocol::ProducerName::default_name(),
                ),
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
    let owner = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("jobs").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
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
        (4, None, discarded_message(message_id, owner.clone(), None)),
        (
            5,
            Some(run_id),
            EventPayload::ProducerMessagesReleased { claim_seq: 3 },
        ),
        (6, None, discarded_message(message_id, owner, None)),
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
fn generic_and_goal_discards_require_exact_durable_identity() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let reminder = GoalReminderIdentity {
        goal_id,
        revision: 8,
        kind: cookie_agent_protocol::GoalReminderKind::Continuation,
    };
    let plugin = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("jobs").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
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
            discarded_message(message_id, owner, accepted_reminder),
        );
    }

    let goal_message_id = ProducerMessageId::new_v7();
    reduce_event(
        &mut state,
        session_id,
        None,
        9,
        jiff::Timestamp::new(9, 0).expect("timestamp"),
        accepted_message(
            goal_message_id,
            goal.clone(),
            "goal-reminder",
            Some(reminder),
        ),
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        10,
        jiff::Timestamp::new(10, 0).expect("timestamp"),
        discarded_message(goal_message_id, goal.clone(), Some(reminder)),
    );
    message_ids.push(goal_message_id);

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
    let owner = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("jobs").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
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
        (10, None, discarded_message(discarded, owner, None)),
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
                ProducerOwner::external(
                    cookie_agent_protocol::ProducerSource::plugin("worker").unwrap(),
                    cookie_agent_protocol::ProducerName::default_name(),
                ),
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
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Interrupted {
                reason: None,
            }),
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

    // A revert to sequence 4 cuts the log after it.
    events.truncate(4);
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
                producer_owner: ProducerOwner::external(
                    cookie_agent_protocol::ProducerSource::plugin("worker").unwrap(),
                    cookie_agent_protocol::ProducerName::default_name(),
                ),
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("retry").expect("key"),
                body: "result".into(),
                reminder: None,
                agent_hop: None,
                retained_output: None,
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
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Interrupted {
                reason: None,
            }),
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
            repeat: 1,
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
                producer_owner: ProducerOwner::external(
                    cookie_agent_protocol::ProducerSource::plugin("worker").unwrap(),
                    cookie_agent_protocol::ProducerName::default_name(),
                ),
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("unique-row").expect("key"),
                body: "message".into(),
                reminder: None,
                agent_hop: None,
                retained_output: None,
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
                producer_owner: ProducerOwner::external(
                    cookie_agent_protocol::ProducerSource::plugin("worker").unwrap(),
                    cookie_agent_protocol::ProducerName::default_name(),
                ),
                mode: ProducerDeliveryMode::Queue,
                idempotency_key: ProducerIdempotencyKey::new("first").expect("key"),
                body: "first".into(),
                reminder: None,
                agent_hop: None,
                retained_output: None,
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
                retained_output: None,
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
                acp_model: None,
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
            child_times: Vec::new(),
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
    rebuild_committed_children(
        &mut state,
        item_id,
        1,
        20,
        0,
        &turn,
        None,
        jiff::Timestamp::UNIX_EPOCH,
    );
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
fn tool_termination_replaces_the_live_display_with_the_result_display() {
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
        ToolCallState::started(
            call_id,
            owner.clone(),
            base_presentation(cookie_agent_protocol::PreparedPresentation::new(
                cookie_agent_protocol::SafeDisplayText::new("Bash").expect("presentation title"),
                None,
            )),
            "{}".into(),
        ),
    );

    reduce_transient(
        &mut state,
        jiff::Timestamp::now(),
        EventPayload::ToolCallProgress {
            tool_call_id: call_id,
            message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                .expect("progress message"),
            display: Some(
                cookie_agent_protocol::DisplayDelta::append(0, "streamed preview").into(),
            ),
        },
    );
    assert_eq!(state.tools[&call_id].shown_text(), "streamed preview");
    assert_eq!(
        state.tools[&call_id].progress.as_deref(),
        Some("bash stdout")
    );

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
                    display: cookie_agent_protocol::text_display("stdout:\nonce\n"),
                    background: None,
                    retained_output: None,
                    title: cookie_agent_protocol::SafeDisplayText::new("Bash")
                        .expect("result title"),
                    output: "stdout:\nonce\n\nstderr:\n".into(),
                    metadata: serde_json::Value::Null,
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
                    loaded_tools: Vec::new(),
                }),
                error: None,
            },
        },
    );

    assert_eq!(state.tools[&call_id].status, ToolStatus::Completed);
    assert!(state.tools[&call_id].live_display.is_empty());
    assert_eq!(state.tools[&call_id].progress, None);
    // The message the result emitted shows under its display, as the
    // model reads it.
    assert_eq!(
        state.tools[&call_id].shown_text(),
        "stdout:\nonce\n\nSent to the model after this result, as a user message:\n\
         review this clip\n[file: clip.mp4 (video/mp4)]"
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
            background: false,
            tool_call_id: call,
            output: Default::default(),
            owner: owner.clone(),
            presentation: base_presentation(cookie_agent_protocol::PreparedPresentation::new(
                cookie_agent_protocol::SafeDisplayText::new("Test").unwrap(),
                None,
            )),
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
                display: cookie_agent_protocol::text_display("final display\nsecond line"),
                background: None,
                retained_output: None,
                metadata: serde_json::json!({"model_only": true}),
                attachments: Vec::new(),
                additional_messages: Vec::new(),
                loaded_tools: Vec::new(),
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
    for _ in 2..302 {
        reduce_transient(
            &mut live,
            jiff::Timestamp::now(),
            EventPayload::ToolCallProgress {
                tool_call_id: call,
                message: cookie_agent_protocol::SafeDisplayText::new("status-only").unwrap(),
                display: Some(
                    cookie_agent_protocol::DisplayDelta::append(0, "\u{20ac}".repeat(100)).into(),
                ),
            },
        );
    }
    let streamed = live.tools[&call].shown_text();
    assert!(streamed.len() <= cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES);
    assert!(streamed.len() >= cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES - 3);
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
    assert_eq!(live.tools[&call].shown_text(), "final display\nsecond line");
    assert_eq!(
        replay.tools[&call].shown_text(),
        live.tools[&call].shown_text()
    );
    assert!(!live.tools[&call].shown_text().contains("model-only"));
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

    rebuild_committed_children(
        &mut state,
        item_id,
        1,
        20,
        0,
        &turn,
        None,
        jiff::Timestamp::UNIX_EPOCH,
    );

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
                    background: false,
                    output: Default::default(),
                    tool_call_id: call_id,
                    owner: owner.clone(),
                    presentation: base_presentation(
                        cookie_agent_protocol::PreparedPresentation::new(
                            cookie_agent_protocol::SafeDisplayText::new("Bash").expect("title"),
                            None,
                        ),
                    ),
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
                display: Some(
                    cookie_agent_protocol::DisplayDelta::append(0, "historical chunk").into(),
                ),
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
                        display: cookie_agent_protocol::text_display("committed replacement"),
                        background: None,
                        retained_output: None,
                        title: cookie_agent_protocol::SafeDisplayText::new("Bash").expect("title"),
                        output: "committed replacement".into(),
                        metadata: serde_json::Value::Null,
                        attachments: Vec::new(),
                        additional_messages: Vec::new(),
                        loaded_tools: Vec::new(),
                    }),
                    error: None,
                },
            },
        ),
    ];

    let replayed = reduce_session_events(session_id, 0, &events);
    assert_eq!(
        replayed.tools[&call_id].shown_text(),
        "committed replacement"
    );
    assert!(replayed.tools[&call_id].live_display.is_empty());
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
        EventPayload::approval_step(
            approval_id,
            cookie_agent_protocol::ApprovalStep::Escalated {
                reason_code: cookie_agent_protocol::ApprovalReasonCode::Escalated,
                expires_at: None,
            },
        ),
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        2,
        jiff::Timestamp::now(),
        EventPayload::approval_finalized(
            approval_id,
            cookie_agent_protocol::ApprovalFinalization::Decided {
                decision: cookie_agent_protocol::ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Rejected,
                    source: cookie_agent_protocol::ApprovalDecisionSource::Policy,
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::PolicyDenied,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        ),
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        3,
        jiff::Timestamp::now(),
        EventPayload::approval_finalized(
            approval_id,
            cookie_agent_protocol::ApprovalFinalization::Decided {
                decision: cookie_agent_protocol::ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Expired,
                    source: cookie_agent_protocol::ApprovalDecisionSource::System,
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::ApprovalExpired,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        ),
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
                acp_model: None,
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
            child_times: Vec::new(),
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
    rebuild_committed_children(
        &mut state,
        item_id,
        1,
        20,
        0,
        &turn,
        None,
        jiff::Timestamp::UNIX_EPOCH,
    );
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

/// The user messages of `session_id`'s transcript, in order.
fn user_texts(store: &StateStore, session_id: SessionId) -> Vec<String> {
    store.sessions[&session_id]
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::User { markdown, .. } => Some(markdown.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

fn submitted(session_id: SessionId, seq: u64, input: &str) -> StoredEvent {
    stored_event(
        session_id,
        None,
        seq,
        EventPayload::UserInputSubmitted {
            content: input.into(),
        },
    )
}

/// Delivers a replay of `events` that ends at `final_seq`.
fn replay(
    store: &mut StateStore,
    session_id: SessionId,
    rebuild: bool,
    final_seq: u64,
    events: Vec<StoredEvent>,
) -> Vec<DeliveryOutcome> {
    let mut deliveries = vec![ClientDelivery::ReplayStart {
        session_id,
        generation: 0,
        final_seq,
        rebuild,
    }];
    deliveries.extend(events.into_iter().map(|event| ClientDelivery::ReplayEvent {
        session_id,
        generation: 0,
        final_seq,
        event: Box::new(event),
    }));
    deliveries.push(ClientDelivery::ReplayEnd {
        session_id,
        generation: 0,
        final_seq,
    });
    deliveries
        .into_iter()
        .map(|delivery| store.apply_delivery(delivery))
        .collect()
}

fn live(message: EventSubscriptionMessage) -> ClientDelivery {
    ClientDelivery::Live {
        message: Box::new(message),
        generation: 0,
    }
}

#[test]
fn rewound_keeps_the_projection_until_the_full_replay_rebuilds_it() {
    let session_id = SessionId::new_v7();
    let mut store = StateStore::default();
    for (seq, input) in [(1, "kept"), (2, "removed"), (3, "also removed")] {
        assert!(store.apply_event(submitted(session_id, seq, input)));
    }
    assert_eq!(
        store.apply_delivery(live(EventSubscriptionMessage::Rewound {
            session_id,
            through_seq: 1,
        })),
        DeliveryOutcome::Applied
    );
    // An append that reuses a cut sequence is left to the full replay the
    // client runs next, and so is a replay from the view's cursor.
    assert_eq!(
        store.apply_delivery(live(EventSubscriptionMessage::Event {
            event: std::sync::Arc::new(submitted(session_id, 2, "edited")),
        })),
        DeliveryOutcome::Applied
    );
    assert_eq!(
        user_texts(&store, session_id),
        ["kept", "removed", "also removed"]
    );
    assert_eq!(
        replay(&mut store, session_id, false, 3, Vec::new())[0],
        DeliveryOutcome::ReplayFailed { session_id }
    );
    let outcomes = replay(
        &mut store,
        session_id,
        true,
        2,
        vec![
            submitted(session_id, 1, "kept"),
            submitted(session_id, 2, "edited"),
        ],
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| *outcome == DeliveryOutcome::Applied)
    );
    assert_eq!(user_texts(&store, session_id), ["kept", "edited"]);
    // Live events follow on again.
    assert!(store.apply_event(submitted(session_id, 3, "next")));
    assert_eq!(user_texts(&store, session_id), ["kept", "edited", "next"]);
}

#[test]
fn a_replay_from_the_cursor_changes_the_projection_only_when_it_is_complete() {
    let session_id = SessionId::new_v7();
    let mut store = StateStore::default();
    assert!(store.apply_event(submitted(session_id, 1, "first")));
    store
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .background_calls = vec![
        serde_json::from_value(serde_json::json!({
            "job": "job-1",
            "tool": "bash",
            "tool_call_id": ToolCallId::new_v7(),
            "started_at": "2026-10-23T10:00:00Z",
            "detached_at": "2026-10-23T10:00:01Z",
            "by": "user",
        }))
        .expect("background call"),
    ];
    let version = store.sessions[&session_id].version;
    for delivery in [
        ClientDelivery::ReplayStart {
            session_id,
            generation: 0,
            final_seq: 3,
            rebuild: false,
        },
        ClientDelivery::ReplayEvent {
            session_id,
            generation: 0,
            final_seq: 3,
            event: Box::new(submitted(session_id, 2, "second")),
        },
    ] {
        assert_eq!(store.apply_delivery(delivery), DeliveryOutcome::Applied);
    }
    // Staged: the visible projection has not moved.
    assert_eq!(user_texts(&store, session_id), ["first"]);
    assert_eq!(store.sessions[&session_id].version, version);
    for delivery in [
        ClientDelivery::ReplayEvent {
            session_id,
            generation: 0,
            final_seq: 3,
            event: Box::new(submitted(session_id, 3, "third")),
        },
        ClientDelivery::ReplayEnd {
            session_id,
            generation: 0,
            final_seq: 3,
        },
    ] {
        assert_eq!(store.apply_delivery(delivery), DeliveryOutcome::Applied);
    }
    let state = &store.sessions[&session_id];
    assert_eq!(user_texts(&store, session_id), ["first", "second", "third"]);
    assert_eq!(state.last_seq, 3);
    assert_ne!(state.version, version);
    // What the log does not hold stays.
    assert_eq!(state.background_calls.len(), 1);
    // A replay that ends short changes nothing.
    let outcomes = replay(
        &mut store,
        session_id,
        false,
        5,
        vec![submitted(session_id, 4, "fourth")],
    );
    assert_eq!(
        outcomes.last(),
        Some(&DeliveryOutcome::ReplayFailed { session_id })
    );
    assert_eq!(user_texts(&store, session_id), ["first", "second", "third"]);
}

#[test]
fn an_evicted_session_drops_what_still_arrives_until_it_is_replayed_again() {
    let session_id = SessionId::new_v7();
    let mut store = StateStore::default();
    assert!(store.apply_event(submitted(session_id, 1, "first")));
    assert!(store.can_evict(session_id));
    assert!(store.evict_session(session_id));
    assert!(!store.sessions.contains_key(&session_id));
    // Nothing is left to keep.
    assert!(store.can_evict(session_id));
    // A message in flight when the subscription was released does not
    // bring back a projection missing the start of the log.
    assert_eq!(
        store.apply_delivery(live(EventSubscriptionMessage::Event {
            event: std::sync::Arc::new(submitted(session_id, 2, "second")),
        })),
        DeliveryOutcome::Applied
    );
    assert!(!store.sessions.contains_key(&session_id));
    let outcomes = replay(
        &mut store,
        session_id,
        true,
        2,
        vec![
            submitted(session_id, 1, "first"),
            submitted(session_id, 2, "second"),
        ],
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| *outcome == DeliveryOutcome::Applied)
    );
    assert_eq!(user_texts(&store, session_id), ["first", "second"]);
    assert!(store.apply_event(submitted(session_id, 3, "third")));
    assert_eq!(user_texts(&store, session_id), ["first", "second", "third"]);
}

#[test]
fn a_session_still_owed_to_its_view_is_not_evicted() {
    let session_id = SessionId::new_v7();
    let mut store = StateStore::default();
    assert!(store.apply_event(submitted(session_id, 1, "first")));
    store
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .active_run = Some(RunId::new_v7());
    assert!(!store.can_evict(session_id));
    assert!(!store.evict_session(session_id));
    assert!(store.sessions.contains_key(&session_id));
}

#[test]
fn delegation_hinted_tool_starts_update_agent_activity() {
    let session_id = SessionId::new_v7();
    let mut state = SessionState::default();
    let fingerprint = || {
        serde_json::from_value(serde_json::json!({
            "digest": "1".repeat(64)
        }))
        .expect("operation fingerprint")
    };
    let start = |model_turn_seq, model_call_id: &str, hint: &str| {
        let model_call_id =
            cookie_agent_protocol::ModelCallId::new(model_call_id).expect("model call ID");
        cookie_agent_protocol::ToolCallStart {
            background: false,
            output: Default::default(),
            tool_call_id: ToolCallId::new_v7(),
            owner: cookie_agent_protocol::AssistantToolCallRef {
                model_turn_seq,
                content_index: 0,
                model_call_id,
                provider_item_id: None,
            },
            // The hint decides, not the tool's name.
            presentation: base_presentation(
                cookie_agent_protocol::PreparedPresentation::new(
                    cookie_agent_protocol::SafeDisplayText::new("delegate_subagent")
                        .expect("title"),
                    None,
                )
                .with_hint(hint),
            ),
            operation_fingerprint: fingerprint(),
        }
    };
    let delegated_at = "2026-08-06T11:00:00Z".parse().expect("timestamp");
    reduce_event(
        &mut state,
        session_id,
        None,
        1,
        delegated_at,
        EventPayload::ToolCallStarted {
            start: start(
                1,
                "delegate-call",
                cookie_agent_protocol::ToolHint::DELEGATE,
            ),
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
            start: start(3, "read-call", cookie_agent_protocol::ToolHint::FILE_READ),
        },
    );
    assert_eq!(state.last_agent_activity, Some(delegated_at));
}

#[test]
fn repeated_warnings_fold_into_one_counted_row() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let model_error: ModelErrorSummary = serde_json::from_value(serde_json::json!({"kind":"timeout","message":"OpenAI request failed","retryable":true,"stage":"stream_read","http_status":503,"bytes_received":97,"vendor_code":null,"request_id":null,"retry_after_ms":null})).unwrap();
    let mut events = Vec::new();
    let mut push = |payload| {
        let seq = events.len() as u64 + 1;
        events.push(stored_event(session, Some(run), seq, payload));
    };
    // A retry loop: each attempt opens a block, then fails the same way.
    for _ in 0..3 {
        let attempt_id = AttemptId::new_v7();
        push(EventPayload::ModelAttemptStarted {
            attempt_id,
            attempt_ordinal: 1,
            fallback_index: 0,
            retry_ordinal: 0,
            resolved_model: resolved_model(),
            prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
        });
        push(EventPayload::AttemptAbandoned {
            attempt_id,
            model_error: Some(model_error.clone()),
        });
    }
    // A different failure starts a new row.
    let attempt_id = AttemptId::new_v7();
    push(EventPayload::AttemptAbandoned {
        attempt_id,
        model_error: None,
    });
    let state = reduce_session_events(session, 0, &events);
    let rows = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Event { text, repeat, .. } => Some((text.as_str(), *repeat)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[0].0.contains("HTTP 503"), "{rows:?}");
    assert_eq!(rows[0].1, 3);
    assert_eq!(rows[1], ("model attempt abandoned", 1));
}

#[test]
fn repeated_warnings_fold_onto_one_row_across_other_rows() {
    let mut state = SessionState::default();
    let at = |second: i64| jiff::Timestamp::new(second, 0).unwrap();
    let push = |state: &mut SessionState, level, text: &str, second| {
        super::reduce::push_event(state, level, text.into(), at(second));
    };
    push(&mut state, EventLevel::Warning, "fallback", 1);
    push(&mut state, EventLevel::Info, "turn committed", 2);
    push(&mut state, EventLevel::Warning, "other warning", 3);
    push(&mut state, EventLevel::Error, "fallback", 4);
    push(&mut state, EventLevel::Warning, "fallback", 5);
    let rows = state
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::Event {
                id,
                level,
                text,
                repeat,
                ..
            } => (*level, text.as_str(), *repeat, state.item_time(*id)),
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    // The repeat moved to its latest occurrence and took its time; a row
    // at another level never folds with it.
    assert_eq!(
        rows,
        [
            (EventLevel::Info, "turn committed", 1, Some(at(2))),
            (EventLevel::Warning, "other warning", 1, Some(at(3))),
            (EventLevel::Error, "fallback", 1, Some(at(4))),
            (EventLevel::Warning, "fallback", 2, Some(at(5))),
        ]
    );
}

#[test]
fn repeated_info_rows_stay_one_per_event() {
    let mut state = SessionState::default();
    for _ in 0..2 {
        super::reduce::push_event(
            &mut state,
            EventLevel::Info,
            "turn committed".into(),
            jiff::Timestamp::now(),
        );
    }
    assert_eq!(state.transcript.len(), 2);
}

#[test]
fn connection_failures_become_error_rows_in_each_named_session() {
    let named = SessionId::new_v7();
    let other = SessionId::new_v7();
    let unknown = SessionId::new_v7();
    let mut store = StateStore::default();
    store.sessions.insert(named, SessionState::default());
    store.sessions.insert(other, SessionState::default());
    let errors = |store: &StateStore, session: SessionId| {
        store.sessions[&session]
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Event {
                    level: EventLevel::Error,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let runtime_id = |id: &str| cookie_agent_protocol::RuntimeId::new(id).unwrap();
    assert!(matches!(
        store.apply_delivery(ClientDelivery::McpChanged(
            cookie_agent_protocol::McpChangedNotification {
                failures: vec![
                    cookie_agent_protocol::McpServerFailure {
                        runtime_id: runtime_id("global"),
                        workspace: None,
                        server: "docs".into(),
                        reason: "spawn failed".into(),
                        session_ids: vec![named, unknown],
                    },
                    cookie_agent_protocol::McpServerFailure {
                        runtime_id: runtime_id("labpc"),
                        workspace: Some("/srv/api".into()),
                        server: "lsp".into(),
                        reason: "exited".into(),
                        session_ids: vec![named],
                    },
                ],
            }
        )),
        DeliveryOutcome::Applied
    ));
    assert!(matches!(
        store.apply_delivery(ClientDelivery::RuntimesChanged(
            cookie_agent_protocol::RuntimesChangedNotification {
                failures: vec![cookie_agent_protocol::RuntimeFailure {
                    runtime_id: runtime_id("labpc"),
                    capability: cookie_agent_protocol::CapabilityName::coding(),
                    workspace: Some("/srv/api".into()),
                    reason: "connection refused".into(),
                    session_ids: vec![named],
                }],
            }
        )),
        DeliveryOutcome::Applied
    ));
    assert_eq!(
        errors(&store, named),
        [
            "MCP server `docs` (global runtime) is unavailable: spawn failed",
            "MCP server `lsp` (labpc runtime at /srv/api) is unavailable: exited",
            "runtime `labpc` at /srv/api is unavailable: connection refused",
        ]
    );
    assert!(errors(&store, other).is_empty());
    assert!(!store.sessions.contains_key(&unknown));
}

#[test]
fn user_file_parts_render_as_attachment_chips() {
    use cookie_agent_protocol::{ArtifactRef, Open, UserContent, UserContentPart};
    let content = UserContent::new(vec![
        Open::Known(UserContentPart::Text {
            text: "What is wrong with this *layout*?".into(),
        }),
        Open::Known(UserContentPart::File {
            artifact: ArtifactRef {
                sha256: "ab".repeat(32),
                byte_length: 482_113,
                media_type: "image/png".into(),
                filename: Some("my_screenshot.png".into()),
            },
        }),
    ])
    .unwrap();
    assert_eq!(
        super::user_content_text(&content),
        "What is wrong with this *layout*?\n📎 my_screenshot.png · image/png · 471 KB"
    );
    assert_eq!(super::user_content_text(&"plain".into()), "plain");
    let mut state = SessionState::default();
    reduce_event(
        &mut state,
        SessionId::new_v7(),
        Some(RunId::new_v7()),
        1,
        jiff::Timestamp::new(1, 0).expect("timestamp"),
        EventPayload::UserInputSubmitted { content },
    );
    let Some(TranscriptItem::User { markdown, .. }) = state.transcript.last() else {
        panic!("expected a user row");
    };
    assert_eq!(
        markdown.as_str(),
        "What is wrong with this *layout*?\n📎 my\\_screenshot\\.png · image\\/png · 471 KB"
    );
}

/// `presentation` as `tool_call_started` stores it for base-layer tool
/// `presentation.title`.
fn base_presentation(
    presentation: cookie_agent_protocol::PreparedPresentation,
) -> cookie_agent_protocol::ToolPresentation {
    let name = presentation.title.to_string();
    cookie_agent_protocol::ToolPresentation::new(
        presentation,
        cookie_agent_protocol::ToolIdentity {
            name: name.clone(),
            published_name: name,
            source: Some(cookie_agent_protocol::Open::Known(
                cookie_agent_protocol::ToolSource::Base,
            )),
        },
    )
}

/// An event a newer daemon wrote changes nothing in the projection but its
/// sequence, in a replay and live, and shows only as a debug row.
#[test]
fn unknown_events_advance_the_sequence_and_show_only_in_the_debug_view() {
    let session_id = SessionId::new_v7();
    let unknown = |seq| {
        Box::new(cookie_agent_protocol::UnknownEvent {
            session_id,
            run_id: None,
            seq,
            timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            origin: None,
            tag: "from_the_future".into(),
        })
    };
    let mut store = StateStore::default();
    let created = stored_event(
        session_id,
        Some(RunId::new_v7()),
        1,
        EventPayload::UserInputAdmitted {
            content: "hello".into(),
        },
    );
    for delivery in [
        ClientDelivery::ReplayStart {
            session_id,
            generation: 0,
            final_seq: 2,
            rebuild: true,
        },
        ClientDelivery::ReplayEvent {
            session_id,
            generation: 0,
            final_seq: 2,
            event: Box::new(created),
        },
        ClientDelivery::UnknownEvent {
            event: unknown(2),
            generation: 0,
            replay_final_seq: Some(2),
        },
        ClientDelivery::ReplayEnd {
            session_id,
            generation: 0,
            final_seq: 2,
        },
    ] {
        assert_eq!(store.apply_delivery(delivery), DeliveryOutcome::Applied);
    }
    assert_eq!(store.sessions[&session_id].last_seq, 2);
    assert_eq!(
        store.apply_delivery(ClientDelivery::UnknownEvent {
            event: unknown(3),
            generation: 0,
            replay_final_seq: None,
        }),
        DeliveryOutcome::Applied
    );
    let state = &store.sessions[&session_id];
    assert_eq!(state.last_seq, 3);
    let rows = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Event { level, text, .. } if text.contains("from_the_future") => {
                Some(*level)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, [EventLevel::Debug, EventLevel::Debug]);
}

#[test]
fn a_user_messages_files_are_kept_by_its_sequence() {
    let session_id = SessionId::new_v7();
    let artifact: cookie_agent_protocol::ArtifactRef = serde_json::from_value(serde_json::json!({
        "sha256": "b".repeat(64),
        "byte_length": 3,
        "media_type": "image/png",
        "filename": "dot.png",
    }))
    .expect("artifact");
    let content = cookie_agent_protocol::UserContent::new(vec![
        cookie_agent_protocol::Open::Known(cookie_agent_protocol::UserContentPart::Text {
            text: "look".into(),
        }),
        cookie_agent_protocol::Open::Known(cookie_agent_protocol::UserContentPart::File {
            artifact: artifact.clone(),
        }),
    ])
    .expect("content");
    let mut state = SessionState::default();
    reduce_event(
        &mut state,
        session_id,
        None,
        7,
        jiff::Timestamp::new(7, 0).expect("timestamp"),
        EventPayload::UserInputSubmitted { content },
    );
    assert_eq!(state.user_files.get(&7), Some(&vec![artifact]));
}

#[test]
fn an_acp_sessions_commands_options_and_plan_fold_from_its_events() {
    use cookie_agent_protocol::{
        AcpCommand, AcpConfigOption, AcpConfigValue, AcpSessionRecord, AcpSessionStart,
        AcpSessionUpdate, DisplayBlock, Open, PlanEntry, PlanEntryStatus,
    };
    let session_id = SessionId::new_v7();
    let (first, second) = (RunId::new_v7(), RunId::new_v7());
    let mut state = SessionState::default();
    let apply = |state: &mut SessionState, run, seq, payload| {
        reduce_event(
            state,
            session_id,
            Some(run),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    };
    let plan = |statuses: &[PlanEntryStatus]| {
        Open::Known(DisplayBlock::Plan {
            entries: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| PlanEntry {
                    content: format!("step {index}"),
                    status: *status,
                    priority: None,
                })
                .collect(),
        })
    };
    let plans = |state: &SessionState| {
        state
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Plan { run, entries, .. } => Some((
                    *run,
                    entries.iter().map(|entry| entry.status).collect::<Vec<_>>(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    apply(
        &mut state,
        first,
        1,
        EventPayload::AcpSessionStarted {
            record: AcpSessionRecord {
                agent: AgentId::new("local-claude").unwrap(),
                runtime_id: cookie_agent_protocol::RuntimeId::new("local").unwrap(),
                runtime_agent: AgentId::new("claude").unwrap(),
                acp_session_id: "s-1".into(),
                cwd: "/work".into(),
                protocol_version: 1,
                agent_info: serde_json::Value::Null,
                how: AcpSessionStart::New,
                steering: false,
                forked_from: None,
                fork: None,
                fork_point: None,
            },
        },
    );
    assert_eq!(
        state.acp.agent.as_ref().map(AgentId::as_str),
        Some("local-claude")
    );
    apply(
        &mut state,
        first,
        2,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                commands: Some(vec![
                    AcpCommand {
                        name: "review".into(),
                        description: "Review the changes".into(),
                        input_hint: Some("what to review".into()),
                    },
                    AcpCommand {
                        name: "compact".into(),
                        description: String::new(),
                        input_hint: None,
                    },
                ]),
                options: Some(vec![AcpConfigOption {
                    id: "mode".into(),
                    name: "Mode".into(),
                    kind: Default::default(),
                    description: None,
                    category: Some("mode".into()),
                    current_value: "auto".into(),
                    values: vec![AcpConfigValue {
                        value: "auto".into(),
                        name: "Auto".into(),
                        description: None,
                    }],
                }]),
                plan: Some(plan(&[
                    PlanEntryStatus::InProgress,
                    PlanEntryStatus::Pending,
                ])),
                ..AcpSessionUpdate::default()
            },
        },
    );
    assert_eq!(
        state
            .acp
            .commands_matching("re")
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>(),
        ["review"]
    );
    assert!(state.acp.has_command("compact"));
    assert_eq!(state.acp.options[0].current_value, "auto");
    // The starting options a session was created with are kept beside the
    // options, which they do not replace.
    apply(
        &mut state,
        first,
        3,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                starting_options: Some(vec![cookie_agent_protocol::AcpOptionChoice {
                    option_id: "model".into(),
                    value: "opus".into(),
                }]),
                ..AcpSessionUpdate::default()
            },
        },
    );
    assert_eq!(state.acp.starting_options[0].value, "opus");
    assert_eq!(state.acp.options[0].current_value, "auto");
    // A later update of the same run replaces the run's plan in place and
    // keeps what it does not carry.
    apply(
        &mut state,
        first,
        3,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                plan: Some(plan(&[
                    PlanEntryStatus::Completed,
                    PlanEntryStatus::InProgress,
                ])),
                ..AcpSessionUpdate::default()
            },
        },
    );
    assert_eq!(state.acp.commands.len(), 2);
    assert_eq!(
        plans(&state),
        [(
            Some(first),
            vec![PlanEntryStatus::Completed, PlanEntryStatus::InProgress]
        )]
    );
    // Another run's plan is a row of its own.
    apply(
        &mut state,
        second,
        4,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                plan: Some(plan(&[PlanEntryStatus::Pending])),
                ..AcpSessionUpdate::default()
            },
        },
    );
    assert_eq!(plans(&state).len(), 2);
    assert_eq!(
        plans(&state)[1],
        (Some(second), vec![PlanEntryStatus::Pending])
    );
}

#[test]
fn an_acp_agents_own_subagent_session_refuses_messages() {
    use cookie_agent_protocol::{AcpSessionRecord, AcpSessionStart};
    let session_id = SessionId::new_v7();
    let mut state = SessionState::default();
    let started = |how| EventPayload::AcpSessionStarted {
        record: AcpSessionRecord {
            agent: AgentId::new("claude").unwrap(),
            runtime_id: cookie_agent_protocol::RuntimeId::new("local").unwrap(),
            runtime_agent: AgentId::new("claude").unwrap(),
            acp_session_id: "s-1".into(),
            cwd: "/work".into(),
            protocol_version: 2,
            agent_info: serde_json::Value::Null,
            how,
            steering: true,
            forked_from: None,
            fork: None,
            fork_point: None,
        },
    };
    let at = jiff::Timestamp::new(1, 0).expect("timestamp");
    reduce_event(
        &mut state,
        session_id,
        None,
        1,
        at,
        started(AcpSessionStart::New),
    );
    assert_eq!(state.acp.refusal(), None);
    reduce_event(
        &mut state,
        session_id,
        None,
        2,
        at,
        started(AcpSessionStart::Subagent),
    );
    let refusal = state.acp.refusal().expect("a subagent refuses messages");
    assert!(refusal.contains("`claude`'s own subagent"), "{refusal}");
}

#[test]
fn an_acp_sessions_steering_notices_and_late_tool_presentations_fold_from_its_events() {
    use cookie_agent_protocol::{
        AcpNotice, AcpNoticeSeverity, AcpSessionRecord, AcpSessionStart, AcpSessionUpdate,
        AcpSteered, AcpToolPresentation, BoundedDisplayText, PreparedPresentation, SafeDisplayText,
    };
    let session_id = SessionId::new_v7();
    let run = RunId::new_v7();
    let mut state = SessionState::default();
    let apply = |state: &mut SessionState, seq, payload| {
        reduce_event(
            state,
            session_id,
            Some(run),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    };
    let agent = AgentId::new("claude").unwrap();
    apply(
        &mut state,
        1,
        EventPayload::AcpSessionStarted {
            record: AcpSessionRecord {
                agent: agent.clone(),
                runtime_id: cookie_agent_protocol::RuntimeId::new("local").unwrap(),
                runtime_agent: agent.clone(),
                acp_session_id: "s-1".into(),
                cwd: "/work".into(),
                protocol_version: 1,
                agent_info: serde_json::Value::Null,
                how: AcpSessionStart::New,
                steering: true,
                forked_from: None,
                fork: None,
                fork_point: None,
            },
        },
    );
    assert_eq!(state.acp.steers_into_turn(Some(&agent)), Some(true));
    assert_eq!(
        state
            .acp
            .steers_into_turn(Some(&AgentId::new("primary").unwrap())),
        None,
        "another agent runs the session now"
    );
    apply(
        &mut state,
        2,
        EventPayload::UserInputSubmitted {
            content: "look at notes.txt".into(),
        },
    );
    let version = state.transcript.last().unwrap().version();
    apply(
        &mut state,
        3,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                steered: Some(AcpSteered {
                    user_inputs: vec![2],
                    producer_messages: Vec::new(),
                }),
                ..AcpSessionUpdate::default()
            },
        },
    );
    assert!(state.acp.steered.contains(&2));
    let user = state
        .transcript
        .iter()
        .find(|item| matches!(item, TranscriptItem::User { .. }))
        .unwrap();
    assert_eq!(user.version(), version + 1, "the marked row is drawn again");
    apply(
        &mut state,
        4,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                notice: Some(AcpNotice {
                    severity: AcpNoticeSeverity::Warning,
                    title: "Rate limited".into(),
                    description: Some("Retrying in 1 s.".into()),
                }),
                ..AcpSessionUpdate::default()
            },
        },
    );
    assert!(matches!(
        state.transcript.last(),
        Some(TranscriptItem::AcpNotice { notice, .. })
            if notice.title == "Rate limited" && notice.severity == AcpNoticeSeverity::Warning
    ));
    let call = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: cookie_agent_protocol::ModelCallId::new("call-late").unwrap(),
        provider_item_id: None,
    };
    state.tools.insert(
        call,
        ToolCallState::started(
            call,
            owner,
            base_presentation(PreparedPresentation::new(
                SafeDisplayText::new("Tool").unwrap(),
                None,
            )),
            "{}".into(),
        ),
    );
    let revision = state.tools[&call].revision;
    apply(
        &mut state,
        5,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                tool_presentation: Some(AcpToolPresentation {
                    tool_call_id: call,
                    presentation: PreparedPresentation::new(
                        SafeDisplayText::new("Read notes.txt").unwrap(),
                        Some(BoundedDisplayText::new("/work/notes.txt").unwrap()),
                    ),
                }),
                ..AcpSessionUpdate::default()
            },
        },
    );
    let tool = &state.tools[&call];
    assert_eq!(tool.presentation.title.as_str(), "Read notes.txt");
    assert_eq!(
        tool.presentation
            .primary_argument
            .as_ref()
            .map(|text| text.as_str()),
        Some("/work/notes.txt")
    );
    assert_ne!(tool.revision, revision);
}

fn background_call(job: &str) -> cookie_agent_protocol::BackgroundCall {
    serde_json::from_value(serde_json::json!({
        "job": job,
        "tool": "bash",
        "tool_call_id": ToolCallId::new_v7(),
        "started_at": "2026-10-23T10:00:00Z",
        "detached_at": "2026-10-23T10:00:01Z",
        "by": "user",
    }))
    .expect("background call")
}

fn background_jobs(store: &StateStore, session_id: SessionId) -> Vec<String> {
    store.sessions[&session_id]
        .background_calls
        .iter()
        .map(|call| call.job.clone())
        .collect()
}

/// A `background.changed` that lands while `session.background.list` is on
/// its way is at least as new as the listing, which may have been read
/// before the change: the listing landing after it is dropped.
#[test]
fn a_background_listing_older_than_a_background_change_is_dropped() {
    let session_id = SessionId::new_v7();
    let mut store = StateStore::default();
    assert!(store.apply_event(submitted(session_id, 1, "first")));

    // Nothing changed while the listing was on its way: it is installed.
    let mark = store.background_mark(session_id);
    assert!(store.install_background_listing(session_id, mark, vec![background_call("job-1")]));
    assert_eq!(background_jobs(&store, session_id), ["job-1"]);

    // The listing is asked for, the call ends, and only then does the
    // listing, read before the end, land.
    let mark = store.background_mark(session_id);
    assert_eq!(
        store.apply_delivery(ClientDelivery::BackgroundChanged(
            cookie_agent_protocol::BackgroundChangedNotification {
                session_id,
                calls: Vec::new(),
            }
        )),
        DeliveryOutcome::Applied
    );
    assert!(!store.install_background_listing(session_id, mark, vec![background_call("job-1")]));
    assert!(background_jobs(&store, session_id).is_empty());

    // Another session's change leaves this session's listing alone.
    let mark = store.background_mark(session_id);
    store.apply_delivery(ClientDelivery::BackgroundChanged(
        cookie_agent_protocol::BackgroundChangedNotification {
            session_id: SessionId::new_v7(),
            calls: vec![background_call("elsewhere")],
        },
    ));
    assert!(store.install_background_listing(session_id, mark, vec![background_call("job-2")]));
    assert_eq!(background_jobs(&store, session_id), ["job-2"]);
}

/// The background calls are not in the event log: a full replay keeps the
/// ones a `background.changed` brought while it ran.
#[test]
fn a_rebuilding_replay_keeps_the_background_calls() {
    let session_id = SessionId::new_v7();
    let mut store = StateStore::default();
    assert!(store.apply_event(submitted(session_id, 1, "first")));
    store.apply_delivery(ClientDelivery::BackgroundChanged(
        cookie_agent_protocol::BackgroundChangedNotification {
            session_id,
            calls: vec![background_call("job-1")],
        },
    ));
    let outcomes = replay(
        &mut store,
        session_id,
        true,
        1,
        vec![submitted(session_id, 1, "first")],
    );
    assert_eq!(outcomes.last(), Some(&DeliveryOutcome::Applied));
    assert_eq!(user_texts(&store, session_id), ["first"]);
    assert_eq!(background_jobs(&store, session_id), ["job-1"]);
}

#[test]
fn an_acp_agents_context_use_is_the_sessions_context_across_its_turns() {
    use cookie_agent_protocol::{AcpContextUsage, AcpSessionUpdate};
    let session_id = SessionId::new_v7();
    let run = RunId::new_v7();
    let mut state = SessionState::default();
    let apply = |state: &mut SessionState, seq, payload| {
        reduce_event(
            state,
            session_id,
            Some(run),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    };
    let acp_turn = || {
        let mut turn = committed_turn(1);
        if let EventPayload::ModelTurnCommitted { resolved_model, .. } = &mut turn {
            *resolved_model = serde_json::from_value(serde_json::json!({
                "provider_id": "acp",
                "model_id": "claude",
                "adapter_id": "acp",
                "selection": {"model": "acp/claude", "variant": null},
                "selection_fingerprint": "b".repeat(64)
            }))
            .expect("resolved model");
        }
        turn
    };
    apply(
        &mut state,
        2,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                context: Some(AcpContextUsage {
                    used: 30_000,
                    size: Some(120_000),
                }),
                ..AcpSessionUpdate::default()
            },
        },
    );
    assert_eq!(state.context_tokens, Some(30_000));
    assert_eq!(state.context_window, Some(120_000));
    // The agent's turns report no usage and leave its context standing.
    apply(&mut state, 3, acp_turn());
    assert_eq!(state.context_tokens, Some(30_000));
    assert_eq!(state.context_window, Some(120_000));
    // A model's usage-less turn clears both: the catalog gives its window.
    apply(&mut state, 4, committed_turn(3));
    assert_eq!(state.context_tokens, None);
    assert_eq!(state.context_window, None);
}

#[test]
fn a_question_waits_until_it_is_resolved_or_its_run_ends() {
    use cookie_agent_protocol::{
        ElicitationAction, ElicitationAnsweredBy, ElicitationId, ElicitationRequest,
        ElicitationResolution,
    };

    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let ask = |message: &str| ElicitationRequest {
        elicitation_id: ElicitationId::new_v7(),
        agent: cookie_agent_protocol::AgentId::new("claude").unwrap(),
        tool_call_id: None,
        message: message.to_owned(),
        form: None,
        url: Some("https://example.com/connect".to_owned()),
    };
    let first = ask("Sign in?");
    let second = ask("Again?");
    let events = vec![
        stored_event(
            session,
            Some(run),
            1,
            EventPayload::ElicitationRequested {
                request: first.clone(),
            },
        ),
        stored_event(
            session,
            Some(run),
            2,
            EventPayload::ElicitationRequested {
                request: second.clone(),
            },
        ),
        stored_event(
            session,
            Some(run),
            3,
            EventPayload::ElicitationResolved {
                resolution: ElicitationResolution {
                    elicitation_id: first.elicitation_id,
                    action: ElicitationAction::Decline,
                    content: Default::default(),
                    answered_by: ElicitationAnsweredBy::Unattended,
                    reason: Some("no one is here".to_owned()),
                },
            },
        ),
    ];
    let state = reduce_session_events(session, 0, &events);
    assert_eq!(state.questions.len(), 1);
    assert_eq!(state.questions[0].request, second);
    assert_eq!(state.questions[0].run_id, Some(run));
    assert!(state.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::Event { level: EventLevel::Info, text, .. }
            if text == "claude asks: “Sign in?” declined (no one attends the session): no one is here"
    )));

    let mut events = events;
    events.push(stored_event(
        session,
        Some(run),
        4,
        EventPayload::RunFinished {
            outcome: cookie_agent_protocol::RunOutcome::Cancelled { reason: None }.into(),
        },
    ));
    assert!(
        reduce_session_events(session, 0, &events)
            .questions
            .is_empty()
    );
}

#[test]
fn pickers_show_the_document_id_only_when_it_differs_from_the_name() {
    use crate::document_id;
    // A native model's full key, an ACP option value, a variant ID.
    assert_eq!(document_id("GPT-5", "openai/gpt-5"), Some("openai/gpt-5"));
    assert_eq!(document_id("Opus", "opus"), Some("opus"));
    assert_eq!(document_id("Variant low", "low"), Some("low"));
    // The name is the ID: once.
    assert_eq!(document_id("high", "high"), None);
    assert_eq!(document_id("base", "base"), None);
}

/// A request for `git status` with no window of its own, as a 1.28.0
/// daemon writes it.
fn windowless_approval_request() -> cookie_agent_protocol::ApprovalRequest {
    use cookie_agent_protocol as p;
    let resource = p::PreparedApprovalResource {
        capability: p::PermissionAction::BASH,
        canonical: p::PreparedResourceIdentity::new("command:git-status").unwrap(),
        binding_digest: p::PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
        binding_lifetime: p::PreparedBindingLifetime::ProcessLocal,
        boundary: p::ApprovalBoundary::Exact,
        source: p::ApprovalResourceSource::PrimaryOperation,
    };
    let resource_digest = resource.binding_digest.clone();
    let operation = p::PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"arguments"),
        vec![p::ApprovalCapability {
            action: p::PermissionAction::BASH,
            operation: p::PreparedCapabilityOperation::new("execute").unwrap(),
        }],
        vec![resource],
        Sha256Digest::of_bytes(b"context"),
    )
    .unwrap();
    p::ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        p::ToolCallId::new_v7(),
        ApprovalTrigger::PermissionPolicy,
        operation,
        vec![p::ApprovalEvaluation {
            resource_digest,
            effect: p::PermissionEffect::Ask,
            trace: p::DecisionTrace {
                action: p::PermissionAction::BASH,
                normalized_resource: "git status".into(),
                candidates: Vec::new(),
                effect: p::PermissionEffect::Ask,
                precedence_reason: "test".into(),
            },
        }],
        p::ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .unwrap()
}

/// The user's window opens at the escalation (client protocol 1.28.0): the
/// escalated step's `expires_at` becomes the approval's.
#[test]
fn an_escalation_opens_the_approval_window() {
    let session_id = SessionId::new_v7();
    let request = windowless_approval_request();
    let approval_id = request.approval_id();
    let mut state = SessionState::default();
    reduce_event(
        &mut state,
        session_id,
        None,
        1,
        jiff::Timestamp::now(),
        EventPayload::ApprovalRequested { request },
    );
    let expires_at = jiff::Timestamp::from_second(4_000_000_000).unwrap();
    reduce_event(
        &mut state,
        session_id,
        None,
        2,
        jiff::Timestamp::now(),
        EventPayload::approval_step(
            approval_id,
            cookie_agent_protocol::ApprovalStep::Escalated {
                reason_code: cookie_agent_protocol::ApprovalReasonCode::Escalated,
                expires_at: Some(expires_at),
            },
        ),
    );
    let approval = &state.approvals[0];
    assert!(approval.escalated);
    assert_eq!(approval.constraints.expires_at, Some(expires_at));
    assert!(approval.is_visible_user_escalation());
}

/// The reviewer's reason reaches the approval and, for a call resolved
/// without the user, its info row (approval reviewer spec section 7).
#[test]
fn the_reviewers_reason_reaches_the_approval_and_its_info_row() {
    let session_id = SessionId::new_v7();
    let request = windowless_approval_request();
    let approval_id = request.approval_id();
    let evaluations = request.evaluations().to_vec();
    let mut state = SessionState::default();
    let now = jiff::Timestamp::now();
    reduce_event(
        &mut state,
        session_id,
        None,
        1,
        now,
        EventPayload::ApprovalRequested {
            request: request.clone(),
        },
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        2,
        now,
        EventPayload::approval_step(
            approval_id,
            cookie_agent_protocol::ApprovalStep::Evaluated {
                decision: cookie_agent_protocol::ApprovalInternalDecision {
                    decision: cookie_agent_protocol::ApprovalInternalDecisionKind::Deny,
                    source: cookie_agent_protocol::ApprovalDecisionSource::InternalAgent,
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::InternalAgentDenied,
                    evaluations: evaluations.clone(),
                    rationale: Some("Deletes the user's data.".into()),
                },
            },
        ),
    );
    assert_eq!(
        state.approvals[0].reviewer,
        Some(ReviewerNote::Decided {
            reason: Some("Deletes the user's data.".into())
        })
    );
    reduce_event(
        &mut state,
        session_id,
        None,
        3,
        now,
        EventPayload::approval_finalized(
            approval_id,
            cookie_agent_protocol::ApprovalFinalization::Decided {
                decision: cookie_agent_protocol::ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Rejected,
                    source: cookie_agent_protocol::ApprovalDecisionSource::InternalAgent,
                    reason_code: cookie_agent_protocol::ApprovalReasonCode::InternalAgentDenied,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        ),
    );
    let last = state
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::Event { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("an info row");
    assert!(
        last.ends_with(" · the reviewer: Deletes the user's data."),
        "{last}"
    );

    // An escalated record read from `approval.list` carries the note too,
    // and a failed review says so.
    let record = cookie_agent_protocol::ApprovalRecord {
        session_id,
        request: request.with_expires_at(None),
        status: cookie_agent_protocol::ApprovalStatus::Escalated,
        internal_decision: Some(cookie_agent_protocol::ApprovalInternalDecision {
            decision: cookie_agent_protocol::ApprovalInternalDecisionKind::Escalate,
            source: cookie_agent_protocol::ApprovalDecisionSource::InternalAgent,
            reason_code: cookie_agent_protocol::ApprovalReasonCode::InternalAgentFailed,
            evaluations,
            rationale: None,
        }),
        user_decision: None,
        final_decision: None,
    };
    let approval = approval_state_from_record(record).expect("visible escalation");
    assert_eq!(approval.reviewer, Some(ReviewerNote::Failed));
    assert_eq!(
        approval.reviewer.unwrap().escalation_line().as_deref(),
        Some("The reviewer failed, so the call is yours to decide.")
    );
}

#[test]
fn an_acp_reply_is_headed_by_the_agents_model_and_effort_as_it_started() {
    let session = SessionId::new_v7();
    let acp_model: ResolvedModelRef = serde_json::from_value(serde_json::json!({
        "provider_id": "acp",
        "model_id": "claude",
        "adapter_id": "openai-compatible",
        "selection": {"model": "acp/claude", "variant": null},
        "selection_fingerprint": "b".repeat(64)
    }))
    .expect("resolved model");
    let select = |id: &str, category: &str, current: &str, values: &[(&str, &str)]| {
        cookie_agent_protocol::AcpConfigOption {
            id: id.into(),
            name: id.into(),
            kind: Default::default(),
            description: None,
            category: Some(category.into()),
            current_value: current.into(),
            values: values
                .iter()
                .map(|(value, name)| cookie_agent_protocol::AcpConfigValue {
                    value: (*value).into(),
                    name: (*name).into(),
                    description: None,
                })
                .collect(),
        }
    };
    let options = |model: &str, effort: &str| EventPayload::AcpSessionUpdated {
        update: cookie_agent_protocol::AcpSessionUpdate {
            options: Some(vec![
                select(
                    "model",
                    "model",
                    model,
                    &[("opus", "Opus"), ("sonnet", "Sonnet")],
                ),
                select(
                    "effort",
                    "thought_level",
                    effort,
                    &[("high", "High"), ("low", "Low")],
                ),
            ]),
            ..cookie_agent_protocol::AcpSessionUpdate::default()
        },
    };
    let attempt = |resolved_model: ResolvedModelRef| EventPayload::ModelAttemptStarted {
        attempt_id: AttemptId::new_v7(),
        attempt_ordinal: 1,
        fallback_index: 0,
        retry_ordinal: 0,
        resolved_model,
        prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
    };
    let first = RunId::new_v7();
    let second = RunId::new_v7();
    let third = RunId::new_v7();
    let events = vec![
        stored_event(session, Some(first), 1, options("opus", "high")),
        stored_event(session, Some(first), 2, attempt(acp_model.clone())),
        stored_event(session, None, 3, options("sonnet", "low")),
        stored_event(session, Some(second), 4, attempt(acp_model)),
        stored_event(session, Some(third), 5, attempt(resolved_model())),
    ];
    let state = reduce_session_events(session, 0, &events);
    let labels = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Assistant { attribution, .. } => attribution.acp_model.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Each reply keeps the values its attempt started with; a model run's
    // reply has none.
    assert_eq!(labels, ["Opus • High", "Sonnet • Low"]);
    let header = state
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::Assistant { attribution, .. } => {
                Some(attribution.header(&ModelDisplayNames::default()))
            }
            _ => None,
        })
        .unwrap();
    assert!(header.ends_with(" • Opus • High"), "{header}");
}

#[test]
fn an_acp_agents_compaction_and_session_messages_show_as_rows_the_run_does_not_straddle() {
    use cookie_agent_protocol::{
        AcpCompaction, AcpCompactionState, AcpMessageDirection, AcpSessionMessage, AcpSessionUpdate,
    };
    let session_id = SessionId::new_v7();
    let run = RunId::new_v7();
    let mut state = SessionState::default();
    let apply = |state: &mut SessionState, seq, payload| {
        reduce_event(
            state,
            session_id,
            Some(run),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    };
    let attempt = || EventPayload::ModelAttemptStarted {
        attempt_id: AttemptId::new_v7(),
        attempt_ordinal: 1,
        fallback_index: 0,
        retry_ordinal: 0,
        resolved_model: resolved_model(),
        prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
    };
    let compaction = |state| EventPayload::AcpSessionUpdated {
        update: AcpSessionUpdate {
            compaction: Some(AcpCompaction {
                compaction_id: "c1".into(),
                state,
                summary: (state == AcpCompactionState::Completed)
                    .then(|| "We read notes.txt.".into()),
            }),
            ..AcpSessionUpdate::default()
        },
    };
    apply(&mut state, 1, attempt());
    let item_id = state.transcript[0].id();
    append_assistant_delta(
        &mut state,
        item_id,
        1,
        "reading".into(),
        AssistantPartKind::Text,
        jiff::Timestamp::now(),
    );
    apply(&mut state, 2, compaction(AcpCompactionState::InProgress));
    assert!(matches!(
        state.transcript.last(),
        Some(TranscriptItem::Compacting {
            source: CompactingSource::Agent { compaction_id },
            ..
        }) if compaction_id == "c1"
    ));
    apply(&mut state, 3, compaction(AcpCompactionState::Completed));
    apply(
        &mut state,
        4,
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                session_message: Some(AcpSessionMessage {
                    direction: AcpMessageDirection::Sent,
                    peer: Some("reviewer".into()),
                    text: "check the tests".into(),
                }),
                ..AcpSessionUpdate::default()
            },
        },
    );
    // What the run shows next opens below them.
    apply(&mut state, 5, attempt());
    let rows = state
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::Assistant { .. } => "assistant".to_owned(),
            TranscriptItem::AgentCompaction { summary, .. } => {
                format!("compaction: {}", summary.as_deref().unwrap_or_default())
            }
            TranscriptItem::AgentSessionMessage { message, .. } => format!(
                "{}: {}",
                crate::rows::agent_session_message_summary(message),
                message.text
            ),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            "assistant",
            "compaction: We read notes.txt.",
            "message to subagent reviewer: check the tests",
            "assistant",
        ]
    );
}

#[test]
fn an_acp_agents_ended_background_command_shows_as_a_row_with_how_it_ran() {
    use cookie_agent_protocol::{AcpBackgroundEnded, AcpSessionUpdate};
    let at = |seconds| jiff::Timestamp::new(seconds, 0).expect("timestamp");
    let ended = AcpBackgroundEnded {
        tool_call_id: ToolCallId::new_v7(),
        tool: "Bash".into(),
        display: Some("cargo   test\n --workspace".into()),
        started_at: at(48),
        exit_code: None,
        signal: None,
    };
    let mut state = SessionState::default();
    // Between runs: no run records it.
    reduce_event(
        &mut state,
        SessionId::new_v7(),
        None,
        7,
        at(300),
        EventPayload::AcpSessionUpdated {
            update: AcpSessionUpdate {
                background_ended: Some(ended.clone()),
                ..AcpSessionUpdate::default()
            },
        },
    );
    let [
        TranscriptItem::AgentBackgroundEnded {
            seq,
            ended_at,
            ended: shown,
            ..
        },
    ] = state.transcript.as_slice()
    else {
        panic!("{:?}", state.transcript);
    };
    assert_eq!((*seq, *ended_at, shown), (7, at(300), &ended));
    assert_eq!(
        crate::rows::agent_background_ended_summary(shown),
        "Background: cargo test --workspace ended"
    );
    assert_eq!(
        crate::rows::agent_background_ended_details(shown, *ended_at),
        "the agent's Bash call · ran in the background for 4m 12s · no exit code reported"
    );
    let details = |exit_code, signal: Option<&str>, ended_at| {
        crate::rows::agent_background_ended_details(
            &AcpBackgroundEnded {
                display: None,
                exit_code,
                signal: signal.map(str::to_owned),
                ..ended.clone()
            },
            at(ended_at),
        )
    };
    assert_eq!(
        details(Some(2), None, 50),
        "the agent's Bash call · ran in the background for 2s · exit code 2"
    );
    assert_eq!(
        details(None, Some("SIGTERM"), 48 + 3 * 3600 + 5 * 60 + 9),
        "the agent's Bash call · ran in the background for 3h 5m · ended by SIGTERM"
    );
    // Without a command, the row names its tool.
    assert_eq!(
        crate::rows::agent_background_ended_summary(&AcpBackgroundEnded {
            display: Some("  ".into()),
            ..ended
        }),
        "Background: Bash ended"
    );
}

#[test]
fn an_assistant_block_ends_at_the_row_that_split_it_or_at_its_runs_end() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let attempt_id = AttemptId::new_v7();
    let event = |seq, payload| stored_event(session, Some(run), seq, payload);
    let attempt = |seq, attempt_id| {
        event(
            seq,
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model(),
                prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
            },
        )
    };
    let committed = |seq, attempt_id, model_turn_seq, text: &str| {
        let EventPayload::ModelTurnCommitted {
            resolved_model,
            input_through_seq,
            mut turn,
            warnings,
            ..
        } = committed_turn(seq - 1)
        else {
            unreachable!()
        };
        turn.content = vec![cookie_agent_protocol::PersistedAssistantPart::Text {
            text: text.into(),
            metadata: None,
        }];
        event(
            seq,
            EventPayload::ModelTurnCommitted {
                attempt_id,
                model_turn_seq,
                resolved_model,
                input_through_seq,
                turn,
                warnings,
            },
        )
    };
    let second = AttemptId::new_v7();
    let mut events = vec![
        attempt(1, attempt_id),
        committed(2, attempt_id, 1, "one"),
        // A row between the turns: the next turn opens a block below it,
        // and the block above ended with the row.
        event(
            4,
            EventPayload::MemoryChanged {
                change: cookie_agent_protocol::MemoryChangeKind::Edited,
                url: "user://preferences".into(),
            },
        ),
        attempt(6, second),
        committed(7, second, 2, "two"),
    ];
    let blocks = |state: &SessionState| {
        state
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Assistant { .. }))
            .map(TranscriptItem::id)
            .collect::<Vec<_>>()
    };
    let state = reduce_session_events(session, 0, &events);
    let [above, below] = blocks(&state)[..] else {
        panic!("two blocks: {:?}", state.transcript);
    };
    assert_eq!(
        state.assistant_end(above),
        Some(AssistantEnd {
            at: jiff::Timestamp::new(4, 0).unwrap(),
            stopped: false
        })
    );
    assert!(!state.assistant_live(above));
    assert_eq!(state.assistant_end(below), None);
    assert!(state.assistant_live(below));
    let now = jiff::Timestamp::new(16, 0).unwrap();
    let summary = crate::transcript_mode::work_summary(&state, above, &[], now);
    assert_eq!(
        (summary.live, summary.elapsed),
        (false, Duration::from_secs(3))
    );
    let summary = crate::transcript_mode::work_summary(&state, below, &[], now);
    assert_eq!(
        (summary.live, summary.elapsed),
        (true, Duration::from_secs(10))
    );

    events.push(event(
        9,
        EventPayload::RunFinished {
            outcome: cookie_agent_protocol::RunOutcome::Cancelled { reason: None }.into(),
        },
    ));
    let state = reduce_session_events(session, 0, &events);
    assert_eq!(blocks(&state), [above, below]);
    assert_eq!(
        state.assistant_end(above).map(|end| end.stopped),
        Some(false)
    );
    assert_eq!(
        state.assistant_end(below),
        Some(AssistantEnd {
            at: jiff::Timestamp::new(9, 0).unwrap(),
            stopped: true
        })
    );
    let summary = crate::transcript_mode::work_summary(&state, below, &[], now);
    assert_eq!(summary.label(), "Stopped after 3s");
}

#[test]
fn an_acp_turn_that_said_nothing_leaves_no_empty_reply() {
    let session = SessionId::new_v7();
    let acp_model: ResolvedModelRef = serde_json::from_value(serde_json::json!({
        "provider_id": "acp",
        "model_id": "codex",
        "adapter_id": "acp",
        "selection": {"model": "acp/codex", "variant": null},
        "selection_fingerprint": "b".repeat(64)
    }))
    .expect("resolved model");
    let turn = |resolved_model: ResolvedModelRef| {
        let attempt_id = AttemptId::new_v7();
        let started = EventPayload::ModelAttemptStarted {
            attempt_id,
            attempt_ordinal: 1,
            fallback_index: 0,
            retry_ordinal: 0,
            resolved_model: resolved_model.clone(),
            prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
        };
        let mut committed = committed_turn(1);
        if let EventPayload::ModelTurnCommitted {
            attempt_id: id,
            resolved_model: model,
            ..
        } = &mut committed
        {
            *id = attempt_id;
            *model = resolved_model;
        }
        [started, committed]
    };
    let assistants = |model: ResolvedModelRef| {
        let run = RunId::new_v7();
        let events = turn(model)
            .into_iter()
            .enumerate()
            .map(|(index, payload)| stored_event(session, Some(run), index as u64 + 1, payload))
            .collect::<Vec<_>>();
        reduce_session_events(session, 0, &events)
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Assistant { .. }))
            .count()
    };
    assert_eq!(assistants(acp_model), 0, "the agent's compact turn");
    assert_eq!(
        assistants(resolved_model()),
        1,
        "a model's empty turn still shows"
    );
}

#[test]
fn an_acp_subagent_shows_what_it_inherits_and_no_system_prompt() {
    use cookie_agent_protocol::{AcpSessionRecord, AcpSessionStart};
    let session = SessionId::new_v7();
    let started = EventPayload::AcpSessionStarted {
        record: AcpSessionRecord {
            agent: AgentId::new("codex").unwrap(),
            runtime_id: cookie_agent_protocol::RuntimeId::new("local").unwrap(),
            runtime_agent: AgentId::new("codex").unwrap(),
            acp_session_id: "s-1".into(),
            cwd: "/work".into(),
            protocol_version: 2,
            agent_info: serde_json::Value::Null,
            how: AcpSessionStart::Subagent,
            steering: true,
            forked_from: None,
            fork: None,
            fork_point: None,
        },
    };
    let inherited = EventPayload::AcpSessionUpdated {
        update: cookie_agent_protocol::AcpSessionUpdate {
            prompt_context: Some(vec!["<tool_instructions>".into(), "You review.".into()]),
            ..cookie_agent_protocol::AcpSessionUpdate::default()
        },
    };
    let events = vec![
        stored_event(session, None, 1, started),
        stored_event(session, None, 2, inherited),
    ];
    let replayed = reduce_session_events(session, 0, &events);
    assert!(!crate::rows::shows_system_prompt(&replayed));
    assert_eq!(
        model_inputs(&replayed),
        [(
            crate::model_input::ModelInputKind::InheritedContext,
            "context from the parent · its system prompt · 3 lines",
            "<tool_instructions>\n\nYou review."
        )]
    );
}

#[test]
fn an_agents_question_splits_its_work_and_its_row_says_the_answer() {
    use cookie_agent_protocol::{
        ElicitationAction, ElicitationAnsweredBy, ElicitationId, ElicitationRequest,
        ElicitationResolution, ElicitationValue,
    };
    let session_id = SessionId::new_v7();
    let run = RunId::new_v7();
    let mut state = SessionState::default();
    let apply = |state: &mut SessionState, seq, payload| {
        reduce_event(
            state,
            session_id,
            Some(run),
            seq,
            jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
            payload,
        );
    };
    let attempt = || EventPayload::ModelAttemptStarted {
        attempt_id: AttemptId::new_v7(),
        attempt_ordinal: 1,
        fallback_index: 0,
        retry_ordinal: 0,
        resolved_model: resolved_model(),
        prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(b"prompt"),
    };
    let request: ElicitationRequest = serde_json::from_value(serde_json::json!({
        "elicitation_id": ElicitationId::new_v7(),
        "agent": "claude",
        "message": "Which plan?",
        "form": {"fields": [{"name": "plan", "title": "Plan", "required": true,
                             "input": {"type": "text"}}]},
    }))
    .expect("request");
    apply(&mut state, 1, attempt());
    let item_id = state.transcript[0].id();
    append_assistant_delta(
        &mut state,
        item_id,
        1,
        "reading".into(),
        AssistantPartKind::Text,
        jiff::Timestamp::now(),
    );
    apply(
        &mut state,
        2,
        EventPayload::ElicitationRequested {
            request: request.clone(),
        },
    );
    let question = |state: &SessionState| {
        state.transcript.iter().find_map(|item| match item {
            TranscriptItem::AcpNotice {
                version, notice, ..
            } => Some((*version, notice.title.clone(), notice.description.clone())),
            _ => None,
        })
    };
    assert_eq!(
        question(&state),
        Some((
            0,
            "claude asks: “Which plan?”".to_owned(),
            Some("waiting for your answer".to_owned())
        ))
    );
    apply(
        &mut state,
        3,
        EventPayload::ElicitationResolved {
            resolution: ElicitationResolution {
                elicitation_id: request.elicitation_id,
                action: ElicitationAction::Accept,
                content: [("plan".to_owned(), ElicitationValue::Text("safe".into()))]
                    .into_iter()
                    .collect(),
                answered_by: ElicitationAnsweredBy::User,
                reason: None,
            },
        },
    );
    assert_eq!(
        question(&state).map(|(version, _, description)| (version, description)),
        Some((1, Some("accepted\nPlan: safe".to_owned())))
    );
    // What the agent does after it opens below the question.
    apply(&mut state, 4, attempt());
    let rows = state
        .transcript
        .iter()
        .filter(|item| !matches!(item, TranscriptItem::Event { .. }))
        .map(|item| match item {
            TranscriptItem::Assistant { .. } => "assistant",
            TranscriptItem::AcpNotice { .. } => "question",
            _ => "other",
        })
        .collect::<Vec<_>>();
    assert_eq!(rows, ["assistant", "question", "assistant"]);
}

/// What the busy counts stand for, scanned from the projection.
fn busy_scanned(state: &SessionState) -> (bool, bool, usize) {
    (
        state
            .tools
            .values()
            .any(|tool| tool.status == ToolStatus::Running),
        state
            .transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::Compacting { .. })),
        state
            .transcript
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    TranscriptItem::ProducerMessage {
                        status: ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted,
                        ..
                    }
                )
            })
            .count(),
    )
}

fn busy_counted(state: &SessionState) -> (bool, bool, usize) {
    (
        state.has_running_tool(),
        state.is_compacting(),
        state.waiting_producers(),
    )
}

#[test]
fn a_sessions_busy_counts_follow_its_calls_compactions_and_producer_messages() {
    let session = SessionId::new_v7();
    let call = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: cookie_agent_protocol::ModelCallId::new("call").unwrap(),
        provider_item_id: None,
    };
    let message_id = ProducerMessageId::new_v7();
    let producer = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("ci").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
    let mut state = SessionState::default();
    let mut seq = 0;
    let mut durable = |state: &mut SessionState, payload: EventPayload| {
        seq += 1;
        let event = stored_event(session, None, seq, payload);
        reduce_event(
            state,
            session,
            event.run_id,
            event.seq,
            event.timestamp,
            event.payload,
        );
    };
    durable(
        &mut state,
        EventPayload::ToolCallStarted {
            start: cookie_agent_protocol::ToolCallStart {
                background: false,
                tool_call_id: call,
                owner: owner.clone(),
                output: Default::default(),
                presentation: base_presentation(cookie_agent_protocol::PreparedPresentation::new(
                    cookie_agent_protocol::SafeDisplayText::new("bash").unwrap(),
                    None,
                )),
                operation_fingerprint: serde_json::from_value(
                    serde_json::json!({"digest": "1".repeat(64)}),
                )
                .unwrap(),
            },
        },
    );
    assert_eq!(busy_counted(&state), (true, false, 0));
    reduce_transient(
        &mut state,
        jiff::Timestamp::now(),
        EventPayload::NativeCompactionStarted {
            resolved_model: resolved_model(),
        },
    );
    assert_eq!(busy_counted(&state), (true, true, 0));
    durable(
        &mut state,
        accepted_message(message_id, producer.clone(), "build-1", None),
    );
    assert_eq!(busy_counted(&state), (true, true, 1));
    durable(
        &mut state,
        EventPayload::ToolCallTerminated {
            termination: cookie_agent_protocol::ToolCallTermination {
                tool_call_id: call,
                owner,
                outcome: ToolTerminationOutcome::Completed,
                result: None,
                error: None,
            },
        },
    );
    assert_eq!(busy_counted(&state), (false, true, 1));
    reduce_transient(
        &mut state,
        jiff::Timestamp::now(),
        EventPayload::NativeCompactionFinished {
            resolved_model: resolved_model(),
        },
    );
    assert_eq!(busy_counted(&state), (false, false, 1));
    durable(&mut state, discarded_message(message_id, producer, None));
    assert_eq!(busy_counted(&state), busy_scanned(&state));
    assert_eq!(busy_counted(&state), (false, false, 0));
}

#[test]
fn a_replayed_sessions_busy_counts_match_its_projection() {
    // The producer queue's own scenario, replayed whole: the counts a
    // rebuild ends with are the projection's.
    let session_id = SessionId::new_v7();
    let run = RunId::new_v7();
    let owner = ProducerOwner::external(
        cookie_agent_protocol::ProducerSource::plugin("build-monitor").unwrap(),
        cookie_agent_protocol::ProducerName::default_name(),
    );
    let first = ProducerMessageId::new_v7();
    let second = ProducerMessageId::new_v7();
    let events = [
        stored_event(
            session_id,
            None,
            1,
            accepted_message(first, owner.clone(), "one", None),
        ),
        stored_event(
            session_id,
            None,
            2,
            accepted_message(second, owner.clone(), "two", None),
        ),
        stored_event(
            session_id,
            Some(run),
            3,
            EventPayload::ProducerMessageAdmitted { message_id: first },
        ),
    ];
    let state = reduce_session_events(session_id, 0, &events);
    assert_eq!(busy_counted(&state), busy_scanned(&state));
    assert_eq!(state.waiting_producers(), 2);
    // A projection edited by hand is counted again on request.
    let mut edited = state.clone();
    edited.transcript.truncate(1);
    edited.recount_busy();
    assert_eq!(busy_counted(&edited), busy_scanned(&edited));
    assert_eq!(edited.waiting_producers(), 1);
}

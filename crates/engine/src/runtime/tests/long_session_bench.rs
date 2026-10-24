//! Per-step costs of the session actor on long session logs.
//!
//! Ignored: a benchmark, not a check. Run it with
//! `TMPDIR=/dev/shm cargo test --release -p cookie_agent_engine --lib --
//! --ignored --nocapture long_session_bench`; on a disk the fsyncs of the
//! records that matter for recovery swamp what it measures.
//! `COOKIE_BENCH_EVENTS=20000,50000` picks the log sizes.
//!
//! It grows a session to each size with a realistic mix of runs through
//! the actor, then times what a model step makes the actor do with an
//! active run on that log, and the whole-log passes one by one.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use cookie_agent_protocol::{
    AgentId, AttemptId, ClientRunId, EventPayload, ModelCallId, ModelFinishReason,
    PersistedAssistantPart, PersistedModelTurn, ProducerDeliveryMode, ProducerIdempotencyKey,
    ProducerMessageId, ProducerOwner, RunId, RunStartParams, SafeCode, SafeDisplayText, SessionId,
    Sha256Digest, StoredEvent, ToolCallId, ToolTerminationOutcome, Usage,
};

use crate::Engine;

use super::super::SessionCommand;
use super::support::*;

/// What a synthetic run copies from the session's real first run.
struct Template {
    run_started: EventPayload,
    resolved_model: cookie_agent_protocol::ResolvedModelRef,
    agent_id: AgentId,
    prompt_fingerprint: Sha256Digest,
}

struct Grower<'a> {
    engine: &'a Engine,
    session: SessionId,
    template: Template,
    next_turn_seq: u64,
    runs: u64,
    appended: u64,
    tail: Vec<Duration>,
    /// Append time and count per event kind since the last reset.
    by_kind: std::collections::HashMap<String, (Duration, u32)>,
}

fn origin() -> cookie_agent_protocol::EventOrigin {
    cookie_agent_protocol::EventOrigin::new("engine:bench").expect("bench origin")
}

impl Grower<'_> {
    async fn append(&mut self, run: Option<RunId>, payload: EventPayload) {
        let kind = format!("{payload:?}")
            .split([' ', '{', '('])
            .next()
            .unwrap_or_default()
            .to_owned();
        let started = Instant::now();
        self.engine
            .append(self.session, run, origin(), payload)
            .await
            .expect("bench append");
        let elapsed = started.elapsed();
        self.appended += 1;
        self.tail.push(elapsed);
        if self.tail.len() > 1_000 {
            self.tail.remove(0);
        }
        let entry = self.by_kind.entry(kind).or_default();
        entry.0 += elapsed;
        entry.1 += 1;
    }

    fn last_seq(&self) -> u64 {
        self.engine
            .inner
            .store
            .log(self.session)
            .expect("bench log")
            .last_event()
            .map_or(0, |event| event.seq)
    }

    /// Appends one model step of `run`: an attempt, its streamed parts, a
    /// committed turn with two tool calls, usage, and both calls' results.
    /// `started` is an attempt already started and prepared, which the step
    /// commits instead of starting its own.
    async fn step(
        &mut self,
        run: RunId,
        attempt_ordinal: u32,
        input_through_seq: u64,
        started: Option<AttemptId>,
    ) {
        let attempt_id = started.unwrap_or_else(AttemptId::new_v7);
        if started.is_none() {
            self.append(
                Some(run),
                EventPayload::ModelAttemptStarted {
                    attempt_id,
                    attempt_ordinal,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model: self.template.resolved_model.clone(),
                    prompt_fingerprint: self.template.prompt_fingerprint.clone(),
                },
            )
            .await;
            self.append(
                Some(run),
                EventPayload::ModelRequestPrepared {
                    attempt_id,
                    prompt_fingerprint: self.template.prompt_fingerprint.clone(),
                },
            )
            .await;
        }
        for kind in [
            cookie_agent_protocol::StreamedOutputKind::Reasoning,
            cookie_agent_protocol::StreamedOutputKind::Text,
        ] {
            self.append(
                Some(run),
                EventPayload::ModelOutputStarted { attempt_id, kind },
            )
            .await;
        }
        let turn_seq = self.next_turn_seq;
        self.next_turn_seq += 1;
        let owners = (0..2_u32)
            .map(|index| cookie_agent_protocol::AssistantToolCallRef {
                model_turn_seq: turn_seq,
                content_index: index + 1,
                model_call_id: ModelCallId::new(format!("call-{turn_seq}-{index}"))
                    .expect("model call id"),
                provider_item_id: None,
            })
            .collect::<Vec<_>>();
        let mut content = vec![PersistedAssistantPart::Text {
            text: "I will read the file and then run the tests. ".into(),
            metadata: None,
        }];
        content.extend(owners.iter().map(|owner| PersistedAssistantPart::ToolCall {
            id: owner.model_call_id.clone(),
            provider_item_id: None,
            name: SafeCode::new("read").expect("tool name"),
            input: serde_json::json!({"path": "src/lib.rs", "limit": 40}),
            raw_input: None,
            metadata: None,
        }));
        let usage = Usage {
            input_tokens: Some(2_000),
            output_tokens: Some(60),
            ..Usage::default()
        };
        self.append(
            Some(run),
            EventPayload::ModelTurnCommitted {
                attempt_id,
                model_turn_seq: turn_seq,
                resolved_model: self.template.resolved_model.clone(),
                input_through_seq,
                turn: PersistedModelTurn {
                    content,
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::ToolCalls,
                    usage: usage.clone(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        )
        .await;
        self.append(
            Some(run),
            EventPayload::ModelUsageRecorded {
                model_turn_seq: turn_seq,
                agent_id: self.template.agent_id.clone(),
                resolved_model: self.template.resolved_model.clone(),
                usage,
                estimated_cost_pico_usd: Some(1_000),
            },
        )
        .await;
        for owner in owners {
            let tool_call_id = ToolCallId::new_v7();
            self.append(
                Some(run),
                EventPayload::ToolCallStarted {
                    start: cookie_agent_protocol::ToolCallStart {
                        background: false,
                        output: Default::default(),
                        tool_call_id,
                        owner: owner.clone(),
                        presentation: crate::test_support::base_presentation(
                            cookie_agent_protocol::PreparedPresentation::new(
                                SafeDisplayText::new("Read").expect("title"),
                                None,
                            ),
                        ),
                        operation_fingerprint: serde_json::from_value(serde_json::json!({
                            "digest": Sha256Digest::of_bytes(tool_call_id.to_string().as_bytes())
                        }))
                        .expect("operation fingerprint"),
                    },
                },
            )
            .await;
            self.append(
                Some(run),
                EventPayload::ToolCallTerminated {
                    termination: cookie_agent_protocol::ToolCallTermination {
                        tool_call_id,
                        owner,
                        outcome: ToolTerminationOutcome::Completed,
                        result: Some(cookie_agent_protocol::PersistedToolResult {
                            title: SafeDisplayText::new("Read").expect("title"),
                            output: "    let value = compute(input)?;\n".repeat(4),
                            display: Vec::new(),
                            background: None,
                            retained_output: None,
                            metadata: serde_json::Value::Null,
                            attachments: Vec::new(),
                            additional_messages: Vec::new(),
                        }),
                        error: None,
                    },
                },
            )
            .await;
        }
    }

    /// Appends one finished run: a queued user input, every fourth run a
    /// subagent's steer message arriving while it runs, `steps` tool steps,
    /// and a final answer.
    async fn run(&mut self, steps: u32) {
        let run = RunId::new_v7();
        let message = self.runs.is_multiple_of(4).then(ProducerMessageId::new_v7);
        self.append(
            None,
            EventPayload::UserInputAdmitted {
                content: "Please keep going with the refactor.".into(),
            },
        )
        .await;
        self.append(Some(run), self.template.run_started.clone())
            .await;
        self.append(
            Some(run),
            EventPayload::UserInputSubmitted {
                content: "Please keep going with the refactor.".into(),
            },
        )
        .await;
        let submitted = self.last_seq();
        self.append(
            Some(run),
            EventPayload::UserInputApplied {
                user_input_seq: submitted,
            },
        )
        .await;
        if let Some(message_id) = message {
            self.append(
                None,
                EventPayload::ProducerMessageAccepted {
                    message_id,
                    producer_owner: ProducerOwner::Subagent {
                        session_id: SessionId::new_v7(),
                    },
                    mode: ProducerDeliveryMode::Steer,
                    idempotency_key: ProducerIdempotencyKey::new(message_id.to_string())
                        .expect("idempotency key"),
                    description: SafeDisplayText::new("Subagent: report").expect("description"),
                    body: "<subagent_report>done</subagent_report>".into(),
                    reminder: None,
                    agent_hop: None,
                    retained_output: None,
                },
            )
            .await;
            self.append(
                Some(run),
                EventPayload::ProducerMessageAdmitted { message_id },
            )
            .await;
            self.append(
                Some(run),
                EventPayload::ProducerMessagesClaimed {
                    message_ids: vec![message_id],
                },
            )
            .await;
        }
        for step in 0..steps {
            let through = self.last_seq();
            self.step(run, step + 1, through, None).await;
        }
        self.append(
            Some(run),
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Completed {
                final_text: Some("Done.".into()),
            }),
        )
        .await;
        self.runs += 1;
    }
}

/// A model server that answers its first request with a final answer and
/// holds every later one open, so a run stays active mid-attempt.
async fn first_answer_then_hold_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bench listener");
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        let mut held = Vec::new();
        let mut first = true;
        while let Ok((mut socket, _)) = listener.accept().await {
            let _ = read_scripted_http_request(&mut socket).await;
            if std::mem::take(&mut first) {
                write_scripted_sse(&mut socket, &scripted_text_body("ready")).await;
            } else {
                held.push(socket);
            }
        }
    });
    format!("http://{address}/v1")
}

fn mean(samples: &[Duration]) -> Duration {
    samples.iter().sum::<Duration>() / u32::try_from(samples.len().max(1)).expect("sample count")
}

fn time<T>(iterations: u32, mut work: impl FnMut() -> T) -> Duration {
    let started = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(work());
    }
    started.elapsed() / iterations
}

async fn bench(target: u64) {
    crate::session::skip_reference_fold();
    let endpoint = first_answer_then_hold_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine
        .create_session(selection.clone())
        .expect("session")
        .session_id;
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new("bench-first").expect("run ID"),
                selection: selection.clone(),
                content: "start".to_owned().into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first run");
    wait_for_session_not_running(engine, session).await;
    let events = engine
        .inner
        .store
        .log(session)
        .expect("log")
        .event_snapshot();
    let template = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::RunStarted { agent, .. } => Some(Template {
                run_started: event.payload.clone(),
                resolved_model: agent.selected_suffix()[0].resolved.clone(),
                agent_id: agent.agent.clone(),
                prompt_fingerprint: agent.prompt_fingerprint.clone(),
            }),
            _ => None,
        })
        .expect("run started");
    let next_turn_seq = events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelTurnCommitted { model_turn_seq, .. } => Some(model_turn_seq),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 1;
    drop(events);
    let mut grower = Grower {
        engine,
        session,
        template,
        next_turn_seq,
        runs: 0,
        appended: 0,
        tail: Vec::new(),
        by_kind: std::collections::HashMap::new(),
    };
    let growth = Instant::now();
    while grower.last_seq() < target {
        if grower.last_seq() + 2_000 >= target {
            grower.by_kind.clear();
        }
        grower.run(6).await;
    }
    let growth = growth.elapsed();
    let mut kinds = grower
        .by_kind
        .iter()
        .map(|(kind, (total, count))| (kind.clone(), *total / (*count).max(1), *count))
        .collect::<Vec<_>>();
    kinds.sort_by_key(|(_, mean, _)| std::cmp::Reverse(*mean));
    println!("[{target}] append mean by kind near the tail: {kinds:?}");
    println!(
        "[{target}] grew to {} events through the actor in {growth:?} \
         ({:?}/append); last 1000 appends mean {:?}",
        grower.last_seq(),
        growth / u32::try_from(grower.appended).expect("append count"),
        mean(&grower.tail),
    );

    // Run start on the long log, through to the model request.
    let started = Instant::now();
    let run = engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new("bench-active").expect("run ID"),
                selection,
                content: "continue".to_owned().into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("active run")
        .run_id;
    let admitted = started.elapsed();
    let prepared = |event: &StoredEvent| match event.payload {
        EventPayload::ModelRequestPrepared { attempt_id, .. } if event.run_id == Some(run) => {
            Some(attempt_id)
        }
        _ => None,
    };
    let mut real_attempt = loop {
        let log = engine.inner.store.log(session).expect("log");
        if let Some(attempt) = log
            .event_snapshot()
            .iter()
            .rev()
            .take(64)
            .find_map(|event| prepared(event))
        {
            break Some(attempt);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    println!(
        "[{target}] start_run admitted in {admitted:?}; first request prepared after {:?}",
        started.elapsed()
    );

    // Model steps against the active run: the appends a step makes, then
    // the actor work that prepares the next request.
    const STEPS: u32 = 40;
    let mut appends = Vec::new();
    let mut promote_pending = Vec::new();
    let mut promote_or_complete = Vec::new();
    for step in 0..STEPS {
        grower.tail.clear();
        let through = grower.last_seq();
        grower
            .step(run, step + 1, through, real_attempt.take())
            .await;
        appends.push(grower.tail.iter().sum::<Duration>());
        let started = Instant::now();
        let claimed = engine
            .request(session, |reply| SessionCommand::PromotePendingInputs {
                run,
                reply,
            })
            .await
            .expect("promote pending inputs");
        promote_pending.push(started.elapsed());
        drop(claimed);
        let started = Instant::now();
        engine
            .request(session, |reply| SessionCommand::PromotePendingOrComplete {
                run,
                final_text: None,
                complete_if_empty: false,
                reply,
            })
            .await
            .expect("promote pending or complete");
        promote_or_complete.push(started.elapsed());
    }
    println!(
        "[{target}] per step: its appends {:?}; PromotePendingInputs {:?}; \
         PromotePendingOrComplete {:?}",
        mean(&appends),
        mean(&promote_pending),
        mean(&promote_or_complete),
    );

    // The whole-log passes one by one.
    let snapshot = engine
        .inner
        .store
        .log(session)
        .expect("log")
        .event_snapshot();
    let goal_fold = time(20, || {
        crate::goal_projection::GoalProducerProjection::from_events(&snapshot)
    });
    let model_state = time(20, || {
        cookie_agent_protocol::SessionModelState::from_events(&snapshot)
    });
    let pending = time(20, || super::super::mailbox::pending_inputs(&snapshot, run));
    let runless = time(20, || {
        super::super::residency::has_runless_pending_inputs(&snapshot)
    });
    let parent_reconcile = time(20, || {
        engine.inner.delegation_events.reconcile_parent(session)
    });
    let scan = time(20, || {
        snapshot
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ToolCallTerminated { .. }))
            .count()
    });
    let deep_copy = time(5, || engine.inner.store.log(session).expect("log").events());
    println!(
        "[{target}] passes over {} events: goal/producer fold {goal_fold:?}; \
         SessionModelState fold {model_state:?}; pending_inputs {pending:?}; \
         has_runless_pending_inputs {runless:?}; delegation reconcile_parent \
         {parent_reconcile:?}; bare match scan {scan:?}; deep copy (events()) {deep_copy:?}",
        snapshot.len(),
    );
    drop(snapshot);
    fixture.engine.shutdown().await;
}

#[tokio::test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
async fn long_session_bench() {
    let targets = std::env::var("COOKIE_BENCH_EVENTS")
        .unwrap_or_else(|_| "20000,100000".into())
        .split(',')
        .map(|target| target.trim().parse::<u64>().expect("event count"))
        .collect::<Vec<_>>();
    for target in targets {
        bench(target).await;
    }
}

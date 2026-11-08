//! The long native scenario of `docs/site/specs/forever-sessions.md` section
//! 5.3: what a request assembles from a log's model context is what it
//! assembles from the whole log, at every record, and a log that let go of
//! everything else, each time it was asked, would still hold that.

use std::sync::Arc;

use crate::events::{model_context::model_context, state::LogState};

use super::*;

/// A native session's log, written a record at a time.
struct Scenario {
    events: Vec<StoredEvent>,
    run: RunId,
    model_turn_seq: u64,
    /// What a log that lets go of what its model's context does not need
    /// would hold now.
    held: Vec<Arc<StoredEvent>>,
    store: Arc<crate::ArtifactRouter>,
    _directory: tempfile::TempDir,
    /// The model the session started on, which compacts natively, and the
    /// one it switches to.
    native: FrozenModelBinding,
    other: FrozenModelBinding,
    window: cookie_agent_protocol::NativeContextWindow,
    /// What each check found the model context let go of, at most.
    let_go: usize,
}

impl Scenario {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let store =
            crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
        let (native, window) = native_binding_and_window(crate::test_support::model_binding());
        Self {
            events: Vec::new(),
            run: RunId::new_v7(),
            model_turn_seq: 0,
            held: Vec::new(),
            store,
            _directory: directory,
            native,
            other: crate::test_support::model_binding_named("fallback-one"),
            window,
            let_go: 0,
        }
    }

    fn tip(&self) -> u64 {
        self.events.last().map_or(0, |event| event.seq)
    }

    /// Appends `payload` to the current run, or outside any, and checks.
    fn push(&mut self, run: Option<RunId>, payload: EventPayload) -> u64 {
        let seq = self.tip() + 1;
        let mut event = event(seq, self.run, payload);
        event.run_id = run;
        self.events.push(event);
        self.check();
        seq
    }

    fn record(&mut self, payload: EventPayload) -> u64 {
        self.push(Some(self.run), payload)
    }

    fn start_run(&mut self, binding: &FrozenModelBinding) {
        self.run = RunId::new_v7();
        let seq = self.tip() + 1;
        let started = run_started_event(seq, self.run, binding);
        self.events.push(started);
        self.check();
    }

    fn finish_run(&mut self) {
        self.record(EventPayload::run_finished(
            cookie_agent_protocol::RunOutcome::Completed {
                final_text: Some("Done.".into()),
            },
        ));
    }

    fn user(&mut self, text: &str) {
        let submitted = self.record(EventPayload::UserInputSubmitted {
            content: text.into(),
        });
        self.record(EventPayload::UserInputApplied {
            user_input_seq: submitted,
        });
    }

    /// A committed turn of `resolved` saying `text` and calling `calls`.
    fn turn(
        &mut self,
        resolved: ResolvedModelRef,
        text: &str,
        calls: &[&str],
    ) -> Vec<AssistantToolCallRef> {
        self.model_turn_seq += 1;
        let model_turn_seq = self.model_turn_seq;
        let mut content = vec![
            PersistedAssistantPart::Reasoning {
                text: format!("thinking before {text}"),
                metadata: None,
            },
            PersistedAssistantPart::Text {
                text: text.into(),
                metadata: None,
            },
        ];
        let owners = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                content.push(PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new(format!("{call}-{model_turn_seq}")).unwrap(),
                    provider_item_id: None,
                    name: SafeCode::new("read").unwrap(),
                    input: serde_json::json!({ "path": call }),
                    raw_input: None,
                    metadata: None,
                });
                AssistantToolCallRef {
                    model_turn_seq,
                    content_index: u32::try_from(index + 2).unwrap(),
                    model_call_id: ModelCallId::new(format!("{call}-{model_turn_seq}")).unwrap(),
                    provider_item_id: None,
                }
            })
            .collect::<Vec<_>>();
        let input_through_seq = self.tip();
        self.record(EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq,
            resolved_model: resolved,
            input_through_seq,
            turn: PersistedModelTurn {
                content,
                provider_options: BTreeMap::new(),
                finish_reason: if calls.is_empty() {
                    ModelFinishReason::Stop
                } else {
                    ModelFinishReason::ToolCalls
                },
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: None,
            },
            warnings: Vec::new(),
            prefix_fingerprint: None,
        });
        owners
    }

    fn start_call(&mut self, owner: &AssistantToolCallRef) -> ToolCallId {
        let tool_call_id = ToolCallId::new_v7();
        self.record(EventPayload::ToolCallStarted {
            start: ToolCallStart {
                background: true,
                output: Default::default(),
                tool_call_id,
                owner: owner.clone(),
                presentation: crate::test_support::base_presentation(PreparedPresentation::new(
                    SafeDisplayText::new("Read").unwrap(),
                    None,
                )),
                operation_fingerprint: OperationFingerprint::from_prepared_operation(&operation()),
            },
        });
        tool_call_id
    }

    fn end_call(&mut self, run: RunId, tool_call_id: ToolCallId, owner: AssistantToolCallRef) {
        self.push(
            Some(run),
            EventPayload::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(PersistedToolResult {
                        display: Vec::new(),
                        background: None,
                        retained_output: None,
                        title: SafeDisplayText::new("Read").unwrap(),
                        output: format!("output of {tool_call_id}"),
                        metadata: serde_json::Value::Null,
                        attachments: Vec::new(),
                        additional_messages: Vec::new(),
                        loaded_tools: Vec::new(),
                    }),
                    error: None,
                    presentation: None,
                },
            },
        );
    }

    fn call(&mut self, resolved: ResolvedModelRef, path: &str) {
        let owner = self.turn(resolved, "Reading.", &[path]).remove(0);
        let id = self.start_call(&owner);
        self.end_call(self.run, id, owner);
    }

    fn skill(&mut self, name: &str, body: &str) {
        self.record(EventPayload::SkillLoaded {
            name: name.into(),
            rendered_body: body.into(),
            source_path: format!("/{name}/SKILL.md"),
            args: String::new(),
            base_dir: format!("/{name}"),
            supporting_files: Vec::new(),
        });
    }

    fn agent_md(&mut self, content: &str) {
        let seq = self.tip() + 1;
        self.events.push(agent_md_event(seq, self.run, content));
        self.check();
    }

    fn memory(&mut self, body: &str) {
        let seq = self.tip() + 1;
        self.events.push(memory_event(seq, self.run, Some(body)));
        self.check();
    }

    /// The run's system prompt, composed afresh at a checkpoint inside it.
    fn composed(&mut self, prompt: &str) {
        self.record(EventPayload::SystemPromptComposed {
            composed_prompt: prompt.into(),
            prompt_fingerprint: Sha256Digest::of_bytes(prompt.as_bytes()),
            blocks: None,
        });
    }

    /// The system prompt a request of the latest run carries.
    fn prompt_in_force(&self) -> String {
        let fingerprint = Sha256Digest::of_bytes(b"System prompt.");
        super::super::prompt_in_force(&self.whole(), ("System prompt.", &fingerprint))
            .0
            .to_owned()
    }

    fn accept_message(&mut self, key: &str, body: &str) -> ProducerMessageId {
        let message_id = ProducerMessageId::new_v7();
        let seq = self.tip() + 1;
        self.events.push(producer_accepted_event(
            seq,
            message_id,
            ProducerOwner::Subagent {
                session_id: SessionId::new_v7(),
            },
            key,
            body,
        ));
        self.check();
        message_id
    }

    /// A summary checkpoint of everything so far, keeping the events from
    /// `recent_from`, with the provider window when `window`. With `fresh`,
    /// as this build commits one: `AGENTS.md` and memory recorded just
    /// before it.
    fn checkpoint(&mut self, recent_from: Option<u64>, window: bool, fresh: Option<(&str, &str)>) {
        let through = self.tip();
        let previous = self
            .events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.payload,
                    EventPayload::ContextCheckpointCommitted { .. }
                )
            })
            .map(|event| event.seq);
        if let Some((agent_md, memory)) = fresh {
            self.agent_md(agent_md);
            self.memory(memory);
        }
        let mut commit = summary_commit(
            &format!("summary through {through}"),
            previous
                .map_or(1, |seq| seq + 1)
                .min(recent_from.unwrap_or(u64::MAX)),
            through,
            recent_from,
            previous,
        );
        commit.context_fresh = fresh.is_some();
        if window && let ContextCheckpoint::InternalSummary { native, .. } = &mut commit.checkpoint
        {
            *native = Some(cookie_agent_protocol::CheckpointNativeWindow {
                window: self.window.clone(),
                source_through_seq: through,
            });
        }
        self.record(EventPayload::ContextCheckpointCommitted { commit });
    }

    /// The candidate for a kept tail nearest the tip.
    fn latest_tail(&self) -> Option<u64> {
        compaction_tail_candidates(&self.events).last().copied()
    }

    fn whole(&self) -> Vec<Arc<StoredEvent>> {
        self.events.iter().cloned().map(Arc::new).collect()
    }

    fn check(&mut self) {
        let whole = self.whole();
        let mut state = LogState::default();
        for event in &whole {
            state.observe(event);
        }
        let selected = model_context(&whole, &state).unwrap_or_else(|| whole.clone());
        // The log's own fold gives the prompt in force as a pass does.
        let run = state.latest_run_started().and_then(|event| event.run_id);
        assert_eq!(
            run.and_then(|run| state.run_prompt(run))
                .map(|event| event.seq),
            crate::events::model_context::latest_composed_prompt(&whole).map(|event| event.seq),
            "the run's prompt, at {}",
            self.tip()
        );
        // A log that let go of the rest each time holds the same.
        let tip = self.held.last().map_or(0, |event| event.seq);
        let mut held = std::mem::take(&mut self.held);
        held.extend(whole.iter().filter(|event| event.seq > tip).cloned());
        let held = model_context(&held, &state).unwrap_or(held);
        assert_eq!(
            held.iter().map(|event| event.seq).collect::<Vec<_>>(),
            selected.iter().map(|event| event.seq).collect::<Vec<_>>(),
            "what a native window would hold, at {}",
            self.tip()
        );
        self.held = held;
        self.let_go = self.let_go.max(whole.len() - selected.len());
        for binding in [&self.native, &self.other] {
            super::super::assert_assembles_as_whole(
                &selected,
                &whole,
                &self.store,
                binding,
                "System prompt.",
            );
        }
    }

    fn assembled(&self, binding: &FrozenModelBinding) -> super::super::ModelContext {
        assemble_model_context(&self.events, &self.store, binding, "System prompt.")
            .expect("the model's context")
    }
}

fn rendered(context: &super::super::ModelContext) -> String {
    serde_json::to_string(&context.history).unwrap()
}

/// An ACP agent's synthetic model: its runs name `acp/<agent>`.
fn acp_model(binding: &FrozenModelBinding) -> ResolvedModelRef {
    let agent = cookie_agent_protocol::AgentId::new("claude").unwrap();
    let selection = cookie_agent_protocol::acp_model_selection(&agent);
    ResolvedModelRef {
        provider_id: selection.model.provider_id().clone(),
        model_id: selection.model.model_id().clone(),
        selection_fingerprint: Sha256Digest::of_bytes(b"acp/claude"),
        selection,
        ..wire_model(binding)
    }
}

#[test]
fn a_long_native_session_assembles_from_its_model_context_as_from_the_whole_log() {
    let mut log = Scenario::new();
    let native = wire_model(&log.native);
    let other = wire_model(&log.other);

    // An ACP agent's runs first; the session then switches to a native model,
    // whose context is the whole log until a checkpoint.
    log.start_run(&log.native.clone());
    log.user("an ACP agent's question");
    log.turn(acp_model(&log.native), "the ACP agent's answer", &[]);
    log.finish_run();

    log.start_run(&log.native.clone());
    log.agent_md("rules, first version");
    log.memory("lessons, first version");
    log.user("read the sources");
    log.call(native.clone(), "src/lib.rs");
    log.skill("review", "REVIEW-SKILL-BODY");
    log.call(native.clone(), "src/main.rs");
    log.turn(native.clone(), "first answer", &[]);
    log.finish_run();

    // An earlier build's checkpoint: it pins every skill and the latest
    // AGENTS.md and memory.
    log.start_run(&log.native.clone());
    log.agent_md("rules, first version");
    log.memory("lessons, first version");
    log.user("keep going");
    log.call(native.clone(), "src/old.rs");
    let recent = log.latest_tail();
    log.checkpoint(recent, false, None);
    let context = log.assembled(&log.native);
    assert!(rendered(&context).contains("REVIEW-SKILL-BODY"));
    log.turn(native.clone(), "after the older checkpoint", &[]);
    log.finish_run();

    // A skill loaded, a background call left open, and a producer message
    // waiting, then a checkpoint of this build with the provider window.
    let pending = log.accept_message("report-1", "PENDING-REPORT-BODY");
    log.start_run(&log.native.clone());
    log.agent_md("rules, first version");
    log.memory("lessons, first version");
    log.user("plan the release");
    log.skill("release", "RELEASE-SKILL-BODY");
    let open = log
        .turn(native.clone(), "Starting the build.", &["build"])
        .remove(0);
    let open_run = log.run;
    let open_call = log.start_call(&open);
    log.call(native.clone(), "CHANGELOG.md");
    log.turn(native.clone(), "the build runs in the background", &[]);
    log.finish_run();
    log.start_run(&log.native.clone());
    log.agent_md("rules, first version");
    log.memory("lessons, first version");
    log.user("summarize the plan");
    log.turn(native.clone(), "the plan, summarized", &[]);
    let recent = log.latest_tail();
    log.checkpoint(
        recent,
        true,
        Some(("rules, second version", "lessons, second version")),
    );
    let with_window = log.assembled(&log.native);
    assert!(with_window.native_context.is_some());
    let summary = log.assembled(&log.other);
    assert!(summary.native_context.is_none());
    for context in [&with_window, &summary] {
        let text = rendered(context);
        // Read afresh, and nothing older carried.
        assert!(text.contains("rules, second version"), "{text}");
        assert!(text.contains("lessons, second version"));
        assert!(!text.contains("rules, first version"));
        assert!(!text.contains("REVIEW-SKILL-BODY"));
    }
    // The open call's turn and the pending message are pinned; the log lets
    // go of what is summarized.
    assert!(log.held.iter().any(|event| event.seq == open.model_turn_seq
        || matches!(
            &event.payload,
            EventPayload::ToolCallStarted { start } if start.tool_call_id == open_call
        )));
    assert!(log.let_go > 0, "nothing was let go of");

    // The call ends after the checkpoint, and the message is admitted to the
    // next run.
    log.end_call(open_run, open_call, open);
    log.start_run(&log.native.clone());
    log.record(EventPayload::ProducerMessageAdmitted {
        message_id: pending,
    });
    log.turn(native.clone(), "the report arrived", &[]);
    assert!(rendered(&log.assembled(&log.native)).contains("PENDING-REPORT-BODY"));
    // A skill loaded again after the checkpoint shows its body where it was
    // loaded.
    log.skill("review", "REVIEW-SKILL-BODY");
    log.turn(native.clone(), "reviewing again", &[]);
    log.finish_run();

    // A model switch: the window serves only its own model.
    log.start_run(&log.other.clone());
    log.user("now on another model");
    log.call(other.clone(), "src/other.rs");
    log.turn(other.clone(), "the other model's answer", &[]);
    let recent = log.latest_tail();
    log.checkpoint(
        recent,
        false,
        Some(("rules, third version", "lessons, third version")),
    );
    log.composed("PROMPT-AFTER-THIRD");
    log.turn(other.clone(), "after the summary-only checkpoint", &[]);
    assert_eq!(log.prompt_in_force(), "PROMPT-AFTER-THIRD");
    log.finish_run();

    // A message admitted to a run that ended before taking it, back to the
    // native model, a replay cut, and one more checkpoint with no kept tail,
    // in the middle of a run.
    let waiting = log.accept_message("report-2", "WAITING-REPORT-BODY");
    log.start_run(&log.native.clone());
    // A run's own prompt again: the earlier run's composed one was its own.
    assert_eq!(log.prompt_in_force(), "System prompt.");
    log.record(EventPayload::ProducerMessageAdmitted {
        message_id: waiting,
    });
    log.record(EventPayload::run_finished(
        cookie_agent_protocol::RunOutcome::Cancelled { reason: None },
    ));
    log.start_run(&log.native.clone());
    log.user("back on the first model");
    log.call(native.clone(), "src/back.rs");
    let open = log
        .turn(native.clone(), "Testing in the background.", &["tests"])
        .remove(0);
    let open_call = log.start_call(&open);
    log.record(EventPayload::NativeReplayCut {
        through_seq: log.tip(),
        cause: cookie_agent_protocol::ReplayCutCause::PrefixChanged,
    });
    log.checkpoint(
        None,
        true,
        Some(("rules, fourth version", "lessons, fourth version")),
    );
    log.composed("PROMPT-AFTER-FOURTH");
    // A call that ends after a checkpoint that kept nothing names the turn
    // and the start before it.
    log.end_call(log.run, open_call, open);
    log.user("the last question");
    log.turn(native.clone(), "the last answer", &[]);
    let text = rendered(&log.assembled(&log.other));
    assert!(text.contains("rules, fourth version"));
    assert!(text.contains(&format!("output of {open_call}")), "{text}");
    // The earliest tail a compaction may keep starts in the window, never at
    // a record pinned before it, such as the waiting message's admission.
    let earliest = compaction_tail_candidates(&log.events).first().copied();
    let start = crate::events::model_context::context_window_start(&log.events).unwrap();
    assert!(earliest.is_some_and(|seq| seq >= start));
    let composed = log
        .events
        .iter()
        .rfind(|event| matches!(event.payload, EventPayload::SystemPromptComposed { .. }))
        .unwrap()
        .seq;
    assert!(earliest.is_some_and(|seq| seq > composed));
    // The prompt this checkpoint composes is the same, so it records none:
    // the one before its window is the prompt in force, and is pinned.
    log.checkpoint(
        earliest,
        false,
        Some(("rules, fifth version", "lessons, fourth version")),
    );
    assert!(
        crate::events::model_context::context_window_start(&log.events)
            .is_some_and(|start| start > composed)
    );
    assert!(log.held.iter().any(|event| event.seq == composed));
    assert_eq!(log.prompt_in_force(), "PROMPT-AFTER-FOURTH");
    log.turn(native, "after the earliest tail", &[]);
    log.finish_run();
    let text = rendered(&log.assembled(&log.other));
    assert!(text.contains("rules, fifth version"));
    assert!(!text.contains("WAITING-REPORT-BODY"));
    assert!(!text.contains("src/other.rs"));
}

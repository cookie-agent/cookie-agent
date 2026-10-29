//! The session-event reducer and the projection helpers it drives.

use super::*;

/// The first id handed to a part opened by live-only output. Parts opened by
/// durable events take ids derived from their sequence, which stay far below
/// this, so the two never collide.
pub(super) const LIVE_PART_ID_BASE: u64 = 1 << 62;

/// Reduces live-only stream output (text and reasoning deltas, tool
/// progress). It is not durable: nothing here indexes a sequence, and the
/// committed turn or tool termination that follows is canonical. Returns
/// whether the projection changed.
pub(super) fn reduce_transient(
    state: &mut SessionState,
    timestamp: jiff::Timestamp,
    payload: EventPayload,
) -> bool {
    let (attempt_id, text, kind) = match payload {
        EventPayload::TextDelta { attempt_id, text } => (attempt_id, text, AssistantPartKind::Text),
        EventPayload::ReasoningDelta { attempt_id, text } => {
            (attempt_id, text, AssistantPartKind::Thinking)
        }
        EventPayload::ToolCallProgress {
            tool_call_id,
            message,
            display,
        } => {
            if let Some(tool) = state.tools.get_mut(&tool_call_id)
                && tool.status == ToolStatus::Running
            {
                let message = message.as_str().trim();
                let message = (!message.is_empty()).then(|| message.to_owned());
                let mut changed = tool.progress != message;
                tool.progress = message;
                if let Some(cookie_agent_protocol::Open::Known(delta)) = display {
                    changed |= matches!(tool.live_display.admit(delta), Ok(Some(_)));
                }
                if changed {
                    tool.revision = next_tool_revision();
                }
            }
            bump_tool_item(state, tool_call_id);
            return true;
        }
        EventPayload::InternalAgentTextDelta {
            invocation_id,
            text,
        } => return append_compacting_text(state, invocation_id, &text),
        EventPayload::NativeCompactionStarted { resolved_model } => {
            let seq = state.last_seq;
            push_item(state, timestamp, |id| TranscriptItem::Compacting {
                id,
                version: 0,
                seq,
                source: CompactingSource::Native {
                    model: render_model(&resolved_model),
                },
            });
            return true;
        }
        EventPayload::NativeCompactionFinished { .. } => return remove_native_compacting(state),
        _ => return false,
    };
    // Empty deltas (some providers emit an initial empty content chunk)
    // carry no content: they neither open a part nor count as the
    // attempt's first output.
    if text.is_empty() {
        return false;
    }
    if state.pending_attempt == Some(attempt_id) {
        state.pending_attempt = None;
    }
    // An attempt this view never saw start (it began before a mid-reply
    // attach) has no block to extend; its committed turn will show it.
    let Some(item_id) = assistant_segment_target(state, attempt_id, kind, timestamp) else {
        return true;
    };
    state.next_live_part_id = state.next_live_part_id.wrapping_add(1);
    let part_id = LIVE_PART_ID_BASE.wrapping_add(state.next_live_part_id);
    append_assistant_delta(state, item_id, part_id, text, kind, timestamp);
    true
}

/// [`reduce_event_from`] for an event without an origin.
#[cfg(test)]
pub(super) fn reduce_event(
    state: &mut SessionState,
    session_id: SessionId,
    run_id: Option<RunId>,
    sequence: u64,
    timestamp: jiff::Timestamp,
    payload: EventPayload,
) {
    reduce_event_from(
        state, session_id, run_id, sequence, timestamp, None, payload,
    );
}

pub(super) fn reduce_event_from(
    state: &mut SessionState,
    session_id: SessionId,
    run_id: Option<RunId>,
    sequence: u64,
    timestamp: jiff::Timestamp,
    origin: Option<cookie_agent_protocol::EventOrigin>,
    payload: EventPayload,
) {
    // The queued lane is the protocol's own reduction, shared with the
    // engine; what it voids goes back to its authors.
    let voided = state
        .queued_inputs
        .apply_payload(sequence, timestamp, origin.as_ref(), &payload);
    state
        .voided_inputs
        .extend(voided.into_iter().map(|entry| VoidedInput {
            seq: entry.queued_seq,
            origin: entry.origin,
            parked: false,
            input: ComposerInput::of(&entry.content),
        }));
    // Every durable timestamp is indexed by sequence so a committed turn
    // can measure its generation wall time from the exact event that closed
    // its input window — replay-exact, never render-time wall clock.
    state.event_timestamps.insert(sequence, timestamp);
    state.model_selection.apply(run_id, &payload);
    state.runtime_attachments.observe(&payload);
    match payload {
        EventPayload::GoalActivated {
            goal_id,
            objective,
            revision,
            // Future-run selection does not change frozen producing attribution.
            selection: _,
        } => {
            let replaceable = state
                .goal
                .as_ref()
                .is_none_or(|goal| goal_status_is_terminal(goal.status));
            let distinct = state
                .goal
                .as_ref()
                .is_none_or(|goal| goal.goal_id != goal_id);
            let unseen = !state.goal_revisions.contains_key(&goal_id);
            if objective.trim().is_empty() || !replaceable || !distinct || !unseen {
                return;
            }
            let goal = GoalState {
                goal_id,
                objective,
                status: GoalStatus::Active,
                items: Vec::new(),
                revision,
            };
            state.goal_revisions.insert(goal_id, revision);
            state.goal = Some(goal.clone());
            push_item(state, timestamp, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                activation: true,
                goal,
            });
        }
        EventPayload::GoalChecklistRevised {
            goal_id,
            items,
            revision,
        } => {
            let valid_items = items.iter().all(|item| !item.description.trim().is_empty());
            let valid = valid_items
                && state.goal.as_ref().is_some_and(|goal| {
                    goal.goal_id == goal_id
                        && !goal_status_is_terminal(goal.status)
                        && revision > goal.revision
                });
            if !valid {
                return;
            }
            let goal = state.goal.as_mut().expect("validated current goal");
            goal.items = items;
            goal.revision = revision;
            state.goal_revisions.insert(goal_id, revision);
            let snapshot = goal.clone();
            push_item(state, timestamp, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                activation: false,
                goal: snapshot,
            });
        }
        EventPayload::GoalLifecycleChanged {
            goal_id,
            status,
            revision,
            selection,
        } => {
            let valid = state.goal.as_ref().is_some_and(|goal| {
                goal.goal_id == goal_id
                    && revision > goal.revision
                    && valid_goal_lifecycle_change(goal, status)
                    && (selection.is_none() || status == GoalStatus::Active)
            });
            if !valid {
                return;
            }
            let goal = state.goal.as_mut().expect("validated current goal");
            goal.status = status;
            goal.revision = revision;
            state.goal_revisions.insert(goal_id, revision);
            let snapshot = goal.clone();
            push_item(state, timestamp, |id| TranscriptItem::Goal {
                id,
                seq: sequence,
                activation: false,
                goal: snapshot,
            });
        }
        EventPayload::ProducerMessageAccepted {
            message_id,
            producer_owner,
            mode,
            idempotency_key,
            body,
            description,
            reminder,
            // Internal guard metadata: never transcript-visible, so replay
            // ignores it rather than carrying a second projection.
            agent_hop: _,
            // The body already previews the output it keeps alive.
            retained_output: _,
        } => {
            if !valid_producer_reminder_owner(&producer_owner, reminder.as_ref())
                || state.producer_messages.contains_key(&message_id)
                || state
                    .producer_dedup
                    .contains_key(&(producer_owner.clone(), idempotency_key.clone()))
            {
                return;
            }
            let summary =
                (!description.as_str().trim().is_empty()).then(|| description.as_str().to_owned());
            let transcript_index = state.transcript.len();
            push_item(state, timestamp, |id| TranscriptItem::ProducerMessage {
                id,
                seq: sequence,
                accepted_at: timestamp,
                message_id,
                producer_owner: producer_owner.clone(),
                mode,
                body: user_content_text(&body),
                summary,
                reminder,
                status: ProducerMessageStatus::Pending,
            });
            state.producer_messages.insert(
                message_id,
                ProducerMessageProjection {
                    transcript_index,
                    producer_owner: producer_owner.clone(),
                    reminder,
                    accepted_seq: sequence,
                    admission: None,
                    claims: HashSet::new(),
                    status: ProducerMessageStatus::Pending,
                    discarded_seq: None,
                    consumed_run: None,
                    consumption_recorded: false,
                },
            );
            state
                .producer_dedup
                .insert((producer_owner, idempotency_key), message_id);
        }
        EventPayload::ProducerMessageAdmitted { message_id } => {
            let Some(run_id) = run_id else {
                return;
            };
            if state.terminal_runs.contains(&run_id) {
                return;
            }
            let Some(message) = state.producer_messages.get(&message_id) else {
                return;
            };
            let replaceable = message.admission.is_some_and(|(prior_run, _)| {
                prior_run != run_id && state.terminal_runs.contains(&prior_run)
            });
            if message.consumed_run.is_some()
                || message.discarded_seq.is_some()
                || (message.admission.is_some() && !replaceable)
            {
                return;
            }
            let message = state
                .producer_messages
                .get_mut(&message_id)
                .expect("validated producer message");
            message.admission = Some((run_id, sequence));
            state.initial_input_submitted.insert(run_id);
            let status = if message.claims.is_empty() {
                ProducerMessageStatus::Admitted
            } else {
                ProducerMessageStatus::Claimed
            };
            let transcript_index = message.transcript_index;
            update_producer_message_status(state, message_id, status);
            move_input_to_boundary(state, transcript_index, Some(run_id), timestamp);
        }
        EventPayload::ProducerMessagesClaimed { message_ids } => {
            let Some(run_id) = run_id else {
                return;
            };
            let unique = message_ids.iter().copied().collect::<HashSet<_>>();
            let valid = !message_ids.is_empty()
                && unique.len() == message_ids.len()
                && !state.producer_claims.contains_key(&sequence)
                && message_ids.iter().all(|message_id| {
                    state
                        .producer_messages
                        .get(message_id)
                        .is_some_and(|message| {
                            message.accepted_seq < sequence
                                && message.consumed_run.is_none()
                                && message.discarded_seq.is_none()
                                && message.admission.is_some_and(
                                    |(admission_run, admission_seq)| {
                                        admission_run == run_id && admission_seq < sequence
                                    },
                                )
                        })
                });
            if !valid {
                return;
            }
            for message_id in &message_ids {
                state
                    .producer_messages
                    .get_mut(message_id)
                    .expect("validated producer message")
                    .claims
                    .insert(sequence);
                update_producer_message_status(state, *message_id, ProducerMessageStatus::Claimed);
            }
            state.producer_claims.insert(
                sequence,
                ProducerClaimProjection {
                    run_id,
                    message_ids,
                },
            );
        }
        EventPayload::ProducerMessagesReleased { claim_seq } => {
            let Some(run_id) = run_id else {
                return;
            };
            let Some(claim) = state.producer_claims.get(&claim_seq) else {
                return;
            };
            if claim_seq == 0 || claim.run_id != run_id {
                return;
            }
            let message_ids = claim.message_ids.clone();
            for message_id in &message_ids {
                let Some(message) = state.producer_messages.get_mut(message_id) else {
                    continue;
                };
                message.claims.remove(&claim_seq);
                let status = producer_message_status(message);
                update_producer_message_status(state, *message_id, status);
            }
            state.producer_claims.remove(&claim_seq);
        }
        EventPayload::ProducerMessageConsumed {
            message_id,
            run_id: consumed_run,
        } => {
            let valid = run_id == Some(consumed_run)
                && state
                    .producer_messages
                    .get(&message_id)
                    .is_some_and(|message| {
                        message.admission.map(|(run, _)| run) == Some(consumed_run)
                            && message.consumed_run == Some(consumed_run)
                            && !message.consumption_recorded
                    });
            if valid {
                state
                    .producer_messages
                    .get_mut(&message_id)
                    .expect("validated producer message")
                    .consumption_recorded = true;
            }
        }
        EventPayload::ProducerMessageDiscarded {
            message_id,
            reminder,
            producer_owner,
        } => {
            let valid = state
                .producer_messages
                .get(&message_id)
                .is_some_and(|message| {
                    let identity_matches = producer_owner == message.producer_owner
                        && match &message.producer_owner {
                            ProducerOwner::Goal { .. } => {
                                reminder.as_ref().is_some_and(|identity| {
                                    message.reminder.as_ref() == Some(identity)
                                })
                            }
                            _ => reminder
                                .as_ref()
                                .is_none_or(|identity| message.reminder.as_ref() == Some(identity)),
                        };
                    identity_matches && message.consumed_run.is_none() && message.claims.is_empty()
                });
            if valid {
                let message = state
                    .producer_messages
                    .get_mut(&message_id)
                    .expect("validated producer message");
                if message.discarded_seq.is_none() {
                    message.discarded_seq = Some(sequence);
                    update_producer_message_status(
                        state,
                        message_id,
                        ProducerMessageStatus::Discarded,
                    );
                }
            }
        }
        EventPayload::RunStarted { agent, .. } => {
            close_open_assistant(state, timestamp);
            seal_run_assistant(state, timestamp, false);
            state.open_run_assistant = None;
            state.pending_attempt = None;
            state.active_run = run_id;
            state.run_agent = Some(agent.agent.clone());
            state.run_snapshot = Some(agent);
            state.agent_md_previous_run = std::mem::take(&mut state.agent_md_latest_run);
            state.agent_md_sources.clear();
            state.agent_md_row = None;
        }
        EventPayload::UserInputAdmitted { content } => {
            close_open_assistant(state, timestamp);
            state.last_agent_activity = Some(timestamp);
            state.pending_inputs.push_back(PendingInput {
                text: user_content_text(&content),
                composer: ComposerInput::of(&content),
                admission_seq: sequence,
                admitted_at: timestamp,
                origin,
            });
        }
        EventPayload::UserInputSubmitted { content } => {
            // The user acting clears a hold.
            state.held = None;
            close_open_assistant(state, timestamp);
            state.last_agent_activity = Some(timestamp);
            if run_id.is_some_and(|run_id| !state.initial_input_submitted.insert(run_id)) {
                // Promotion: only a submission after the run's initial input
                // graduates a lane entry — the oldest, strictly positionally,
                // exactly like the engine's own replay.
                state.pending_inputs.pop_front();
            }
            let files = content.files().cloned().collect::<Vec<_>>();
            if !files.is_empty() {
                state.user_files.insert(sequence, files);
            }
            push_item(state, timestamp, |id| TranscriptItem::User {
                id,
                version: 0,
                markdown: MarkdownDocument::new(user_content_markdown(&content)).finished(),
                seq: sequence,
            });
        }
        // The session model state folded it above.
        EventPayload::SessionModelSelected { .. } => {}
        // The queued lane was folded above, before the payload moved.
        EventPayload::UserInputQueued { .. }
        | EventPayload::UserInputQueueRecalled { .. }
        | EventPayload::UserInputQueueVoided {} => {
            state.last_agent_activity = Some(timestamp);
        }
        EventPayload::UserInputRecalled { user_input_seq, .. } => {
            close_open_assistant(state, timestamp);
            if let Some(position) = state
                .pending_inputs
                .iter()
                .position(|pending| pending.admission_seq == user_input_seq)
            {
                state.pending_inputs.remove(position);
            }
        }
        EventPayload::ModelAttemptStarted {
            attempt_id,
            resolved_model,
            ..
        } => {
            close_open_assistant(state, timestamp);
            // Attempt attribution is frozen: the producing agent comes from
            // the owning `RunStarted`, the exact resolved model from this
            // event — never from the current picker or live configuration.
            let agent = state
                .run_agent
                .clone()
                .unwrap_or_else(|| AgentId::new("unknown").expect("static agent id"));
            let mut attribution_marker = None;
            if let Some(run_id) = run_id {
                relocate_empty_split_block(state, run_id, &resolved_model);
            }
            let item_id = if let Some(run_id) = run_id {
                if let Some(projection) = state
                    .open_run_assistant
                    .as_ref()
                    .filter(|projection| projection.run_id == run_id && !projection.split_pending)
                {
                    let item_id = projection.item_id;
                    let changed = projection.current_model != resolved_model;
                    if changed {
                        attribution_marker =
                            append_attribution(state, item_id, resolved_model.clone(), timestamp);
                    }
                    state
                        .open_run_assistant
                        .as_mut()
                        .expect("run projection remains open")
                        .current_model = resolved_model;
                    item_id
                } else {
                    seal_run_assistant(state, timestamp, false);
                    let item_id = open_assistant_item(
                        state,
                        FrozenAssistantAttribution {
                            acp_model: acp_model_label(state, &resolved_model),
                            agent,
                            resolved_model: resolved_model.clone(),
                        },
                        timestamp,
                    );
                    state.open_run_assistant = Some(RunAssistantProjection {
                        run_id,
                        item_id,
                        committed_prefix: 0,
                        current_model: resolved_model,
                        split_pending: false,
                        split_at: None,
                    });
                    item_id
                }
            } else {
                open_assistant_item(
                    state,
                    FrozenAssistantAttribution {
                        acp_model: acp_model_label(state, &resolved_model),
                        agent,
                        resolved_model,
                    },
                    timestamp,
                )
            };
            let committed_prefix = state
                .open_run_assistant
                .as_ref()
                .filter(|projection| projection.item_id == item_id)
                .map_or(0, |projection| projection.committed_prefix);
            state.attempts.insert(
                attempt_id,
                AttemptProjection {
                    item_id,
                    run_id,
                    committed_prefix,
                    attribution_marker,
                    split_segments: Vec::new(),
                },
            );
            state.pending_attempt = Some(attempt_id);
        }
        // Live-only output never reaches the durable reducer; see
        // `reduce_transient`.
        EventPayload::TextDelta { .. }
        | EventPayload::ReasoningDelta { .. }
        | EventPayload::InternalAgentTextDelta { .. }
        | EventPayload::NativeCompactionStarted { .. }
        | EventPayload::NativeCompactionFinished { .. }
        | EventPayload::ToolCallProgress { .. } => {}
        // The attempt has visible output from here on, so a later input
        // boundary no longer moves it; its deltas themselves are live-only.
        EventPayload::ModelOutputStarted { attempt_id, kind } => {
            if state.pending_attempt == Some(attempt_id) {
                state.pending_attempt = None;
            }
            state
                .stream_marks
                .entry(attempt_id)
                .or_default()
                .push((kind, timestamp));
        }
        EventPayload::AttemptAbandoned {
            attempt_id,
            model_error,
        } => {
            close_open_assistant(state, timestamp);
            if state.pending_attempt == Some(attempt_id) {
                state.pending_attempt = None;
            }
            state.stream_marks.remove(&attempt_id);
            let attempt = state.attempts.remove(&attempt_id);
            if let Some(attempt) = &attempt
                && attempt.run_id.is_some()
            {
                prune_split_segments(state, &attempt.split_segments);
                prune_abandoned_attempt(state, attempt.item_id, attempt.committed_prefix);
            }
            let interrupted = model_error
                .as_ref()
                .is_some_and(|error| error.kind == cookie_agent_protocol::ModelErrorKind::Abort);
            if interrupted {
                // A commit that kept content already noted the interruption
                // with the marker the model reads after it.
                let marked = state.interrupt_marked.remove(&attempt_id);
                let text = crate::model_input::interrupted_note(marked.is_some());
                // Without a tracked block (the attempt started before this
                // view's replay window) the interruption still shows, as a row.
                let noted = if marked == Some(true) {
                    Some(0)
                } else {
                    attempt.as_ref().and_then(|attempt| {
                        append_assistant_notice(state, attempt.item_id, text.clone(), timestamp)
                    })
                };
                if noted.is_none() {
                    push_event(state, EventLevel::Warning, text, timestamp);
                }
            } else {
                let message = match &model_error {
                    Some(error) => {
                        format!("model attempt abandoned: {}", render_model_error(error))
                    }
                    None => "model attempt abandoned".into(),
                };
                push_event(state, EventLevel::Warning, message, timestamp);
                state.mark_attempt_boundary_split(run_id, timestamp);
            }
        }
        EventPayload::ModelTurnCommitted {
            attempt_id,
            model_turn_seq,
            resolved_model,
            input_through_seq,
            turn,
            warnings,
            ..
        } => {
            if state.pending_attempt == Some(attempt_id) {
                state.pending_attempt = None;
            }
            let streamed_thinking = state
                .stream_marks
                .remove(&attempt_id)
                .map(|marks| streamed_thinking_durations(&marks, timestamp));
            if let Some(run_id) = run_id {
                consume_producer_messages_through(state, run_id, input_through_seq);
            }
            close_open_assistant(state, timestamp);
            // The context the turn left behind: what it consumed plus what
            // it generated. A usage-less turn clears the display, except an
            // interrupted one: it never reports usage, and the previous
            // turn's context remains the best estimate.
            // An ACP agent's turns report no usage: its context use arrives
            // in `acp_session_updated` and stands across its turns.
            let acp_turn = resolved_model.selection.model.provider_id().as_str()
                == cookie_agent_protocol::ACP_MODEL_PROVIDER;
            if turn.finish_reason != cookie_agent_protocol::ModelFinishReason::Aborted && !acp_turn
            {
                state.context_tokens = match (turn.usage.input_tokens, turn.usage.output_tokens) {
                    (Some(input), Some(output)) => Some(input.saturating_add(output)),
                    _ => None,
                };
                state.context_window = None;
            }
            // Generation wall time: the durable span between the event that
            // closed this turn's input window and the commit itself.
            // Missing or clock-skewed (negative/zero) spans contribute
            // nothing, so unmeasured generation never dilutes the rate.
            let generation = state
                .event_timestamps
                .get(&input_through_seq)
                .and_then(|started| {
                    std::time::Duration::try_from(timestamp.duration_since(*started)).ok()
                })
                .filter(|duration| !duration.is_zero());
            // Later commits always close their inputs at a newer sequence,
            // so older timestamps are dead weight.
            state.event_timestamps = state.event_timestamps.split_off(&input_through_seq);
            // The committed turn is the canonical boundary: every
            // text/thinking/tool child is rebuilt in exact
            // `PersistedModelTurn.content` order, preserving multiple
            // segments and content indices. Tool parts become committed
            // placeholders linked by `owner.content_index` when their start
            // event arrives.
            // A turn carrying tool calls or media starts new segments: when
            // an event row interleaved mid-turn, the whole turn's canonical
            // rebuild belongs in a fresh block below the row.
            // An ACP agent keeps its own history: cookie adds no marker.
            let interrupted_with_content = turn.finish_reason
                == cookie_agent_protocol::ModelFinishReason::Aborted
                && !turn.content.is_empty()
                && !acp_turn;
            if turn.content.iter().any(|part| {
                matches!(
                    part,
                    cookie_agent_protocol::PersistedAssistantPart::ToolCall { .. }
                        | cookie_agent_protocol::PersistedAssistantPart::File { .. }
                )
            }) {
                split_assistant_if_pending(state, attempt_id, timestamp);
            }
            if let Some(projection) = state.attempts.get(&attempt_id) {
                let item_id = projection.item_id;
                let committed_prefix = projection.committed_prefix;
                let metrics = state.assistant_metrics.entry(item_id).or_default();
                if let Some(generation) = generation {
                    metrics.timed_output_tokens = metrics
                        .timed_output_tokens
                        .saturating_add(turn.usage.output_tokens.unwrap_or(0));
                    metrics.generation += generation;
                    metrics.timed_turns = metrics.timed_turns.saturating_add(1);
                }
                // The context the block now holds: what the turn consumed
                // plus everything it generated. Either side missing makes
                // the total unknowable, so the footer stays hidden.
                if let (Some(input), Some(output)) =
                    (turn.usage.input_tokens, turn.usage.output_tokens)
                {
                    metrics.context_tokens = Some(input.saturating_add(output));
                }
                mark_committed(state, item_id, model_turn_seq, &resolved_model);
                index_turn_tool_content(state, model_turn_seq, &turn);
                rebuild_committed_children(
                    state,
                    item_id,
                    model_turn_seq,
                    sequence,
                    committed_prefix,
                    &turn,
                    streamed_thinking,
                    timestamp,
                );
                // The committed turn rebuilt canonically in the newest block,
                // so the attempt's pre-split streamed output in earlier
                // blocks is superseded.
                let segments = state
                    .attempts
                    .get_mut(&attempt_id)
                    .map(|attempt| std::mem::take(&mut attempt.split_segments))
                    .unwrap_or_default();
                prune_split_segments(state, &segments);
                // A reply an interrupt cut off with content left is followed
                // by the interrupt marker in later requests: its note quotes
                // it, inside the committed prefix so the abandon keeps it.
                if interrupted_with_content {
                    let noted = append_assistant_notice(
                        state,
                        item_id,
                        crate::model_input::interrupted_note(true),
                        timestamp,
                    )
                    .is_some();
                    state.interrupt_marked.insert(attempt_id, noted);
                }
                // The block now holds committed content, so the attempt's
                // prune boundary must cover it: an interrupt commits the
                // partial turn and then abandons the attempt, and the abandon
                // must not prune the content the commit just made canonical.
                let committed = state
                    .transcript
                    .iter()
                    .find_map(|item| match item {
                        TranscriptItem::Assistant { id, children, .. } if *id == item_id => {
                            Some(children.len())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Some(attempt) = state.attempts.get_mut(&attempt_id) {
                    attempt.committed_prefix = committed;
                }
            } else {
                index_turn_tool_content(state, model_turn_seq, &turn);
                if interrupted_with_content {
                    state.interrupt_marked.insert(attempt_id, false);
                }
            }
            place_tool_rows(state);
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "model {} committed · finish {:?} · usage {}",
                    render_model(&resolved_model),
                    turn.finish_reason,
                    render_usage(&turn.usage)
                ),
                timestamp,
            );
            for warning in warnings {
                if let Some(run_id) = run_id
                    && !state
                        .model_turn_warnings
                        .insert((run_id, warning.to_string()))
                {
                    continue;
                }
                push_event(
                    state,
                    EventLevel::Warning,
                    format!(
                        "model warning from {}: {warning}",
                        render_model(&resolved_model)
                    ),
                    timestamp,
                );
            }
        }
        EventPayload::ModelReplayEvaluated {
            resolved_model,
            ordered_decisions,
            base_attempt_id,
            ..
        } => {
            close_open_assistant(state, timestamp);
            // Incompatible replay/cache discards are one WARNING per logical
            // run transition. Reconstruction is the expected consequence and
            // remains DEBUG, as do routine replay details. An evaluation
            // written against a base lists only what it adds, and the base's
            // decisions were rendered with it.
            if ordered_decisions.is_empty() && base_attempt_id.is_none() {
                push_event(
                    state,
                    EventLevel::Info,
                    format!(
                        "model {} replay · no history entries",
                        render_model(&resolved_model)
                    ),
                    timestamp,
                );
            }
            for source in &ordered_decisions {
                let (level, decision) = render_replay_decision(source);
                if level == EventLevel::Warning
                    && let Some(run_id) = run_id
                    && let Some(key) = replay_context_warning_key(&resolved_model, source)
                    && !state.replay_context_warnings.insert(key.with_run(run_id))
                {
                    continue;
                }
                push_event(
                    state,
                    level,
                    format!(
                        "model {} replay · {decision}",
                        render_model(&resolved_model)
                    ),
                    timestamp,
                );
            }
        }
        EventPayload::ModelFallback {
            from,
            to,
            attempts_on_from,
            error,
            ..
        } => {
            close_open_assistant(state, timestamp);
            push_event(
                state,
                EventLevel::Warning,
                format!(
                    "model fallback {} → {} after {attempts_on_from} attempt(s) · {}",
                    render_model(&from),
                    render_model(&to),
                    render_model_error(&error)
                ),
                timestamp,
            );
            state.mark_attempt_boundary_split(run_id, timestamp);
        }
        EventPayload::ToolCallStarted { start } => {
            close_open_assistant(state, timestamp);
            if start
                .presentation
                .hint
                .as_ref()
                .is_some_and(|hint| hint.rendered() == cookie_agent_protocol::ToolHint::DELEGATE)
            {
                state.last_agent_activity = Some(timestamp);
            }
            let arguments = find_tool_call_content(
                state,
                start.owner.model_turn_seq,
                &start.owner.model_call_id,
            )
            .unwrap_or_else(|| "{}".into());
            state.pending_tool_rows.push(PendingToolRow {
                started_at: timestamp,
                turn_seq: start.owner.model_turn_seq,
                content_index: start.owner.content_index,
                call_id: start.tool_call_id,
            });
            let mut tool = ToolCallState::started(
                start.tool_call_id,
                start.owner.clone(),
                start.presentation.clone(),
                arguments,
            );
            tool.backgroundable = start.background;
            state.tools.insert(start.tool_call_id, tool);
            place_tool_rows(state);
        }
        EventPayload::ToolCallTerminated { termination } => {
            let tool_call_id = termination.tool_call_id;
            let status = match termination.outcome {
                ToolTerminationOutcome::Completed => ToolStatus::Completed,
                ToolTerminationOutcome::Failed => ToolStatus::Failed,
                ToolTerminationOutcome::Cancelled => ToolStatus::Cancelled,
                ToolTerminationOutcome::Interrupted => ToolStatus::Interrupted,
            };
            let failed = !matches!(termination.outcome, ToolTerminationOutcome::Completed);
            let backgrounded = termination
                .result
                .as_ref()
                .is_some_and(|result| result.background.is_some());
            let failure_message = matches!(
                termination.outcome,
                ToolTerminationOutcome::Failed | ToolTerminationOutcome::Interrupted
            )
            .then(|| cookie_agent_protocol::diagnostics::tool(&termination));
            // The final display replaces the live one. A call that did not
            // complete and has none keeps what it streamed, else shows the
            // output its result carries.
            let streamed = state
                .tools
                .get_mut(&tool_call_id)
                .map(|tool| std::mem::take(&mut tool.live_display).into_blocks())
                .filter(|blocks| !blocks.is_empty());
            let result_display = termination
                .result
                .as_ref()
                .map(|result| result.display.clone())
                .filter(|display| !display.is_empty());
            // Named capture can contain just [stdout]/[stderr] headings with no bytes.
            let partial_output = termination.result.as_ref().and_then(|result| {
                (!result.output.trim().is_empty()
                    && result.retained_output.as_ref().is_none_or(|retained| {
                        retained.streams.iter().any(|stream| stream.byte_length > 0)
                    }))
                .then(|| {
                    cookie_agent_protocol::text_display(
                        cookie_agent_protocol::model_text::without_loop_warning(&result.output)
                            .to_owned(),
                    )
                })
            });
            let mut display = match result_display {
                Some(display) => display,
                None if failed => streamed.or(partial_output).unwrap_or_default(),
                None => Vec::new(),
            };
            // What the result sent the model beyond the tool's own output
            // shows under it, word for word.
            if let Some(result) = &termination.result {
                display.extend(crate::model_input::loop_warning_note(&result.output));
                display.extend(crate::model_input::emitted_message_notes(
                    &result.additional_messages,
                ));
                if result.additional_messages.is_empty() {
                    state.tool_emitted_messages.remove(&tool_call_id);
                } else {
                    state
                        .tool_emitted_messages
                        .insert(tool_call_id, result.additional_messages.len());
                }
            }
            let failure = if !failed {
                None
            } else if display.is_empty() {
                termination
                    .error
                    .as_ref()
                    .map(|error| error.message.to_string())
                    .or(failure_message.clone())
            } else {
                // The renderer bounds output lines; the failure reason goes
                // ahead of them.
                failure_message.clone()
            };
            if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                tool.status = status;
                tool.display = display;
                tool.failure = failure;
                tool.progress = None;
                tool.full.clear();
                tool.backgrounded = backgrounded;
                tool.revision = next_tool_revision();
            }
            // A failed call is fed back to the model as its tool result and
            // carries the failure inline on its tool item; only surface an
            // event row when no tool item exists to hold it.
            if let Some(message) =
                failure_message.filter(|_| !state.tools.contains_key(&tool_call_id))
            {
                push_event(
                    state,
                    EventLevel::Error,
                    format!("tool {tool_call_id}: {message}"),
                    timestamp,
                );
            }
            bump_tool_item(state, tool_call_id);
            state.last_tool_termination = Some((sequence, tool_call_id, run_id));
        }
        EventPayload::ApprovalRequested { request } => {
            state
                .approvals
                .retain(|approval| approval.approval_id != request.approval_id());
            state
                .approvals
                .push(approval_state_from_request(session_id, request, false));
        }
        EventPayload::ApprovalStep { approval_id, step } => match step.into_known() {
            Some(cookie_agent_protocol::ApprovalStep::Evaluated { decision }) => {
                if let Some(approval) = state
                    .approvals
                    .iter_mut()
                    .find(|approval| approval.approval_id == approval_id)
                {
                    approval.reviewer = crate::ReviewerNote::of(&decision);
                }
                push_event(
                    state,
                    EventLevel::Debug,
                    format!(
                        "approval {approval_id} evaluated: {:?} ({:?})",
                        decision.decision, decision.reason_code
                    )
                    .to_lowercase(),
                    timestamp,
                );
            }
            Some(cookie_agent_protocol::ApprovalStep::Escalated {
                reason_code,
                expires_at,
            }) => {
                if let Some(approval) = state
                    .approvals
                    .iter_mut()
                    .find(|approval| approval.approval_id == approval_id)
                {
                    approval.escalated = true;
                    // The user's window starts at the escalation (client
                    // protocol 1.28.0); an earlier daemon's request carries it.
                    if expires_at.is_some() {
                        approval.constraints.expires_at = expires_at;
                    }
                }
                push_event(
                    state,
                    EventLevel::Info,
                    format!("approval {approval_id} escalated: {reason_code:?}").to_lowercase(),
                    timestamp,
                );
            }
            Some(cookie_agent_protocol::ApprovalStep::UserDecision { decision, .. }) => push_event(
                state,
                EventLevel::Info,
                format!("approval {approval_id} response recorded: {decision:?}").to_lowercase(),
                timestamp,
            ),
            Some(cookie_agent_protocol::ApprovalStep::DoomLoop {
                operation_fingerprint,
                repetitions,
            }) => push_event(
                state,
                EventLevel::Error,
                format!(
                    "approval {approval_id} doom loop: {} repeated {repetitions} times",
                    operation_fingerprint.digest()
                ),
                timestamp,
            ),
            // A step this build does not know shows nothing.
            None => {}
        },
        EventPayload::ApprovalFinalized {
            approval_id,
            outcome,
        } => {
            let reviewer_reason = state
                .approvals
                .iter()
                .find(|approval| approval.approval_id == approval_id)
                .and_then(|approval| match &approval.reviewer {
                    Some(crate::ReviewerNote::Decided {
                        reason: Some(reason),
                    }) => Some(reason.clone()),
                    _ => None,
                });
            state
                .approvals
                .retain(|approval| approval.approval_id != approval_id);
            let message = match outcome.known() {
                Some(cookie_agent_protocol::ApprovalFinalization::Decided { decision }) => format!(
                    "approval {approval_id}: {} ({:?})",
                    approval_outcome_label(decision.outcome),
                    decision.reason_code
                ),
                Some(cookie_agent_protocol::ApprovalFinalization::Cancelled { reason_code }) => {
                    format!("approval {approval_id} cancelled: {reason_code:?}")
                }
                None => format!("approval {approval_id} ended"),
            };
            // A call the reviewer resolved, or that its permission mode
            // resolved after the reviewer asked, says why (approval reviewer
            // spec section 7).
            let resolved_without_user = outcome.known().is_some_and(|outcome| {
                matches!(
                    outcome.decision().source,
                    cookie_agent_protocol::ApprovalDecisionSource::InternalAgent
                        | cookie_agent_protocol::ApprovalDecisionSource::PermissionMode
                )
            });
            let message = match reviewer_reason.filter(|_| resolved_without_user) {
                Some(reason) => format!("{} · the reviewer: {reason}", message.to_lowercase()),
                None => message.to_lowercase(),
            };
            push_event(state, EventLevel::Info, message, timestamp);
        }
        EventPayload::ElicitationRequested { request } => {
            state
                .questions
                .retain(|question| question.request.elicitation_id != request.elicitation_id);
            state.questions.push(crate::elicitation::QuestionState {
                session_id,
                run_id,
                request,
            });
        }
        EventPayload::ElicitationResolved { resolution } => {
            let asked = state
                .questions
                .iter()
                .position(|question| question.request.elicitation_id == resolution.elicitation_id)
                .map(|index| state.questions.remove(index));
            let message = asked.map_or_else(
                || "a question".to_owned(),
                |question| format!("{}: “{}”", question.heading(), question.request.message),
            );
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "{message} {}",
                    crate::elicitation::resolution_text(&resolution)
                ),
                timestamp,
            );
        }
        EventPayload::TreeApprovalGrantCommitted { grant } => push_event(
            state,
            EventLevel::Debug,
            format!(
                "tree approval grant {} committed for {}",
                grant.grant_id,
                grant.operation_fingerprint.digest()
            ),
            timestamp,
        ),
        EventPayload::RunFinished { outcome } => {
            close_open_assistant(state, timestamp);
            // An agent's compaction its run left without an end ended with
            // it.
            while remove_compacting_where(state, |source| {
                matches!(source, CompactingSource::Agent { .. })
            }) {}
            let interrupted = outcome.status() == cookie_agent_protocol::SessionStatus::Interrupted;
            let stopped =
                interrupted || outcome.status() == cookie_agent_protocol::SessionStatus::Cancelled;
            seal_run_assistant(state, timestamp, stopped);
            if interrupted {
                // The run's open block keeps an `interrupted` footer marker.
                if let Some(projection) = state.open_run_assistant.take() {
                    state.interrupted_assistant_items.insert(projection.item_id);
                    // The marker changes what the block shows even when
                    // nothing was streaming to close.
                    if let Some(TranscriptItem::Assistant { version, .. }) = state
                        .transcript
                        .iter_mut()
                        .rfind(|item| item.id() == projection.item_id)
                    {
                        *version = version.wrapping_add(1);
                    }
                }
            } else {
                state.open_run_assistant = None;
            }
            state.pending_attempt = None;
            state.active_run = None;
            state.attempts.clear();
            state.stream_marks.clear();
            state.pending_tool_rows.clear();
            void_pending_inputs(state);
            state.approvals.clear();
            state.questions.clear();
            if let Some(run_id) = run_id {
                state.terminal_runs.insert(run_id);
            }
            let (level, message) = run_finished_message(&outcome);
            push_event(state, level, message, timestamp);
        }
        EventPayload::InternalAgentStarted {
            invocation_id,
            kind,
            backend,
            call,
            ..
        } => {
            push_event(
                state,
                EventLevel::Info,
                format!(
                    "internal agent {kind:?} started via {}: {}",
                    render_internal_backend(&backend),
                    call.input_summary
                )
                .to_lowercase(),
                timestamp,
            );
            if kind == cookie_agent_protocol::InternalAgentKind::ContextCompaction {
                // Native compaction failed over to the summarizer.
                remove_native_compacting(state);
                push_item(state, timestamp, |id| TranscriptItem::Compacting {
                    id,
                    version: 0,
                    seq: sequence,
                    source: CompactingSource::Summary {
                        invocation_id,
                        text: String::new(),
                    },
                });
            }
        }
        EventPayload::InternalAgentFinished {
            invocation_id,
            kind,
            outcome,
            ..
        } => {
            remove_compacting(state, invocation_id);
            let (level, message) = internal_agent_finished_message(kind, &outcome);
            push_event(state, level, message, timestamp);
        }
        EventPayload::InternalAgentFallback {
            invocation_id,
            kind,
            from,
            to,
            failure,
            attempts,
            ..
        } => {
            // The next model streams its summary from the start.
            clear_compacting_text(state, invocation_id);
            push_event(
                state,
                EventLevel::Warning,
                cookie_agent_protocol::diagnostics::internal_fallback(
                    kind, &from, &to, attempts, &failure,
                ),
                timestamp,
            );
        }
        EventPayload::ContextCheckpointCommitted { commit } => {
            // The compaction row is the run's chronological boundary: the next
            // new segment opens fresh below it, and a block that never
            // committed anything is moved under the row itself instead of
            // being split off as an empty header.
            remove_native_compacting(state);
            let relocate = state.split_run_at_compaction(timestamp);
            push_item(state, timestamp, |id| TranscriptItem::Compaction {
                id,
                version: 0,
                seq: sequence,
                commit,
            });
            if let Some(index) = relocate {
                move_transcript_item_to_end(state, index);
            }
        }
        // The model reads a marker in place of the output from here on: the
        // call's row says so under its output.
        EventPayload::ToolOutputElided {
            tool_call_id,
            original_bytes,
            retained,
        } => {
            let emitted = state
                .tool_emitted_messages
                .get(&tool_call_id)
                .copied()
                .unwrap_or(0);
            let note = crate::model_input::elision_note(&retained.uri, original_bytes, emitted);
            match state.tools.get_mut(&tool_call_id) {
                Some(tool) => {
                    tool.display.push(note);
                    tool.revision = next_tool_revision();
                    bump_tool_item(state, tool_call_id);
                }
                None => push_event(
                    state,
                    EventLevel::Debug,
                    format!(
                        "tool output {tool_call_id} elided ({original_bytes} bytes retained at {})",
                        retained.uri
                    ),
                    timestamp,
                ),
            }
        }
        EventPayload::DelegationProgressed {
            invocation_id,
            stage:
                cookie_agent_protocol::Open::Known(cookie_agent_protocol::DelegationStage::Queued {
                    position,
                }),
        } => push_event(
            state,
            EventLevel::Info,
            position.map_or_else(
                || format!("subagent {invocation_id} queued"),
                |position| format!("subagent {invocation_id} queued at position {position}"),
            ),
            timestamp,
        ),
        EventPayload::DelegationFinished {
            child_session_id,
            status,
            total_lines,
            ..
        } => push_event(
            state,
            if matches!(status, cookie_agent_protocol::SessionStatus::Completed) {
                EventLevel::Info
            } else {
                EventLevel::Warning
            },
            format!("subagent {child_session_id} finished: {status:?} ({total_lines} lines)")
                .to_lowercase(),
            timestamp,
        ),
        // The snapshot reaches the model as a pinned turn: a row shows the
        // turn whenever it differs from the one shown last, and says so
        // when memory leaves the model's context.
        EventPayload::MemoryLoaded { snapshot } => {
            let text = cookie_agent_protocol::model_text::memory_snapshot_turn(&snapshot)
                .unwrap_or_default();
            let shown = state.memory_turn.as_deref().unwrap_or_default();
            if text != shown {
                state.memory_turn = Some(text.clone());
                push_model_input(
                    state,
                    timestamp,
                    sequence,
                    crate::model_input::ModelInputKind::Memory,
                    crate::model_input::memory_header(&text),
                    text,
                );
            }
        }
        // A model's memory change, as daemons before 2026-10-24 recorded
        // it (memory spec section 16.9.10). The call that made it shows the
        // change on its own row, so a change that follows its call folds
        // into it; one the reducer cannot match keeps a line that opens the
        // Memory view, with the call above it and what the run does next
        // below it, as for a skill.
        EventPayload::MemoryChanged { change, url } => {
            if memory_change_call(state, run_id, sequence, &url).is_some() {
                return;
            }
            state.mark_event_split_pending(timestamp);
            push_item(state, timestamp, |id| TranscriptItem::MemoryChange {
                id,
                seq: sequence,
                change,
                url,
            });
        }
        // The ACP session an agent talks in, shown at the debug level.
        EventPayload::AcpSessionStarted { record } => {
            state.acp.agent = Some(record.agent.clone());
            state.acp.runtime_id = Some(record.runtime_id.clone());
            state.acp.steering = record.steering;
            state.acp.subagent = record.how == cookie_agent_protocol::AcpSessionStart::Subagent;
            push_event(
                state,
                EventLevel::Debug,
                format!(
                    "ACP session {} of {} ({:?})",
                    record.acp_session_id, record.agent, record.how
                ),
                timestamp,
            );
        }
        // Each member is the whole new value.
        EventPayload::AcpSessionUpdated { update } => {
            if let Some(commands) = update.commands {
                state.acp.commands = commands;
            }
            if let Some(options) = update.options {
                state.acp.options = options;
            }
            if let Some(context) = update.context {
                state.context_tokens = Some(context.used);
                state.context_window = context.size;
            }
            if let Some(starting) = update.starting_options {
                state.acp.starting_options = starting;
            }
            if let Some(cookie_agent_protocol::Open::Known(
                cookie_agent_protocol::DisplayBlock::Plan { entries },
            )) = update.plan
            {
                upsert_plan(state, run_id, entries, timestamp);
            }
            if let Some(compaction) = update.compaction {
                agent_compaction(state, timestamp, sequence, compaction);
            }
            if let Some(parts) = update.prompt_context.filter(|parts| !parts.is_empty()) {
                let text = parts.join("\n\n");
                let agent = state
                    .acp
                    .agent
                    .as_ref()
                    .map_or("the agent", |agent| agent.as_str());
                let header = crate::model_input::agent_context_header(agent, &text);
                push_model_input(
                    state,
                    timestamp,
                    sequence,
                    crate::model_input::ModelInputKind::AgentContext,
                    header,
                    text,
                );
            }
            if let Some(notice) = update.notice {
                push_split_item(state, timestamp, |id| TranscriptItem::AcpNotice {
                    id,
                    notice,
                });
            }
            if let Some(message) = update.session_message {
                push_split_item(state, timestamp, |id| TranscriptItem::AgentSessionMessage {
                    id,
                    seq: sequence,
                    message,
                });
            }
            if let Some(ended) = update.background_ended {
                push_split_item(state, timestamp, |id| {
                    TranscriptItem::AgentBackgroundEnded {
                        id,
                        seq: sequence,
                        ended_at: timestamp,
                        ended,
                    }
                });
            }
            if let Some(steered) = update.steered {
                for seq in steered.user_inputs {
                    state.acp.steered.insert(seq);
                    if let Some(TranscriptItem::User { version, .. }) =
                        state.transcript.iter_mut().rev().find(
                            |item| matches!(item, TranscriptItem::User { seq: user, .. } if *user == seq),
                        )
                    {
                        *version = version.wrapping_add(1);
                    }
                }
            }
            if let Some(presented) = update.tool_presentation {
                let tool_call_id = presented.tool_call_id;
                if let Some(tool) = state.tools.get_mut(&tool_call_id) {
                    let presentation = presented.presentation;
                    tool.presentation.title = presentation.title;
                    tool.presentation.hint = presentation.hint;
                    tool.presentation.primary_argument = presentation.primary_argument;
                    tool.presentation.arguments = presentation.arguments;
                    tool.presentation.preview = presentation.preview;
                    tool.revision = next_tool_revision();
                }
                bump_tool_item(state, tool_call_id);
            }
        }
        // What a writer left out changes nothing; the debug view names it.
        EventPayload::RecordOmitted { omitted_type } => push_event(
            state,
            EventLevel::Debug,
            format!("event `{omitted_type}` (seq {sequence}) was not carried over"),
            timestamp,
        ),
        EventPayload::DelegateChildTerminated { status, reason } => push_event(
            state,
            if matches!(status, cookie_agent_protocol::SessionStatus::Failed) {
                EventLevel::Error
            } else {
                EventLevel::Info
            },
            reason.map_or_else(
                || format!("subagent {status:?}").to_lowercase(),
                |reason| format!("subagent {status:?}: {reason}").to_lowercase(),
            ),
            timestamp,
        ),
        EventPayload::Diagnostic {
            source,
            kind,
            message,
            count,
            ..
        } => {
            let message = match cookie_agent_protocol::diagnostic_subject(&source, &kind) {
                Some(subject) => format!("{subject}: {message}"),
                None => message,
            };
            let message = match count {
                Some(count) if count > 1 => format!("{message} (count: {count})"),
                _ => message,
            };
            push_event(state, EventLevel::Warning, message, timestamp);
        }
        EventPayload::SessionTitleCommitted { change, .. } => {
            push_event(
                state,
                EventLevel::Info,
                render_title_commit(&change),
                timestamp,
            );
        }
        EventPayload::UserInputApplied { user_input_seq } => {
            if let Some(index) = state.transcript.iter().rposition(
                |item| matches!(item, TranscriptItem::User { seq, .. } if *seq == user_input_seq),
            ) {
                move_input_to_boundary(state, index, run_id, timestamp);
            }
        }
        EventPayload::SessionCreated {
            cwd_identity,
            creation_agent,
            ..
        } => {
            // Before any run starts, attempts (for example title generation)
            // attribute to the creation agent's frozen identity.
            if state.run_agent.is_none() {
                state.run_agent = Some(creation_agent.agent.clone());
            }
            state.cwd_identity = Some(cwd_identity);
            state.creation_agent = Some(creation_agent);
            state.created_at = Some(timestamp);
        }
        EventPayload::ModelUsageRecorded {
            model_turn_seq,
            estimated_cost_pico_usd,
            resolved_model,
            usage,
            ..
        } => {
            if let Some(item_id) = state.turn_items.get(&model_turn_seq).copied() {
                let opened = state.item_times.get(&item_id).copied();
                let context = state.context_tokens;
                let metrics = state.assistant_metrics.entry(item_id).or_default();
                metrics.record_cost(estimated_cost_pico_usd);
                // An ACP agent reports a prompt's usage at its end, not its
                // turns': its output over the block's time so far gives the
                // rate, and its latest context use the context.
                if resolved_model.provider_id.as_str() == cookie_agent_protocol::ACP_MODEL_PROVIDER
                {
                    metrics.timed_output_tokens = metrics
                        .timed_output_tokens
                        .saturating_add(usage.output_tokens.unwrap_or(0));
                    if let Some(span) = opened.and_then(|opened| {
                        std::time::Duration::try_from(timestamp.duration_since(opened)).ok()
                    }) {
                        metrics.generation = span;
                    }
                    if context.is_some() {
                        metrics.context_tokens = context;
                    }
                }
                if let Some(TranscriptItem::Assistant { version, .. }) = state
                    .transcript
                    .iter_mut()
                    .rfind(|item| item.id() == item_id)
                {
                    *version = version.wrapping_add(1);
                }
            }
        }
        EventPayload::MessageInjected { role, content } => {
            push_item(state, timestamp, |id| TranscriptItem::PluginMessage {
                id,
                version: 0,
                seq: sequence,
                role,
                input: user_content_text(&content),
            })
        }
        EventPayload::AgentMdLoaded {
            entries,
            attachment,
            project,
        } => {
            // Every root run reloads AGENTS.md, one event per attachment and
            // one for the session's project; a row appears only when a
            // source's context differs from its previous run's. A run that
            // loaded none for a source breaks its chain, so re-enabling
            // shows a row again.
            // The row shows the run's whole AGENTS.md turn as the model
            // reads it, so a later load of the run updates it.
            let key = match &project {
                Some(project) => format!("project:{}", project.id),
                None => attachment.as_str().to_owned(),
            };
            let unchanged = state.agent_md_previous_run.get(&key) == Some(&entries);
            state
                .agent_md_latest_run
                .insert(key.clone(), entries.clone());
            state.agent_md_sources.retain(|source| source.key != key);
            state.agent_md_sources.push(crate::AgentMdSourceState {
                key,
                attachment: attachment.as_str().to_owned(),
                project: project.is_some(),
                entries,
            });
            let (all_entries, text) = crate::model_input::agent_md_turn(&state.agent_md_sources);
            let row = state.agent_md_row.and_then(|row| {
                state
                    .transcript
                    .iter_mut()
                    .rev()
                    .find_map(|item| match item {
                        TranscriptItem::AgentMd {
                            id,
                            version,
                            entries,
                            text,
                            ..
                        } if *id == row => Some((version, entries, text)),
                        _ => None,
                    })
            });
            match row {
                Some((version, entries, shown)) => {
                    *entries = all_entries;
                    *shown = text;
                    *version = version.wrapping_add(1);
                }
                None if !unchanged => {
                    push_item(state, timestamp, |id| TranscriptItem::AgentMd {
                        id,
                        version: 0,
                        seq: sequence,
                        entries: all_entries,
                        text,
                    });
                    state.agent_md_row = Some(state.next_transcript_id);
                }
                None => {}
            }
        }
        EventPayload::ModelToolsPublished {
            tool_names,
            deferred_tool_names,
            ..
        } => {
            state.model_tools = Some(tool_names.into());
            state.model_deferred_tools = deferred_tool_names.into();
        }
        EventPayload::SkillLoaded {
            name,
            rendered_body,
            source_path,
            args,
            ..
        } => {
            // A skill loaded by a tool mid-turn sits between that call and
            // the output that follows it, like any interleaved event row.
            state.mark_event_split_pending(timestamp);
            push_item(state, timestamp, |id| TranscriptItem::SkillLoaded {
                id,
                version: 0,
                seq: sequence,
                name,
                source_path,
                args,
                rendered_body,
            });
        }
        EventPayload::DelegatedContextSeeded { turns, .. } => {
            if !turns.is_empty() {
                let (header, text) = crate::model_input::inherited_context(&turns);
                push_model_input(
                    state,
                    timestamp,
                    sequence,
                    crate::model_input::ModelInputKind::InheritedContext,
                    header,
                    text,
                );
            }
        }
        EventPayload::DeferredToolsAnnounced { added, removed } => {
            let (header, text) = crate::model_input::deferred_tools(&added, &removed);
            push_model_input(
                state,
                timestamp,
                sequence,
                crate::model_input::ModelInputKind::DeferredTools,
                header,
                text,
            );
        }
        EventPayload::PromptBlocksAnnounced { blocks } => {
            let (header, text) = crate::model_input::prompt_blocks(&blocks);
            push_model_input(
                state,
                timestamp,
                sequence,
                crate::model_input::ModelInputKind::PromptBlocks,
                header,
                text,
            );
        }
        EventPayload::UserInputTransformed { .. }
        | EventPayload::DelegationReserved { .. }
        | EventPayload::DelegationProgressed { .. }
        | EventPayload::ModelRequestPrepared { .. }
        | EventPayload::ToolRoundStarted { .. }
        | EventPayload::PromptBlocksRecorded { .. }
        | EventPayload::InternalAgentUsageRecorded { .. }
        | EventPayload::ToolCallLinked { .. }
        | EventPayload::SessionPermissionOverlaySet { .. }
        | EventPayload::PermissionModeSet { .. }
        | EventPayload::SessionProjectSet { .. }
        | EventPayload::DelegatedAttachmentsAdded { .. }
        | EventPayload::RuntimeAttached { .. }
        | EventPayload::RuntimeDetached { .. }
        | EventPayload::NativeReplayCut { .. }
        | EventPayload::SkillInvocationNoted { .. } => {}
        EventPayload::AutomaticRunsHeld { reason } => {
            push_event(
                state,
                EventLevel::Warning,
                crate::held::held_row(&reason),
                timestamp,
            );
            state.held = Some(reason.clone());
        }
        EventPayload::AutomaticRunsResumed {} => {
            state.held = None;
            push_event(
                state,
                EventLevel::Info,
                "Automatic runs resumed.".to_owned(),
                timestamp,
            );
        }
    }
}

pub(super) fn reduce_session_events<E: std::borrow::Borrow<StoredEvent>>(
    session_id: SessionId,
    generation: u64,
    physical_events: &[E],
) -> SessionState {
    let mut state = SessionState {
        generation,
        last_seq: physical_events.last().map_or(0, |event| event.borrow().seq),
        ..SessionState::default()
    };
    for event in physical_events {
        let event = event.borrow();
        reduce_event_from(
            &mut state,
            session_id,
            event.run_id,
            event.seq,
            event.timestamp,
            event.origin.clone(),
            event.payload.clone(),
        );
    }
    // Rebuilding re-reduces durable history: a run that ended long ago must
    // not paste its unsent text into the composer again. Only a live run
    // termination restores unsent input.
    state.voided_inputs.clear();
    state
}

pub(super) fn goal_status_is_terminal(status: GoalStatus) -> bool {
    matches!(status, GoalStatus::Completed | GoalStatus::Cancelled)
}

pub(super) fn valid_goal_lifecycle_change(goal: &GoalState, status: GoalStatus) -> bool {
    match (goal.status, status) {
        (GoalStatus::Active, GoalStatus::Paused | GoalStatus::Cancelled)
        | (GoalStatus::Paused, GoalStatus::Active | GoalStatus::Cancelled) => true,
        (GoalStatus::Active | GoalStatus::Paused, GoalStatus::Completed) => {
            !goal.items.is_empty() && goal.items.iter().all(|item| item.finished)
        }
        _ => false,
    }
}

pub(super) fn valid_producer_reminder_owner(
    owner: &ProducerOwner,
    reminder: Option<&GoalReminderIdentity>,
) -> bool {
    match (owner, reminder) {
        (ProducerOwner::Goal { goal_id }, Some(reminder)) => *goal_id == reminder.goal_id,
        (ProducerOwner::Goal { .. }, None) | (_, Some(_)) => false,
        (_, None) => true,
    }
}

/// Inputs end run-level grouping. Retries can start before input promotion;
/// only a retry with no output can move to the new segment.
pub(super) fn move_input_to_boundary(
    state: &mut SessionState,
    index: usize,
    run_id: Option<RunId>,
    timestamp: jiff::Timestamp,
) {
    close_open_assistant(state, timestamp);
    let pending = state.pending_attempt.and_then(|attempt_id| {
        let attempt = state.attempts.get(&attempt_id)?.clone();
        let projection = state.open_run_assistant.as_ref()?;
        (Some(projection.run_id) == run_id && projection.item_id == attempt.item_id)
            .then(|| (attempt_id, attempt, projection.clone()))
    });
    state.open_run_assistant = None;
    move_transcript_item_to_end(state, index);
    if let Some((attempt_id, attempt, projection)) = pending {
        rebind_pending_attempt(state, attempt_id, attempt, projection, timestamp);
    }
}

pub(super) fn rebind_pending_attempt(
    state: &mut SessionState,
    attempt_id: AttemptId,
    attempt: AttemptProjection,
    mut projection: RunAssistantProjection,
    timestamp: jiff::Timestamp,
) {
    let index = state
        .transcript
        .iter()
        .rposition(|item| item.id() == attempt.item_id)
        .expect("pending attempt owns an assistant item");
    let TranscriptItem::Assistant {
        attribution,
        children,
        child_times,
        committed_turn_seq,
        version,
        ..
    } = &mut state.transcript[index]
    else {
        unreachable!("pending attempt owns an assistant item")
    };
    let new_attribution = FrozenAssistantAttribution {
        agent: attribution.agent.clone(),
        resolved_model: projection.current_model.clone(),
        acp_model: attribution.acp_model.clone(),
    };
    // A fallback marker belongs to this unstreamed retry, not the old block.
    if let Some(marker) = attempt.attribution_marker {
        if child_times.len() == children.len() {
            child_times.remove(marker);
        }
        children.remove(marker);
        *version = version.wrapping_add(1);
    }
    let reuse_empty = children.is_empty() && committed_turn_seq.is_none();
    if reuse_empty {
        *attribution = new_attribution;
        *version = version.wrapping_add(1);
        move_transcript_item_to_end(state, index);
    } else {
        seal_assistant(
            state,
            projection.item_id,
            projection.split_at.unwrap_or(timestamp),
            false,
        );
        projection.item_id = open_assistant_item(state, new_attribution, timestamp);
    }
    projection.committed_prefix = 0;
    projection.clear_split();
    state.attempts.insert(
        attempt_id,
        AttemptProjection {
            item_id: projection.item_id,
            run_id: Some(projection.run_id),
            committed_prefix: 0,
            attribution_marker: None,
            split_segments: Vec::new(),
        },
    );
    state.open_run_assistant = Some(projection);
}

/// Live compaction summary text kept per row; a checkpoint summary is
/// bounded by `max_summary_bytes`, 256 KiB by default.
const MAX_COMPACTING_TEXT_BYTES: usize = 256 * 1024;

fn compacting_text_mut(
    state: &mut SessionState,
    invocation: cookie_agent_protocol::InternalAgentInvocationId,
) -> Option<(&mut u64, &mut String)> {
    state.transcript.iter_mut().find_map(|item| match item {
        TranscriptItem::Compacting {
            version,
            source:
                CompactingSource::Summary {
                    invocation_id,
                    text,
                },
            ..
        } if *invocation_id == invocation => Some((version, text)),
        _ => None,
    })
}

/// Appends streamed summary text to its compaction row. Returns whether the
/// projection changed.
fn append_compacting_text(
    state: &mut SessionState,
    invocation: cookie_agent_protocol::InternalAgentInvocationId,
    delta: &str,
) -> bool {
    let Some((version, text)) = compacting_text_mut(state, invocation) else {
        return false;
    };
    let room = MAX_COMPACTING_TEXT_BYTES.saturating_sub(text.len());
    let mut end = delta.len().min(room);
    while !delta.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return false;
    }
    text.push_str(&delta[..end]);
    *version = version.wrapping_add(1);
    true
}

fn clear_compacting_text(
    state: &mut SessionState,
    invocation: cookie_agent_protocol::InternalAgentInvocationId,
) {
    if let Some((version, text)) = compacting_text_mut(state, invocation)
        && !text.is_empty()
    {
        text.clear();
        *version = version.wrapping_add(1);
    }
}

fn remove_compacting(
    state: &mut SessionState,
    invocation: cookie_agent_protocol::InternalAgentInvocationId,
) {
    remove_compacting_where(
        state,
        |source| matches!(source, CompactingSource::Summary { invocation_id, .. } if *invocation_id == invocation),
    );
}

/// Removes a native compaction row. Its end is live-only, so the
/// summarizer taking over or a committed checkpoint also clears it. Returns
/// whether a row was removed.
fn remove_native_compacting(state: &mut SessionState) -> bool {
    remove_compacting_where(state, |source| {
        matches!(source, CompactingSource::Native { .. })
    })
}

/// A compaction of the ACP agent's own: a compacting row while it runs, a
/// compaction row once it completed, as the session's own compaction shows.
fn agent_compaction(
    state: &mut SessionState,
    timestamp: jiff::Timestamp,
    sequence: u64,
    compaction: cookie_agent_protocol::AcpCompaction,
) {
    use cookie_agent_protocol::AcpCompactionState;
    let compaction_id = compaction.compaction_id;
    if compaction.state == AcpCompactionState::InProgress {
        push_item(state, timestamp, |id| TranscriptItem::Compacting {
            id,
            version: 0,
            seq: sequence,
            source: CompactingSource::Agent { compaction_id },
        });
        return;
    }
    remove_compacting_where(
        state,
        |source| matches!(source, CompactingSource::Agent { compaction_id: running } if *running == compaction_id),
    );
    // A failure's warning comes as a notice; an abandoned one leaves nothing.
    if matches!(
        compaction.state,
        AcpCompactionState::Failed | AcpCompactionState::Cancelled
    ) {
        return;
    }
    let summary = compaction.summary;
    push_split_item(state, timestamp, |id| TranscriptItem::AgentCompaction {
        id,
        seq: sequence,
        summary,
    });
}

/// Pushes a row the run's output must not straddle, as a compaction row
/// is: what the run shows next opens below it.
fn push_split_item(
    state: &mut SessionState,
    timestamp: jiff::Timestamp,
    item: impl FnOnce(u64) -> TranscriptItem,
) {
    let relocate = state.split_run_at_compaction(timestamp);
    push_item(state, timestamp, item);
    if let Some(index) = relocate {
        move_transcript_item_to_end(state, index);
    }
}

fn remove_compacting_where(
    state: &mut SessionState,
    matches: impl Fn(&CompactingSource) -> bool,
) -> bool {
    let Some(index) = state.transcript.iter().position(
        |item| matches!(item, TranscriptItem::Compacting { source, .. } if matches(source)),
    ) else {
        return false;
    };
    state.transcript.remove(index);
    for message in state.producer_messages.values_mut() {
        if message.transcript_index > index {
            message.transcript_index -= 1;
        }
    }
    true
}

pub(super) fn move_transcript_item_to_end(state: &mut SessionState, index: usize) {
    state.transcript[index..].rotate_left(1);
    let last = state.transcript.len() - 1;
    for message in state.producer_messages.values_mut() {
        if message.transcript_index == index {
            message.transcript_index = last;
        } else if message.transcript_index > index {
            message.transcript_index -= 1;
        }
    }
}

pub(super) fn producer_message_status(
    message: &ProducerMessageProjection,
) -> ProducerMessageStatus {
    if message.consumed_run.is_some() {
        ProducerMessageStatus::Consumed
    } else if message.discarded_seq.is_some() {
        ProducerMessageStatus::Discarded
    } else if !message.claims.is_empty() {
        ProducerMessageStatus::Claimed
    } else if message.admission.is_some() {
        ProducerMessageStatus::Admitted
    } else {
        ProducerMessageStatus::Pending
    }
}

pub(super) fn update_producer_message_status(
    state: &mut SessionState,
    message_id: ProducerMessageId,
    status: ProducerMessageStatus,
) {
    let Some(projection) = state.producer_messages.get_mut(&message_id) else {
        return;
    };
    let transcript_index = projection.transcript_index;
    let Some(TranscriptItem::ProducerMessage {
        message_id: row_message_id,
        status: row_status,
        ..
    }) = state.transcript.get_mut(transcript_index)
    else {
        return;
    };
    if *row_message_id != message_id {
        return;
    }
    projection.status = status;
    *row_status = status;
}

pub(super) fn consume_producer_messages_through(
    state: &mut SessionState,
    run_id: RunId,
    input_through_seq: u64,
) {
    let consumed = state
        .producer_messages
        .iter()
        .filter_map(|(message_id, message)| {
            (message.consumed_run.is_none()
                && message
                    .admission
                    .is_some_and(|(admission_run, admission_seq)| {
                        admission_run == run_id && admission_seq <= input_through_seq
                    })
                && message
                    .discarded_seq
                    .is_none_or(|discarded_seq| discarded_seq > input_through_seq))
            .then_some(*message_id)
        })
        .collect::<Vec<_>>();
    for message_id in consumed {
        let message = state
            .producer_messages
            .get_mut(&message_id)
            .expect("projected producer message");
        message.discarded_seq = None;
        message.consumed_run = Some(run_id);
        update_producer_message_status(state, message_id, ProducerMessageStatus::Consumed);
    }
}

/// Open a fresh assistant item for a run segment or run-less streaming attempt.
pub(super) fn open_assistant_item(
    state: &mut SessionState,
    attribution: FrozenAssistantAttribution,
    timestamp: jiff::Timestamp,
) -> u64 {
    state.open_assistant = None;
    push_item(state, timestamp, |id| TranscriptItem::Assistant {
        id,
        version: 0,
        attribution,
        committed_turn_seq: None,
        children: Vec::new(),
        child_times: Vec::new(),
    });
    state
        .transcript
        .last()
        .expect("assistant item was just pushed")
        .id()
}

pub(super) fn append_attribution(
    state: &mut SessionState,
    item_id: u64,
    resolved_model: ResolvedModelRef,
    timestamp: jiff::Timestamp,
) -> Option<usize> {
    if let Some(TranscriptItem::Assistant {
        version,
        children,
        child_times,
        ..
    }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == item_id)
    {
        let index = children.len();
        push_child(
            children,
            child_times,
            AssistantChild::Attribution { resolved_model },
            timestamp,
        );
        *version = version.wrapping_add(1);
        return Some(index);
    }
    None
}

pub(super) fn append_assistant_notice(
    state: &mut SessionState,
    item_id: u64,
    text: String,
    timestamp: jiff::Timestamp,
) -> Option<usize> {
    if let Some(TranscriptItem::Assistant {
        version,
        children,
        child_times,
        ..
    }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == item_id)
    {
        let index = children.len();
        push_child(
            children,
            child_times,
            AssistantChild::Notice { text },
            timestamp,
        );
        *version = version.wrapping_add(1);
        return Some(index);
    }
    None
}

pub(super) fn prune_abandoned_attempt(
    state: &mut SessionState,
    item_id: u64,
    committed_prefix: usize,
) {
    if let Some(TranscriptItem::Assistant {
        version,
        children,
        child_times,
        ..
    }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == item_id)
    {
        let timed = child_times.len() == children.len();
        let committed_prefix = committed_prefix.min(children.len());
        let tail = children.split_off(committed_prefix);
        let tail_times = if timed {
            child_times.split_off(committed_prefix)
        } else {
            Vec::new()
        };
        for child in &tail {
            if let AssistantChild::Thinking { id, .. } = child {
                state.thinking_durations.remove(&(item_id, *id));
            }
        }
        // Attribution markers survive the prune, with their times.
        for (index, child) in tail.into_iter().enumerate() {
            if matches!(child, AssistantChild::Attribution { .. }) {
                if let Some(time) = tail_times.get(index) {
                    child_times.push(*time);
                }
                children.push(child);
            }
        }
        *version = version.wrapping_add(1);
    }
}

pub(super) fn mark_committed(
    state: &mut SessionState,
    item_id: u64,
    model_turn_seq: u64,
    resolved_model: &ResolvedModelRef,
) {
    if let Some(TranscriptItem::Assistant {
        version,
        attribution,
        committed_turn_seq,
        ..
    }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == item_id)
    {
        // Only the first commit may reconcile the first attempt's header.
        if committed_turn_seq.is_none() {
            attribution.resolved_model = resolved_model.clone();
        }
        *committed_turn_seq = Some(model_turn_seq);
        *version = version.wrapping_add(1);
    }
    state.turn_items.insert(model_turn_seq, item_id);
}

/// Index durable tool input from committed turn content so an ownership
/// event's expanded row can show exact arguments without the start event
/// duplicating them.
pub(super) fn index_turn_tool_content(
    state: &mut SessionState,
    model_turn_seq: u64,
    turn: &PersistedModelTurn,
) {
    for part in &turn.content {
        if let cookie_agent_protocol::PersistedAssistantPart::ToolCall { id, input, .. } = part {
            state.turn_tool_index.insert(
                (model_turn_seq, id.as_str().to_owned()),
                IndexedToolCall {
                    arguments: input.to_string(),
                },
            );
        }
    }
}

/// Rebuild every text/thinking/tool child in exact
/// `PersistedModelTurn.content` order. Each content part becomes one child:
/// text and thinking parts preserve multiple segments; tool calls become
/// placeholders that started tools link by their exact `content_index`.
/// Deltas that streamed ahead of the commit are superseded by the durable
/// turn, which is the sole canonical content.
#[allow(clippy::too_many_arguments)]
pub(super) fn rebuild_committed_children(
    state: &mut SessionState,
    item_id: u64,
    model_turn_seq: u64,
    sequence: u64,
    committed_prefix: usize,
    turn: &PersistedModelTurn,
    streamed_thinking: Option<Vec<Option<Duration>>>,
    timestamp: jiff::Timestamp,
) {
    state.open_assistant = None;
    let mut children = Vec::with_capacity(turn.content.len());
    for (index, part) in turn.content.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        match part {
            cookie_agent_protocol::PersistedAssistantPart::Text { text, .. }
                if !text.is_empty() =>
            {
                children.push(AssistantChild::Text {
                    id: sequence,
                    version: 0,
                    markdown: MarkdownDocument::new(text.clone()).finished(),
                });
            }
            cookie_agent_protocol::PersistedAssistantPart::Reasoning { text, .. }
                if !text.is_empty() =>
            {
                children.push(AssistantChild::Thinking {
                    id: sequence,
                    version: 0,
                    text: text.clone(),
                });
            }
            cookie_agent_protocol::PersistedAssistantPart::ToolCall { name, .. } => {
                children.push(AssistantChild::CommittedTool {
                    turn_seq: model_turn_seq,
                    content_index: index,
                    name: name.clone(),
                });
            }
            cookie_agent_protocol::PersistedAssistantPart::File { file } => {
                children.push(AssistantChild::MediaFile {
                    turn_seq: model_turn_seq,
                    content_index: index,
                    file: file.clone(),
                });
            }
            _ => {}
        }
    }
    // Bump the sequence-derived child id so distinct segments never share
    // one id across consecutive parts of the same kind.
    for (offset, child) in children.iter_mut().enumerate() {
        let id = sequence.wrapping_add(offset as u64).max(1);
        match child {
            AssistantChild::Text { id: existing, .. }
            | AssistantChild::Thinking { id: existing, .. } => {
                *existing = id;
            }
            AssistantChild::Attribution { .. }
            | AssistantChild::CommittedTool { .. }
            | AssistantChild::MediaFile { .. }
            | AssistantChild::Notice { .. }
            | AssistantChild::Tool { .. } => {}
        }
    }
    if let Some(TranscriptItem::Assistant {
        version,
        children: existing,
        child_times,
        ..
    }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == item_id)
    {
        let committed_prefix = committed_prefix.min(existing.len());
        // Whitespace-only parts leading a block with no committed content
        // render as blank lines directly under the header; once the block
        // holds committed content the same parts are meaningful spacing
        // between sections and turns.
        if committed_prefix == 0 {
            let leading = children
                .iter()
                .take_while(|child| match child {
                    AssistantChild::Text { markdown, .. } => markdown.as_str().trim().is_empty(),
                    AssistantChild::Thinking { text, .. } => text.trim().is_empty(),
                    _ => false,
                })
                .count();
            children.drain(..leading);
        }
        // Streamed thinking parts are superseded by their committed
        // counterparts. The committed thinking children take, in order, the
        // durations the attempt's durable stream marks measure, which a
        // replay reproduces exactly; without marks, the streamed parts'
        // sealed durations transfer so "thought for Ns" survives the swap.
        let mut sealed_durations = Vec::new();
        for child in existing.iter().skip(committed_prefix) {
            if let AssistantChild::Thinking { id, .. } = child {
                sealed_durations.push(state.thinking_durations.remove(&(item_id, *id)));
            }
        }
        let mut sealed_durations = streamed_thinking.unwrap_or(sealed_durations).into_iter();
        for child in &mut children {
            if let AssistantChild::Thinking { id, .. } = child
                && let Some(Some(duration)) = sealed_durations.next()
            {
                state.thinking_durations.insert((item_id, *id), duration);
            }
        }
        // The committed turn is placed at its commit: that is when its
        // canonical content entered the block, live and on replay alike.
        let timed = child_times.len() == existing.len();
        let superseded = existing.drain(committed_prefix..).collect::<Vec<_>>();
        let superseded_times = if timed {
            child_times.split_off(committed_prefix)
        } else {
            Vec::new()
        };
        for (index, child) in superseded.into_iter().enumerate() {
            if matches!(child, AssistantChild::Attribution { .. }) {
                if let Some(time) = superseded_times.get(index) {
                    child_times.push(*time);
                }
                existing.push(child);
            }
        }
        if timed {
            child_times.extend(std::iter::repeat_n(timestamp, children.len()));
        }
        existing.extend(children);
        *version = version.wrapping_add(1);
        if let Some(projection) = state
            .open_run_assistant
            .as_mut()
            .filter(|projection| projection.item_id == item_id)
        {
            projection.committed_prefix = existing.len();
        }
    }
}

/// Appends streamed text to the open part of `kind`, or opens a new part
/// with id `part_id` (unused when an open part continues).
pub(super) fn append_assistant_delta(
    state: &mut SessionState,
    item_id: u64,
    part_id: u64,
    text: String,
    kind: AssistantPartKind,
    timestamp: jiff::Timestamp,
) {
    if let Some(open) = state.open_assistant
        && open.item_id == item_id
        && let Some(TranscriptItem::Assistant {
            version,
            children,
            child_times,
            ..
        }) = state
            .transcript
            .iter_mut()
            .rfind(|item| item.id() == open.item_id)
    {
        if open.kind == kind {
            let part_index = children
                .iter()
                .position(|part| part.id() == open.part_id && assistant_part_is_kind(part, kind))
                .or_else(|| {
                    children
                        .iter()
                        .rposition(|part| assistant_part_is_kind(part, kind))
                });
            if let Some(part_index) = part_index {
                let part = &mut children[part_index];
                let part_id = part.id();
                match (part, kind) {
                    (
                        AssistantChild::Text {
                            version, markdown, ..
                        },
                        AssistantPartKind::Text,
                    ) => {
                        markdown.append(&text);
                        *version = version.wrapping_add(1);
                    }
                    (
                        AssistantChild::Thinking {
                            version,
                            text: existing,
                            ..
                        },
                        AssistantPartKind::Thinking,
                    ) => {
                        existing.push_str(&text);
                        *version = version.wrapping_add(1);
                    }
                    _ => unreachable!("open assistant part kind matches its projection"),
                }
                *version = version.wrapping_add(1);
                // Continuing the same open part preserves its original
                // opening timestamp; the rare rposition fallback continues a
                // different part, whose opening time is no longer known.
                state.open_assistant = Some(OpenAssistantProjection {
                    opened_at: if part_id == open.part_id {
                        open.opened_at
                    } else {
                        timestamp
                    },
                    part_id,
                    ..open
                });
                return;
            }
        }
        // A part of a different kind (or an unknown continuation) replaces
        // the open projection: seal the previous thinking part first.
        if let Some(previous) = state.open_assistant.take() {
            seal_open_thinking(&mut state.thinking_durations, previous, timestamp);
        }
        push_child(
            children,
            child_times,
            new_assistant_part(part_id, text, kind),
            timestamp,
        );
        *version = version.wrapping_add(1);
        state.open_assistant = Some(OpenAssistantProjection {
            item_id,
            part_id,
            kind,
            opened_at: timestamp,
        });
        return;
    }
    if let Some(previous) = state.open_assistant.take() {
        seal_open_thinking(&mut state.thinking_durations, previous, timestamp);
    }
    if let Some(TranscriptItem::Assistant {
        version,
        children,
        child_times,
        ..
    }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == item_id)
    {
        push_child(
            children,
            child_times,
            new_assistant_part(part_id, text, kind),
            timestamp,
        );
        *version = version.wrapping_add(1);
        state.open_assistant = Some(OpenAssistantProjection {
            item_id,
            part_id,
            kind,
            opened_at: timestamp,
        });
    } else {
        // The owning item is gone; keep the projection cleared.
        state.open_assistant = None;
    }
}

pub(super) fn assistant_part_is_kind(part: &AssistantChild, kind: AssistantPartKind) -> bool {
    matches!(
        (part, kind),
        (AssistantChild::Text { .. }, AssistantPartKind::Text)
            | (AssistantChild::Thinking { .. }, AssistantPartKind::Thinking)
    )
}

pub(super) fn new_assistant_part(
    sequence: u64,
    text: String,
    kind: AssistantPartKind,
) -> AssistantChild {
    match kind {
        AssistantPartKind::Text => AssistantChild::Text {
            id: sequence,
            version: 0,
            markdown: MarkdownDocument::new(text),
        },
        AssistantPartKind::Thinking => AssistantChild::Thinking {
            id: sequence,
            version: 0,
            text,
        },
    }
}

/// Link started tools into their owning assistant item at the committed
/// placeholder with the exact same content index. A tool begins only after
/// its owning turn is durable; rows whose owning item or placeholder is not
/// yet known stay buffered, so out-of-order starts/completions can never
/// reorder children.
pub(super) fn place_tool_rows(state: &mut SessionState) {
    if state.pending_tool_rows.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut state.pending_tool_rows);
    let mut deferred = Vec::new();
    for row in pending {
        let linked = state
            .turn_items
            .get(&row.turn_seq)
            .copied()
            .is_some_and(|item_id| link_tool_child(state, item_id, &row));
        if !linked {
            deferred.push(row);
        }
    }
    state.pending_tool_rows = deferred;
}

/// Replace the committed placeholder at `content_index` with the started
/// tool. Returns false when the owning item or placeholder is not durable
/// yet (or the index does not name a tool part).
pub(super) fn link_tool_child(
    state: &mut SessionState,
    item_id: u64,
    row: &PendingToolRow,
) -> bool {
    let (turn_seq, content_index, call_id) = (row.turn_seq, row.content_index, row.call_id);
    let Some(TranscriptItem::Assistant {
        version,
        children,
        child_times,
        ..
    }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == item_id)
    else {
        return false;
    };
    let already = children.iter().any(
        |child| matches!(child, AssistantChild::Tool { call_id: existing } if *existing == call_id),
    );
    if already {
        return true;
    }
    let timed = child_times.len() == children.len();
    for (index, child) in children.iter_mut().enumerate() {
        if let AssistantChild::CommittedTool {
            turn_seq: placeholder_turn,
            content_index: placeholder,
            ..
        } = child
            && *placeholder_turn == turn_seq
            && *placeholder == content_index
        {
            *child = AssistantChild::Tool { call_id };
            // A started tool row belongs where the call started running, not
            // where the turn committed it (a queued call can start long
            // after its siblings).
            if timed {
                child_times[index] = child_times[index].max(row.started_at);
            }
            *version = version.wrapping_add(1);
            return true;
        }
    }
    false
}

/// Remove one pending lane entry after a promotion (oldest position) or a
/// recall (newest). The event's text correlates the entry; the FIFO
/// position is the fallback should payloads and lane ever diverge, so the
/// lane never strands an entry the engine says is gone.
/// Run end voids every still-pending steered input without per-entry
/// events: move their text aside so the UI can restore it into the composer
/// rather than ever losing it.
pub(super) fn void_pending_inputs(state: &mut SessionState) {
    let drained = state
        .pending_inputs
        .drain(..)
        .map(|pending| VoidedInput {
            seq: pending.admission_seq,
            origin: pending.origin,
            parked: false,
            input: pending.composer,
        })
        .collect::<Vec<_>>();
    state.voided_inputs.extend(drained);
}

/// Records that the assistant block `item_id` stopped taking output at the
/// durable time `at`. The first end recorded stands; a cancelled or
/// interrupted run's end marks a block it already ended as stopped too.
pub(super) fn seal_assistant(
    state: &mut SessionState,
    item_id: u64,
    at: jiff::Timestamp,
    stopped: bool,
) {
    let end = state
        .assistant_ends
        .entry(item_id)
        .or_insert(crate::AssistantEnd { at, stopped });
    end.stopped |= stopped;
}

/// Seals the run's open block, if any: at the row that made a split
/// pending, since nothing after that row joined it, else at `at`.
pub(super) fn seal_run_assistant(state: &mut SessionState, at: jiff::Timestamp, stopped: bool) {
    let Some(projection) = state.open_run_assistant.as_ref() else {
        return;
    };
    let item_id = projection.item_id;
    let at = projection.split_at.unwrap_or(at);
    seal_assistant(state, item_id, at, stopped);
}

pub(super) fn close_open_assistant(state: &mut SessionState, sealed_at: jiff::Timestamp) {
    let Some(open) = state.open_assistant.take() else {
        return;
    };
    seal_open_thinking(&mut state.thinking_durations, open, sealed_at);
    if let Some(TranscriptItem::Assistant { version, .. }) = state
        .transcript
        .iter_mut()
        .rfind(|item| item.id() == open.item_id)
    {
        *version = version.wrapping_add(1);
    }
}

/// Record a sealed thinking part's elapsed time, derived from the durable
/// timestamps of the events that opened and sealed it. Non-thinking parts
/// and clock-skewed (negative) spans record nothing.
/// The duration of each reasoning part an attempt streamed, in order: from
/// its mark to the next part's mark, or to the commit at `end`.
pub(super) fn streamed_thinking_durations(
    marks: &[(StreamedOutputKind, jiff::Timestamp)],
    end: jiff::Timestamp,
) -> Vec<Option<Duration>> {
    marks
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| *kind == StreamedOutputKind::Reasoning)
        .map(|(index, (_, started))| {
            let ended = marks.get(index + 1).map_or(end, |(_, next)| *next);
            Duration::try_from(ended.duration_since(*started)).ok()
        })
        .collect()
}

pub(super) fn seal_open_thinking(
    thinking_durations: &mut HashMap<(u64, u64), Duration>,
    open: OpenAssistantProjection,
    sealed_at: jiff::Timestamp,
) {
    if open.kind != AssistantPartKind::Thinking {
        return;
    }
    let elapsed = sealed_at.duration_since(open.opened_at);
    let Ok(duration) = std::time::Duration::try_from(elapsed) else {
        return;
    };
    thinking_durations.insert((open.item_id, open.part_id), duration);
}

/// Append one assistant child with its durable time, keeping the parallel
/// times in step. Items whose times are already unknown stay unknown.
fn push_child(
    children: &mut Vec<AssistantChild>,
    child_times: &mut Vec<jiff::Timestamp>,
    child: AssistantChild,
    timestamp: jiff::Timestamp,
) {
    if child_times.len() == children.len() {
        child_times.push(timestamp);
    }
    children.push(child);
}

/// The memory tool call a `memory_changed` of `run_id` at `sequence` for
/// `url` directly follows: the one running call naming `url`, as the change
/// was recorded while it ran (a session runs one run at a time, and a
/// memory write runs alone in it; a call moved to the background does not
/// count), or the call of the same run whose termination is the event just
/// before it. `None` when no call matches, such as in a window of history
/// that starts at the change.
fn memory_change_call(
    state: &SessionState,
    run_id: Option<RunId>,
    sequence: u64,
    url: &str,
) -> Option<ToolCallId> {
    let names = |tool: &ToolCallState| {
        tool.presentation
            .primary_argument
            .as_ref()
            .is_some_and(|argument| argument.as_str().trim() == url)
    };
    let mut running = state
        .tools
        .values()
        .filter(|tool| tool.status == ToolStatus::Running && !tool.backgrounded && names(tool));
    let running = running
        .next()
        .map(|tool| tool.id)
        .filter(|_| running.next().is_none());
    running.or_else(|| {
        let (terminated_at, call, terminated_run) = state.last_tool_termination?;
        (terminated_run == run_id
            && terminated_at.checked_add(1) == Some(sequence)
            && state.tools.get(&call).is_some_and(names))
        .then_some(call)
    })
}

/// The plan of run `run`: its row replaced in place, or a new row for a
/// run that had none.
fn upsert_plan(
    state: &mut SessionState,
    run: Option<RunId>,
    entries: Vec<cookie_agent_protocol::PlanEntry>,
    timestamp: jiff::Timestamp,
) {
    let existing = state
        .transcript
        .iter_mut()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::Plan {
                run: item_run,
                version,
                entries: item_entries,
                ..
            } if *item_run == run => Some((version, item_entries)),
            _ => None,
        });
    match existing {
        Some((version, item_entries)) => {
            *item_entries = entries;
            *version = version.wrapping_add(1);
        }
        None => push_item(state, timestamp, |id| TranscriptItem::Plan {
            id,
            version: 0,
            run,
            entries,
        }),
    }
}

pub(super) fn push_item(
    state: &mut SessionState,
    timestamp: jiff::Timestamp,
    item: impl FnOnce(u64) -> TranscriptItem,
) {
    state.next_transcript_id = state.next_transcript_id.wrapping_add(1).max(1);
    let id = state.next_transcript_id;
    state.item_times.insert(id, timestamp);
    state.transcript.push(item(id));
}

/// Adds a row for input the engine gives the model beyond the
/// conversation. Like a skill loaded mid-turn, it sits between what came
/// before and what the run does next.
fn push_model_input(
    state: &mut SessionState,
    timestamp: jiff::Timestamp,
    seq: u64,
    kind: crate::model_input::ModelInputKind,
    header: String,
    text: String,
) {
    state.mark_event_split_pending(timestamp);
    push_item(state, timestamp, |id| TranscriptItem::ModelInput {
        id,
        version: 0,
        seq,
        kind,
        header,
        text,
    });
}

pub(super) fn push_event(
    state: &mut SessionState,
    level: EventLevel,
    text: String,
    timestamp: jiff::Timestamp,
) {
    // A warning-or-worse row injected while the run's block is open marks
    // it split-pending: the part streaming right now keeps streaming into
    // the existing block above the row, but the next new segment (part,
    // tool call, or attempt) opens a fresh block below it. That includes
    // rows between turns (a failed background delegate, a turn warning), so
    // later turns never render above them. Debug/Info rows (replay
    // decisions, commit notices, lifecycle chatter) never split, so a run's
    // turns otherwise keep sharing a block.
    if level >= EventLevel::Warning {
        state.mark_event_split_pending(timestamp);
    }
    // A warning or error identical to any earlier one (a retry loop hitting
    // the same provider error, every approval falling back off one dead
    // model) counts on that row instead of stacking a copy, however much
    // happened in between. The row moves down to this occurrence and takes
    // its time, so it stands where a fresh row would and nothing above it
    // happened later. Debug and info rows stay one per event: they mark
    // lifecycle points (a turn commit without usage reads the same every
    // turn) whose times anchor descendant rows.
    let existing = if level >= EventLevel::Warning {
        state.transcript.iter().rposition(|item| {
            matches!(
                item,
                TranscriptItem::Event {
                    level: existing_level,
                    text: existing_text,
                    ..
                } if *existing_level == level && *existing_text == text
            )
        })
    } else {
        None
    };
    if let Some(index) = existing {
        if let TranscriptItem::Event {
            id,
            version,
            repeat,
            ..
        } = &mut state.transcript[index]
        {
            *repeat = repeat.saturating_add(1);
            *version = version.wrapping_add(1);
            state.item_times.insert(*id, timestamp);
        }
        move_transcript_item_to_end(state, index);
        return;
    }
    push_item(state, timestamp, |id| TranscriptItem::Event {
        id,
        version: 0,
        level,
        text,
        repeat: 1,
    });
}

/// Rebind the attempt to a fresh assistant block after an interleaved event
/// row when its run's block is split-pending. Returns the item new segments
/// belong to. A still-empty, never-committed block simply moves below the
/// row instead of leaving a ghost block behind. The split-off block is
/// remembered so abandonment or the turn commit can prune the attempt's
/// uncommitted output there (the committed turn rebuilds canonically in the
/// newest block).
pub(super) fn split_assistant_if_pending(
    state: &mut SessionState,
    attempt_id: AttemptId,
    timestamp: jiff::Timestamp,
) -> Option<u64> {
    let attempt = state.attempts.get(&attempt_id)?.clone();
    let split = state.open_run_assistant.as_ref().is_some_and(|projection| {
        projection.split_pending
            && Some(projection.run_id) == attempt.run_id
            && projection.item_id == attempt.item_id
    });
    if !split {
        return Some(attempt.item_id);
    }
    close_open_assistant(state, timestamp);
    let Some(index) = state
        .transcript
        .iter()
        .rposition(|item| item.id() == attempt.item_id)
    else {
        return Some(attempt.item_id);
    };
    let TranscriptItem::Assistant {
        attribution,
        children,
        committed_turn_seq,
        ..
    } = &state.transcript[index]
    else {
        return Some(attempt.item_id);
    };
    let agent = attribution.agent.clone();
    let reuse_empty = children.is_empty() && committed_turn_seq.is_none();
    let mut projection = state
        .open_run_assistant
        .take()
        .expect("split-pending run projection");
    let split_at = projection.split_at.unwrap_or(timestamp);
    projection.clear_split();
    if reuse_empty {
        move_transcript_item_to_end(state, index);
        projection.committed_prefix = 0;
        state.open_run_assistant = Some(projection);
        return Some(attempt.item_id);
    }
    seal_assistant(state, attempt.item_id, split_at, false);
    let item_id = open_assistant_item(
        state,
        FrozenAssistantAttribution {
            acp_model: acp_model_label(state, &projection.current_model),
            agent,
            resolved_model: projection.current_model.clone(),
        },
        timestamp,
    );
    projection.item_id = item_id;
    projection.committed_prefix = 0;
    state.open_run_assistant = Some(projection);
    let mut split_segments = attempt.split_segments;
    split_segments.push((attempt.item_id, attempt.committed_prefix));
    state.attempts.insert(
        attempt_id,
        AttemptProjection {
            item_id,
            run_id: attempt.run_id,
            committed_prefix: 0,
            attribution_marker: None,
            split_segments,
        },
    );
    Some(item_id)
}

/// A split-pending run block that an abandoned attempt left empty (its
/// partials pruned, nothing ever committed) has nothing to keep above the
/// rows that split it: move it below them and let the new attempt continue
/// in it under its own model, instead of stranding an empty header.
pub(super) fn relocate_empty_split_block(
    state: &mut SessionState,
    run_id: RunId,
    resolved_model: &ResolvedModelRef,
) {
    let Some(item_id) = state
        .open_run_assistant
        .as_ref()
        .filter(|projection| projection.run_id == run_id && projection.split_pending)
        .map(|projection| projection.item_id)
    else {
        return;
    };
    let Some(index) = state.transcript.iter().rposition(|item| {
        matches!(
            item,
            TranscriptItem::Assistant {
                id,
                children,
                committed_turn_seq: None,
                ..
            } if *id == item_id && children.is_empty()
        )
    }) else {
        return;
    };
    if let TranscriptItem::Assistant {
        version,
        attribution,
        ..
    } = &mut state.transcript[index]
    {
        attribution.resolved_model = resolved_model.clone();
        *version = version.wrapping_add(1);
    }
    move_transcript_item_to_end(state, index);
    let projection = state
        .open_run_assistant
        .as_mut()
        .expect("located run projection");
    projection.clear_split();
    projection.committed_prefix = 0;
    projection.current_model = resolved_model.clone();
}

/// Prune one split-off block back to its committed prefix (keeping
/// attribution markers), then drop it entirely when nothing committed ever
/// landed there so no empty block is left behind.
pub(super) fn prune_split_segment(state: &mut SessionState, item_id: u64, committed_prefix: usize) {
    prune_abandoned_attempt(state, item_id, committed_prefix);
    let empty = state.transcript.iter().rev().any(|item| {
        matches!(
            item,
            TranscriptItem::Assistant {
                id,
                children,
                committed_turn_seq,
                ..
            } if *id == item_id && children.is_empty() && committed_turn_seq.is_none()
        )
    });
    if empty {
        state.transcript.retain(|item| item.id() != item_id);
        state.assistant_metrics.remove(&item_id);
    }
}

/// Prune the attempt's uncommitted output in every block an event row split
/// off before its current one.
pub(super) fn prune_split_segments(state: &mut SessionState, segments: &[(u64, usize)]) {
    for (item_id, committed_prefix) in segments {
        prune_split_segment(state, *item_id, *committed_prefix);
    }
}

/// The item a delta belongs to: the open part's item while the same kind
/// keeps streaming (even across an interleaved event row), otherwise the
/// split-pending rebind target for a new segment.
pub(super) fn assistant_segment_target(
    state: &mut SessionState,
    attempt_id: AttemptId,
    kind: AssistantPartKind,
    timestamp: jiff::Timestamp,
) -> Option<u64> {
    let attempt = state.attempts.get(&attempt_id)?;
    let continues_open_part = state
        .open_assistant
        .is_some_and(|open| open.item_id == attempt.item_id && open.kind == kind);
    if continues_open_part {
        return Some(attempt.item_id);
    }
    split_assistant_if_pending(state, attempt_id, timestamp)
}

pub(super) fn bump_tool_item(state: &mut SessionState, tool_call_id: ToolCallId) {
    if let Some(TranscriptItem::Assistant { version, children, .. }) = state
        .transcript
        .iter_mut()
        .rfind(|item| {
            matches!(item, TranscriptItem::Assistant { children, .. } if children.iter().any(
                |child| matches!(child, AssistantChild::Tool { call_id } if *call_id == tool_call_id)))
        })
    {
        let _ = children;
        *version = version.wrapping_add(1);
    }
}

pub(super) fn approval_outcome_label(outcome: ApprovalFinalOutcome) -> &'static str {
    match outcome {
        ApprovalFinalOutcome::Approved => "approved",
        ApprovalFinalOutcome::Rejected => "rejected",
        ApprovalFinalOutcome::Cancelled => "cancelled",
        ApprovalFinalOutcome::Expired => "expired",
    }
}

pub fn approval_state_from_record(record: ApprovalRecord) -> Option<ApprovalState> {
    match record.status {
        ApprovalStatus::Escalated => {}
        ApprovalStatus::Pending
        | ApprovalStatus::Approved
        | ApprovalStatus::Rejected
        | ApprovalStatus::Cancelled
        | ApprovalStatus::Expired => return None,
    }
    let mut approval = approval_state_from_request(record.session_id, record.request, true);
    approval.reviewer = record
        .internal_decision
        .as_ref()
        .and_then(crate::ReviewerNote::of);
    approval.is_visible_user_escalation().then_some(approval)
}

pub(super) fn approval_request_metadata(
    request: &cookie_agent_protocol::ApprovalRequest,
) -> (u64, ApprovalTrigger) {
    let wire = serde_json::to_value(request).expect("protocol approval request serializes");
    let revision = wire["revision"]
        .as_u64()
        .expect("protocol approval revision is an integer");
    let trigger = serde_json::from_value(wire["trigger"].clone())
        .expect("protocol approval trigger deserializes");
    (revision, trigger)
}

/// Serialized views over private approval identity fields. The wire form is
/// the exact durable protocol shape, so display projection stays honest
/// without new accessors.
pub(super) fn approval_operation_parts(
    operation: &cookie_agent_protocol::PreparedOperationIdentity,
) -> (Sha256Digest, Sha256Digest, PreparedCapabilityLifetime) {
    let wire = serde_json::to_value(operation).expect("protocol operation serializes");
    let arguments = serde_json::from_value(wire["normalized_arguments_digest"].clone())
        .expect("arguments digest deserializes");
    let context = serde_json::from_value(wire["execution_context_digest"].clone())
        .expect("context digest deserializes");
    let lifetime = serde_json::from_value(wire["capability_lifetime"].clone())
        .expect("capability lifetime deserializes");
    (arguments, context, lifetime)
}

pub(super) fn approval_request_parts(
    request: &ApprovalRequest,
) -> (Vec<ApprovalEvaluation>, ApprovalConstraints) {
    let wire = serde_json::to_value(request).expect("protocol approval request serializes");
    let evaluations =
        serde_json::from_value(wire["evaluations"].clone()).expect("evaluations deserialize");
    let constraints =
        serde_json::from_value(wire["constraints"].clone()).expect("constraints deserialize");
    (evaluations, constraints)
}

pub(super) fn approval_state_from_request(
    session_id: SessionId,
    request: ApprovalRequest,
    escalated: bool,
) -> ApprovalState {
    let (request_revision, trigger) = approval_request_metadata(&request);
    let operation = request.operation();
    let (normalized_arguments_digest, execution_context_digest, capability_lifetime) =
        approval_operation_parts(operation);
    let (evaluations, constraints) = approval_request_parts(&request);
    ApprovalState {
        session_id,
        approval_id: request.approval_id(),
        request_revision,
        tool_call_id: request.tool_call_id(),
        operation_fingerprint: request.operation_fingerprint().clone(),
        trigger,
        normalized_arguments_digest,
        execution_context_digest,
        capability_lifetime,
        capabilities: operation.capabilities().to_vec(),
        resources: operation.resources().to_vec(),
        evaluations,
        constraints,
        escalated,
        reviewer: None,
    }
}

pub(super) fn render_model(model: &ResolvedModelRef) -> String {
    let variant = model
        .selection
        .variant
        .as_ref()
        .map_or_else(|| "base".to_owned(), |variant| variant.to_string());
    format!(
        "{}/{} ({variant}, {})",
        model.provider_id,
        model.model_id,
        model.adapter_id.as_str()
    )
}

pub(super) fn render_usage(usage: &Usage) -> String {
    let value = |value: Option<u64>| value.map_or_else(|| "?".into(), |value| value.to_string());
    format!(
        "in {} [direct {}, cache read {}, cache write {}], out {} [text {}, thinking {}]",
        value(usage.input_tokens),
        value(usage.input_tokens_no_cache),
        value(usage.input_tokens_cache_read),
        value(usage.input_tokens_cache_write),
        value(usage.output_tokens),
        value(usage.output_tokens_text),
        value(usage.output_tokens_reasoning)
    )
}

pub(super) fn render_replay_decision(decision: &ReplayDecision) -> (EventLevel, String) {
    let (level, disposition) = match &decision.disposition {
        ReplayDisposition::Replayed => (EventLevel::Debug, "replayed".into()),
        ReplayDisposition::NoArtifact => (EventLevel::Debug, "no artifact".into()),
        ReplayDisposition::DiscardedForeignAdapter { found, expected } => (
            EventLevel::Warning,
            format!(
                "discarded foreign adapter {found} (expected {})",
                expected.as_str()
            ),
        ),
        ReplayDisposition::DiscardedForeignModelSelection { found, expected } => (
            EventLevel::Warning,
            format!(
                "discarded foreign model selection {}/{} (expected {}/{})",
                found.model,
                found
                    .variant
                    .as_ref()
                    .map_or("base".into(), |variant| variant.to_string()),
                expected.model,
                expected
                    .variant
                    .as_ref()
                    .map_or("base".into(), |variant| variant.to_string())
            ),
        ),
        ReplayDisposition::DiscardedForeignVariant { found, expected } => (
            EventLevel::Warning,
            format!(
                "discarded foreign variant {} (expected {})",
                found
                    .as_ref()
                    .map_or("base".into(), |variant| variant.to_string()),
                expected
                    .as_ref()
                    .map_or("base".into(), |variant| variant.to_string())
            ),
        ),
        ReplayDisposition::DiscardedInvalidPayload { reason } => (
            EventLevel::Warning,
            format!("discarded invalid payload: {reason}"),
        ),
        ReplayDisposition::ReconstructedNormalizedHistory => {
            (EventLevel::Debug, "reconstructed normalized history".into())
        }
    };
    (level, format!("#{} {disposition}", decision.history_index))
}

pub(super) fn replay_context_warning_key(
    model: &ResolvedModelRef,
    decision: &ReplayDecision,
) -> Option<ReplayContextWarningKey> {
    let transition = match &decision.disposition {
        ReplayDisposition::DiscardedForeignAdapter { found, expected } => {
            ReplayContextTransition::Adapter {
                found: found.clone(),
                expected: expected.clone(),
            }
        }
        ReplayDisposition::DiscardedForeignModelSelection { found, expected } => {
            ReplayContextTransition::ModelSelection {
                found: found.clone(),
                expected: expected.clone(),
            }
        }
        ReplayDisposition::DiscardedForeignVariant { found, expected } => {
            ReplayContextTransition::Variant {
                found: found.clone(),
                expected: expected.clone(),
            }
        }
        ReplayDisposition::DiscardedInvalidPayload { reason } => {
            ReplayContextTransition::InvalidPayload {
                reason: reason.to_string(),
            }
        }
        _ => return None,
    };
    Some(ReplayContextWarningKey {
        selection_fingerprint: model.selection_fingerprint.clone(),
        transition,
    })
}

pub struct ReplayContextWarningKey {
    selection_fingerprint: Sha256Digest,
    transition: ReplayContextTransition,
}

impl ReplayContextWarningKey {
    fn with_run(self, run_id: RunId) -> (RunId, Sha256Digest, ReplayContextTransition) {
        (run_id, self.selection_fingerprint, self.transition)
    }
}

pub(super) fn render_model_error(error: &ModelErrorSummary) -> String {
    cookie_agent_protocol::diagnostics::model(error)
}

/// Locate the durable tool input for an ownership reference from the
/// referenced committed turn's content; used only for the expanded row.
pub(super) fn find_tool_call_content(
    state: &SessionState,
    model_turn_seq: u64,
    model_call_id: &cookie_agent_protocol::ModelCallId,
) -> Option<String> {
    state
        .turn_tool_index
        .get(&(model_turn_seq, model_call_id.as_str().to_owned()))
        .map(|tool| tool.arguments.clone())
}

pub(super) fn render_internal_backend(
    backend: &cookie_agent_protocol::InternalAgentBackend,
) -> String {
    match backend {
        cookie_agent_protocol::InternalAgentBackend::Model { resolved_model } => {
            render_model(resolved_model)
        }
        cookie_agent_protocol::InternalAgentBackend::Builtin { name, revision } => {
            format!("builtin {name}@{revision}")
        }
    }
}

pub(super) fn render_title_commit(change: &SessionTitleChange) -> String {
    match change {
        SessionTitleChange::UserSet { title, .. } => format!("session renamed to {title}"),
        SessionTitleChange::UserClear { .. } => "session title cleared".into(),
        SessionTitleChange::UserReset { .. } => "session title reset".into(),
        SessionTitleChange::InternalAgentSet { title, .. } => {
            format!("session title set to {title}")
        }
        SessionTitleChange::DelegatedSet { title, .. } => {
            format!("delegated session titled {title}")
        }
        SessionTitleChange::FallbackSet { title } => format!("session title set to {title}"),
    }
}

/// The event row a run's end shows. An outcome this build does not know
/// reads as interrupted.
fn run_finished_message(
    outcome: &cookie_agent_protocol::Open<cookie_agent_protocol::RunOutcome>,
) -> (EventLevel, String) {
    use cookie_agent_protocol::RunOutcome;
    match outcome.known() {
        Some(RunOutcome::Completed { .. }) => (EventLevel::Info, "run completed".into()),
        Some(RunOutcome::Failed {
            error,
            model_error,
            resolved_model,
        }) => (
            EventLevel::Error,
            format!(
                "run failed: {}",
                cookie_agent_protocol::diagnostics::run_error(
                    error,
                    model_error.as_ref(),
                    resolved_model.as_ref()
                )
            ),
        ),
        Some(RunOutcome::Cancelled { reason }) => (
            EventLevel::Info,
            reason.as_ref().map_or_else(
                || "run cancelled".into(),
                |reason| format!("run cancelled: {reason}"),
            ),
        ),
        Some(RunOutcome::Interrupted { reason }) => (
            EventLevel::Error,
            reason.as_ref().map_or_else(
                || "run interrupted".into(),
                |reason| format!("run interrupted: {reason}"),
            ),
        ),
        None => (EventLevel::Error, "run interrupted".into()),
    }
}

/// The event row an internal agent's end shows. An outcome this build does
/// not know reads as interrupted.
fn internal_agent_finished_message(
    kind: cookie_agent_protocol::InternalAgentKind,
    outcome: &cookie_agent_protocol::Open<cookie_agent_protocol::InternalAgentOutcome>,
) -> (EventLevel, String) {
    use cookie_agent_protocol::InternalAgentOutcome;
    match outcome.known() {
        Some(InternalAgentOutcome::Completed { result }) => (
            EventLevel::Info,
            format!(
                "internal agent {kind:?} completed: {}",
                result.output_summary
            )
            .to_lowercase(),
        ),
        Some(InternalAgentOutcome::Failed { failure }) => (
            EventLevel::Error,
            format!(
                "internal agent {kind:?} failed: {}",
                cookie_agent_protocol::diagnostics::internal(failure)
            ),
        ),
        Some(InternalAgentOutcome::Cancelled { reason }) => (
            EventLevel::Info,
            reason.as_ref().map_or_else(
                || format!("internal agent {kind:?} cancelled").to_lowercase(),
                |reason| format!("internal agent {kind:?} cancelled: {reason}").to_lowercase(),
            ),
        ),
        Some(InternalAgentOutcome::Interrupted { reason }) => (
            EventLevel::Error,
            reason.as_ref().map_or_else(
                || format!("internal agent {kind:?} interrupted").to_lowercase(),
                |reason| format!("internal agent {kind:?} interrupted: {reason}").to_lowercase(),
            ),
        ),
        None => (
            EventLevel::Error,
            format!("internal agent {kind:?} interrupted").to_lowercase(),
        ),
    }
}

/// The header label of an attempt by `resolved_model`: an ACP agent's own
/// model and effort from the options its session holds as the attempt
/// starts, which `acp_session_updated` recorded before it.
fn acp_model_label(state: &SessionState, resolved_model: &ResolvedModelRef) -> Option<String> {
    crate::acp::is_acp_model(&resolved_model.selection)
        .then(|| crate::acp::model_label(&state.acp.options))
        .flatten()
}

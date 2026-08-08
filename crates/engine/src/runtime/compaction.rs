use super::*;

pub(super) const COMPACTION_INSTRUCTION: &str = "Create a detailed technical summary of the conversation so work can continue without the earlier context. Include: the goal/objective; decisions and their rationale; files changed and current code state; commands run and their outcomes; errors encountered and fixes applied; and the pending next step. Preserve exact identifiers, paths, constraints, and unresolved questions. Return summary text only and do not call tools.";
pub(super) const TOOL_OUTPUT_ELISION_MIN_BYTES: usize = 8 * 1024;
pub(super) const REHYDRATION_MAX_FILES: usize = 5;
pub(super) const REHYDRATION_MAX_FILE_BYTES: usize = 32 * 1024;
pub(super) const REHYDRATION_MAX_TOTAL_BYTES: usize = 128 * 1024;

pub(super) struct CompactionInput<'a> {
    pub(super) session: SessionId,
    pub(super) run: RunId,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) binding: &'a cookie_agent_protocol::FrozenModelBinding,
    pub(super) internal_policy: &'a FrozenInternalAgentPolicy,
    pub(super) tools: &'a [ToolDefinition],
    pub(super) events: Vec<StoredEvent>,
    pub(super) force: bool,
    pub(super) focus: Option<&'a str>,
    pub(super) actor_direct: bool,
}

impl Engine {
    pub async fn compact_session(
        &self,
        session: SessionId,
        focus: Option<&str>,
    ) -> Result<bool, EngineError> {
        let projection = self.inner.store.get(session)?;
        if projection.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(session));
        }
        let events = projection.log.events();
        let run = events
            .iter()
            .rev()
            .find_map(|event| {
                matches!(event.payload, Event::RunStarted { .. }).then_some(event.run_id)
            })
            .flatten()
            .ok_or(EngineError::NoRunnableModel)?;
        let policy = self.historical_title_policy(&events, run)?;
        let binding = policy
            .selected_suffix
            .first()
            .ok_or(EngineError::NoRunnableModel)?;
        let internal_policy = self.internal_agent_policy(
            InternalAgentKind::ContextCompaction,
            &policy,
            Some(binding),
        )?;
        let tools = self.tool_definitions(session, &policy)?;
        let before = latest_checkpoint_seq(&events);
        let compacted = self
            .maybe_compact_context(CompactionInput {
                session,
                run,
                cancellation: &CancellationToken::new(),
                binding,
                internal_policy: &internal_policy,
                tools: &tools,
                events,
                force: true,
                focus,
                actor_direct: false,
            })
            .await?;
        Ok(latest_checkpoint_seq(&compacted) > before)
    }

    pub(super) async fn maybe_compact_context(
        &self,
        input: CompactionInput<'_>,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let Some(context_limit) = input.binding.descriptor.capabilities.limits.context else {
            return Ok(input.events);
        };
        let config = &self.inner.config.runtime.context_compaction;
        let trigger_tokens = effective_compaction_limit(context_limit, config.buffer_tokens);
        let auto_disabled = self
            .inner
            .compaction_auto_disabled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&input.session);
        if !compaction_gate(
            input.force,
            config.auto_compaction,
            auto_disabled,
            trigger_tokens,
        ) {
            return Ok(input.events);
        }
        if !input.force {
            let Some((usage_seq, observed_tokens)) = latest_real_usage(&input.events) else {
                return Ok(input.events);
            };
            let last_checkpoint_seq = latest_checkpoint_seq(&input.events);
            if usage_seq < last_checkpoint_seq {
                return Ok(input.events);
            }
            let pending_postcheck = if usage_seq > last_checkpoint_seq {
                self.inner
                    .compaction_postcheck_pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&input.session)
            } else {
                false
            };
            if observed_tokens < trigger_tokens {
                return Ok(input.events);
            }
            if should_latch_auto_compaction(
                pending_postcheck,
                usage_seq,
                last_checkpoint_seq,
                observed_tokens,
                trigger_tokens,
            ) {
                self.inner
                    .compaction_auto_disabled
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(input.session);
                self.append_compaction_event(
                    input.session,
                    Some(input.run),
                    Event::ContextCompactionAutoDisabled {
                        observed_tokens,
                        trigger_tokens,
                    },
                    input.actor_direct,
                )
                .await?;
                return Ok(self.inner.store.get(input.session)?.log.events());
            }
        }

        let mut events = self
            .stage_tool_output_elision(input.session, input.events, input.actor_direct)
            .await?;
        let composed_prompt = self.run_agent_prompt(input.session, input.run)?;
        let context = assemble_model_context(
            &events,
            &self.inner.artifacts,
            input.binding,
            &composed_prompt,
        )?;
        let input_tokens_before = estimated_request_tokens(&context.history, input.tools)?;
        if !input.force && input_tokens_before < trigger_tokens {
            return Ok(events);
        }

        let input_through_seq = events.last().map_or(0, |event| event.seq);
        if events.iter().rev().any(|event| {
            matches!(
                &event.payload,
                Event::ContextCheckpointCommitted { commit }
                    if commit.boundaries.input_through_seq >= input_through_seq
            )
        }) {
            return Ok(events);
        }
        let previous = latest_checkpoint_seq(&events);
        let source_from_seq = if previous == 0 {
            1
        } else {
            previous.saturating_add(1)
        };
        let boundaries = ContextCheckpointBoundaries {
            source_from_seq,
            source_through_seq: input_through_seq,
            input_through_seq,
            prior_checkpoint_seq: (previous > 0).then_some(previous),
        };
        let summary_limit = SummaryByteLimit::new(config.max_summary_bytes as u64)
            .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?;
        let (history, instruction) = compaction_history(context.history, input.focus);
        let summary = self
            .run_internal_history_agent(
                input.session,
                Some(input.run),
                InternalAgentKind::ContextCompaction,
                input.internal_policy,
                InternalAgentHistoryInput {
                    history,
                    summary_source: instruction,
                    tools: input.tools.to_vec(),
                    reject_non_text: true,
                },
                InternalAgentExecution {
                    cancellation: input.cancellation,
                    actor_direct: input.actor_direct,
                },
            )
            .await;
        let Ok(summary) = summary else {
            return Ok(events);
        };
        if summary.text.trim().is_empty() {
            return Ok(events);
        }
        let checkpoint = InternalSummaryCheckpoint::new(
            summary.text,
            summary.invocation_id,
            summary.internal_run_id,
            summary_limit,
        )
        .map_err(|error| EngineError::from(ModelError::invalid_response(error.to_string())))?;
        let input_tokens_after = estimated_tokens_for_bytes(
            model_history::framed_compaction_summary(checkpoint.summary()).len(),
        );
        let budgets = ContextCheckpointBudgets {
            context_limit_tokens: context_limit,
            trigger_tokens: trigger_tokens.max(1).min(context_limit),
            input_tokens_before,
            input_tokens_after,
            max_summary_bytes: summary_limit,
        };
        let commit = ContextCheckpointCommit {
            checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
            boundaries,
            budgets,
        };
        if commit.validate().is_err() {
            return Ok(events);
        }
        self.append_compaction_event(
            input.session,
            Some(input.run),
            Event::ContextCheckpointCommitted { commit },
            input.actor_direct,
        )
        .await?;
        self.inner
            .compaction_postcheck_pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(input.session);
        let files = rehydrated_files(&events, self.inner.store.cwd());
        if !files.is_empty() {
            self.append_compaction_event(
                input.session,
                Some(input.run),
                Event::ContextRehydrated { files },
                input.actor_direct,
            )
            .await?;
        }
        events = self.inner.store.get(input.session)?.log.events();
        Ok(events)
    }

    async fn stage_tool_output_elision(
        &self,
        session: SessionId,
        events: Vec<StoredEvent>,
        actor_direct: bool,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let protected_turns = events
            .iter()
            .rev()
            .filter_map(|event| match &event.payload {
                Event::ModelTurnCommitted { model_turn_seq, .. } => Some(*model_turn_seq),
                _ => None,
            })
            .take(2)
            .collect::<HashSet<_>>();
        let starts = events
            .iter()
            .filter_map(|event| match &event.payload {
                Event::ToolCallStarted { start } => {
                    Some((start.tool_call_id, start.owner.model_turn_seq))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let already_elided = events
            .iter()
            .filter_map(|event| match event.payload {
                Event::ToolOutputElided { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        for event in &events {
            let Event::ToolCallTerminated { termination } = &event.payload else {
                continue;
            };
            let Some(result) = &termination.result else {
                continue;
            };
            let Some(model_turn_seq) = starts.get(&termination.tool_call_id) else {
                continue;
            };
            if !should_elide_tool_output(
                *model_turn_seq,
                &protected_turns,
                already_elided.contains(&termination.tool_call_id),
                result.output.len(),
            ) {
                continue;
            }
            let (retained, _) = self.inner.artifacts.retain(result.output.as_bytes())?;
            self.append_compaction_event(
                session,
                event.run_id,
                Event::ToolOutputElided {
                    tool_call_id: termination.tool_call_id,
                    original_bytes: result.output.len() as u64,
                    retained,
                },
                actor_direct,
            )
            .await?;
        }
        Ok(self.inner.store.get(session)?.log.events())
    }

    async fn append_compaction_event(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
        actor_direct: bool,
    ) -> Result<(), EngineError> {
        if actor_direct {
            self.append_direct(session, run, event)
        } else {
            self.append(session, run, event).await
        }
    }
}

pub(super) fn effective_compaction_limit(context_limit: u64, buffer_tokens: u64) -> u64 {
    context_limit.saturating_sub(buffer_tokens)
}

fn compaction_gate(force: bool, auto: bool, auto_disabled: bool, trigger_tokens: u64) -> bool {
    force || (auto && !auto_disabled && trigger_tokens > 0)
}

fn latest_real_usage(events: &[StoredEvent]) -> Option<(u64, u64)> {
    events.iter().rev().find_map(|event| match &event.payload {
        Event::ModelTurnCommitted { turn, .. } => {
            let input = turn.usage.input_tokens;
            let output = turn.usage.output_tokens;
            (input.is_some() || output.is_some()).then(|| {
                (
                    event.seq,
                    input
                        .unwrap_or_default()
                        .saturating_add(output.unwrap_or_default()),
                )
            })
        }
        _ => None,
    })
}

fn latest_checkpoint_seq(events: &[StoredEvent]) -> u64 {
    events
        .iter()
        .rev()
        .find_map(|event| {
            matches!(event.payload, Event::ContextCheckpointCommitted { .. }).then_some(event.seq)
        })
        .unwrap_or(0)
}

fn estimated_request_tokens(
    history: &[oven_sdk::HistoryTurn],
    tools: &[ToolDefinition],
) -> Result<u64, EngineError> {
    let bytes = serde_json::to_vec(&(history, tools))
        .map_err(|error| EngineError::from(ModelError::invalid_request(error.to_string())))?
        .len();
    Ok(estimated_tokens_for_bytes(bytes))
}

fn estimated_tokens_for_bytes(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

fn compaction_instruction(focus: Option<&str>) -> String {
    focus.map_or_else(
        || COMPACTION_INSTRUCTION.to_owned(),
        |focus| format!("{COMPACTION_INSTRUCTION}\n\nUser-requested focus: {focus}"),
    )
}

fn compaction_history(
    mut history: Vec<oven_sdk::HistoryTurn>,
    focus: Option<&str>,
) -> (Vec<oven_sdk::HistoryTurn>, String) {
    let instruction = compaction_instruction(focus);
    history.push(oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(
        vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
            instruction.clone(),
        ))],
    )));
    (history, instruction)
}

fn should_elide_tool_output(
    model_turn_seq: u64,
    protected_turns: &HashSet<u64>,
    already_elided: bool,
    output_bytes: usize,
) -> bool {
    !protected_turns.contains(&model_turn_seq)
        && !already_elided
        && output_bytes >= TOOL_OUTPUT_ELISION_MIN_BYTES
}

fn should_latch_auto_compaction(
    pending_postcheck: bool,
    usage_seq: u64,
    checkpoint_seq: u64,
    observed_tokens: u64,
    trigger_tokens: u64,
) -> bool {
    pending_postcheck
        && usage_seq > checkpoint_seq
        && observed_tokens >= trigger_tokens
        && trigger_tokens > 0
}

fn rehydrated_files(events: &[StoredEvent], cwd: &Path) -> Vec<ContextRehydratedFile> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for event in events.iter().rev() {
        let Event::ToolCallTerminated { termination } = &event.payload else {
            continue;
        };
        let Some(result) = &termination.result else {
            continue;
        };
        let Some(path) = read_file_path(&result.output) else {
            continue;
        };
        if seen.insert(path.clone()) {
            paths.push(path);
        }
        if paths.len() == REHYDRATION_MAX_FILES {
            break;
        }
    }
    paths.reverse();
    load_rehydrated_files(paths, cwd)
}

fn load_rehydrated_files(paths: Vec<String>, cwd: &Path) -> Vec<ContextRehydratedFile> {
    let mut total = 0_usize;
    paths
        .into_iter()
        .filter_map(|display_path| {
            let path = Path::new(&display_path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            let mut file = fs::File::open(path).ok()?;
            let read_limit =
                REHYDRATION_MAX_FILE_BYTES.min(REHYDRATION_MAX_TOTAL_BYTES.saturating_sub(total));
            let mut bytes = Vec::with_capacity(read_limit);
            std::io::Read::by_ref(&mut file)
                .take(read_limit as u64)
                .read_to_end(&mut bytes)
                .ok()?;
            let text = std::str::from_utf8(&bytes).ok()?;
            let remaining = REHYDRATION_MAX_TOTAL_BYTES.saturating_sub(total);
            if remaining == 0 {
                return None;
            }
            let limit = REHYDRATION_MAX_FILE_BYTES.min(remaining).min(text.len());
            let mut boundary = limit;
            while boundary > 0 && !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            let content = text[..boundary].to_owned();
            total = total.saturating_add(content.len());
            Some(ContextRehydratedFile {
                path: safe_display(&display_path),
                byte_length: content.len() as u64,
                sha256: Sha256Digest::of_bytes(content.as_bytes()),
                content,
            })
        })
        .collect()
}

fn read_file_path(output: &str) -> Option<String> {
    let path = output.strip_prefix("<path>")?.split_once("</path>")?.0;
    output
        .contains("</path>\n<type>file</type>")
        .then(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_buffer_math_is_saturating() {
        assert_eq!(effective_compaction_limit(200_000, 33_000), 167_000);
        assert_eq!(effective_compaction_limit(8_192, 33_000), 0);
    }

    #[test]
    fn auto_off_blocks_automatic_compaction_but_not_manual_force() {
        assert!(!compaction_gate(false, false, false, 100));
        assert!(compaction_gate(true, false, true, 0));
    }

    #[test]
    fn focus_is_appended_without_changing_the_fixed_instruction() {
        assert_eq!(compaction_instruction(None), COMPACTION_INSTRUCTION);
        assert_eq!(
            compaction_instruction(Some("preserve parser work")),
            format!("{COMPACTION_INSTRUCTION}\n\nUser-requested focus: preserve parser work")
        );
    }

    #[test]
    fn compact_history_is_the_normal_prefix_plus_one_user_instruction() {
        let normal = vec![
            oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
                oven_sdk::SystemPart::Text(oven_sdk::TextPart::new("system")),
            ])),
            oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![
                oven_sdk::InputPart::Text(oven_sdk::TextPart::new("work")),
            ])),
        ];
        let tools = vec![ToolDefinition::new(
            "read",
            "Read a file",
            JsonSchema::new(serde_json::json!({
                "type": "object",
                "properties": {"filePath": {"type": "string"}},
                "required": ["filePath"]
            }))
            .expect("schema"),
        )];
        let normal_request = ModelRequest::new(normal.clone()).with_tools(tools.clone());
        let (compact, _) = compaction_history(normal, None);
        let compact_request = ModelRequest::new(compact).with_tools(tools);

        assert_eq!(
            serde_json::to_vec(&normal_request.history).unwrap(),
            serde_json::to_vec(&compact_request.history[..normal_request.history.len()]).unwrap()
        );
        assert_eq!(normal_request.tools, compact_request.tools);
        assert_eq!(
            compact_request.history.len(),
            normal_request.history.len() + 1
        );
    }

    #[test]
    fn elision_protects_recent_turns_and_requires_bulky_output() {
        let protected = HashSet::from([8, 9]);
        assert!(!should_elide_tool_output(
            9,
            &protected,
            false,
            TOOL_OUTPUT_ELISION_MIN_BYTES
        ));
        assert!(!should_elide_tool_output(
            7,
            &protected,
            false,
            TOOL_OUTPUT_ELISION_MIN_BYTES - 1
        ));
        assert!(should_elide_tool_output(
            7,
            &protected,
            false,
            TOOL_OUTPUT_ELISION_MIN_BYTES
        ));
    }

    #[test]
    fn anti_thrash_latches_only_for_the_first_over_limit_postcheck() {
        assert!(should_latch_auto_compaction(true, 11, 10, 90, 80));
        assert!(!should_latch_auto_compaction(false, 11, 10, 90, 80));
        assert!(!should_latch_auto_compaction(true, 11, 10, 79, 80));
    }

    #[test]
    fn rehydration_loads_recent_files_and_skips_missing() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("present.rs"), "fn present() {}\n").unwrap();
        let files = load_rehydrated_files(
            vec!["missing.rs".into(), "present.rs".into()],
            directory.path(),
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.as_str(), "present.rs");
        assert_eq!(files[0].content, "fn present() {}\n");
    }

    #[test]
    fn read_path_parser_accepts_only_file_results() {
        assert_eq!(
            read_file_path("<path>src/lib.rs</path>\n<type>file</type>\n<content>\nx\n</content>"),
            Some("src/lib.rs".into())
        );
        assert_eq!(
            read_file_path("<path>src</path>\n<type>directory</type>"),
            None
        );
    }
}

use super::artifacts::MAX_ATTACHMENT_BYTES;
use super::*;

impl Engine {
    pub(super) async fn prepare_tool_call(
        &self,
        session_id: SessionId,
        run: RunId,
        call: ToolCall,
        policy: &FrozenRunPolicy,
    ) -> PreparedToolCall {
        let fallback_presentation = safe_tool_presentation(&call);
        let session = match self.inner.store.get(session_id) {
            Ok(session) => session,
            Err(error) => {
                return PreparedToolCall {
                    call,
                    presentation: fallback_presentation,
                    prepared: Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: error.to_string(),
                    }),
                };
            }
        };
        let depth = session_depth(&session.meta.origin);
        let delegate_enabled = policy.agent.delegation.as_ref().is_some_and(|delegation| {
            depth < delegation.effective_depth_ceiling
                && delegation.targets.iter().any(|target| {
                    policy.registry.get(target).is_some_and(|agent| {
                        agent.document.frontmatter.enabled
                            && matches!(
                                agent.document.frontmatter.mode,
                                cookie_agent_config::AgentMode::Subagent
                                    | cookie_agent_config::AgentMode::All
                            )
                    })
                })
        });
        let enabled_tools = policy.tools();
        if (call.name == "delegate" && !delegate_enabled)
            || (call.name != "delegate"
                && (!enabled_tools.contains(&call.name)
                    || !PermissionPipeline::tool_visible(&policy.agent, &call.name)))
        {
            return PreparedToolCall {
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is not enabled for this session", call.name),
                }),
                call,
                presentation: fallback_presentation,
            };
        }
        let providers = self
            .inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let provider = providers.into_iter().find(|provider| {
            provider
                .tools_for_session(&SessionToolContext {
                    session: session_id,
                })
                .ok()
                .is_some_and(|tools| tools.iter().any(|tool| tool.name == call.name))
        });
        let Some(provider) = provider else {
            return PreparedToolCall {
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is unavailable", call.name),
                }),
                call,
                presentation: fallback_presentation,
            };
        };
        let presentation = provider.presentation(&call);
        let context = ToolPreparationContext {
            session: session_id,
            run,
            cwd: self.inner.store.cwd().to_owned(),
            workspace_root: self.inner.store.cwd().to_owned(),
        };
        let prepared = provider
            .prepare(context, call.clone())
            .await
            .map_err(Into::into);
        PreparedToolCall {
            call,
            presentation,
            prepared,
        }
    }

    pub(super) async fn execute_tool(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        prepared: PreparedToolCall,
    ) -> Result<ToolResult, ToolFailure> {
        let engine = self.clone();
        {
            let PreparedToolCall { call, prepared, .. } = prepared;
            let prepared = prepared?;
            let operation = prepared.operation.clone();
            let policy_labels = prepared.policy_labels.clone();
            let _serialization_guard = if let Some(key) = &prepared.serialization_key {
                Some(engine.mutation_lock(key).lock_owned().await)
            } else {
                None
            };
            let permission = engine.inner.permissions.decide_operation(
                &active.policy.agent,
                &operation,
                &policy_labels,
                engine.inner.store.cwd(),
            );
            if permission.effect != cookie_agent_protocol::PermissionEffect::Allow {
                if permission.effect == cookie_agent_protocol::PermissionEffect::Ask {
                    let allow_tree_grant = operation.resources().iter().all(|resource| {
                        resource.binding_lifetime
                            == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
                            && !matches!(
                                resource.capability,
                                cookie_agent_protocol::PermissionAction::Read
                                    | cookie_agent_protocol::PermissionAction::Write
                                    | cookie_agent_protocol::PermissionAction::Grep
                                    | cookie_agent_protocol::PermissionAction::Glob
                                    | cookie_agent_protocol::PermissionAction::ExternalDirectory
                            )
                    });
                    let approval_policy = engine
                        .active_internal_policy(&active, InternalAgentKind::Approval)
                        .map_err(|error| ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: error.to_string(),
                        })?;
                    let request = ApprovalRequest::new(
                        ApprovalId::new_v7(),
                        1,
                        ApprovalTrigger::PermissionPolicy,
                        operation.clone(),
                        permission.evaluations.clone(),
                        ApprovalConstraints {
                            allow_once: true,
                            allow_tree_grant,
                            cancellable: true,
                            expires_at: approval_expiry(approval_policy.limits.timeout_ms),
                        },
                    )
                    .map_err(|error| ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: error.to_string(),
                    })?;
                    let outcome = engine
                        .await_user_approval(
                            &active,
                            run,
                            request,
                            prepared.executor.clone(),
                            true,
                            ApprovalToolInput {
                                name: &call.name,
                                normalized_parameters: prepared.normalized_arguments(),
                            },
                        )
                        .await
                        .map_err(|error| ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: error.to_string(),
                        })?;
                    if !outcome.approved {
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: denied_tool_failure(
                                ApprovalDecisionSource::Policy,
                                "permission refused by user",
                                outcome.feedback,
                            ),
                        });
                    }
                } else {
                    return Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: denied_tool_failure(
                            ApprovalDecisionSource::Policy,
                            "permission denied",
                            None,
                        ),
                    });
                }
            }
            let executor = prepared
                .executor
                .lock()
                .await
                .take()
                .ok_or_else(|| ToolFailure {
                    code: ToolCallFailureCode::PreparedCapabilityLost,
                    message: "prepared executor capability was already consumed or lost".into(),
                })?;
            let (progress_tx, mut progress_rx) = mpsc::channel(64);
            let hub = engine
                .inner
                .output_hubs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(call.id)
                .or_insert_with(|| OutputHub::new(call.id, 64 * 1024))
                .clone();
            let interactive = call.name == "bash"
                && call
                    .arguments
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let capture = (call.name == "bash")
                .then(|| OutputCapture::new(engine.inner.artifacts.clone()))
                .transpose()
                .map_err(|error| ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool output capture setup failed: {error}"),
                })?;
            let (stdin_tx, stdin) = ToolStdin::channel(64);
            if interactive {
                active
                    .stdin
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(call.id, stdin_tx);
            }
            let invoke = executor.execute(ToolExecutionContext {
                session: active.session,
                run,
                progress: capture.as_ref().map_or_else(
                    || ProgressSink::new(progress_tx.clone(), hub.clone()),
                    |capture| {
                        ProgressSink::with_capture(
                            progress_tx.clone(),
                            hub.clone(),
                            capture.clone(),
                        )
                    },
                ),
                cancellation: active.cancellation.child_token(),
                stdin: interactive.then_some(stdin),
                artifacts: engine.inner.artifacts.clone(),
            });
            tokio::pin!(invoke);
            loop {
                tokio::select! {
                    result = &mut invoke => {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&call.id);
                        // Tool implementations drain their producers before
                        // resolving.  Finalizing here makes all emitted deltas
                        // precede the completion notification committed by the
                        // session actor.
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        return match result {
                            Ok(result) => {
                                let bounded = if let Some(capture) = &capture {
                                    capture.finish(
                                        result,
                                        active.policy.result_limits.tool_output_max_lines,
                                        active.policy.result_limits.tool_output_max_bytes,
                                    )
                                } else {
                                    bound_tool_result(
                                        result,
                                        &call.name,
                                        call.id,
                                        &engine.inner.artifacts,
                                        active.policy.result_limits.tool_output_max_lines,
                                        active.policy.result_limits.tool_output_max_bytes,
                                    )
                                };
                                bounded.map_err(|error| ToolFailure {
                                    code: ToolCallFailureCode::ExecutionFailed,
                                    message: error.to_string(),
                                })
                            }
                            Err(error) => {
                                if let Some(capture) = &capture {
                                    capture.discard();
                                }
                                Err(error.into())
                            }
                        };
                    }
                    Some(progress) = progress_rx.recv() => {
                        let _ = engine.append(active.session, Some(run), Event::ToolCallProgress { tool_call_id: progress.tool_call_id, message: safe_display(&progress.message) }).await;
                    }
                    _ = active.cancellation.cancelled() => {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&call.id);
                        if let Some(capture) = &capture {
                            capture.discard();
                        }
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message: "tool call cancelled after it started".into(),
                        });
                    }
                }
            }
        }
    }

    pub(super) fn tool_definitions(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<Vec<ToolDefinition>, EngineError> {
        let depth = session_depth(&self.inner.store.get(session)?.meta.origin);
        let delegate_enabled = policy.agent.delegation.as_ref().is_some_and(|delegation| {
            depth < delegation.effective_depth_ceiling
                && delegation.targets.iter().any(|target| {
                    policy.registry.get(target).is_some_and(|agent| {
                        agent.document.frontmatter.enabled
                            && matches!(
                                agent.document.frontmatter.mode,
                                cookie_agent_config::AgentMode::Subagent
                                    | cookie_agent_config::AgentMode::All
                            )
                    })
                })
        });
        let enabled_tools = policy.tools();
        let mut names = HashSet::new();
        let mut output = Vec::new();
        let providers = self
            .inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for provider in &providers {
            for tool in provider
                .tools_for_session(&SessionToolContext { session })
                .map_err(|error| EngineError::MissingTool(error.to_string()))?
            {
                if ((tool.name != "delegate"
                    && enabled_tools.contains(&tool.name)
                    && PermissionPipeline::tool_visible(&policy.agent, &tool.name))
                    || (tool.name == "delegate" && delegate_enabled))
                    && names.insert(tool.name.clone())
                {
                    let schema = JsonSchema::new(tool.parameters).map_err(|error| {
                        EngineError::MissingTool(format!(
                            "tool `{}` has invalid JSON Schema: {error}",
                            tool.name
                        ))
                    })?;
                    output.push(ToolDefinition::new(tool.name, tool.description, schema));
                }
            }
        }
        output.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(output)
    }
}

pub(super) fn fallback_operation_fingerprint(call: &ToolCall) -> OperationFingerprint {
    let action = PermissionPipeline::action_for_tool(&call.name)
        .unwrap_or(cookie_agent_protocol::PermissionAction::Read);
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(
            &serde_json::to_vec(&call.arguments).expect("tool arguments serialize"),
        ),
        vec![cookie_agent_protocol::ApprovalCapability {
            action,
            operation: cookie_agent_protocol::PreparedCapabilityOperation::new(format!(
                "{}:prepare",
                call.name
                    .replace(|character: char| !character.is_ascii_alphanumeric(), "_")
            ))
            .unwrap_or_else(|_| {
                cookie_agent_protocol::PreparedCapabilityOperation::new("tool:prepare")
                    .expect("static operation")
            }),
        }],
        Vec::new(),
        Sha256Digest::of_bytes(b"prepare-failed"),
    )
    .expect("fallback prepared identity");
    OperationFingerprint::from_prepared_operation(&operation)
}

pub(crate) fn safe_tool_presentation(call: &ToolCall) -> ToolCallPresentation {
    let primary = match call.name.as_str() {
        "bash" => call.arguments.get("command").and_then(Value::as_str),
        "read" | "write" | "edit" => call
            .arguments
            .get("filePath")
            .or_else(|| call.arguments.get("file_path"))
            .or_else(|| call.arguments.get("path"))
            .and_then(Value::as_str),
        "grep" => call.arguments.get("pattern").and_then(Value::as_str),
        "glob" => call.arguments.get("pattern").and_then(Value::as_str),
        "delegate" => {
            let agent = call.arguments.get("agent").and_then(Value::as_str);
            let task = call.arguments.get("task").and_then(Value::as_str);
            return ToolCallPresentation {
                title: safe_display(&call.name),
                primary_argument: agent.map(|agent| {
                    let agent = redact_presentation(agent);
                    let task = task
                        .map(|task| redact_presentation(&truncate_utf8(task, 160)))
                        .filter(|task| !task.is_empty());
                    safe_display(&sanitize_safe_text(
                        &task.map_or(agent.clone(), |task| format!("{agent}: {task}")),
                        SafeDisplayText::MAX_BYTES,
                    ))
                }),
            };
        }
        _ => None,
    }
    .map(redact_presentation);
    ToolCallPresentation {
        title: safe_display(&call.name),
        primary_argument: primary.as_deref().map(safe_display),
    }
}

pub(super) fn redact_presentation(value: &str) -> String {
    const SECRET_MARKERS: [&str; 6] = [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "authorization",
    ];
    let mut sanitized = sanitize_safe_text(value, 512);
    let lowercase = sanitized.to_ascii_lowercase();
    if SECRET_MARKERS
        .iter()
        .any(|marker| lowercase.contains(marker))
    {
        sanitized = "<redacted>".into();
    }
    sanitized
}
pub(super) struct TruncatedToolOutput {
    pub(super) content: String,
}

pub(super) fn truncate_tool_output(
    output: &str,
    max_lines: usize,
    max_bytes: usize,
) -> Option<TruncatedToolOutput> {
    let lines = output.split('\n').collect::<Vec<_>>();
    let line_truncated = lines.len() > max_lines;
    let mut preview = lines
        .iter()
        .take(max_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let byte_truncated = output.len() > max_bytes || preview.len() > max_bytes;
    if !line_truncated && !byte_truncated {
        return None;
    }
    if preview.len() > max_bytes {
        let mut boundary = max_bytes;
        while boundary > 0 && !preview.is_char_boundary(boundary) {
            boundary -= 1;
        }
        preview.truncate(boundary);
    }
    Some(TruncatedToolOutput { content: preview })
}

pub(super) fn bound_tool_result(
    mut result: ToolResult,
    _tool_name: &str,
    _call_id: ToolCallId,
    artifacts: &ArtifactStore,
    max_lines: usize,
    max_bytes: usize,
) -> std::io::Result<ToolResult> {
    let Some(preview) = truncate_tool_output(&result.output, max_lines, max_bytes) else {
        return Ok(result);
    };
    let original_bytes = result.output.len() as u64;
    let original_lines = result.output.split('\n').count() as u64;
    let (retained, _) = artifacts.retain(result.output.as_bytes())?;
    result.output = preview.content;
    result.truncation = Some(ToolOutputTruncation {
        original_bytes,
        original_lines,
        retained,
    });
    Ok(result)
}

pub(crate) fn validate_attachment(
    mime_type: &str,
    path: &Path,
    bytes: &[u8],
) -> Result<(), ToolError> {
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(ToolError::resource_limit(format!(
            "attachment is {} bytes; the limit is {MAX_ATTACHMENT_BYTES} bytes",
            bytes.len()
        )));
    }
    let validated = approved_media_type(path, bytes)?
        .ok_or_else(|| ToolError::execution("attachment is not a supported image or PDF"))?;
    if validated != mime_type {
        return Err(ToolError::execution(format!(
            "attachment MIME mismatch: declared {mime_type}, validated {validated}"
        )));
    }
    Ok(())
}

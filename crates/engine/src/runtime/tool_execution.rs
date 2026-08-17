use std::{collections::HashSet, path::Path, sync::Arc};

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalDecisionSource, ApprovalId, ApprovalRequest, ApprovalTrigger,
    InternalAgentKind, OperationFingerprint, PersistedToolResult as ToolResult,
    PreparedOperationIdentity, RunId, SessionId, Sha256Digest, ToolCallId, ToolCallPresentation,
    ToolOutputTruncation,
};
use oven_sdk::{JsonSchema, ToolDefinition};
use serde_json::Value;
use tokio::sync::mpsc;

use super::artifacts::MAX_ATTACHMENT_BYTES;
use super::{
    ActiveRun, ApprovalToolInput, Engine, EngineError, Event, PreparedToolCall, PublishedTool,
    PublishedToolSet, ToolCallFailureCode, ToolFailure,
    approval_flow::approval_expiry,
    approval_projection::denied_tool_failure,
    artifacts::{ArtifactStore, OutputCapture},
    helpers::{safe_display, sanitize_safe_text, session_depth},
};
use crate::{
    events::OutputHub,
    media::approved_media_type,
    permissions::PermissionPipeline,
    policy::FrozenRunPolicy,
    tool_api::{
        PreparedTool, ProgressSink, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
        ToolPreparationContext, ToolProvider, ToolStdin,
    },
};

impl Engine {
    pub(super) async fn prepare_tool_call(
        &self,
        session_id: SessionId,
        run: RunId,
        call: ToolCall,
        policy: &FrozenRunPolicy,
        turn_context: Arc<crate::TurnAgentContext>,
    ) -> PreparedToolCall {
        self.prepare_tool_call_with_publication(session_id, run, call, policy, turn_context, None)
            .await
    }

    pub(super) async fn prepare_published_tool_call(
        &self,
        session_id: SessionId,
        run: RunId,
        call: ToolCall,
        policy: &FrozenRunPolicy,
        turn_context: Arc<crate::TurnAgentContext>,
        published: Option<&PublishedTool>,
    ) -> PreparedToolCall {
        self.prepare_tool_call_with_publication(
            session_id,
            run,
            call,
            policy,
            turn_context,
            Some(published),
        )
        .await
    }

    async fn prepare_tool_call_with_publication(
        &self,
        session_id: SessionId,
        run: RunId,
        call: ToolCall,
        policy: &FrozenRunPolicy,
        turn_context: Arc<crate::TurnAgentContext>,
        published: Option<Option<&PublishedTool>>,
    ) -> PreparedToolCall {
        let fallback_presentation = tool_title_only(&call.name);
        let session = match self.inner.store.get(session_id) {
            Ok(session) => session,
            Err(error) => {
                return PreparedToolCall {
                    call,
                    permission_name: None,
                    presentation: fallback_presentation,
                    prepared: Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: error.to_string(),
                    }),
                };
            }
        };
        let depth = session_depth(&session.meta.origin);
        let permission_overlay = &session.permission_overlay;
        let grants = self.skill_grants_for_session(session_id);
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
        let providers = self
            .inner
            .tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let current = providers.iter().find_map(|provider| {
            provider
                .tools_for_session(&SessionToolContext {
                    session: session_id,
                })
                .ok()?
                .into_iter()
                .find(|tool| tool.name == call.name)
                .map(|tool| (provider.clone(), tool))
        });
        let mut provider = match published {
            Some(Some(published))
                if current.as_ref().is_some_and(|(provider, spec)| {
                    Arc::ptr_eq(provider, &published.provider) && spec == &published.spec
                }) =>
            {
                Some((
                    published.provider.clone(),
                    published.spec.permission_name.clone(),
                ))
            }
            Some(Some(_)) => {
                return PreparedToolCall {
                    prepared: Err(ToolFailure {
                        code: ToolCallFailureCode::OperationChanged,
                        message: format!(
                            "tool definition changed after `{}` was published to the model",
                            call.name
                        ),
                    }),
                    call,
                    permission_name: None,
                    presentation: fallback_presentation,
                };
            }
            Some(None) => {
                return PreparedToolCall {
                    prepared: Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: format!("tool `{}` was not published to the model", call.name),
                    }),
                    call,
                    permission_name: None,
                    presentation: fallback_presentation,
                };
            }
            None => current.map(|(provider, spec)| (provider, spec.permission_name)),
        };
        if published.is_none()
            && provider.is_none()
            && (call.name != "skill" || self.is_direct_skill_call(call.id))
        {
            provider = providers.iter().find_map(|candidate| {
                candidate
                    .permission_for_unlisted_tool(&call.name)
                    .ok()
                    .flatten()
                    .map(|permission_name| (candidate.clone(), permission_name.to_owned()))
            });
        }
        let Some((provider, permission_name)) = provider else {
            return PreparedToolCall {
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is unavailable", call.name),
                }),
                call,
                permission_name: None,
                presentation: fallback_presentation,
            };
        };
        let delegation_tool = permission_name == "delegate";
        let enabled = !delegation_tool || delegate_enabled;
        let direct_skill = call.name == "skill" && self.is_direct_skill_call(call.id);
        if !enabled
            || (!direct_skill
                && !PermissionPipeline::tool_visible_with_grants(
                    &policy.agent,
                    Some(permission_overlay),
                    grants.as_ref(),
                    &permission_name,
                    self.inner.store.cwd(),
                ))
        {
            return PreparedToolCall {
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is not enabled for this session", call.name),
                }),
                call,
                permission_name: Some(permission_name),
                presentation: fallback_presentation,
            };
        }
        let presentation = provider.presentation(&call);
        let context = ToolPreparationContext {
            session: session_id,
            run,
            cwd: self.inner.store.cwd().to_owned(),
            workspace_root: self.inner.store.cwd().to_owned(),
            turn_context,
        };
        let prepared = match provider.prepare(context, call.clone()).await {
            Ok(prepared) => {
                apply_permission_resource(provider.as_ref(), &call.name, &permission_name, prepared)
                    .map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        };
        PreparedToolCall {
            call,
            permission_name: Some(permission_name),
            presentation,
            prepared,
        }
    }

    pub(super) async fn execute_tool(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        prepared: PreparedToolCall,
        turn_context: Arc<crate::TurnAgentContext>,
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
            let permission_overlay = engine
                .inner
                .store
                .get(active.session)
                .map_err(|error| ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: error.to_string(),
                })?
                .permission_overlay;
            let grants = engine.skill_grants_for_session(active.session);
            let permission = engine.inner.permissions.decide_operation_with_grants(
                &active.policy.agent,
                Some(&permission_overlay),
                grants.as_ref(),
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
                turn_context,
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

    pub(crate) fn tool_definitions(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<Vec<ToolDefinition>, EngineError> {
        Ok(self
            .published_tool_definitions(session, policy)?
            .definitions)
    }

    pub(super) fn published_tool_definitions(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<PublishedToolSet, EngineError> {
        let session_projection = self.inner.store.get(session)?;
        let grants = self.skill_grants_for_session(session);
        let depth = session_depth(&session_projection.meta.origin);
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
        let mut names = HashSet::new();
        let mut output = Vec::new();
        let mut published = std::collections::HashMap::new();
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
                let delegation_tool = tool.permission_name == "delegate";
                let enabled = !delegation_tool || delegate_enabled;
                if enabled
                    && PermissionPipeline::tool_visible_with_grants(
                        &policy.agent,
                        Some(&session_projection.permission_overlay),
                        grants.as_ref(),
                        &tool.permission_name,
                        self.inner.store.cwd(),
                    )
                {
                    if !names.insert(tool.name.clone()) {
                        return Err(EngineError::MissingTool(format!(
                            "tool name `{}` is published by more than one provider",
                            tool.name
                        )));
                    }
                    let schema = JsonSchema::new(tool.parameters.clone()).map_err(|error| {
                        EngineError::MissingTool(format!(
                            "tool `{}` has invalid JSON Schema: {error}",
                            tool.name
                        ))
                    })?;
                    output.push(ToolDefinition::new(
                        tool.name.clone(),
                        tool.description.clone(),
                        schema,
                    ));
                    published.insert(
                        tool.name.clone(),
                        PublishedTool {
                            provider: provider.clone(),
                            spec: tool,
                        },
                    );
                }
            }
        }
        output.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(PublishedToolSet {
            definitions: output,
            tools: published,
        })
    }
}

pub(super) fn fallback_operation_fingerprint(
    call: &ToolCall,
    permission_name: Option<&str>,
) -> OperationFingerprint {
    let action = permission_name
        .or_else(|| fallback_permission_name(&call.name))
        .and_then(|name| PermissionPipeline::action_for_permission_name(name).ok())
        .unwrap_or(cookie_agent_protocol::PermissionAction::Read);
    let binding = serde_json::to_vec(&call.arguments).expect("tool arguments serialize");
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(&binding),
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
        vec![cookie_agent_protocol::PreparedApprovalResource {
            capability: action,
            canonical: cookie_agent_protocol::PreparedResourceIdentity::new("tool:prepare-failed")
                .expect("static resource identity"),
            binding_digest:
                cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(
                    &binding,
                ),
            binding_lifetime: cookie_agent_protocol::PreparedBindingLifetime::ProcessLocal,
            boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
            source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"prepare-failed"),
    )
    .expect("fallback prepared identity");
    OperationFingerprint::from_prepared_operation(&operation)
}

fn fallback_permission_name(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "read" => Some("read"),
        "write" | "edit" => Some("write"),
        "bash" => Some("bash"),
        "delegate_subagent" | "get_subagent_result" | "steer_subagent" | "cancel_subagent" => {
            Some("delegate")
        }
        "skill" => Some("skill"),
        _ => None,
    }
}

pub(crate) fn apply_permission_resource(
    provider: &dyn ToolProvider,
    name: &str,
    expected_permission_name: &str,
    prepared: PreparedTool,
) -> Result<PreparedTool, ToolError> {
    let (permission_name, resource) =
        provider.get_permission_resource(name, prepared.normalized_arguments())?;
    if permission_name != expected_permission_name
        && !(permission_name == "plugin" && expected_permission_name.starts_with("plugin:"))
    {
        return Err(ToolError::execution(
            "tool permission metadata changed between discovery and preparation",
        ));
    }
    let action = PermissionPipeline::action_for_permission_name(permission_name)
        .map_err(|error| ToolError::execution(error.to_string()))?;
    if prepared
        .operation()
        .resources()
        .iter()
        .any(|prepared| prepared.capability != action)
    {
        return Err(ToolError::execution(
            "prepared permission capability does not match provider metadata",
        ));
    }
    prepared.with_permission_resource(resource)
}

pub(crate) fn tool_title_only(name: &str) -> ToolCallPresentation {
    ToolCallPresentation {
        title: safe_display(name),
        primary_argument: None,
    }
}

pub(crate) fn tool_presentation(name: &str, primary: &str) -> ToolCallPresentation {
    ToolCallPresentation {
        title: safe_display(name),
        primary_argument: Some(safe_display(&redact_presentation(primary))),
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

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::ToolCallId;

    use super::{ToolCall, fallback_operation_fingerprint};

    #[test]
    fn discovery_failure_fingerprints_keep_known_tool_actions() {
        for (tool_name, permission_name) in [
            ("write", "write"),
            ("edit", "write"),
            ("bash", "bash"),
            ("delegate_subagent", "delegate"),
            ("get_subagent_result", "delegate"),
            ("steer_subagent", "delegate"),
            ("cancel_subagent", "delegate"),
        ] {
            let call = ToolCall {
                id: ToolCallId::new_v7(),
                name: tool_name.into(),
                arguments: serde_json::json!({"invalid":"prepare failure"}),
            };
            assert_eq!(
                fallback_operation_fingerprint(&call, None),
                fallback_operation_fingerprint(&call, Some(permission_name)),
                "{tool_name}"
            );
        }
    }
}

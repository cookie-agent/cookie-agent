use std::{collections::HashSet, path::Path, sync::Arc};

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalDecisionSource, ApprovalId, ApprovalRequest, ApprovalTrigger,
    ExtensionToolBeforeCallAction, ExtensionToolBeforeCallParams, InternalAgentKind,
    OperationFingerprint, PersistedToolResult as ToolResult, PluginDiagnosticKind,
    PreparedOperationIdentity, RunId, SessionId, Sha256Digest, ToolCallPresentation,
    ToolOutputTruncation,
};
use oven_sdk::{JsonSchema, ToolDefinition};
use serde_json::Value;
use tokio::sync::mpsc;

use super::artifacts::MAX_ATTACHMENT_BYTES;
use super::{
    ActiveRun, ApprovalToolInput, Engine, EngineError, Event, PreparedToolCall, PublishedTool,
    PublishedToolSet, ToolCallFailureCode, ToolFailure, ToolInterceptionContext,
    approval_flow::approval_expiry,
    approval_projection::denied_tool_failure,
    artifacts::{ArtifactStore, OutputCapture},
    helpers::{safe_display, sanitize_safe_text, session_depth},
};
use crate::{
    events::OutputHub,
    media::{approved_media_type, canonical_video_mime_type},
    permissions::PermissionPipeline,
    policy::FrozenRunPolicy,
    tool_api::{
        PreparedTool, ProgressSink, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
        ToolPreparationContext, ToolProgress, ToolProvider, ToolStdin,
    },
};

const MAX_VIDEO_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

const TOOL_CANCELLATION_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn tool_progress_event(progress: &ToolProgress) -> Event {
    Event::ToolCallProgress {
        tool_call_id: progress.tool_call_id,
        message: safe_display(&progress.message),
        output_chunk: progress.output_chunk.as_deref().map(safe_display),
    }
}

async fn append_tool_progress(
    engine: &Engine,
    session: SessionId,
    run: RunId,
    progress: ToolProgress,
) -> Result<(), EngineError> {
    engine
        .append(
            session,
            Some(run),
            super::event_origin("engine:tool-execution"),
            tool_progress_event(&progress),
        )
        .await
}

async fn enqueue_cleanup_tool_progress(
    engine: &Engine,
    session: SessionId,
    run: RunId,
    progress: ToolProgress,
) -> Result<(), EngineError> {
    #[cfg(test)]
    let block = engine
        .inner
        .tool_progress_append_block
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    #[cfg(test)]
    if let Some(block) = block {
        block.notified().await;
    }
    let completion = engine
        .enqueue_append(
            session,
            Some(run),
            super::event_origin("engine:tool-execution"),
            tool_progress_event(&progress),
        )
        .await?;
    drop(completion);
    Ok(())
}

fn close_and_discard_progress(progress_rx: &mut mpsc::Receiver<ToolProgress>) -> usize {
    progress_rx.close();
    let mut discarded = 0;
    while progress_rx.try_recv().is_ok() {
        discarded += 1;
    }
    discarded
}

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
                    intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                    call,
                    permission_name: None,
                    presentation: fallback_presentation,
                    prepared: Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: error.to_string(),
                    }),
                    interception: None,
                };
            }
        };
        let depth = session_depth(&session.meta.origin);
        let permission_overlay = &session.permission_overlay;
        let grants = self.skill_grants_for_session(session_id);
        let delegate_enabled = policy.agent.delegation.as_ref().is_some_and(|delegation| {
            depth < delegation.effective_depth_ceiling
                && delegation
                    .targets
                    .iter()
                    .any(|target| policy.delegation_target_available(target))
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
                Some((published.provider.clone(), published.spec.clone()))
            }
            Some(Some(_)) => {
                return PreparedToolCall {
                    intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
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
                    interception: None,
                };
            }
            Some(None) => {
                return PreparedToolCall {
                    intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                    prepared: Err(ToolFailure {
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: format!("tool `{}` was not published to the model", call.name),
                    }),
                    call,
                    permission_name: None,
                    presentation: fallback_presentation,
                    interception: None,
                };
            }
            None => current,
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
                    .map(|permission_name| {
                        (
                            candidate.clone(),
                            crate::ToolSpec {
                                name: call.name.clone(),
                                permission_name: permission_name.to_owned(),
                                description: String::new(),
                                parameters: serde_json::json!(true),
                                result_truncation: Default::default(),
                            },
                        )
                    })
            });
        }
        let Some((provider, tool_spec)) = provider else {
            return PreparedToolCall {
                intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is unavailable", call.name),
                }),
                call,
                permission_name: None,
                presentation: fallback_presentation,
                interception: None,
            };
        };
        let permission_name = tool_spec.permission_name.clone();
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
                intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                prepared: Err(ToolFailure {
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is not enabled for this session", call.name),
                }),
                call,
                permission_name: Some(permission_name),
                presentation: fallback_presentation,
                interception: None,
            };
        }
        let original_resource = provider
            .get_permission_resource(&call.name, &call.arguments)
            .ok()
            .and_then(|(_, resource)| resource);
        let presentation = provider.presentation(&call);
        let context = ToolPreparationContext {
            session: session_id,
            run,
            cwd: self.inner.store.cwd().to_owned(),
            workspace_root: self.inner.store.cwd().to_owned(),
            turn_context,
        };
        let prepared = match provider.prepare(context.clone(), call.clone()).await {
            Ok(prepared) => {
                apply_permission_resource(provider.as_ref(), &call.name, &permission_name, prepared)
                    .map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        };
        PreparedToolCall {
            intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
            call,
            permission_name: Some(permission_name.clone()),
            presentation,
            prepared,
            interception: Some(ToolInterceptionContext {
                provider,
                spec: tool_spec,
                preparation: context,
                permission_name,
                permission_resource: original_resource,
            }),
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
            let PreparedToolCall {
                mut call,
                prepared,
                interception,
                intercepted_arguments,
                ..
            } = prepared;
            let mut prepared = prepared?;
            let result_truncation = interception
                .as_ref()
                .map(|interception| interception.spec.result_truncation)
                .unwrap_or_default();
            let operation = prepared.operation.clone();
            let policy_labels = prepared.policy_labels.clone();
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
            if let Some(interception) = interception {
                let context_id = crate::plugin::plugin_context_id();
                for plugin in engine.inner.plugins.interception_plugins(
                    cookie_agent_protocol::ExtensionInterceptionHook::ToolBeforeCall,
                ) {
                    let result = engine
                        .inner
                        .plugins
                        .intercept_named::<_, cookie_agent_protocol::ExtensionToolBeforeCallResult>(
                            &plugin,
                            cookie_agent_protocol::PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
                            &ExtensionToolBeforeCallParams {
                                session_id: active.session,
                                context_id: context_id.clone(),
                                tool: call.name.clone(),
                                arguments: call.arguments.clone(),
                                permission_name: interception.permission_name.clone(),
                                resource: interception.permission_resource.clone(),
                            },
                            Some(active.session),
                            Some(&context_id),
                        )
                        .await;
                    let result = match result {
                        Ok(result) => result,
                        Err(error) => {
                            let kind =
                                if error.contains("crashed") || error.contains("not connected") {
                                    PluginDiagnosticKind::InterceptionCrash
                                } else {
                                    PluginDiagnosticKind::InterceptionTimeout
                                };
                            engine.record_plugin_diagnostic(active.session, plugin, kind, error);
                            continue;
                        }
                    };
                    if result.action == ExtensionToolBeforeCallAction::Block {
                        let message = result
                            .message_to_model
                            .or(result.reason)
                            .unwrap_or_else(|| format!("plugin `{plugin}` blocked the tool call"));
                        engine.record_plugin_diagnostic(
                            active.session,
                            plugin,
                            PluginDiagnosticKind::HookBlocked,
                            message.clone(),
                        );
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message,
                        });
                    }
                    let Some(arguments) = result.modified_arguments else {
                        continue;
                    };
                    let valid_schema = jsonschema::validator_for(&interception.spec.parameters)
                        .is_ok_and(|validator| validator.is_valid(&arguments));
                    let modified_resource = interception
                        .provider
                        .get_permission_resource(&call.name, &arguments)
                        .ok()
                        .and_then(|(_, resource)| resource);
                    if !arguments.is_object()
                        || !valid_schema
                        || modified_resource != interception.permission_resource
                    {
                        let message = format!(
                            "plugin `{plugin}` produced invalid tool arguments or changed the permission resource"
                        );
                        engine.record_plugin_diagnostic(
                            active.session,
                            plugin,
                            PluginDiagnosticKind::InvalidModification,
                            message.clone(),
                        );
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message,
                        });
                    }
                    let mut modified_call = call.clone();
                    modified_call.arguments = arguments;
                    let candidate = interception
                        .provider
                        .prepare(interception.preparation.clone(), modified_call.clone())
                        .await
                        .and_then(|prepared| {
                            apply_permission_resource(
                                interception.provider.as_ref(),
                                &modified_call.name,
                                &interception.permission_name,
                                prepared,
                            )
                        })
                        .map_err(ToolFailure::from)?;
                    if candidate.operation.capabilities() != operation.capabilities()
                        || candidate.operation.resources() != operation.resources()
                        || candidate.policy_labels != policy_labels
                    {
                        let message = format!(
                            "plugin `{plugin}` changed the approved permission capability or resource"
                        );
                        engine.record_plugin_diagnostic(
                            active.session,
                            plugin,
                            PluginDiagnosticKind::InvalidModification,
                            message.clone(),
                        );
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message,
                        });
                    }
                    call = modified_call;
                    prepared = candidate;
                    *intercepted_arguments
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = call.arguments.clone();
                }
            }
            let _serialization_guard = if let Some(key) = &prepared.serialization_key {
                Some(engine.mutation_lock(key).lock_owned().await)
            } else {
                None
            };
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
            let tool_cancellation = active.cancellation.child_token();
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
                cancellation: tool_cancellation.clone(),
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
                        while let Ok(progress) = progress_rx.try_recv() {
                            let _ = append_tool_progress(
                                &engine,
                                active.session,
                                run,
                                progress,
                            )
                            .await;
                        }
                        // Tool implementations drain their producers before
                        // resolving.  Finalizing here makes all emitted deltas
                        // precede the completion notification committed by the
                        // session actor.
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        return match result {
                            Ok(result) => {
                                if let Some(capture) = &capture {
                                    return capture
                                        .finish(
                                        result,
                                        active.policy.result_limits.tool_output_max_lines,
                                        active.policy.result_limits.tool_output_max_bytes,
                                    )
                                        .map_err(|error| ToolFailure {
                                            code: ToolCallFailureCode::ExecutionFailed,
                                            message: error.to_string(),
                                        });
                                }
                                bound_tool_result(
                                    result,
                                    result_truncation,
                                    &engine.inner.artifacts,
                                    active.policy.result_limits.tool_output_max_lines,
                                    active.policy.result_limits.tool_output_max_bytes,
                                )
                                .map_err(ToolFailure::from)
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
                        let _ = append_tool_progress(
                            &engine,
                            active.session,
                            run,
                            progress,
                        )
                        .await;
                    }
                    _ = active.cancellation.cancelled() => {
                        tool_cancellation.cancel();
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&call.id);
                        let cleanup_deadline =
                            tokio::time::Instant::now() + TOOL_CANCELLATION_CLEANUP_TIMEOUT;
                        let cleanup = tokio::time::sleep_until(cleanup_deadline);
                        tokio::pin!(cleanup);
                        let mut discarded_progress = 0;
                        let mut cleanup_timed_out = false;
                        'cleanup: loop {
                            tokio::select! {
                                _ = &mut invoke => {
                                    while let Ok(progress) = progress_rx.try_recv() {
                                        if tokio::time::timeout_at(
                                            cleanup_deadline,
                                            enqueue_cleanup_tool_progress(&engine, active.session, run, progress),
                                        )
                                        .await
                                        .is_err()
                                        {
                                            cleanup_timed_out = true;
                                            discarded_progress = 1 + close_and_discard_progress(&mut progress_rx);
                                            break 'cleanup;
                                        }
                                    }
                                    break;
                                }
                                Some(progress) = progress_rx.recv() => {
                                    if tokio::time::timeout_at(
                                        cleanup_deadline,
                                        enqueue_cleanup_tool_progress(&engine, active.session, run, progress),
                                    )
                                    .await
                                    .is_err()
                                    {
                                        cleanup_timed_out = true;
                                        discarded_progress = 1 + close_and_discard_progress(&mut progress_rx);
                                        break;
                                    }
                                }
                                () = &mut cleanup => {
                                    cleanup_timed_out = true;
                                    discarded_progress = close_and_discard_progress(&mut progress_rx);
                                    break;
                                }
                            }
                        }
                        if let Some(capture) = &capture {
                            capture.discard();
                        }
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        let message = if cleanup_timed_out {
                            format!(
                                "tool call cancelled after it started; cleanup deadline elapsed and {discarded_progress} progress record(s) never entered the session mailbox and were discarded"
                            )
                        } else {
                            "tool call cancelled after it started".into()
                        };
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message,
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
                && delegation
                    .targets
                    .iter()
                    .any(|target| policy.delegation_target_available(target))
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
    policy: crate::ToolResultTruncationPolicy,
    artifacts: &ArtifactStore,
    max_lines: usize,
    max_bytes: usize,
) -> Result<ToolResult, ToolError> {
    if policy == crate::ToolResultTruncationPolicy::OptOut {
        result.truncation = None;
        if result.output.len() > ToolResult::MAX_OUTPUT_BYTES {
            return Err(ToolError::resource_limit(format!(
                "tool output is {} bytes; the event limit is {} bytes",
                result.output.len(),
                ToolResult::MAX_OUTPUT_BYTES
            )));
        }
        return Ok(result);
    }
    let Some(preview) = truncate_tool_output(&result.output, max_lines, max_bytes) else {
        return Ok(result);
    };
    let original_bytes = result.output.len() as u64;
    let original_lines = result.output.split('\n').count() as u64;
    let (retained, _) = artifacts
        .retain(result.output.as_bytes())
        .map_err(|error| ToolError::execution(error.to_string()))?;
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
    let max_attachment_bytes = if mime_type.starts_with("video/") {
        MAX_VIDEO_ATTACHMENT_BYTES
    } else {
        MAX_ATTACHMENT_BYTES
    };
    if bytes.len() as u64 > max_attachment_bytes {
        return Err(ToolError::resource_limit(format!(
            "attachment is {} bytes; the limit is {max_attachment_bytes} bytes",
            bytes.len()
        )));
    }
    let validated = approved_media_type(path, bytes)?.ok_or_else(|| {
        ToolError::execution("attachment is not a supported image, PDF, or video")
    })?;
    if canonical_video_mime_type(validated) != canonical_video_mime_type(mime_type) {
        return Err(ToolError::execution(format!(
            "attachment MIME mismatch: declared {mime_type}, validated {validated}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cookie_agent_protocol::{PersistedToolResult, SafeDisplayText, ToolCallId};

    use super::{
        ArtifactStore, MAX_VIDEO_ATTACHMENT_BYTES, ToolCall, bound_tool_result,
        fallback_operation_fingerprint, validate_attachment,
    };
    use crate::{ToolError, ToolResultTruncationPolicy};

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

    #[test]
    fn video_attachment_limit_accepts_boundary_and_rejects_plus_one() {
        let video = |size: usize| {
            let mut bytes = vec![0_u8; size];
            bytes[..4].copy_from_slice(b"FLV\x01");
            bytes
        };
        let at_limit = video(MAX_VIDEO_ATTACHMENT_BYTES as usize);
        validate_attachment("video/x-flv", Path::new("clip.flv"), &at_limit).unwrap();

        let over_limit = video(MAX_VIDEO_ATTACHMENT_BYTES as usize + 1);
        let error =
            validate_attachment("video/x-flv", Path::new("clip.flv"), &over_limit).unwrap_err();
        assert!(error.to_string().contains("26214400"));
    }

    #[test]
    fn video_mime_aliases_validate_in_alias_and_canonical_forms() {
        let mut quicktime = 16_u32.to_be_bytes().to_vec();
        quicktime.extend_from_slice(b"ftypqt  \0\0\0\0");
        for declared in ["video/mov", "video/quicktime"] {
            validate_attachment(declared, Path::new("clip.mov"), &quicktime).unwrap();
        }

        let avi = b"RIFF\x04\x00\x00\x00AVI ";
        for declared in ["video/avi", "video/x-msvideo"] {
            validate_attachment(declared, Path::new("clip.avi"), avi).unwrap();
        }

        let mpeg = [0x00, 0x00, 0x01, 0xba];
        for declared in ["video/mpg", "video/mpeg"] {
            validate_attachment(declared, Path::new("clip.mpg"), &mpeg).unwrap();
        }
    }

    fn result(output: String) -> PersistedToolResult {
        PersistedToolResult {
            title: SafeDisplayText::new("Tool output").unwrap(),
            output,
            metadata: serde_json::Value::Null,
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        }
    }

    #[test]
    fn opted_out_results_ignore_config_limits_without_retention() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_path = directory.path().join("artifacts");
        let artifacts = ArtifactStore::open(artifact_path.clone()).unwrap();
        let output = "line\n".repeat(1_000);
        let bounded = bound_tool_result(
            result(output.clone()),
            ToolResultTruncationPolicy::OptOut,
            &artifacts,
            1,
            1,
        )
        .unwrap();
        assert_eq!(bounded.output, output);
        assert!(bounded.truncation.is_none());
        assert_eq!(std::fs::read_dir(artifact_path).unwrap().count(), 0);
    }

    #[test]
    fn opted_out_result_over_event_limit_is_a_resource_error() {
        let directory = tempfile::tempdir().unwrap();
        let artifacts = ArtifactStore::open(directory.path().join("artifacts")).unwrap();
        let error = bound_tool_result(
            result("x".repeat(PersistedToolResult::MAX_OUTPUT_BYTES + 1)),
            ToolResultTruncationPolicy::OptOut,
            &artifacts,
            usize::MAX,
            usize::MAX,
        )
        .unwrap_err();
        assert!(matches!(error, ToolError::ResourceLimit(_)));
        assert!(error.to_string().contains("event limit"));
    }

    #[test]
    fn default_external_tool_policy_still_truncates_and_retains() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_path = directory.path().join("artifacts");
        let artifacts = ArtifactStore::open(artifact_path.clone()).unwrap();
        let external_spec = crate::ToolSpec {
            name: "mcp_fixture".into(),
            permission_name: "mcp".into(),
            description: "External fixture".into(),
            parameters: serde_json::json!({"type":"object"}),
            result_truncation: Default::default(),
        };
        assert_eq!(
            external_spec.result_truncation,
            ToolResultTruncationPolicy::Bounded
        );
        let bounded = bound_tool_result(
            result("first\nsecond\n".into()),
            external_spec.result_truncation,
            &artifacts,
            1,
            5,
        )
        .unwrap();
        assert_eq!(bounded.output, "first");
        let truncation = bounded.truncation.expect("bounded truncation metadata");
        assert_eq!(truncation.original_bytes, 13);
        assert_eq!(truncation.original_lines, 3);
        assert!(truncation.retained.uri.starts_with("artifact://sha256/"));
        assert_eq!(std::fs::read_dir(artifact_path).unwrap().count(), 1);
    }
}

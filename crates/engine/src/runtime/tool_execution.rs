use std::sync::Arc;

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalDecisionSource, ApprovalId, ApprovalRequest, ApprovalTrigger,
    ExtensionToolBeforeCallAction, ExtensionToolBeforeCallParams, OperationFingerprint,
    PersistedToolResult as ToolResult, PluginDiagnosticKind, PreparedOperationIdentity, RunId,
    SessionId, Sha256Digest,
};
use futures_util::FutureExt as _;
use oven_sdk::{JsonSchema, ToolDefinition};
use serde_json::Value;
use tokio::sync::mpsc;

use super::{
    ActiveRun, ApprovalToolInput, Engine, EngineError, Event, OutputCapture, PreparedToolCall,
    PublishedTool, PublishedToolSet, ToolCallFailureCode, ToolFailure, ToolInterceptionContext,
    approval_flow::approval_expiry,
    approval_projection::denied_tool_failure,
    helpers::{safe_display, session_depth},
};
use crate::{
    PreparedTool, ProgressSink, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProgress, ToolProvider, ToolStdin,
    events::OutputHub,
    permissions::{PermissionDecision, PermissionPipeline},
    policy::FrozenRunPolicy,
    runtimes::{ComposedTool, Composition, SourcedProvider, compose},
};

const TOOL_CANCELLATION_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn tool_progress_event(progress: &ToolProgress) -> Event {
    Event::ToolCallProgress {
        tool_call_id: progress.tool_call_id,
        message: safe_display(&progress.message),
        display: progress.display.as_deref().map(|text| {
            cookie_agent_runtime_api::sanitize_tool_display(
                text,
                cookie_agent_protocol::SafeDisplayText::MAX_BYTES,
            )
        }),
    }
}

/// Publishes tool progress to live subscribers. It is never stored: the
/// tool termination (and its retained output) is the durable record.
fn publish_tool_progress(engine: &Engine, session: SessionId, run: RunId, progress: &ToolProgress) {
    engine.publish_transient(session, run, tool_progress_event(progress));
}

impl Engine {
    pub(super) fn decide_tool_permission(
        &self,
        active: &ActiveRun,
        prepared_call: &PreparedToolCall,
    ) -> Result<PermissionDecision, ToolFailure> {
        let prepared = prepared_call.prepared.as_ref().map_err(Clone::clone)?;
        if prepared_call
            .permission_name
            .as_deref()
            .is_some_and(PermissionPipeline::is_unconditional)
        {
            return Ok(PermissionDecision::unconditional());
        }
        let permission_overlay = self
            .inner
            .store
            .get(active.session)
            .map_err(|error| ToolFailure {
                partial_output: None,
                code: ToolCallFailureCode::ExecutionFailed,
                message: error.to_string(),
            })?
            .permission_overlay
            .clone();
        let grants = self.skill_grants_for_session(active.session);
        Ok(self.inner.permissions.decide_operation_with_grants(
            &active.policy.agent,
            Some(&permission_overlay),
            grants.as_ref(),
            prepared.operation(),
            prepared.policy_labels(),
            prepared_call
                .origin()
                .and_then(crate::ToolOrigin::workspace_root),
        ))
    }

    pub(super) async fn resolve_tool_permission(
        &self,
        active: &ActiveRun,
        run: RunId,
        prepared_call: &PreparedToolCall,
        permission: &PermissionDecision,
    ) -> Result<(), ToolFailure> {
        let prepared = prepared_call.prepared.as_ref().map_err(Clone::clone)?;
        match permission.effect {
            cookie_agent_protocol::PermissionEffect::Allow => Ok(()),
            cookie_agent_protocol::PermissionEffect::Deny => Err(ToolError::PermissionDenied(
                denied_tool_failure(ApprovalDecisionSource::Policy, "permission denied", None),
            )
            .into()),
            cookie_agent_protocol::PermissionEffect::Ask => {
                let allow_tree_grant = prepared.operation().resources().iter().all(|resource| {
                    resource.binding_lifetime
                        == cookie_agent_protocol::PreparedBindingLifetime::RestartStable
                        && !matches!(
                            resource.capability,
                            cookie_agent_protocol::PermissionAction::Read
                                | cookie_agent_protocol::PermissionAction::Write
                        )
                });
                let request = ApprovalRequest::new(
                    ApprovalId::new_v7(),
                    1,
                    ApprovalTrigger::PermissionPolicy,
                    prepared.operation().clone(),
                    permission.evaluations.clone(),
                    ApprovalConstraints {
                        allow_once: true,
                        allow_tree_grant,
                        cancellable: true,
                        // The user-facing approval window comes from
                        // `[approval].timeout_ms`, not the internal approval
                        // agent's classifier model-call budget.
                        expires_at: approval_expiry(self.inner.config.runtime.approval.timeout_ms),
                    },
                )
                .map_err(|error| ToolFailure {
                    partial_output: None,
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: error.to_string(),
                })?;
                let outcome = self
                    .await_user_approval(
                        active,
                        run,
                        request,
                        prepared.executor().clone(),
                        true,
                        ApprovalToolInput {
                            name: &prepared_call.call.name,
                            origin: prepared_call.origin(),
                            normalized_parameters: prepared.normalized_arguments(),
                        },
                    )
                    .await
                    .map_err(|error| ToolFailure {
                        partial_output: None,
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: error.to_string(),
                    })?;
                if outcome.approved {
                    Ok(())
                } else {
                    Err(ToolFailure {
                        partial_output: None,
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: denied_tool_failure(
                            ApprovalDecisionSource::Policy,
                            "permission refused by user",
                            outcome.feedback,
                        ),
                    })
                }
            }
        }
    }

    pub(super) async fn prepare_tool_call(
        &self,
        session_id: SessionId,
        run: RunId,
        call: ToolCall,
        policy: &FrozenRunPolicy,
        turn_context: Arc<crate::TurnAgentContext>,
    ) -> PreparedToolCall {
        self.prepare_tool_call_with_publication(
            session_id,
            run,
            call,
            policy,
            turn_context,
            None,
            None,
        )
        .await
    }

    pub(super) async fn prepare_published_tool_calls(
        &self,
        session_id: SessionId,
        run: RunId,
        calls: Vec<(ToolCall, Option<&PublishedTool>)>,
        policy: &FrozenRunPolicy,
        turn_context: Arc<crate::TurnAgentContext>,
    ) -> Vec<PreparedToolCall> {
        let composition = self.compose_for_policy(session_id, policy).ok();
        // Calls grouped by the provider and attachment that answer them, each
        // with the name its source knows the tool by.
        let mut groups: Vec<(SourcedProvider, Vec<(usize, String)>)> = Vec::new();
        for (index, (call, _)) in calls.iter().enumerate() {
            let Some(tool) = composition
                .as_ref()
                .and_then(|composition| composition.get(&call.name))
            else {
                continue;
            };
            let entry = (index, tool.spec.name.clone());
            if let Some((_, indexes)) = groups.iter_mut().find(|(candidate, _)| {
                Arc::ptr_eq(&candidate.provider, &tool.source.provider)
                    && candidate.attachment == tool.source.attachment
            }) {
                indexes.push(entry);
            } else {
                groups.push((tool.source.clone(), vec![entry]));
            }
        }
        let mut prepared = (0..calls.len()).map(|_| None).collect::<Vec<_>>();
        for (source, indexes) in groups {
            let provider_calls = indexes
                .iter()
                .map(|(index, name)| ToolCall {
                    name: name.clone(),
                    ..calls[*index].0.clone()
                })
                .collect();
            let context = ToolPreparationContext {
                session: session_id,
                run,
                origin: source.origin.clone(),
                turn_context: turn_context.clone(),
            };
            let results = source
                .provider
                .prepare_parallel(context, provider_calls)
                .await;
            for ((index, _), result) in indexes.into_iter().zip(results) {
                prepared[index] = Some(result);
            }
        }
        let mut output = Vec::with_capacity(calls.len());
        for (index, (call, published)) in calls.into_iter().enumerate() {
            output.push(
                self.prepare_tool_call_with_publication(
                    session_id,
                    run,
                    call,
                    policy,
                    turn_context.clone(),
                    Some(published),
                    prepared[index].take(),
                )
                .await,
            );
        }
        output
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_tool_call_with_publication(
        &self,
        session_id: SessionId,
        run: RunId,
        call: ToolCall,
        policy: &FrozenRunPolicy,
        turn_context: Arc<crate::TurnAgentContext>,
        published: Option<Option<&PublishedTool>>,
        prepared_override: Option<Result<PreparedTool, ToolError>>,
    ) -> PreparedToolCall {
        let fallback_presentation = cookie_agent_runtime_api::tool_title_only(&call.name);
        if call.arguments.is_null() {
            // A null argument object is the marked-invalid convention for
            // provider argument text that could not be parsed. Refuse before
            // schema validation so a permissive published tool can never run
            // with unsalvageable input.
            return PreparedToolCall {
                intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                call,
                permission_name: None,
                presentation: fallback_presentation,
                prepared: Err(ToolFailure {
                    partial_output: None,
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: "tool call arguments are invalid or unsalvageable; refusing to execute with null input"
                        .to_owned(),
                }),
                interception: None,
            };
        }
        let session = match self.inner.store.get(session_id) {
            Ok(session) => session,
            Err(error) => {
                return PreparedToolCall {
                    intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                    call,
                    permission_name: None,
                    presentation: fallback_presentation,
                    prepared: Err(ToolFailure {
                        partial_output: None,
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
        let delegate_enabled = !policy.delegate_targets(depth).is_empty();
        let mut provider = match published {
            Some(Some(published)) => {
                // The tool must still be what the model was offered: same
                // attachment, same instance generation, same declaration.
                let current = self
                    .compose_for_policy(session_id, policy)
                    .ok()
                    .and_then(|composition| composition.get(&call.name).cloned());
                if current.is_some_and(|current| {
                    current.source.attachment == published.source.attachment
                        && current.source.generation == published.source.generation
                        && current.spec == published.spec
                }) {
                    Some((published.source.clone(), published.spec.clone()))
                } else {
                    return PreparedToolCall {
                        intercepted_arguments: Arc::new(std::sync::Mutex::new(
                            call.arguments.clone(),
                        )),
                        prepared: Err(ToolFailure {
                            partial_output: None,
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
            }
            Some(None) => {
                return PreparedToolCall {
                    intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                    prepared: Err(ToolFailure {
                        partial_output: None,
                        code: ToolCallFailureCode::ExecutionFailed,
                        message: format!("tool `{}` was not published to the model", call.name),
                    }),
                    call,
                    permission_name: None,
                    presentation: fallback_presentation,
                    interception: None,
                };
            }
            None => self
                .compose_session_tools(session_id, |_, _| true)
                .ok()
                .and_then(|composition| {
                    composition
                        .get(&call.name)
                        .map(|tool| (tool.source.clone(), tool.spec.clone()))
                }),
        };
        if published.is_none()
            && provider.is_none()
            && (call.name != "skill" || self.is_direct_skill_call(call.id))
        {
            let sources = self.session_tool_sources(session_id).unwrap_or_default();
            provider = sources.iter().find_map(|candidate| {
                candidate
                    .provider
                    .permission_for_unlisted_tool(&call.name)
                    .ok()
                    .flatten()
                    .map(|permission_name| {
                        (
                            candidate.clone(),
                            crate::ToolSpec {
                                collision: Default::default(),
                                output: Default::default(),
                                concurrency: Default::default(),
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
        let Some((source, tool_spec)) = provider else {
            return PreparedToolCall {
                intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                prepared: Err(ToolFailure {
                    partial_output: None,
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is unavailable", call.name),
                }),
                call,
                permission_name: None,
                presentation: fallback_presentation,
                interception: None,
            };
        };
        let SourcedProvider {
            attachment,
            origin,
            provider,
            ..
        } = source;
        // A prefixed tool runs under the name its runtime knows it by.
        let call = ToolCall {
            name: tool_spec.name.clone(),
            ..call
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
                    origin.workspace_root(),
                ))
        {
            return PreparedToolCall {
                intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                prepared: Err(ToolFailure {
                    partial_output: None,
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
            origin,
            turn_context,
        };
        let prepared = match match prepared_override {
            Some(prepared) => prepared,
            None => provider.prepare(context.clone(), call.clone()).await,
        } {
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
                attachment,
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
        let permission = self.decide_tool_permission(&active, &prepared)?;
        self.resolve_tool_permission(&active, run, &prepared, &permission)
            .await?;
        self.execute_approved_tool(active, run, prepared, turn_context, false)
            .await
    }

    pub(super) async fn execute_approved_tool(
        &self,
        active: Arc<ActiveRun>,
        run: RunId,
        prepared: PreparedToolCall,
        turn_context: Arc<crate::TurnAgentContext>,
        retain_output: bool,
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
            let output_declaration = interception
                .as_ref()
                .map(|interception| interception.spec.output.clone())
                .unwrap_or_default();
            if result_truncation == crate::ToolResultTruncationPolicy::OptOut
                && output_declaration != cookie_agent_protocol::ToolOutputDeclaration::Single
            {
                return Err(ToolError::execution(
                    "self-paginating tools require a single output declaration",
                )
                .into());
            }
            let operation = prepared.operation().clone();
            let policy_labels = prepared.policy_labels().to_vec();
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
                            partial_output: None,
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
                            partial_output: None,
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
                    if candidate.operation().capabilities() != operation.capabilities()
                        || candidate.operation().resources() != operation.resources()
                        || candidate.policy_labels() != policy_labels
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
                            partial_output: None,
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
            let _serialization_guard = if let Some(key) = prepared.serialization_key() {
                Some(engine.mutation_lock(key).lock_owned().await)
            } else {
                None
            };
            let executor = prepared
                .executor()
                .lock()
                .await
                .take()
                .ok_or_else(|| ToolFailure {
                    partial_output: None,
                    code: ToolCallFailureCode::PreparedCapabilityLost,
                    message: "prepared executor capability was already consumed or lost".into(),
                })?;
            executor.revalidate().await.map_err(ToolFailure::from)?;
            let (progress_tx, mut progress_rx) = mpsc::channel(64);
            let hub = engine
                .inner
                .output
                .hubs
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
            let capture = if retain_output
                && result_truncation != crate::ToolResultTruncationPolicy::OptOut
            {
                Some(
                    OutputCapture::with_limits(
                        engine.inner.artifacts.clone(),
                        active.session,
                        output_declaration,
                        active.policy.result_limits.tool_output,
                    )
                    .await
                    .map_err(ToolFailure::from)?,
                )
            } else {
                None
            };
            if let Some(capture) = &capture {
                engine
                    .inner
                    .output
                    .captures
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(call.id, capture.clone());
            }
            let (stdin_tx, stdin) = ToolStdin::channel(64);
            if interactive {
                active
                    .stdin
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(call.id, stdin_tx);
            }
            let tool_cancellation = active.cancellation.child_token();
            let invoke = std::panic::AssertUnwindSafe(executor.execute(ToolExecutionContext {
                session: active.session,
                run,
                progress: match &capture {
                    Some(capture)
                        if result_truncation != crate::ToolResultTruncationPolicy::OptOut =>
                    {
                        capture.progress_sink(progress_tx.clone(), hub.clone())
                    }
                    _ => ProgressSink::new(progress_tx.clone()),
                },
                cancellation: tool_cancellation.clone(),
                stdin: interactive.then_some(stdin),
                turn_context,
                artifacts: engine.inner.artifacts.clone(),
                background: Some(Arc::new(super::background_tasks::BackgroundCapability {
                    inner: Arc::downgrade(&engine.inner),
                    tool_call_id: call.id,
                    limits: active.policy.result_limits.tool_output,
                })),
            }))
            .catch_unwind()
            .map(|result| {
                result.unwrap_or_else(|_| Err(ToolError::execution("tool execution panicked")))
            });
            let mut invoke = Box::pin(invoke);
            loop {
                tokio::select! {
                    result = &mut invoke => {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&call.id);
                        while let Ok(progress) = progress_rx.try_recv() {
                            publish_tool_progress(&engine, active.session, run, &progress);
                        }
                        // Tool implementations drain their producers before
                        // resolving.  Finalizing here makes all emitted deltas
                        // precede the completion notification committed by the
                        // session actor.
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        return match result {
                            Ok(result) => {
                                let failed = result.failed;
                                let result = match finalize_output(capture.as_ref(), result, failed, result_truncation).await {
                                    Ok(result) => result,
                                    Err(error) => {
                                        let mut failure = ToolFailure::from(error);
                                        failure.partial_output = finalize_output(capture.as_ref(), incomplete_completion(&failure.message), true, result_truncation).await.ok().map(Box::new);
                                        return Err(failure);
                                    }
                                };
                                if failed {
                                    let message = cookie_agent_protocol::diagnostics::tool_result(&result);
                                    Err(ToolFailure { code: ToolCallFailureCode::ExecutionFailed, message, partial_output: Some(Box::new(result)) })
                                } else { Ok(result) }
                            }
                            Err(error) => {
                                let mut failure = ToolFailure::from(error);
                                failure.partial_output = finalize_output(capture.as_ref(), incomplete_completion(&failure.message), true, result_truncation).await.ok().map(Box::new);
                                Err(failure)
                            }
                        };
                    }
                    Some(progress) = progress_rx.recv() => {
                        publish_tool_progress(&engine, active.session, run, &progress);
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
                        let mut cleanup_timed_out = false;
                        let mut delegate_result = None;
                        loop {
                            tokio::select! {
                                result = &mut invoke => {
                                    delegate_result = result.ok().filter(|result| {
                                        engine.is_delegate_call_result(active.session, run, call.id, &result.result)
                                    });
                                    while let Ok(progress) = progress_rx.try_recv() {
                                        publish_tool_progress(&engine, active.session, run, &progress);
                                    }
                                    break;
                                }
                                Some(progress) = progress_rx.recv() => {
                                    publish_tool_progress(&engine, active.session, run, &progress);
                                }
                                () = &mut cleanup => {
                                    cleanup_timed_out = true;
                                    progress_rx.close();
                                    break;
                                }
                            }
                        }
                        // Drop unsubmitted I/O admission waits before finalization. Jobs
                        // already submitted own their permits and are drained by capture.
                        drop(invoke);
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        if !cleanup_timed_out && let Some(result) = delegate_result {
                            return finalize_output(capture.as_ref(), result, true, result_truncation).await.map_err(ToolFailure::from);
                        }
                        let message = if cleanup_timed_out {
                            "tool call cancelled after it started; cleanup deadline elapsed"
                        } else {
                            "tool call cancelled after it started"
                        }
                        .to_owned();
                        let partial_output = finalize_output(capture.as_ref(), incomplete_completion(&message), true, result_truncation).await.ok().map(Box::new);
                        return Err(ToolFailure {
                            code: ToolCallFailureCode::ExecutionFailed,
                            message,
                            partial_output,
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

    /// The tool set of `session` as composed now from the sources `keep`
    /// admits. Composition diagnostics are reported once each.
    pub(crate) fn compose_session_tools(
        &self,
        session: SessionId,
        keep: impl Fn(&SourcedProvider, &crate::ToolSpec) -> bool,
    ) -> Result<Composition, EngineError> {
        let context = SessionToolContext::new(session);
        let mut candidates = Vec::new();
        for source in self.session_tool_sources(session)? {
            for tool in source
                .provider
                .tools_for_session(&context)
                .map_err(|error| EngineError::ToolFailed(error.to_string()))?
            {
                if keep(&source, &tool) {
                    candidates.push((source.clone(), tool));
                }
            }
        }
        let composition =
            compose(candidates, crate::base_tools::reserved_tool_names()).map_err(|name| {
                EngineError::ToolFailed(format!(
                    "tool name `{name}` is published twice by one runtime"
                ))
            })?;
        self.inner.runtimes.report(&composition.diagnostics);
        Ok(composition)
    }

    /// The tool set `policy` publishes for `session`: enabled tools the
    /// agent and the session overlay make visible, composed.
    fn compose_for_policy(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<Composition, EngineError> {
        let session_projection = self.inner.store.get(session)?;
        let depth = session_depth(&session_projection.meta.origin);
        let delegate_enabled = !policy.delegate_targets(depth).is_empty();
        self.compose_session_tools(session, |source, tool| {
            let delegation_tool = tool.permission_name == "delegate";
            let goal_tool = source.attachment.is_none()
                && matches!(tool.name.as_str(), "goal_get" | "goal_update");
            (!delegation_tool || delegate_enabled)
                && (!goal_tool || policy.goal_tools_enabled)
                && PermissionPipeline::tool_visible_with_overlay(
                    &policy.agent,
                    Some(&session_projection.permission_overlay),
                    &tool.permission_name,
                )
        })
    }

    pub(super) fn published_tool_definitions(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<PublishedToolSet, EngineError> {
        let composition = self.compose_for_policy(session, policy)?;
        let mut output = Vec::with_capacity(composition.tools.len());
        let mut published = std::collections::HashMap::with_capacity(composition.tools.len());
        for ComposedTool { name, source, spec } in composition.tools {
            spec.output.validate().map_err(EngineError::ToolFailed)?;
            let schema = JsonSchema::new(spec.parameters.clone()).map_err(|error| {
                EngineError::ToolFailed(format!("tool `{name}` has invalid JSON Schema: {error}"))
            })?;
            output.push(ToolDefinition::new(
                name.clone(),
                spec.description.clone(),
                schema,
            ));
            published.insert(name, PublishedTool { source, spec });
        }
        output.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(PublishedToolSet {
            definitions: output,
            tools: published,
        })
    }
}

async fn finalize_output(
    capture: Option<&OutputCapture>,
    completion: crate::ToolCompletion,
    incomplete: bool,
    policy: crate::ToolResultTruncationPolicy,
) -> Result<ToolResult, ToolError> {
    if let Some(capture) = capture {
        return capture.finish(completion, incomplete).await;
    }
    if policy == crate::ToolResultTruncationPolicy::OptOut {
        return super::finish_page(completion);
    }
    // Direct skill dispatch is nested work, not another model tool-call. Its caller owns
    // the returned single output, so only the outer tool call captures and publishes it.
    let mut result = completion.result;
    match completion.output {
        cookie_agent_protocol::ToolCompletionOutput::Single { text } => result.output = text,
        cookie_agent_protocol::ToolCompletionOutput::Streamed if incomplete => {}
        _ => {
            return Err(ToolError::execution(
                "direct skill dispatch requires terminal single output",
            ));
        }
    }
    Ok(result)
}

fn incomplete_completion(message: &str) -> crate::ToolCompletion {
    crate::ToolCompletion::streamed(ToolResult {
        title: safe_display("Incomplete tool output"),
        output: String::new(),
        display: Some(cookie_agent_runtime_api::bounded_tool_display(message)),
        retained_output: None,
        metadata: Value::Null,
        truncation: None,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    })
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
        "delegate_subagent" | "get_subagent_result" | "cancel_subagent" => Some("delegate"),
        "skill" => Some("skill"),
        "webfetch" => Some("webfetch"),
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
    if PermissionPipeline::is_unconditional(permission_name) {
        // No rule is evaluated, so the prepared capability is not checked
        // against an action.
        return match resource {
            Some(resource) => prepared.with_permission_resource(Some(resource)),
            None => Ok(prepared),
        };
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
    // A provider may return None when it manages policy labels while preparing
    // the operation. Providers that need the loose marker set it explicitly.
    match resource {
        Some(resource) => prepared.with_permission_resource(Some(resource)),
        None => Ok(prepared),
    }
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

    #[tokio::test]
    async fn null_tool_arguments_are_rejected_before_schema_validation() {
        use std::sync::Arc;

        use cookie_agent_protocol::RunId;

        use cookie_agent_runtime_api::{ToolCall, TurnAgentContext};

        use super::ToolCallFailureCode;

        let (fixture, selection) = crate::runtime::tests::support::custom_fixture();
        let policy = crate::runtime::tests::support::frozen_root_policy(&fixture, &selection);
        let binding = policy
            .selected_suffix
            .first()
            .expect("frozen binding")
            .clone();
        let turn_context = Arc::new(TurnAgentContext {
            agent: policy.agent.agent.clone(),
            model: binding.selection.model.clone(),
            adapter: crate::policy::wire_adapter(binding.descriptor.adapter_id.as_str()),
            adapter_family: crate::policy::tool_adapter_family(
                binding.descriptor.adapter_id.as_str(),
            ),
            capabilities: policy
                .model_capabilities(&binding)
                .expect("model capabilities"),
        });
        let prepared = fixture
            .engine
            .prepare_tool_call_with_publication(
                crate::runtime::test_session_id(),
                RunId::new_v7(),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "permissive".into(),
                    arguments: serde_json::Value::Null,
                },
                &policy,
                turn_context,
                None,
                None,
            )
            .await;
        let failure = match prepared.prepared {
            Ok(_) => panic!("null arguments must fail before schema validation"),
            Err(failure) => failure,
        };
        assert!(matches!(failure.code, ToolCallFailureCode::ExecutionFailed));
        assert_eq!(
            failure.message,
            "tool call arguments are invalid or unsalvageable; refusing to execute with null input"
        );
        fixture.engine.shutdown().await;
    }
}

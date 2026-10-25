use std::sync::Arc;

use cookie_agent_protocol::{
    ApprovalConstraints, ApprovalDecisionSource, ApprovalId, ApprovalRequest, ApprovalTrigger,
    ExtensionToolBeforeCallAction, ExtensionToolBeforeCallParams, Open, OperationFingerprint,
    PersistedToolResult as ToolResult, PluginDiagnosticKind, PreparedOperationIdentity, RunId,
    RuntimeId, SessionId, Sha256Digest, ToolIdentity, ToolPresentation, ToolSource,
};
use cookie_agent_runtime_api::ProvidedToolSource;
use futures_util::FutureExt as _;
use oven_sdk::{JsonSchema, ToolDefinition};
use serde_json::Value;
use tokio::sync::mpsc;

use super::{
    ActiveRun, ApprovalToolInput, Engine, EngineError, Event, OutputCapture, PreparedToolCall,
    PublishedTool, PublishedToolSet, ToolCallFailureCode, ToolFailure, ToolInterceptionContext,
    approval_projection::denied_tool_failure,
    helpers::{safe_display, session_depth},
};
use crate::{
    PreparedTool, ProgressSink, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProgress, ToolProvider,
    events::OutputHub,
    permissions::{MergedRules, PermissionDecision, PermissionPipeline, PermissionScope},
    policy::FrozenRunPolicy,
    runtimes::{ComposedTool, Composition, SourcedProvider, compose},
};

pub(crate) const TOOL_CANCELLATION_CLEANUP_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);

fn tool_progress_event(progress: &ToolProgress) -> Event {
    Event::ToolCallProgress {
        tool_call_id: progress.tool_call_id,
        message: safe_display(&progress.message),
        // The progress sink already sanitized and bounded the delta.
        display: progress
            .display
            .clone()
            .filter(|delta| delta.validate().is_ok())
            .map(cookie_agent_protocol::Open::Known),
    }
}

/// Publishes tool progress to live subscribers. It is never stored: the
/// tool termination (and its retained output) is the durable record.
fn publish_tool_progress(engine: &Engine, session: SessionId, run: RunId, progress: &ToolProgress) {
    engine.publish_transient(session, run, tool_progress_event(progress));
}

/// The most display-block kinds one diagnostic names.
const MAX_REPORTED_BLOCK_KINDS: usize = 8;

/// The source of a diagnostic about what `tool`'s provider sent: its plugin
/// or runtime, else the engine's tool host.
fn peer_diagnostic_source(tool: &ToolIdentity) -> cookie_agent_protocol::DiagnosticSource {
    let source = match tool.source.as_ref().and_then(Open::known) {
        Some(ToolSource::Plugin { plugin }) => {
            cookie_agent_protocol::DiagnosticSource::plugin(plugin).ok()
        }
        Some(
            ToolSource::Runtime { runtime_id, .. }
            | ToolSource::Mcp {
                runtime_id: Some(runtime_id),
                ..
            },
        ) => cookie_agent_protocol::DiagnosticSource::runtime(runtime_id.as_str()).ok(),
        _ => None,
    };
    source.unwrap_or_else(|| {
        cookie_agent_protocol::DiagnosticSource::engine("tools")
            .expect("static diagnostic source is valid")
    })
}

impl Engine {
    /// Records that the daemon dropped display blocks of kinds this build
    /// does not know, of tags `tags`, from the `part` (`presentation`,
    /// `progress`, or `result`) of a call of `tool`. A plugin or runtime
    /// sends only the kinds the daemon's version includes, so such a block
    /// comes from a peer newer than its handshake says
    /// (`docs/site/specs/compatibility-v1.md` section 12.4).
    pub(super) fn report_unknown_display_blocks(
        &self,
        session: SessionId,
        tool: &ToolIdentity,
        part: &'static str,
        mut tags: Vec<String>,
    ) {
        if tags.is_empty() {
            return;
        }
        let count = tags.len() as u64;
        tags.sort_unstable();
        tags.dedup();
        tags.truncate(MAX_REPORTED_BLOCK_KINDS);
        let kinds = tags
            .iter()
            .map(|tag| cookie_agent_protocol::diagnostics::sanitize(tag, 64))
            .collect::<Vec<_>>();
        let message = format!(
            "dropped {count} display block{} of a kind this daemon does not know from the {part} of tool `{}`: {}",
            if count == 1 { "" } else { "s" },
            cookie_agent_protocol::diagnostics::sanitize(&tool.published_name, 64),
            kinds.join(", ")
        );
        let event = Event::Diagnostic {
            source: peer_diagnostic_source(tool),
            kind: cookie_agent_protocol::PeerDiagnosticKind::UnknownDisplayBlock.label(),
            message,
            count: None,
            detail: Some(serde_json::json!({
                "tool": tool.published_name,
                "part": part,
                "kinds": kinds,
            })),
        };
        let engine = self.clone();
        tokio::spawn(async move {
            let _ = engine
                .on_actor(session, move |engine| {
                    engine.append_direct(session, None, super::event_origin("engine:tools"), event)
                })
                .await;
        });
    }

    pub(super) fn decide_tool_permission(
        &self,
        active: &ActiveRun,
        prepared_call: &PreparedToolCall,
    ) -> Result<PermissionDecision, ToolFailure> {
        let prepared = prepared_call.prepared.as_ref().map_err(Clone::clone)?;
        // A resolved tool without a permission name needs no permission.
        if prepared_call
            .interception
            .as_ref()
            .is_some_and(|interception| interception.permission_name.is_none())
        {
            return Ok(PermissionDecision::permission_free());
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
        let grants = self.skill_grants_for_session(active.session, prepared_call.attachment());
        let decide = |scope: PermissionScope<'_>| {
            self.inner.permissions.decide_operation_with_grants(
                &active.policy.agent,
                Some(&permission_overlay),
                grants.as_ref(),
                prepared.operation(),
                prepared.policy_labels(),
                scope,
            )
        };
        let runtimes = prepared.permission_runtimes();
        if runtimes.is_empty() {
            return Ok(decide(
                prepared_call
                    .origin()
                    .map_or(PermissionScope::BASE, PermissionScope::of),
            ));
        }
        // A call that acts on several runtimes is checked as
        // `action@runtime` under each; the strictest decision wins, per
        // resource.
        Ok(strictest_decision(runtimes.iter().map(|runtime| {
            decide(PermissionScope::runtime(runtime, None))
        })))
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
                        && !resource.capability.is_path()
                });
                let request = ApprovalRequest::new(
                    ApprovalId::new_v7(),
                    1,
                    prepared_call.call.id,
                    ApprovalTrigger::PermissionPolicy,
                    prepared.operation().clone(),
                    permission.evaluations.clone(),
                    ApprovalConstraints {
                        allow_once: true,
                        allow_tree_grant,
                        cancellable: true,
                        // The user's window, `[approval].timeout_ms`, starts
                        // when the call escalates to them.
                        expires_at: None,
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
                            tool_call_id: prepared_call.call.id,
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
        let mut groups: Vec<(SourcedProvider, Vec<(usize, crate::ToolSpec)>)> = Vec::new();
        for (index, (call, _)) in calls.iter().enumerate() {
            let Some(tool) = composition
                .as_ref()
                .and_then(|composition| composition.get(&call.name))
            else {
                continue;
            };
            let entry = (index, tool.spec.clone());
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
                .map(|(index, spec)| {
                    let mut call = ToolCall {
                        name: spec.name.clone(),
                        ..calls[*index].0.clone()
                    };
                    // Resolution reports a malformed `background` itself.
                    let _ = take_background(spec, &mut call);
                    call
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
        // A call that fails before it prepares shows the name the model
        // called, and the tool's source when its publication names it.
        let mut fallback_presentation = ToolPresentation::new(
            cookie_agent_runtime_api::tool_title_only(&call.name),
            tool_identity(
                &call.name,
                published
                    .flatten()
                    .map(|published| (&published.source, &published.spec)),
            ),
        );
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
                background: None,
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
                    background: None,
                };
            }
        };
        let depth = session_depth(&session.meta.origin);
        let permission_overlay = &session.permission_overlay;
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
                        background: None,
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
                    background: None,
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
                                background: false,
                                background_default: false,
                                collision: Default::default(),
                                output: Default::default(),
                                concurrency: Default::default(),
                                name: call.name.clone(),
                                permission_name: Some(permission_name.to_owned()),
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
                background: None,
            };
        };
        fallback_presentation.tool = tool_identity(&call.name, Some((&source, &tool_spec)));
        let tool = fallback_presentation.tool.clone();
        let SourcedProvider {
            attachment,
            origin,
            provider,
            ..
        } = source;
        // A prefixed tool runs under the name its runtime knows it by.
        let mut call = ToolCall {
            name: tool_spec.name.clone(),
            ..call
        };
        let background = match take_background(&tool_spec, &mut call) {
            Ok(background) => background,
            Err(message) => {
                return PreparedToolCall {
                    intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                    prepared: Err(ToolFailure {
                        partial_output: None,
                        code: ToolCallFailureCode::ExecutionFailed,
                        message,
                    }),
                    call,
                    permission_name: tool_spec.permission_name.clone(),
                    presentation: fallback_presentation,
                    interception: None,
                    background: None,
                };
            }
        };
        let grants = self.skill_grants_for_session(session_id, attachment.as_ref());
        let permission_name = tool_spec.permission_name.clone();
        let delegation_tool = permission_name.as_deref() == Some("delegate");
        let enabled = !delegation_tool || delegate_enabled;
        let direct_skill = call.name == "skill" && self.is_direct_skill_call(call.id);
        if !enabled
            || (!direct_skill
                && !permission_name.as_deref().is_none_or(|permission_name| {
                    PermissionPipeline::tool_offered(
                        &policy.agent,
                        Some(permission_overlay),
                        grants.as_ref(),
                        permission_name,
                        &call.name,
                        PermissionScope::of(&origin),
                    )
                }))
        {
            return PreparedToolCall {
                intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
                prepared: Err(ToolFailure {
                    partial_output: None,
                    code: ToolCallFailureCode::ExecutionFailed,
                    message: format!("tool `{}` is not enabled for this session", call.name),
                }),
                call,
                permission_name,
                presentation: fallback_presentation,
                interception: None,
                background: None,
            };
        }
        let context = ToolPreparationContext {
            session: session_id,
            run,
            origin,
            turn_context,
        };
        let (prepared, original_resource) = match match prepared_override {
            Some(prepared) => prepared,
            None => provider.prepare(context.clone(), call.clone()).await,
        } {
            Ok(prepared) => {
                match apply_permission_resource(
                    provider.as_ref(),
                    &call.name,
                    permission_name.as_deref(),
                    &tool,
                    &context.origin,
                    prepared,
                ) {
                    Ok((prepared, resource)) => (Ok(prepared), resource),
                    Err(error) => (Err(error.into()), None),
                }
            }
            Err(error) => (Err(error.into()), None),
        };
        // A provider that reports its metadata at preparation (a runtime
        // behind the runtime protocol), or that shows the call from what it
        // prepared, reports how the call is shown with it.
        let mut prepared_presentation = match &prepared {
            Ok(prepared) => prepared.presentation().cloned(),
            Err(_) => None,
        }
        .unwrap_or_else(|| provider.presentation(&call));
        let unknown = prepared_presentation.fit();
        self.report_unknown_display_blocks(session_id, &tool, "presentation", unknown);
        let presentation = ToolPresentation::new(prepared_presentation, tool.clone());
        PreparedToolCall {
            intercepted_arguments: Arc::new(std::sync::Mutex::new(call.arguments.clone())),
            call,
            permission_name: permission_name.clone(),
            presentation,
            prepared,
            interception: Some(ToolInterceptionContext {
                attachment,
                tool,
                provider,
                spec: tool_spec,
                preparation: context,
                permission_name,
                permission_resource: original_resource,
            }),
            background,
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
                presentation,
                background,
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
                for hooked in engine.inner.plugins.interception_plugins(
                    cookie_agent_protocol::ExtensionInterceptionHook::ToolBeforeCall,
                ) {
                    let plugin = hooked.name.clone();
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
                                origin: interception.hook_origin().into(),
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
                            if let Some(failure) = engine.hook_failed(
                                active.session,
                                &hooked,
                                cookie_agent_protocol::ExtensionInterceptionHook::ToolBeforeCall,
                                error,
                            ) {
                                return Err(ToolFailure {
                                    partial_output: None,
                                    code: ToolCallFailureCode::ExecutionFailed,
                                    message: format!("Tool call blocked: {failure}."),
                                });
                            }
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
                    let invalid_modification = |message: String| {
                        engine.record_plugin_diagnostic(
                            active.session,
                            plugin.clone(),
                            PluginDiagnosticKind::InvalidModification,
                            message.clone(),
                        );
                        ToolFailure {
                            partial_output: None,
                            code: ToolCallFailureCode::ExecutionFailed,
                            message,
                        }
                    };
                    let invalid_arguments = || {
                        format!(
                            "plugin `{plugin}` produced invalid tool arguments or changed the permission resource"
                        )
                    };
                    if !arguments.is_object() || !valid_schema {
                        return Err(invalid_modification(invalid_arguments()));
                    }
                    let mut modified_call = call.clone();
                    modified_call.arguments = arguments;
                    // The permission resource of the modified arguments is
                    // what preparing them reports.
                    let (candidate, modified_resource) = interception
                        .provider
                        .prepare(interception.preparation.clone(), modified_call.clone())
                        .await
                        .and_then(|prepared| {
                            apply_permission_resource(
                                interception.provider.as_ref(),
                                &modified_call.name,
                                interception.permission_name.as_deref(),
                                &interception.tool,
                                &interception.preparation.origin,
                                prepared,
                            )
                        })
                        .map_err(ToolFailure::from)?;
                    if modified_resource != interception.permission_resource {
                        return Err(invalid_modification(invalid_arguments()));
                    }
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
            // The call's own token: the run cancels it only while the call is
            // in the foreground, so a detached call outlives the run.
            let tool_cancellation = tokio_util::sync::CancellationToken::new();
            let (detach_point, mut detach_watch) = cookie_agent_runtime_api::DetachPoint::channel();
            // A display block of a kind this build does not know is dropped,
            // and reported once per call and part.
            let unknown_reporter = |part: &'static str| {
                let engine = engine.clone();
                let session = active.session;
                let tool = presentation.tool.clone();
                let reported = std::sync::atomic::AtomicBool::new(false);
                move |tags: Vec<String>| {
                    if !reported.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        engine.report_unknown_display_blocks(session, &tool, part, tags);
                    }
                }
            };
            let report_unknown_progress = unknown_reporter("progress");
            let report_unknown_result = unknown_reporter("result");
            let invoke = std::panic::AssertUnwindSafe(
                executor.execute(ToolExecutionContext {
                    session: active.session,
                    run,
                    progress: match &capture {
                        Some(capture)
                            if result_truncation != crate::ToolResultTruncationPolicy::OptOut =>
                        {
                            capture.progress_sink(progress_tx.clone(), hub.clone())
                        }
                        _ => ProgressSink::new(progress_tx.clone()),
                    }
                    .with_unknown_block_reporter(Arc::new(report_unknown_progress)),
                    cancellation: tool_cancellation.clone(),
                    turn_context,
                    artifacts: engine.inner.artifacts.clone(),
                    detach: detach_point,
                    background: background == Some(true),
                    producers: None,
                }),
            )
            .catch_unwind()
            .map(move |result| {
                let mut result =
                    result.unwrap_or_else(|_| Err(ToolError::execution("tool execution panicked")));
                if let Ok(completion) = &mut result {
                    let unknown =
                        cookie_agent_protocol::take_unknown_blocks(&mut completion.result.display);
                    if !unknown.is_empty() {
                        report_unknown_result(unknown);
                    }
                }
                result
            });
            let mut invoke: std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<crate::ToolCompletion, ToolError>>
                        + Send,
                >,
            > = Box::pin(invoke);
            // The model asks for the background up front; the user may ask
            // while the call runs.
            let mut detach_by = (background == Some(true))
                .then_some(super::background_tasks::BackgroundActor::Model);
            let detach_request = background.map(|_| {
                let request = Arc::new(tokio::sync::Notify::new());
                active
                    .detach_requests
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(call.id, Arc::clone(&request));
                request
            });
            let _detach_registration = DetachRegistration {
                active: &active,
                call: call.id,
            };
            let started_at = jiff::Timestamp::now();
            loop {
                tokio::select! {
                    () = async {
                        match &detach_request {
                            Some(request) => request.notified().await,
                            None => std::future::pending().await,
                        }
                    }, if detach_by.is_none() => {
                        detach_by = Some(super::background_tasks::BackgroundActor::User);
                    }
                    detachable = detach_watch.reached(), if detach_by.is_some() => {
                        while let Ok(progress) = progress_rx.try_recv() {
                            publish_tool_progress(&engine, active.session, run, &progress);
                        }
                        // The call's live display ends here; its capture goes on.
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        let by = detach_by.expect("detaching on request");
                        if detachable.reports_itself {
                            // The work reports itself (a subagent): stop
                            // waiting; dropping the execution releases it.
                            drop(invoke);
                            return Ok(super::background_tasks::self_reporting_result(
                                by,
                                detachable.note.as_deref(),
                            ));
                        }
                        return engine
                            .detach_call(super::background_tasks::DetachedCall {
                                session: active.session,
                                tool_call_id: call.id,
                                tool: call.name.clone(),
                                display: presentation
                                    .primary_argument
                                    .as_ref()
                                    .map(|display| display.as_str().to_owned()),
                                by,
                                started_at,
                                note: detachable.note,
                                execution: invoke,
                                progress: progress_rx,
                                capture,
                                truncation: result_truncation,
                                cancellation: tool_cancellation,
                            })
                            .await
                            .map_err(ToolFailure::from);
                    }
                    result = &mut invoke => {
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
                        // Work that reports itself outlives the run that
                        // waited for it: stop waiting instead of cancelling.
                        let mut self_reporting = detach_watch.current().filter(|detachable| detachable.reports_itself);
                        let mut cleanup_timed_out = false;
                        let mut delegate_result = None;
                        if self_reporting.is_none() {
                            tool_cancellation.cancel();
                            let cleanup_deadline =
                                tokio::time::Instant::now() + TOOL_CANCELLATION_CLEANUP_TIMEOUT;
                            let cleanup = tokio::time::sleep_until(cleanup_deadline);
                            tokio::pin!(cleanup);
                            // A call may reach its detach point while it winds
                            // down: a subagent whose start was still being
                            // confirmed when the run was cancelled. It then
                            // waits on its child, which only its own turn
                            // ends, so stop waiting there as above rather
                            // than at the deadline, which loses the handle.
                            let mut watch_detach = true;
                            loop {
                                tokio::select! {
                                    detachable = detach_watch.reached(), if watch_detach => {
                                        if detachable.reports_itself {
                                            self_reporting = Some(detachable);
                                            break;
                                        }
                                        watch_detach = false;
                                    }
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
                        }
                        // Drop unsubmitted I/O admission waits before finalization. Jobs
                        // already submitted own their permits and are drained by capture.
                        drop(invoke);
                        hub.finalize();
                        engine.retain_finalized_output_hub(call.id);
                        let cause = active.cancel_cause();
                        let user_interrupt = cause == Some(super::CancelCause::User);
                        if let Some(detachable) = self_reporting {
                            let message = match detachable.on_user_interrupt.filter(|_| user_interrupt) {
                                Some(message) => message,
                                None => {
                                    let mut message = interrupted_message(cause.as_ref(), false);
                                    message.push_str("\nIts work keeps running and reports itself.");
                                    if let Some(note) = detachable.note.as_deref().map(str::trim).filter(|note| !note.is_empty()) {
                                        message.push('\n');
                                        message.push_str(note);
                                    }
                                    message
                                }
                            };
                            let partial_output = finalize_output(capture.as_ref(), incomplete_completion(&message), true, result_truncation).await.ok().map(Box::new);
                            return Err(ToolFailure {
                                code: ToolCallFailureCode::ExecutionFailed,
                                message,
                                partial_output,
                            });
                        }
                        if !cleanup_timed_out && let Some(result) = delegate_result {
                            return finalize_output(capture.as_ref(), result, true, result_truncation).await.map_err(ToolFailure::from);
                        }
                        // A call that wound down within the grace may supply
                        // its own result for a user interrupt.
                        let message = detach_watch
                            .current()
                            .and_then(|detachable| detachable.on_user_interrupt)
                            .filter(|_| user_interrupt && !cleanup_timed_out)
                            .unwrap_or_else(|| interrupted_message(cause.as_ref(), cleanup_timed_out));
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
        mut keep: impl FnMut(&SourcedProvider, &crate::ToolSpec) -> bool,
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
    pub(crate) fn compose_for_policy(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<Composition, EngineError> {
        let session_projection = self.inner.store.get(session)?;
        let depth = session_depth(&session_projection.meta.origin);
        let delegate_enabled = !policy.delegate_targets(depth).is_empty();
        // The memory tools follow the snapshot: none at all without a
        // `memory` rule, whatever `memory_write` rules say, and the write
        // tools only when some memory is both readable and writable.
        let memory = super::memory_runs::memory_access(
            true,
            &policy.agent,
            Some(&session_projection.permission_overlay),
        );
        // One merged rule list per home the runtimes reported, not one per
        // tool.
        let mut merged_by_home = Vec::<(Option<String>, MergedRules<'_>)>::new();
        let mut composition = self.compose_session_tools(session, |source, tool| {
            let delegation_tool = tool.permission_name.as_deref() == Some("delegate");
            let goal_tool = source.attachment.is_none()
                && matches!(tool.name.as_str(), "goal_get" | "goal_update");
            let memory_tool = source.provider.provider_id() == crate::memory_tools::PROVIDER_ID;
            let memory_write_tool =
                memory_tool && tool.permission_name.as_deref() == Some("memory_write");
            (!delegation_tool || delegate_enabled)
                && (!goal_tool || policy.goal_tools_enabled)
                && (!memory_tool || memory.available)
                && (!memory_write_tool || memory.writable)
                && tool
                    .permission_name
                    .as_deref()
                    .is_none_or(|permission_name| {
                        let scope = PermissionScope::of(&source.origin);
                        let index = merged_by_home
                            .iter()
                            .position(|(home, _)| home.as_deref() == scope.home)
                            .unwrap_or_else(|| {
                                merged_by_home.push((
                                    scope.home.map(str::to_owned),
                                    MergedRules::of(
                                        &policy.agent,
                                        Some(&session_projection.permission_overlay),
                                        scope.home,
                                    ),
                                ));
                                merged_by_home.len() - 1
                            });
                        PermissionPipeline::tool_offered_by(
                            &merged_by_home[index].1,
                            None,
                            permission_name,
                            &tool.name,
                            scope,
                        )
                    })
        })?;
        // `cron_list` comes with the other cron tools, and `list_agents` with
        // them or with `delegate_subagent` (cron spec section 8.1).
        let from_global = |tool: &ComposedTool, provider: &str| {
            tool.source.attachment.is_some() && tool.source.provider.provider_id() == provider
        };
        let offers_cron = composition.tools.iter().any(|tool| {
            from_global(tool, crate::cron_tools::PROVIDER_ID) && tool.spec.permission_name.is_some()
        });
        let offers_delegate = composition
            .tools
            .iter()
            .any(|tool| tool.source.attachment.is_none() && tool.spec.name == "delegate_subagent");
        composition.tools.retain(|tool| {
            (offers_cron || !from_global(tool, crate::cron_tools::PROVIDER_ID))
                && (offers_cron
                    || offers_delegate
                    || !from_global(tool, crate::agents_tool::PROVIDER_ID))
        });
        // `stop_background` only makes sense next to a tool that can run in
        // the background.
        if !composition.tools.iter().any(|tool| tool.spec.background) {
            composition.tools.retain(|tool| {
                tool.source.attachment.is_some()
                    || tool.spec.name != crate::base_tools::STOP_BACKGROUND_TOOL
            });
        }
        Ok(composition)
    }

    pub(super) fn published_tool_definitions(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<PublishedToolSet, EngineError> {
        let composition = self.compose_for_policy(session, policy)?;
        let notes = self.runtime_tool_notes(session);
        let mut output = Vec::with_capacity(composition.tools.len());
        let mut published = std::collections::HashMap::with_capacity(composition.tools.len());
        for ComposedTool { name, source, spec } in composition.tools {
            spec.output.validate().map_err(EngineError::ToolFailed)?;
            let mut parameters = spec.parameters.clone();
            if spec.background {
                add_background_argument(&mut parameters, spec.background_default);
            }
            let schema = JsonSchema::new(parameters).map_err(|error| {
                EngineError::ToolFailed(format!("tool `{name}` has invalid JSON Schema: {error}"))
            })?;
            let description = match source.attachment.as_ref().and_then(|name| notes.get(name)) {
                Some(note) if spec.description.is_empty() => note.clone(),
                Some(note) => format!("{}\n\n{note}", spec.description),
                None => spec.description.clone(),
            };
            output.push(ToolDefinition::new(name.clone(), description, schema));
            published.insert(name, PublishedTool { source, spec });
        }
        output.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(PublishedToolSet {
            definitions: output,
            tools: published,
        })
    }
}

/// Takes the host-owned `background` argument out of a call of a
/// background-capable tool: the tool never sees it, and it is not part of
/// what is approved. `Ok(None)` for any other tool; the tool's default when
/// the model left the argument out.
fn take_background(spec: &crate::ToolSpec, call: &mut ToolCall) -> Result<Option<bool>, String> {
    if !spec.background {
        return Ok(None);
    }
    match call
        .arguments
        .as_object_mut()
        .and_then(|arguments| arguments.remove("background"))
    {
        None => Ok(Some(spec.background_default)),
        Some(Value::Bool(background)) => Ok(Some(background)),
        Some(_) => Err("`background` must be a boolean".into()),
    }
}

/// Unregisters an executing call's detach request when its execution leaves
/// the foreground loop.
struct DetachRegistration<'a> {
    active: &'a ActiveRun,
    call: cookie_agent_protocol::ToolCallId,
}

impl Drop for DetachRegistration<'_> {
    fn drop(&mut self) {
        self.active
            .detach_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.call);
    }
}

/// Adds the host-owned `background` argument to a background-capable tool's
/// parameters; composition has already refused a tool that declares one.
fn add_background_argument(parameters: &mut Value, default: bool) {
    let Some(object) = parameters.as_object_mut() else {
        return;
    };
    let properties = object
        .entry("properties")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(properties) = properties.as_object_mut() {
        properties.insert(
            "background".into(),
            serde_json::json!({
                "type": "boolean",
                "default": default,
                "description": "Run in the background: the call returns at once with a job ID, and a <background_result> with its result arrives when it finishes.",
            }),
        );
    }
}

pub(super) async fn finalize_output(
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

/// The tool a call goes to as clients see it: the name its source knows it
/// by, the name the model called it by, and where it comes from. `source`
/// is the composed tool, when the call names one.
fn tool_identity(
    published_name: &str,
    source: Option<(&SourcedProvider, &crate::ToolSpec)>,
) -> ToolIdentity {
    ToolIdentity {
        name: source
            .map_or(published_name, |(_, spec)| spec.name.as_str())
            .to_owned(),
        published_name: published_name.to_owned(),
        source: source.map(|(source, spec)| Open::Known(tool_source(source, &spec.name))),
    }
}

/// Where `source`'s tool `name` comes from: the plugin or MCP server its
/// provider names, else the runtime attachment that offers it, else the base
/// layer.
fn tool_source(source: &SourcedProvider, name: &str) -> ToolSource {
    let runtime = source.origin.runtime();
    let runtime_id = || runtime.and_then(|runtime| RuntimeId::new(runtime.id()).ok());
    match source.provider.tool_source(name) {
        Some(ProvidedToolSource::Plugin { plugin }) => ToolSource::Plugin { plugin },
        Some(ProvidedToolSource::Mcp { server }) => ToolSource::Mcp {
            server,
            runtime_id: runtime_id(),
        },
        None => match (&source.attachment, runtime, runtime_id()) {
            (Some(attachment), Some(runtime), Some(runtime_id)) => ToolSource::Runtime {
                runtime_id,
                attachment: attachment.clone(),
                capability: runtime.capability().clone(),
            },
            _ => ToolSource::Base,
        },
    }
}

/// The host's result for a call its run's cancellation interrupted, naming
/// why the run was cancelled; `cleanup_timed_out` when the call did not wind
/// down within its cleanup grace.
pub(super) fn interrupted_message(
    cause: Option<&super::CancelCause>,
    cleanup_timed_out: bool,
) -> String {
    let detail = if cleanup_timed_out {
        "; cleanup deadline elapsed"
    } else {
        ""
    };
    match cause {
        Some(super::CancelCause::User) => {
            format!("Interrupted by the user before it finished{detail}.")
        }
        Some(super::CancelCause::Shutdown) => {
            format!("Interrupted by engine shutdown before it finished{detail}.")
        }
        Some(super::CancelCause::Origin(origin)) => format!(
            "Interrupted by `{}` before it finished{detail}.",
            origin.as_str()
        ),
        Some(super::CancelCause::Reason(reason)) => format!(
            "Interrupted before it finished: {}{detail}.",
            reason.trim().trim_end_matches('.')
        ),
        None => format!("Interrupted before it finished: its run was cancelled{detail}."),
    }
}

pub(super) fn incomplete_completion(message: &str) -> crate::ToolCompletion {
    crate::ToolCompletion::streamed(ToolResult {
        title: safe_display("Incomplete tool output"),
        output: String::new(),
        display: cookie_agent_runtime_api::bounded_tool_display(message),
        background: None,
        retained_output: None,
        metadata: Value::Null,
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
        .unwrap_or(cookie_agent_protocol::PermissionAction::READ);
    let binding = serde_json::to_vec(&call.arguments).expect("tool arguments serialize");
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(&binding),
        vec![cookie_agent_protocol::ApprovalCapability {
            action: action.clone(),
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
        "delegate_subagent" | "get_subagent_result" => Some("delegate"),
        "skill" => Some("skill"),
        "webfetch" => Some("webfetch"),
        _ => None,
    }
}

/// Applies the permission resource of a prepared call's normalized
/// arguments, and returns it. A provider that reported its metadata at
/// preparation is not asked again. A call its provider only declared gets
/// the approval identity the daemon builds for `tool`, prepared under
/// `origin`.
pub(crate) fn apply_permission_resource(
    provider: &dyn ToolProvider,
    name: &str,
    expected_permission_name: Option<&str>,
    tool: &ToolIdentity,
    origin: &crate::ToolOrigin,
    prepared: PreparedTool,
) -> Result<(PreparedTool, Option<String>), ToolError> {
    let (permission_name, resource) = match prepared.declaration() {
        Some(declaration) => (
            declaration
                .permission_name
                .clone()
                .map(std::borrow::Cow::Owned),
            declaration.permission_resource.clone(),
        ),
        None => {
            let (permission_name, resource) =
                provider.get_permission_resource(name, prepared.normalized_arguments())?;
            (permission_name.map(std::borrow::Cow::Borrowed), resource)
        }
    };
    let permission_name = permission_name.as_deref();
    let matches = match (permission_name, expected_permission_name) {
        (Some(actual), Some(expected)) => actual == expected,
        (None, None) => true,
        _ => false,
    };
    if !matches {
        return Err(ToolError::execution(
            "tool permission metadata changed between discovery and preparation",
        ));
    }
    let action = permission_name
        .map(|permission_name| {
            PermissionPipeline::action_for_permission_name(permission_name)
                .map_err(|error| ToolError::execution(error.to_string()))
        })
        .transpose()?;
    if prepared.is_declared() {
        let operation = crate::permissions::identity::declared_operation(
            tool,
            &origin.context_bytes(),
            action.as_ref(),
            resource.as_deref(),
            prepared.normalized_arguments(),
        )?;
        return Ok((prepared.identify(operation, resource.clone())?, resource));
    }
    let Some(action) = action else {
        // A tool that needs no permission evaluates no rule, so the prepared
        // capability is not checked against an action.
        return match resource {
            Some(resource) => Ok((
                prepared.with_permission_resource(Some(resource.clone()))?,
                Some(resource),
            )),
            None => Ok((prepared, None)),
        };
    };
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
        Some(resource) => Ok((
            prepared.with_permission_resource(Some(resource.clone()))?,
            Some(resource),
        )),
        None => Ok((prepared, None)),
    }
}

/// The strictest of `decisions` over one operation, resource by resource:
/// deny over ask over allow. Allowed when there are none.
pub(crate) fn strictest_decision(
    decisions: impl IntoIterator<Item = PermissionDecision>,
) -> PermissionDecision {
    let mut decisions = decisions.into_iter();
    let Some(mut decision) = decisions.next() else {
        return PermissionDecision::permission_free();
    };
    for other in decisions {
        for (evaluation, candidate) in decision.evaluations.iter_mut().zip(other.evaluations) {
            if effect_rank(candidate.effect) > effect_rank(evaluation.effect) {
                *evaluation = candidate;
            }
        }
    }
    decision.effect = decision
        .evaluations
        .iter()
        .map(|evaluation| evaluation.effect)
        .max_by_key(|effect| effect_rank(*effect))
        .unwrap_or(cookie_agent_protocol::PermissionEffect::Allow);
    decision
}

/// How strict a permission effect is: deny over ask over allow.
const fn effect_rank(effect: cookie_agent_protocol::PermissionEffect) -> u8 {
    match effect {
        cookie_agent_protocol::PermissionEffect::Allow => 0,
        cookie_agent_protocol::PermissionEffect::Ask => 1,
        cookie_agent_protocol::PermissionEffect::Deny => 2,
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::ToolCallId;

    use super::{ToolCall, fallback_operation_fingerprint};

    /// A provider whose tools come from `source`.
    struct Sourced(Option<cookie_agent_runtime_api::ProvidedToolSource>);

    #[async_trait::async_trait]
    impl crate::ToolProvider for Sourced {
        fn provider_id(&self) -> &'static str {
            "test.sourced"
        }
        fn tools_for_session(
            &self,
            _: &crate::SessionToolContext,
        ) -> Result<Vec<crate::ToolSpec>, crate::ToolError> {
            Ok(Vec::new())
        }
        fn tool_source(&self, _: &str) -> Option<cookie_agent_runtime_api::ProvidedToolSource> {
            self.0.clone()
        }
        fn get_permission_name(_: &str) -> Result<Option<&'static str>, crate::ToolError> {
            Ok(None)
        }
        fn get_permission_resource(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<(Option<&'static str>, Option<String>), crate::ToolError> {
            Ok((None, None))
        }
        fn get_display_argument(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<String, crate::ToolError> {
            Ok(String::new())
        }
        async fn prepare(
            &self,
            _: crate::ToolPreparationContext,
            _: ToolCall,
        ) -> Result<crate::PreparedTool, crate::ToolError> {
            Err(crate::ToolError::execution("unused"))
        }
    }

    #[test]
    fn the_daemon_names_each_tools_source() {
        use cookie_agent_protocol::{AttachmentName, CapabilityName, RuntimeId, ToolSource};
        use cookie_agent_runtime_api::ProvidedToolSource;

        let runtime = crate::ToolOrigin::Runtime(std::sync::Arc::new(
            cookie_agent_runtime_api::RuntimeInfo::new(
                "labpc",
                CapabilityName::new("coding").unwrap(),
                cookie_agent_protocol::RuntimeKind::new("coding").unwrap(),
                "labpc_repo",
                None,
            ),
        ));
        let source = |origin: &crate::ToolOrigin, attachment: Option<&str>, provided| {
            crate::runtimes::SourcedProvider {
                attachment: attachment.map(|name| AttachmentName::new(name).unwrap()),
                generation: 0,
                origin: origin.clone(),
                provider: std::sync::Arc::new(Sourced(provided)),
            }
        };
        let base = crate::ToolOrigin::Base;
        for (source, expected) in [
            (source(&base, None, None), ToolSource::Base),
            (
                source(&runtime, Some("labpc_repo"), None),
                ToolSource::Runtime {
                    runtime_id: RuntimeId::new("labpc").unwrap(),
                    attachment: AttachmentName::new("labpc_repo").unwrap(),
                    capability: CapabilityName::new("coding").unwrap(),
                },
            ),
            (
                source(
                    &runtime,
                    Some("global"),
                    Some(ProvidedToolSource::Plugin {
                        plugin: "issues".into(),
                    }),
                ),
                ToolSource::Plugin {
                    plugin: "issues".into(),
                },
            ),
            (
                source(
                    &runtime,
                    Some("labpc_repo"),
                    Some(ProvidedToolSource::Mcp {
                        server: "docs".into(),
                    }),
                ),
                ToolSource::Mcp {
                    server: "docs".into(),
                    runtime_id: Some(RuntimeId::new("labpc").unwrap()),
                },
            ),
        ] {
            assert_eq!(super::tool_source(&source, "edit"), expected);
        }
        let unknown = super::tool_identity("missing_tool", None);
        assert_eq!(unknown.name, "missing_tool");
        assert_eq!(unknown.published_name, "missing_tool");
        assert_eq!(unknown.source, None);
    }

    #[test]
    fn discovery_failure_fingerprints_keep_known_tool_actions() {
        for (tool_name, permission_name) in [
            ("write", "write"),
            ("edit", "write"),
            ("bash", "bash"),
            ("delegate_subagent", "delegate"),
            ("get_subagent_result", "delegate"),
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
            input_media: crate::policy::tool_input_media(
                binding.descriptor.adapter_id.as_str(),
                &policy
                    .model_capabilities(&binding)
                    .expect("model capabilities"),
            ),
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

    #[test]
    fn an_unavailable_runtime_fails_with_its_own_code() {
        let failure = super::ToolFailure::from(crate::ToolError::runtime_unavailable(
            "MCP server `docs` is reconnecting",
        ));
        assert!(matches!(
            failure.code,
            super::ToolCallFailureCode::RuntimeUnavailable
        ));
        assert_eq!(failure.code.safe_code().as_str(), "runtime_unavailable");
        assert_eq!(failure.message, "MCP server `docs` is reconnecting");
    }
}

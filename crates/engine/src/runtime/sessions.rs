use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use cookie_agent_protocol::{
    AgentId, AgentUsageResult, ChildSummary, GlobalUsageResult, InvocationId, PermissionMode,
    RunSelection, SessionForkResult, SessionId, SessionMeta, SessionOrigin, SessionRenameChange,
    SessionRenameParams, SessionRenameResult, SessionRevertResult, SessionTreeUsageResult,
    SessionUsageResult, UsageRollup,
};

use super::{
    Engine, EngineError, Event, SessionCommand,
    helpers::{cwd_identity, session_depth},
};
use crate::policy::{self, freeze_root_agent_policy, resolve_agent};

impl Engine {
    pub(super) fn rebuild_visible_tree_grants(&self) {
        let invalidated = self.inner.grant_journal.invalidated_ids();
        let grants = self
            .inner
            .store
            .all()
            .into_iter()
            .flat_map(|session| session.log.events())
            .filter_map(|event| match event.payload {
                Event::TreeApprovalGrantCommitted { grant }
                    if !invalidated.contains(&grant.grant_id) =>
                {
                    Some(grant)
                }
                _ => None,
            });
        self.inner.approvals.replace(grants);
    }

    pub fn create_session(&self, selection: RunSelection) -> Result<SessionMeta, EngineError> {
        self.reconcile_provider_store()?;
        let runtime = self.current_runtime();
        if runtime.result.snapshot.models.is_empty()
            || !runtime
                .result
                .snapshot
                .agents
                .iter()
                .any(|agent| agent.runnable_as_root)
        {
            return Err(EngineError::NoRunnableModel);
        }
        let agents = Arc::clone(&runtime.agents);
        let agent = resolve_agent(&agents, &selection.agent)?.clone();
        if !agent.runnable_as_root {
            return Err(EngineError::NoRunnableModel);
        }
        let policy = freeze_root_agent_policy(
            &agent,
            agents,
            Arc::clone(&runtime),
            &selection.model,
            self.inner.config.runtime.delegation.max_depth,
            policy::ResultLimits {
                tool_output_max_lines: self.inner.config.runtime.tool_output.max_lines,
                tool_output_max_bytes: self.inner.config.runtime.tool_output.max_bytes,
            },
            self.inner.config.runtime.prompt_caching.strategy(),
        )?;
        let id = SessionId::new_v7();
        let cwd_identity = cwd_identity(self.inner.store.cwd())?;
        let creation = Event::SessionCreated {
            origin: SessionOrigin::Root,
            cwd_identity: cwd_identity.clone(),
            creation_selection: selection.clone(),
            creation_agent: Box::new(policy.agent.clone()),
            runtime_revision: runtime.result.snapshot.runtime_revision.clone(),
            catalog_revision: runtime.result.snapshot.catalog_revision.clone(),
            provider_state_revision: runtime.result.snapshot.provider_state_revision.clone(),
            model_revision: runtime.result.snapshot.model_revision.clone(),
            agent_revision: runtime.result.snapshot.agent_revision.clone(),
            recipe_registry_revision: runtime.result.snapshot.recipe_registry_revision.clone(),
            manifest_revision: runtime.current_manifest.revision.clone(),
        };
        self.inner.store.create(id, creation)?;
        self.spawn_actor(id);
        Ok(self.inner.store.get(id)?.metadata())
    }

    #[must_use]
    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        self.inner
            .store
            .all_summaries()
            .into_iter()
            .map(|session| session.meta)
            .collect()
    }
    pub fn get_session(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        let session = self.inner.store.get(id)?;
        #[cfg(test)]
        let hook = self
            .inner
            .read_only_reopen_hook
            .lock()
            .expect("read-only reopen hook lock poisoned")
            .take();
        #[cfg(test)]
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("read-only reopen reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            let _ = hook
                .release
                .lock()
                .expect("read-only reopen release lock poisoned")
                .recv();
        }
        Ok(session.metadata())
    }

    #[cfg(test)]
    pub(crate) fn install_read_only_reopen_hook_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .inner
            .read_only_reopen_hook
            .lock()
            .expect("read-only reopen hook lock poisoned") = Some(super::ReadOnlyReopenHook {
            reached: std::sync::Mutex::new(Some(reached)),
            release: std::sync::Mutex::new(release_receiver),
        });
        (receiver, release)
    }
    pub fn session_usage(&self, id: SessionId) -> Result<SessionUsageResult, EngineError> {
        let catalog = self.catalog_pricing();
        Ok(self
            .inner
            .store
            .session_usage(id, &self.inner.config.runtime.pricing, &catalog)?)
    }
    pub fn agent_usage(&self, agent_id: AgentId) -> AgentUsageResult {
        let mut usage = UsageRollup::default();
        for session in self.inner.store.all_summaries() {
            if let Some(agent) = session.agent_usage.get(&agent_id) {
                crate::usage::merge(&mut usage, agent);
            }
        }
        AgentUsageResult {
            agent_id,
            usage: crate::usage::with_pricing(
                usage,
                &self.inner.config.runtime.pricing,
                &self.catalog_pricing(),
            ),
        }
    }
    pub fn global_usage(&self) -> GlobalUsageResult {
        let mut usage = UsageRollup::default();
        for session in self.inner.store.all_summaries() {
            crate::usage::merge(&mut usage, &session.usage_rollup);
        }
        GlobalUsageResult {
            usage: crate::usage::with_pricing(
                usage,
                &self.inner.config.runtime.pricing,
                &self.catalog_pricing(),
            ),
        }
    }
    pub fn session_tree_usage(&self, id: SessionId) -> Result<SessionTreeUsageResult, EngineError> {
        self.inner.store.summary(id)?;
        let summaries = self.inner.store.all_summaries();
        let known: HashSet<_> = self
            .inner
            .delegation_events
            .entries()
            .into_iter()
            .map(|entry| {
                (
                    entry.reservation.invocation_id,
                    entry.reservation.parent_session_id,
                    entry.reservation.child_session_id,
                )
            })
            .collect();
        let mut children: HashMap<SessionId, Vec<(SessionId, InvocationId)>> = HashMap::new();
        let mut rollups = HashMap::new();
        for summary in summaries {
            let session_id = summary.meta.session_id;
            if let Some((parent_session_id, edge)) =
                validated_tree_usage_edge(&summary.meta.origin, session_id, &known)
            {
                children.entry(parent_session_id).or_default().push(edge);
            }
            rollups.insert(session_id, summary.usage_rollup);
        }

        let mut usage = UsageRollup::default();
        let session_count = merge_session_tree_usage(id, &children, &rollups, &mut usage)?;
        Ok(SessionTreeUsageResult {
            session_id: id,
            usage: crate::usage::with_pricing(
                usage,
                &self.inner.config.runtime.pricing,
                &self.catalog_pricing(),
            ),
            session_count,
        })
    }
    pub(super) fn catalog_pricing(
        &self,
    ) -> BTreeMap<cookie_agent_protocol::ModelKey, cookie_agent_models::catalog::CatalogModelCost>
    {
        self.inner
            .model_manager
            .current()
            .models()
            .iter()
            .filter_map(|(model, runtime)| {
                runtime
                    .model
                    .cost
                    .as_ref()
                    .map(|cost| (model.clone(), cost.clone()))
            })
            .collect()
    }
    pub fn delegate_targets(&self, id: SessionId) -> Result<Vec<AgentId>, EngineError> {
        let session = self.inner.store.get(id)?;
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .find(|active| active.session == id)
            .cloned();
        let Some(active) = active else {
            return Ok(Vec::new());
        };
        let depth = session_depth(&session.meta.origin);
        let Some(delegation) = &active.policy.agent.delegation else {
            return Ok(Vec::new());
        };
        if depth >= delegation.effective_depth_ceiling {
            return Ok(Vec::new());
        }
        Ok(delegation
            .targets
            .iter()
            .filter(|target| {
                active.policy.registry.get(target).is_some_and(|agent| {
                    agent.document.frontmatter.enabled
                        && matches!(
                            agent.document.frontmatter.mode,
                            cookie_agent_config::AgentMode::Subagent
                                | cookie_agent_config::AgentMode::All
                        )
                })
            })
            .cloned()
            .collect())
    }
    #[must_use]
    pub fn children(&self, id: SessionId) -> Vec<cookie_agent_protocol::ChildSummary> {
        let known: HashSet<_> = self
            .inner
            .delegation_events
            .entries()
            .into_iter()
            .map(|entry| {
                (
                    entry.reservation.invocation_id,
                    entry.reservation.parent_session_id,
                    entry.reservation.child_session_id,
                )
            })
            .collect();
        self.inner
            .store
            .all_summaries()
            .into_iter()
            .filter_map(|child| match child.meta.origin {
                SessionOrigin::Delegated {
                    parent_session_id,
                    invocation_id,
                    ..
                } if parent_session_id == id
                    && known.contains(&(
                        invocation_id,
                        parent_session_id,
                        child.meta.session_id,
                    )) =>
                {
                    Some(ChildSummary {
                        session_id: child.meta.session_id,
                        agent: child.meta.creation_selection.agent.clone(),
                        title: child.meta.title.clone(),
                        title_updated_seq: child.meta.title_updated_seq,
                        status: child.meta.status,
                        usage: child.usage,
                    })
                }
                _ => None,
            })
            .collect()
    }
    pub fn tree(&self, id: SessionId) -> Result<cookie_agent_protocol::SessionTree, EngineError> {
        self.inner.store.get(id)?;
        self.tree_summary(id)
    }

    fn tree_summary(
        &self,
        id: SessionId,
    ) -> Result<cookie_agent_protocol::SessionTree, EngineError> {
        Ok(cookie_agent_protocol::SessionTree {
            session: self.inner.store.summary(id)?.meta,
            children: self
                .children(id)
                .into_iter()
                .map(|child| self.tree_summary(child.session_id))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
    pub async fn resume(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        self.request(id, |reply| SessionCommand::Resume { reply })
            .await
    }
    pub async fn revert_session(
        &self,
        session_id: SessionId,
        through_seq: u64,
    ) -> Result<SessionRevertResult, EngineError> {
        let context_id = crate::plugin::plugin_context_id();
        let mut instructions = self
            .inner
            .store
            .get(session_id)?
            .log
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                cookie_agent_protocol::EventPayload::UserInputSubmitted { input }
                    if event.seq > through_seq =>
                {
                    Some(input.clone())
                }
                _ => None,
            });
        let mut instructions_override = None;
        for plugin in self.inner.plugins.interception_plugins(
            cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeRevert,
        ) {
            let result = self
                .inner
                .plugins
                .intercept_named::<_, cookie_agent_protocol::ExtensionSessionBeforeRevertResult>(
                    &plugin,
                    cookie_agent_protocol::PLUGIN_INTERCEPT_SESSION_BEFORE_REVERT_METHOD,
                    &cookie_agent_protocol::ExtensionSessionBeforeRevertParams {
                        session_id,
                        context_id: context_id.clone(),
                        through_seq,
                        instructions: instructions.clone(),
                    },
                    Some(session_id),
                    Some(&context_id),
                )
                .await;
            match result {
                Ok(result)
                    if result.action
                        == cookie_agent_protocol::ExtensionSessionBeforeRevertAction::Block =>
                {
                    let reason = result
                        .reason
                        .unwrap_or_else(|| format!("session revert blocked by {plugin}"));
                    self.record_plugin_diagnostic(
                        session_id,
                        plugin,
                        cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
                        reason.clone(),
                    );
                    return Err(EngineError::SessionOperationBlocked(reason));
                }
                Ok(result)
                    if result.action
                        == cookie_agent_protocol::ExtensionSessionBeforeRevertAction::Override =>
                {
                    if let Some(replacement) = result
                        .instructions_override
                        .filter(|value| !value.trim().is_empty())
                    {
                        instructions = Some(replacement.clone());
                        instructions_override = Some(replacement);
                    } else {
                        self.record_plugin_diagnostic(
                            session_id,
                            plugin,
                            cookie_agent_protocol::PluginDiagnosticKind::InvalidModification,
                            "empty revert instruction override".into(),
                        );
                    }
                }
                Ok(_) => {}
                Err(error) => self.record_interception_error(session_id, plugin, error),
            }
        }
        self.request(session_id, |reply| SessionCommand::Revert {
            through_seq,
            instructions_override,
            reply,
        })
        .await
    }
    pub async fn fork_session(
        &self,
        session_id: SessionId,
        through_seq: u64,
    ) -> Result<SessionForkResult, EngineError> {
        let context_id = crate::plugin::plugin_context_id();
        for plugin in self.inner.plugins.interception_plugins(
            cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeFork,
        ) {
            let result = self
                .inner
                .plugins
                .intercept_named::<_, cookie_agent_protocol::ExtensionSessionBeforeForkResult>(
                    &plugin,
                    cookie_agent_protocol::PLUGIN_INTERCEPT_SESSION_BEFORE_FORK_METHOD,
                    &cookie_agent_protocol::ExtensionSessionBeforeForkParams {
                        session_id,
                        context_id: context_id.clone(),
                        through_seq,
                    },
                    Some(session_id),
                    Some(&context_id),
                )
                .await;
            match result {
                Ok(result)
                    if result.action == cookie_agent_protocol::ExtensionAllowBlockAction::Block =>
                {
                    let reason = result
                        .reason
                        .unwrap_or_else(|| format!("session fork blocked by {plugin}"));
                    self.record_plugin_diagnostic(
                        session_id,
                        plugin,
                        cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
                        reason.clone(),
                    );
                    return Err(EngineError::SessionOperationBlocked(reason));
                }
                Ok(_) => {}
                Err(error) => self.record_interception_error(session_id, plugin, error),
            }
        }
        self.request(session_id, |reply| SessionCommand::Fork {
            through_seq,
            reply,
        })
        .await
    }
    pub fn set_permission_mode(
        &self,
        session_id: SessionId,
        mode: PermissionMode,
    ) -> Result<(), EngineError> {
        self.inner.store.get(session_id)?;
        self.inner
            .permission_modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, mode);
        Ok(())
    }
    pub(crate) fn permission_mode(&self, session_id: SessionId) -> PermissionMode {
        self.inner
            .permission_modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session_id)
            .copied()
            .unwrap_or_default()
    }
    pub async fn rename_session(
        &self,
        params: SessionRenameParams,
    ) -> Result<SessionRenameResult, EngineError> {
        let session_id = params.session_id;
        let reset = matches!(params.change, SessionRenameChange::Reset);
        let mut result = self
            .request(session_id, |reply| SessionCommand::Rename { params, reply })
            .await?;
        if reset {
            self.generate_title_after_reset(session_id).await?;
            result.session = self.inner.store.get(session_id)?.metadata();
        }
        Ok(result)
    }
}

fn validated_tree_usage_edge(
    origin: &SessionOrigin,
    child_session_id: SessionId,
    known: &HashSet<(InvocationId, SessionId, SessionId)>,
) -> Option<(SessionId, (SessionId, InvocationId))> {
    match origin {
        SessionOrigin::Delegated {
            parent_session_id,
            invocation_id,
            ..
        } if known.contains(&(*invocation_id, *parent_session_id, child_session_id)) => {
            Some((*parent_session_id, (child_session_id, *invocation_id)))
        }
        _ => None,
    }
}

fn merge_session_tree_usage(
    session_id: SessionId,
    children: &HashMap<SessionId, Vec<(SessionId, InvocationId)>>,
    rollups: &HashMap<SessionId, UsageRollup>,
    usage: &mut UsageRollup,
) -> Result<u64, EngineError> {
    let mut visited = HashSet::new();
    let mut pending = vec![(session_id, None)];
    let mut session_count = 0_u64;
    while let Some((current_id, incoming_invocation)) = pending.pop() {
        if !visited.insert(current_id) {
            let invocation_id = incoming_invocation.expect("only descendants can repeat");
            return Err(
                crate::delegation_events::DelegationEventError::Corrupt(invocation_id).into(),
            );
        }
        if let Some(rollup) = rollups.get(&current_id) {
            crate::usage::merge(usage, rollup);
            session_count = session_count.saturating_add(1);
        }
        if let Some(descendants) = children.get(&current_id) {
            pending.extend(
                descendants
                    .iter()
                    .map(|(child_id, invocation_id)| (*child_id, Some(*invocation_id))),
            );
        }
    }
    Ok(session_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_usage_tree_cycle_is_reported_as_delegation_corruption() {
        let first = SessionId::new_v7();
        let second = SessionId::new_v7();
        let first_to_second = InvocationId::new_v7();
        let second_to_first = InvocationId::new_v7();
        let first_run = cookie_agent_protocol::RunId::new_v7();
        let second_run = cookie_agent_protocol::RunId::new_v7();
        let first_origin = SessionOrigin::Delegated {
            root_session_id: first,
            parent_session_id: second,
            parent_run_id: second_run,
            parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            invocation_id: second_to_first,
            depth: 2,
        };
        let second_origin = SessionOrigin::Delegated {
            root_session_id: first,
            parent_session_id: first,
            parent_run_id: first_run,
            parent_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            invocation_id: first_to_second,
            depth: 1,
        };
        let known = HashSet::from([
            (first_to_second, first, second),
            (second_to_first, second, first),
        ]);
        let mut children: HashMap<SessionId, Vec<(SessionId, InvocationId)>> = HashMap::new();
        for (session_id, origin) in [(first, first_origin), (second, second_origin)] {
            let (parent_id, edge) = validated_tree_usage_edge(&origin, session_id, &known)
                .expect("matching origin and reservation triple");
            children.entry(parent_id).or_default().push(edge);
        }
        let rollups = HashMap::from([
            (first, UsageRollup::default()),
            (second, UsageRollup::default()),
        ]);
        let error =
            merge_session_tree_usage(first, &children, &rollups, &mut UsageRollup::default())
                .expect_err("cycle must fail");
        assert!(matches!(
            error,
            EngineError::DelegationEvents(
                crate::delegation_events::DelegationEventError::Corrupt(invocation_id)
            ) if invocation_id == second_to_first
        ));
    }
}

use std::{collections::HashSet, sync::Arc};

use cookie_agent_protocol::{
    AgentId, ChildSummary, PermissionMode, RunSelection, SessionForkResult, SessionId, SessionMeta,
    SessionOrigin, SessionRenameChange, SessionRenameParams, SessionRenameResult,
    SessionRevertResult,
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
            .all()
            .into_iter()
            .map(|session| session.metadata())
            .collect()
    }
    pub fn get_session(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        Ok(self.inner.store.get(id)?.metadata())
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
            .journal
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
            .all()
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
                        status: child.status,
                        usage: child.usage,
                    })
                }
                _ => None,
            })
            .collect()
    }
    pub fn tree(&self, id: SessionId) -> Result<cookie_agent_protocol::SessionTree, EngineError> {
        Ok(cookie_agent_protocol::SessionTree {
            session: self.inner.store.get(id)?.metadata(),
            children: self
                .children(id)
                .into_iter()
                .map(|child| self.tree(child.session_id))
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
        self.request(session_id, |reply| SessionCommand::Revert {
            through_seq,
            reply,
        })
        .await
    }
    pub async fn fork_session(
        &self,
        session_id: SessionId,
        through_seq: u64,
    ) -> Result<SessionForkResult, EngineError> {
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

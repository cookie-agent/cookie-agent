use super::*;

impl Engine {
    pub fn create_session(&self, selection: RunSelection) -> Result<SessionMeta, EngineError> {
        let snapshot = self.inner.model_manager.current();
        let agents = self.materialize_agents(snapshot.model_set())?;
        let agent = resolve_agent(&agents, &selection.agent)?.clone();
        if !agent.runnable_as_root {
            return Err(EngineError::IneligibleAgent(selection.agent));
        }
        let policy = freeze_root_agent_policy(
            &agent,
            agents,
            Arc::clone(&snapshot),
            &selection.model,
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
            model_snapshot_fingerprint: protocol_digest(
                policy.model_snapshot.model_set().fingerprint(),
            )?,
        };
        let meta = session_meta(id, SessionOrigin::Root, cwd_identity, selection);
        self.inner.store.create(meta.clone(), creation)?;
        self.inner
            .session_model_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, snapshot);
        self.spawn_actor(id);
        Ok(meta)
    }

    #[must_use]
    pub fn list_sessions(&self) -> Vec<SessionMeta> {
        self.inner
            .store
            .all()
            .into_iter()
            .map(|session| session.meta)
            .collect()
    }
    pub fn get_session(&self, id: SessionId) -> Result<SessionMeta, EngineError> {
        Ok(self.inner.store.get(id)?.meta)
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
            session: self.inner.store.get(id)?.meta,
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
            result.session = self.inner.store.get(session_id)?.meta;
        }
        Ok(result)
    }
    #[must_use]
    pub fn list_agents(&self) -> AgentListResult {
        let snapshot = self.inner.model_manager.current();
        let agents = self
            .materialize_agents(snapshot.model_set())
            .expect("current model snapshot resolves authored agents")
            .descriptors()
            .into_iter()
            .map(wire_agent_descriptor)
            .collect::<Vec<_>>();
        let revision = cookie_agent_protocol::SnapshotRevision::new(format!(
            "sha256:{}",
            Sha256Digest::of_bytes(
                &serde_json::to_vec(&agents).expect("agent descriptors serialize")
            )
            .as_str()
        ))
        .expect("valid agent revision");
        AgentListResult {
            revision,
            model_revision: cookie_agent_protocol::SnapshotRevision::new(snapshot.revision())
                .expect("model manager revision is validated"),
            generated_at: snapshot
                .generated_at()
                .parse()
                .expect("model snapshot timestamp is valid"),
            agents,
        }
    }
}

pub(crate) fn session_meta(
    id: SessionId,
    origin: SessionOrigin,
    cwd_identity: cookie_agent_protocol::CwdIdentity,
    creation_selection: RunSelection,
) -> SessionMeta {
    SessionMeta {
        meta_schema_version: cookie_agent_protocol::SessionMetaSchemaVersion::current(),
        session_id: id,
        origin,
        cwd_identity,
        creation_selection,
        title: None,
        title_updated_seq: 0,
        last_event_seq: 1,
        status: SessionStatus::Idle,
    }
}

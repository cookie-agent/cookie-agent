//! The ACP agent's own subagents as cookie child sessions
//! (`docs/site/specs/acp-v2.md` section 8).
//!
//! The agent announces a subagent with `subagent_update` in its parent's
//! session: the child's ACP session ID, its title and description, and the
//! child's state mirrored. Its work then arrives under the child's own
//! session ID. Cookie makes the child a delegated session of the parent's
//! (recorded `how: subagent`), routes the child's stream to it, and listens
//! to it as to any live session: the child's runs are its own work, started
//! as wakes ([`super::wakes`]).
//!
//! These children are the agent's, not cookie's delegations: they report
//! nothing to their parent (the parent agent gets the child's result
//! itself), take no prompts, and do not outlive the agent's process.

use std::sync::Arc;

use cookie_agent_protocol::{
    AcpSessionRecord, AcpSessionStart, RunId, SessionId, SessionOrigin, SessionTitle,
    SessionTitleChange, StoredEvent, ToolCallId,
};
use serde_json::Value;

use super::super::{Engine, EngineError, Event, event_origin};
use super::runner::LiveSession;

fn origin() -> cookie_agent_protocol::EventOrigin {
    event_origin("engine:acp")
}

/// Whether `events` are those of an ACP agent's own subagent session.
pub(crate) fn is_acp_subagent(events: &[Arc<StoredEvent>]) -> bool {
    events.iter().any(|event| {
        matches!(
            &event.payload,
            Event::AcpSessionStarted { record } if record.how == AcpSessionStart::Subagent
        )
    })
}

/// The ACP adapters whose subagents get a prompt of their own rather than
/// the session's system prompt (Claude Code's: a subagent runs on its own
/// definition), by the name their `initialize` answer gives.
const OWN_PROMPT_SUBAGENTS: [&str; 2] = [
    "@agentclientprotocol/claude-agent-acp",
    "@zed-industries/claude-code-acp",
];

/// Whether the subagents of the agent that answered `initialize` with
/// `init` get the session's system prompt: an agent that takes one, and is
/// not known to give its subagents a prompt of their own (Codex hands its
/// on to the threads it spawns).
pub(crate) fn subagents_inherit_system_prompt(init: &super::wire::InitializeResponse) -> bool {
    let name = init.info.get("name").and_then(Value::as_str).unwrap_or("");
    init.capabilities.session("systemPrompt") && !OWN_PROMPT_SUBAGENTS.contains(&name)
}

impl Engine {
    /// A `subagent_update` in `parent`'s session, during its run `run` or
    /// between runs: the child's cookie session, made on first sight, and
    /// its title.
    pub(super) async fn acp_subagent_update(
        &self,
        parent: &LiveSession,
        run: Option<RunId>,
        update: &Value,
    ) {
        let Some(child_acp) = update
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let title = ["title", "description"].into_iter().find_map(|key| {
            update
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        });
        let known = self
            .inner
            .acp
            .subagents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(child_acp)
            .copied();
        let child = match known {
            Some(child) => child,
            None => match self.create_acp_subagent(parent, run, child_acp).await {
                Ok(child) => child,
                Err(error) => {
                    self.acp_notice(
                        parent.session,
                        run,
                        format!("The agent's subagent could not be shown: {error}"),
                    )
                    .await;
                    return;
                }
            },
        };
        if let Some(title) = title {
            let _ = self.title_acp_subagent(child, title).await;
        }
    }

    /// Makes the cookie session of the agent's subagent `child_acp`, a
    /// child of `parent`'s, and starts listening to its stream.
    async fn create_acp_subagent(
        &self,
        parent: &LiveSession,
        run: Option<RunId>,
        child_acp: &str,
    ) -> Result<SessionId, EngineError> {
        let projection = self.inner.store.get(parent.session)?;
        let events = projection.log.event_snapshot();
        let (selection, agent, revisions) = events
            .iter()
            .find_map(|event| match &event.payload {
                Event::SessionCreated {
                    creation_selection,
                    creation_agent,
                    revisions,
                    ..
                } => Some((
                    creation_selection.clone(),
                    creation_agent.clone(),
                    revisions.clone(),
                )),
                _ => None,
            })
            .ok_or_else(|| EngineError::Acp("the parent session has no creation".to_owned()))?;
        let parent_run = run
            .or_else(|| {
                events.iter().rev().find_map(|event| match &event.payload {
                    Event::RunStarted { .. } => event.run_id,
                    _ => None,
                })
            })
            .ok_or_else(|| EngineError::Acp("the parent session has no run".to_owned()))?;
        // The agent's call that started the subagent is the agent's own;
        // the link names a call of no cookie run.
        let tool_call = ToolCallId::new_v7();
        let (root, depth) = match projection.meta.origin {
            SessionOrigin::Delegated {
                root_session_id,
                depth,
                ..
            } => (root_session_id, depth + 1),
            SessionOrigin::Root => (parent.session, 1),
        };
        let short_id = {
            let existing = self.existing_tree_handles(root)?;
            super::super::handles::generate_handle(
                selection.agent.as_str(),
                &existing,
                super::super::handles::random_hex,
            )?
        };
        let child = SessionId::new_v7();
        let creation = Event::SessionCreated {
            cron: None,
            history_format: cookie_agent_protocol::HISTORY_FORMAT,
            origin: SessionOrigin::Delegated {
                root_session_id: root,
                parent_session_id: parent.session,
                parent_run_id: parent_run,
                parent_tool_call_id: tool_call,
                invocation_id: super::super::helpers::invocation_id(
                    parent.session,
                    parent_run,
                    tool_call,
                ),
                depth,
            },
            short_id,
            cwd_identity: projection.meta.cwd_identity.clone(),
            creation_selection: selection,
            creation_agent: agent,
            revisions,
        };
        let process = Arc::clone(&parent.process);
        // Marked as the agent's own from its first events: a client that
        // reads the tree as the session is announced already finds it.
        let mut initial = super::super::attachments::initial_attachment_events(
            &self.inherited_attachments(&projection)?,
        );
        initial.push(Event::AcpSessionStarted {
            record: AcpSessionRecord {
                agent: parent.agent.id.clone(),
                runtime_id: parent.agent.runtime.clone(),
                runtime_agent: parent.agent.agent.clone(),
                acp_session_id: child_acp.to_owned(),
                cwd: parent.cwd.clone(),
                protocol_version: process.init().protocol_version,
                agent_info: process.init().info.clone(),
                how: AcpSessionStart::Subagent,
                steering: true,
                forked_from: None,
                fork: Some(false),
                fork_point: None,
            },
        });
        let store = self.inner.store.clone();
        self.spawn_admission_blocking(move || {
            store.create_with_initial(child, origin(), creation, initial)
        })
        .await?;
        let receiver = process.route(child_acp);
        let live = LiveSession {
            session: child,
            process,
            agent: parent.agent.clone(),
            acp_session_id: child_acp.to_owned(),
            cwd: parent.cwd.clone(),
            bridge: parent.bridge.clone(),
            options: Vec::new(),
            first_prompt: false,
            events: Arc::new(tokio::sync::Mutex::new(receiver)),
            carried: Arc::default(),
            wanted: Arc::default(),
            waiting: Arc::default(),
            rounds: Arc::default(),
        };
        // What the agent passes its subagent of what cookie gave it: the
        // parent's system prompt, when cookie sent its context as one and
        // the agent hands that on.
        let inherited = super::prompt::recorded_prompt_context(&events);
        if !inherited.is_empty() && subagents_inherit_system_prompt(parent.process.init()) {
            let _ = self
                .append(
                    child,
                    None,
                    origin(),
                    Event::AcpSessionUpdated {
                        update: cookie_agent_protocol::AcpSessionUpdate {
                            prompt_context: Some(inherited),
                            ..cookie_agent_protocol::AcpSessionUpdate::default()
                        },
                    },
                )
                .await;
        }
        self.inner
            .acp
            .subagents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(child_acp.to_owned(), child);
        self.remember_live(&live);
        self.listen_between_runs(&live);
        Ok(child)
    }

    /// How a message between the agent's sessions names `peer`, the other
    /// ACP session, from `session`: a subagent's title or handle, or
    /// `parent` from a subagent of the agent's own.
    pub(super) fn acp_peer_label(&self, session: SessionId, peer: &str) -> Option<String> {
        let child = self
            .inner
            .acp
            .subagents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(peer)
            .copied();
        if let Some(child) = child {
            let meta = self.inner.store.get(child).ok()?.meta.clone();
            return meta
                .title
                .map(|title| title.as_str().to_owned())
                .or(meta.short_id);
        }
        let projection = self.inner.store.get(session).ok()?;
        is_acp_subagent(&projection.log.event_snapshot()).then(|| "parent".to_owned())
    }

    /// Names the subagent's session as the agent titles it, unless the
    /// user named it.
    async fn title_acp_subagent(&self, child: SessionId, title: &str) -> Result<(), EngineError> {
        let projection = self.inner.store.get(child)?;
        let Ok(title) = SessionTitle::new(title.chars().take(512).collect::<String>()) else {
            return Ok(());
        };
        let SessionOrigin::Delegated { invocation_id, .. } = projection.meta.origin else {
            return Ok(());
        };
        if projection.fold.titled_by_user() || projection.meta.title.as_ref() == Some(&title) {
            return Ok(());
        }
        // A delegated child's title is runless, keyed by its invocation.
        self.append(
            child,
            None,
            event_origin("engine:session-title"),
            Event::SessionTitleCommitted {
                change: SessionTitleChange::DelegatedSet {
                    title,
                    invocation_id,
                },
                input_through_seq: projection.meta.last_event_seq,
            },
        )
        .await
    }
}

impl Engine {
    /// The parent session of `session` when it is the ACP agent's own
    /// subagent.
    pub(crate) fn acp_subagent_parent(&self, session: SessionId) -> Option<SessionId> {
        let projection = self.inner.store.get(session).ok()?;
        if !is_acp_subagent(&projection.log.event_snapshot()) {
            return None;
        }
        match projection.meta.origin {
            SessionOrigin::Delegated {
                parent_session_id, ..
            } => Some(parent_session_id),
            SessionOrigin::Root => None,
        }
    }

    /// The tools the latest model request of `session` was offered
    /// (`model_tools_published`).
    pub(crate) fn latest_model_tool_names(&self, session: SessionId) -> Option<Vec<String>> {
        let projection = self.inner.store.get(session).ok()?;
        projection
            .log
            .event_snapshot()
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                Event::ModelToolsPublished { tool_names, .. } => Some(tool_names.clone()),
                _ => None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::subagents_inherit_system_prompt;

    fn init(name: &str, system_prompt: bool) -> super::super::wire::InitializeResponse {
        let mut session = serde_json::json!({});
        if system_prompt {
            session["systemPrompt"] = serde_json::json!({});
        }
        serde_json::from_value(serde_json::json!({
            "protocolVersion": 2,
            "info": {"name": name},
            "capabilities": {"session": session},
        }))
        .expect("initialize answer")
    }

    #[test]
    fn subagents_inherit_a_system_prompt_unless_their_agent_gives_them_their_own() {
        assert!(subagents_inherit_system_prompt(&init(
            "@agentclientprotocol/codex-acp",
            true
        )));
        assert!(subagents_inherit_system_prompt(&init(
            "some-new-agent",
            true
        )));
        assert!(!subagents_inherit_system_prompt(&init(
            "@agentclientprotocol/claude-agent-acp",
            true
        )));
        // Without a system prompt, the context went with the first message.
        assert!(!subagents_inherit_system_prompt(&init(
            "some-new-agent",
            false
        )));
    }
}

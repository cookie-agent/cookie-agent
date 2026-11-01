//! How a run's context before the conversation is composed: the system
//! prompt, the `AGENTS.md` turn, and the memory turn stay the previous run's
//! until a refresh point (`docs/site/specs/frozen-context.md`, section 6).

use cookie_agent_protocol::{
    EventPayload as Event, PromptBlockSpan, RefreshReason, RefreshedParts, RefreshedTools, RunId,
    SessionId, Sha256Digest,
};

use super::{Engine, EngineError, agent_md::AttachmentAgentMd};
use crate::policy::FrozenRunPolicy;

/// The context the previous run was sent, as its records hold it.
pub(super) struct PreviousContext {
    pub(super) composed_prompt: String,
    pub(super) prompt_fingerprint: Sha256Digest,
    pub(super) prompt_blocks: Vec<PromptBlockSpan>,
    /// Its `AGENTS.md` turn, as recorded before the conversation.
    pub(super) agent_md: Vec<AttachmentAgentMd>,
}

/// How a run composes its context.
pub(super) enum ContextComposition {
    /// Afresh, with no refresh to record: the session's first run, the
    /// first after a compaction, an ACP agent's, or one after a run whose
    /// prompt was not recorded in blocks (a plugin replaced it).
    Live,
    /// Afresh, at a refresh point.
    Refresh {
        reason: RefreshReason,
        drift: crate::deferred_tools::RoundDrift,
        previous: PreviousContext,
    },
    /// As the previous run's.
    Frozen(PreviousContext),
}

/// A refresh to record, with what it found changed.
pub(super) struct Refresh {
    pub(super) reason: RefreshReason,
    pub(super) drift: crate::deferred_tools::RoundDrift,
    pub(super) previous: PreviousContext,
    /// The `AGENTS.md` turn differs from the previous run's.
    pub(super) agent_md: bool,
    /// The memory turn differs from the one before.
    pub(super) memory: bool,
}

impl ContextComposition {
    pub(super) fn frozen(&self) -> Option<&PreviousContext> {
        match self {
            Self::Frozen(previous) => Some(previous),
            _ => None,
        }
    }
}

impl Engine {
    /// Decides how a run of `policy` in `session` composes its context.
    /// `preset` is the run's agent preset, which, like its agent, decides
    /// the system prompt's body.
    pub(super) fn context_composition(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
        preset: Option<&str>,
        acp: bool,
    ) -> Result<ContextComposition, EngineError> {
        if acp {
            return Ok(ContextComposition::Live);
        }
        let log = self.inner.store.log(session)?;
        if log.prompt_blocks().checkpoint_since_run {
            return Ok(ContextComposition::Live);
        }
        let events = log.event_snapshot();
        let Some((run, agent, previous_preset)) =
            events.iter().rev().find_map(|event| match &event.payload {
                Event::RunStarted {
                    agent, selection, ..
                } => event
                    .run_id
                    .map(|run| (run, agent, selection.preset.as_deref())),
                _ => None,
            })
        else {
            return Ok(ContextComposition::Live);
        };
        let Some(prompt_blocks) = events.iter().rev().find_map(|event| match &event.payload {
            Event::PromptBlocksRecorded { blocks, .. } if event.run_id == Some(run) => {
                Some(blocks.clone())
            }
            _ => None,
        }) else {
            return Ok(ContextComposition::Live);
        };
        let previous = PreviousContext {
            composed_prompt: agent.composed_prompt.clone(),
            prompt_fingerprint: agent.prompt_fingerprint.clone(),
            prompt_blocks,
            agent_md: previous_agent_md(&events, run),
        };
        if agent.agent != policy.agent.agent || previous_preset != preset {
            return Ok(ContextComposition::Refresh {
                reason: RefreshReason::AgentChanged,
                drift: crate::deferred_tools::RoundDrift::default(),
                previous,
            });
        }
        // A tool set that cannot be composed now is no reason to refresh;
        // the run's first request reports it.
        let drift = self
            .published_tool_definitions(session, policy)
            .map(|published| published.drift)
            .unwrap_or_default();
        if drift.refreshes() {
            let current = crate::release::build_id();
            let engine_updated = log
                .deferral()
                .round
                .as_ref()
                .and_then(|round| round.engine_build.as_ref())
                .is_some_and(|build| Some(build) != current.as_ref());
            return Ok(ContextComposition::Refresh {
                reason: if engine_updated {
                    RefreshReason::EngineUpdated
                } else {
                    RefreshReason::ToolsChanged
                },
                drift,
                previous,
            });
        }
        Ok(ContextComposition::Frozen(previous))
    }

    /// Records a refresh, with what it composed differently, before the
    /// run's first request: the tool round starts over from it.
    pub(super) async fn record_refresh(
        &self,
        session: SessionId,
        run: RunId,
        refresh: Refresh,
        policy: &FrozenRunPolicy,
    ) -> Result<(), EngineError> {
        let Refresh {
            reason,
            drift,
            previous,
            agent_md,
            memory,
        } = refresh;
        let changed = RefreshedParts {
            system_prompt: policy.agent.composed_prompt != previous.composed_prompt,
            tools: RefreshedTools {
                added: drift.added,
                removed: drift.removed,
                changed: drift.changed,
            },
            agent_md,
            memory,
        };
        self.append(
            session,
            Some(run),
            super::event_origin("engine:prompt"),
            Event::ContextRefreshed { reason, changed },
        )
        .await
    }
}

impl Engine {
    /// What runtimes a client attached since the previous run ended add to
    /// the model's context, recorded at this run's admission where the model
    /// reads them: each one's workspace `AGENTS.md`, and the memories of a
    /// machine new to the session (frozen context spec, section 6.3). A
    /// runtime the agent attached itself brought both in its tool's result.
    pub(super) async fn add_attached_context(
        &self,
        session: SessionId,
        run: RunId,
        preset: Option<&str>,
        agent_md: bool,
    ) -> Result<(), EngineError> {
        let log = self.inner.store.log(session)?;
        let events = log.event_snapshot();
        // The previous run's end; the run just started is the latest.
        let Some(ended) = events.iter().rev().find_map(|event| {
            matches!(event.payload, Event::RunFinished { .. }).then_some(event.seq)
        }) else {
            return Ok(());
        };
        let attached = events
            .iter()
            .filter(|event| event.seq > ended)
            .filter_map(|event| match &event.payload {
                Event::RuntimeAttached { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        if attached.is_empty() {
            return Ok(());
        }
        let attachments = self
            .session_runtimes(session)?
            .into_iter()
            .filter(|attachment| attached.contains(&attachment.name))
            .collect::<Vec<_>>();
        if agent_md {
            for attachment in attachments
                .iter()
                .filter(|attachment| attachment.workspace.is_some())
            {
                let Some(instance) = self.inner.runtimes.instance(attachment) else {
                    continue;
                };
                let Some(context) = instance.agent_md(preset).await? else {
                    continue;
                };
                if context.entries.is_empty() {
                    continue;
                }
                self.append(
                    session,
                    Some(run),
                    super::event_origin("engine:agent-md"),
                    Event::AgentMdLoaded {
                        entries: context.entries,
                        attachment: attachment.name.clone(),
                        project: None,
                        in_place: true,
                    },
                )
                .await?;
            }
        }
        // A machine is new when every attachment on it is.
        let before = self
            .session_machines(session)
            .into_iter()
            .filter(|(_, on)| on.iter().any(|name| !attached.contains(name)))
            .map(|(machine, _)| machine)
            .collect::<Vec<_>>();
        if let Some(snapshot) = self.attached_machine_memories(session, &before).await? {
            self.append(
                session,
                Some(run),
                super::event_origin("engine:memory"),
                Event::MemoryLoaded {
                    snapshot,
                    in_place: true,
                },
            )
            .await?;
        }
        Ok(())
    }
}

/// The `AGENTS.md` turn `run` recorded before the conversation.
fn previous_agent_md(
    events: &[std::sync::Arc<cookie_agent_protocol::StoredEvent>],
    run: RunId,
) -> Vec<AttachmentAgentMd> {
    events
        .iter()
        .filter(|event| event.run_id == Some(run))
        .filter_map(|event| match &event.payload {
            Event::AgentMdLoaded {
                entries,
                attachment,
                project,
                in_place: false,
            } => Some(AttachmentAgentMd {
                attachment: attachment.clone(),
                entries: entries.clone(),
                skipped: Vec::new(),
                project: project.clone(),
            }),
            _ => None,
        })
        .collect()
}

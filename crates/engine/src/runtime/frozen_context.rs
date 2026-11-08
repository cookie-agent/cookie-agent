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
        let events = log.model_context_events()?;
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
        // The prompt the run's requests carried last, with its blocks: one a
        // checkpoint inside the run composed, or its own. A run whose prompt
        // a plugin replaced records no blocks.
        let Some((prompt, prompt_blocks)) =
            events.iter().rev().find_map(|event| match &event.payload {
                Event::PromptBlocksRecorded { blocks, .. } if event.run_id == Some(run) => Some((
                    (
                        agent.composed_prompt.clone(),
                        agent.prompt_fingerprint.clone(),
                    ),
                    Some(blocks.clone()),
                )),
                Event::SystemPromptComposed {
                    composed_prompt,
                    prompt_fingerprint,
                    blocks,
                } if event.run_id == Some(run) => Some((
                    (composed_prompt.clone(), prompt_fingerprint.clone()),
                    blocks.clone(),
                )),
                _ => None,
            })
        else {
            return Ok(ContextComposition::Live);
        };
        let Some(prompt_blocks) = prompt_blocks else {
            return Ok(ContextComposition::Live);
        };
        let previous = PreviousContext {
            composed_prompt: prompt.0,
            prompt_fingerprint: prompt.1,
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
    /// Composes `run`'s system prompt afresh at a checkpoint inside it, as a
    /// run's admission composes it, and records it as the prompt the run's
    /// requests carry from here when it differs from the one they carry now
    /// (`docs/site/specs/forever-sessions.md` section 5.2): the agent's body
    /// as the live registry holds it now, the runtimes, attachments, tool
    /// instructions, and skills blocks, and the plugins' system-prompt hooks.
    /// Only the prompt changes; `policy`, the run's frozen policy, keeps its
    /// permissions, models, and limits. A prompt that cannot be composed
    /// now, too long or refused by a hook that blocks, leaves the one in
    /// force.
    pub(super) async fn recompose_run_prompt(
        &self,
        session: SessionId,
        run: RunId,
        policy: &FrozenRunPolicy,
    ) -> Result<(), EngineError> {
        if policy.acp_agent().is_some() {
            return Ok(());
        }
        let mut composed = policy.clone();
        // The agent's body as it reads now; its frozen one when the live
        // registry no longer holds it as a native agent.
        let live = self
            .current_runtime()
            .agents_for_preset(policy.preset.as_deref())
            .ok();
        let agent = live
            .as_deref()
            .and_then(|agents| agents.get(&policy.agent.agent))
            .filter(|agent| agent.acp.is_none())
            .or_else(|| policy.registry.get(&policy.agent.agent));
        let Some(agent) = agent else {
            return Ok(());
        };
        crate::policy::reset_composed_prompt(&mut composed.agent, agent)?;
        composed.prompt_blocks.clear();
        self.compose_runtimes_prompt(&mut composed, session);
        self.compose_attachments_prompt(&mut composed, session);
        match self.compose_tool_prompt_sections(&mut composed, session) {
            Err(EngineError::ToolPrompt(_)) => return Ok(()),
            composed => composed?,
        }
        self.compose_skill_listing(session, &mut composed)?;
        // A message a hook injects joins a run at its admission only.
        let hooked = match self.agent_prompt_hooks(session, &mut composed, None).await {
            Ok(hooked) => hooked,
            Err(EngineError::PluginHookFailed(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let blocks = (!hooked.replaced).then_some(composed.prompt_blocks);
        let prompt = composed.agent.composed_prompt;
        let in_force = self.run_agent_prompt(session, run)?;
        let known = self.inner.store.log(session)?.prompt_blocks().run_blocks;
        let texts = blocks
            .as_deref()
            .and_then(|blocks| crate::events::prompt_block_texts(&prompt, blocks));
        if prompt == in_force && texts == known {
            return Ok(());
        }
        self.append(
            session,
            Some(run),
            super::event_origin("engine:prompt"),
            Event::SystemPromptComposed {
                composed_prompt: prompt,
                prompt_fingerprint: composed.agent.prompt_fingerprint,
                blocks,
            },
        )
        .await
    }

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
        // Since the previous run's end; the run just started is the latest.
        let Some(attached) = log.state().attached_since_run_end().cloned() else {
            return Ok(());
        };
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
        if let Some((snapshot, shown)) = self.attached_machine_memories(session, &before).await? {
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
            self.saw_memories(session, shown).await;
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

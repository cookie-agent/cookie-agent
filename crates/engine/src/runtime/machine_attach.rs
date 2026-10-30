//! Attaching and detaching machines from a session's own run
//! (`docs/site/specs/machines-on-demand.md` sections 3 and 4):
//! `attach_machine` and `detach_machine` record the usual
//! `runtime_attached` and `runtime_detached` events while the session runs,
//! and no machine may carry two attachments of the session that offer a
//! tool of the same name.

use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use cookie_agent_protocol::{AgentMdEntry, MachineName, RuntimeAttachment, RuntimeId, SessionId};
use cookie_agent_runtime_api::SessionToolContext;

use super::{Engine, EngineError, attachments::attached_event};
use crate::runtimes::RuntimeInstance;

/// What `attach_machine` tells the model at once, since the runtimes block
/// is frozen for the run.
pub(crate) struct AttachedMachine {
    pub(crate) attachment: RuntimeAttachment,
    pub(crate) machine: Option<MachineName>,
    pub(crate) platform: Option<String>,
    pub(crate) home: Option<String>,
    /// The workspace's `AGENTS.md` files, as a run would load them.
    pub(crate) agent_md: Vec<AgentMdEntry>,
}

impl Engine {
    /// Attaches `capability` of `runtime_id` (at `workspace` when it takes
    /// one) to `session` from the session's own run. The same checks as
    /// [`Engine::attach_runtime`] apply, except that the session may be
    /// running.
    pub(crate) async fn attach_from_run(
        &self,
        session: SessionId,
        runtime_id: &str,
        capability: &str,
        workspace: Option<PathBuf>,
        preset: Option<&str>,
    ) -> Result<AttachedMachine, EngineError> {
        self.ensure_session_owned(session)?;
        self.refuse_second_acp_runtime(session, runtime_id)?;
        let instance = self
            .ensure_runtime(runtime_id, Some(capability), workspace.as_deref())
            .await?;
        if let Some(connection) = instance.connection() {
            connection.retry_failed_mcp();
        }
        self.refuse_overlap(session, &instance)?;
        let attachment = self
            .on_actor(session, move |engine| {
                let projection = engine.inner.store.get(session)?;
                let mut attachments = engine.attachments_of(&projection);
                let attachment = super::attachments::push_instance(&mut attachments, &instance)?;
                engine.append_direct(
                    session,
                    None,
                    super::event_origin("tool:attach_machine"),
                    attached_event(&attachment),
                )?;
                Ok(attachment)
            })
            .await?;
        let agent_md = match self.inner.runtimes.instance(&attachment) {
            Some(instance) if attachment.workspace.is_some() => instance
                .agent_md(preset)
                .await
                .ok()
                .flatten()
                .map(|context| context.entries)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let listing = self
            .runtime_templates()
            .into_iter()
            .find(|template| template.runtime_id == runtime_id)
            .map(|template| template.listing);
        Ok(AttachedMachine {
            machine: self.runtime_machine(runtime_id),
            platform: self
                .inner
                .runtimes
                .instance(&attachment)
                .and_then(|instance| {
                    instance
                        .info()
                        .workspace()
                        .and_then(|workspace| workspace.platform().map(str::to_owned))
                })
                .or_else(|| {
                    listing
                        .as_ref()
                        .and_then(|listing| listing.platform.clone())
                }),
            home: listing.and_then(|listing| listing.home),
            agent_md,
            attachment,
        })
    }

    /// Detaches the attachment of `capability` of `runtime_id` from
    /// `session`, from the session's own run. Refused while one of the
    /// session's background calls runs on it.
    pub(crate) async fn detach_from_run(
        &self,
        session: SessionId,
        runtime_id: &str,
        capability: &str,
    ) -> Result<RuntimeAttachment, EngineError> {
        self.ensure_session_owned(session)?;
        let projection = self.inner.store.get(session)?;
        let attachment = self
            .attachments_of(&projection)
            .into_iter()
            .find(|attachment| {
                attachment.runtime_id.as_str() == runtime_id
                    && attachment.capability.as_str() == capability
            })
            .ok_or_else(|| {
                EngineError::Runtime(format!(
                    "this session has no attachment of `{runtime_id}/{capability}`"
                ))
            })?;
        if attachment.runtime_id.as_str() == RuntimeId::GLOBAL {
            return Err(EngineError::Runtime(
                "`global` cannot be detached".to_owned(),
            ));
        }
        let running = self.background_calls_on(session, &attachment);
        if !running.is_empty() {
            return Err(EngineError::Runtime(format!(
                "calls still run on `{runtime_id}/{capability}`: {}; stop them with \
                 stop_background first",
                running.join(", ")
            )));
        }
        let name = attachment.name.clone();
        self.on_actor(session, move |engine| {
            engine.append_direct(
                session,
                None,
                super::event_origin("tool:detach_machine"),
                super::Event::RuntimeDetached { name },
            )
        })
        .await?;
        Ok(attachment)
    }

    /// Whether `agent` of `preset` is an ACP agent.
    pub(crate) fn selects_acp_agent(
        &self,
        agent: &cookie_agent_protocol::AgentId,
        preset: Option<&str>,
    ) -> bool {
        self.current_runtime()
            .agents_for_preset(preset)
            .ok()
            .and_then(|agents| agents.get(agent).map(|agent| agent.acp.is_some()))
            .unwrap_or(false)
    }

    /// The preset of run `run`, while it runs.
    pub(crate) fn run_preset(&self, run: cookie_agent_protocol::RunId) -> Option<String> {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .and_then(|active| active.policy.preset.clone())
    }

    /// `session`'s attachment of `target`'s runtime capability, if any.
    pub(crate) fn attachment_of_capability(
        &self,
        session: SessionId,
        target: &crate::DelegatedCapability,
    ) -> Option<RuntimeAttachment> {
        let projection = self.inner.store.get(session).ok()?;
        self.attachments_of(&projection)
            .into_iter()
            .find(|attachment| {
                attachment.runtime_id == target.runtime_id
                    && attachment.capability == target.capability
            })
    }

    /// `session`'s background calls still running on `attachment`: a call
    /// of one of its tools, by its bare or its `<attachment>_` name.
    fn background_calls_on(
        &self,
        session: SessionId,
        attachment: &RuntimeAttachment,
    ) -> Vec<String> {
        let Some(instance) = self.inner.runtimes.instance(attachment) else {
            return Vec::new();
        };
        let bare = tool_names(&instance, &SessionToolContext::new(session));
        let tools = bare
            .iter()
            .map(|tool| format!("{}_{tool}", attachment.name))
            .chain(bare.iter().cloned())
            .collect();
        self.inner.background.running_calls_of(session, &tools)
    }

    /// A session runs its ACP agent on the one attached runtime that offers
    /// it, so a second such runtime is refused.
    pub(crate) fn refuse_second_acp_runtime(
        &self,
        session: SessionId,
        runtime_id: &str,
    ) -> Result<(), EngineError> {
        let Some(offers) = self.session_acp_offers(session)? else {
            return Ok(());
        };
        let attached = self.session_runtime_ids(session)?;
        let offering = offers
            .runtimes()
            .into_iter()
            .filter(|offering| attached.contains(offering))
            .collect::<Vec<_>>();
        if offers
            .runtimes()
            .iter()
            .any(|offering| offering.as_str() == runtime_id)
            && let Some(other) = offering.iter().find(|other| other.as_str() != runtime_id)
        {
            return Err(EngineError::Runtime(format!(
                "runtime `{runtime_id}` offers ACP agent `{}`, which this session runs on \
                 runtime `{other}`; a session runs it on one runtime, so detach `{other}` \
                 first",
                offers.id
            )));
        }
        Ok(())
    }

    /// Refuses `instance` when one of its tools has the name of a tool that
    /// another of `session`'s attachments on the same machine offers
    /// (machines on demand, section 4). A runtime the daemon cannot place
    /// is its own machine.
    pub(crate) fn refuse_overlap(
        &self,
        session: SessionId,
        instance: &Arc<RuntimeInstance>,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        self.refuse_overlap_with(
            &self.attachments_of(&projection),
            instance,
            &SessionToolContext::new(session),
        )
    }

    /// [`Engine::refuse_overlap`] against the attachments `existing`, for a
    /// session that may not exist yet (its creation, a subagent's
    /// capabilities): tool lists are read with `context`.
    pub(crate) fn refuse_overlap_with(
        &self,
        existing: &[RuntimeAttachment],
        instance: &Arc<RuntimeInstance>,
        context: &SessionToolContext,
    ) -> Result<(), EngineError> {
        let runtime_id = instance.info().id().to_owned();
        if runtime_id == RuntimeId::GLOBAL {
            return Ok(());
        }
        let Some(machine) = self.runtime_machine(&runtime_id) else {
            return Ok(());
        };
        let offered = tool_names(instance, context);
        if offered.is_empty() {
            return Ok(());
        }
        for attachment in existing {
            if attachment.runtime_id.as_str() == RuntimeId::GLOBAL
                || self
                    .runtime_machine(attachment.runtime_id.as_str())
                    .as_ref()
                    != Some(&machine)
            {
                continue;
            }
            let Some(other) = self.inner.runtimes.instance(attachment) else {
                continue;
            };
            if Arc::ptr_eq(&other, instance) {
                // Attaching the instance twice is refused on its own.
                continue;
            }
            let shared = tool_names(&other, context)
                .intersection(&offered)
                .cloned()
                .collect::<Vec<_>>();
            if !shared.is_empty() {
                return Err(EngineError::Runtime(overlap_refusal(
                    attachment, &machine, &shared, instance,
                )));
            }
        }
        Ok(())
    }
}

/// The names of the tools `instance` offers a session.
fn tool_names(instance: &RuntimeInstance, context: &SessionToolContext) -> BTreeSet<String> {
    instance
        .sources()
        .1
        .iter()
        .filter_map(|provider| provider.tools_for_session(context).ok())
        .flatten()
        .map(|tool| tool.name)
        .collect()
}

fn overlap_refusal(
    existing: &RuntimeAttachment,
    machine: &MachineName,
    shared: &[String],
    instance: &RuntimeInstance,
) -> String {
    let tools = shared
        .iter()
        .map(|tool| format!("`{tool}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!(
        "machine `{machine}` already has `{}/{}` attached{}, which offers {tools}: one machine \
         offers each tool once. Use absolute paths to reach another directory there, or detach \
         it first",
        existing.runtime_id,
        existing.capability,
        existing
            .workspace
            .as_deref()
            .map(|workspace| format!(" at {workspace}"))
            .unwrap_or_default(),
    );
    if instance.info().id() != existing.runtime_id.as_str() {
        message.push_str(&format!(
            ". If runtime `{}` is a container or another environment with files of its own, \
             give it a machine of its own with `cookie machine unassign {}`",
            instance.info().id(),
            instance.info().id()
        ));
    }
    message
}

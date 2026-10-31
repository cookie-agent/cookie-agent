//! Attaching and detaching machines from a session's own run
//! (`docs/site/specs/machines-on-demand.md` sections 3 and 4):
//! `attach_machine` and `detach_machine` record the usual
//! `runtime_attached` and `runtime_detached` events while the session runs,
//! and no machine may carry two attachments of the session that offer a
//! tool of the same name.

use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use cookie_agent_protocol::{
    AgentMdEntry, AttachmentName, MachineName, RuntimeAttachment, RuntimeId, SessionId,
};
use cookie_agent_runtime_api::SessionToolContext;

use super::{Engine, EngineError, attachments::attached_event};
use crate::{policy::FrozenRunPolicy, runtimes::RuntimeInstance};

/// What `attach_machine` tells the model at once, since the runtimes block
/// is frozen for the run.
pub(crate) struct AttachedMachine {
    pub(crate) attachment: RuntimeAttachment,
    pub(crate) machine: Option<MachineName>,
    pub(crate) platform: Option<String>,
    pub(crate) home: Option<String>,
    /// The workspace's `AGENTS.md` files, as a run would load them.
    pub(crate) agent_md: Vec<AgentMdEntry>,
    /// The tools the attachment brings, sorted by name; `None` when the
    /// attaching run is gone or they cannot be listed.
    pub(crate) tools: Option<Vec<AttachedTool>>,
}

/// One tool an attachment brings to a session, as its run publishes it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AttachedTool {
    /// The name the model calls it by.
    pub(crate) name: String,
    /// The `machine` a call names to run it on the attachment's machine,
    /// when the tool is routed (machines on demand, section 5).
    pub(crate) machine: Option<String>,
    /// The group it is announced under while deferred: the model finds it
    /// with `tool_search` before calling it; `None` once it is declared.
    pub(crate) deferred: Option<String>,
    /// The first line of its description.
    pub(crate) summary: String,
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
        policy: Option<&FrozenRunPolicy>,
    ) -> Result<AttachedMachine, EngineError> {
        let preset = policy.and_then(|policy| policy.preset.as_deref());
        self.ensure_session_owned(session)?;
        self.refuse_second_acp_runtime(session, runtime_id)?;
        let instance = self
            .ensure_runtime(runtime_id, Some(capability), workspace.as_deref())
            .await?;
        if let Some(connection) = instance.connection() {
            connection.retry_failed_mcp();
        }
        let runtime_id_owned = runtime_id.to_owned();
        let attachment = self
            .on_actor(session, move |engine| {
                // Checked on the actor, so of two attaches under way only
                // the first can pass.
                engine.refuse_second_acp_runtime(session, runtime_id_owned.as_str())?;
                let projection = engine.inner.store.get(session)?;
                let mut attachments = engine.attachments_of(&projection);
                engine.refuse_overlap_with(
                    &attachments,
                    &instance,
                    &SessionToolContext::new(session),
                )?;
                let attachment = super::attachments::push_instance(&mut attachments, &instance)?;
                engine.append_direct(
                    session,
                    None,
                    super::event_origin("engine:machines"),
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
            // The attachment stands even when its tools cannot be listed.
            tools: policy.and_then(|policy| {
                self.attachment_tools(session, policy, &attachment.name)
                    .ok()
            }),
            attachment,
        })
    }

    /// The tools `attachment` brings to `session` as a run of `policy`
    /// publishes them now: by their published names, with the `machine` that
    /// routes a call there and the group of those still deferred, the tools
    /// the agent hides from its ACP agent left out.
    pub(crate) fn attachment_tools(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
        attachment: &AttachmentName,
    ) -> Result<Vec<AttachedTool>, EngineError> {
        let composition = self.compose_for_policy(session, policy)?;
        let (entries, _) = self.route_tools(session, composition.tools)?;
        let hide = self
            .current_runtime()
            .agents
            .get(&policy.agent.agent)
            .and_then(|agent| agent.document.frontmatter.acp.clone())
            .map(|acp| acp.hidden_tools())
            .unwrap_or_default();
        let defers = self.defers_tools(policy);
        let fold = self.inner.store.get(session)?.log.deferral();
        let config = self.inner.config.load();
        let mut tools = entries
            .into_iter()
            .filter(|(tool, _)| {
                !hide
                    .iter()
                    .any(|pattern| cookie_agent_config::simple_wildcard_match(pattern, &tool.name))
            })
            .filter_map(|(tool, routes)| {
                // A routed tool runs here through its member of this machine.
                let (machine, source, spec) = match routes {
                    Some(routes) => {
                        let (machine, member) =
                            routes.machines.into_iter().find(|(_, member)| {
                                member.source.attachment.as_ref() == Some(attachment)
                            })?;
                        (Some(machine), member.source, member.spec)
                    }
                    None if tool.source.attachment.as_ref() == Some(attachment) => {
                        (None, tool.source, tool.spec)
                    }
                    None => return None,
                };
                // A tool outside the round is deferred until a result loads
                // it: the policy defers it, or it arrived mid-round
                // (deferred tools, section 8.2).
                let deferred = defers
                    && !fold.loaded.contains(&tool.name)
                    && match &fold.round {
                        Some(round) => !round.tools.iter().any(|eager| eager.name == tool.name),
                        None => {
                            !fold.folded.contains(&tool.name)
                                && crate::deferred_tools::policy_defers(
                                    &source,
                                    &spec.name,
                                    &config.runtime.deferred_tools,
                                )
                        }
                    };
                let deferred =
                    deferred.then(|| crate::deferred_tools::group_of(&source, &spec.name).name);
                Some(AttachedTool {
                    name: tool.name,
                    machine,
                    deferred,
                    summary: spec
                        .description
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                })
            })
            .collect::<Vec<_>>();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(tools)
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
                super::event_origin("engine:machines"),
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

    /// The policy of run `run`, while it runs.
    pub(crate) fn run_policy(
        &self,
        run: cookie_agent_protocol::RunId,
    ) -> Option<Arc<FrozenRunPolicy>> {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .map(|active| Arc::clone(&active.policy))
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
    /// another attachment among `existing` on the same machine offers
    /// (machines on demand, section 4); a runtime the daemon cannot place is
    /// its own machine. The session may not exist yet (its creation, a
    /// subagent's capabilities): tool lists are read with `context`.
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

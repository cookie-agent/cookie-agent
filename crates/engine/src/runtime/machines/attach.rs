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

use crate::runtime::{Engine, EngineError, attachments::attached_event};
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
    /// The memories of the machine, when it is new to the session: the
    /// memory turn's machines section, as the model reads it.
    pub(crate) memories: Option<String>,
    /// The tools the attachment brings, sorted by name; `None` when the
    /// attaching run is gone or they cannot be listed.
    pub(crate) tools: Option<Vec<AttachedTool>>,
    /// For an ACP agent, the machine the attachment is on when it is not
    /// the agent's own: its harness's tools do not reach it
    /// (`docs/site/specs/acp-several-runtimes.md` section 5).
    pub(crate) beyond_agent: Option<String>,
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
        let instance = self
            .ensure_runtime(runtime_id, Some(capability), workspace.as_deref())
            .await?;
        if let Some(connection) = instance.connection() {
            connection.retry_failed_mcp();
        }
        let machines_before = self.machine_names(session);
        let attachment = self
            .on_actor(session, move |engine| {
                let projection = engine.inner.store.get(session)?;
                let mut attachments = engine.attachments_of(&projection);
                if !engine.attaches_on_acp_host_machine(session, &attachments, &instance)? {
                    engine.refuse_overlap_with(
                        &attachments,
                        &instance,
                        &SessionToolContext::new(session),
                    )?;
                }
                let attachment =
                    crate::runtime::attachments::push_instance(&mut attachments, &instance)?;
                engine.append_direct(
                    session,
                    None,
                    crate::runtime::event_origin("engine:machines"),
                    attached_event(&attachment),
                )?;
                Ok(attachment)
            })
            .await?;
        // Its AGENTS.md and the memories of a machine new to the session
        // reach the model in the tool's result, where it attached them
        // (frozen context spec, section 6.3).
        let memories = self
            .attached_machine_memories(session, &machines_before)
            .await
            .unwrap_or_else(|error| {
                eprintln!("cookie-agent: session {session}: machine memories not read: {error}");
                None
            })
            .and_then(|snapshot| cookie_agent_protocol::model_text::machine_memory_turn(&snapshot));
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
        let machine = self.runtime_machine(runtime_id);
        let beyond_agent = self
            .session_acp_offers(session)
            .ok()
            .flatten()
            .and_then(|offers| self.place_acp_agent(session, &offers).ok())
            .filter(|agent| {
                agent.runtime.as_str() != runtime_id
                    && (machine.is_none()
                        || self.runtime_machine(agent.runtime.as_str()) != machine)
            })
            .map(|_| {
                machine
                    .as_ref()
                    .map_or_else(|| runtime_id.to_owned(), ToString::to_string)
            });
        Ok(AttachedMachine {
            beyond_agent,
            machine,
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
            memories,
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
    /// routes a call there and the group of those still deferred. Groups
    /// that are off are not composed.
    pub(crate) fn attachment_tools(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
        attachment: &AttachmentName,
    ) -> Result<Vec<AttachedTool>, EngineError> {
        let composition = self.compose_for_policy(session, policy)?;
        let (entries, _) = self.route_tools(session, composition.tools)?;
        let defers = self.defers_tools(policy);
        let projection = self.inner.store.get(session)?;
        let exposure = crate::exposure::Layers::of(&projection, policy);
        let fold = projection.log.deferral();
        let config = self.inner.config.load();
        let mut tools = entries
            .into_iter()
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
                                    exposure.forced(&source, &spec.name),
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
        let detached = attachment.clone();
        self.on_actor(session, move |engine| {
            engine.refuse_detaching_acp_host(session, &detached)?;
            engine.append_direct(
                session,
                None,
                crate::runtime::event_origin("engine:machines"),
                crate::runtime::Event::RuntimeDetached {
                    name: detached.name.clone(),
                },
            )?;
            engine.forget_detached_acp_choice(
                session,
                &detached,
                crate::runtime::event_origin("engine:machines"),
            )
        })
        .await?;
        Ok(attachment)
    }

    /// The runtimes that offer `agent` of `preset`, when it is an ACP
    /// agent.
    pub(crate) fn selected_acp_offers(
        &self,
        agent: &cookie_agent_protocol::AgentId,
        preset: Option<&str>,
    ) -> Option<crate::runtime_snapshot::AcpOffers> {
        self.current_runtime()
            .agents_for_preset(preset)
            .ok()
            .and_then(|agents| agents.get(agent).and_then(|agent| agent.acp.clone()))
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

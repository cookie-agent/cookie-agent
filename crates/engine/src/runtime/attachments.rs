//! Session attachments: which runtime instances a session uses, under which
//! session-unique names (`docs/site/specs/runtimes.md` section 5).

use std::{path::PathBuf, sync::Arc};

use cookie_agent_protocol::{
    AttachmentName, EventOrigin, RuntimeAttachment, RuntimeId, SessionId, SessionStatus,
};

use super::{Engine, EngineError, Event};
use crate::{
    DialInRefusal,
    runtimes::{
        RuntimeInstance, RuntimeInstanceSummary, RuntimeTemplate, RuntimeTemplateSummary,
        SourcedProvider,
    },
    session::SessionProjection,
};

/// A runtime ID reserved for one runtime dialing in, from
/// [`Engine::claim_dial_in_runtime`] until it is accepted or dropped.
pub struct DialInClaim {
    engine: Engine,
    template: Arc<crate::runtimes::DialInTemplate>,
}

impl DialInClaim {
    /// Takes the runtime that dialed in over `transport`: initializes it,
    /// makes it the runtime of the claimed ID, and reopens the instances
    /// its sessions use.
    pub async fn accept(
        self,
        transport: Box<dyn cookie_agent_protocol::Transport>,
    ) -> Result<(), DialInRefusal> {
        let inner = &self.engine.inner;
        let runtime_id = crate::runtimes::RuntimeTemplate::runtime_id(self.template.as_ref());
        let settings = crate::runtimes::runtime_settings(&inner.config.load(), runtime_id);
        let client = self
            .template
            .connect(
                cookie_agent_runtime_api::RuntimeConnection::new(transport),
                &settings,
                Some(Arc::clone(&inner.mcp_hub)),
            )
            .await
            .map_err(|reason| DialInRefusal::Initialize(runtime_id.to_owned(), reason))?;
        inner.runtimes.watch_connection(&self.template, &client);
        inner.runtimes.revive(runtime_id);
        Ok(())
    }
}

impl Drop for DialInClaim {
    fn drop(&mut self) {
        self.template.release();
    }
}

/// One capability a new session attaches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRuntimeRequest {
    pub runtime_id: String,
    /// The runtime's only capability when `None`.
    pub capability: Option<String>,
    /// Required exactly when the capability takes a workspace, unless the
    /// runtime has a default one.
    pub workspace: Option<PathBuf>,
}

/// Where a new session works and the runtimes it attaches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRuntimes {
    /// Runtimes besides `global`, in order; `None` attaches `local`'s
    /// `coding` capability at `cwd`.
    pub runtimes: Option<Vec<SessionRuntimeRequest>>,
    /// Whether the session attaches the global runtime.
    pub global: bool,
    /// The client's directory: the session's cwd identity and the default
    /// `coding` workspace. The daemon's own directory when `None`.
    pub cwd: Option<PathBuf>,
}

impl Default for SessionRuntimes {
    fn default() -> Self {
        Self {
            runtimes: None,
            global: true,
            cwd: None,
        }
    }
}

/// The name a session attaches an instance under: the name the instance
/// reports, suffixed with `_2`, `_3`, ... while the session already has an
/// attachment of that name.
pub(crate) fn allocate_attachment_name(
    existing: &[RuntimeAttachment],
    reported: &str,
) -> Result<AttachmentName, EngineError> {
    let taken = |name: &str| {
        existing
            .iter()
            .any(|attachment| attachment.name.as_str() == name)
    };
    let invalid = |name: &str| EngineError::Runtime(format!("invalid attachment name `{name}`"));
    if !taken(reported) {
        return AttachmentName::new(reported).map_err(|_| invalid(reported));
    }
    (2..=existing.len() + 1)
        .map(|suffix| format!("{reported}_{suffix}"))
        .find(|candidate| !taken(candidate))
        .ok_or_else(|| invalid(reported))
        .and_then(|name| AttachmentName::new(name.as_str()).map_err(|_| invalid(&name)))
}

/// Whether `attachment` refers to `instance`.
fn refers_to(attachment: &RuntimeAttachment, instance: &RuntimeInstance) -> bool {
    attachment.runtime_id.as_str() == instance.info().id()
        && &attachment.capability == instance.info().capability()
        && attachment.workspace.as_deref().map(std::path::Path::new)
            == instance
                .info()
                .workspace()
                .map(|workspace| workspace.root())
}

/// `instance` added to `attachments` under its reported name. Attaching one
/// instance twice is an error.
pub(crate) fn push_instance(
    attachments: &mut Vec<RuntimeAttachment>,
    instance: &RuntimeInstance,
) -> Result<RuntimeAttachment, EngineError> {
    if let Some(existing) = attachments
        .iter()
        .find(|attachment| refers_to(attachment, instance))
    {
        return Err(EngineError::Runtime(format!(
            "capability `{}` of runtime `{}` is already attached as `{}`",
            instance.info().capability(),
            instance.info().id(),
            existing.name
        )));
    }
    let name = allocate_attachment_name(attachments, instance.info().attachment_name())?;
    let attachment = instance.attachment(name)?;
    attachments.push(attachment.clone());
    Ok(attachment)
}

/// The events a new session's creation writes for `attachments`.
pub(crate) fn initial_attachment_events(attachments: &[RuntimeAttachment]) -> Vec<Event> {
    attachments.iter().map(attached_event).collect()
}

pub(crate) fn attached_event(attachment: &RuntimeAttachment) -> Event {
    Event::RuntimeAttached {
        name: attachment.name.clone(),
        runtime_id: attachment.runtime_id.clone(),
        capability: attachment.capability.clone(),
        kind: attachment.kind.clone(),
        workspace: attachment.workspace.clone(),
    }
}

impl Engine {
    /// What a new session attaches by default: `global`, then `local`.
    pub(crate) fn default_attachments(&self) -> Vec<RuntimeAttachment> {
        let mut attachments = Vec::with_capacity(2);
        for instance in [self.inner.runtimes.global(), self.inner.runtimes.local()] {
            push_instance(&mut attachments, instance).expect("built-in runtimes attach");
        }
        attachments
    }

    /// The session's attachments in attach order.
    pub(crate) fn attachments_of(&self, projection: &SessionProjection) -> Vec<RuntimeAttachment> {
        projection.runtime_attachments.attachments().to_vec()
    }

    /// The runtimes `session` is attached to, in attach order.
    pub fn session_runtimes(
        &self,
        session: SessionId,
    ) -> Result<Vec<RuntimeAttachment>, EngineError> {
        Ok(self.attachments_of(&*self.inner.store.get(session)?))
    }

    /// The tool sources of a session with `attachments`: the base layer, then
    /// each attachment's running instance, and after the first attachment of
    /// a runtime that has tools of its own (its runtime-scoped MCP servers')
    /// those, once per runtime however many of its workspaces the session
    /// attaches. They compose under the runtime's ID and run without a
    /// workspace. An attachment whose instance is not running contributes
    /// nothing.
    pub(crate) fn tool_sources_for(
        &self,
        attachments: &[RuntimeAttachment],
    ) -> Vec<SourcedProvider> {
        let mut sources = self
            .inner
            .runtimes
            .base_providers()
            .into_iter()
            .map(|provider| SourcedProvider {
                attachment: None,
                generation: 0,
                origin: crate::ToolOrigin::Base,
                provider,
            })
            .collect::<Vec<_>>();
        let mut shared_runtimes = std::collections::HashSet::new();
        for attachment in attachments {
            let Some(instance) = self.inner.runtimes.instance(attachment) else {
                continue;
            };
            let origin = instance.origin();
            let (generation, providers) = instance.sources();
            sources.extend(providers.into_iter().map(|provider| SourcedProvider {
                attachment: Some(attachment.name.clone()),
                generation,
                origin: origin.clone(),
                provider,
            }));
            if let Some((generation, shared)) = instance.shared_source()
                && shared_runtimes.insert(attachment.runtime_id.clone())
            {
                sources.push(SourcedProvider {
                    attachment: Some(attachment.name.clone()),
                    generation,
                    origin: crate::ToolOrigin::Runtime(shared.info),
                    provider: shared.provider,
                });
            }
        }
        sources
    }

    /// The tool sources of `session` (see [`Self::tool_sources_for`]).
    pub(crate) fn session_tool_sources(
        &self,
        session: SessionId,
    ) -> Result<Vec<SourcedProvider>, EngineError> {
        Ok(self.tool_sources_for(&self.session_runtimes(session)?))
    }

    /// The instance of `capability` of `runtime_id` at `workspace` that a
    /// session is about to attach, as creating a session, attaching a
    /// runtime, and delegating with capabilities do: opened when it is not
    /// running, and never over a runtime that is not online (see
    /// `RuntimeRegistry::ensure`). A dial-in runtime this daemon has not
    /// seen since it started, but remembers, is refused as not connected,
    /// with when it was last seen.
    pub(crate) async fn ensure_runtime(
        &self,
        runtime_id: &str,
        capability: Option<&str>,
        workspace: Option<&std::path::Path>,
    ) -> Result<Arc<RuntimeInstance>, EngineError> {
        if let Some(offline) = self.remembered_offline(runtime_id) {
            return Err(EngineError::Runtime(offline));
        }
        self.inner
            .runtimes
            .ensure(runtime_id, capability, workspace)
            .await
    }

    /// Why dial-in runtime `runtime_id` cannot be used, when this daemon
    /// has not seen it connect since it started but remembers it: it is
    /// not connected, last seen when its record says.
    pub(crate) fn remembered_offline(&self, runtime_id: &str) -> Option<String> {
        if self.inner.runtimes.knows(runtime_id) {
            return None;
        }
        let record = self
            .inner
            .machines
            .record(&RuntimeId::new(runtime_id).ok()?)?;
        (record.kind == crate::machines::RecordKind::DialIn)
            .then(|| crate::runtimes::not_connected(runtime_id, Some(record.last_seen)))
    }

    /// The attachments a new session gets for `runtimes`, starting the
    /// instances that are not running.
    pub(crate) async fn resolve_session_runtimes(
        &self,
        runtimes: &SessionRuntimes,
    ) -> Result<Vec<RuntimeAttachment>, EngineError> {
        let mut attachments = Vec::new();
        if runtimes.global {
            push_instance(&mut attachments, self.inner.runtimes.global())?;
        }
        match &runtimes.runtimes {
            None => match &runtimes.cwd {
                Some(cwd) => {
                    let instance = self
                        .ensure_runtime(
                            RuntimeId::LOCAL,
                            Some(cookie_agent_protocol::CapabilityName::CODING),
                            Some(cwd),
                        )
                        .await?;
                    push_instance(&mut attachments, &instance)?;
                }
                None => {
                    push_instance(&mut attachments, self.inner.runtimes.local())?;
                }
            },
            Some(requests) => {
                for request in requests {
                    let instance = self
                        .ensure_runtime(
                            &request.runtime_id,
                            request.capability.as_deref(),
                            request.workspace.as_deref(),
                        )
                        .await?;
                    push_instance(&mut attachments, &instance)?;
                }
            }
        }
        Ok(attachments)
    }

    /// Makes sure every instance `session` is attached to is running, as when
    /// the session is opened in a daemon that has not started them. Returns
    /// the attachments that could not be started.
    pub(crate) async fn ensure_session_runtimes(
        &self,
        session: SessionId,
    ) -> Result<Vec<(AttachmentName, EngineError)>, EngineError> {
        let mut failures = Vec::new();
        for attachment in self.session_runtimes(session)? {
            if let Err(error) = self.inner.runtimes.ensure_attachment(&attachment).await {
                failures.push((attachment.name, error));
            }
        }
        Ok(failures)
    }

    /// Connects what `session` uses, as when it is created or opened: every
    /// attached instance is started, and every MCP server of an attached
    /// instance whose last connection attempt failed is retried. The next
    /// run waits for those attempts to finish; failures reach clients
    /// viewing the session through the failure broadcasts.
    pub(crate) async fn connect_session(&self, session: SessionId) {
        self.ensure_session_runtimes_logged(session).await;
        for attachment in self.session_runtimes(session).unwrap_or_default() {
            if attachment.runtime_id.as_str() == RuntimeId::GLOBAL {
                self.retry_global_mcp();
            } else if let Some(connection) = self
                .inner
                .runtimes
                .instance(&attachment)
                .and_then(|instance| instance.connection())
            {
                connection.retry_failed_mcp();
            }
        }
    }

    /// Waits until the MCP servers of `session`'s attached runtime instances
    /// have finished the connection attempts [`Self::connect_session`]
    /// started, so a run's tool list is not built from a partial snapshot.
    pub(crate) async fn await_session_mcp(&self, session: SessionId) {
        for attachment in self.session_runtimes(session).unwrap_or_default() {
            if let Some(connection) = self
                .inner
                .runtimes
                .instance(&attachment)
                .and_then(|instance| instance.connection())
            {
                connection.mcp_settled().await;
            }
        }
    }

    /// Retries the global runtime's failed MCP servers (see
    /// [`Self::connect_session`]).
    fn retry_global_mcp(&self) {
        if let Some(runtime) = &self.inner.runtime {
            self.inner.mcp.retry_failed(runtime);
        }
    }

    /// [`Self::ensure_session_runtimes`] that reports failures on stderr:
    /// the attachment then publishes no tools.
    pub(crate) async fn ensure_session_runtimes_logged(&self, session: SessionId) {
        match self.ensure_session_runtimes(session).await {
            Ok(failures) => {
                for (name, error) in failures {
                    eprintln!(
                        "cookie-agent: session {session}: runtime `{name}` is unavailable: {error}"
                    );
                }
            }
            Err(error) => {
                eprintln!("cookie-agent: session {session}: runtimes not checked: {error}");
            }
        }
    }

    /// Attaches `capability` of `runtime_id` (its only capability when
    /// `None`; at `workspace`, when the capability takes one) to `session`,
    /// opening the instance when needed. Rejected while the session has an
    /// active run; a failed start records nothing.
    pub async fn attach_runtime(
        &self,
        session: SessionId,
        runtime_id: &str,
        capability: Option<&str>,
        workspace: Option<PathBuf>,
        origin: EventOrigin,
    ) -> Result<RuntimeAttachment, EngineError> {
        self.ensure_session_owned(session)?;
        if self.inner.store.get(session)?.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(session));
        }
        // A session runs its ACP agent on the one attached runtime that
        // offers it, so a second such runtime is refused.
        if let Some(offers) = self.session_acp_offers(session)? {
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
        }
        let instance = self
            .ensure_runtime(runtime_id, capability, workspace.as_deref())
            .await?;
        if runtime_id == RuntimeId::GLOBAL {
            self.retry_global_mcp();
        } else if let Some(connection) = instance.connection() {
            connection.retry_failed_mcp();
        }
        self.on_actor(session, move |engine| {
            let projection = engine.inner.store.get(session)?;
            if projection.status == SessionStatus::Running {
                return Err(EngineError::SessionRunning(session));
            }
            let mut attachments = engine.attachments_of(&projection);
            let attachment = push_instance(&mut attachments, &instance)?;
            engine.append_direct(session, None, origin, attached_event(&attachment))?;
            Ok(attachment)
        })
        .await
    }

    /// Removes the attachment `name` from `session`. Rejected while the
    /// session has an active run.
    pub async fn detach_runtime(
        &self,
        session: SessionId,
        name: AttachmentName,
        origin: EventOrigin,
    ) -> Result<(), EngineError> {
        self.ensure_session_owned(session)?;
        self.on_actor(session, move |engine| {
            let projection = engine.inner.store.get(session)?;
            if projection.status == SessionStatus::Running {
                return Err(EngineError::SessionRunning(session));
            }
            if !engine
                .attachments_of(&projection)
                .iter()
                .any(|attachment| attachment.name == name)
            {
                return Err(EngineError::Runtime(format!(
                    "session has no runtime attached as `{name}`"
                )));
            }
            engine.append_direct(session, None, origin, Event::RuntimeDetached { name })?;
            Ok(())
        })
        .await
    }

    /// Registers a runtime template so sessions can attach it. The engine
    /// registers the built-in `global` and `local` templates and the
    /// configured `[runtimes.<id>]` ones; embedding hosts and tests may add
    /// in-process ones.
    pub fn register_runtime_template(
        &self,
        template: Arc<dyn RuntimeTemplate>,
    ) -> Result<(), EngineError> {
        self.inner.runtimes.register_template(template)
    }

    /// Reserves `runtime_id` for a runtime dialing in, before its
    /// connection is accepted, so a refusal can be answered while the
    /// request is still plain HTTP. A malformed or reserved ID, one the
    /// daemon configures, and one whose runtime is connected or being
    /// accepted are refused.
    pub fn claim_dial_in_runtime(&self, runtime_id: &str) -> Result<DialInClaim, DialInRefusal> {
        let template = self.inner.runtimes.dial_in_template(runtime_id)?;
        template.claim()?;
        Ok(DialInClaim {
            engine: self.clone(),
            template,
        })
    }

    /// Claims `runtime_id` ([`Self::claim_dial_in_runtime`]) and accepts
    /// the runtime that dialed in over `transport` ([`DialInClaim::accept`]).
    pub async fn accept_dial_in_runtime(
        &self,
        runtime_id: &str,
        transport: Box<dyn cookie_agent_protocol::Transport>,
    ) -> Result<(), DialInRefusal> {
        self.claim_dial_in_runtime(runtime_id)?
            .accept(transport)
            .await
    }

    /// Every registered runtime template.
    #[must_use]
    pub fn runtime_templates(&self) -> Vec<RuntimeTemplateSummary> {
        self.inner.runtimes.templates()
    }

    /// Every runtime instance this daemon has started or is starting.
    #[must_use]
    pub fn runtime_instances(&self) -> Vec<RuntimeInstanceSummary> {
        self.inner.runtimes.instances()
    }

    /// Retries the instance of `capability` of `runtime_id` (its only
    /// capability when `None`; at `workspace`, when the capability takes
    /// one) now, as `runtimes.reconnect` and the `/runtime`
    /// panel do: a failed instance starts again under a new generation, one
    /// that is not running starts, and a ready or reconnecting one is left as
    /// it is. Retrying an instance also retries the failed MCP servers it
    /// hosts. Returns the instance afterwards.
    pub async fn reconnect_runtime(
        &self,
        runtime_id: &str,
        capability: Option<&str>,
        workspace: Option<PathBuf>,
    ) -> Result<RuntimeInstanceSummary, EngineError> {
        let instance = self
            .ensure_runtime(runtime_id, capability, workspace.as_deref())
            .await?;
        if runtime_id == RuntimeId::GLOBAL {
            self.retry_global_mcp();
        } else if let Some(connection) = instance.connection() {
            connection.retry_failed_mcp();
        }
        self.inner
            .runtimes
            .summary(&instance)
            .ok_or_else(|| EngineError::Runtime(format!("runtime `{runtime_id}` stopped")))
    }

    /// Runtime instances that failed to start, as they happen.
    #[must_use]
    pub fn subscribe_runtime_failures(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::RuntimeFailure> {
        self.inner.runtimes.subscribe_failures()
    }

    /// MCP servers of any runtime that failed to connect, or dropped and did
    /// not reconnect, as they happen.
    #[must_use]
    pub fn subscribe_mcp_failures(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::McpServerFailure> {
        self.inner.mcp_hub.subscribe_failures()
    }

    /// Signals every runtime template or instance status change.
    #[must_use]
    pub fn subscribe_runtime_instance_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.runtimes.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{AttachmentName, RuntimeAttachment, RuntimeId, RuntimeKind};

    use super::allocate_attachment_name;

    fn attachment(name: &str) -> RuntimeAttachment {
        RuntimeAttachment {
            name: AttachmentName::new(name).unwrap(),
            runtime_id: RuntimeId::new("labpc").unwrap(),
            capability: cookie_agent_protocol::CapabilityName::coding(),
            kind: RuntimeKind::new("coding").unwrap(),
            workspace: Some("/srv/api".into()),
        }
    }

    #[test]
    fn a_clashing_attachment_name_gets_the_next_free_suffix() {
        assert_eq!(
            allocate_attachment_name(&[], "labpc_api").unwrap().as_str(),
            "labpc_api"
        );
        assert_eq!(
            allocate_attachment_name(&[attachment("labpc_api")], "labpc_api")
                .unwrap()
                .as_str(),
            "labpc_api_2"
        );
        assert_eq!(
            allocate_attachment_name(
                &[attachment("labpc_api"), attachment("labpc_api_2")],
                "labpc_api"
            )
            .unwrap()
            .as_str(),
            "labpc_api_3"
        );
        assert_eq!(
            allocate_attachment_name(&[attachment("labpc_api_2")], "labpc_api")
                .unwrap()
                .as_str(),
            "labpc_api"
        );
    }
}

//! Session attachments: which runtime instances a session uses, under which
//! session-unique names (`docs/site/specs/runtimes.md` section 5).

use std::{path::PathBuf, sync::Arc};

use cookie_agent_protocol::{
    AttachmentName, EventOrigin, RuntimeAttachment, SessionId, SessionStatus,
};

use super::{Engine, EngineError, Event};
use crate::{
    runtimes::{
        RuntimeInstance, RuntimeInstanceSummary, RuntimeTemplate, RuntimeTemplateSummary,
        SourcedProvider,
    },
    session::SessionProjection,
};

/// One coding runtime a new session attaches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRuntimeRequest {
    pub runtime_id: String,
    /// Required exactly when the runtime's template takes a workspace.
    pub workspace: Option<PathBuf>,
}

/// The runtimes a new session attaches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRuntimes {
    /// Runtimes besides `global`, in order; `None` attaches `local`.
    pub runtimes: Option<Vec<SessionRuntimeRequest>>,
    /// Whether the session attaches the global runtime.
    pub global: bool,
}

impl Default for SessionRuntimes {
    fn default() -> Self {
        Self {
            runtimes: None,
            global: true,
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
        && attachment.workspace.as_deref().map(std::path::Path::new)
            == instance
                .info()
                .workspace()
                .map(|workspace| workspace.root())
}

/// `instance` added to `attachments` under its reported name. Attaching one
/// instance twice is an error.
fn push_instance(
    attachments: &mut Vec<RuntimeAttachment>,
    instance: &RuntimeInstance,
) -> Result<RuntimeAttachment, EngineError> {
    if let Some(existing) = attachments
        .iter()
        .find(|attachment| refers_to(attachment, instance))
    {
        return Err(EngineError::Runtime(format!(
            "runtime `{}` is already attached as `{}`",
            instance.info().id(),
            existing.name
        )));
    }
    let name = allocate_attachment_name(attachments, instance.info().attachment_name())?;
    let attachment = instance.attachment(name)?;
    attachments.push(attachment.clone());
    Ok(attachment)
}

pub(crate) fn attached_event(attachment: &RuntimeAttachment) -> Event {
    Event::RuntimeAttached {
        name: attachment.name.clone(),
        runtime_id: attachment.runtime_id.clone(),
        kind: attachment.kind,
        workspace: attachment.workspace.clone(),
    }
}

impl Engine {
    /// What a new session attaches by default, and what a session whose log
    /// predates runtimes is attached to: `global`, then `local`.
    pub(crate) fn default_attachments(&self) -> Vec<RuntimeAttachment> {
        let mut attachments = Vec::with_capacity(2);
        for instance in [self.inner.runtimes.global(), self.inner.runtimes.local()] {
            push_instance(&mut attachments, instance).expect("built-in runtimes attach");
        }
        attachments
    }

    /// The session's attachments in attach order.
    pub(crate) fn attachments_of(&self, projection: &SessionProjection) -> Vec<RuntimeAttachment> {
        projection
            .runtime_attachments
            .attachments()
            .map_or_else(|| self.default_attachments(), <[_]>::to_vec)
    }

    /// The runtimes `session` is attached to, in attach order.
    pub fn session_runtimes(
        &self,
        session: SessionId,
    ) -> Result<Vec<RuntimeAttachment>, EngineError> {
        Ok(self.attachments_of(&*self.inner.store.get(session)?))
    }

    /// The tool sources of a session with `attachments`: the base layer, then
    /// each attachment's running instance. An attachment whose instance is
    /// not running contributes nothing.
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
        for attachment in attachments {
            let Some(instance) = self.inner.runtimes.instance(attachment) else {
                continue;
            };
            let origin = instance.origin();
            sources.extend(
                instance
                    .providers()
                    .into_iter()
                    .map(|provider| SourcedProvider {
                        attachment: Some(attachment.name.clone()),
                        generation: instance.generation(),
                        origin: origin.clone(),
                        provider,
                    }),
            );
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
            None => {
                push_instance(&mut attachments, self.inner.runtimes.local())?;
            }
            Some(requests) => {
                for request in requests {
                    let instance = self
                        .inner
                        .runtimes
                        .ensure(&request.runtime_id, request.workspace.as_deref())
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

    /// Attaches the instance of `runtime_id` (at `workspace`, when its
    /// template takes one) to `session`, starting it when needed. Rejected
    /// while the session has an active run; a failed start records nothing.
    pub async fn attach_runtime(
        &self,
        session: SessionId,
        runtime_id: &str,
        workspace: Option<PathBuf>,
        origin: EventOrigin,
    ) -> Result<RuntimeAttachment, EngineError> {
        self.ensure_session_owned(session)?;
        if self.inner.store.get(session)?.status == SessionStatus::Running {
            return Err(EngineError::SessionRunning(session));
        }
        let instance = self
            .inner
            .runtimes
            .ensure(runtime_id, workspace.as_deref())
            .await?;
        self.on_actor(session, move |engine| {
            let projection = engine.inner.store.get(session)?;
            if projection.status == SessionStatus::Running {
                return Err(EngineError::SessionRunning(session));
            }
            let mut attachments = engine.attachments_of(&projection);
            let attachment = push_instance(&mut attachments, &instance)?;
            engine.materialize_legacy_attachments(session, &projection, &origin)?;
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
            engine.materialize_legacy_attachments(session, &projection, &origin)?;
            engine.append_direct(session, None, origin, Event::RuntimeDetached { name })?;
            Ok(())
        })
        .await
    }

    /// A log that predates runtimes holds no attachment event and reads as
    /// attached to `global` and `local`. Before its first attach or detach
    /// those implicit attachments are written, so the fold stays the source
    /// of truth.
    fn materialize_legacy_attachments(
        &self,
        session: SessionId,
        projection: &SessionProjection,
        origin: &EventOrigin,
    ) -> Result<(), EngineError> {
        if projection.runtime_attachments.attachments().is_some() {
            return Ok(());
        }
        for attachment in self.default_attachments() {
            self.append_direct(session, None, origin.clone(), attached_event(&attachment))?;
        }
        Ok(())
    }

    /// Registers a runtime template so sessions can attach it. Production
    /// registers only the built-in `global` and `local` templates; embedding
    /// hosts and tests may add in-process ones.
    pub fn register_runtime_template(
        &self,
        template: Arc<dyn RuntimeTemplate>,
    ) -> Result<(), EngineError> {
        self.inner.runtimes.register_template(template)
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
            kind: RuntimeKind::Coding,
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

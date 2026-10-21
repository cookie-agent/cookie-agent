use std::path::Path;

use cookie_agent_protocol::{
    InvocationId, PersistedToolResult as ToolResult, RunId, SafeCode, SafeDisplayText,
    SafeErrorMessage, SafeToolError, SessionId, SessionOrigin, ToolCallId, ToolCallTermination,
    ToolTerminationOutcome,
};
use oven_sdk::ModelError;
use uuid::Uuid;

use super::{Engine, EngineError, Event};

impl Engine {
    pub(super) fn model_header_context(
        &self,
        session_id: SessionId,
    ) -> Result<oven_sdk::HeaderContext, EngineError> {
        let session = self.inner.store.get(session_id)?;
        let context = oven_sdk::HeaderContext::new(session_id.to_string());
        Ok(match session.meta.origin {
            SessionOrigin::Root => context,
            SessionOrigin::Delegated {
                parent_session_id, ..
            } => context.with_parent_session_id(parent_session_id.to_string()),
        })
    }

    pub(super) fn tool_call_owner(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
    ) -> Result<cookie_agent_protocol::AssistantToolCallRef, EngineError> {
        self.inner
            .store
            .get(session_id)?
            .log
            .event_snapshot()
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                Event::ToolCallStarted { start }
                    if event.run_id == Some(run_id) && start.tool_call_id == tool_call_id =>
                {
                    Some(start.owner.clone())
                }
                _ => None,
            })
            .ok_or_else(|| EngineError::ToolFailed("tool ownership is missing".into()))
    }

    pub(super) fn terminate_tool_direct(
        &self,
        session_id: SessionId,
        run_id: RunId,
        tool_call_id: ToolCallId,
        outcome: ToolTerminationOutcome,
        result: Option<ToolResult>,
        error: Option<SafeToolError>,
    ) -> Result<(), EngineError> {
        self.append_direct(
            session_id,
            Some(run_id),
            super::event_origin("engine:tool-result"),
            Event::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id,
                    owner: self.tool_call_owner(session_id, run_id, tool_call_id)?,
                    outcome,
                    result,
                    error,
                },
            },
        )
    }

    pub(super) fn next_model_turn_seq(&self, session_id: SessionId) -> Result<u64, EngineError> {
        Ok(self.inner.store.log(session_id)?.physical_model_turns() + 1)
    }

    pub(super) fn run_agent_prompt(
        &self,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<String, EngineError> {
        self.inner
            .store
            .get(session_id)?
            .log
            .event_snapshot()
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                Event::RunStarted { agent, .. } if event.run_id == Some(run_id) => {
                    Some(agent.composed_prompt.clone())
                }
                _ => None,
            })
            .ok_or(EngineError::MissingRun(run_id))
    }
}

pub(super) fn root_id(origin: &SessionOrigin, session: SessionId) -> SessionId {
    match origin {
        SessionOrigin::Delegated {
            root_session_id, ..
        } => *root_session_id,
        _ => session,
    }
}

pub(super) fn session_depth(origin: &SessionOrigin) -> u32 {
    match origin {
        SessionOrigin::Root => 0,
        SessionOrigin::Delegated { depth, .. } => *depth,
    }
}

/// A new root session's directory identity, decided once at creation, after
/// its attachments have opened: the canonical workspace that the first of
/// `attachments` with a workspace reported, on its runtime's machine. A
/// session with no workspace attachment gets `cwd` canonicalized on the
/// daemon's machine, or `cwd` as given when that fails. The client's typed
/// text never decides it when a workspace is attached, so `~/x` and
/// `/home/ada/x` attaching one workspace give one identity.
pub(crate) fn session_cwd_identity(
    attachments: &[cookie_agent_protocol::RuntimeAttachment],
    cwd: &Path,
) -> Result<cookie_agent_protocol::CwdIdentity, EngineError> {
    match attachments
        .iter()
        .find_map(|attachment| attachment.workspace.as_deref())
    {
        Some(workspace) => cookie_agent_protocol::CwdIdentity::new(workspace)
            .map_err(|error| invalid_cwd_identity(&error)),
        None => cwd_identity(cwd),
    }
}

fn cwd_identity(path: &Path) -> Result<cookie_agent_protocol::CwdIdentity, EngineError> {
    let canonical =
        cookie_agent_protocol::paths::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    cookie_agent_protocol::CwdIdentity::new(canonical.to_string_lossy())
        .map_err(|error| invalid_cwd_identity(&error))
}

fn invalid_cwd_identity(error: &impl std::fmt::Display) -> EngineError {
    EngineError::from(ModelError::invalid_request(format!(
        "invalid cwd identity: {error}"
    )))
}

pub(crate) fn invocation_id(session: SessionId, run: RunId, call: ToolCallId) -> InvocationId {
    InvocationId(Uuid::from_u128(hash_parts(&[
        &session.to_string(),
        &run.to_string(),
        &call.to_string(),
    ])))
}
pub(super) fn hash_parts(parts: &[&str]) -> u128 {
    use std::hash::{Hash, Hasher};
    let mut first = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut first);
    let high = first.finish() as u128;
    let mut second = std::collections::hash_map::DefaultHasher::new();
    "cookie_agent".hash(&mut second);
    parts.hash(&mut second);
    (high << 64) | second.finish() as u128
}

pub(super) fn safe_code(value: &str) -> SafeCode {
    SafeCode::new(value).expect("static safe code")
}

pub(super) fn safe_display(value: &str) -> SafeDisplayText {
    SafeDisplayText::new(sanitize_safe_text(value, SafeDisplayText::MAX_BYTES))
        .expect("sanitized display text")
}

pub(super) fn safe_error(value: &str) -> SafeErrorMessage {
    cookie_agent_protocol::diagnostics::headline(value)
}

pub(super) fn sanitize_safe_text(value: &str, maximum: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len() + character.len_utf8() > maximum {
            break;
        }
        output.push(character);
    }
    if output.is_empty() {
        "unavailable".into()
    } else {
        output
    }
}

pub(super) fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

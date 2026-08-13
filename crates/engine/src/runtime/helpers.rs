use super::*;

impl Engine {
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
            .events()
            .into_iter()
            .find_map(|event| match event.payload {
                Event::ToolCallStarted { start }
                    if event.run_id == Some(run_id) && start.tool_call_id == tool_call_id =>
                {
                    Some(start.owner)
                }
                _ => None,
            })
            .ok_or_else(|| EngineError::MissingTool("tool ownership is missing".into()))
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
        Ok(self
            .inner
            .store
            .get(session_id)?
            .log
            .all_events()
            .iter()
            .filter(|event| matches!(event.payload, Event::ModelTurnCommitted { .. }))
            .count() as u64
            + 1)
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
            .events()
            .into_iter()
            .find_map(|event| match event.payload {
                Event::RunStarted { agent, .. } if event.run_id == Some(run_id) => {
                    Some(agent.composed_prompt)
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

pub(crate) fn cwd_identity(path: &Path) -> Result<cookie_agent_protocol::CwdIdentity, EngineError> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    cookie_agent_protocol::CwdIdentity::new(canonical.to_string_lossy()).map_err(|error| {
        EngineError::from(ModelError::invalid_request(format!(
            "invalid cwd identity: {error}"
        )))
    })
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
    SafeErrorMessage::new(sanitize_safe_text(value, SafeErrorMessage::MAX_BYTES))
        .expect("sanitized safe error")
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

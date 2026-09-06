use std::{future::Future, pin::Pin};

use cookie_agent_protocol::{
    AgentId, InvocationId, PersistedToolResult as ToolResult, RunId, SessionId, ToolCallId,
};
use serde::{Deserialize, Serialize};

use crate::runtime::{Engine, EngineError};

pub(crate) fn delegate_result_matches_child(
    result: &ToolResult,
    child_session_id: SessionId,
) -> bool {
    let expected = serde_json::json!(child_session_id);
    result
        .metadata
        .get("session_id")
        .or_else(|| result.metadata.get("child_session_id"))
        == Some(&expected)
}

/// Immutable arguments for one delegate-tool invocation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DelegateInvocation {
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: ToolCallId,
    pub agent_type: AgentId,
    pub description: String,
    pub prompt: String,
    pub background: bool,
    pub resume_session_id: Option<SessionId>,
    pub inherit_context: bool,
}

/// Stable child identity returned to the delegate tool provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegateHandle {
    pub invocation_id: InvocationId,
    pub child_session_id: SessionId,
    pub child_run_id: Option<RunId>,
}

/// A delegate wait that cancels its child if its consumer abandons the wait.
pub struct DelegateAwait {
    pub(crate) future: Pin<Box<dyn Future<Output = Result<ToolResult, EngineError>> + Send>>,
    pub(crate) engine: Engine,
    pub(crate) runtime: Option<tokio::runtime::Handle>,
    pub(crate) handle: DelegateHandle,
    pub(crate) completed: bool,
}

impl Future for DelegateAwait {
    type Output = Result<ToolResult, EngineError>;

    fn poll(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let result = self.future.as_mut().poll(context);
        if result.is_ready() {
            self.completed = true;
        }
        result
    }
}

impl Drop for DelegateAwait {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // Delegate waits are created and polled from the Tokio tool task. If that
        // task is dropped, retain the cancellation in a detached runtime task.
        // This closes the abandoned-tool-call child-run leak.
        if let Some(runtime) = self
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        {
            let engine = self.engine.clone();
            let cancel_engine = engine.clone();
            let handle = self.handle;
            let _ = engine.spawn_admission_task(&runtime, async move {
                let _ = cancel_engine.cancel_delegate(handle).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_result_requires_the_recorded_child_identity() {
        let child = SessionId::new_v7();
        let other = SessionId::new_v7();
        for key in ["session_id", "child_session_id"] {
            for (value, expected) in [
                (serde_json::json!(child), true),
                (serde_json::json!(other), false),
                (serde_json::Value::Null, false),
            ] {
                let result = ToolResult {
                    title: cookie_agent_protocol::SafeDisplayText::new("Delegate cancelled")
                        .unwrap(),
                    output: String::new(),
                    metadata: serde_json::json!({key: value}),
                    truncation: None,
                    attachments: Vec::new(),
                    additional_messages: Vec::new(),
                };
                assert_eq!(delegate_result_matches_child(&result, child), expected);
            }
        }
    }
}

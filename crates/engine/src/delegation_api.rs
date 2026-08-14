use std::{future::Future, pin::Pin};

use cookie_agent_protocol::{
    AgentId, InvocationId, PersistedToolResult as ToolResult, RunId, SessionId, ToolCallId,
};
use serde::{Deserialize, Serialize};

use crate::runtime::{Engine, EngineError};

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

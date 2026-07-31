//! Single-conversation runtime interfaces and actor scaffolding.

use async_trait::async_trait;
use cookiecode_protocol::{RunId, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

pub mod actor;
pub mod events;
pub mod journal;
pub mod permissions;
pub mod run;
pub mod session;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionToolContext {
    pub session: SessionId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub content: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolProgress {
    pub tool_call_id: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ProgressSink {
    sender: mpsc::Sender<ToolProgress>,
}

impl ProgressSink {
    #[must_use]
    pub fn new(sender: mpsc::Sender<ToolProgress>) -> Self {
        Self { sender }
    }

    pub async fn send(&self, progress: ToolProgress) -> Result<(), ToolError> {
        self.sender
            .send(progress)
            .await
            .map_err(|_| ToolError::ProgressSinkClosed)
    }
}

#[derive(Clone, Debug)]
pub struct ToolInvocationContext {
    pub session: SessionId,
    pub run: RunId,
    pub progress: ProgressSink,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool progress sink closed")]
    ProgressSinkClosed,
    #[error("tool failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait ToolProvider: Send + Sync {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError>;

    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<ToolResult, ToolError>;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DepthLimit {
    Finite(u32),
    Unlimited,
}

impl DepthLimit {
    #[must_use]
    pub const fn allows_delegation(self) -> bool {
        !matches!(self, Self::Finite(0))
    }
}

#[derive(Debug)]
pub struct SessionActor {
    mailbox: mpsc::Sender<SessionMessage>,
    inbox: mpsc::Receiver<SessionMessage>,
}

#[derive(Debug)]
pub enum SessionMessage {
    StartRun,
    Cancel,
    ToolResult(ToolResult),
}

impl SessionActor {
    #[must_use]
    pub fn new(mailbox_capacity: usize) -> Self {
        let (mailbox, inbox) = mpsc::channel(mailbox_capacity);
        Self { mailbox, inbox }
    }

    #[must_use]
    pub fn mailbox(&self) -> mpsc::Sender<SessionMessage> {
        self.mailbox.clone()
    }

    pub async fn run(mut self) {
        while let Some(message) = self.inbox.recv().await {
            self.handle(message).await;
        }
    }

    async fn handle(&mut self, _message: SessionMessage) {
        todo!("implement session actor mailbox handling")
    }

    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }
}

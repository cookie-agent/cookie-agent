//! Long-lived per-session mailbox actors.
//!
//! The actor owns command ordering only.  Provider streams and tool futures
//! are spawned by a command handler and report back through this mailbox; they
//! must never be awaited by the actor itself.

use std::future::Future;

use tokio::sync::mpsc;

/// Handle to a session actor: one task on the engine's runtime that runs its
/// commands in order.
///
/// The mailbox is bounded so producers apply backpressure instead of growing
/// memory without limit. Dropping every handle ends the task once it has
/// drained what was already queued.
#[derive(Debug)]
pub struct SessionActor<M> {
    sender: mpsc::Sender<M>,
}

impl<M: Send + 'static> Clone for SessionActor<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<M: Send + 'static> SessionActor<M> {
    /// Spawns the actor on `runtime`, or on a thread of its own when there is
    /// no runtime to share (an engine driven outside Tokio).
    #[must_use]
    pub fn spawn<F, Fut>(
        capacity: usize,
        runtime: Option<&tokio::runtime::Handle>,
        mut handler: F,
    ) -> Self
    where
        F: FnMut(M) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (sender, mut receiver) = mpsc::channel(capacity);
        let run = async move {
            while let Some(message) = receiver.recv().await {
                handler(message).await;
            }
        };
        if let Some(runtime) = runtime {
            runtime.spawn(run);
        } else {
            std::thread::Builder::new()
                .name("cookie_agent_session_actor".into())
                .spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("create session actor runtime")
                        .block_on(run);
                })
                .expect("spawn session actor thread");
        }
        Self { sender }
    }

    pub async fn send(&self, message: M) -> Result<(), mpsc::error::SendError<M>> {
        self.sender.send(message).await
    }
}

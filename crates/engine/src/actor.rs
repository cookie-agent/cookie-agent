//! Long-lived per-session mailbox actors.
//!
//! The actor owns command ordering only.  Provider streams and tool futures
//! are spawned by a command handler and report back through this mailbox; they
//! must never be awaited by the actor itself.

use std::{future::Future, thread};

use tokio::sync::mpsc;

/// Handle to a dedicated session actor.
///
/// The mailbox is bounded so producers apply backpressure instead of growing
/// memory without limit. The actor has its own thread; callers on Tokio worker
/// threads use [`Self::send`] and await capacity rather than blocking workers.
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
    #[must_use]
    pub fn spawn<F, Fut>(capacity: usize, mut handler: F) -> Self
    where
        F: FnMut(M) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (sender, mut receiver) = mpsc::channel(capacity);
        thread::Builder::new()
            .name("cookiecode-session-actor".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("create session actor runtime");
                runtime.block_on(async move {
                    while let Some(message) = receiver.recv().await {
                        handler(message).await;
                    }
                });
            })
            .expect("spawn session actor thread");
        Self { sender }
    }

    pub async fn send(&self, message: M) -> Result<(), mpsc::error::SendError<M>> {
        self.sender.send(message).await
    }

    /// Used only by synchronous facade methods called outside a Tokio worker.
    pub fn blocking_send(&self, message: M) -> Result<(), mpsc::error::SendError<M>> {
        self.sender.blocking_send(message)
    }
}

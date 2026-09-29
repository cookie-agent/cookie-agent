use cookie_agent_protocol::{EventSubscriptionMessage, ServerContext};
use tokio::sync::mpsc;

use super::Server;

impl Server {
    pub(super) fn start_event_tail(
        &self,
        mut receiver: mpsc::Receiver<EventSubscriptionMessage>,
        context: ServerContext,
    ) {
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let message = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    message = receiver.recv() => match message {
                        Some(message) => message,
                        None => return,
                    },
                };
                if context
                    .notify("events.subscription", &message)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }
}

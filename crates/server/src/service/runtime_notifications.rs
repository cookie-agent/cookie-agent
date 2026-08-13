use cookie_agent_protocol::ServerContext;

use super::Server;

impl Server {
    pub(super) fn start_runtime_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_runtime_changes();
        let startup = self.engine.current_runtime().result.snapshot.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            let startup = cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: None,
                snapshot: startup,
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::Startup],
            };
            if context
                .notify(cookie_agent_protocol::RUNTIME_CHANGED_METHOD, &startup)
                .await
                .is_err()
            {
                return;
            }
            loop {
                let changed = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.recv() => match changed {
                        Ok(changed) => changed,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if context
                    .notify(cookie_agent_protocol::RUNTIME_CHANGED_METHOD, &changed)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }
}

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::Server;

impl Server {
    pub(super) fn start_runtime_notifications(
        &self,
        notifications: mpsc::Sender<Value>,
        shutdown: CancellationToken,
    ) {
        let mut receiver = self.engine.subscribe_runtime_changes();
        let startup = self.engine.current_runtime().result.snapshot.clone();
        tokio::spawn(async move {
            let startup = cookie_agent_protocol::RuntimeChangedNotification {
                previous_revision: None,
                snapshot: startup,
                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::Startup],
            };
            let Ok(params) = serde_json::to_value(startup) else {
                return;
            };
            let Ok(notification) = serde_json::to_value(cookie_agent_protocol::Notification::new(
                cookie_agent_protocol::RUNTIME_CHANGED_METHOD,
                Some(params),
            )) else {
                return;
            };
            if notifications.send(notification).await.is_err() {
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
                let Ok(params) = serde_json::to_value(changed) else {
                    return;
                };
                let Ok(notification) =
                    serde_json::to_value(cookie_agent_protocol::Notification::new(
                        cookie_agent_protocol::RUNTIME_CHANGED_METHOD,
                        Some(params),
                    ))
                else {
                    return;
                };
                if notifications.send(notification).await.is_err() {
                    return;
                }
            }
        });
    }
}

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

    pub(crate) fn start_engine_event_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_engine_events();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let event = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    event = receiver.recv() => match event {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                let cookie_agent_engine::EngineEvent::PluginEvent {
                    session_id,
                    plugin,
                    name,
                    payload,
                } = event;
                if !context.is_session_subscribed(session_id) {
                    continue;
                }
                let params = cookie_agent_protocol::ExtensionBusEventParams {
                    session_id,
                    context_id: None,
                    plugin,
                    name,
                    payload,
                };
                if context.notify("events.plugin", &params).await.is_err() {
                    return;
                }
            }
        });
    }
}

use cookie_agent_protocol::ServerContext;

use super::Server;

impl Server {
    pub(super) fn start_runtime_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_runtime_changes();
        let startup = self.engine.current_runtime().result.snapshot.clone();
        let engine = self.engine.clone();
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
                        // The skipped changes broke this connection's revision
                        // chain: hand it the current runtime to start over from.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            cookie_agent_protocol::RuntimeChangedNotification {
                                previous_revision: None,
                                snapshot: engine.current_runtime().result.snapshot.clone(),
                                reasons: vec![cookie_agent_protocol::RuntimeChangeReason::Resync],
                            }
                        }
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

    /// Forwards every session index change to this connection. A connection
    /// too slow to keep up is told to resync instead of silently missing
    /// changes.
    pub(super) fn start_session_change_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_session_changes();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let change = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    change = receiver.recv() => match change {
                        Ok(change) => change,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            cookie_agent_protocol::SessionsChangedNotification::Resync
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if context
                    .notify(cookie_agent_protocol::SESSIONS_CHANGED_METHOD, &change)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals MCP server changes to this connection; bursts coalesce.
    pub(super) fn start_mcp_change_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_mcp_changes();
        receiver.mark_unchanged();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => if changed.is_err() {
                        return;
                    },
                }
                if context
                    .notify(
                        cookie_agent_protocol::MCP_CHANGED_METHOD,
                        &cookie_agent_protocol::McpChangedNotification {},
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals runtime template and instance status changes to this
    /// connection; bursts coalesce.
    pub(super) fn start_runtime_instance_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_runtime_instance_changes();
        receiver.mark_unchanged();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => if changed.is_err() {
                        return;
                    },
                }
                if context
                    .notify(
                        cookie_agent_protocol::RUNTIMES_CHANGED_METHOD,
                        &cookie_agent_protocol::RuntimesChangedNotification {},
                    )
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

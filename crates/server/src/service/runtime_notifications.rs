use cookie_agent_engine::Engine;
use cookie_agent_protocol::{RuntimeId, ServerContext, SessionId};

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
    /// A connection failure is sent on its own, naming the subscribed
    /// sessions that use the server.
    pub(super) fn start_mcp_change_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_mcp_changes();
        receiver.mark_unchanged();
        let mut failures = self.engine.subscribe_mcp_failures();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let notification = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        cookie_agent_protocol::McpChangedNotification::default()
                    }
                    failure = failures.recv() => match failure {
                        Ok(failure) => cookie_agent_protocol::McpChangedNotification {
                            failures: mcp_failure(&engine, &context.subscribed_sessions(), failure)
                                .into_iter()
                                .collect(),
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if context
                    .notify(cookie_agent_protocol::MCP_CHANGED_METHOD, &notification)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals runtime template and instance status changes to this
    /// connection; bursts coalesce. An instance failure is sent on its own,
    /// naming the subscribed sessions attached to the instance.
    pub(super) fn start_runtime_instance_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_runtime_instance_changes();
        receiver.mark_unchanged();
        let mut failures = self.engine.subscribe_runtime_failures();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let notification = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        cookie_agent_protocol::RuntimesChangedNotification::default()
                    }
                    failure = failures.recv() => match failure {
                        Ok(failure) => cookie_agent_protocol::RuntimesChangedNotification {
                            failures: runtime_failure(
                                &engine,
                                &context.subscribed_sessions(),
                                failure,
                            )
                            .into_iter()
                            .collect(),
                        },
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if context
                    .notify(
                        cookie_agent_protocol::RUNTIMES_CHANGED_METHOD,
                        &notification,
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Sends the current background calls of a subscribed session whenever
    /// they change. A connection that falls behind gets every subscribed
    /// session's list again.
    pub(super) fn start_background_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_background_changes();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let sessions = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.recv() => match changed {
                        Ok(session) => context
                            .subscribed_sessions()
                            .into_iter()
                            .filter(|subscribed| *subscribed == session)
                            .collect::<Vec<_>>(),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            context.subscribed_sessions()
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                for session_id in sessions {
                    let notification = cookie_agent_protocol::BackgroundChangedNotification {
                        session_id,
                        calls: engine.background_calls(session_id),
                    };
                    if context
                        .notify(
                            cookie_agent_protocol::BACKGROUND_CHANGED_METHOD,
                            &notification,
                        )
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }

    /// Tells a connection that just subscribed to `session` about the
    /// failures that already affect it: failures reported before the
    /// subscription named no session of this connection.
    pub(super) fn replay_connection_failures(&self, context: &ServerContext, session: SessionId) {
        let sessions = [session];
        let mcp = self
            .engine
            .mcp_failures()
            .into_iter()
            .filter_map(|failure| mcp_failure(&self.engine, &sessions, failure))
            .collect::<Vec<_>>();
        let runtimes = self
            .engine
            .runtime_instances()
            .into_iter()
            .filter(|instance| {
                instance.status == cookie_agent_engine::RuntimeInstanceStatus::Failed
            })
            .filter_map(|instance| {
                runtime_failure(
                    &self.engine,
                    &sessions,
                    cookie_agent_engine::RuntimeFailure {
                        runtime_id: instance.runtime_id,
                        workspace: instance.workspace,
                        reason: instance.message.unwrap_or_default(),
                    },
                )
            })
            .collect::<Vec<_>>();
        if mcp.is_empty() && runtimes.is_empty() {
            return;
        }
        let context = context.clone();
        tokio::spawn(async move {
            if !mcp.is_empty() {
                let notification = cookie_agent_protocol::McpChangedNotification { failures: mcp };
                if context
                    .notify(cookie_agent_protocol::MCP_CHANGED_METHOD, &notification)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if !runtimes.is_empty() {
                let notification =
                    cookie_agent_protocol::RuntimesChangedNotification { failures: runtimes };
                let _ = context
                    .notify(
                        cookie_agent_protocol::RUNTIMES_CHANGED_METHOD,
                        &notification,
                    )
                    .await;
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

/// The sessions among `sessions` attached to a runtime that `matches`.
fn affected_sessions(
    engine: &Engine,
    sessions: &[SessionId],
    matches: impl Fn(&cookie_agent_protocol::RuntimeAttachment) -> bool,
) -> Vec<SessionId> {
    let mut affected = sessions
        .iter()
        .copied()
        .filter(|session| {
            engine
                .session_runtimes(*session)
                .is_ok_and(|attachments| attachments.iter().any(&matches))
        })
        .collect::<Vec<_>>();
    affected.sort_unstable();
    affected
}

/// `failure` as a client sees it, when one of `sessions` uses the server:
/// it is attached to the instance that hosts it. Top-level MCP servers
/// belong to the global runtime.
fn mcp_failure(
    engine: &Engine,
    sessions: &[SessionId],
    failure: cookie_agent_engine::McpServerFailure,
) -> Option<cookie_agent_protocol::McpServerFailure> {
    let runtime_id = RuntimeId::new(failure.runtime_id.as_str()).ok()?;
    let session_ids = affected_sessions(engine, sessions, |attachment| {
        attachment.runtime_id == runtime_id && attachment.workspace == failure.workspace
    });
    (!session_ids.is_empty()).then(|| cookie_agent_protocol::McpServerFailure {
        runtime_id: runtime_id.clone(),
        workspace: failure.workspace,
        server: failure.server,
        reason: failure.reason,
        session_ids,
    })
}

/// `failure` as a client sees it, when one of `sessions` is attached to the
/// instance.
fn runtime_failure(
    engine: &Engine,
    sessions: &[SessionId],
    failure: cookie_agent_engine::RuntimeFailure,
) -> Option<cookie_agent_protocol::RuntimeFailure> {
    let runtime_id = RuntimeId::new(failure.runtime_id.as_str()).ok()?;
    let session_ids = affected_sessions(engine, sessions, |attachment| {
        attachment.runtime_id == runtime_id && attachment.workspace == failure.workspace
    });
    (!session_ids.is_empty()).then(|| cookie_agent_protocol::RuntimeFailure {
        runtime_id: runtime_id.clone(),
        workspace: failure.workspace,
        reason: failure.reason,
        session_ids,
    })
}

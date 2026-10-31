use std::collections::HashMap;

use cookie_agent_engine::Engine;
use cookie_agent_protocol::{RuntimeId, SessionId};
use cookie_agent_protocol_transport::ServerContext;

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

    /// Forwards `provider.auth.completed` for sign-ins the daemon's own
    /// listener finished.
    pub(super) fn start_provider_auth_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_provider_auth();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let completed = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    completed = receiver.recv() => match completed {
                        Ok(completed) => completed,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                if context
                    .notify(
                        cookie_agent_protocol::PROVIDER_AUTH_COMPLETED_METHOD,
                        &completed,
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Sends `onboarding.changed` whenever `onboarding.get`'s answer
    /// changes. It changes only with the configuration or the runtime, both
    /// of which publish a runtime change; the engine computes it once per
    /// publication, and each connection compares it with what it last sent.
    pub(super) fn start_onboarding_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_onboarding();
        let mut last = receiver.borrow_and_update().clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => if changed.is_err() {
                        return;
                    },
                }
                let state = receiver.borrow_and_update().clone();
                if state == last {
                    continue;
                }
                last = state.clone();
                if context
                    .notify(
                        cookie_agent_protocol::ONBOARDING_CHANGED_METHOD,
                        &crate::onboarding::status_to_wire(state),
                    )
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
                // A deleted session's subscription ends with it: nothing more
                // is forwarded for it, and its failures no longer name it.
                if let cookie_agent_protocol::SessionsChangedNotification::Removed { session_id } =
                    &change
                {
                    context.unregister_session_subscription(*session_id);
                }
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

    /// Signals plugin status changes to this connection; bursts coalesce.
    pub(super) fn start_plugin_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_plugin_changes();
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
                        cookie_agent_protocol::PLUGINS_CHANGED_METHOD,
                        &cookie_agent_protocol::PluginsChangedNotification {},
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals machine registry changes to this connection; bursts
    /// coalesce.
    pub(super) fn start_machine_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_machine_changes();
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
                        cookie_agent_protocol::MACHINES_CHANGED_METHOD,
                        &cookie_agent_protocol::MachinesChangedNotification {},
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals project changes to this connection; bursts coalesce.
    pub(super) fn start_project_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_project_changes();
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
                        cookie_agent_protocol::PROJECTS_CHANGED_METHOD,
                        &cookie_agent_protocol::ProjectsChangedNotification {},
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals cron job changes to this connection; bursts coalesce.
    pub(super) fn start_cron_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_cron_changes();
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
                        cookie_agent_protocol::CRONS_CHANGED_METHOD,
                        &cookie_agent_protocol::CronsChangedNotification {},
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals push subscription changes to this connection; bursts
    /// coalesce.
    pub(super) fn start_push_notifications(&self, context: ServerContext) {
        let mut receiver = self.push.subscribe_changes();
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
                        cookie_agent_protocol::NOTIFICATIONS_CHANGED_METHOD,
                        &cookie_agent_protocol::NotificationsChangedNotification {},
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Tells this connection about a scheduled restart: at once when one is
    /// already scheduled, then whenever its `when` or the active runs
    /// change.
    pub(super) fn start_restart_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_restart();
        receiver.mark_changed();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => if changed.is_err() {
                        return;
                    },
                }
                let Some(status) = *receiver.borrow_and_update() else {
                    continue;
                };
                if context
                    .notify(
                        cookie_agent_protocol::DAEMON_RESTARTING_METHOD,
                        &restarting_notification(status),
                    )
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Signals memory files this daemon changed to this connection;
    /// bursts coalesce.
    pub(super) fn start_memory_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_memory_changes();
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
                        cookie_agent_protocol::MEMORY_CHANGED_METHOD,
                        &cookie_agent_protocol::MemoryChangedNotification {},
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
    /// connection, and changes to what keeps each runtime busy; bursts
    /// coalesce. An instance failure is sent on its own, naming the
    /// subscribed sessions attached to the instance.
    pub(super) fn start_runtime_instance_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_runtime_instance_changes();
        receiver.mark_unchanged();
        let mut failures = self.engine.subscribe_runtime_failures();
        let mut activity_changes = self.engine.subscribe_runtime_activity();
        activity_changes.mark_unchanged();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            let mut activity = engine.runtime_activity();
            loop {
                let notification = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        cookie_agent_protocol::RuntimesChangedNotification::default()
                    }
                    changed = activity_changes.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        // A run of a session that attaches only the
                        // built-in runtimes changes nothing listed.
                        let now = engine.runtime_activity();
                        if now == activity {
                            continue;
                        }
                        activity = now;
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
    /// they change. A change that leaves a session's list as this connection
    /// was last sent it sends nothing. A connection that falls behind gets
    /// every subscribed session's list again.
    pub(super) fn start_background_notifications(&self, context: ServerContext) {
        let mut receiver = self.engine.subscribe_background_changes();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            // The list last sent for each subscribed session.
            let mut sent = HashMap::<SessionId, Vec<cookie_agent_protocol::BackgroundCall>>::new();
            loop {
                let sessions = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    changed = receiver.recv() => match changed {
                        Ok(session) => {
                            let subscribed = context.subscribed_sessions();
                            // A session subscribed again starts from what it
                            // is sent next.
                            sent.retain(|session, _| subscribed.contains(session));
                            subscribed
                                .into_iter()
                                .filter(|subscribed| *subscribed == session)
                                .collect::<Vec<_>>()
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            sent.clear();
                            context.subscribed_sessions()
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                };
                for session_id in sessions {
                    let calls = engine.background_calls(session_id);
                    if sent.get(&session_id) == Some(&calls) {
                        continue;
                    }
                    sent.insert(session_id, calls.clone());
                    let notification =
                        cookie_agent_protocol::BackgroundChangedNotification { session_id, calls };
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
                        capability: instance.capability,
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
/// it is attached to the instance that hosts it, or, for a server its
/// runtime hosts without a workspace (a runtime-scoped one), to any
/// instance of that runtime. Top-level MCP servers belong to the global
/// runtime.
fn mcp_failure(
    engine: &Engine,
    sessions: &[SessionId],
    failure: cookie_agent_engine::McpServerFailure,
) -> Option<cookie_agent_protocol::McpServerFailure> {
    let runtime_id = RuntimeId::new(failure.runtime_id.as_str()).ok()?;
    let session_ids = affected_sessions(engine, sessions, |attachment| {
        attachment.runtime_id == runtime_id
            && (failure.workspace.is_none() || attachment.workspace == failure.workspace)
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
    let capability = cookie_agent_protocol::CapabilityName::new(failure.capability).ok()?;
    let session_ids = affected_sessions(engine, sessions, |attachment| {
        attachment.runtime_id == runtime_id
            && attachment.capability == capability
            && attachment.workspace == failure.workspace
    });
    (!session_ids.is_empty()).then(|| cookie_agent_protocol::RuntimeFailure {
        runtime_id: runtime_id.clone(),
        capability: capability.clone(),
        workspace: failure.workspace,
        reason: failure.reason,
        session_ids,
    })
}

/// A restart's status as clients are told it, with what it waits for.
fn restarting_notification(
    status: cookie_agent_engine::RestartStatus,
) -> cookie_agent_protocol::DaemonRestartingNotification {
    let reason = if status.due {
        "restarting now".to_owned()
    } else {
        let count = |count: u32, one: &str, many: &str| {
            (count > 0).then(|| format!("{count} {}", if count == 1 { one } else { many }))
        };
        let waiting = [
            count(status.active_runs, "running turn", "running turns"),
            count(
                status.background_calls,
                "background call",
                "background calls",
            ),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" and ");
        format!("waiting for {waiting} to finish")
    };
    cookie_agent_protocol::DaemonRestartingNotification {
        when: status.when,
        active_runs: status.active_runs,
        background_calls: status.background_calls,
        reason: Some(reason),
    }
}

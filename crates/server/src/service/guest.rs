//! Guest connections (`docs/site/specs/conversation-shares.md` section 5):
//! `/s/<id>/ws` speaks the client protocol to whoever holds a conversation
//! share, limited to that link's session and what it allows.
//!
//! [`access`] is the one list of what a guest may ask for. A method it does
//! not name is refused, so a method added to the protocol later stays out of
//! reach until someone decides otherwise; a test keeps every method decided.
//! Notifications go the same way: [`GuestGuard::notify`] passes the few it
//! names, and only about this session's tree. The link is read again before
//! each request, so a revoked link or a change to it applies at once, and
//! [`watch_link`] closes the connection when the link ends.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts as _, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use cookie_agent_engine::{Engine, GuestLink};
use cookie_agent_protocol::{
    AssetStore, ClientHello, EventOrigin, GuestHello, RunId, ServerHello, SessionId, ShareId,
    errors, guest_origin,
};
use cookie_agent_protocol_transport::{ConnectionGuard, ServerFault};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use tokio_util::sync::CancellationToken;
use yawc::IncomingUpgrade;

use super::websocket::{WebSocketState, WebSocketStream, socket_options};

/// What a method asks of a guest's link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Access {
    /// Reads nothing of a session and changes nothing.
    Open,
    /// Reads a session of the tree, named by this member of its params.
    Read(&'static str),
    /// Reads an asset, by its `store`: the tree's own, or the assets every
    /// session of a project shows.
    ReadAsset,
    /// Acts in the shared session itself, named by this member.
    Act(&'static str),
    /// Acts on what a session of the tree waits for, named by this member.
    ActInTree(&'static str),
    /// Acts on the shared session's active run, named by its `run_id`.
    ActOnRun,
    /// Continues an upload this connection began, which only it can name.
    ActOnUpload,
    Refused,
}

/// What a guest may ask for.
fn access(method: &str) -> Access {
    use cookie_agent_protocol as protocol;
    match method {
        protocol::DAEMON_STATUS_METHOD | protocol::RUNTIME_SNAPSHOT_GET_METHOD => Access::Open,
        "session.get"
        | "session.usage"
        | "session.tree_usage"
        | "session.children"
        | "session.tree"
        | "session.resume"
        | "session.permission.get"
        | "skills.list"
        | "skills.get"
        | "events.subscribe"
        | protocol::SESSION_GOAL_GET_METHOD
        | protocol::SESSION_PRODUCERS_METHOD
        | protocol::SESSION_TOOLS_GET_METHOD
        | protocol::EVENTS_PAGE_METHOD
        | protocol::EVENT_READ_METHOD
        | protocol::EVENTS_UNSUBSCRIBE_METHOD
        | protocol::SESSION_BACKGROUND_LIST_METHOD
        | protocol::ARTIFACT_READ_METHOD => Access::Read("session_id"),
        "approval.list" => Access::Read("root_session_id"),
        protocol::ASSET_GET_METHOD | protocol::ASSET_READ_METHOD => Access::ReadAsset,
        "session.compact"
        | "session.revert"
        | protocol::SESSION_COMPACT_CANCEL_METHOD
        | protocol::SESSION_GOAL_SET_METHOD
        | protocol::SESSION_GOAL_LIFECYCLE_METHOD
        | protocol::SESSION_HELD_RESUME_METHOD
        | protocol::SESSION_SUBMIT_INPUT_METHOD
        | protocol::SESSION_RECALL_INPUT_METHOD
        | protocol::ARTIFACT_UPLOAD_BEGIN_METHOD => Access::Act("session_id"),
        "approval.respond"
        | protocol::ELICITATION_RESPOND_METHOD
        | protocol::ATTACHMENT_DECIDE_METHOD
        | protocol::SESSION_BACKGROUND_STOP_METHOD => Access::ActInTree("session_id"),
        "run.steer" | "run.cancel" | protocol::RUN_TOOL_BACKGROUND_METHOD => Access::ActOnRun,
        protocol::ARTIFACT_UPLOAD_CHUNK_METHOD
        | protocol::ARTIFACT_UPLOAD_FINISH_METHOD
        | protocol::ARTIFACT_UPLOAD_CANCEL_METHOD => Access::ActOnUpload,
        _ => Access::Refused,
    }
}

fn refused(method: &str) -> ServerFault {
    ServerFault::from_error(
        errors::NOT_ALLOWED_FOR_GUEST
            .error()
            .field("method", method)
            .build(),
    )
}

/// The session a request's params name in `member`.
fn named_session(params: Option<&Value>, member: &str) -> Option<SessionId> {
    serde_json::from_value(params?.get(member)?.clone()).ok()
}

/// What limits one guest connection.
pub(super) struct GuestGuard {
    engine: Engine,
    id: ShareId,
    /// The link as the connection opened: the session, and the label its
    /// events carry for as long as it lasts.
    link: GuestLink,
    /// The sessions of the tree it has been let read. The notifications that
    /// name a session and not its root pass for these.
    seen: Mutex<HashSet<SessionId>>,
}

impl GuestGuard {
    fn new(engine: Engine, id: ShareId, link: GuestLink) -> Self {
        let seen = Mutex::new(HashSet::from([link.session]));
        Self {
            engine,
            id,
            link,
            seen,
        }
    }

    fn root(&self) -> SessionId {
        self.link.session
    }

    /// Whether what an act asks for leaves the session's setup alone: it may
    /// name no `selection`, the agent and model a run uses, and may not reset
    /// the fallback model, as only an explicit choice of the owner's does.
    fn keeps_setup(params: Option<&Value>) -> bool {
        let Some(params) = params else {
            return true;
        };
        params.get("selection").is_none_or(Value::is_null)
            && !params
                .get("reset_fallback")
                .is_some_and(|reset| reset.as_bool() != Some(false))
    }

    fn in_tree(&self, session: SessionId) -> bool {
        let known = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&session);
        if known {
            return true;
        }
        let inside = self.engine.session_in_tree(self.root(), session);
        if inside {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(session);
        }
        inside
    }

    /// Whether an asset's store is one this session's transcript can show:
    /// the tree's own, or a project's or the shared store, whose assets are
    /// named only by IDs nobody can guess and which the guest cannot list.
    fn shows_store(&self, store: &AssetStore) -> bool {
        match store {
            AssetStore::Session { session } => *session == self.root(),
            AssetStore::Project { .. } | AssetStore::Shared => true,
        }
    }
}

#[async_trait]
impl ConnectionGuard for GuestGuard {
    fn origin(&self, hello: &ClientHello) -> EventOrigin {
        let instance = hello.client.instance_id.0.as_bytes();
        let connection =
            u32::from_be_bytes([instance[12], instance[13], instance[14], instance[15]]);
        guest_origin(&self.link.label, connection)
    }

    fn hello(&self, mut hello: ServerHello) -> ServerHello {
        hello.machine_id = None;
        hello.page_url = None;
        hello.page_ticket = None;
        hello.share_url = None;
        hello.guest = Some(GuestHello {
            session_id: self.root(),
            label: self.link.label.clone(),
            can_send: self.link.can_send,
            expires_at: self.link.expires_at,
        });
        hello
    }

    async fn admit(&self, method: &str, params: Option<&Value>) -> Result<(), ServerFault> {
        let wanted = access(method);
        if wanted == Access::Refused {
            return Err(refused(method));
        }
        // The link as it is now: gone, it refuses everything, and watching
        // only is decided here.
        let link = self
            .engine
            .guest_link(self.id.clone())
            .await
            .ok()
            .flatten()
            .ok_or_else(|| refused(method))?;
        let allowed = match wanted {
            Access::Open => true,
            Access::Read(member) => {
                named_session(params, member).is_some_and(|session| self.in_tree(session))
            }
            Access::ReadAsset => params
                .and_then(|params| params.get("store"))
                .and_then(|store| serde_json::from_value::<AssetStore>(store.clone()).ok())
                .is_some_and(|store| self.shows_store(&store)),
            Access::Act(member) => {
                link.can_send
                    && Self::keeps_setup(params)
                    && named_session(params, member) == Some(self.root())
            }
            Access::ActInTree(member) => {
                link.can_send
                    && named_session(params, member).is_some_and(|session| self.in_tree(session))
            }
            Access::ActOnRun => {
                link.can_send
                    && params
                        .and_then(|params| params.get("run_id"))
                        .and_then(|run| serde_json::from_value::<RunId>(run.clone()).ok())
                        .and_then(|run| self.engine.session_of_active_run(run))
                        == Some(self.root())
            }
            Access::ActOnUpload => link.can_send,
            Access::Refused => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(refused(method))
        }
    }

    fn notify(&self, method: &str, params: Option<&RawValue>) -> bool {
        use cookie_agent_protocol as protocol;
        match method {
            // Only sessions this connection was let subscribe to.
            "events.subscription"
            | protocol::DAEMON_STATUS_METHOD
            | protocol::DAEMON_RESTARTING_METHOD
            | protocol::RUNTIME_CHANGED_METHOD
            | protocol::ASSETS_CHANGED_METHOD => true,
            protocol::BACKGROUND_CHANGED_METHOD => {
                #[derive(Deserialize)]
                struct Changed {
                    session_id: SessionId,
                }
                params
                    .and_then(|params| serde_json::from_str::<Changed>(params.get()).ok())
                    .is_some_and(|changed| {
                        self.seen
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .contains(&changed.session_id)
                    })
            }
            protocol::SESSIONS_CHANGED_METHOD => params
                .and_then(|params| serde_json::from_str(params.get()).ok())
                .is_some_and(|change| self.about_tree(&change)),
            _ => false,
        }
    }
}

impl GuestGuard {
    /// Whether a `sessions.changed` is about this tree. One of a kind this
    /// build does not know stays out.
    fn about_tree(&self, change: &Value) -> bool {
        let session = |value: &Value| serde_json::from_value::<SessionId>(value.clone()).ok();
        match change.get("change").and_then(Value::as_str) {
            Some("upserted") => {
                let Some(meta) = change.get("session") else {
                    return false;
                };
                meta.get("session_id").and_then(session) == Some(self.root())
                    || meta
                        .get("origin")
                        .and_then(|origin| origin.get("root_session_id"))
                        .and_then(session)
                        == Some(self.root())
            }
            Some("removed") => change
                .get("session_id")
                .and_then(session)
                .is_some_and(|gone| {
                    self.seen
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .contains(&gone)
                }),
            Some("approvals_changed" | "permission_mode_changed") => {
                change.get("root_session_id").and_then(session) == Some(self.root())
            }
            Some("resync") => true,
            _ => false,
        }
    }
}

/// Closes a guest connection, by `close`, once its link no longer works or
/// changes: revoked, removed with its session, past its expiry, or set up
/// otherwise. A client whose link still works connects again and hears from
/// the handshake what it may do now, under the label it now goes by.
async fn watch_link(engine: Engine, id: ShareId, opened: GuestLink, close: CancellationToken) {
    let mut changes = engine.subscribe_share_changes();
    loop {
        let link = match engine.guest_link(id.clone()).await {
            Ok(Some(link)) if link == opened => link,
            _ => {
                close.cancel();
                return;
            }
        };
        let until_expiry = link.expires_at.map(|at| {
            let left = at.duration_since(jiff::Timestamp::now());
            std::time::Duration::try_from(left).unwrap_or_default()
        });
        tokio::select! {
            () = close.cancelled() => return,
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = async {
                match until_expiry {
                    Some(left) => tokio::time::sleep(left).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    }
}

/// `/s/<id>/ws`: a guest connecting. The link is the only credential, and
/// any `Origin` may present it: the guest page's is `null`.
pub(super) async fn guest_upgrade(
    state: WebSocketState,
    id: ShareId,
    request: Request,
) -> Response {
    let (mut parts, _) = request.into_parts();
    let Ok(upgrade) = IncomingUpgrade::from_request_parts(&mut parts, &()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(server) = state.server.get() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let engine = server.engine.clone();
    let Ok(Some(link)) = engine.guest_link(id.clone()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let shared = state.shared;
    // Held until the connection has closed, as for the owner's connections.
    let open = shared.connections.subscribe();
    let Ok((response, upgraded)) = upgrade.upgrade(socket_options()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let server = state.server;
    tokio::spawn(async move {
        if let Ok(socket) = upgraded.await {
            let presence = engine.guest_presence(id.clone());
            let stream = WebSocketStream::client(socket, Arc::clone(&shared.restarting));
            let close = shared.shutdown.child_token();
            let watcher = tokio::spawn(watch_link(
                engine.clone(),
                id.clone(),
                link.clone(),
                close.clone(),
            ));
            let guard: Arc<dyn ConnectionGuard> = Arc::new(GuestGuard::new(engine, id, link));
            let _ = cookie_agent_protocol_transport::serve_starting(
                cookie_agent_protocol_transport::Startup {
                    server,
                    status: shared.board.subscribe(),
                    about: cookie_agent_protocol_transport::About {
                        engine_version: cookie_agent_engine::release::version().to_owned(),
                        page_url: None,
                        page_ticket: None,
                        share_url: None,
                    },
                    guard: Some(guard),
                },
                stream,
                close.clone(),
                cookie_agent_protocol_transport::Authentication::Authenticated,
            )
            .await;
            close.cancel();
            watcher.abort();
            drop(presence);
        }
        drop(open);
    });
    response.into_response()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{Direction, MessageKind, Surface, surface_methods};

    use super::*;

    /// Every client method a guest is refused, by name. A method added to
    /// the protocol fails the test below until it is put here or in
    /// [`access`], so somebody decides.
    const REFUSED: &[&str] = &[
        "handshake",
        "run.start",
        "session.create",
        "session.list",
        "session.rename",
        "session.set_permission_mode",
        "session.permission.set",
        "session.permission.clear",
        "session.permission.preview",
        "session.tools.set",
        "runtimes.groups",
        "skills.update",
        "agents.update",
        "agents.documents",
        "agents.customize",
        "agents.set_enabled",
        "agents.reset",
        "agents.replace",
        "session.delete",
        "session.fork",
        "session.select_model",
        "mcp.auth.begin",
        "mcp.auth.complete",
        "mcp.auth.cancel",
        "session.runtime.attach",
        "session.runtime.detach",
        "runtimes.list",
        "runtimes.reconnect",
        "runtimes.connect",
        "runtimes.disconnect",
        "runtimes.agents.list",
        "runtimes.agents.set",
        "runtimes.agents.remove",
        "runtimes.agents.rename",
        "machines.list",
        "machines.rename",
        "machines.assign",
        "machines.unassign",
        "machines.merge",
        "machines.forget",
        "machines.forget_runtime",
        "machines.runtime_token",
        "memory.list",
        "memory.read",
        "memory.write",
        "memory.delete",
        "projects.list",
        "project.get",
        "project.create",
        "project.update",
        "project.delete",
        "session.project.set",
        "session.agent_option.set",
        "crons.list",
        "crons.create",
        "crons.update",
        "crons.delete",
        "crons.run",
        "notifications.get",
        "notifications.subscribe",
        "notifications.unsubscribe",
        "notifications.test",
        "client.presence",
        "session.unread.set",
        "session.pin.set",
        "assets.list",
        "assets.search",
        "attachment.save_asset",
        "asset.rename",
        "asset.delete",
        "shares.list",
        "share.create",
        "share.update",
        "share.revoke",
        "shares.revoke_all",
        "session_share.create",
        "session_share.update",
        "plugins.list",
        "plugins.restart",
        "plugins.settings.get",
        "plugins.settings.set",
        "config.reload",
        "config.get",
        "config.apply",
        "daemon.restart",
        "onboarding.get",
        "onboarding.preview",
        "onboarding.run",
        "acp.check",
        "mcp.server.list",
        "mcp.server.add",
        "mcp.server.edit",
        "mcp.server.remove",
        "mcp.server.set_enabled",
        "mcp.server.rename",
        "mcp.server.reconnect",
        "mcp.server.persist",
        "provider.connect",
        "provider.disconnect",
        "provider.auth.begin",
        "provider.auth.complete",
        "provider.auth.cancel",
    ];

    #[test]
    fn every_client_method_is_decided_for_guests() {
        let mut undecided = Vec::new();
        let mut names = HashSet::new();
        for spec in surface_methods(Surface::Client) {
            if spec.direction != Direction::ToDaemon || spec.kind != MessageKind::Request {
                continue;
            }
            names.insert(spec.method);
            let listed = REFUSED.contains(&spec.method);
            let allowed = access(spec.method) != Access::Refused;
            if listed == allowed {
                undecided.push(spec.method);
            }
        }
        assert_eq!(undecided, Vec::<&str>::new());
        for spec in surface_methods(Surface::Client) {
            let member = match access(spec.method) {
                Access::Read(member) | Access::Act(member) | Access::ActInTree(member) => member,
                Access::ReadAsset => "store",
                Access::ActOnRun => "run_id",
                _ => continue,
            };
            let schema = serde_json::to_value((spec.params.schema)()).unwrap();
            assert!(
                schema["properties"].get(member).is_some(),
                "{}: its params have no `{member}`: {schema}",
                spec.method
            );
        }
        let stale: Vec<_> = REFUSED
            .iter()
            .filter(|name| !names.contains(*name))
            .collect();
        assert_eq!(stale, Vec::<&&str>::new());
    }
}

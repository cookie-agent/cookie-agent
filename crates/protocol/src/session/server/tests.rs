use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;

/// A transport whose far end is driven by the test.
struct ChannelTransport {
    incoming: mpsc::UnboundedReceiver<MessageFrame>,
    outgoing: mpsc::UnboundedSender<MessageFrame>,
}

#[async_trait]
impl Transport for ChannelTransport {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.outgoing
            .send(frame)
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

/// `session.get` blocks until released; `session.list` answers at once;
/// `events.subscribe` starts a tail that notifies immediately, then takes a
/// while to build its own response.
#[derive(Default)]
struct StubServer {
    release_get: Notify,
}

#[async_trait]
impl ServerProtocol for StubServer {
    async fn list_sessions(
        &self,
        _: crate::SessionListParams,
    ) -> Result<crate::SessionListResult, ServerFault> {
        Ok(crate::SessionListResult {
            sessions: Vec::new(),
        })
    }
    async fn get_session(
        &self,
        _: crate::SessionGetParams,
    ) -> Result<crate::SessionGetResult, ServerFault> {
        self.release_get.notified().await;
        Err(ServerFault::internal())
    }
    async fn subscribe_events(
        &self,
        _: crate::EventsSubscribeParams,
        context: &ServerContext,
    ) -> Result<crate::EventsSubscribeResult, ServerFault> {
        let context = context.clone();
        tokio::spawn(async move {
            let _ = context.notify("stub.tail", &json!({})).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(crate::EventsSubscribeResult {
            events: Vec::new(),
            has_more: false,
            stale_cursor: false,
        })
    }
    async fn create_session(
        &self,
        _: crate::SessionCreateParams,
    ) -> Result<crate::SessionCreateResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_session_goal(
        &self,
        _: crate::SessionGoalGetParams,
    ) -> Result<crate::SessionGoalGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_session_goal(
        &self,
        _: crate::SessionGoalSetParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionGoalSetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn change_session_goal_lifecycle(
        &self,
        _: crate::SessionGoalLifecycleParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionGoalLifecycleResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_producers(
        &self,
        _: crate::SessionProducersParams,
    ) -> Result<crate::SessionProducersResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_usage(
        &self,
        _: crate::SessionUsageParams,
    ) -> Result<crate::SessionUsageResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_tree_usage(
        &self,
        _: crate::SessionUsageParams,
    ) -> Result<crate::SessionTreeUsageResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_children(
        &self,
        _: crate::SessionChildrenParams,
    ) -> Result<crate::SessionChildrenResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_tree(
        &self,
        _: crate::SessionTreeParams,
    ) -> Result<crate::SessionTreeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn resume_session(
        &self,
        _: crate::SessionResumeParams,
    ) -> Result<crate::SessionResumeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn rename_session(
        &self,
        _: crate::SessionRenameParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionRenameResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_permission_mode(
        &self,
        _: crate::SessionSetPermissionModeParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionSetPermissionModeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_session_permissions(
        &self,
        _: crate::SessionPermissionGetParams,
    ) -> Result<crate::SessionPermissionGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_session_permission(
        &self,
        _: crate::SessionPermissionSetParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionPermissionMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn clear_session_permission(
        &self,
        _: crate::SessionPermissionClearParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionPermissionMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn preview_session_permissions(
        &self,
        _: crate::SessionPermissionPreviewParams,
    ) -> Result<crate::SessionPermissionPreviewResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_skills(
        &self,
        _: crate::SkillsListParams,
    ) -> Result<crate::SkillsListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_skill(
        &self,
        _: crate::SkillsGetParams,
    ) -> Result<crate::SkillsGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn compact_session(
        &self,
        _: crate::SessionCompactParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionCompactResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_compaction(
        &self,
        _: crate::SessionCompactCancelParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionCompactCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn revert_session(
        &self,
        _: crate::SessionRevertParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionRevertResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn fork_session(
        &self,
        _: crate::SessionForkParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionForkResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn start_run(
        &self,
        _: crate::RunStartParams,
        _context: &ServerContext,
    ) -> Result<crate::RunStartResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn steer_run(
        &self,
        _: crate::RunSteerParams,
        _context: &ServerContext,
    ) -> Result<crate::RunSteerResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn select_model(
        &self,
        _: crate::SessionSelectModelParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionSelectModelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn unsubscribe_events(
        &self,
        _: crate::EventsUnsubscribeParams,
        _context: &ServerContext,
    ) -> Result<crate::EventsUnsubscribeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn submit_input(
        &self,
        _: crate::SessionSubmitInputParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionSubmitInputResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn recall_input(
        &self,
        _: crate::SessionRecallInputParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionRecallInputResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_run(
        &self,
        _: crate::RunCancelParams,
        _context: &ServerContext,
    ) -> Result<crate::RunCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn respond_approval(
        &self,
        _: crate::ApprovalRespondParams,
        _context: &ServerContext,
    ) -> Result<crate::ApprovalRespondResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_approvals(
        &self,
        _: crate::ApprovalListParams,
    ) -> Result<crate::ApprovalListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn begin_mcp_auth(
        &self,
        _: crate::McpAuthBeginParams,
    ) -> Result<crate::McpAuthBeginResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn complete_mcp_auth(
        &self,
        _: crate::McpAuthCompleteParams,
    ) -> Result<crate::McpAuthCompleteResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_mcp_auth(
        &self,
        _: crate::McpAuthCancelParams,
    ) -> Result<crate::McpAuthCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn attach_runtime(
        &self,
        _: crate::SessionRuntimeAttachParams,
        _: &ServerContext,
    ) -> Result<crate::SessionRuntimeAttachResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn detach_runtime(
        &self,
        _: crate::SessionRuntimeDetachParams,
        _: &ServerContext,
    ) -> Result<crate::SessionRuntimeDetachResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_runtimes(
        &self,
        _: crate::RuntimesListParams,
    ) -> Result<crate::RuntimesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn reconnect_runtime(
        &self,
        _: crate::RuntimesReconnectParams,
    ) -> Result<crate::RuntimesReconnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_machines(
        &self,
        _: crate::MachinesListParams,
    ) -> Result<crate::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn rename_machine(
        &self,
        _: crate::MachinesRenameParams,
    ) -> Result<crate::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn assign_machine(
        &self,
        _: crate::MachinesAssignParams,
    ) -> Result<crate::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn unassign_machine(
        &self,
        _: crate::MachinesUnassignParams,
    ) -> Result<crate::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn merge_machines(
        &self,
        _: crate::MachinesMergeParams,
    ) -> Result<crate::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn forget_machine(
        &self,
        _: crate::MachinesForgetParams,
    ) -> Result<crate::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_plugins(
        &self,
        _: crate::PluginsListParams,
    ) -> Result<crate::PluginsListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn restart_plugin(
        &self,
        _: crate::PluginsRestartParams,
    ) -> Result<crate::PluginsRestartResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_config(
        &self,
        _: crate::ConfigGetParams,
    ) -> Result<crate::ConfigGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn apply_config(
        &self,
        _: crate::ConfigApplyParams,
    ) -> Result<crate::ConfigApplyResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn reload_config(
        &self,
        _: crate::ConfigReloadParams,
    ) -> Result<crate::ConfigReloadResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn background_tool(
        &self,
        _: crate::RunToolBackgroundParams,
        _context: &ServerContext,
    ) -> Result<crate::RunToolBackgroundResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_background(
        &self,
        _: crate::SessionBackgroundListParams,
    ) -> Result<crate::SessionBackgroundListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn stop_background(
        &self,
        _: crate::SessionBackgroundStopParams,
        _context: &ServerContext,
    ) -> Result<crate::SessionBackgroundStopResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_mcp_servers(
        &self,
        _: crate::McpServerListParams,
    ) -> Result<crate::McpServerListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn add_mcp_server(
        &self,
        _: crate::McpServerAddParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn edit_mcp_server(
        &self,
        _: crate::McpServerEditParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn remove_mcp_server(
        &self,
        _: crate::McpServerNameParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_mcp_server_enabled(
        &self,
        _: crate::McpServerSetEnabledParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn reconnect_mcp_server(
        &self,
        _: crate::McpServerReconnectParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn persist_mcp_server(
        &self,
        _: crate::McpServerPersistParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn begin_upload(
        &self,
        _: crate::ArtifactUploadBeginParams,
        _: &ServerContext,
    ) -> Result<crate::ArtifactUploadBeginResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn upload_chunk(
        &self,
        _: crate::ArtifactUploadChunkParams,
        _: &ServerContext,
    ) -> Result<crate::ArtifactUploadChunkResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn finish_upload(
        &self,
        _: crate::ArtifactUploadFinishParams,
        _: &ServerContext,
    ) -> Result<crate::ArtifactUploadFinishResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_upload(
        &self,
        _: crate::ArtifactUploadCancelParams,
        _: &ServerContext,
    ) -> Result<crate::ArtifactUploadCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn read_artifact(
        &self,
        _: crate::ArtifactReadParams,
    ) -> Result<crate::ArtifactReadResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn runtime_snapshot(
        &self,
        _: crate::RuntimeSnapshotGetParams,
    ) -> Result<crate::RuntimeSnapshotResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn connect_provider(
        &self,
        _: crate::ProviderConnectParams,
    ) -> Result<crate::ProviderConnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn disconnect_provider(
        &self,
        _: crate::ProviderDisconnectParams,
    ) -> Result<crate::ProviderDisconnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
}

struct Connection {
    requests: mpsc::UnboundedSender<MessageFrame>,
    frames: mpsc::UnboundedReceiver<MessageFrame>,
}

impl Connection {
    async fn open(server: Arc<StubServer>) -> Self {
        let (requests, incoming) = mpsc::unbounded_channel();
        let (outgoing, frames) = mpsc::unbounded_channel();
        tokio::spawn(serve(
            server,
            ChannelTransport { incoming, outgoing },
            CancellationToken::new(),
            Authentication::Authenticated,
        ));
        let mut connection = Self { requests, frames };
        connection.send(0, "handshake", hello(None));
        assert_eq!(connection.next().await["id"], 0);
        connection
    }

    /// A connection that must authenticate in its handshake within
    /// `deadline`, not yet handshaken.
    fn unauthenticated(server: Arc<StubServer>, deadline: Duration) -> Self {
        let (requests, incoming) = mpsc::unbounded_channel();
        let (outgoing, frames) = mpsc::unbounded_channel();
        tokio::spawn(serve_within(
            server,
            ChannelTransport { incoming, outgoing },
            CancellationToken::new(),
            Authentication::Token(Arc::new(zeroize::Zeroizing::new(TOKEN.to_owned()))),
            deadline,
        ));
        Self { requests, frames }
    }

    /// Whether the server closed the connection.
    async fn closed(&mut self) -> bool {
        tokio::time::timeout(Duration::from_secs(2), self.frames.recv())
            .await
            .expect("the server acts in time")
            .is_none()
    }

    fn send(&self, id: i64, method: &str, params: Value) {
        self.requests
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            })))
            .expect("server reading");
    }

    async fn next(&mut self) -> Value {
        let frame = tokio::time::timeout(Duration::from_secs(1), self.frames.recv())
            .await
            .expect("frame in time")
            .expect("server open");
        let MessageFrame::Text(text) = frame else {
            panic!("expected a text frame");
        };
        serde_json::from_str(&text).expect("JSON frame")
    }
}

const TOKEN: &str = "the-daemon-token";

fn hello(token: Option<&str>) -> Value {
    let mut hello = json!({
        "protocol_version": crate::CLIENT_PROTOCOL_VERSION,
        "client": {"kind": "test", "instance_id": crate::ClientInstanceId::new_v7()},
    });
    if let Some(token) = token {
        hello["token"] = json!(token);
    }
    hello
}

#[tokio::test]
async fn an_unauthenticated_connection_may_only_handshake_with_the_token() {
    let mut connection =
        Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_secs(60));
    connection.send(1, "session.list", json!({}));
    let reply = connection.next().await;
    assert_eq!(reply["error"]["code"], crate::errors::UNAUTHENTICATED);
    assert_eq!(reply["error"]["data"]["code"], "unauthenticated");
    // A handshake of another version is refused before the token is read,
    // and the connection stays open.
    connection.send(
        2,
        "handshake",
        json!({"protocol_version": "0.0.1", "client": {"kind": "test", "instance_id": crate::ClientInstanceId::new_v7()}}),
    );
    assert_eq!(
        connection.next().await["error"]["code"],
        crate::errors::UNSUPPORTED_PROTOCOL
    );
    connection.send(3, "handshake", hello(Some(TOKEN)));
    let reply = connection.next().await;
    assert_eq!(
        reply["result"]["protocol_version"],
        crate::CLIENT_PROTOCOL_VERSION.to_string(),
        "{reply}"
    );
    connection.send(4, "session.list", json!({}));
    assert_eq!(connection.next().await["result"]["sessions"], json!([]));
}

#[tokio::test]
async fn a_wrong_or_missing_token_is_refused_and_closes_the_connection() {
    for token in [Some("wrong"), Some(""), None] {
        let mut connection =
            Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_secs(60));
        connection.send(1, "handshake", hello(token));
        let reply = connection.next().await;
        assert_eq!(
            reply["error"]["code"],
            crate::errors::UNAUTHENTICATED,
            "{token:?}"
        );
        assert!(connection.closed().await, "{token:?}");
    }
}

#[tokio::test]
async fn a_connection_that_does_not_authenticate_in_time_is_closed() {
    let mut connection =
        Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_millis(100));
    // Being answered does not extend the deadline.
    connection.send(1, "session.list", json!({}));
    assert_eq!(
        connection.next().await["error"]["code"],
        crate::errors::UNAUTHENTICATED
    );
    assert!(connection.closed().await);

    // Authenticating in time keeps it open past the deadline.
    let mut connection =
        Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_millis(100));
    connection.send(1, "handshake", hello(Some(TOKEN)));
    assert!(connection.next().await["result"].is_object());
    tokio::time::sleep(Duration::from_millis(200)).await;
    connection.send(2, "session.list", json!({}));
    assert_eq!(connection.next().await["id"], 2);
}

#[tokio::test]
async fn an_authenticated_connection_must_still_handshake_first() {
    let (requests, incoming) = mpsc::unbounded_channel();
    let (outgoing, frames) = mpsc::unbounded_channel();
    tokio::spawn(serve(
        Arc::new(StubServer::default()),
        ChannelTransport { incoming, outgoing },
        CancellationToken::new(),
        Authentication::Authenticated,
    ));
    let mut connection = Connection { requests, frames };
    connection.send(1, "session.list", json!({}));
    let reply = connection.next().await;
    assert_eq!(reply["error"]["code"], crate::errors::HANDSHAKE_REQUIRED);
    assert_eq!(reply["error"]["data"]["code"], "handshake_required");
    connection.send(2, "handshake", hello(None));
    assert!(connection.next().await["result"].is_object());
}

#[tokio::test]
async fn a_slow_request_does_not_hold_up_later_ones() {
    let server = Arc::new(StubServer::default());
    let mut connection = Connection::open(Arc::clone(&server)).await;
    let session_id = crate::SessionId::new_v7();
    connection.send(1, "session.get", json!({ "session_id": session_id }));
    connection.send(2, "session.list", json!({}));
    assert_eq!(
        connection.next().await["id"],
        2,
        "the fast request answers first"
    );
    server.release_get.notify_one();
    assert_eq!(connection.next().await["id"], 1);
}

#[tokio::test]
async fn a_request_s_notifications_follow_its_response() {
    let mut connection = Connection::open(Arc::new(StubServer::default())).await;
    let session_id = crate::SessionId::new_v7();
    connection.send(1, "events.subscribe", json!({ "session_id": session_id }));
    let first = connection.next().await;
    assert_eq!(first["id"], 1, "the response precedes its tail: {first}");
    assert_eq!(connection.next().await["method"], "stub.tail");
}

#[tokio::test]
async fn a_replacement_session_tail_forwards_after_the_tail_it_replaces() {
    async fn next_gap(outbound: &mut mpsc::Receiver<String>) -> u64 {
        let text = tokio::time::timeout(Duration::from_secs(2), outbound.recv())
            .await
            .expect("notification in time")
            .expect("connection open");
        let value: Value = serde_json::from_str(&text).expect("JSON notification");
        assert_eq!(value["method"], "events.subscription");
        value["params"]["last_delivered_seq"]
            .as_u64()
            .expect("gap sequence")
    }

    let (notifications, mut outbound) = mpsc::channel(16);
    let context = ServerContext::new(notifications, CancellationToken::new());
    let session_id = crate::SessionId::new_v7();
    let gap = |seq| EventSubscriptionMessage::Gap {
        session_id,
        last_delivered_seq: seq,
    };
    let (replaced_sender, replaced) = mpsc::channel(8);
    let (replacement_sender, replacement) = mpsc::channel(8);
    replaced_sender.send(gap(1)).await.expect("queue");
    replacement_sender.send(gap(3)).await.expect("queue");
    context.forward_session_tail(session_id, replaced);
    context.forward_session_tail(session_id, replacement);
    assert_eq!(next_gap(&mut outbound).await, 1);
    // Until its producer closes the replaced tail, what that tail still
    // carries comes first, even though the replacement is ready.
    replaced_sender.send(gap(2)).await.expect("queue");
    assert_eq!(next_gap(&mut outbound).await, 2);
    drop(replaced_sender);
    assert_eq!(next_gap(&mut outbound).await, 3);
}

#[tokio::test]
async fn params_refuse_unknown_members_inside_types_history_reads_tolerantly() {
    let mut connection = Connection::open(Arc::new(StubServer::default())).await;
    let session_id = crate::SessionId::new_v7();
    let selection = json!({
        "agent": "build",
        "model": {"model": "openai/gpt-5", "variant": null},
    });
    let mut nested = selection.clone();
    nested["model"]["extra"] = json!(true);
    let rule = json!({"action": "bash", "resource": "*", "effect": "allow", "note": "x"});
    for (id, method, params, field) in [
        (
            1,
            "run.start",
            json!({
                "session_id": session_id,
                "client_run_id": "run-1",
                "selection": {"agent": "build", "model": selection["model"], "extra": 1},
                "content": [{"type": "text", "text": "hi"}],
            }),
            "selection.extra",
        ),
        (
            2,
            "session.select_model",
            json!({"session_id": session_id, "selection": nested}),
            "selection.model.extra",
        ),
        (
            3,
            "session.create",
            json!({"selection": selection, "permission_rules": [rule]}),
            "permission_rules[0].note",
        ),
        (
            4,
            "approval.respond",
            json!({
                "session_id": session_id,
                "approval_id": crate::ApprovalId::new_v7(),
                "request_revision": 1,
                "operation_fingerprint": {"digest": "a".repeat(64), "extra": 1},
                "client_response_id": "response-1",
                "decision": "reject",
            }),
            "operation_fingerprint.extra",
        ),
        (
            5,
            "session.submit_input",
            json!({
                "session_id": session_id,
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "file", "artifact": {
                        "sha256": "a".repeat(64), "byte_length": 3,
                        "media_type": "image/png", "width": 2,
                    }},
                ],
                "delivery": "steer",
                "client_run_id": "run-2",
            }),
            "content[1].artifact.width",
        ),
    ] {
        connection.send(id, method, params);
        let reply = connection.next().await;
        assert_eq!(reply["id"], id, "{reply}");
        assert_eq!(reply["error"]["code"], -32602, "{reply}");
        assert_eq!(reply["error"]["data"]["code"], "unknown_field", "{reply}");
        assert_eq!(reply["error"]["data"]["field"], field, "{reply}");
    }
}

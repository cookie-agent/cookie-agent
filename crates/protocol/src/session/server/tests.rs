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
        })
    }
    async fn create_session(
        &self,
        _: crate::SessionCreateParams,
    ) -> Result<crate::SessionCreateResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn get_session_goal(
        &self,
        _: crate::SessionGoalGetParams,
    ) -> Result<crate::SessionGoalGetResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn set_session_goal(
        &self,
        _: crate::SessionGoalSetParams,
    ) -> Result<crate::SessionGoalSetResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn change_session_goal_lifecycle(
        &self,
        _: crate::SessionGoalLifecycleParams,
    ) -> Result<crate::SessionGoalLifecycleResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn session_producers(
        &self,
        _: crate::SessionProducersParams,
    ) -> Result<crate::SessionProducersResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn session_usage(
        &self,
        _: crate::SessionUsageParams,
    ) -> Result<crate::SessionUsageResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn session_tree_usage(
        &self,
        _: crate::SessionUsageParams,
    ) -> Result<crate::SessionTreeUsageResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn session_children(
        &self,
        _: crate::SessionChildrenParams,
    ) -> Result<crate::SessionChildrenResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn session_tree(
        &self,
        _: crate::SessionTreeParams,
    ) -> Result<crate::SessionTreeResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn resume_session(
        &self,
        _: crate::SessionResumeParams,
    ) -> Result<crate::SessionResumeResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn rename_session(
        &self,
        _: crate::SessionRenameParams,
    ) -> Result<crate::SessionRenameResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn set_permission_mode(
        &self,
        _: crate::SessionSetPermissionModeParams,
    ) -> Result<crate::SessionSetPermissionModeResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn get_session_permissions(
        &self,
        _: crate::SessionPermissionGetParams,
    ) -> Result<crate::SessionPermissionGetResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn set_session_permission(
        &self,
        _: crate::SessionPermissionSetParams,
    ) -> Result<crate::SessionPermissionMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn clear_session_permission(
        &self,
        _: crate::SessionPermissionClearParams,
    ) -> Result<crate::SessionPermissionMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn list_skills(
        &self,
        _: crate::SkillsListParams,
    ) -> Result<crate::SkillsListResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn get_skill(
        &self,
        _: crate::SkillsGetParams,
    ) -> Result<crate::SkillsGetResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn compact_session(
        &self,
        _: crate::SessionCompactParams,
    ) -> Result<crate::SessionCompactResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn revert_session(
        &self,
        _: crate::SessionRevertParams,
    ) -> Result<crate::SessionRevertResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn fork_session(
        &self,
        _: crate::SessionForkParams,
    ) -> Result<crate::SessionForkResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn start_run(
        &self,
        _: crate::RunStartParams,
    ) -> Result<crate::RunStartResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn steer_run(
        &self,
        _: crate::RunSteerParams,
    ) -> Result<crate::RunSteerResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn recall_steer(
        &self,
        _: crate::RunRecallSteerParams,
    ) -> Result<crate::RunRecallSteerResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn cancel_run(
        &self,
        _: crate::RunCancelParams,
    ) -> Result<crate::RunCancelResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn tool_stdin(
        &self,
        _: crate::RunToolStdinParams,
    ) -> Result<crate::RunToolStdinResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn respond_approval(
        &self,
        _: crate::ApprovalRespondParams,
    ) -> Result<crate::ApprovalRespondResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn list_approvals(
        &self,
        _: crate::ApprovalListParams,
    ) -> Result<crate::ApprovalListResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn begin_mcp_auth(
        &self,
        _: crate::McpAuthBeginParams,
    ) -> Result<crate::McpAuthBeginResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn cancel_mcp_auth(
        &self,
        _: crate::McpAuthCancelParams,
    ) -> Result<crate::McpAuthCancelResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn list_mcp_servers(
        &self,
        _: crate::McpServerListParams,
    ) -> Result<crate::McpServerListResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn add_mcp_server(
        &self,
        _: crate::McpServerAddParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn edit_mcp_server(
        &self,
        _: crate::McpServerEditParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn remove_mcp_server(
        &self,
        _: crate::McpServerNameParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn set_mcp_server_enabled(
        &self,
        _: crate::McpServerSetEnabledParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn reconnect_mcp_server(
        &self,
        _: crate::McpServerNameParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn persist_mcp_server(
        &self,
        _: crate::McpServerPersistParams,
    ) -> Result<crate::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn runtime_snapshot(
        &self,
        _: crate::RuntimeSnapshotGetParams,
    ) -> Result<crate::RuntimeSnapshotResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn connect_provider(
        &self,
        _: crate::ProviderConnectParams,
    ) -> Result<crate::ProviderConnectResult, ServerFault> {
        Err(ServerFault::method_not_found())
    }
    async fn disconnect_provider(
        &self,
        _: crate::ProviderDisconnectParams,
    ) -> Result<crate::ProviderDisconnectResult, ServerFault> {
        Err(ServerFault::method_not_found())
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
        ));
        let mut connection = Self { requests, frames };
        connection.send(
            0,
            "handshake",
            json!({ "protocol_version": crate::PROTOCOL_VERSION }),
        );
        assert_eq!(connection.next().await["id"], 0);
        connection
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
        let MessageFrame::Value(value) = frame else {
            panic!("expected a value frame");
        };
        value
    }
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

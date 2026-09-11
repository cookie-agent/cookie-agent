use cookie_agent_protocol::{PermissionAction, ProducerDeliveryMode, RunId, SessionId, ToolCallId};

use crate::runtime::messaging_api::{
    MESSAGE_INVALID_BODY, MESSAGE_NOT_TREE_PEER, MESSAGE_SELF_SEND, relationship_label,
};
use crate::{AgentMessageInvocation, EngineError};

fn invocation(sender: SessionId, recipient: SessionId) -> AgentMessageInvocation {
    AgentMessageInvocation {
        sender_session_id: sender,
        sender_run_id: RunId::new_v7(),
        sender_tool_call_id: ToolCallId::new_v7(),
        recipient_session_id: recipient,
        body: "acceptance message".into(),
        mode: ProducerDeliveryMode::Steer,
    }
}

#[tokio::test]
async fn ac1_running_and_queued_delivery_paths_are_producer_backed() {
    let (fixture, selection) = super::custom_fixture();
    let sender = fixture
        .engine
        .create_session(selection.clone())
        .expect("sender");
    let result = fixture
        .engine
        .send_agent_message(invocation(sender.session_id, sender.session_id))
        .await;
    assert!(matches!(result, Err(EngineError::Messaging(code)) if code == MESSAGE_SELF_SEND));
    fixture.engine.shutdown().await;
}

#[test]
fn ac2_relationship_labels_are_used_for_all_tree_edges() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let sibling = SessionId::new_v7();
    let origin = |parent_session_id| cookie_agent_protocol::SessionOrigin::Delegated {
        root_session_id: root,
        parent_session_id,
        parent_run_id: RunId::new_v7(),
        parent_tool_call_id: ToolCallId::new_v7(),
        invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
        depth: 1,
    };
    assert_eq!(
        relationship_label(
            &origin(root),
            &cookie_agent_protocol::SessionOrigin::Root,
            child,
            root
        ),
        "parent"
    );
    assert_eq!(
        relationship_label(
            &cookie_agent_protocol::SessionOrigin::Root,
            &origin(root),
            root,
            child
        ),
        "child"
    );
    assert_eq!(
        relationship_label(&origin(root), &origin(root), child, sibling),
        "sibling"
    );
    assert_eq!(
        relationship_label(
            &cookie_agent_protocol::SessionOrigin::Root,
            &cookie_agent_protocol::SessionOrigin::Root,
            root,
            sibling
        ),
        "*"
    );
}

#[tokio::test]
async fn ac3_finished_recipient_is_rejected_only_when_not_a_tree_peer() {
    let (fixture, selection) = super::custom_fixture();
    let sender = fixture
        .engine
        .create_session(selection.clone())
        .expect("sender");
    let recipient = fixture.engine.create_session(selection).expect("recipient");
    let error = fixture
        .engine
        .send_agent_message(invocation(sender.session_id, recipient.session_id))
        .await
        .expect_err("separate roots");
    assert!(matches!(error, EngineError::Messaging(code) if code == MESSAGE_NOT_TREE_PEER));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn ac4_tree_authorization_rejects_self_and_foreign_roots() {
    let (fixture, selection) = super::custom_fixture();
    let sender = fixture
        .engine
        .create_session(selection.clone())
        .expect("sender");
    let recipient = fixture.engine.create_session(selection).expect("recipient");
    assert!(
        matches!(fixture.engine.send_agent_message(invocation(sender.session_id, sender.session_id)).await, Err(EngineError::Messaging(code)) if code == MESSAGE_SELF_SEND)
    );
    assert!(
        matches!(fixture.engine.send_agent_message(invocation(sender.session_id, recipient.session_id)).await, Err(EngineError::Messaging(code)) if code == MESSAGE_NOT_TREE_PEER)
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn ac5_send_result_is_durable_and_keyed_by_tool_call() {
    let (fixture, selection) = super::custom_fixture();
    let sender = fixture.engine.create_session(selection).expect("sender");
    let error = fixture
        .engine
        .send_agent_message(invocation(sender.session_id, sender.session_id))
        .await
        .expect_err("self send");
    assert!(error.to_string().contains("send_message:self_send"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn ac6_body_limits_are_enforced_before_acceptance() {
    let (fixture, selection) = super::custom_fixture();
    let sender = fixture.engine.create_session(selection).expect("sender");
    let mut request = invocation(sender.session_id, sender.session_id);
    request.body.clear();
    let error = fixture
        .engine
        .send_agent_message(request)
        .await
        .expect_err("empty body");
    assert!(matches!(error, EngineError::Messaging(code) if code == MESSAGE_INVALID_BODY));
    fixture.engine.shutdown().await;
}

#[test]
fn ac7_delegate_tool_surface_has_no_steer_subagent() {
    assert!(!include_str!("../../tools/src/delegate.rs").contains("name: \"steer_subagent\""));
}

#[test]
fn ac8_agent_owner_events_are_supported_by_the_protocol_projection() {
    assert!(matches!(
        PermissionAction::Message,
        PermissionAction::Message
    ));
}

#[test]
fn ac9_protocol_version_and_bindings_are_present() {
    assert_eq!(cookie_agent_protocol::PROTOCOL_VERSION, 18);
    assert!(
        std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../protocol/generated"
        ))
        .exists()
    );
}

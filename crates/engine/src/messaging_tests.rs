use cookie_agent_protocol::{InvocationId, RunId, SessionId, SessionOrigin, ToolCallId};

use crate::runtime::messaging_api::{MESSAGE_NOT_TREE_PEER, MESSAGE_SELF_SEND, relationship_label};

#[test]
fn message_relationship_labels_tree_peers() {
    let root = SessionId::new_v7();
    let parent = SessionId::new_v7();
    let child = SessionId::new_v7();
    let sibling = SessionId::new_v7();
    let origin = |root_session_id, parent_session_id| SessionOrigin::Delegated {
        root_session_id,
        parent_session_id,
        parent_run_id: RunId::new_v7(),
        parent_tool_call_id: ToolCallId::new_v7(),
        invocation_id: InvocationId::new_v7(),
        depth: 1,
    };

    assert_eq!(
        relationship_label(&origin(root, root), &SessionOrigin::Root, parent, root,),
        "parent"
    );
    assert_eq!(
        relationship_label(&SessionOrigin::Root, &origin(root, root), root, child),
        "child"
    );
    assert_eq!(
        relationship_label(&origin(root, parent), &origin(root, parent), sibling, child),
        "sibling"
    );
    assert_eq!(
        relationship_label(&SessionOrigin::Root, &SessionOrigin::Root, root, child),
        "*"
    );
}

#[test]
fn stable_self_and_foreign_peer_codes_are_exposed() {
    assert_eq!(MESSAGE_SELF_SEND, "send_message:self_send");
    assert_eq!(MESSAGE_NOT_TREE_PEER, "send_message:not_tree_peer");
}

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, InvocationId, PermissionAction, ProducerDeliveryMode,
    ProducerIdempotencyKey, ProducerOwner, RunId, SessionId, SessionOrigin, ToolCallId,
};

use crate::runtime::messaging::api::{
    MESSAGE_INFLIGHT_FULL, MESSAGE_INVALID_BODY, MESSAGE_MAX_HOPS_EXCEEDED, MESSAGE_NOT_TREE_PEER,
    MESSAGE_SELF_SEND, relationship_label,
};
use crate::{AgentMessageInvocation, EngineError};

fn tree_recipient(engine: &crate::Engine, sender: SessionId) -> SessionId {
    tree_session(engine, sender, sender, 1)
}

/// A session delegated from `parent` in the tree rooted at `root`.
pub(super) fn tree_session(
    engine: &crate::Engine,
    root: SessionId,
    parent: SessionId,
    depth: u32,
) -> SessionId {
    let source = engine.inner.store.get(root).expect("root projection");
    let recipient = SessionId::new_v7();
    engine
        .inner
        .store
        .create(
            recipient,
            EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionCreated {
                cron: None,
                history_format: cookie_agent_protocol::HISTORY_FORMAT,
                short_id: "test_00000000".into(),
                origin: SessionOrigin::Delegated {
                    root_session_id: root,
                    parent_session_id: parent,
                    parent_run_id: RunId::new_v7(),
                    parent_tool_call_id: ToolCallId::new_v7(),
                    invocation_id: InvocationId::new_v7(),
                    depth,
                },
                cwd_identity: source.meta.cwd_identity.clone(),
                creation_selection: source.meta.creation_selection.clone(),
                creation_agent: Box::new(source.creation_agent.as_ref().clone()),
                revisions: cookie_agent_protocol::RuntimeRevisions {
                    runtime_revision: source.meta.runtime_revision.clone(),
                    catalog_revision: source.meta.catalog_revision.clone(),
                    provider_state_revision: source.meta.provider_state_revision.clone(),
                    model_revision: source.meta.model_revision.clone(),
                    agent_revision: source.meta.agent_revision.clone(),
                    recipe_registry_revision: source.meta.recipe_registry_revision.clone(),
                    manifest_revision: source.meta.manifest_revision.clone(),
                },
            },
        )
        .expect("recipient");
    engine.spawn_actor(recipient).expect("recipient actor");
    recipient
}

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
    let (fixture, selection) = super::support::custom_fixture();
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
async fn agent_message_inflight_window_is_directed_and_idempotent() {
    let (mut fixture, selection) = super::support::custom_fixture();
    fixture.config.runtime.messaging.max_inflight_per_pair = 2;
    fixture.engine.shutdown().await;
    fixture.engine =
        super::support::reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    let sender = fixture
        .engine
        .create_session(selection.clone())
        .expect("sender");
    let recipient = tree_recipient(&fixture.engine, sender.session_id);
    let authority = crate::runtime::messaging::producers::ProducerAuthority {
        owner: ProducerOwner::Agent {
            session_id: sender.session_id,
        },
        connection_epoch: None,
    };
    let producer = fixture
        .engine
        .register_producer(recipient, authority.clone())
        .await
        .expect("producer");
    super::support::settle_session_actor(&fixture.engine, recipient).await;
    let description = cookie_agent_protocol::SafeDisplayText::new("test message").unwrap();
    let send = |key: &str| {
        fixture.engine.accept_agent_message_direct_for_test(
            recipient,
            &authority,
            producer,
            ProducerDeliveryMode::Steer,
            ProducerIdempotencyKey::new(key).unwrap(),
            description.clone(),
            sender.session_id,
            "test",
            "body".into(),
            0,
        )
    };
    let first = send("one").expect("first");
    let second = send("two").expect("second");
    assert!(
        matches!(send("three"), Err(EngineError::Messaging(code)) if code == MESSAGE_INFLIGHT_FULL)
    );
    assert_eq!(send("one").expect("idempotent retry"), first);
    let other = SessionId::new_v7();
    let other_authority = crate::runtime::messaging::producers::ProducerAuthority {
        owner: ProducerOwner::Agent { session_id: other },
        connection_epoch: None,
    };
    let other_producer = fixture
        .engine
        .register_producer(recipient, other_authority.clone())
        .await
        .expect("other producer");
    super::support::settle_session_actor(&fixture.engine, recipient).await;
    fixture
        .engine
        .accept_agent_message_direct_for_test(
            recipient,
            &other_authority,
            other_producer,
            ProducerDeliveryMode::Steer,
            ProducerIdempotencyKey::new("other").unwrap(),
            description,
            other,
            "other",
            "body".into(),
            0,
        )
        .expect("other pair remains available");
    let accepted = fixture
        .engine
        .inner
        .store
        .get(recipient)
        .unwrap()
        .log
        .events()
        .into_iter()
        .filter(|event| matches!(event.payload, EventPayload::ProducerMessageAccepted { .. }))
        .count();
    assert_eq!(accepted, 3);
    assert_ne!(first, second);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn agent_message_hop_guard_rejects_above_limit_and_zero_disables_it() {
    let (mut fixture, selection) = super::support::custom_fixture();
    fixture.config.runtime.messaging.max_hops = 1;
    fixture.engine.shutdown().await;
    fixture.engine =
        super::support::reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    let sender = fixture
        .engine
        .create_session(selection.clone())
        .expect("sender");
    let recipient = tree_recipient(&fixture.engine, sender.session_id);
    let authority = crate::runtime::messaging::producers::ProducerAuthority {
        owner: ProducerOwner::Agent {
            session_id: sender.session_id,
        },
        connection_epoch: None,
    };
    let producer = fixture
        .engine
        .register_producer(recipient, authority.clone())
        .await
        .expect("producer");
    super::support::settle_session_actor(&fixture.engine, recipient).await;
    let description = cookie_agent_protocol::SafeDisplayText::new("test message").unwrap();
    let send = |key: &str, hop| {
        fixture.engine.accept_agent_message_direct_for_test(
            recipient,
            &authority,
            producer,
            ProducerDeliveryMode::Steer,
            ProducerIdempotencyKey::new(key).unwrap(),
            description.clone(),
            sender.session_id,
            "test",
            "body".into(),
            hop,
        )
    };
    let accepted = send("one", 1).expect("boundary hop");
    assert!(
        matches!(send("two", 2), Err(EngineError::Messaging(code)) if code == MESSAGE_MAX_HOPS_EXCEEDED)
    );
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(recipient)
            .unwrap()
            .log
            .events()
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ProducerMessageAccepted { .. }))
            .count(),
        1
    );
    fixture.engine.shutdown().await;
    fixture.config.runtime.messaging.max_hops = 0;
    fixture.engine =
        super::support::reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    let producer = fixture
        .engine
        .register_producer(recipient, authority.clone())
        .await
        .expect("producer");
    super::support::settle_session_actor(&fixture.engine, recipient).await;
    let admitted = fixture
        .engine
        .accept_agent_message_direct_for_test(
            recipient,
            &authority,
            producer,
            ProducerDeliveryMode::Steer,
            ProducerIdempotencyKey::new("large").unwrap(),
            description,
            sender.session_id,
            "test",
            "body".into(),
            99,
        )
        .expect("disabled hop guard");
    assert_ne!(accepted, admitted);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn agent_message_hop_is_inherited_by_the_send_path() {
    let (mut fixture, selection) = super::support::custom_fixture();
    fixture.config.runtime.messaging.max_hops = 1;
    fixture.engine.shutdown().await;
    fixture.engine =
        super::support::reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    let sender = fixture
        .engine
        .create_session(selection.clone())
        .expect("sender");
    // A grandchild: mail to it is a hop, unlike mail to a direct child.
    let child = tree_recipient(&fixture.engine, sender.session_id);
    let recipient = tree_session(&fixture.engine, sender.session_id, child, 2);
    let previous_sender = SessionId::new_v7();
    let previous_authority = crate::runtime::messaging::producers::ProducerAuthority {
        owner: ProducerOwner::Agent {
            session_id: previous_sender,
        },
        connection_epoch: None,
    };
    let previous_producer = fixture
        .engine
        .register_producer(sender.session_id, previous_authority.clone())
        .await
        .expect("previous producer");
    super::support::settle_session_actor(&fixture.engine, sender.session_id).await;
    let observed_run = RunId::new_v7();
    let observed = fixture
        .engine
        .accept_agent_message_direct_for_test(
            sender.session_id,
            &previous_authority,
            previous_producer,
            ProducerDeliveryMode::Queue,
            ProducerIdempotencyKey::new("observed").unwrap(),
            cookie_agent_protocol::SafeDisplayText::new("observed mail").unwrap(),
            previous_sender,
            "test",
            "already-read mail".into(),
            1,
        )
        .expect("observed mail");
    let projection = fixture
        .engine
        .inner
        .store
        .get(sender.session_id)
        .expect("sender projection");
    fixture
        .engine
        .append_direct(
            sender.session_id,
            Some(observed_run),
            EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunStarted {
                client_run_id: ClientRunId::new("observed-run").unwrap(),
                selection,
                agent: Box::new(projection.creation_agent.as_ref().clone()),
                revisions: Some(cookie_agent_protocol::RuntimeRevisions {
                    runtime_revision: projection.meta.runtime_revision.clone(),
                    catalog_revision: projection.meta.catalog_revision.clone(),
                    provider_state_revision: projection.meta.provider_state_revision.clone(),
                    model_revision: projection.meta.model_revision.clone(),
                    agent_revision: projection.meta.agent_revision.clone(),
                    recipe_registry_revision: projection.meta.recipe_registry_revision.clone(),
                    manifest_revision: projection.meta.manifest_revision.clone(),
                }),
                internal_agents: crate::test_support::internal_agent_definitions(),
                input_through_seq: 2,
            },
        )
        .expect("start observing run");
    fixture
        .engine
        .append_direct(
            sender.session_id,
            Some(observed_run),
            EventOrigin::new("engine:test").unwrap(),
            EventPayload::ProducerMessageAdmitted {
                message_id: observed,
            },
        )
        .expect("admit observed mail");

    let mut blocked = invocation(sender.session_id, recipient);
    blocked.sender_run_id = observed_run;
    assert!(
        matches!(fixture.engine.send_agent_message(blocked).await, Err(EngineError::Messaging(code)) if code == MESSAGE_MAX_HOPS_EXCEEDED)
    );
    // Mail to the direct child carries the inherited depth without adding a
    // hop, so the guard never refuses it.
    let mut family = invocation(sender.session_id, child);
    family.sender_run_id = observed_run;
    let family = fixture
        .engine
        .send_agent_message(family)
        .await
        .expect("parent to child mail is not a hop");
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(child)
            .unwrap()
            .log
            .events()
            .iter()
            .any(|event| matches!(
                event.payload,
                EventPayload::ProducerMessageAccepted { message_id, agent_hop: Some(1), .. }
                    if message_id == family.message_id
            ))
    );
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(recipient)
            .unwrap()
            .log
            .events()
            .iter()
            .all(|event| !matches!(event.payload, EventPayload::ProducerMessageAccepted { .. }))
    );

    let fresh = fixture
        .engine
        .send_agent_message(invocation(sender.session_id, recipient))
        .await
        .expect("fresh run starts a chain");
    let accepted = fixture
        .engine
        .inner
        .store
        .get(recipient)
        .unwrap()
        .log
        .events()
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::ProducerMessageAccepted {
                message_id,
                agent_hop,
                ..
            } => Some((message_id, agent_hop)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(accepted, vec![(fresh.message_id, Some(0))]);
    fixture.engine.shutdown().await;
}

#[test]
fn ac10_guard_codes_are_stable_and_distinct() {
    assert_ne!(MESSAGE_MAX_HOPS_EXCEEDED, MESSAGE_INFLIGHT_FULL);
    assert_eq!(MESSAGE_MAX_HOPS_EXCEEDED, "send_message:max_hops_exceeded");
    assert_eq!(MESSAGE_INFLIGHT_FULL, "send_message:inflight_full");
}

#[tokio::test]
async fn ac3_finished_recipient_is_rejected_only_when_not_a_tree_peer() {
    let (fixture, selection) = super::support::custom_fixture();
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
    let (fixture, selection) = super::support::custom_fixture();
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
    let (fixture, selection) = super::support::custom_fixture();
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
    let (fixture, selection) = super::support::custom_fixture();
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
    assert!(
        !crate::base_layer_tool_specs("reviewer")
            .iter()
            .any(|spec| spec.name == "steer_subagent")
    );
}

#[test]
fn ac8_agent_owner_events_are_supported_by_the_protocol_projection() {
    assert_eq!(PermissionAction::MESSAGE.as_str(), "message");
}

#[test]
fn ac9_protocol_version_and_schema_baselines_are_present() {
    let client_baseline = std::fs::read_to_string(format!(
        "{}/../protocol/compat/client/version",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("client baseline version");
    assert_eq!(
        client_baseline.trim(),
        cookie_agent_protocol::CLIENT_PROTOCOL_VERSION.to_string()
    );
    for baseline in [
        "/../protocol/compat/events/schemas/EventPayload.schema.json",
        "/../protocol/compat/plugin/methods.json",
    ] {
        assert!(
            std::path::Path::new(&format!("{}{baseline}", env!("CARGO_MANIFEST_DIR"))).exists()
        );
    }
}

/// Mail from a parent tells the child to answer in its final answer, which
/// reaches the parent as the turn's report; mail the other way names
/// `send_message`, the only way back to the parent.
#[tokio::test]
async fn the_reply_hint_follows_the_direction_of_the_mail() {
    let (fixture, selection) = super::support::custom_fixture();
    let parent = fixture
        .engine
        .create_session(selection)
        .expect("parent")
        .session_id;
    let child = tree_recipient(&fixture.engine, parent);
    let body_of = |session: SessionId| {
        fixture
            .engine
            .inner
            .store
            .get(session)
            .unwrap()
            .log
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::ProducerMessageAccepted { body, .. } => Some(body.clone()),
                _ => None,
            })
            .expect("accepted mail")
    };

    fixture
        .engine
        .send_agent_message(invocation(parent, child))
        .await
        .expect("parent to child");
    let to_child = body_of(child);
    assert!(
        to_child
            .as_plain_text()
            .contains("[reply in your final answer: it reaches"),
        "{to_child}"
    );
    assert!(
        !to_child.as_plain_text().contains("send_message"),
        "{to_child}"
    );

    fixture
        .engine
        .send_agent_message(invocation(child, parent))
        .await
        .expect("child to parent");
    let to_parent = body_of(parent);
    assert!(
        !to_parent.as_plain_text().contains("final answer"),
        "{to_parent}"
    );
    fixture.engine.shutdown().await;
}

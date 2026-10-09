use std::sync::Arc;

use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalId, ApprovalResourceSource, ClientRunId,
    EventPayload, PermissionAction, PermissionEffect, PermissionMode, PermissionRule,
    PermissionRuleSource, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, RunSelection, SessionId, Sha256Digest, ToolCallId, TreeApprovalGrant,
    TreeApprovalGrantId, WildcardPattern,
};

use jiff::Timestamp;

use crate::{Engine, ToolCall, ToolPreparationContext, ToolProvider};

use super::support::*;

#[tokio::test]
async fn permission_query_reports_the_current_session_mode() {
    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    assert_eq!(
        fixture
            .engine
            .get_session_permissions(session.session_id)
            .expect("default permission query")
            .current_mode,
        Some(PermissionMode::AutoApprove)
    );

    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::AutoApproveY)
        .expect("set permission mode");
    assert_eq!(
        fixture
            .engine
            .get_session_permissions(session.session_id)
            .expect("updated permission query")
            .current_mode,
        Some(PermissionMode::AutoApproveY)
    );
    fixture.engine.shutdown().await;
}

fn rule(action: PermissionAction, resource: &str, effect: PermissionEffect) -> PermissionRule {
    PermissionRule {
        action,
        resource: WildcardPattern::new(resource).expect("pattern"),
        effect,
    }
}

#[tokio::test]
async fn preview_matches_the_session_a_create_with_the_same_rules_produces() {
    let (fixture, selection) = custom_fixture();
    let rules = vec![
        rule(PermissionAction::Bash, "git log*", PermissionEffect::Allow),
        rule(PermissionAction::Webfetch, "*", PermissionEffect::Deny),
    ];
    let preview = fixture
        .engine
        .preview_session_permissions(&selection, rules.clone())
        .expect("preview");
    let bash = preview
        .iter()
        .find(|permission| permission.action == PermissionAction::Bash)
        .expect("bash");
    assert!(bash.patterns.iter().any(|pattern| {
        pattern.resource.as_str() == "git log*"
            && pattern.source == PermissionRuleSource::SessionOverlay
    }));
    // Previewing creates nothing.
    assert!(fixture.engine.list_sessions().is_empty());

    let session = fixture
        .engine
        .create_session_with_permissions(selection, Some(PermissionMode::Ask), rules)
        .await
        .expect("session");
    let live = fixture
        .engine
        .get_session_permissions(session.session_id)
        .expect("live permissions");
    assert_eq!(live.permissions, preview);
    assert_eq!(live.current_mode, Some(PermissionMode::Ask));
    // The overlay is durable, committed right after the creation event.
    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("session projection");
    assert_eq!(projection.permission_overlay.rules.len(), 2);
    let events = projection.log.event_snapshot();
    assert!(matches!(
        events[1].payload,
        EventPayload::SessionPermissionOverlaySet { .. }
    ));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn invalid_initial_rules_fail_before_a_session_exists() {
    let (fixture, selection) = custom_fixture();
    let duplicate = vec![
        rule(PermissionAction::Bash, "git*", PermissionEffect::Allow),
        rule(PermissionAction::Bash, "git*", PermissionEffect::Deny),
    ];
    assert!(
        fixture
            .engine
            .preview_session_permissions(&selection, duplicate.clone())
            .is_err()
    );
    assert!(
        fixture
            .engine
            .create_session_with_permissions(selection.clone(), None, duplicate)
            .await
            .is_err()
    );
    assert!(fixture.engine.list_sessions().is_empty());

    // No settings: the plain create, with the default mode.
    let session = fixture
        .engine
        .create_session_with_permissions(selection, None, Vec::new())
        .await
        .expect("session");
    let live = fixture
        .engine
        .get_session_permissions(session.session_id)
        .expect("permissions");
    assert_eq!(live.current_mode, Some(PermissionMode::AutoApprove));
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("projection")
            .log
            .event_snapshot()
            .len(),
        1
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn session_permission_overlay_is_durable_and_evaluated_after_restart() {
    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Bash,
            WildcardPattern::new("*").expect("wildcard"),
            PermissionEffect::Deny,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("set overlay");
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    let projection = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("reloaded session");
    assert_eq!(projection.permission_overlay.rules.len(), 1);
    let view = reopened
        .get_session_permissions(session.session_id)
        .expect("effective permissions");
    let bash = view
        .permissions
        .iter()
        .find(|permission| permission.action == PermissionAction::Bash)
        .expect("bash permission");
    assert_eq!(bash.effect, PermissionEffect::Deny);
    assert_eq!(bash.source, PermissionRuleSource::SessionOverlay);

    let resource = PreparedApprovalResource {
        capability: PermissionAction::Bash,
        canonical: PreparedResourceIdentity::new("command:git-status").expect("identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
        binding_lifetime: PreparedBindingLifetime::RestartStable,
        boundary: ApprovalBoundary::CommandPrefix {
            prefix: "git status".into(),
        },
        source: ApprovalResourceSource::PrimaryOperation,
    };
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"args"),
        vec![ApprovalCapability {
            action: PermissionAction::Bash,
            operation: PreparedCapabilityOperation::new("bash:execute").expect("operation"),
        }],
        vec![resource],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("prepared operation");
    let decision = crate::permissions::PermissionPipeline::default().decide_operation_with_overlay(
        &projection.creation_agent,
        Some(&projection.permission_overlay),
        &operation,
        &[Some("git status".into())],
        reopened.inner.store.cwd(),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    reopened.shutdown().await;
}

#[tokio::test]
async fn tightening_overlay_invalidates_tree_grants_durably() {
    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Bash,
            WildcardPattern::new("*").unwrap(),
            PermissionEffect::Ask,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("explicit ask before tightening");
    let resource = PreparedApprovalResource {
        capability: PermissionAction::Bash,
        canonical: PreparedResourceIdentity::new("command:git-status").expect("identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
        binding_lifetime: PreparedBindingLifetime::RestartStable,
        boundary: ApprovalBoundary::CommandPrefix {
            prefix: "git status".into(),
        },
        source: ApprovalResourceSource::PrimaryOperation,
    };
    let capabilities = vec![ApprovalCapability {
        action: PermissionAction::Bash,
        operation: PreparedCapabilityOperation::new("bash:execute").expect("operation"),
    }];
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"args"),
        capabilities.clone(),
        vec![resource.clone()],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("prepared operation");
    let grant_id = TreeApprovalGrantId::new_v7();
    fixture
        .engine
        .inner
        .approvals
        .store
        .grant(TreeApprovalGrant {
            grant_id,
            root_session_id: session.session_id,
            approval_id: ApprovalId::new_v7(),
            operation_fingerprint:
                cookie_agent_protocol::OperationFingerprint::from_prepared_operation(&operation),
            capabilities,
            resources: vec![resource],
            created_at: Timestamp::now(),
        });
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Bash,
            WildcardPattern::new("*").expect("wildcard"),
            PermissionEffect::Deny,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("tighten overlay");
    assert!(
        fixture
            .engine
            .inner
            .approvals
            .store
            .for_root(session.session_id)
            .is_empty()
    );
    assert!(
        fixture
            .engine
            .inner
            .grant_journals
            .for_root(session.session_id)
            .expect("tree grant journal")
            .invalidated_ids()
            .contains(&grant_id)
    );
    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    assert!(
        reopened
            .inner
            .grant_journals
            .for_root(session.session_id)
            .expect("tree grant journal")
            .invalidated_ids()
            .contains(&grant_id)
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn clearing_allow_overlay_to_default_deny_invalidates_tree_grants() {
    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    let wildcard = WildcardPattern::new("*").expect("wildcard");
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Bash,
            wildcard.clone(),
            PermissionEffect::Allow,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("allow overlay");
    let resource = PreparedApprovalResource {
        capability: PermissionAction::Bash,
        canonical: PreparedResourceIdentity::new("command:git-status").expect("identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
        binding_lifetime: PreparedBindingLifetime::RestartStable,
        boundary: ApprovalBoundary::CommandPrefix {
            prefix: "git status".into(),
        },
        source: ApprovalResourceSource::PrimaryOperation,
    };
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"args"),
        vec![ApprovalCapability {
            action: PermissionAction::Bash,
            operation: PreparedCapabilityOperation::new("bash:execute").expect("operation"),
        }],
        vec![resource.clone()],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("prepared operation");
    let grant_id = TreeApprovalGrantId::new_v7();
    fixture
        .engine
        .inner
        .approvals
        .store
        .grant(TreeApprovalGrant {
            grant_id,
            root_session_id: session.session_id,
            approval_id: ApprovalId::new_v7(),
            operation_fingerprint:
                cookie_agent_protocol::OperationFingerprint::from_prepared_operation(&operation),
            capabilities: operation.capabilities().to_vec(),
            resources: vec![resource],
            created_at: Timestamp::now(),
        });

    fixture
        .engine
        .clear_session_permission(
            session.session_id,
            PermissionAction::Bash,
            &wildcard,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("clear allow overlay");

    assert!(
        fixture
            .engine
            .inner
            .approvals
            .store
            .for_root(session.session_id)
            .is_empty()
    );
    assert!(
        fixture
            .engine
            .inner
            .grant_journals
            .for_root(session.session_id)
            .expect("tree grant journal")
            .invalidated_ids()
            .contains(&grant_id)
    );
    let bash = fixture
        .engine
        .get_session_permissions(session.session_id)
        .expect("permission view")
        .permissions
        .into_iter()
        .find(|permission| permission.action == PermissionAction::Bash)
        .expect("bash permission");
    assert_eq!(bash.effect, PermissionEffect::Deny);
    assert_eq!(bash.source, PermissionRuleSource::Default);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn child_log_tree_grants_arrive_with_the_lazy_tree_load() {
    use cookie_agent_protocol::{
        ApprovalConstraints, ApprovalEvaluation, ApprovalRequest, ApprovalTrigger, DecisionTrace,
    };

    let (fixture, selection) = custom_fixture();
    let origin = cookie_agent_protocol::EventOrigin::new("client:test").expect("event origin");
    let root = fixture
        .engine
        .create_session(selection.clone())
        .expect("root session");
    let child = create_buffered_delegated_child(&fixture.engine, root.session_id);

    // A durable log needs a started run and a first user message (§2.1).
    let publish =
        |engine: &Engine, id, run: cookie_agent_protocol::RunId, selection: &RunSelection| {
            let projection = engine.inner.store.get(id).expect("projection");
            engine
                .inner
                .store
                .append(
                    id,
                    Some(run),
                    origin.clone(),
                    EventPayload::RunStarted {
                        client_run_id: ClientRunId::new("grant-setup-run").expect("client run ID"),
                        selection: selection.clone(),
                        agent: Box::new(projection.creation_agent.as_ref().clone()),
                        runtime_revision: projection.meta.runtime_revision.clone(),
                        catalog_revision: projection.meta.catalog_revision.clone(),
                        provider_state_revision: projection.meta.provider_state_revision.clone(),
                        model_revision: projection.meta.model_revision.clone(),
                        agent_revision: projection.meta.agent_revision.clone(),
                        recipe_registry_revision: projection.meta.recipe_registry_revision.clone(),
                        manifest_revision: projection.meta.manifest_revision.clone(),
                        selected_suffix: projection.creation_agent.fallback_chain.clone(),
                        internal_agents: Vec::new(),
                        input_through_seq: 1,
                    },
                )
                .expect("start the run");
            engine
                .inner
                .store
                .append(
                    id,
                    Some(run),
                    origin.clone(),
                    EventPayload::UserInputSubmitted {
                        input: "persist the log".into(),
                    },
                )
                .expect("publish the session");
            run
        };
    let _root_run = publish(
        &fixture.engine,
        root.session_id,
        cookie_agent_protocol::RunId::new_v7(),
        &selection,
    );
    let child_run = publish(
        &fixture.engine,
        child,
        cookie_agent_protocol::RunId::new_v7(),
        &selection,
    );

    let resource = |lifetime| PreparedApprovalResource {
        capability: PermissionAction::Bash,
        canonical: PreparedResourceIdentity::new("command:git-status").expect("identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
        binding_lifetime: lifetime,
        boundary: ApprovalBoundary::CommandPrefix {
            prefix: "git status".into(),
        },
        source: ApprovalResourceSource::PrimaryOperation,
    };
    let capabilities = vec![ApprovalCapability {
        action: PermissionAction::Bash,
        operation: PreparedCapabilityOperation::new("bash:execute").expect("operation"),
    }];
    let commit_grant = |engine: &Engine, grant_id| {
        let resources = vec![resource(PreparedBindingLifetime::RestartStable)];
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"args"),
            capabilities.clone(),
            resources.clone(),
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("prepared operation");
        let approval_id = ApprovalId::new_v7();
        engine
            .inner
            .store
            .append(
                child,
                Some(child_run),
                origin.clone(),
                EventPayload::ApprovalRequested {
                    request: ApprovalRequest::new(
                        approval_id,
                        1,
                        ApprovalTrigger::PermissionPolicy,
                        operation.clone(),
                        vec![ApprovalEvaluation {
                            resource_digest: resources[0].binding_digest.clone(),
                            effect: PermissionEffect::Ask,
                            trace: DecisionTrace {
                                action: PermissionAction::Bash,
                                normalized_resource: "git status".into(),
                                candidates: Vec::new(),
                                effect: PermissionEffect::Ask,
                                precedence_reason: "test escalation".into(),
                            },
                        }],
                        ApprovalConstraints {
                            allow_once: true,
                            allow_tree_grant: true,
                            cancellable: true,
                            expires_at: None,
                        },
                    )
                    .expect("approval request"),
                },
            )
            .expect("request approval in the child log");
        engine
            .inner
            .store
            .append(
                child,
                Some(child_run),
                origin.clone(),
                EventPayload::TreeApprovalGrantCommitted {
                    grant: TreeApprovalGrant {
                        grant_id,
                        root_session_id: root.session_id,
                        approval_id,
                        operation_fingerprint:
                            cookie_agent_protocol::OperationFingerprint::from_prepared_operation(
                                &operation,
                            ),
                        capabilities: capabilities.clone(),
                        resources,
                        created_at: Timestamp::now(),
                    },
                },
            )
            .expect("commit a tree grant into the child log");
    };
    let grant_id = TreeApprovalGrantId::new_v7();
    commit_grant(&fixture.engine, grant_id);

    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    assert!(
        !reopened.inner.store.is_tree_loaded(root.session_id),
        "startup must not read child logs"
    );
    assert!(
        reopened
            .inner
            .approvals
            .store
            .for_root(root.session_id)
            .is_empty(),
        "a grant living in a child log cannot be restored at startup"
    );

    let _ = reopened.children(root.session_id).expect("children");
    let granted = reopened.inner.approvals.store.for_root(root.session_id);
    assert_eq!(
        granted
            .iter()
            .map(|grant| grant.grant_id)
            .collect::<Vec<_>>(),
        vec![grant_id],
        "the tree load restores the grant committed in the child log"
    );
    reopened.shutdown().await;

    // A second pass over the same tree must not duplicate the grant.
    let reopened = reopen_engine(&fixture);
    let _ = reopened.children(root.session_id).expect("children");
    let _ = reopened.tree(root.session_id).expect("root tree");
    assert_eq!(
        reopened
            .inner
            .approvals
            .store
            .for_root(root.session_id)
            .iter()
            .filter(|grant| grant.grant_id == grant_id)
            .count(),
        1
    );
    reopened.shutdown().await;
}

#[test]
fn test_write_provider_exposes_permission_resources() {
    let write = TestWriteProvider {
        executed: Arc::new(TestFlag::default()),
    };
    assert_eq!(
        write
            .get_permission_resource("write", &serde_json::json!({}))
            .expect("write resource"),
        ("write", Some("approval-test.txt".into()))
    );
    assert_eq!(
        write
            .get_display_argument("write", &serde_json::json!({}))
            .expect("write display"),
        "approval-test.txt"
    );
}

#[tokio::test]
async fn permission_labels_come_from_prepared_permission_resource() {
    let provider = DivergentReadProvider {
        raw_resource: Some("canonical/src/lib.rs".into()),
    };
    let raw = serde_json::json!({"filePath":"src/lib.rs"});
    assert_eq!(
        provider
            .get_permission_resource("read", &raw)
            .expect("raw permission resource"),
        ("read", Some("canonical/src/lib.rs".into()))
    );
    let prepared = provider
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: cookie_agent_protocol::RunId::new_v7(),
                cwd: "/tmp".into(),
                workspace_root: "/tmp".into(),
                turn_context: test_turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: raw,
            },
        )
        .await
        .expect("prepare");
    assert_eq!(prepared.policy_labels(), [Some("divergent-raw".into())]);
    let labeled = crate::runtime::tool_execution::apply_permission_resource(
        &provider, "read", "read", prepared,
    )
    .expect("overwrite");
    assert_eq!(
        provider
            .get_permission_resource("read", labeled.normalized_arguments())
            .expect("prepared permission resource"),
        ("read", Some("canonical/src/lib.rs".into()))
    );
    assert_eq!(
        labeled.policy_labels(),
        [Some("canonical/src/lib.rs".into())]
    );

    let provider = DivergentReadProvider { raw_resource: None };
    let prepared = provider
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: cookie_agent_protocol::RunId::new_v7(),
                cwd: "/tmp".into(),
                workspace_root: "/tmp".into(),
                turn_context: test_turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: serde_json::json!({"filePath":"src/lib.rs"}),
            },
        )
        .await
        .expect("prepare without resource");
    let labeled = crate::runtime::tool_execution::apply_permission_resource(
        &provider, "read", "read", prepared,
    )
    .expect("preserve labels");
    assert_eq!(labeled.policy_labels(), [Some("divergent-raw".into())]);
}

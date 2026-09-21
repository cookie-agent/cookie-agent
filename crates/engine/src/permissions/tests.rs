use cookie_agent_protocol::{
    AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot, ApprovalBoundary,
    ApprovalCapability, ApprovalResourceSource, PermissionAction, PermissionEffect, PermissionRule,
    PermissionRuleSource, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, SafeCode, SessionPermissionOverlay, Sha256Digest, WildcardPattern,
};

use super::{PermissionPipeline, select_governing_agent, tool_visible};

fn policy(rules: Vec<PermissionRule>) -> AgentSnapshot {
    AgentSnapshot {
        agent: AgentId::new("test").expect("agent id"),
        schema: AgentSchemaVersion::current(),
        mode: AgentMode::Primary,
        description: "Test agent".into(),
        document_source: AgentDocumentSource::Workspace,
        document_fingerprint: Sha256Digest::of_bytes(b"test document"),
        composed_prompt: "Test permission evaluation.\n".into(),
        prompt_fingerprint: Sha256Digest::of_bytes(b"Test permission evaluation.\n"),
        max_output_tokens: 0,
        permissions: rules,
        delegation: None,
        fallback_chain: Vec::new(),
        selected_suffix_start: 0,
    }
}

#[test]
fn effective_permission_view_reports_message_with_default_deny() {
    let view =
        super::effective_permission_view(&policy(Vec::new()), &SessionPermissionOverlay::empty());
    let message = view
        .iter()
        .find(|entry| entry.action == PermissionAction::Message)
        .expect("message action is reported");
    assert_eq!(message.effect, PermissionEffect::Deny);
    assert_eq!(message.source, PermissionRuleSource::Default);
    assert!(message.patterns.is_empty());
}

#[test]
fn message_permission_name_maps_and_unknown_names_still_fail() {
    assert_eq!(
        PermissionPipeline::action_for_permission_name("message").expect("message action"),
        PermissionAction::Message
    );
    assert!(PermissionPipeline::action_for_permission_name("messaging").is_err());
    assert!(PermissionPipeline::action_for_permission_name("send_message").is_err());
}

#[test]
fn message_tool_visibility_requires_an_allow_or_ask_rule() {
    assert!(!tool_visible(&[], None, PermissionAction::Message));
    let only_deny = [rule(
        "deny-all",
        PermissionAction::Message,
        "*",
        PermissionEffect::Deny,
    )];
    assert!(!tool_visible(&only_deny, None, PermissionAction::Message));
    let allows = [rule(
        "allow-child",
        PermissionAction::Message,
        "child",
        PermissionEffect::Allow,
    )];
    assert!(tool_visible(&allows, None, PermissionAction::Message));
    let asks = [rule(
        "ask-sibling",
        PermissionAction::Message,
        "sibling",
        PermissionEffect::Ask,
    )];
    assert!(tool_visible(&asks, None, PermissionAction::Message));
    // Delegate rules never make the message tool visible: the actions are
    // independent by decision.
    let delegate_only = [rule(
        "allow-delegate",
        PermissionAction::Delegate,
        "*",
        PermissionEffect::Allow,
    )];
    assert!(!tool_visible(
        &delegate_only,
        None,
        PermissionAction::Message
    ));
}

#[test]
fn message_relationship_labels_match_literal_resources_with_wildcard_catch_all() {
    let rules = policy(vec![
        rule(
            "allow-child",
            PermissionAction::Message,
            "child",
            PermissionEffect::Allow,
        ),
        rule(
            "ask-sibling",
            PermissionAction::Message,
            "sibling",
            PermissionEffect::Ask,
        ),
        rule(
            "deny-rest",
            PermissionAction::Message,
            "*",
            PermissionEffect::Deny,
        ),
    ]);
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::Message, "child", b"child")
        )
        .effect,
        PermissionEffect::Allow
    );
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::Message, "parent", b"parent")
        )
        .effect,
        PermissionEffect::Deny
    );
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::Message, "sibling", b"sibling")
        )
        .effect,
        PermissionEffect::Ask
    );
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::Message, "grandparent", b"grandparent")
        )
        .effect,
        PermissionEffect::Deny
    );
    // Without any rule the default deny applies to every label.
    assert_eq!(
        decide(
            &policy(Vec::new()),
            resource(PermissionAction::Message, "child", b"child")
        )
        .effect,
        PermissionEffect::Deny
    );
}

#[test]
fn later_run_agent_governs_permission_mutation_classification() {
    let creation = policy(vec![rule(
        "creation",
        PermissionAction::Read,
        "*",
        PermissionEffect::Allow,
    )]);
    let later = policy(vec![rule(
        "later",
        PermissionAction::Read,
        "*",
        PermissionEffect::Deny,
    )]);

    assert_eq!(
        select_governing_agent(&creation, Some(&later)).permissions,
        later.permissions
    );
    assert_eq!(
        select_governing_agent(&creation, None).permissions,
        creation.permissions
    );
}

#[test]
fn artifact_uris_are_not_workspace_files_for_permission_patterns() {
    let workspace = std::path::Path::new("/workspace");
    let uri = format!("artifact://{}/results", "a".repeat(64));
    let files = policy(vec![rule(
        "files",
        PermissionAction::Read,
        "${workspace_dir}/*",
        PermissionEffect::Allow,
    )]);
    assert_eq!(super::absolute_resource(workspace, &uri), uri);
    assert_eq!(
        super::effective_permission(&files, PermissionAction::Read, &uri, workspace).0,
        PermissionEffect::Deny
    );
    assert_eq!(
        super::effective_permission(&files, PermissionAction::Read, "src/main.rs", workspace).0,
        PermissionEffect::Allow
    );
    let artifacts = policy(vec![rule(
        "artifacts",
        PermissionAction::Read,
        "artifact://*",
        PermissionEffect::Allow,
    )]);
    assert_eq!(
        super::effective_permission(&artifacts, PermissionAction::Read, &uri, workspace).0,
        PermissionEffect::Allow
    );
    assert_eq!(
        super::effective_permission(&artifacts, PermissionAction::Read, "src/main.rs", workspace).0,
        PermissionEffect::Deny
    );
}

#[test]
fn skill_grants_allow_ask_but_never_override_deny() {
    let grants = SessionPermissionOverlay {
        rules: vec![rule(
            "skill-grant",
            PermissionAction::Bash,
            "git status",
            PermissionEffect::Allow,
        )],
    };
    let operation = operation(vec![resource(
        PermissionAction::Bash,
        "git status",
        b"git status",
    )]);
    let labels = [Some("git status".into())];
    let pipeline = PermissionPipeline::default();
    let ask = pipeline.decide_operation_with_grants(
        &policy(vec![rule(
            "ask",
            PermissionAction::Bash,
            "*",
            PermissionEffect::Ask,
        )]),
        None,
        Some(&grants),
        &operation,
        &labels,
        std::path::Path::new("/workspace"),
    );
    assert_eq!(ask.effect, PermissionEffect::Allow);

    let denied = pipeline.decide_operation_with_grants(
        &policy(vec![rule(
            "deny",
            PermissionAction::Bash,
            "*",
            PermissionEffect::Deny,
        )]),
        None,
        Some(&grants),
        &operation,
        &labels,
        std::path::Path::new("/workspace"),
    );
    assert_eq!(denied.effect, PermissionEffect::Deny);
}

#[test]
fn turn_scoped_skill_grant_requires_explicit_ask() {
    let base_policy = policy(vec![rule(
        "ask",
        PermissionAction::Bash,
        "git*",
        PermissionEffect::Ask,
    )]);
    let grants = SessionPermissionOverlay {
        rules: vec![
            rule(
                "skill-grant-exact",
                PermissionAction::Bash,
                "git",
                PermissionEffect::Allow,
            ),
            rule(
                "skill-grant-prefix",
                PermissionAction::Bash,
                "git *",
                PermissionEffect::Allow,
            ),
        ],
    };
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        None,
        "bash",
        std::path::Path::new("/workspace"),
    ));
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        Some(&grants),
        "bash",
        std::path::Path::new("/workspace"),
    ));
    for command in ["git", "git status", "git commit -m x"] {
        let operation = operation(vec![resource(
            PermissionAction::Bash,
            command,
            command.as_bytes(),
        )]);
        let decision = PermissionPipeline::default().decide_operation_with_grants(
            &base_policy,
            None,
            Some(&grants),
            &operation,
            &[Some(command.into())],
            std::path::Path::new("/workspace"),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow, "{command}");
    }
    let ungranted = PermissionPipeline::default().decide_operation_with_grants(
        &base_policy,
        None,
        Some(&grants),
        &operation(vec![resource(
            PermissionAction::Bash,
            "cargo test",
            b"cargo test",
        )]),
        &[Some("cargo test".into())],
        std::path::Path::new("/workspace"),
    );
    assert_eq!(ungranted.effect, PermissionEffect::Deny);
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        None,
        "bash",
        std::path::Path::new("/workspace"),
    ));

    let denied = policy(vec![rule(
        "deny",
        PermissionAction::Bash,
        "*",
        PermissionEffect::Deny,
    )]);
    assert!(!PermissionPipeline::tool_visible_with_grants(
        &denied,
        None,
        Some(&grants),
        "bash",
        std::path::Path::new("/workspace"),
    ));
}

#[test]
fn skill_grant_can_publish_another_skill_action() {
    let base_policy = policy(vec![rule(
        "ask",
        PermissionAction::Skill,
        "other-skill",
        PermissionEffect::Ask,
    )]);
    let grants = SessionPermissionOverlay {
        rules: vec![rule(
            "skill-chain",
            PermissionAction::Skill,
            "other-skill",
            PermissionEffect::Allow,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        None,
        "skill",
        std::path::Path::new("/workspace"),
    ));
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        Some(&grants),
        "skill",
        std::path::Path::new("/workspace"),
    ));
    let operation = operation(vec![resource(
        PermissionAction::Skill,
        "other-skill",
        b"other-skill",
    )]);
    let decision = PermissionPipeline::default().decide_operation_with_grants(
        &base_policy,
        None,
        Some(&grants),
        &operation,
        &[Some("other-skill".into())],
        std::path::Path::new("/workspace"),
    );
    assert_eq!(decision.effect, PermissionEffect::Allow);
}

fn rule(
    _id: &str,
    action: PermissionAction,
    resource: &str,
    effect: PermissionEffect,
) -> PermissionRule {
    PermissionRule {
        action,
        resource: WildcardPattern::new(resource).expect("wildcard"),
        effect,
    }
}

fn resource(action: PermissionAction, label: &str, binding: &[u8]) -> PreparedApprovalResource {
    PreparedApprovalResource {
        capability: action,
        canonical: PreparedResourceIdentity::new(format!(
            "label:{}",
            Sha256Digest::of_bytes(label.as_bytes()).as_str()
        ))
        .expect("identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(binding),
        binding_lifetime: PreparedBindingLifetime::ProcessLocal,
        boundary: ApprovalBoundary::CommandPrefix {
            prefix: label.into(),
        },
        source: ApprovalResourceSource::PrimaryOperation,
    }
}

fn operation(resources: Vec<PreparedApprovalResource>) -> PreparedOperationIdentity {
    let action = resources.first().expect("test resources").capability;
    let capabilities = vec![ApprovalCapability {
        action,
        operation: PreparedCapabilityOperation::new("permission:test").expect("operation"),
    }];
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"args"),
        capabilities,
        resources,
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("operation")
}

fn decide(policy: &AgentSnapshot, resource: PreparedApprovalResource) -> super::PermissionDecision {
    decide_many(policy, vec![resource])
}

fn decide_many(
    policy: &AgentSnapshot,
    resources: Vec<PreparedApprovalResource>,
) -> super::PermissionDecision {
    let labels = resources
        .iter()
        .map(|resource| match &resource.boundary {
            ApprovalBoundary::CommandPrefix { prefix } => Some(prefix.clone()),
            _ => unreachable!("test resources carry explicit labels"),
        })
        .collect::<Vec<_>>();
    PermissionPipeline::default().decide_operation(
        policy,
        &operation(resources),
        &labels,
        std::path::Path::new("/workspace"),
    )
}

fn decide_loose(policy: &AgentSnapshot, action: PermissionAction) -> super::PermissionDecision {
    let prepared_resource = resource(action, "unscoped-test-identity", b"permission-name-only");
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"loose args"),
        vec![ApprovalCapability {
            action,
            operation: PreparedCapabilityOperation::new("permission:loose")
                .expect("loose operation"),
        }],
        vec![prepared_resource],
        Sha256Digest::of_bytes(b"loose context"),
    )
    .expect("loose prepared operation");
    PermissionPipeline::default().decide_operation(
        policy,
        &operation,
        &[None],
        std::path::Path::new("/workspace"),
    )
}

#[test]
fn loose_permission_uses_only_bare_or_wildcard_effect() {
    let bare_allow = cookie_agent_config::PermissionValue::Effect(PermissionEffect::Allow)
        .rules(PermissionAction::Delegate);
    assert_eq!(
        decide_loose(&policy(bare_allow), PermissionAction::Delegate).effect,
        PermissionEffect::Allow
    );
    for effect in [
        PermissionEffect::Allow,
        PermissionEffect::Ask,
        PermissionEffect::Deny,
    ] {
        let decision = decide_loose(
            &policy(vec![rule(
                "wildcard",
                PermissionAction::Delegate,
                "*",
                effect,
            )]),
            PermissionAction::Delegate,
        );
        assert_eq!(decision.effect, effect);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 1);
    }
    let specific_only = decide_loose(
        &policy(vec![rule(
            "specific",
            PermissionAction::Delegate,
            "reviewer",
            PermissionEffect::Allow,
        )]),
        PermissionAction::Delegate,
    );
    assert_eq!(specific_only.effect, PermissionEffect::Deny);
    assert!(specific_only.evaluations[0].trace.candidates.is_empty());
}

#[test]
fn former_loose_marker_literal_remains_a_scoped_resource() {
    let literal = "<permission-name-only>";
    let decision = decide(
        &policy(vec![
            rule(
                "literal-deny",
                PermissionAction::Bash,
                literal,
                PermissionEffect::Deny,
            ),
            rule(
                "fallback-allow",
                PermissionAction::Bash,
                "*",
                PermissionEffect::Allow,
            ),
        ]),
        resource(PermissionAction::Bash, literal, literal.as_bytes()),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    assert_eq!(decision.evaluations[0].trace.normalized_resource, literal);
}

#[test]
fn delegate_spawn_matches_agent_pattern_while_session_tools_ignore_it() {
    let delegate_policy = policy(vec![
        rule(
            "reviewer",
            PermissionAction::Delegate,
            "reviewer",
            PermissionEffect::Allow,
        ),
        rule(
            "fallback",
            PermissionAction::Delegate,
            "*",
            PermissionEffect::Deny,
        ),
    ]);
    let spawn = decide(
        &delegate_policy,
        resource(PermissionAction::Delegate, "reviewer", b"reviewer"),
    );
    assert_eq!(spawn.effect, PermissionEffect::Allow);
    let session_tool = decide_loose(&delegate_policy, PermissionAction::Delegate);
    assert_eq!(session_tool.effect, PermissionEffect::Deny);

    let specific_deny_with_wildcard_allow = decide_loose(
        &policy(vec![
            rule(
                "reviewer",
                PermissionAction::Delegate,
                "reviewer",
                PermissionEffect::Deny,
            ),
            rule(
                "fallback",
                PermissionAction::Delegate,
                "*",
                PermissionEffect::Allow,
            ),
        ]),
        PermissionAction::Delegate,
    );
    assert_eq!(
        specific_deny_with_wildcard_allow.effect,
        PermissionEffect::Allow
    );
}

#[test]
fn mcp_permissions_are_scoped_by_generated_tool_name() {
    let mcp_policy = policy(vec![
        rule(
            "server-allow",
            PermissionAction::Mcp,
            "github_*",
            PermissionEffect::Allow,
        ),
        rule(
            "tool-deny",
            PermissionAction::Mcp,
            "github_delete_repo",
            PermissionEffect::Deny,
        ),
    ]);
    assert_eq!(
        decide(
            &mcp_policy,
            resource(PermissionAction::Mcp, "github_search", b"search")
        )
        .effect,
        PermissionEffect::Allow
    );
    assert_eq!(
        decide(
            &mcp_policy,
            resource(PermissionAction::Mcp, "github_delete_repo", b"delete")
        )
        .effect,
        PermissionEffect::Deny
    );
    assert_eq!(
        decide(
            &mcp_policy,
            resource(PermissionAction::Mcp, "slack_search", b"unmatched")
        )
        .effect,
        PermissionEffect::Deny
    );
    assert_eq!(
        PermissionPipeline::action_for_permission_name("mcp").expect("MCP action"),
        PermissionAction::Mcp
    );
    assert!(PermissionPipeline::tool_visible(&mcp_policy, "mcp"));
    assert!(!PermissionPipeline::tool_visible(
        &policy(Vec::new()),
        "mcp"
    ));
    assert!(!PermissionPipeline::tool_visible(
        &policy(vec![rule(
            "deny-all",
            PermissionAction::Mcp,
            "*",
            PermissionEffect::Deny,
        )]),
        "mcp"
    ));
    assert!(PermissionPipeline::tool_visible(
        &policy(vec![
            rule(
                "deny-all",
                PermissionAction::Mcp,
                "*",
                PermissionEffect::Deny,
            ),
            rule(
                "allow-github",
                PermissionAction::Mcp,
                "github_*",
                PermissionEffect::Allow,
            ),
        ]),
        "mcp"
    ));
}

#[test]
fn session_overlay_precedes_more_specific_agent_rules() {
    let policy = policy(vec![rule(
        "agent-specific",
        PermissionAction::Bash,
        "git status",
        PermissionEffect::Allow,
    )]);
    let overlay = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-wildcard",
            PermissionAction::Bash,
            "*",
            PermissionEffect::Deny,
        )],
    };
    let decision = PermissionPipeline::default().decide_operation_with_overlay(
        &policy,
        Some(&overlay),
        &operation(vec![resource(
            PermissionAction::Bash,
            "git status",
            b"git status",
        )]),
        &[Some("git status".into())],
        std::path::Path::new("/workspace"),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    assert_eq!(
        decision.evaluations[0]
            .trace
            .candidates
            .last()
            .expect("overlay candidate")
            .source_layer
            .as_str(),
        "session_overlay"
    );
}

#[test]
fn exact_rule_matches_normalized_label_not_opaque_identity() {
    let decision = decide(
        &policy(vec![rule(
            "exact",
            PermissionAction::Read,
            "/workspace/a.txt",
            PermissionEffect::Allow,
        )]),
        resource(PermissionAction::Read, "/workspace/a.txt", b"file"),
    );
    assert_eq!(decision.effect, PermissionEffect::Allow);
    assert_eq!(
        decision.evaluations[0].trace.normalized_resource,
        "/workspace/a.txt"
    );
}

#[test]
fn more_literals_win_before_wildcard_count() {
    let decision = decide(
        &policy(vec![
            rule(
                "allow",
                PermissionAction::Read,
                "/workspace/*",
                PermissionEffect::Allow,
            ),
            rule(
                "deny",
                PermissionAction::Read,
                "*/secret.txt",
                PermissionEffect::Deny,
            ),
        ]),
        resource(PermissionAction::Read, "/workspace/secret.txt", b"secret"),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
}

#[test]
fn universal_catch_all_is_least_specific() {
    let decision = decide(
        &policy(vec![
            rule(
                "allow",
                PermissionAction::Read,
                "*",
                PermissionEffect::Allow,
            ),
            rule(
                "deny",
                PermissionAction::Read,
                "*/.env.*",
                PermissionEffect::Deny,
            ),
        ]),
        resource(PermissionAction::Read, "nested/.env.local", b"secret"),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
}

#[test]
fn later_declaration_wins_final_specificity_tie() {
    let decision = decide(
        &policy(vec![
            rule(
                "earlier-deny",
                PermissionAction::Read,
                "/workspace/*",
                PermissionEffect::Deny,
            ),
            rule(
                "later-allow",
                PermissionAction::Read,
                "/workspace/*",
                PermissionEffect::Allow,
            ),
        ]),
        resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
    );
    assert_eq!(decision.effect, PermissionEffect::Allow);
    assert_eq!(decision.evaluations[0].trace.candidates.len(), 2);
    assert_eq!(
        decision.evaluations[0].trace.candidates[1].source_layer,
        SafeCode::new("agent_document").expect("safe code")
    );
}

#[test]
fn subagent_operation_matches_owned_child_agent_type_in_delegate_map() {
    let resource = resource(PermissionAction::Delegate, "reviewer", b"reviewer");
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"delegate args"),
        vec![ApprovalCapability {
            action: PermissionAction::Delegate,
            operation: PreparedCapabilityOperation::new("delegate_subagent:spawn")
                .expect("delegate operation"),
        }],
        vec![resource],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("prepared subagent operation");
    let allowed = PermissionPipeline::default().decide_operation(
        &policy(vec![rule(
            "allow-reviewer",
            PermissionAction::Delegate,
            "reviewer",
            PermissionEffect::Allow,
        )]),
        &operation,
        &[Some("reviewer".into())],
        std::path::Path::new("/workspace"),
    );
    assert_eq!(
        PermissionPipeline::action_for_permission_name("delegate").expect("delegate action"),
        PermissionAction::Delegate
    );
    assert_eq!(allowed.effect, PermissionEffect::Allow);

    let denied = PermissionPipeline::default().decide_operation(
        &policy(vec![rule(
            "allow-explorer",
            PermissionAction::Delegate,
            "explorer",
            PermissionEffect::Allow,
        )]),
        &operation,
        &[Some("reviewer".into())],
        std::path::Path::new("/workspace"),
    );
    assert_eq!(denied.effect, PermissionEffect::Deny);
}

#[test]
fn wildcard_allow_applies_to_non_secret_workspace_path() {
    let decision = decide(
        &policy(vec![rule(
            "allow",
            PermissionAction::Read,
            "/workspace/*",
            PermissionEffect::Allow,
        )]),
        resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
    );
    assert_eq!(decision.effect, PermissionEffect::Allow);
}

#[test]
fn multi_resource_deny_wins_over_allow() {
    let decision = decide_many(
        &policy(vec![
            rule(
                "allow-public",
                PermissionAction::Read,
                "/workspace/public.txt",
                PermissionEffect::Allow,
            ),
            rule(
                "deny-secret",
                PermissionAction::Read,
                "/workspace/secret.txt",
                PermissionEffect::Deny,
            ),
        ]),
        vec![
            resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
            resource(PermissionAction::Read, "/workspace/secret.txt", b"secret"),
        ],
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    assert_eq!(decision.evaluations.len(), 2);
}

#[test]
fn multi_resource_ask_beats_allow() {
    let decision = decide_many(
        &policy(vec![
            rule(
                "ask-review",
                PermissionAction::Read,
                "/workspace/review.txt",
                PermissionEffect::Ask,
            ),
            rule(
                "allow-public",
                PermissionAction::Read,
                "/workspace/public.txt",
                PermissionEffect::Allow,
            ),
        ]),
        vec![
            resource(PermissionAction::Read, "/workspace/public.txt", b"public"),
            resource(PermissionAction::Read, "/workspace/review.txt", b"review"),
        ],
    );
    assert_eq!(decision.effect, PermissionEffect::Ask);
    assert_eq!(decision.evaluations.len(), 2);
}

#[test]
fn absolute_rule_controls_outside_read() {
    let decision = decide(
        &policy(vec![
            rule(
                "read-all",
                PermissionAction::Read,
                "*",
                PermissionEffect::Allow,
            ),
            rule(
                "outside-etc",
                PermissionAction::Read,
                "/etc/*",
                PermissionEffect::Ask,
            ),
        ]),
        resource(PermissionAction::Read, "/etc/passwd", b"passwd"),
    );
    assert_eq!(decision.effect, PermissionEffect::Ask);
}

#[test]
fn absolute_deny_pattern_catches_outside_ssh_read() {
    let decision = decide(
        &policy(vec![
            rule(
                "read-all",
                PermissionAction::Read,
                "*",
                PermissionEffect::Allow,
            ),
            rule(
                "deny-ssh",
                PermissionAction::Read,
                "*/.ssh/*",
                PermissionEffect::Deny,
            ),
        ]),
        resource(
            PermissionAction::Read,
            "/home/other/.ssh/id_ed25519",
            b"ssh-key",
        ),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
}

#[test]
fn resource_names_do_not_override_configured_permission_effects() {
    let file_names = [
        ".env",
        ".env.local",
        ".env.example",
        "nested/.env.local",
        "store-v3.json",
        "nested/store-v3.json",
        ".ssh/id_ed25519",
        ".netrc",
        "application_default_credentials.json",
        "AGENTS.md",
        ".cookie-agent/config.toml",
    ];
    let cases = [
        (PermissionAction::Read, file_names.as_slice()),
        (PermissionAction::Write, file_names.as_slice()),
        (PermissionAction::Read, &["tool_result:call_123"][..]),
        (PermissionAction::Bash, &["cat .env", "rm -rf x"][..]),
        (PermissionAction::Delegate, &["reviewer"][..]),
        (PermissionAction::Mcp, &["github_delete_repo"][..]),
        (PermissionAction::Plugin, &["read .env"][..]),
        (PermissionAction::Skill, &["review"][..]),
    ];
    for (action, names) in cases {
        for name in names {
            let operation = operation(vec![resource(action, name, name.as_bytes())]);
            let labels = [Some((*name).into())];
            for effect in [
                PermissionEffect::Allow,
                PermissionEffect::Deny,
                PermissionEffect::Ask,
            ] {
                let rules = vec![rule("configured", action, "*", effect)];
                for (policy, overlay) in [
                    (policy(rules.clone()), None),
                    (
                        policy(vec![rule("agent", action, name, PermissionEffect::Deny)]),
                        Some(SessionPermissionOverlay { rules }),
                    ),
                ] {
                    let decision = PermissionPipeline::default().decide_operation_with_overlay(
                        &policy,
                        overlay.as_ref(),
                        &operation,
                        &labels,
                        std::path::Path::new("/workspace"),
                    );
                    assert_eq!(decision.effect, effect, "{action:?} {name}");
                    assert_eq!(decision.evaluations[0].trace.effect, effect);
                }
            }
            let decision = decide(&policy(Vec::new()), resource(action, name, name.as_bytes()));
            assert_eq!(
                decision.effect,
                PermissionEffect::Deny,
                "unmatched {action:?} {name}"
            );
            assert!(decision.evaluations[0].trace.candidates.is_empty());
        }
    }
}

#[test]
fn dotenv_files_accept_broad_policy_and_overlay_allows() {
    for action in [PermissionAction::Read, PermissionAction::Write] {
        for pattern in ["*", "${workspace_dir}/*"] {
            let allow_all = rule("allow-all", action, pattern, PermissionEffect::Allow);
            let overlay = SessionPermissionOverlay {
                rules: vec![allow_all.clone()],
            };
            for path in [
                ".env",
                ".env.local",
                "nested/.env",
                "nested/.env.local",
                "nested/.env.production.example",
                "/workspace/.env",
                "/workspace/.env.local",
            ] {
                let decision = decide(
                    &policy(vec![allow_all.clone()]),
                    resource(action, path, path.as_bytes()),
                );
                assert_eq!(
                    decision.effect,
                    PermissionEffect::Allow,
                    "{action:?} {pattern} {path}"
                );
                assert_eq!(decision.evaluations[0].trace.candidates.len(), 1);

                let decision = PermissionPipeline::default().decide_operation_with_overlay(
                    &policy(vec![rule(
                        "agent-deny",
                        action,
                        path,
                        PermissionEffect::Deny,
                    )]),
                    Some(&overlay),
                    &operation(vec![resource(action, path, path.as_bytes())]),
                    &[Some(path.into())],
                    std::path::Path::new("/workspace"),
                );
                assert_eq!(
                    decision.effect,
                    PermissionEffect::Allow,
                    "overlay {action:?} {pattern} {path}"
                );
                assert_eq!(
                    decision.evaluations[0].trace.precedence_reason,
                    "session overlay matching rule takes precedence over the agent document"
                );
            }
        }
    }
}

#[test]
fn dotenv_specific_rules_override_broad_allows_in_each_layer() {
    for action in [PermissionAction::Read, PermissionAction::Write] {
        for effect in [
            PermissionEffect::Allow,
            PermissionEffect::Deny,
            PermissionEffect::Ask,
        ] {
            for (broad, specific, path) in [
                ("*", ".env", ".env"),
                ("*", ".env.*", ".env.local"),
                ("*", "*/.env.*", "nested/.env.local"),
                ("${workspace_dir}/*", "${workspace_dir}/.env", ".env"),
                (
                    "${workspace_dir}/*",
                    "${workspace_dir}/.env.*",
                    ".env.local",
                ),
            ] {
                let rules = vec![
                    rule("specific", action, specific, effect),
                    rule("later-broad", action, broad, PermissionEffect::Allow),
                ];
                let decision = decide(
                    &policy(rules.clone()),
                    resource(action, path, path.as_bytes()),
                );
                assert_eq!(decision.effect, effect, "{action:?} {specific}");

                let decision = PermissionPipeline::default().decide_operation_with_overlay(
                    &policy(vec![rule(
                        "agent-allow",
                        action,
                        path,
                        PermissionEffect::Allow,
                    )]),
                    Some(&SessionPermissionOverlay { rules }),
                    &operation(vec![resource(action, path, path.as_bytes())]),
                    &[Some(path.into())],
                    std::path::Path::new("/workspace"),
                );
                assert_eq!(decision.effect, effect, "overlay {action:?} {specific}");
            }
        }
    }
}

#[test]
fn later_generic_allow_cannot_override_exact_env_deny() {
    let decision = decide(
        &policy(vec![
            rule(
                "exact-deny",
                PermissionAction::Read,
                "nested/.env.local",
                PermissionEffect::Deny,
            ),
            rule(
                "allow-all",
                PermissionAction::Read,
                "*",
                PermissionEffect::Allow,
            ),
        ]),
        resource(PermissionAction::Read, "nested/.env.local", b"env"),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
}

#[test]
fn unmatched_dotenv_files_use_normal_default_deny() {
    for action in [PermissionAction::Read, PermissionAction::Write] {
        for path in [".env", ".env.local", "nested/.env.local", "/outside/.env"] {
            for rules in [
                Vec::new(),
                vec![rule(
                    "unrelated",
                    action,
                    "${workspace_dir}/src/*",
                    PermissionEffect::Allow,
                )],
            ] {
                let decision = PermissionPipeline::default().decide_operation_with_overlay(
                    &policy(rules.clone()),
                    Some(&SessionPermissionOverlay { rules }),
                    &operation(vec![resource(action, path, path.as_bytes())]),
                    &[Some(path.into())],
                    std::path::Path::new("/workspace"),
                );
                assert_eq!(decision.effect, PermissionEffect::Deny, "{action:?} {path}");
                assert!(decision.evaluations[0].trace.candidates.is_empty());
                assert_eq!(
                    decision.evaluations[0].trace.precedence_reason,
                    "no matching rule; deny by default"
                );
            }
        }
    }
}

#[test]
fn tool_visibility_requires_an_effective_non_deny_rule() {
    let hidden = policy(vec![rule(
        "deny-all",
        PermissionAction::Read,
        "*",
        PermissionEffect::Deny,
    )]);
    assert!(!PermissionPipeline::tool_visible(&hidden, "read"));

    let specific_exception = policy(vec![
        rule(
            "deny-all",
            PermissionAction::Read,
            "*",
            PermissionEffect::Deny,
        ),
        rule(
            "allow-readme",
            PermissionAction::Read,
            "README.md",
            PermissionEffect::Allow,
        ),
    ]);
    assert!(PermissionPipeline::tool_visible(
        &specific_exception,
        "read"
    ));

    let denied_again = policy(vec![
        rule(
            "deny-all",
            PermissionAction::Read,
            "*",
            PermissionEffect::Deny,
        ),
        rule(
            "allow-readme",
            PermissionAction::Read,
            "README.md",
            PermissionEffect::Allow,
        ),
        rule(
            "deny-all-later",
            PermissionAction::Read,
            "*",
            PermissionEffect::Deny,
        ),
    ]);
    assert!(PermissionPipeline::tool_visible(&denied_again, "read"));

    let delegate_hidden = policy(vec![rule(
        "deny-delegate",
        PermissionAction::Delegate,
        "*",
        PermissionEffect::Deny,
    )]);
    assert!(!PermissionPipeline::tool_visible(
        &delegate_hidden,
        "delegate"
    ));
    let delegate_exception = policy(vec![
        rule(
            "deny-delegate",
            PermissionAction::Delegate,
            "*",
            PermissionEffect::Deny,
        ),
        rule(
            "allow-reviewer",
            PermissionAction::Delegate,
            "reviewer",
            PermissionEffect::Allow,
        ),
    ]);
    assert!(PermissionPipeline::tool_visible(
        &delegate_exception,
        "delegate"
    ));

    for permission_name in ["write", "bash"] {
        assert!(!PermissionPipeline::tool_visible(
            &policy(vec![rule(
                "deny-action",
                PermissionPipeline::action_for_permission_name(permission_name).unwrap(),
                "*",
                PermissionEffect::Deny,
            )]),
            permission_name,
        ));
    }

    assert!(!PermissionPipeline::tool_visible(
        &policy(Vec::new()),
        "unknown"
    ));
}

#[test]
fn empty_permissions_hide_all_known_actions() {
    let policy = policy(Vec::new());
    for permission_name in [
        "read",
        "write",
        "bash",
        "delegate",
        "mcp",
        "plugin:echo",
        "skill",
        "webfetch",
    ] {
        assert!(!PermissionPipeline::tool_visible(&policy, permission_name));
    }
}

#[test]
fn visibility_uses_any_rule_without_resource_precedence() {
    for effect in [PermissionEffect::Allow, PermissionEffect::Ask] {
        let rules = vec![
            rule(
                "opt-in",
                PermissionAction::Webfetch,
                "https://*.quantumcookie.xyz/*",
                effect,
            ),
            rule(
                "later-deny",
                PermissionAction::Webfetch,
                "https://*.quantumcookie.xyz/*",
                PermissionEffect::Deny,
            ),
        ];
        let deny = SessionPermissionOverlay {
            rules: vec![rule(
                "deny",
                PermissionAction::Webfetch,
                "*",
                PermissionEffect::Deny,
            )],
        };
        assert!(super::tool_visible(
            &rules,
            Some(&deny),
            PermissionAction::Webfetch
        ));
        assert!(super::tool_visible(
            &[],
            Some(&SessionPermissionOverlay {
                rules: rules.clone()
            }),
            PermissionAction::Webfetch
        ));
        assert!(!super::tool_visible(&rules, None, PermissionAction::Read));
        assert!(!super::tool_visible(
            &[],
            Some(&deny),
            PermissionAction::Webfetch
        ));
    }
    assert_eq!(
        decide_loose(&policy(Vec::new()), PermissionAction::Webfetch).effect,
        PermissionEffect::Deny
    );
}

#[test]
fn plugin_visibility_is_action_scoped_and_execution_remains_authoritative() {
    let plugin_policy = policy(vec![rule(
        "allow-echo",
        PermissionAction::Plugin,
        "echo *",
        PermissionEffect::Allow,
    )]);
    assert!(PermissionPipeline::tool_visible(
        &plugin_policy,
        "plugin:echo"
    ));
    assert!(PermissionPipeline::tool_visible(
        &plugin_policy,
        "plugin:delete"
    ));

    let overlay = SessionPermissionOverlay {
        rules: vec![rule(
            "deny-echo",
            PermissionAction::Plugin,
            "echo *",
            PermissionEffect::Deny,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &plugin_policy,
        Some(&overlay),
        "plugin:echo"
    ));

    let grant = SessionPermissionOverlay {
        rules: vec![rule(
            "grant-delete",
            PermissionAction::Plugin,
            "delete *",
            PermissionEffect::Allow,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_grants(
        &plugin_policy,
        None,
        Some(&grant),
        "plugin:delete",
        std::path::Path::new("/workspace"),
    ));

    let read_only_grant = SessionPermissionOverlay {
        rules: vec![rule(
            "grant-issue-read",
            PermissionAction::Plugin,
            "issue_read *",
            PermissionEffect::Allow,
        )],
    };
    let empty = policy(Vec::new());
    assert!(!PermissionPipeline::tool_visible_with_grants(
        &empty,
        None,
        Some(&read_only_grant),
        "plugin:issue_read",
        std::path::Path::new("/workspace"),
    ));
    assert!(!PermissionPipeline::tool_visible_with_grants(
        &empty,
        None,
        Some(&read_only_grant),
        "plugin:issue_delete",
        std::path::Path::new("/workspace"),
    ));
}

#[test]
fn session_overlay_denies_do_not_hide_agent_non_deny_rules() {
    let empty_policy = policy(Vec::new());
    let named_allow = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-named-allow",
            PermissionAction::Read,
            "README.md",
            PermissionEffect::Allow,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &empty_policy,
        Some(&named_allow),
        "read"
    ));

    let deny_with_exception = SessionPermissionOverlay {
        rules: vec![
            rule(
                "overlay-wildcard-deny",
                PermissionAction::Read,
                "*",
                PermissionEffect::Deny,
            ),
            rule(
                "overlay-named-ask",
                PermissionAction::Read,
                "README.md",
                PermissionEffect::Ask,
            ),
        ],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &empty_policy,
        Some(&deny_with_exception),
        "read"
    ));

    let wildcard_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-wildcard-deny",
            PermissionAction::Read,
            "*",
            PermissionEffect::Deny,
        )],
    };
    let policy_allow = policy(vec![rule(
        "policy-allow",
        PermissionAction::Read,
        "*",
        PermissionEffect::Allow,
    )]);
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &policy_allow,
        Some(&wildcard_deny),
        "read"
    ));

    let named_policy_allow = policy(vec![rule(
        "policy-named-allow",
        PermissionAction::Read,
        "README.md",
        PermissionEffect::Allow,
    )]);
    let identical_named_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-named-deny",
            PermissionAction::Read,
            "README.md",
            PermissionEffect::Deny,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &named_policy_allow,
        Some(&identical_named_deny),
        "read"
    ));

    let markdown_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-markdown-deny",
            PermissionAction::Read,
            "*.md",
            PermissionEffect::Deny,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &named_policy_allow,
        Some(&markdown_deny),
        "read"
    ));

    let text_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-text-deny",
            PermissionAction::Read,
            "*.txt",
            PermissionEffect::Deny,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &named_policy_allow,
        Some(&text_deny),
        "read"
    ));

    let policy_wildcard_allow = policy(vec![rule(
        "policy-wildcard-allow",
        PermissionAction::Read,
        "*",
        PermissionEffect::Allow,
    )]);
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &policy_wildcard_allow,
        Some(&identical_named_deny),
        "read"
    ));
}

#[test]
fn workspace_dir_pattern_matches_absolute_workspace_path_only() {
    let policy = policy(vec![rule(
        "workspace-write",
        PermissionAction::Write,
        "${workspace_dir}/src/*",
        PermissionEffect::Allow,
    )]);
    assert_eq!(
        super::effective_permission(
            &policy,
            PermissionAction::Write,
            "src/main.rs",
            std::path::Path::new("/workspace"),
        )
        .0,
        PermissionEffect::Allow
    );
    assert_eq!(
        super::effective_permission(
            &policy,
            PermissionAction::Write,
            "/outside/src/main.rs",
            std::path::Path::new("/workspace"),
        )
        .0,
        PermissionEffect::Deny
    );
}

#[test]
fn relative_and_workspace_dir_patterns_share_specificity_ordering() {
    let policy = policy(vec![
        rule(
            "relative",
            PermissionAction::Write,
            "src/*",
            PermissionEffect::Deny,
        ),
        rule(
            "absolute",
            PermissionAction::Write,
            "${workspace_dir}/src/*",
            PermissionEffect::Allow,
        ),
    ]);
    assert_eq!(
        super::effective_permission(
            &policy,
            PermissionAction::Write,
            "src/main.rs",
            std::path::Path::new("/workspace"),
        )
        .0,
        PermissionEffect::Allow
    );
}

#[cfg(unix)]
#[test]
fn workspace_dir_expansion_canonicalizes_the_workspace_anchor() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().join("real-workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let alias = temp.path().join("workspace-alias");
    symlink(&workspace, &alias).expect("workspace symlink");
    let policy = policy(vec![rule(
        "workspace-write",
        PermissionAction::Write,
        "${workspace_dir}/src/*",
        PermissionEffect::Allow,
    )]);
    assert_eq!(
        super::effective_permission(&policy, PermissionAction::Write, "src/main.rs", &alias,).0,
        PermissionEffect::Allow
    );
}

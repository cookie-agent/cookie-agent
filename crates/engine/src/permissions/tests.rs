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
        document_source: AgentDocumentSource::User,
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
    let view = super::effective_permission_view(
        &policy(Vec::new()),
        &SessionPermissionOverlay::empty(),
        &[],
    );
    let message = view
        .iter()
        .find(|entry| entry.action == PermissionAction::MESSAGE)
        .expect("message action is reported");
    assert_eq!(message.effect, PermissionEffect::Deny);
    assert_eq!(message.source, PermissionRuleSource::Default);
    assert!(message.patterns.is_empty());
}

#[test]
fn permission_names_map_to_actions_and_invalid_names_fail() {
    assert_eq!(
        PermissionPipeline::action_for_permission_name("message").expect("message action"),
        PermissionAction::MESSAGE
    );
    assert_eq!(
        PermissionPipeline::action_for_permission_name("browse")
            .expect("a runtime's own action")
            .as_str(),
        "browse"
    );
    for invalid in ["Message", "send-message", "plugin:"] {
        assert!(
            PermissionPipeline::action_for_permission_name(invalid).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn message_tool_visibility_requires_an_allow_or_ask_rule() {
    assert!(!tool_visible(&[], None, &PermissionAction::MESSAGE, None));
    let only_deny = [rule(
        "deny-all",
        PermissionAction::MESSAGE,
        "*",
        PermissionEffect::Deny,
    )];
    assert!(!tool_visible(
        &only_deny,
        None,
        &PermissionAction::MESSAGE,
        None
    ));
    let allows = [rule(
        "allow-child",
        PermissionAction::MESSAGE,
        "child",
        PermissionEffect::Allow,
    )];
    assert!(tool_visible(
        &allows,
        None,
        &PermissionAction::MESSAGE,
        None
    ));
    let asks = [rule(
        "ask-sibling",
        PermissionAction::MESSAGE,
        "sibling",
        PermissionEffect::Ask,
    )];
    assert!(tool_visible(&asks, None, &PermissionAction::MESSAGE, None));
    // Delegate rules never make the message tool visible: the actions are
    // independent by decision.
    let delegate_only = [rule(
        "allow-delegate",
        PermissionAction::DELEGATE,
        "*",
        PermissionEffect::Allow,
    )];
    assert!(!tool_visible(
        &delegate_only,
        None,
        &PermissionAction::MESSAGE,
        None
    ));
}

#[test]
fn message_relationship_labels_match_literal_resources_with_wildcard_catch_all() {
    let rules = policy(vec![
        rule(
            "allow-child",
            PermissionAction::MESSAGE,
            "child",
            PermissionEffect::Allow,
        ),
        rule(
            "ask-sibling",
            PermissionAction::MESSAGE,
            "sibling",
            PermissionEffect::Ask,
        ),
        rule(
            "deny-rest",
            PermissionAction::MESSAGE,
            "*",
            PermissionEffect::Deny,
        ),
    ]);
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::MESSAGE, "child", b"child")
        )
        .effect,
        PermissionEffect::Allow
    );
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::MESSAGE, "parent", b"parent")
        )
        .effect,
        PermissionEffect::Deny
    );
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::MESSAGE, "sibling", b"sibling")
        )
        .effect,
        PermissionEffect::Ask
    );
    assert_eq!(
        decide(
            &rules,
            resource(PermissionAction::MESSAGE, "grandparent", b"grandparent")
        )
        .effect,
        PermissionEffect::Deny
    );
    // Without any rule the default deny applies to every label.
    assert_eq!(
        decide(
            &policy(Vec::new()),
            resource(PermissionAction::MESSAGE, "child", b"child")
        )
        .effect,
        PermissionEffect::Deny
    );
}

#[test]
fn later_run_agent_governs_permission_mutation_classification() {
    let creation = policy(vec![rule(
        "creation",
        PermissionAction::READ,
        "*",
        PermissionEffect::Allow,
    )]);
    let later = policy(vec![rule(
        "later",
        PermissionAction::READ,
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
fn the_name_unconditional_is_an_ordinary_action() {
    // Permission-free tools declare no permission name; `unconditional` is no
    // longer special, so a rule can name it like any other action.
    assert_eq!(
        PermissionPipeline::action_for_permission_name("unconditional")
            .unwrap()
            .as_str(),
        "unconditional"
    );
    assert!(!PermissionPipeline::tool_visible_with_overlay(
        &policy(Vec::new()),
        None,
        "unconditional",
        None
    ));
    let decision = super::PermissionDecision::permission_free();
    assert_eq!(decision.effect, PermissionEffect::Allow);
    assert!(decision.evaluations.is_empty());
}

#[test]
fn skill_grants_allow_ask_but_never_override_deny() {
    let grants = SessionPermissionOverlay {
        rules: vec![rule(
            "skill-grant",
            PermissionAction::BASH,
            "git status",
            PermissionEffect::Allow,
        )],
    };
    let operation = operation(vec![resource(
        PermissionAction::BASH,
        "git status",
        b"git status",
    )]);
    let labels = [Some("git status".into())];
    let pipeline = PermissionPipeline::default();
    let ask = pipeline.decide_operation_with_grants(
        &policy(vec![rule(
            "ask",
            PermissionAction::BASH,
            "*",
            PermissionEffect::Ask,
        )]),
        None,
        Some(&grants),
        &operation,
        &labels,
        Some(std::path::Path::new("/workspace")),
    );
    assert_eq!(ask.effect, PermissionEffect::Allow);

    let denied = pipeline.decide_operation_with_grants(
        &policy(vec![rule(
            "deny",
            PermissionAction::BASH,
            "*",
            PermissionEffect::Deny,
        )]),
        None,
        Some(&grants),
        &operation,
        &labels,
        Some(std::path::Path::new("/workspace")),
    );
    assert_eq!(denied.effect, PermissionEffect::Deny);
}

#[test]
fn turn_scoped_skill_grant_requires_explicit_ask() {
    let base_policy = policy(vec![rule(
        "ask",
        PermissionAction::BASH,
        "git*",
        PermissionEffect::Ask,
    )]);
    let grants = SessionPermissionOverlay {
        rules: vec![
            rule(
                "skill-grant-exact",
                PermissionAction::BASH,
                "git",
                PermissionEffect::Allow,
            ),
            rule(
                "skill-grant-prefix",
                PermissionAction::BASH,
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
        Some(std::path::Path::new("/workspace")),
    ));
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        Some(&grants),
        "bash",
        Some(std::path::Path::new("/workspace")),
    ));
    for command in ["git", "git status", "git commit -m x"] {
        let operation = operation(vec![resource(
            PermissionAction::BASH,
            command,
            command.as_bytes(),
        )]);
        let decision = PermissionPipeline::default().decide_operation_with_grants(
            &base_policy,
            None,
            Some(&grants),
            &operation,
            &[Some(command.into())],
            Some(std::path::Path::new("/workspace")),
        );
        assert_eq!(decision.effect, PermissionEffect::Allow, "{command}");
    }
    let ungranted = PermissionPipeline::default().decide_operation_with_grants(
        &base_policy,
        None,
        Some(&grants),
        &operation(vec![resource(
            PermissionAction::BASH,
            "cargo test",
            b"cargo test",
        )]),
        &[Some("cargo test".into())],
        Some(std::path::Path::new("/workspace")),
    );
    assert_eq!(ungranted.effect, PermissionEffect::Deny);
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        None,
        "bash",
        Some(std::path::Path::new("/workspace")),
    ));

    let denied = policy(vec![rule(
        "deny",
        PermissionAction::BASH,
        "*",
        PermissionEffect::Deny,
    )]);
    assert!(!PermissionPipeline::tool_visible_with_grants(
        &denied,
        None,
        Some(&grants),
        "bash",
        Some(std::path::Path::new("/workspace")),
    ));
}

#[test]
fn skill_grant_can_publish_another_skill_action() {
    let base_policy = policy(vec![rule(
        "ask",
        PermissionAction::SKILL,
        "other-skill",
        PermissionEffect::Ask,
    )]);
    let grants = SessionPermissionOverlay {
        rules: vec![rule(
            "skill-chain",
            PermissionAction::SKILL,
            "other-skill",
            PermissionEffect::Allow,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        None,
        "skill",
        Some(std::path::Path::new("/workspace")),
    ));
    assert!(PermissionPipeline::tool_visible_with_grants(
        &base_policy,
        None,
        Some(&grants),
        "skill",
        Some(std::path::Path::new("/workspace")),
    ));
    let operation = operation(vec![resource(
        PermissionAction::SKILL,
        "other-skill",
        b"other-skill",
    )]);
    let decision = PermissionPipeline::default().decide_operation_with_grants(
        &base_policy,
        None,
        Some(&grants),
        &operation,
        &[Some("other-skill".into())],
        Some(std::path::Path::new("/workspace")),
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
        runtime: None,
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
    let action = &resources.first().expect("test resources").capability;
    let capabilities = vec![ApprovalCapability {
        action: action.clone(),
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
        Some(std::path::Path::new("/workspace")),
    )
}

fn decide_loose(policy: &AgentSnapshot, action: PermissionAction) -> super::PermissionDecision {
    let prepared_resource = resource(
        action.clone(),
        "unscoped-test-identity",
        b"permission-name-only",
    );
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
        Some(std::path::Path::new("/workspace")),
    )
}

#[test]
fn loose_permission_uses_only_bare_or_wildcard_effect() {
    let bare_allow = cookie_agent_config::PermissionValue::Effect(PermissionEffect::Allow)
        .rules(PermissionAction::DELEGATE);
    assert_eq!(
        decide_loose(&policy(bare_allow), PermissionAction::DELEGATE).effect,
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
                PermissionAction::DELEGATE,
                "*",
                effect,
            )]),
            PermissionAction::DELEGATE,
        );
        assert_eq!(decision.effect, effect);
        assert_eq!(decision.evaluations[0].trace.candidates.len(), 1);
    }
    let specific_only = decide_loose(
        &policy(vec![rule(
            "specific",
            PermissionAction::DELEGATE,
            "reviewer",
            PermissionEffect::Allow,
        )]),
        PermissionAction::DELEGATE,
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
                PermissionAction::BASH,
                literal,
                PermissionEffect::Deny,
            ),
            rule(
                "fallback-allow",
                PermissionAction::BASH,
                "*",
                PermissionEffect::Allow,
            ),
        ]),
        resource(PermissionAction::BASH, literal, literal.as_bytes()),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    assert_eq!(decision.evaluations[0].trace.normalized_resource, literal);
}

#[test]
fn delegate_spawn_matches_agent_pattern_while_session_tools_ignore_it() {
    let delegate_policy = policy(vec![
        rule(
            "reviewer",
            PermissionAction::DELEGATE,
            "reviewer",
            PermissionEffect::Allow,
        ),
        rule(
            "fallback",
            PermissionAction::DELEGATE,
            "*",
            PermissionEffect::Deny,
        ),
    ]);
    let spawn = decide(
        &delegate_policy,
        resource(PermissionAction::DELEGATE, "reviewer", b"reviewer"),
    );
    assert_eq!(spawn.effect, PermissionEffect::Allow);
    let session_tool = decide_loose(&delegate_policy, PermissionAction::DELEGATE);
    assert_eq!(session_tool.effect, PermissionEffect::Deny);

    let specific_deny_with_wildcard_allow = decide_loose(
        &policy(vec![
            rule(
                "reviewer",
                PermissionAction::DELEGATE,
                "reviewer",
                PermissionEffect::Deny,
            ),
            rule(
                "fallback",
                PermissionAction::DELEGATE,
                "*",
                PermissionEffect::Allow,
            ),
        ]),
        PermissionAction::DELEGATE,
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
            PermissionAction::MCP,
            "github_*",
            PermissionEffect::Allow,
        ),
        rule(
            "tool-deny",
            PermissionAction::MCP,
            "github_delete_repo",
            PermissionEffect::Deny,
        ),
    ]);
    assert_eq!(
        decide(
            &mcp_policy,
            resource(PermissionAction::MCP, "github_search", b"search")
        )
        .effect,
        PermissionEffect::Allow
    );
    assert_eq!(
        decide(
            &mcp_policy,
            resource(PermissionAction::MCP, "github_delete_repo", b"delete")
        )
        .effect,
        PermissionEffect::Deny
    );
    assert_eq!(
        decide(
            &mcp_policy,
            resource(PermissionAction::MCP, "slack_search", b"unmatched")
        )
        .effect,
        PermissionEffect::Deny
    );
    assert_eq!(
        PermissionPipeline::action_for_permission_name("mcp").expect("MCP action"),
        PermissionAction::MCP
    );
    assert!(PermissionPipeline::tool_visible(&mcp_policy, "mcp"));
    assert!(!PermissionPipeline::tool_visible(
        &policy(Vec::new()),
        "mcp"
    ));
    assert!(!PermissionPipeline::tool_visible(
        &policy(vec![rule(
            "deny-all",
            PermissionAction::MCP,
            "*",
            PermissionEffect::Deny,
        )]),
        "mcp"
    ));
    assert!(PermissionPipeline::tool_visible(
        &policy(vec![
            rule(
                "deny-all",
                PermissionAction::MCP,
                "*",
                PermissionEffect::Deny,
            ),
            rule(
                "allow-github",
                PermissionAction::MCP,
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
        PermissionAction::BASH,
        "git status",
        PermissionEffect::Allow,
    )]);
    let overlay = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-wildcard",
            PermissionAction::BASH,
            "*",
            PermissionEffect::Deny,
        )],
    };
    let decision = PermissionPipeline::default().decide_operation_with_overlay(
        &policy,
        Some(&overlay),
        &operation(vec![resource(
            PermissionAction::BASH,
            "git status",
            b"git status",
        )]),
        &[Some("git status".into())],
        Some(std::path::Path::new("/workspace")),
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
            PermissionAction::READ,
            "/workspace/a.txt",
            PermissionEffect::Allow,
        )]),
        resource(PermissionAction::READ, "/workspace/a.txt", b"file"),
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
                PermissionAction::READ,
                "/workspace/*",
                PermissionEffect::Allow,
            ),
            rule(
                "deny",
                PermissionAction::READ,
                "*/secret.txt",
                PermissionEffect::Deny,
            ),
        ]),
        resource(PermissionAction::READ, "/workspace/secret.txt", b"secret"),
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
                PermissionAction::READ,
                "*",
                PermissionEffect::Allow,
            ),
            rule(
                "deny",
                PermissionAction::READ,
                "*/.env.*",
                PermissionEffect::Deny,
            ),
        ]),
        resource(PermissionAction::READ, "nested/.env.local", b"secret"),
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
                PermissionAction::READ,
                "/workspace/*",
                PermissionEffect::Deny,
            ),
            rule(
                "later-allow",
                PermissionAction::READ,
                "/workspace/*",
                PermissionEffect::Allow,
            ),
        ]),
        resource(PermissionAction::READ, "/workspace/public.txt", b"public"),
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
    let resource = resource(PermissionAction::DELEGATE, "reviewer", b"reviewer");
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"delegate args"),
        vec![ApprovalCapability {
            action: PermissionAction::DELEGATE,
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
            PermissionAction::DELEGATE,
            "reviewer",
            PermissionEffect::Allow,
        )]),
        &operation,
        &[Some("reviewer".into())],
        Some(std::path::Path::new("/workspace")),
    );
    assert_eq!(
        PermissionPipeline::action_for_permission_name("delegate").expect("delegate action"),
        PermissionAction::DELEGATE
    );
    assert_eq!(allowed.effect, PermissionEffect::Allow);

    let denied = PermissionPipeline::default().decide_operation(
        &policy(vec![rule(
            "allow-explorer",
            PermissionAction::DELEGATE,
            "explorer",
            PermissionEffect::Allow,
        )]),
        &operation,
        &[Some("reviewer".into())],
        Some(std::path::Path::new("/workspace")),
    );
    assert_eq!(denied.effect, PermissionEffect::Deny);
}

#[test]
fn wildcard_allow_applies_to_non_secret_workspace_path() {
    let decision = decide(
        &policy(vec![rule(
            "allow",
            PermissionAction::READ,
            "/workspace/*",
            PermissionEffect::Allow,
        )]),
        resource(PermissionAction::READ, "/workspace/public.txt", b"public"),
    );
    assert_eq!(decision.effect, PermissionEffect::Allow);
}

#[test]
fn multi_resource_deny_wins_over_allow() {
    let decision = decide_many(
        &policy(vec![
            rule(
                "allow-public",
                PermissionAction::READ,
                "/workspace/public.txt",
                PermissionEffect::Allow,
            ),
            rule(
                "deny-secret",
                PermissionAction::READ,
                "/workspace/secret.txt",
                PermissionEffect::Deny,
            ),
        ]),
        vec![
            resource(PermissionAction::READ, "/workspace/public.txt", b"public"),
            resource(PermissionAction::READ, "/workspace/secret.txt", b"secret"),
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
                PermissionAction::READ,
                "/workspace/review.txt",
                PermissionEffect::Ask,
            ),
            rule(
                "allow-public",
                PermissionAction::READ,
                "/workspace/public.txt",
                PermissionEffect::Allow,
            ),
        ]),
        vec![
            resource(PermissionAction::READ, "/workspace/public.txt", b"public"),
            resource(PermissionAction::READ, "/workspace/review.txt", b"review"),
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
                PermissionAction::READ,
                "*",
                PermissionEffect::Allow,
            ),
            rule(
                "outside-etc",
                PermissionAction::READ,
                "/etc/*",
                PermissionEffect::Ask,
            ),
        ]),
        resource(PermissionAction::READ, "/etc/passwd", b"passwd"),
    );
    assert_eq!(decision.effect, PermissionEffect::Ask);
}

#[test]
fn absolute_deny_pattern_catches_outside_ssh_read() {
    let decision = decide(
        &policy(vec![
            rule(
                "read-all",
                PermissionAction::READ,
                "*",
                PermissionEffect::Allow,
            ),
            rule(
                "deny-ssh",
                PermissionAction::READ,
                "*/.ssh/*",
                PermissionEffect::Deny,
            ),
        ]),
        resource(
            PermissionAction::READ,
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
        (PermissionAction::READ, file_names.as_slice()),
        (PermissionAction::WRITE, file_names.as_slice()),
        (PermissionAction::READ, &["tool_result:call_123"][..]),
        (PermissionAction::BASH, &["cat .env", "rm -rf x"][..]),
        (PermissionAction::DELEGATE, &["reviewer"][..]),
        (PermissionAction::MCP, &["github_delete_repo"][..]),
        (
            PermissionAction::new("issue_read").expect("a plugin's own action"),
            &["read .env"][..],
        ),
        (PermissionAction::SKILL, &["review"][..]),
    ];
    for (action, names) in cases {
        for name in names {
            let operation = operation(vec![resource(action.clone(), name, name.as_bytes())]);
            let labels = [Some((*name).into())];
            for effect in [
                PermissionEffect::Allow,
                PermissionEffect::Deny,
                PermissionEffect::Ask,
            ] {
                let rules = vec![rule("configured", action.clone(), "*", effect)];
                for (policy, overlay) in [
                    (policy(rules.clone()), None),
                    (
                        policy(vec![rule(
                            "agent",
                            action.clone(),
                            name,
                            PermissionEffect::Deny,
                        )]),
                        Some(SessionPermissionOverlay { rules }),
                    ),
                ] {
                    let decision = PermissionPipeline::default().decide_operation_with_overlay(
                        &policy,
                        overlay.as_ref(),
                        &operation,
                        &labels,
                        Some(std::path::Path::new("/workspace")),
                    );
                    assert_eq!(decision.effect, effect, "{action:?} {name}");
                    assert_eq!(decision.evaluations[0].trace.effect, effect);
                }
            }
            let decision = decide(
                &policy(Vec::new()),
                resource(action.clone(), name, name.as_bytes()),
            );
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
    for action in [PermissionAction::READ, PermissionAction::WRITE] {
        for pattern in ["*", "${workspace_dir}/*"] {
            let allow_all = rule(
                "allow-all",
                action.clone(),
                pattern,
                PermissionEffect::Allow,
            );
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
                    resource(action.clone(), path, path.as_bytes()),
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
                        action.clone(),
                        path,
                        PermissionEffect::Deny,
                    )]),
                    Some(&overlay),
                    &operation(vec![resource(action.clone(), path, path.as_bytes())]),
                    &[Some(path.into())],
                    Some(std::path::Path::new("/workspace")),
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
    for action in [PermissionAction::READ, PermissionAction::WRITE] {
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
                    rule("specific", action.clone(), specific, effect),
                    rule(
                        "later-broad",
                        action.clone(),
                        broad,
                        PermissionEffect::Allow,
                    ),
                ];
                let decision = decide(
                    &policy(rules.clone()),
                    resource(action.clone(), path, path.as_bytes()),
                );
                assert_eq!(decision.effect, effect, "{action:?} {specific}");

                let decision = PermissionPipeline::default().decide_operation_with_overlay(
                    &policy(vec![rule(
                        "agent-allow",
                        action.clone(),
                        path,
                        PermissionEffect::Allow,
                    )]),
                    Some(&SessionPermissionOverlay { rules }),
                    &operation(vec![resource(action.clone(), path, path.as_bytes())]),
                    &[Some(path.into())],
                    Some(std::path::Path::new("/workspace")),
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
                PermissionAction::READ,
                "nested/.env.local",
                PermissionEffect::Deny,
            ),
            rule(
                "allow-all",
                PermissionAction::READ,
                "*",
                PermissionEffect::Allow,
            ),
        ]),
        resource(PermissionAction::READ, "nested/.env.local", b"env"),
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
}

#[test]
fn unmatched_dotenv_files_use_normal_default_deny() {
    for action in [PermissionAction::READ, PermissionAction::WRITE] {
        for path in [".env", ".env.local", "nested/.env.local", "/outside/.env"] {
            for rules in [
                Vec::new(),
                vec![rule(
                    "unrelated",
                    action.clone(),
                    "${workspace_dir}/src/*",
                    PermissionEffect::Allow,
                )],
            ] {
                let decision = PermissionPipeline::default().decide_operation_with_overlay(
                    &policy(rules.clone()),
                    Some(&SessionPermissionOverlay { rules }),
                    &operation(vec![resource(action.clone(), path, path.as_bytes())]),
                    &[Some(path.into())],
                    Some(std::path::Path::new("/workspace")),
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
        PermissionAction::READ,
        "*",
        PermissionEffect::Deny,
    )]);
    assert!(!PermissionPipeline::tool_visible(&hidden, "read"));

    let specific_exception = policy(vec![
        rule(
            "deny-all",
            PermissionAction::READ,
            "*",
            PermissionEffect::Deny,
        ),
        rule(
            "allow-readme",
            PermissionAction::READ,
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
            PermissionAction::READ,
            "*",
            PermissionEffect::Deny,
        ),
        rule(
            "allow-readme",
            PermissionAction::READ,
            "README.md",
            PermissionEffect::Allow,
        ),
        rule(
            "deny-all-later",
            PermissionAction::READ,
            "*",
            PermissionEffect::Deny,
        ),
    ]);
    assert!(PermissionPipeline::tool_visible(&denied_again, "read"));

    let delegate_hidden = policy(vec![rule(
        "deny-delegate",
        PermissionAction::DELEGATE,
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
            PermissionAction::DELEGATE,
            "*",
            PermissionEffect::Deny,
        ),
        rule(
            "allow-reviewer",
            PermissionAction::DELEGATE,
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
                PermissionAction::WEBFETCH,
                "https://*.quantumcookie.xyz/*",
                effect,
            ),
            rule(
                "later-deny",
                PermissionAction::WEBFETCH,
                "https://*.quantumcookie.xyz/*",
                PermissionEffect::Deny,
            ),
        ];
        let deny = SessionPermissionOverlay {
            rules: vec![rule(
                "deny",
                PermissionAction::WEBFETCH,
                "*",
                PermissionEffect::Deny,
            )],
        };
        assert!(super::tool_visible(
            &rules,
            Some(&deny),
            &PermissionAction::WEBFETCH,
            None
        ));
        assert!(super::tool_visible(
            &[],
            Some(&SessionPermissionOverlay {
                rules: rules.clone()
            }),
            &PermissionAction::WEBFETCH,
            None
        ));
        assert!(!super::tool_visible(
            &rules,
            None,
            &PermissionAction::READ,
            None
        ));
        assert!(!super::tool_visible(
            &[],
            Some(&deny),
            &PermissionAction::WEBFETCH,
            None
        ));
    }
    assert_eq!(
        decide_loose(&policy(Vec::new()), PermissionAction::WEBFETCH).effect,
        PermissionEffect::Deny
    );
}

#[test]
fn session_overlay_denies_do_not_hide_agent_non_deny_rules() {
    let empty_policy = policy(Vec::new());
    let named_allow = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-named-allow",
            PermissionAction::READ,
            "README.md",
            PermissionEffect::Allow,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &empty_policy,
        Some(&named_allow),
        "read",
        None
    ));

    let deny_with_exception = SessionPermissionOverlay {
        rules: vec![
            rule(
                "overlay-wildcard-deny",
                PermissionAction::READ,
                "*",
                PermissionEffect::Deny,
            ),
            rule(
                "overlay-named-ask",
                PermissionAction::READ,
                "README.md",
                PermissionEffect::Ask,
            ),
        ],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &empty_policy,
        Some(&deny_with_exception),
        "read",
        None
    ));

    let wildcard_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-wildcard-deny",
            PermissionAction::READ,
            "*",
            PermissionEffect::Deny,
        )],
    };
    let policy_allow = policy(vec![rule(
        "policy-allow",
        PermissionAction::READ,
        "*",
        PermissionEffect::Allow,
    )]);
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &policy_allow,
        Some(&wildcard_deny),
        "read",
        None
    ));

    let named_policy_allow = policy(vec![rule(
        "policy-named-allow",
        PermissionAction::READ,
        "README.md",
        PermissionEffect::Allow,
    )]);
    let identical_named_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-named-deny",
            PermissionAction::READ,
            "README.md",
            PermissionEffect::Deny,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &named_policy_allow,
        Some(&identical_named_deny),
        "read",
        None
    ));

    let markdown_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-markdown-deny",
            PermissionAction::READ,
            "*.md",
            PermissionEffect::Deny,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &named_policy_allow,
        Some(&markdown_deny),
        "read",
        None
    ));

    let text_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-text-deny",
            PermissionAction::READ,
            "*.txt",
            PermissionEffect::Deny,
        )],
    };
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &named_policy_allow,
        Some(&text_deny),
        "read",
        None
    ));

    let policy_wildcard_allow = policy(vec![rule(
        "policy-wildcard-allow",
        PermissionAction::READ,
        "*",
        PermissionEffect::Allow,
    )]);
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &policy_wildcard_allow,
        Some(&identical_named_deny),
        "read",
        None
    ));
}

#[test]
fn workspace_dir_pattern_matches_absolute_workspace_path_only() {
    let policy = policy(vec![rule(
        "workspace-write",
        PermissionAction::WRITE,
        "${workspace_dir}/src/*",
        PermissionEffect::Allow,
    )]);
    assert_eq!(
        super::effective_permission(
            &policy,
            &PermissionAction::WRITE,
            "src/main.rs",
            Some(std::path::Path::new("/workspace")),
        )
        .0,
        PermissionEffect::Allow
    );
    assert_eq!(
        super::effective_permission(
            &policy,
            &PermissionAction::WRITE,
            "/outside/src/main.rs",
            Some(std::path::Path::new("/workspace")),
        )
        .0,
        PermissionEffect::Deny
    );
}

#[test]
fn workspace_less_tools_never_match_workspace_dir_or_relative_path_rules() {
    let policy = policy(vec![
        rule(
            "workspace-read",
            PermissionAction::READ,
            "${workspace_dir}/*",
            PermissionEffect::Allow,
        ),
        rule(
            "relative-read",
            PermissionAction::READ,
            "notes/*",
            PermissionEffect::Allow,
        ),
        rule(
            "site",
            PermissionAction::WEBFETCH,
            "https://example.com/*",
            PermissionEffect::Allow,
        ),
    ]);
    let workspace = Some(std::path::Path::new("/workspace"));
    for (action, resource, with_workspace) in [
        (
            PermissionAction::READ,
            "/workspace/a.txt",
            PermissionEffect::Allow,
        ),
        (
            PermissionAction::READ,
            "notes/a.txt",
            PermissionEffect::Allow,
        ),
    ] {
        assert_eq!(
            super::effective_permission(&policy, &action, resource, workspace).0,
            with_workspace,
            "{resource} with a workspace"
        );
        assert_eq!(
            super::effective_permission(&policy, &action, resource, None).0,
            PermissionEffect::Deny,
            "{resource} without a workspace"
        );
    }
    // Rules that need no workspace keep matching workspace-less tools.
    assert_eq!(
        super::effective_permission(
            &policy,
            &PermissionAction::WEBFETCH,
            "https://example.com/page",
            None
        )
        .0,
        PermissionEffect::Allow
    );
    let decision = PermissionPipeline::default().decide_operation(
        &policy,
        &operation(vec![resource(
            PermissionAction::READ,
            "/workspace/a.txt",
            b"file",
        )]),
        &[Some("/workspace/a.txt".into())],
        None,
    );
    assert_eq!(decision.effect, PermissionEffect::Deny);
    assert!(decision.evaluations[0].trace.candidates.is_empty());
}

#[test]
fn relative_and_workspace_dir_patterns_share_specificity_ordering() {
    let policy = policy(vec![
        rule(
            "relative",
            PermissionAction::WRITE,
            "src/*",
            PermissionEffect::Deny,
        ),
        rule(
            "absolute",
            PermissionAction::WRITE,
            "${workspace_dir}/src/*",
            PermissionEffect::Allow,
        ),
    ]);
    assert_eq!(
        super::effective_permission(
            &policy,
            &PermissionAction::WRITE,
            "src/main.rs",
            Some(std::path::Path::new("/workspace")),
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
        PermissionAction::WRITE,
        "${workspace_dir}/src/*",
        PermissionEffect::Allow,
    )]);
    assert_eq!(
        super::effective_permission(
            &policy,
            &PermissionAction::WRITE,
            "src/main.rs",
            Some(alias.as_path()),
        )
        .0,
        PermissionEffect::Allow
    );
}

fn qualified(
    action: PermissionAction,
    runtime: &str,
    resource: &str,
    effect: PermissionEffect,
) -> PermissionRule {
    PermissionRule {
        runtime: Some(cookie_agent_protocol::RuntimeId::new(runtime).expect("runtime ID")),
        ..rule("qualified", action, resource, effect)
    }
}

fn decide_on(
    policy: &AgentSnapshot,
    command: &str,
    scope: super::PermissionScope<'_>,
) -> PermissionEffect {
    let resource = resource(PermissionAction::BASH, command, command.as_bytes());
    PermissionPipeline::default()
        .decide_operation(
            policy,
            &operation(vec![resource]),
            &[Some(command.to_owned())],
            scope,
        )
        .effect
}

#[test]
fn qualified_rules_apply_only_to_their_runtime_and_win_specificity_ties() {
    let policy = policy(vec![
        rule("bash", PermissionAction::BASH, "*", PermissionEffect::Ask),
        qualified(
            PermissionAction::BASH,
            "labpc",
            "*",
            PermissionEffect::Allow,
        ),
        rule(
            "git",
            PermissionAction::BASH,
            "git *",
            PermissionEffect::Deny,
        ),
        qualified(
            PermissionAction::BASH,
            "labpc",
            "cargo *",
            PermissionEffect::Deny,
        ),
        // Declared after the qualified rule with the same pattern: the
        // runtime selector still ranks it lower.
        rule(
            "cargo",
            PermissionAction::BASH,
            "cargo *",
            PermissionEffect::Allow,
        ),
    ]);
    let workspace = std::path::Path::new("/workspace");
    let labpc = super::PermissionScope::runtime("labpc", Some(workspace));
    let local = super::PermissionScope::runtime("local", Some(workspace));
    // Same resource specificity: the qualified rule wins on its runtime.
    assert_eq!(decide_on(&policy, "ls", labpc), PermissionEffect::Allow);
    assert_eq!(decide_on(&policy, "ls", local), PermissionEffect::Ask);
    assert_eq!(
        decide_on(&policy, "ls", super::PermissionScope::BASE),
        PermissionEffect::Ask
    );
    assert_eq!(
        decide_on(&policy, "cargo test", labpc),
        PermissionEffect::Deny
    );
    assert_eq!(
        decide_on(&policy, "cargo test", local),
        PermissionEffect::Allow
    );
    // A more specific unqualified pattern still beats a qualified catch-all.
    assert_eq!(
        decide_on(&policy, "git push", labpc),
        PermissionEffect::Deny
    );
}

#[test]
fn a_runtime_selector_limits_tool_visibility_to_that_runtime() {
    let policy = policy(vec![qualified(
        PermissionAction::BASH,
        "labpc",
        "*",
        PermissionEffect::Allow,
    )]);
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &policy,
        None,
        "bash",
        Some("labpc")
    ));
    assert!(!PermissionPipeline::tool_visible_with_overlay(
        &policy,
        None,
        "bash",
        Some("local")
    ));
    assert!(!PermissionPipeline::tool_visible_with_overlay(
        &policy, None, "bash", None
    ));
}

#[test]
fn overlay_rules_are_unique_per_runtime_selector() {
    let overlay = SessionPermissionOverlay {
        rules: vec![
            rule("bash", PermissionAction::BASH, "*", PermissionEffect::Ask),
            qualified(
                PermissionAction::BASH,
                "labpc",
                "*",
                PermissionEffect::Allow,
            ),
        ],
    };
    overlay.validate().expect("distinct selectors");
    let view = super::effective_permission_view(&policy(Vec::new()), &overlay, &[]);
    let bash = view
        .iter()
        .find(|entry| entry.action == PermissionAction::BASH)
        .expect("bash entry");
    assert_eq!(bash.effect, PermissionEffect::Ask);
    assert_eq!(bash.patterns.len(), 1);
    assert_eq!(
        bash.patterns[0]
            .runtime
            .as_ref()
            .map(|runtime| runtime.as_str()),
        Some("labpc")
    );
}

#[test]
fn tree_grants_apply_only_to_their_runtime() {
    use cookie_agent_protocol::{
        ApprovalId, OperationFingerprint, RuntimeId, SessionId, TreeApprovalGrant,
        TreeApprovalGrantId,
    };
    let mut granted = resource(PermissionAction::BASH, "git status", b"git status");
    granted.binding_lifetime = PreparedBindingLifetime::RestartStable;
    let operation = operation(vec![granted]);
    let root = SessionId::new_v7();
    let grant = TreeApprovalGrant {
        grant_id: TreeApprovalGrantId::new_v7(),
        root_session_id: root,
        approval_id: ApprovalId::new_v7(),
        operation_fingerprint: OperationFingerprint::from_prepared_operation(&operation),
        capabilities: operation.capabilities().to_vec(),
        resources: operation.resources().to_vec(),
        created_at: "2026-01-01T00:00:00Z".parse().expect("timestamp"),
        runtime_id: Some(RuntimeId::new("labpc").unwrap()),
    };
    let store = super::ApprovalStore::default();
    store.grant(grant.clone());
    let labpc = RuntimeId::new("labpc").unwrap();
    assert!(store.matching(root, &operation, Some(&labpc)).is_some());
    assert!(
        store
            .matching(root, &operation, Some(&RuntimeId::local()))
            .is_none()
    );
    assert!(store.matching(root, &operation, None).is_none());

    // The runtime ID is required.
    let mut missing = serde_json::to_value(&grant).expect("grant JSON");
    missing
        .as_object_mut()
        .expect("grant object")
        .remove("runtime_id");
    assert!(serde_json::from_value::<TreeApprovalGrant>(missing).is_err());
    // A base-layer grant keeps an explicit null.
    let mut base = serde_json::to_value(&grant).expect("grant JSON");
    base["runtime_id"] = serde_json::Value::Null;
    let base: TreeApprovalGrant = serde_json::from_value(base).expect("base grant");
    assert_eq!(base.runtime_id, None);
}

/// A runtime's own action is an opaque label: denied (so hidden) until a
/// rule names it, matched by plain wildcards, and scoped by runtime
/// selectors like any other action.
#[test]
fn a_runtime_action_is_denied_until_a_rule_names_it() {
    let browse = PermissionAction::new("browse").unwrap();
    assert!(!PermissionPipeline::tool_visible_with_overlay(
        &policy(Vec::new()),
        None,
        "browse",
        Some("web")
    ));

    let rules = vec![
        rule("browse", browse.clone(), "*", PermissionEffect::Ask),
        rule(
            "docs",
            browse.clone(),
            "https://docs.rs/*",
            PermissionEffect::Allow,
        ),
        // At equal specificity a runtime selector wins.
        qualified(
            browse.clone(),
            "labpc",
            "https://docs.rs/*",
            PermissionEffect::Deny,
        ),
    ];
    let agent = policy(rules);
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &agent,
        None,
        "browse",
        Some("web")
    ));
    let effect = |resource: &str, runtime: &str| {
        super::effective_permission(
            &agent,
            &browse,
            resource,
            super::PermissionScope::runtime(runtime, None),
        )
        .0
    };
    assert_eq!(
        effect("https://docs.rs/serde", "web"),
        PermissionEffect::Allow
    );
    assert_eq!(effect("https://example.com", "web"), PermissionEffect::Ask);
    assert_eq!(
        effect("https://docs.rs/serde", "labpc"),
        PermissionEffect::Deny
    );
}

#[test]
fn the_permission_view_lists_declared_and_named_actions_after_the_built_ins() {
    let browse = PermissionAction::new("browse").unwrap();
    let click = PermissionAction::new("click").unwrap();
    let overlay = SessionPermissionOverlay {
        rules: vec![rule("click", click.clone(), "*", PermissionEffect::Ask)],
    };
    let view = super::effective_permission_view(
        &policy(Vec::new()),
        &overlay,
        &[PermissionAction::READ, browse.clone()],
    );
    let actions = view
        .iter()
        .map(|entry| entry.action.clone())
        .collect::<Vec<_>>();
    let mut expected = PermissionAction::BUILT_IN.to_vec();
    expected.extend([browse, click]);
    assert_eq!(actions, expected);
    let browse_entry = &view[PermissionAction::BUILT_IN.len()];
    assert_eq!(browse_entry.effect, PermissionEffect::Deny);
    assert_eq!(browse_entry.source, PermissionRuleSource::Default);
    let click_entry = &view[PermissionAction::BUILT_IN.len() + 1];
    assert_eq!(click_entry.effect, PermissionEffect::Ask);
    assert_eq!(click_entry.source, PermissionRuleSource::SessionOverlay);
}

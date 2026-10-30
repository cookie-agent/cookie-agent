use cookie_agent_protocol::{
    AgentDocumentSource, AgentId, AgentMode, AgentSnapshot, ApprovalBoundary, ApprovalCapability,
    ApprovalResourceSource, PermissionAction, PermissionEffect, PermissionRule,
    PermissionRuleSource, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, SafeCode, SessionPermissionOverlay, Sha256Digest, WildcardPattern,
};

use super::{PermissionPipeline, select_governing_agent, tool_visible};

mod merged;

fn policy(rules: Vec<PermissionRule>) -> AgentSnapshot {
    AgentSnapshot {
        agent: AgentId::new("test").expect("agent id"),
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
fn effective_permission_view_reports_message_asking_by_default() {
    let view = super::effective_permission_view(
        &policy(Vec::new()),
        false,
        &SessionPermissionOverlay::empty(),
        &[],
    );
    let message = view
        .iter()
        .find(|entry| entry.action == PermissionAction::MESSAGE)
        .expect("message action is reported");
    // No rule names `message`: each message asks.
    assert_eq!(message.effect, PermissionEffect::Ask);
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
fn message_tool_visibility_needs_an_allow_or_ask_beside_a_star_deny() {
    // No rule at all leaves every message asking, so the tool is offered.
    assert!(tool_visible(&[], None, &PermissionAction::MESSAGE, None));
    let deny_all = || {
        rule(
            "deny-all",
            PermissionAction::MESSAGE,
            "*",
            PermissionEffect::Deny,
        )
    };
    assert!(!tool_visible(
        &[deny_all()],
        None,
        &PermissionAction::MESSAGE,
        None
    ));
    let allows = [
        deny_all(),
        rule(
            "allow-child",
            PermissionAction::MESSAGE,
            "child",
            PermissionEffect::Allow,
        ),
    ];
    assert!(tool_visible(
        &allows,
        None,
        &PermissionAction::MESSAGE,
        None
    ));
    let asks = [
        deny_all(),
        rule(
            "ask-sibling",
            PermissionAction::MESSAGE,
            "sibling",
            PermissionEffect::Ask,
        ),
    ];
    assert!(tool_visible(&asks, None, &PermissionAction::MESSAGE, None));
    // Delegate rules never make the message tool visible: the actions are
    // independent by decision.
    let delegate_allowed = [
        deny_all(),
        rule(
            "allow-delegate",
            PermissionAction::DELEGATE,
            "*",
            PermissionEffect::Allow,
        ),
    ];
    assert!(!tool_visible(
        &delegate_allowed,
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
    // Without any rule every label asks.
    assert_eq!(
        decide(
            &policy(Vec::new()),
            resource(PermissionAction::MESSAGE, "child", b"child")
        )
        .effect,
        PermissionEffect::Ask
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
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &policy(Vec::new()),
        None,
        "unconditional",
        None
    ));
    assert!(!PermissionPipeline::tool_visible_with_overlay(
        &policy(vec![rule(
            "deny-unconditional",
            PermissionAction::new("unconditional").unwrap(),
            "*",
            PermissionEffect::Deny,
        )]),
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
    // No rule matches it and the grants do not cover it, so it asks.
    assert_eq!(ungranted.effect, PermissionEffect::Ask);
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
    // A specific rule never decides a name-only check, so none matches and
    // it asks.
    assert_eq!(specific_only.effect, PermissionEffect::Ask);
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
        PermissionEffect::Ask
    );
    assert_eq!(
        PermissionPipeline::action_for_permission_name("mcp").expect("MCP action"),
        PermissionAction::MCP
    );
    assert!(PermissionPipeline::tool_visible(&mcp_policy, "mcp"));
    assert!(PermissionPipeline::tool_visible(&policy(Vec::new()), "mcp"));
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

/// A broad session rule no longer beats a narrower document rule: the most
/// specific rule of the merged list wins. A session rule with the document
/// rule's own pattern replaces it.
#[test]
fn a_broad_session_rule_does_not_beat_a_narrower_document_rule() {
    let policy = policy(vec![rule(
        "agent-specific",
        PermissionAction::BASH,
        "git status",
        PermissionEffect::Allow,
    )]);
    let decide_with = |overlay: &SessionPermissionOverlay| {
        PermissionPipeline::default().decide_operation_with_overlay(
            &policy,
            Some(overlay),
            &operation(vec![resource(
                PermissionAction::BASH,
                "git status",
                b"git status",
            )]),
            &[Some("git status".into())],
            Some(std::path::Path::new("/workspace")),
        )
    };
    let broad = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-wildcard",
            PermissionAction::BASH,
            "*",
            PermissionEffect::Deny,
        )],
    };
    let decision = decide_with(&broad);
    assert_eq!(decision.effect, PermissionEffect::Allow);
    let layers = decision.evaluations[0]
        .trace
        .candidates
        .iter()
        .map(|candidate| candidate.source_layer.as_str())
        .collect::<Vec<_>>();
    assert_eq!(layers, ["agent_document", "session_overlay"]);

    let exact = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-exact",
            PermissionAction::BASH,
            "git status",
            PermissionEffect::Deny,
        )],
    };
    let decision = decide_with(&exact);
    assert_eq!(decision.effect, PermissionEffect::Deny);
    let layers = decision.evaluations[0]
        .trace
        .candidates
        .iter()
        .map(|candidate| candidate.source_layer.as_str())
        .collect::<Vec<_>>();
    assert_eq!(layers, ["session_overlay"], "the document rule is replaced");
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

    // An allow for another agent type does not match, so the spawn asks.
    let other_type = PermissionPipeline::default().decide_operation(
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
    assert_eq!(other_type.effect, PermissionEffect::Ask);
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
                // The session's `*` rule replaces the document's.
                for (policy, overlay) in [
                    (policy(rules.clone()), None),
                    (
                        policy(vec![rule(
                            "agent",
                            action.clone(),
                            "*",
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
                PermissionEffect::Ask,
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

                // The session's rule replaces the document's with its pattern.
                let decision = PermissionPipeline::default().decide_operation_with_overlay(
                    &policy(vec![rule(
                        "agent-deny",
                        action.clone(),
                        pattern,
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
                assert_eq!(decision.evaluations[0].trace.candidates.len(), 1);
                assert_eq!(
                    decision.evaluations[0].trace.candidates[0]
                        .source_layer
                        .as_str(),
                    "session_overlay"
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

                // A specific session rule beats the document's broad allow.
                let decision = PermissionPipeline::default().decide_operation_with_overlay(
                    &policy(vec![rule(
                        "agent-broad-allow",
                        action.clone(),
                        broad,
                        PermissionEffect::Allow,
                    )]),
                    Some(&SessionPermissionOverlay {
                        rules: vec![rules[0].clone()],
                    }),
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
fn unmatched_dotenv_files_use_normal_default_ask() {
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
                assert_eq!(decision.effect, PermissionEffect::Ask, "{action:?} {path}");
                assert!(decision.evaluations[0].trace.candidates.is_empty());
                assert_eq!(
                    decision.evaluations[0].trace.precedence_reason,
                    "no matching rule; ask by default"
                );
            }
        }
    }
}

#[test]
fn tool_visibility_needs_a_non_deny_rule_beside_a_star_deny() {
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

    // No rule names `unknown`, so its calls ask and its tools are offered.
    assert!(PermissionPipeline::tool_visible(
        &policy(Vec::new()),
        "unknown"
    ));
}

#[test]
fn empty_permissions_offer_every_action_and_ask() {
    let policy = policy(Vec::new());
    for permission_name in [
        "read", "write", "bash", "delegate", "mcp", "echo", "skill", "webfetch",
    ] {
        assert!(
            PermissionPipeline::tool_visible(&policy, permission_name),
            "{permission_name}"
        );
        let action = PermissionPipeline::action_for_permission_name(permission_name).unwrap();
        assert_eq!(
            decide_loose(&policy, action).effect,
            PermissionEffect::Ask,
            "{permission_name}"
        );
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
        // Webfetch rules do not offer another action a `*` deny hides.
        let read_denied = [
            rules.clone(),
            vec![rule(
                "deny-read",
                PermissionAction::READ,
                "*",
                PermissionEffect::Deny,
            )],
        ]
        .concat();
        assert!(!super::tool_visible(
            &read_denied,
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
        PermissionEffect::Ask
    );
}

/// A session `deny` hides a tool only when it replaces every rule that
/// allows or asks for its action beside a `*` deny; a deny with another
/// pattern leaves the document's rules offering it.
#[test]
fn session_denies_hide_a_tool_only_by_replacing_its_allowing_rules() {
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
    assert!(!PermissionPipeline::tool_visible_with_overlay(
        &policy_allow,
        Some(&wildcard_deny),
        "read",
        None
    ));

    let named_policy_allow = policy(vec![
        rule(
            "policy-named-allow",
            PermissionAction::READ,
            "README.md",
            PermissionEffect::Allow,
        ),
        rule(
            "policy-wildcard-deny",
            PermissionAction::READ,
            "*",
            PermissionEffect::Deny,
        ),
    ]);
    let identical_named_deny = SessionPermissionOverlay {
        rules: vec![rule(
            "overlay-named-deny",
            PermissionAction::READ,
            "README.md",
            PermissionEffect::Deny,
        )],
    };
    assert!(!PermissionPipeline::tool_visible_with_overlay(
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
        PermissionEffect::Ask
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
        // No rule matches without a workspace, so it asks.
        assert_eq!(
            super::effective_permission(&policy, &action, resource, None).0,
            PermissionEffect::Ask,
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
    assert_eq!(decision.effect, PermissionEffect::Ask);
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

fn decide_read(
    policy: &AgentSnapshot,
    path: &str,
    scope: super::PermissionScope<'_>,
) -> PermissionEffect {
    let resource = resource(PermissionAction::READ, path, path.as_bytes());
    PermissionPipeline::default()
        .decide_operation(
            policy,
            &operation(vec![resource]),
            &[Some(path.to_owned())],
            scope,
        )
        .effect
}

/// A leading `~` in a path pattern expands against the home of the runtime
/// the tool runs on; on a runtime that reported none the pattern stays
/// literal and matches no absolute path. Runtime selectors still apply.
#[test]
fn a_home_pattern_expands_against_the_home_its_runtime_reported() {
    let policy = policy(vec![
        rule("read", PermissionAction::READ, "*", PermissionEffect::Deny),
        rule(
            "notes",
            PermissionAction::READ,
            "~/notes/*",
            PermissionEffect::Allow,
        ),
        rule("home", PermissionAction::READ, "~", PermissionEffect::Ask),
        qualified(
            PermissionAction::READ,
            "labpc",
            "~/secret/*",
            PermissionEffect::Ask,
        ),
    ]);
    let workspace = std::path::Path::new("/srv/api");
    let local = super::PermissionScope::runtime("local", Some(workspace));
    let labpc = super::PermissionScope::runtime("labpc", Some(workspace));
    let home = Some("/home/ada");
    // With a home: `~/notes/*` is `/home/ada/notes/*`, and `~` the home.
    for scope in [local.with_home(home), labpc.with_home(home)] {
        assert_eq!(
            decide_read(&policy, "/home/ada/notes/todo.md", scope),
            PermissionEffect::Allow
        );
        assert_eq!(
            decide_read(&policy, "/home/ada", scope),
            PermissionEffect::Ask
        );
        assert_eq!(
            decide_read(&policy, "/home/bob/notes/todo.md", scope),
            PermissionEffect::Deny
        );
    }
    // The qualified rule applies only on its runtime.
    assert_eq!(
        decide_read(&policy, "/home/ada/secret/key", labpc.with_home(home)),
        PermissionEffect::Ask
    );
    assert_eq!(
        decide_read(&policy, "/home/ada/secret/key", local.with_home(home)),
        PermissionEffect::Deny
    );
    // Without a home the patterns stay literal.
    for scope in [local, labpc, super::PermissionScope::BASE] {
        assert_eq!(
            decide_read(&policy, "/home/ada/notes/todo.md", scope),
            PermissionEffect::Deny
        );
        assert_eq!(
            decide_read(&policy, "/home/ada/secret/key", scope),
            PermissionEffect::Deny
        );
    }
    // A Windows home is spelled with `/`, as path labels are.
    assert_eq!(
        decide_read(
            &policy,
            "C:/Users/ada/notes/todo.md",
            local.with_home(Some(r"C:\Users\ada"))
        ),
        PermissionEffect::Allow
    );
}

#[test]
fn a_tool_scope_takes_the_home_of_its_instance() {
    let workspace = tempfile::tempdir().unwrap();
    let context = crate::WorkspaceContext::new(workspace.path()).unwrap();
    let info = cookie_agent_runtime_api::RuntimeInfo::coding("labpc", context);
    let without = crate::ToolOrigin::Runtime(std::sync::Arc::new(
        cookie_agent_runtime_api::RuntimeInfo::coding(
            "labpc",
            crate::WorkspaceContext::new(workspace.path()).unwrap(),
        ),
    ));
    let with = crate::ToolOrigin::Runtime(std::sync::Arc::new(
        info.with_home(Some("/home/ada".into())),
    ));
    assert_eq!(super::PermissionScope::of(&with).home, Some("/home/ada"));
    assert_eq!(super::PermissionScope::of(&with).runtime, Some("labpc"));
    assert_eq!(super::PermissionScope::of(&without).home, None);
    assert_eq!(
        super::PermissionScope::of(&crate::ToolOrigin::Base).home,
        None
    );
}

#[test]
fn only_a_leading_home_expands() {
    use super::expand_home_pattern as expand;
    let home = Some("/home/ada");
    assert_eq!(expand("~", home), "/home/ada");
    assert_eq!(expand("~/x/*", home), "/home/ada/x/*");
    assert_eq!(expand("~/x", Some("/home/ada/")), "/home/ada/x");
    assert_eq!(expand("~/x", Some("/")), "/x");
    assert_eq!(expand("~", Some("/")), "/");
    assert_eq!(expand("~/x", Some(r"C:\Users\ada")), "C:/Users/ada/x");
    for literal in ["~bob/x", "x/~/y", "*", "/etc/*", "${workspace_dir}/*"] {
        assert_eq!(expand(literal, home), literal);
    }
    assert_eq!(expand("~/x", None), "~/x");
    assert!(matches!(
        expand("/etc/*", home),
        std::borrow::Cow::Borrowed(_)
    ));
}

#[test]
fn a_runtime_selector_limits_tool_visibility_to_that_runtime() {
    let policy = policy(vec![
        rule(
            "deny-bash",
            PermissionAction::BASH,
            "*",
            PermissionEffect::Deny,
        ),
        qualified(
            PermissionAction::BASH,
            "labpc",
            "*",
            PermissionEffect::Allow,
        ),
    ]);
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
    let view = super::effective_permission_view(&policy(Vec::new()), false, &overlay, &[]);
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

/// A runtime's own action is an opaque label: asked for (so offered) until
/// a rule names it, matched by plain wildcards, and scoped by runtime
/// selectors like any other action.
#[test]
fn a_runtime_action_asks_until_a_rule_names_it() {
    let browse = PermissionAction::new("browse").unwrap();
    assert!(PermissionPipeline::tool_visible_with_overlay(
        &policy(Vec::new()),
        None,
        "browse",
        Some("web")
    ));
    assert_eq!(
        super::effective_permission(
            &policy(Vec::new()),
            &browse,
            "https://example.com",
            super::PermissionScope::runtime("web", None),
        )
        .0,
        PermissionEffect::Ask
    );

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
        false,
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
    assert_eq!(browse_entry.effect, PermissionEffect::Ask);
    assert_eq!(browse_entry.source, PermissionRuleSource::Default);
    let click_entry = &view[PermissionAction::BUILT_IN.len() + 1];
    assert_eq!(click_entry.effect, PermissionEffect::Ask);
    assert_eq!(click_entry.source, PermissionRuleSource::SessionOverlay);
}

fn agent_document(
    id: &str,
    mode: AgentMode,
    permissions: &[(&str, cookie_agent_config::PermissionValue)],
) -> cookie_agent_config::AgentDocument {
    let digest = cookie_agent_models::Sha256Digest::new("0".repeat(64)).expect("digest");
    cookie_agent_config::AgentDocument {
        official: false,
        official_version: None,
        id: AgentId::new(id).expect("agent ID"),
        frontmatter: cookie_agent_config::AgentFrontmatter {
            description: "Test agent".into(),
            mode,
            enabled: true,
            models: Vec::new(),
            limits: cookie_agent_config::AgentLimits::default(),
            permissions: permissions
                .iter()
                .map(|(key, value)| {
                    (
                        key.parse::<cookie_agent_config::PermissionKey>()
                            .expect("permission key"),
                        value.clone(),
                    )
                })
                .collect(),
            agent_md: None,
            acp: None,
        },
        body: "Test agent.\n".into(),
        source: AgentDocumentSource::User,
        document_fingerprint: digest.clone(),
        prompt_fingerprint: digest,
    }
}

fn effect_value(effect: PermissionEffect) -> cookie_agent_config::PermissionValue {
    cookie_agent_config::PermissionValue::Effect(effect)
}

/// The rules `document` gives a root run of a session with no machine or
/// project.
fn root_rules(document: &cookie_agent_config::AgentDocument) -> Vec<PermissionRule> {
    super::agent_permission_rules(document, false, &super::MemoryVariables::default())
}

/// `(action, resource, effect)` of each rule, in order.
fn rule_triples(rules: &[PermissionRule]) -> Vec<(String, String, PermissionEffect)> {
    rules
        .iter()
        .map(|rule| {
            (
                rule.action.as_str().to_owned(),
                rule.resource.as_str().to_owned(),
                rule.effect,
            )
        })
        .collect()
}

#[test]
fn mode_defaults_follow_the_agent_mode() {
    let triple =
        |action: &str, resource: &str, effect| (action.to_owned(), resource.to_owned(), effect);
    let allow = PermissionEffect::Allow;
    let ask = PermissionEffect::Ask;
    // Memory's, cron's, and attach's defaults depend on delegation too;
    // they have their own tests.
    let mode_rules = |mode| {
        let mut rules = root_rules(&agent_document("worker", mode, &[]));
        rules.retain(|rule| {
            rule.action != PermissionAction::MEMORY
                && rule.action != PermissionAction::MEMORY_WRITE
                && rule.action != PermissionAction::CRON
                && rule.action != PermissionAction::ATTACH
        });
        rule_triples(&rules)
    };
    for mode in [AgentMode::Primary, AgentMode::All] {
        assert_eq!(
            mode_rules(mode),
            [
                triple("list_runtimes", "*", allow),
                triple("skill", "*", allow),
                triple("mcp", "*", ask)
            ],
            "{mode:?}"
        );
    }
    assert_eq!(
        mode_rules(AgentMode::Subagent),
        [triple("skill", "*", allow), triple("mcp", "*", ask)]
    );
    let rules = root_rules(&agent_document("worker", AgentMode::Internal, &[]));
    assert!(rules.is_empty());
}

#[test]
fn root_runs_attach_machines_by_default_and_delegated_runs_do_not() {
    let attach = |rules: Vec<PermissionRule>| {
        rule_triples(
            &rules
                .into_iter()
                .filter(|rule| rule.action == PermissionAction::ATTACH)
                .collect::<Vec<_>>(),
        )
    };
    for mode in [AgentMode::Primary, AgentMode::All] {
        assert_eq!(
            attach(root_rules(&agent_document("worker", mode, &[]))),
            [("attach".to_owned(), "*".to_owned(), PermissionEffect::Allow)]
        );
    }
    assert!(
        attach(crate::permissions::run_default_rules(AgentMode::All, true)).is_empty(),
        "a delegated run attaches only when its document grants `attach`"
    );
    assert!(
        attach(root_rules(&agent_document(
            "worker",
            AgentMode::Internal,
            &[]
        )))
        .is_empty()
    );
}

#[test]
fn root_runs_manage_cron_jobs_by_default_and_delegated_runs_do_not() {
    let cron = |rules: Vec<PermissionRule>| {
        rule_triples(
            &rules
                .into_iter()
                .filter(|rule| rule.action == PermissionAction::CRON)
                .collect::<Vec<_>>(),
        )
    };
    let delegated = |document: &cookie_agent_config::AgentDocument| {
        super::agent_permission_rules(document, true, &super::MemoryVariables::default())
    };
    let allow_all = [("cron".to_owned(), "*".to_owned(), PermissionEffect::Allow)];
    for mode in [AgentMode::Primary, AgentMode::All, AgentMode::Subagent] {
        let document = agent_document("worker", mode, &[]);
        assert_eq!(cron(root_rules(&document)), allow_all, "{mode:?}");
        assert!(cron(delegated(&document)).is_empty(), "{mode:?}");
    }
    let internal = agent_document("worker", AgentMode::Internal, &[]);
    assert!(cron(root_rules(&internal)).is_empty());
    // A `cron` key replaces the default, root or delegated.
    let granted = agent_document(
        "worker",
        AgentMode::Primary,
        &[(
            "cron",
            resources(&[
                ("session", PermissionEffect::Allow),
                ("new", PermissionEffect::Ask),
            ]),
        )],
    );
    let expected = [
        (
            "cron".to_owned(),
            "session".to_owned(),
            PermissionEffect::Allow,
        ),
        ("cron".to_owned(), "new".to_owned(), PermissionEffect::Ask),
    ];
    assert_eq!(cron(root_rules(&granted)), expected);
    assert_eq!(cron(delegated(&granted)), expected);
}

/// The memory rules of `rules`, as `(action, resource, effect)`.
fn memory_triples(rules: &[PermissionRule]) -> Vec<(String, String, PermissionEffect)> {
    rule_triples(rules)
        .into_iter()
        .filter(|(action, _, _)| action == "memory" || action == "memory_write")
        .collect()
}

/// A root run of a non-internal agent without memory keys reads every
/// memory and writes the user layer, its own agent file, and its session's
/// machines and project (memory spec section 16.9.11, decision of
/// 2026-10-22); an internal agent gets no memory default.
#[test]
fn root_runs_default_to_reading_every_memory_and_writing_their_own() {
    let triple =
        |action: &str, resource: &str, effect| (action.to_owned(), resource.to_owned(), effect);
    let allow = PermissionEffect::Allow;
    let variables = super::MemoryVariables {
        machines: vec!["laptop".to_owned(), "labpc".to_owned()],
        project: Some("web".to_owned()),
    };
    for mode in [AgentMode::Primary, AgentMode::All, AgentMode::Subagent] {
        let document = agent_document("worker", mode, &[]);
        let rules = super::agent_permission_rules(&document, false, &variables);
        assert_eq!(
            memory_triples(&rules),
            [
                triple("memory", "*", allow),
                triple("memory_write", "user://*", allow),
                triple("memory_write", "agent://worker", allow),
                triple("memory_write", "machine://laptop/*", allow),
                triple("memory_write", "machine://labpc/*", allow),
                triple("memory_write", "project://web/*", allow),
            ],
            "{mode:?}"
        );
        let snapshot = policy(rules);
        // Writes outside the defaults match no rule, so they ask.
        for (action, resource, effect) in [
            (PermissionAction::MEMORY, "project://other/notes", allow),
            (PermissionAction::MEMORY_WRITE, "user://style", allow),
            (PermissionAction::MEMORY_WRITE, "project://web/build", allow),
            (
                PermissionAction::MEMORY_WRITE,
                "agent://reviewer",
                PermissionEffect::Ask,
            ),
            (
                PermissionAction::MEMORY_WRITE,
                "project://other/notes",
                PermissionEffect::Ask,
            ),
            (
                PermissionAction::MEMORY_WRITE,
                "machine://desktop/setup",
                PermissionEffect::Ask,
            ),
        ] {
            assert_eq!(
                effect_in_scope(&snapshot, &action, resource),
                effect,
                "{mode:?} {action:?} {resource}"
            );
        }
    }
    // Without a project or a machine those defaults match nothing, so they go.
    let rules = root_rules(&agent_document("worker", AgentMode::Primary, &[]));
    assert_eq!(
        memory_triples(&rules),
        [
            triple("memory", "*", allow),
            triple("memory_write", "user://*", allow),
            triple("memory_write", "agent://worker", allow),
        ]
    );
    // An internal agent gets none, root or delegated, so every memory call
    // asks.
    for delegated in [false, true] {
        let document = agent_document("worker", AgentMode::Internal, &[]);
        let rules = super::agent_permission_rules(&document, delegated, &variables);
        assert!(memory_triples(&rules).is_empty(), "{delegated}");
        let snapshot = policy(rules);
        for action in [PermissionAction::MEMORY, PermissionAction::MEMORY_WRITE] {
            assert_eq!(
                effect_in_scope(&snapshot, &action, "user://style"),
                PermissionEffect::Ask,
                "{delegated} {action:?}"
            );
        }
    }
}

/// A declared memory key replaces its action's root default entirely and
/// leaves the other action's default in place.
#[test]
fn a_declared_memory_key_replaces_the_root_default_for_its_action() {
    let document = agent_document(
        "worker",
        AgentMode::Subagent,
        &[("memory_write@global", effect_value(PermissionEffect::Ask))],
    );
    let rules = root_rules(&document);
    assert_eq!(
        action_rules(&rules, &PermissionAction::MEMORY),
        [(None, "*".to_owned(), PermissionEffect::Allow)]
    );
    assert_eq!(
        action_rules(&rules, &PermissionAction::MEMORY_WRITE),
        [(
            Some("global".to_owned()),
            "*".to_owned(),
            PermissionEffect::Ask
        )]
    );
    // Denying both hides every memory tool.
    let document = agent_document(
        "worker",
        AgentMode::Primary,
        &[
            ("memory", effect_value(PermissionEffect::Deny)),
            ("memory_write", effect_value(PermissionEffect::Deny)),
        ],
    );
    let rules = root_rules(&document);
    for action in [PermissionAction::MEMORY, PermissionAction::MEMORY_WRITE] {
        assert!(!super::tool_visible(&rules, None, &action, Some("global")));
    }
}

#[test]
fn a_declared_list_runtimes_key_replaces_its_default() {
    // `list_runtimes` is a subagent's only when its document grants it, and
    // a primary agent's document can turn it off.
    let granted = agent_document(
        "worker",
        AgentMode::Subagent,
        &[("list_runtimes", effect_value(PermissionEffect::Allow))],
    );
    assert_eq!(
        action_rules(&root_rules(&granted), &PermissionAction::LIST_RUNTIMES),
        [(None, "*".to_owned(), PermissionEffect::Allow)]
    );
    let denied = agent_document(
        "worker",
        AgentMode::Primary,
        &[("list_runtimes", effect_value(PermissionEffect::Deny))],
    );
    assert_eq!(
        action_rules(&root_rules(&denied), &PermissionAction::LIST_RUNTIMES),
        [(None, "*".to_owned(), PermissionEffect::Deny)]
    );
}

/// The effect of `action` on `resource` for a tool of the `global` runtime.
fn effect_in_scope(
    agent: &AgentSnapshot,
    action: &PermissionAction,
    resource: &str,
) -> PermissionEffect {
    super::effective_permission(
        agent,
        action,
        resource,
        super::PermissionScope::runtime("global", None),
    )
    .0
}

/// The rules of `action` among `rules`, as `(runtime, resource, effect)`.
fn action_rules(
    rules: &[PermissionRule],
    action: &PermissionAction,
) -> Vec<(Option<String>, String, PermissionEffect)> {
    rules
        .iter()
        .filter(|rule| rule.action == *action)
        .map(|rule| {
            (
                rule.runtime.as_ref().map(ToString::to_string),
                rule.resource.as_str().to_owned(),
                rule.effect,
            )
        })
        .collect()
}

#[test]
fn an_agent_without_an_mcp_key_asks_for_every_mcp_tool() {
    for mode in [AgentMode::Primary, AgentMode::All, AgentMode::Subagent] {
        let rules = root_rules(&agent_document("worker", mode, &[]));
        assert_eq!(
            action_rules(&rules, &PermissionAction::MCP),
            [(None, "*".to_owned(), PermissionEffect::Ask)],
            "{mode:?}"
        );
        assert!(super::tool_visible(
            &rules,
            None,
            &PermissionAction::MCP,
            Some("global")
        ));
        let mut snapshot = policy(rules);
        snapshot.mode = mode;
        assert_eq!(
            effect_in_scope(&snapshot, &PermissionAction::MCP, "github_search"),
            PermissionEffect::Ask
        );
    }
    // An internal agent gets no MCP default; its MCP calls ask because no
    // rule matches them.
    let rules = root_rules(&agent_document("worker", AgentMode::Internal, &[]));
    assert!(action_rules(&rules, &PermissionAction::MCP).is_empty());
    assert!(super::tool_visible(
        &rules,
        None,
        &PermissionAction::MCP,
        Some("global")
    ));
    assert_eq!(
        effect_in_scope(&policy(rules), &PermissionAction::MCP, "github_search"),
        PermissionEffect::Ask
    );
}

#[test]
fn a_declared_mcp_key_replaces_the_mcp_default_entirely() {
    let chrome = resources(&[
        ("my_chrome_*", PermissionEffect::Ask),
        ("*", PermissionEffect::Deny),
    ]);
    let document = agent_document("worker", AgentMode::Primary, &[("mcp", chrome)]);
    let rules = root_rules(&document);
    assert_eq!(
        action_rules(&rules, &PermissionAction::MCP),
        [
            (None, "my_chrome_*".to_owned(), PermissionEffect::Ask),
            (None, "*".to_owned(), PermissionEffect::Deny)
        ]
    );
    let snapshot = policy(rules);
    assert_eq!(
        effect_in_scope(&snapshot, &PermissionAction::MCP, "my_chrome_click"),
        PermissionEffect::Ask
    );
    assert_eq!(
        effect_in_scope(&snapshot, &PermissionAction::MCP, "github_search"),
        PermissionEffect::Deny,
        "only the declared servers' tools are reachable"
    );

    // A key with only a runtime selector declares the action too: the
    // unqualified default goes, so a `*` deny hides other runtimes' MCP
    // tools.
    let document = agent_document(
        "worker",
        AgentMode::Subagent,
        &[
            ("mcp@labpc", effect_value(PermissionEffect::Allow)),
            ("mcp@global", effect_value(PermissionEffect::Deny)),
        ],
    );
    let rules = root_rules(&document);
    assert_eq!(
        action_rules(&rules, &PermissionAction::MCP),
        [
            (
                Some("labpc".to_owned()),
                "*".to_owned(),
                PermissionEffect::Allow
            ),
            (
                Some("global".to_owned()),
                "*".to_owned(),
                PermissionEffect::Deny
            )
        ]
    );
    assert!(super::tool_visible(
        &rules,
        None,
        &PermissionAction::MCP,
        Some("labpc")
    ));
    assert!(!super::tool_visible(
        &rules,
        None,
        &PermissionAction::MCP,
        Some("global")
    ));
}

#[test]
fn the_permission_view_reports_mode_defaults_as_defaults() {
    let view_entry = |rules, delegated, action: &PermissionAction| {
        super::effective_permission_view(
            &policy(rules),
            delegated,
            &SessionPermissionOverlay::empty(),
            &[],
        )
        .into_iter()
        .find(|entry| entry.action == *action)
        .expect("listed action")
    };
    let entry = |rules, action: &PermissionAction| view_entry(rules, false, action);
    let defaulted = root_rules(&agent_document("worker", AgentMode::Primary, &[]));
    let mcp = entry(defaulted.clone(), &PermissionAction::MCP);
    assert_eq!(
        (mcp.effect, mcp.source),
        (PermissionEffect::Ask, PermissionRuleSource::Default)
    );
    let list = entry(defaulted.clone(), &PermissionAction::LIST_RUNTIMES);
    assert_eq!(
        (list.effect, list.source),
        (PermissionEffect::Allow, PermissionRuleSource::Default)
    );
    let memory = entry(defaulted, &PermissionAction::MEMORY);
    assert_eq!(
        (memory.effect, memory.source),
        (PermissionEffect::Allow, PermissionRuleSource::Default)
    );
    // The memory defaults are defaults with their variables expanded, for
    // any number of machines, and for a delegated run its own default.
    let memory_write_sources = |delegated, variables: &super::MemoryVariables| {
        let document = agent_document("test", AgentMode::Primary, &[]);
        let rules = super::agent_permission_rules(&document, delegated, variables);
        view_entry(rules, delegated, &PermissionAction::MEMORY_WRITE)
            .patterns
            .into_iter()
            .map(|rule| rule.source)
            .collect::<Vec<_>>()
    };
    let session = super::MemoryVariables {
        machines: vec!["laptop".to_owned(), "labpc".to_owned()],
        project: Some("web".to_owned()),
    };
    assert_eq!(
        memory_write_sources(false, &session),
        [PermissionRuleSource::Default; 5]
    );
    assert_eq!(
        memory_write_sources(true, &session),
        [PermissionRuleSource::Default]
    );
    // The delegated default in a root run is not the root default.
    let delegated = super::agent_permission_rules(
        &agent_document("test", AgentMode::Primary, &[]),
        true,
        &session,
    );
    let rules = view_entry(delegated, false, &PermissionAction::MEMORY_WRITE).patterns;
    assert_eq!(
        rules.iter().map(|rule| rule.source).collect::<Vec<_>>(),
        [PermissionRuleSource::AgentDocument]
    );
    // A document's own rule is the agent's, even beside a default.
    let declared = root_rules(&agent_document(
        "worker",
        AgentMode::Primary,
        &[("mcp", effect_value(PermissionEffect::Allow))],
    ));
    let mcp = entry(declared, &PermissionAction::MCP);
    assert_eq!(
        (mcp.effect, mcp.source),
        (PermissionEffect::Allow, PermissionRuleSource::AgentDocument)
    );
}

/// A delegated run gets its own agent file for each memory action its
/// document names no key for (memory spec section 16.9.11), not the wider
/// root default, whatever its mode.
#[test]
fn delegated_runs_default_to_their_own_agent_memory() {
    let none = super::MemoryVariables::default();
    for mode in [AgentMode::Primary, AgentMode::All, AgentMode::Subagent] {
        let document = agent_document("reviewer", mode, &[]);
        let rules = super::agent_permission_rules(&document, true, &none);
        for action in [PermissionAction::MEMORY, PermissionAction::MEMORY_WRITE] {
            assert_eq!(
                action_rules(&rules, &action),
                [(None, "agent://reviewer".to_owned(), PermissionEffect::Allow)],
                "{mode:?} {action:?}"
            );
        }
        let rules = super::agent_permission_rules(&document, false, &none);
        assert_eq!(
            action_rules(&rules, &PermissionAction::MEMORY),
            [(None, "*".to_owned(), PermissionEffect::Allow)],
            "{mode:?}"
        );
    }
    // A declared key replaces the default for its action only.
    let document = agent_document(
        "reviewer",
        AgentMode::Subagent,
        &[("memory", effect_value(PermissionEffect::Allow))],
    );
    let rules = super::agent_permission_rules(&document, true, &none);
    assert_eq!(
        action_rules(&rules, &PermissionAction::MEMORY),
        [(None, "*".to_owned(), PermissionEffect::Allow)]
    );
    assert_eq!(
        action_rules(&rules, &PermissionAction::MEMORY_WRITE),
        [(None, "agent://reviewer".to_owned(), PermissionEffect::Allow)]
    );
}

fn resources(entries: &[(&str, PermissionEffect)]) -> cookie_agent_config::PermissionValue {
    cookie_agent_config::PermissionValue::Resources(
        entries
            .iter()
            .map(|(resource, effect)| (WildcardPattern::new(*resource).unwrap(), *effect))
            .collect(),
    )
}

#[test]
fn memory_variables_expand_when_the_policy_is_frozen() {
    let allow = PermissionEffect::Allow;
    let document = agent_document(
        "builder",
        AgentMode::Primary,
        &[
            (
                "memory_write",
                resources(&[
                    ("agent://${agent_id}", allow),
                    ("machine://${session_machine}/*", allow),
                    ("project://${session_project}/*", allow),
                ]),
            ),
            ("memory", resources(&[("*", allow)])),
        ],
    );
    let variables = super::MemoryVariables {
        machines: vec!["laptop".to_owned(), "labpc".to_owned()],
        project: Some("web".to_owned()),
    };
    let rules = super::agent_permission_rules(&document, false, &variables);
    assert_eq!(
        action_rules(&rules, &PermissionAction::MEMORY_WRITE)
            .into_iter()
            .map(|(_, resource, _)| resource)
            .collect::<Vec<_>>(),
        [
            "agent://builder",
            "machine://laptop/*",
            "machine://labpc/*",
            "project://web/*"
        ]
    );
    // Without a project or a machine those rules match nothing, so they go.
    let rules = super::agent_permission_rules(&document, false, &super::MemoryVariables::default());
    assert_eq!(
        action_rules(&rules, &PermissionAction::MEMORY_WRITE),
        [(None, "agent://builder".to_owned(), allow)]
    );
    // Rules of other actions are never expanded.
    assert_eq!(
        action_rules(&rules, &PermissionAction::MEMORY),
        [(None, "*".to_owned(), allow)]
    );
}

#[test]
fn session_rules_refuse_memory_variables() {
    for resource in ["agent://${agent_id}", "project://${session_project}/*"] {
        let rules = vec![rule(
            "overlay",
            PermissionAction::MEMORY_WRITE,
            resource,
            PermissionEffect::Allow,
        )];
        assert!(super::check_rule_selectors(&rules).is_err(), "{resource}");
    }
}

#[test]
fn wildcard_patterns_intersect_exactly() {
    for (left, right) in [
        ("*", "project://*"),
        ("user://*", "user://feedback"),
        ("agent://re*", "agent://*er"),
        ("machine://?????/gpu", "machine://labpc/*"),
        ("project://web/*", "project://*/notes"),
        ("a*b*c", "*x*"),
        ("ab", "a*b"),
    ] {
        assert!(super::patterns_intersect(left, right), "{left} {right}");
        assert!(super::patterns_intersect(right, left), "{right} {left}");
    }
    for (left, right) in [
        ("user://*", "project://*"),
        ("agent://default", "agent://reviewer"),
        ("machine://????/gpu", "machine://labpc/*"),
        ("project://web/*", "project://api/*"),
        ("a*b", "*c"),
    ] {
        assert!(!super::patterns_intersect(left, right), "{left} {right}");
        assert!(!super::patterns_intersect(right, left), "{right} {left}");
    }
}

/// Whether the MCP tool `name` is offered under `rules`, `overlay`, and
/// `grants` on the runtime `runtime`.
fn mcp_offered(
    rules: &AgentSnapshot,
    overlay: Option<&SessionPermissionOverlay>,
    grants: Option<&SessionPermissionOverlay>,
    name: &str,
    runtime: Option<&str>,
) -> bool {
    PermissionPipeline::tool_offered(
        rules,
        overlay,
        grants,
        "mcp",
        name,
        super::PermissionScope {
            runtime,
            workspace: None,
            home: None,
        },
    )
}

#[test]
fn a_narrow_mcp_allow_offers_only_the_tools_it_matches() {
    let narrow = policy(vec![
        rule(
            "chrome",
            PermissionAction::MCP,
            "my_chrome_*",
            PermissionEffect::Allow,
        ),
        rule("rest", PermissionAction::MCP, "*", PermissionEffect::Deny),
    ]);
    assert!(mcp_offered(&narrow, None, None, "my_chrome_click", None));
    assert!(!mcp_offered(&narrow, None, None, "github_search", None));
    // The action is still offered: the per-name check is what narrows it.
    assert!(PermissionPipeline::tool_visible(&narrow, "mcp"));
}

#[test]
fn an_mcp_ask_offers_and_a_deny_pattern_hides_one_tool_of_an_allowed_server() {
    let asks = policy(vec![rule(
        "ask",
        PermissionAction::MCP,
        "*",
        PermissionEffect::Ask,
    )]);
    assert!(mcp_offered(&asks, None, None, "github_search", None));
    let one_denied = policy(vec![
        rule(
            "chrome",
            PermissionAction::MCP,
            "my_chrome_*",
            PermissionEffect::Allow,
        ),
        rule(
            "install",
            PermissionAction::MCP,
            "my_chrome_browser_install",
            PermissionEffect::Deny,
        ),
    ]);
    assert!(mcp_offered(
        &one_denied,
        None,
        None,
        "my_chrome_click",
        None
    ));
    assert!(!mcp_offered(
        &one_denied,
        None,
        None,
        "my_chrome_browser_install",
        None
    ));
    // Without any rule a tool asks, so it is offered.
    assert!(mcp_offered(
        &policy(Vec::new()),
        None,
        None,
        "my_chrome_click",
        None
    ));
}

#[test]
fn a_session_override_changes_which_mcp_tools_are_offered() {
    let asks = policy(vec![rule(
        "ask",
        PermissionAction::MCP,
        "*",
        PermissionEffect::Ask,
    )]);
    let deny_github = SessionPermissionOverlay {
        rules: vec![rule(
            "github",
            PermissionAction::MCP,
            "github_*",
            PermissionEffect::Deny,
        )],
    };
    assert!(!mcp_offered(
        &asks,
        Some(&deny_github),
        None,
        "github_search",
        None
    ));
    assert!(mcp_offered(
        &asks,
        Some(&deny_github),
        None,
        "my_chrome_click",
        None
    ));
    let deny_all = SessionPermissionOverlay {
        rules: vec![rule(
            "all",
            PermissionAction::MCP,
            "*",
            PermissionEffect::Deny,
        )],
    };
    assert!(!mcp_offered(
        &asks,
        Some(&deny_all),
        None,
        "my_chrome_click",
        None
    ));
}

#[test]
fn an_mcp_tool_is_offered_by_the_rules_of_its_runtime() {
    let on_labpc = policy(vec![
        rule("rest", PermissionAction::MCP, "*", PermissionEffect::Deny),
        PermissionRule {
            runtime: Some(cookie_agent_protocol::RuntimeId::new("labpc").expect("runtime ID")),
            action: PermissionAction::MCP,
            resource: WildcardPattern::new("browser_*").expect("wildcard"),
            effect: PermissionEffect::Allow,
        },
    ]);
    assert!(mcp_offered(
        &on_labpc,
        None,
        None,
        "browser_click",
        Some("labpc")
    ));
    assert!(!mcp_offered(
        &on_labpc,
        None,
        None,
        "browser_click",
        Some("global")
    ));
}

#[test]
fn a_grant_never_offers_an_mcp_tool_the_merged_rules_deny() {
    let grants = SessionPermissionOverlay {
        rules: vec![rule(
            "grant",
            PermissionAction::MCP,
            "github_*",
            PermissionEffect::Allow,
        )],
    };
    let asks = policy(vec![rule(
        "ask",
        PermissionAction::MCP,
        "*",
        PermissionEffect::Ask,
    )]);
    assert!(mcp_offered(
        &asks,
        None,
        Some(&grants),
        "github_search",
        None
    ));
    let denied = policy(vec![rule(
        "deny",
        PermissionAction::MCP,
        "github_*",
        PermissionEffect::Deny,
    )]);
    assert!(!mcp_offered(
        &denied,
        None,
        Some(&grants),
        "github_search",
        None
    ));
    let denied_all = policy(vec![rule(
        "deny-all",
        PermissionAction::MCP,
        "*",
        PermissionEffect::Deny,
    )]);
    assert!(!mcp_offered(
        &denied_all,
        None,
        Some(&grants),
        "github_search",
        None
    ));
}

#[test]
fn tools_of_other_actions_stay_offered_by_action() {
    let narrow = policy(vec![rule(
        "cargo",
        PermissionAction::BASH,
        "cargo test *",
        PermissionEffect::Allow,
    )]);
    assert!(PermissionPipeline::tool_offered(
        &narrow,
        None,
        None,
        "bash",
        "bash",
        super::PermissionScope::BASE,
    ));
}

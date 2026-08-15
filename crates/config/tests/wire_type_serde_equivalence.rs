use cookie_agent_config::{
    AgentDocumentSource as ConfigDocumentSource, AgentMode as ConfigAgentMode,
    PermissionAction as ConfigPermissionAction, PermissionEffect as ConfigPermissionEffect,
    PermissionRule as ConfigPermissionRule,
};
use cookie_agent_identity::WildcardPattern;
use cookie_agent_protocol::{
    AgentDocumentSource, AgentMode, PermissionAction, PermissionEffect, PermissionRule,
};
use serde::Serialize;

fn assert_same_json(left: &impl Serialize, right: &impl Serialize) {
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap()
    );
}

#[test]
fn config_agent_wire_types_match_protocol() {
    for (config, protocol) in [
        (ConfigAgentMode::Primary, AgentMode::Primary),
        (ConfigAgentMode::Subagent, AgentMode::Subagent),
        (ConfigAgentMode::All, AgentMode::All),
        (ConfigAgentMode::Internal, AgentMode::Internal),
    ] {
        assert_same_json(&config, &protocol);
    }
    for (config, protocol) in [
        (ConfigPermissionAction::Read, PermissionAction::Read),
        (ConfigPermissionAction::Write, PermissionAction::Write),
        (ConfigPermissionAction::Bash, PermissionAction::Bash),
        (ConfigPermissionAction::Delegate, PermissionAction::Delegate),
        (ConfigPermissionAction::Mcp, PermissionAction::Mcp),
    ] {
        assert_same_json(&config, &protocol);
    }
    for (config, protocol) in [
        (ConfigPermissionEffect::Allow, PermissionEffect::Allow),
        (ConfigPermissionEffect::Ask, PermissionEffect::Ask),
        (ConfigPermissionEffect::Deny, PermissionEffect::Deny),
    ] {
        assert_same_json(&config, &protocol);
    }
    for (config, protocol) in [
        (ConfigDocumentSource::BuiltIn, AgentDocumentSource::BuiltIn),
        (ConfigDocumentSource::User, AgentDocumentSource::User),
        (
            ConfigDocumentSource::Workspace,
            AgentDocumentSource::Workspace,
        ),
    ] {
        assert_same_json(&config, &protocol);
    }

    let resource = WildcardPattern::new("${workspace_dir}/src/*").unwrap();
    assert_same_json(
        &ConfigPermissionRule {
            action: ConfigPermissionAction::Read,
            resource: resource.clone(),
            effect: ConfigPermissionEffect::Allow,
        },
        &PermissionRule {
            action: PermissionAction::Read,
            resource,
            effect: PermissionEffect::Allow,
        },
    );
}

#[test]
fn permission_rules_reject_unknown_fields_in_both_crates() {
    let json = r#"{"action":"read","resource":"*","effect":"allow","extra":true}"#;
    assert!(serde_json::from_str::<ConfigPermissionRule>(json).is_err());
    assert!(serde_json::from_str::<PermissionRule>(json).is_err());
}

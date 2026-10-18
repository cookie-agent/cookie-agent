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
        (ConfigPermissionAction::READ, PermissionAction::READ),
        (ConfigPermissionAction::WRITE, PermissionAction::WRITE),
        (ConfigPermissionAction::BASH, PermissionAction::BASH),
        (ConfigPermissionAction::DELEGATE, PermissionAction::DELEGATE),
        (ConfigPermissionAction::MCP, PermissionAction::MCP),
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
    ] {
        assert_same_json(&config, &protocol);
    }

    let resource = WildcardPattern::new("${workspace_dir}/src/*").unwrap();
    assert_same_json(
        &ConfigPermissionRule {
            action: ConfigPermissionAction::READ,
            runtime: None,
            resource: resource.clone(),
            effect: ConfigPermissionEffect::Allow,
        },
        &PermissionRule {
            action: PermissionAction::READ,
            runtime: None,
            resource,
            effect: PermissionEffect::Allow,
        },
    );
}

#[test]
fn permission_rules_reject_unknown_fields_in_commands() {
    let json = serde_json::json!({"action":"read","resource":"*","effect":"allow","extra":true});
    // Configuration builds its rules rather than decoding them. The rule is
    // also stored in session history, which ignores unknown members; a
    // command that carries one refuses them.
    assert!(serde_json::from_value::<ConfigPermissionRule>(json.clone()).is_ok());
    assert!(matches!(
        cookie_agent_protocol::strict::decode_command::<PermissionRule>(json),
        Err(cookie_agent_protocol::strict::CommandDecodeError::UnknownField { field })
            if field == "extra"
    ));
}

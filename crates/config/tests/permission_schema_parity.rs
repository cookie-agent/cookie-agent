use std::fs;

use cookie_agent_config::{
    AgentFrontmatter, ConfigError, PermissionAction, PermissionEffect, PermissionValue,
    load_from_roots, simple_wildcard_match,
};
use tempfile::TempDir;

fn config_rule(resource: &str) -> Result<PermissionValue, serde_yaml::Error> {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(
        serde_yaml::Value::String(resource.to_owned()),
        serde_yaml::Value::String("allow".to_owned()),
    );
    serde_yaml::from_value(serde_yaml::Value::Mapping(mapping))
}

fn protocol_rule(
    resource: &str,
) -> Result<cookie_agent_protocol::PermissionRule, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "action": "read",
        "resource": resource,
        "effect": "allow"
    }))
}

#[test]
fn config_and_protocol_reject_the_same_adversarial_permission_resources() {
    let three_byte_boundary = "界".repeat(1365);
    let three_byte_overflow = "界".repeat(1366);
    let four_byte_boundary = "😀".repeat(1024);
    let four_byte_overflow = "😀".repeat(1025);
    let cases = [
        ("*", true),
        ("資料/?", true),
        ("${workspace_dir}/src/*", true),
        ("${foo}/src/*", false),
        (three_byte_boundary.as_str(), true),
        (three_byte_overflow.as_str(), false),
        (four_byte_boundary.as_str(), true),
        (four_byte_overflow.as_str(), false),
        ("", false),
        ("**", false),
        ("a**b", false),
        (r"path\\*", false),
        ("[ab]", false),
        ("{a,b}", false),
        ("line\nfeed", false),
        (&"a".repeat(4097), false),
    ];

    for (resource, expected) in cases {
        assert_eq!(
            config_rule(resource).is_ok(),
            expected,
            "config {resource:?}"
        );
        assert_eq!(
            protocol_rule(resource).is_ok(),
            expected,
            "protocol {resource:?}"
        );
    }
}

#[test]
fn old_rule_list_and_delegation_field_are_rejected() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join(".cookie-agent");
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(
        root.join("agents/worker.md"),
        "---\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  - { id: old, action: read, resource: \"*\", effect: allow }\ndelegation: { agents: [worker], max_depth: 1 }\n---\nWorker.\n",
    )
    .unwrap();
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::AgentDocument { .. })
    ));
}

#[test]
fn removed_tools_field_names_permission_driven_replacement() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join(".cookie-agent");
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(
        root.join("agents/worker.md"),
        "---\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\ntools: [read]\npermissions: {}\n---\nWorker.\n",
    )
    .unwrap();
    let error = load_from_roots(None, Some(&root)).unwrap_err();
    assert!(matches!(error, ConfigError::AgentToolsRemoved(_)));
    let message = error.to_string();
    assert!(message.contains("`tools`"));
    assert!(message.contains("`permissions`"));
}

#[test]
fn permissions_field_defaults_to_empty() {
    let frontmatter: AgentFrontmatter = serde_yaml::from_str(
        "description: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\n",
    )
    .unwrap();
    assert!(frontmatter.permissions.is_empty());
}

#[test]
fn frontmatter_uses_action_keyed_ordered_permissions() {
    let frontmatter: AgentFrontmatter = serde_yaml::from_str(
        "description: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  read:\n    \"*\": ask\n    \"file?.rs\": allow\n  bash: deny\n",
    )
    .unwrap();
    let read = frontmatter
        .permissions
        .get(&PermissionAction::Read)
        .unwrap();
    let PermissionValue::Resources(resources) = read else {
        panic!("read should use resource map form");
    };
    assert_eq!(resources.get_index(0).unwrap().0.as_str(), "*");
    assert_eq!(resources.get_index(1).unwrap().0.as_str(), "file?.rs");
    assert_eq!(resources.get_index(1).unwrap().1, &PermissionEffect::Allow);
    assert!(matches!(
        frontmatter.permissions.get(&PermissionAction::Bash),
        Some(PermissionValue::Effect(PermissionEffect::Deny))
    ));
}

#[test]
fn duplicate_action_and_resource_keys_are_rejected() {
    let duplicate_action = "description: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  read: allow\n  read: deny\n";
    assert!(serde_yaml::from_str::<AgentFrontmatter>(duplicate_action).is_err());

    let duplicate_resource = "description: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  read:\n    \"*\": allow\n    \"*\": deny\n";
    assert!(serde_yaml::from_str::<AgentFrontmatter>(duplicate_resource).is_err());
}

#[test]
fn grep_and_glob_permission_keys_are_rejected() {
    for action in ["grep", "glob"] {
        assert!(
            serde_yaml::from_str::<AgentFrontmatter>(&format!(
                "description: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  {action}: deny\n"
            ))
            .is_err(),
            "{action} permission key must be unknown"
        );
    }
}

#[test]
fn leftover_agent_schema_is_rejected_with_removal_guidance() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join(".cookie-agent");
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(
        root.join("agents/worker.md"),
        "---\nschema: 5\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions: {}\n---\nWorker.\n",
    )
    .unwrap();

    let error = load_from_roots(None, Some(&root)).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::AgentSchemaRemoved { line: 2, .. }
    ));
    let message = error.to_string();
    assert!(message.contains("agents/worker.md"), "{message}");
    assert!(message.contains("remove the schema field"), "{message}");
}

#[test]
fn checked_workspace_agents_preserve_permission_outcomes() {
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cookie-agent");
    let loaded = load_from_roots(None, Some(&project)).expect("checked workspace agents");
    assert_eq!(loaded.agents.len(), 5);
    for (id, document) in &loaded.agents {
        let cases = [
            (PermissionAction::Read, ".env", PermissionEffect::Deny),
            (
                PermissionAction::Read,
                "nested/.env.local",
                PermissionEffect::Deny,
            ),
            (
                PermissionAction::Read,
                ".env.example",
                PermissionEffect::Allow,
            ),
            (
                PermissionAction::Read,
                "nested/.env.example",
                PermissionEffect::Allow,
            ),
            (
                PermissionAction::Read,
                "store-v3.json",
                PermissionEffect::Deny,
            ),
            (
                PermissionAction::Read,
                "nested/token-v1",
                PermissionEffect::Deny,
            ),
            (PermissionAction::Read, "id_ed25519", PermissionEffect::Deny),
            (PermissionAction::Read, ".netrc", PermissionEffect::Deny),
            (
                PermissionAction::Read,
                "application_default_credentials.json",
                PermissionEffect::Deny,
            ),
            (
                PermissionAction::Read,
                "src/lib.rs",
                PermissionEffect::Allow,
            ),
        ];
        for (action, resource, expected) in cases {
            assert_eq!(
                configured_effect(&document.frontmatter, action, resource),
                expected,
                "{id} {action:?} {resource}"
            );
        }
        if id.as_str() == "worker" {
            assert!(
                !document
                    .frontmatter
                    .permissions
                    .contains_key(&PermissionAction::Delegate)
            );
        } else {
            assert_eq!(
                configured_effect(&document.frontmatter, PermissionAction::Delegate, "worker"),
                PermissionEffect::Ask
            );
            assert_eq!(
                configured_effect(&document.frontmatter, PermissionAction::Write, "operation"),
                PermissionEffect::Ask
            );
            assert_eq!(
                configured_effect(&document.frontmatter, PermissionAction::Bash, "git status"),
                PermissionEffect::Ask
            );
            assert_eq!(
                configured_effect(&document.frontmatter, PermissionAction::Bash, "cat .env"),
                PermissionEffect::Deny
            );
            assert_eq!(
                configured_effect(&document.frontmatter, PermissionAction::Bash, "rm -rf x"),
                PermissionEffect::Deny
            );
            assert_eq!(
                configured_effect(
                    &document.frontmatter,
                    PermissionAction::Bash,
                    "git status && rm -rf x"
                ),
                PermissionEffect::Deny
            );
        }
    }
}

fn configured_effect(
    frontmatter: &AgentFrontmatter,
    action: PermissionAction,
    resource: &str,
) -> PermissionEffect {
    let Some(value) = frontmatter.permissions.get(&action) else {
        return PermissionEffect::Ask;
    };
    let name = resource.rsplit('/').next().unwrap_or(resource);
    let protected_env = action == PermissionAction::Read
        && (name == ".env" || name.starts_with(".env."))
        && !name.ends_with(".example");
    value
        .rules(action)
        .into_iter()
        .enumerate()
        .filter(|(_, rule)| {
            simple_wildcard_match(rule.resource.as_str(), resource)
                && !(protected_env
                    && rule.effect == PermissionEffect::Allow
                    && rule.resource.as_str() != resource)
        })
        .max_by_key(|(index, rule)| {
            let wildcards = rule
                .resource
                .as_str()
                .chars()
                .filter(|character| matches!(character, '*' | '?'))
                .count();
            let literals = rule.resource.as_str().chars().count() - wildcards;
            (literals, std::cmp::Reverse(wildcards), *index)
        })
        .map_or(PermissionEffect::Ask, |(_, rule)| rule.effect)
}

#[test]
fn workspace_dir_expression_is_action_scoped_and_unknown_expressions_are_rejected() {
    for action in ["bash", "delegate"] {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(".cookie-agent");
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        fs::write(
            root.join("agents/worker.md"),
            format!(
                "---\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  {action}:\n    \"${{workspace_dir}}/*\": allow\n---\nWorker.\n"
            ),
        )
        .unwrap();
        assert!(matches!(
            load_from_roots(None, Some(&root)),
            Err(ConfigError::AgentPermissionExpression(_))
        ));
    }

    for resource in ["${foo}/src/*", "${workspace_dir/src/*"] {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(".cookie-agent");
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        fs::write(
            root.join("agents/worker.md"),
            format!(
                "---\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  read:\n    \"{resource}\": allow\n---\nWorker.\n"
            ),
        )
        .unwrap();
        assert!(matches!(
            load_from_roots(None, Some(&root)),
            Err(ConfigError::AgentPermissionExpression(_))
        ));
    }
}

#[test]
fn workspace_dir_expression_is_portable_for_filesystem_permissions() {
    let temp = TempDir::new().unwrap();
    let document = "---\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\npermissions:\n  read:\n    \"${workspace_dir}/src/*\": allow\n  write:\n    \"${workspace_dir}/src/*\": allow\n---\nWorker.\n";
    let mut fingerprints = Vec::new();
    for name in ["workspace-a", "workspace-b"] {
        let root = temp.path().join(name).join(".cookie-agent");
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        fs::write(root.join("agents/worker.md"), document).unwrap();
        let loaded = load_from_roots(None, Some(&root)).unwrap();
        let worker = loaded.agents.values().next().unwrap();
        fingerprints.push(worker.document_fingerprint.clone());
        let read = worker
            .frontmatter
            .permissions
            .get(&PermissionAction::Read)
            .unwrap();
        assert_eq!(
            read.rules(PermissionAction::Read)[0].resource.as_str(),
            "${workspace_dir}/src/*"
        );
    }
    assert_eq!(fingerprints[0], fingerprints[1]);
}

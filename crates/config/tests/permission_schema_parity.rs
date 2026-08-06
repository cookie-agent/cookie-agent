use std::fs;

use cookie_agent_config::{AgentFrontmatter, ConfigError, PermissionRule, load_from_roots};
use tempfile::TempDir;

fn config_rule(id: &str, resource: &str) -> Result<PermissionRule, serde_yaml::Error> {
    let id = serde_json::to_string(id).unwrap();
    let resource = serde_json::to_string(resource).unwrap();
    serde_yaml::from_str(&format!(
        "id: {id}\naction: read\nresource: {resource}\neffect: allow\n"
    ))
}

fn protocol_rule(
    id: &str,
    resource: &str,
) -> Result<cookie_agent_protocol::PermissionRule, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "action": "read",
        "resource": resource,
        "effect": "allow"
    }))
}

#[test]
fn config_and_protocol_reject_the_same_adversarial_permission_values() {
    let three_byte_boundary = "界".repeat(1365);
    let three_byte_overflow = "界".repeat(1366);
    let four_byte_boundary = "😀".repeat(1024);
    let four_byte_overflow = "😀".repeat(1025);
    let cases = [
        ("allow-read", "*", true),
        (&"a".repeat(128), "資料/?", true),
        ("allow-read", three_byte_boundary.as_str(), true),
        ("allow-read", three_byte_overflow.as_str(), false),
        ("allow-read", four_byte_boundary.as_str(), true),
        ("allow-read", four_byte_overflow.as_str(), false),
        ("Allow-read", "*", false),
        ("-allow-read", "*", false),
        (&"a".repeat(129), "*", false),
        ("allow-read", "", false),
        ("allow-read", "**", false),
        ("allow-read", "a**b", false),
        ("allow-read", r"path\\*", false),
        ("allow-read", "[ab]", false),
        ("allow-read", "{a,b}", false),
        ("allow-read", "line\nfeed", false),
        ("allow-read", &"a".repeat(4097), false),
    ];

    for (id, resource, expected) in cases {
        let config_accepts = config_rule(id, resource).is_ok();
        let protocol_accepts = protocol_rule(id, resource).is_ok();
        assert_eq!(
            config_accepts, expected,
            "config id={id:?} resource={resource:?}"
        );
        assert_eq!(
            protocol_accepts, expected,
            "protocol id={id:?} resource={resource:?}"
        );
    }
}

#[test]
fn invalid_safe_code_rule_id_fails_during_configuration_load() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join(".cookie-agent");
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(
        root.join("agents/worker.md"),
        "---\nschema: 1\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\ntools: []\npermissions:\n  - { id: Invalid, action: read, resource: \"*\", effect: allow }\n---\nWorker.\n",
    )
    .unwrap();
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::AgentFrontmatter(_))
    ));
}

#[test]
fn frontmatter_uses_shared_permission_types() {
    let frontmatter: AgentFrontmatter = serde_yaml::from_str(
        "schema: 1\ndescription: Worker\nmode: subagent\nenabled: true\nmodel_fallback: []\ntools: []\npermissions:\n  - { id: allow-read, action: read, resource: \"file?.rs\", effect: allow }\n",
    )
    .unwrap();
    let rule = &frontmatter.permissions[0];
    assert_eq!(rule.id.as_str(), "allow-read");
    assert_eq!(rule.resource.as_str(), "file?.rs");
}

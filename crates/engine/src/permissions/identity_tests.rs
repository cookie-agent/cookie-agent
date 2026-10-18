use cookie_agent_protocol::{
    ApprovalBoundary, AttachmentName, CapabilityName, Open, OperationFingerprint, PermissionAction,
    PreparedBindingLifetime, RuntimeId, ToolIdentity, ToolSource,
};
use serde_json::json;

use super::declared_operation;

fn runtime_tool(runtime: &str, name: &str) -> ToolIdentity {
    ToolIdentity {
        name: name.to_owned(),
        published_name: name.to_owned(),
        source: Some(Open::Known(ToolSource::Runtime {
            runtime_id: RuntimeId::new(runtime).unwrap(),
            attachment: AttachmentName::new(format!("{runtime}_repo")).unwrap(),
            capability: CapabilityName::coding(),
        })),
    }
}

fn plugin_tool(plugin: &str, name: &str) -> ToolIdentity {
    ToolIdentity {
        name: name.to_owned(),
        published_name: format!("{plugin}_{name}"),
        source: Some(Open::Known(ToolSource::Plugin {
            plugin: plugin.to_owned(),
        })),
    }
}

fn fingerprint(
    tool: &ToolIdentity,
    permission: Option<&PermissionAction>,
    resource: Option<&str>,
    arguments: &serde_json::Value,
) -> OperationFingerprint {
    OperationFingerprint::from_prepared_operation(
        &declared_operation(tool, b"/work/repo", permission, resource, arguments).unwrap(),
    )
}

#[test]
fn a_declared_call_has_one_identity_whatever_order_its_arguments_arrive_in() {
    let tool = runtime_tool("local", "webfetch");
    let action = PermissionAction::WEBFETCH;
    let first = fingerprint(
        &tool,
        Some(&action),
        Some("https://example.com"),
        &json!({"url": "https://example.com", "format": "text"}),
    );
    let reordered: serde_json::Value =
        serde_json::from_str(r#"{"format":"text","url":"https://example.com"}"#).unwrap();
    assert_eq!(
        first,
        fingerprint(
            &tool,
            Some(&action),
            Some("https://example.com"),
            &reordered
        )
    );
    assert_ne!(
        first,
        fingerprint(
            &tool,
            Some(&action),
            Some("https://example.org"),
            &json!({"url": "https://example.org", "format": "text"}),
        ),
        "another resource is another operation"
    );
    assert_ne!(
        first,
        fingerprint(
            &tool,
            Some(&action),
            Some("https://example.com"),
            &json!({"url": "https://example.com", "format": "html"}),
        ),
        "other arguments are another operation"
    );
}

/// What a peer declares is bound to the tool the daemon resolved: a plugin
/// or another runtime that declares exactly another tool's permission,
/// resource, and arguments still gets an identity of its own, so no grant
/// for one tool ever matches another's call.
#[test]
fn a_peer_cannot_state_another_tools_identity() {
    let action = PermissionAction::new("issue_read").unwrap();
    let arguments = json!({"issue": "42"});
    let identity = |tool: &ToolIdentity| {
        declared_operation(
            tool,
            b"runtime:global/global",
            Some(&action),
            Some("42"),
            &arguments,
        )
        .unwrap()
    };
    let tools = [
        plugin_tool("issues", "read_issue"),
        plugin_tool("mirror", "read_issue"),
        plugin_tool("issues", "read_issue_v2"),
        runtime_tool("local", "read_issue"),
        runtime_tool("labpc", "read_issue"),
        ToolIdentity {
            source: Some(Open::Known(ToolSource::Mcp {
                server: "issues".into(),
                runtime_id: Some(RuntimeId::new("local").unwrap()),
            })),
            ..runtime_tool("local", "read_issue")
        },
    ];
    let identities = tools.iter().map(identity).collect::<Vec<_>>();
    for (index, left) in identities.iter().enumerate() {
        for right in &identities[index + 1..] {
            assert_ne!(
                OperationFingerprint::from_prepared_operation(left),
                OperationFingerprint::from_prepared_operation(right)
            );
            assert_ne!(
                left.resources()[0].binding_digest,
                right.resources()[0].binding_digest
            );
        }
    }
    // The published name is the model's; the identity names the source's.
    let renamed = ToolIdentity {
        published_name: "read_issue".into(),
        ..plugin_tool("issues", "read_issue")
    };
    assert_eq!(identity(&renamed), identities[0]);
}

/// The built-in tools keep their approval semantics: commands approve by
/// prefix and, like paths, only for the process; everything else is exact
/// and restart-stable, so it may be granted for a session tree.
#[test]
fn commands_and_paths_bind_to_the_process_and_other_resources_survive_restarts() {
    let tool = runtime_tool("local", "bash");
    let bash = declared_operation(
        &tool,
        b"/work/repo",
        Some(&PermissionAction::BASH),
        Some("cargo test"),
        &json!({"command": "cargo test", "timeout": 120000}),
    )
    .unwrap();
    let resource = &bash.resources()[0];
    assert_eq!(resource.capability, PermissionAction::BASH);
    assert_eq!(
        resource.boundary,
        ApprovalBoundary::CommandPrefix {
            prefix: "cargo test".into()
        }
    );
    assert_eq!(
        resource.binding_lifetime,
        PreparedBindingLifetime::ProcessLocal
    );
    assert_eq!(bash.capabilities()[0].operation.as_str(), "bash:call");

    for action in [PermissionAction::READ, PermissionAction::WRITE] {
        let operation = declared_operation(
            &runtime_tool("local", "write"),
            b"/work/repo",
            Some(&action),
            Some("src/main.rs"),
            &json!({"filePath": "/work/repo/src/main.rs"}),
        )
        .unwrap();
        assert_eq!(operation.resources()[0].boundary, ApprovalBoundary::Exact);
        assert_eq!(
            operation.resources()[0].binding_lifetime,
            PreparedBindingLifetime::ProcessLocal
        );
    }
    for action in [
        PermissionAction::WEBFETCH,
        PermissionAction::MCP,
        PermissionAction::new("browse").unwrap(),
    ] {
        let operation = declared_operation(
            &runtime_tool("local", "fetch"),
            b"/work/repo",
            Some(&action),
            Some("resource"),
            &json!({}),
        )
        .unwrap();
        assert_eq!(operation.resources()[0].boundary, ApprovalBoundary::Exact);
        assert_eq!(
            operation.resources()[0].binding_lifetime,
            PreparedBindingLifetime::RestartStable
        );
    }
}

#[test]
fn a_call_without_a_permission_or_resource_is_still_identified() {
    let operation = declared_operation(
        &plugin_tool("notes", "Take Note!"),
        b"runtime:global/global",
        None,
        None,
        &json!({}),
    )
    .unwrap();
    assert_eq!(operation.capabilities()[0].action.as_str(), "tool");
    assert_eq!(
        operation.capabilities()[0].operation.as_str(),
        "take_note_:call"
    );
    assert_eq!(operation.resources()[0].canonical.as_str(), "tool:unscoped");
    let scoped = declared_operation(
        &plugin_tool("notes", "_hidden"),
        b"runtime:global/global",
        Some(&PermissionAction::new("note").unwrap()),
        Some("today"),
        &json!({}),
    )
    .unwrap();
    assert_eq!(
        scoped.capabilities()[0].operation.as_str(),
        "tool_hidden:call"
    );
    assert!(
        scoped.resources()[0]
            .canonical
            .as_str()
            .starts_with("note:")
    );
}

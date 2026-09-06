use std::{fs, path::Path, process::Command};

use cookie_agent_engine::{
    PreparedTool, ToolCall, ToolExecutionContext, ToolPreparationContext, ToolProvider,
    permissions::PermissionPipeline,
};
use cookie_agent_protocol::{
    AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot, PermissionAction,
    PermissionEffect, PermissionRule, RunId, SessionId, Sha256Digest, ToolCallId, WildcardPattern,
};

use crate::{edit::EditTool, read::ReadTool, write::WriteTool};

#[test]
fn file_tools_expand_home_in_isolated_process() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "path_args::tool_tests::home_path_subprocess_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("HOME", home.path())
        .env("COOKIE_AGENT_HOME_PATH_TEST", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "home-path subprocess failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn context(workspace: &Path) -> ToolPreparationContext {
    ToolPreparationContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        cwd: workspace.to_owned(),
        workspace_root: workspace.to_owned(),
        turn_context: crate::test_turn_context(),
    }
}

fn call(name: &str, path: &str) -> ToolCall {
    let arguments = match name {
        "read" => serde_json::json!({"filePath": path}),
        "write" => serde_json::json!({"filePath": path, "content": "original"}),
        "edit" => {
            serde_json::json!({"filePath": path, "oldString": "original", "newString": "edited"})
        }
        _ => unreachable!(),
    };
    ToolCall {
        id: ToolCallId::new_v7(),
        name: name.into(),
        arguments,
    }
}

fn assert_permission(
    prepared: &PreparedTool,
    workspace: &Path,
    allowed: &str,
    effect: PermissionEffect,
) {
    let action = prepared.operation().capabilities()[0].action;
    let policy = AgentSnapshot {
        agent: AgentId::new("test").unwrap(),
        schema: AgentSchemaVersion::current(),
        mode: AgentMode::Primary,
        description: "Test agent".into(),
        document_source: AgentDocumentSource::Workspace,
        document_fingerprint: Sha256Digest::of_bytes(b"test"),
        composed_prompt: "Test permissions".into(),
        prompt_fingerprint: Sha256Digest::of_bytes(b"test"),
        max_output_tokens: 0,
        permissions: vec![
            PermissionRule {
                action,
                resource: WildcardPattern::new("*").unwrap(),
                effect: PermissionEffect::Deny,
            },
            PermissionRule {
                action,
                resource: WildcardPattern::new(allowed).unwrap(),
                effect: PermissionEffect::Allow,
            },
        ],
        delegation: None,
        fallback_chain: Vec::new(),
        selected_suffix_start: 0,
    };
    let decision = PermissionPipeline::default().decide_operation(
        &policy,
        prepared.operation(),
        prepared.policy_labels(),
        workspace,
    );
    assert_eq!(decision.effect, effect);
}

#[tokio::test]
#[ignore = "subprocess helper with isolated HOME"]
async fn home_path_subprocess_helper() {
    if std::env::var_os("COOKIE_AGENT_HOME_PATH_TEST").is_none() {
        return;
    }
    let home = cookie_agent_protocol::paths::home_dir().unwrap();
    assert_eq!(
        home,
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
    );
    let workspace = home.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let providers: [(&str, Box<dyn ToolProvider>); 3] = [
        ("write", Box::new(WriteTool::new(&workspace))),
        ("read", Box::new(ReadTool::new(&workspace))),
        ("edit", Box::new(EditTool::new(&workspace))),
    ];

    for path in [
        "~/workspace/value",
        "value",
        workspace.join("value").to_str().unwrap(),
    ] {
        for (name, provider) in &providers {
            let request = call(name, path);
            assert_eq!(
                provider
                    .get_permission_resource(name, &request.arguments)
                    .unwrap()
                    .1
                    .as_deref(),
                Some("value")
            );
            let prepared = provider
                .prepare(context(&workspace), request)
                .await
                .unwrap();
            assert_eq!(
                prepared.normalized_arguments()["filePath"],
                workspace.join("value").to_str().unwrap()
            );
            assert_eq!(prepared.policy_labels(), [Some("value".into())]);
            assert_permission(&prepared, &workspace, "value", PermissionEffect::Allow);
            let result = prepared
                .execute_for_test(
                    ToolExecutionContext::for_test(
                        workspace.join("artifacts"),
                        crate::test_turn_context(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            if *name == "read" {
                assert!(result.output.contains("original"));
            }
        }
        assert_eq!(
            fs::read_to_string(workspace.join("value")).unwrap(),
            "edited"
        );
    }

    fs::write(home.join("outside"), "original").unwrap();
    for (name, provider) in &providers {
        let request = call(name, "~/outside");
        let expected = home.join("outside").to_str().unwrap().to_owned();
        assert_eq!(
            provider
                .get_permission_resource(name, &request.arguments)
                .unwrap()
                .1,
            Some(expected.clone())
        );
        let prepared = provider
            .prepare(context(&workspace), request)
            .await
            .unwrap();
        assert_eq!(prepared.policy_labels(), [Some(expected)]);
        assert_permission(&prepared, &workspace, "value", PermissionEffect::Deny);

        let invalid = call(name, "~nosuchuser/x");
        assert!(
            provider
                .get_permission_resource(name, &invalid.arguments)
                .unwrap_err()
                .message()
                .contains("~otheruser expansion is not supported")
        );
        let error = provider
            .prepare(context(&workspace), invalid)
            .await
            .err()
            .expect("invalid tilde path");
        assert!(
            error
                .message()
                .contains("~otheruser expansion is not supported")
        );
    }
    assert_eq!(
        fs::read_to_string(home.join("outside")).unwrap(),
        "original"
    );

    let prepared = ReadTool::new(&workspace)
        .prepare(context(&workspace), call("read", "~"))
        .await
        .unwrap();
    assert_eq!(
        prepared.normalized_arguments()["filePath"],
        home.to_str().unwrap()
    );
    let result = prepared
        .execute_for_test(
            ToolExecutionContext::for_test(workspace.join("artifacts"), crate::test_turn_context())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(result.output.contains("workspace/"));

    std::os::unix::fs::symlink(home.join("outside"), workspace.join("link")).unwrap();
    for (name, provider) in &providers {
        let prepared = provider
            .prepare(context(&workspace), call(name, "~/workspace/link"))
            .await
            .unwrap();
        assert_eq!(prepared.policy_labels(), [Some("link".into())]);
        assert_permission(&prepared, &workspace, "value", PermissionEffect::Deny);
        crate::assert_workspace_rule_allows(
            &prepared,
            &workspace,
            if *name == "read" {
                PermissionAction::Read
            } else {
                PermissionAction::Write
            },
            "link",
        );
        prepared
            .execute_for_test(
                ToolExecutionContext::for_test(
                    workspace.join("artifacts"),
                    crate::test_turn_context(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            fs::symlink_metadata(workspace.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    assert_eq!(fs::read_to_string(home.join("outside")).unwrap(), "edited");
}

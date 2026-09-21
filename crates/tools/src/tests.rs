use cookie_agent_engine::{
    SessionToolContext, ToolCall, ToolError, ToolPreparationContext, ToolProvider,
};
use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, OperationFingerprint,
    PermissionAction, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, RunId, SessionId, Sha256Digest, ToolCallId,
};

use super::{
    BuiltinTools, bash::BashTool, delegate::DelegateToolProvider, edit::EditTool, goal::GoalTools,
    read::ReadTool, write::WriteTool,
};

#[test]
fn static_permission_names_cover_every_builtin_and_delegate_tool() {
    assert_eq!(ReadTool::get_permission_name("read").unwrap(), "read");
    assert_eq!(WriteTool::get_permission_name("write").unwrap(), "write");
    assert_eq!(EditTool::get_permission_name("edit").unwrap(), "write");
    assert_eq!(BashTool::get_permission_name("bash").unwrap(), "bash");
    assert_eq!(
        BuiltinTools::get_permission_name("webfetch").unwrap(),
        "webfetch"
    );
    assert_eq!(
        super::skill::SkillTool::get_permission_name("skill").unwrap(),
        "skill"
    );
    assert_eq!(GoalTools::get_permission_name("goal_get").unwrap(), "read");
    assert_eq!(
        GoalTools::get_permission_name("goal_update").unwrap(),
        "write"
    );
    assert!(BuiltinTools::get_permission_name("read_tool_result").is_err());
    for name in [
        "delegate_subagent",
        "get_subagent_result",
        "cancel_subagent",
    ] {
        assert_eq!(
            DelegateToolProvider::get_permission_name(name).unwrap(),
            "delegate"
        );
    }
    assert!(BuiltinTools::get_permission_name("grep").is_err());
}

#[test]
fn self_paginating_tools_declare_truncation_opt_out() {
    let read = ReadTool::new("/tmp")
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .unwrap()
        .remove(0);
    assert_eq!(
        read.result_truncation,
        cookie_agent_engine::ToolResultTruncationPolicy::OptOut
    );
    assert_eq!(
        super::delegate::result_truncation_policy("get_subagent_result"),
        cookie_agent_engine::ToolResultTruncationPolicy::OptOut
    );
}

#[test]
fn builtin_tools_do_not_expose_grep_or_glob() {
    let tools = BuiltinTools::new("/tmp");
    let names = tools
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .expect("built-in tools")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["read", "write", "edit", "bash", "webfetch"]);
    for name in ["grep", "glob"] {
        let arguments = serde_json::json!({});
        assert!(
            matches!(
                tools.get_permission_resource(name, &arguments),
                Err(ToolError::Failed(_))
            ),
            "{name} permission resource"
        );
        assert!(
            matches!(
                tools.get_display_argument(name, &arguments),
                Err(ToolError::Failed(_))
            ),
            "{name} display"
        );
    }
}

#[tokio::test]
async fn unknown_grep_and_glob_calls_fail_closed() {
    let tools = super::BuiltinTools::new("/tmp");
    let context = ToolPreparationContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        cwd: "/tmp".into(),
        workspace_root: "/tmp".into(),
        turn_context: crate::test_turn_context(),
    };
    for name in ["grep", "glob"] {
        let result = tools
            .prepare(
                context.clone(),
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: name.into(),
                    arguments: serde_json::json!({"pattern":"TODO"}),
                },
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::Failed(message)) if message.contains("unknown built-in tool")),
            "{name}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn builtin_relative_paths_revalidate_a_symlinked_cwd_route() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("root");
    let destination = root.path().join("destination");
    std::fs::create_dir(&destination).expect("destination");
    std::fs::write(destination.join("value.txt"), "old").expect("fixture");
    let cwd = root.path().join("cwd");
    symlink(&destination, &cwd).expect("cwd symlink");
    let context = ToolPreparationContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        cwd: cwd.clone(),
        workspace_root: root.path().to_owned(),
        turn_context: crate::test_turn_context(),
    };
    let tools = BuiltinTools::new(root.path());
    let mut prepared = Vec::new();
    for (name, arguments) in [
        ("read", serde_json::json!({"filePath":"value.txt"})),
        (
            "write",
            serde_json::json!({"filePath":"value.txt","content":"new"}),
        ),
        (
            "edit",
            serde_json::json!({
                "filePath":"value.txt",
                "oldString":"old",
                "newString":"new"
            }),
        ),
    ] {
        prepared.push(
            tools
                .prepare(
                    context.clone(),
                    ToolCall {
                        id: ToolCallId::new_v7(),
                        name: name.into(),
                        arguments,
                    },
                )
                .await
                .expect("prepare through symlinked cwd"),
        );
    }

    std::fs::remove_file(&cwd).expect("remove cwd route");
    symlink(&destination, &cwd).expect("replace cwd route");
    for prepared in prepared {
        let error = prepared
            .execute_for_test(
                cookie_agent_engine::ToolExecutionContext::for_test(
                    root.path().join("artifacts"),
                    crate::test_turn_context(),
                )
                .expect("execution context"),
            )
            .await
            .expect_err("cwd route swap must fail");
        assert!(matches!(error, ToolError::OperationChanged(_)));
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn builtin_parallel_edits_to_the_same_file_chain() {
    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("value.txt"), "alpha beta").expect("fixture");
    let context = ToolPreparationContext {
        session: SessionId::new_v7(),
        run: RunId::new_v7(),
        cwd: root.path().to_owned(),
        workspace_root: root.path().to_owned(),
        turn_context: crate::test_turn_context(),
    };
    let tools = BuiltinTools::new(root.path());
    let call = |old: &str, new: &str| ToolCall {
        id: ToolCallId::new_v7(),
        name: "edit".into(),
        arguments: serde_json::json!({"filePath":"value.txt","oldString":old,"newString":new}),
    };
    // The production wiring registers BuiltinTools as one provider, so
    // batch preparation must route through it to reach the edit chain.
    let prepared = tools
        .prepare_parallel(context, vec![call("alpha", "ALPHA"), call("beta", "BETA")])
        .await;
    let mut prepared = prepared.into_iter();
    let first = prepared.next().expect("first").expect("first edit");
    let second = prepared.next().expect("second").expect("second edit");
    assert!(prepared.next().is_none());
    let execution = || {
        cookie_agent_engine::ToolExecutionContext::for_test(
            root.path().join("artifacts"),
            crate::test_turn_context(),
        )
        .expect("execution context")
    };
    first
        .execute_for_test(execution())
        .await
        .expect("first edit");
    second
        .execute_for_test(execution())
        .await
        .expect("chained edit");
    assert_eq!(
        std::fs::read_to_string(root.path().join("value.txt")).unwrap(),
        "ALPHA BETA"
    );
}

#[test]
fn fixed_identity_v7_fingerprint_is_golden_and_deterministic() {
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"normalized arguments without raw paths"),
        vec![ApprovalCapability {
            action: PermissionAction::Bash,
            operation: PreparedCapabilityOperation::new("execute").expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: PermissionAction::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status").expect("identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                b"executable-content-and-open-directory-binding",
            ),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"execution context"),
    )
    .expect("prepared operation");
    let fingerprint = OperationFingerprint::from_prepared_operation(&operation);
    assert_eq!(
        fingerprint.digest().as_str(),
        "80b953e606ab07a7d25bde074c9d2086012b3b86a62ead3b3751e7cc5f5cbffd"
    );
    assert_eq!(
        fingerprint,
        OperationFingerprint::from_prepared_operation(&operation)
    );
}

#[test]
fn abbreviated_display_path_prefers_workspace_then_home() {
    let workspace = tempfile::tempdir().expect("workspace");
    assert_eq!(
        super::abbreviated_display_path("src/lib.rs", workspace.path()),
        "src/lib.rs"
    );
    assert_eq!(
        super::abbreviated_display_path(
            &workspace.path().join("src/lib.rs").to_string_lossy(),
            workspace.path()
        ),
        "src/lib.rs"
    );
    assert_eq!(
        super::abbreviated_display_path(&workspace.path().to_string_lossy(), workspace.path()),
        "."
    );
    let home = cookie_agent_protocol::paths::home_dir().expect("home directory");
    assert_eq!(
        super::abbreviated_display_path(&home.to_string_lossy(), workspace.path()),
        "~"
    );
}

#[cfg(unix)]
#[test]
fn unix_display_path_comparison_remains_lexical_for_symlinked_workspaces() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("temporary root");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let alias = directory.path().join("workspace-alias");
    symlink(&workspace, &alias).expect("workspace symlink");

    let canonical_child = workspace.join("src/lib.rs");
    assert_eq!(
        super::abbreviated_display_path(&canonical_child.to_string_lossy(), &alias),
        super::normalized_path(&canonical_child),
    );
    let lexical_child = alias.join("src/lib.rs");
    assert_eq!(
        super::abbreviated_display_path(&lexical_child.to_string_lossy(), &alias),
        "src/lib.rs",
    );
}

#[test]
fn relative_display_path_is_returned_before_workspace_comparison() {
    assert_eq!(
        super::abbreviated_display_path("workspace/src/lib.rs", std::path::Path::new("workspace"),),
        "workspace/src/lib.rs",
    );
}

#[cfg(windows)]
fn windows_short_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let required = unsafe { GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0) };
    assert_ne!(
        required,
        0,
        "GetShortPathNameW: {}",
        std::io::Error::last_os_error()
    );
    let mut short = vec![0; required as usize];
    let length = unsafe { GetShortPathNameW(wide.as_ptr(), short.as_mut_ptr(), required) };
    assert!(length > 0 && length < required);
    std::ffi::OsString::from_wide(&short[..length as usize]).into()
}

#[cfg(windows)]
#[tokio::test]
async fn windows_short_workspace_spelling_roundtrips_prepared_resources() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace with a long name");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("value.txt"), "value").expect("fixture");
    let long = workspace.canonicalize().expect("long workspace spelling");
    let short = windows_short_path(&long);
    let tool = ReadTool::new(&long);
    let prepared = tool
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                cwd: short.clone(),
                workspace_root: long.clone(),
                turn_context: crate::test_turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: serde_json::json!({"filePath":"value.txt"}),
            },
        )
        .await
        .expect("prepare through short cwd");
    assert_eq!(prepared.operation().resources().len(), 1);
    assert_eq!(prepared.policy_labels(), [Some("value.txt".into())]);
    super::assert_workspace_rule_allows(&prepared, &long, PermissionAction::Read, "value.txt");
    assert_eq!(
        tool.get_permission_resource("read", prepared.normalized_arguments())
            .unwrap(),
        ("read", Some("value.txt".into()))
    );
    for (requested, workspace) in [(&short, &long), (&long, &short)] {
        assert_eq!(
            super::permission_path_label(
                &requested.join("missing/file.txt").to_string_lossy(),
                workspace
            ),
            "missing/file.txt"
        );
        assert_eq!(
            super::abbreviated_display_path(
                &requested.join("missing/file.txt").to_string_lossy(),
                workspace
            ),
            "missing/file.txt"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_permission_path_label_does_not_resolve_requested_path() {
    let directory = tempfile::tempdir().expect("temporary root");
    let workspace = directory.path().join("workspace");
    let destination = directory.path().join("destination");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&destination).expect("destination");
    let alias = workspace.join("alias");
    if let Err(error) = std::os::windows::fs::symlink_dir(&destination, &alias) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("create directory symlink: {error}");
    }
    let requested = alias.join("value.txt");

    assert_eq!(
        super::permission_path_label(&requested.to_string_lossy(), &workspace),
        "alias/value.txt"
    );
    assert_eq!(
        super::abbreviated_display_path(&requested.to_string_lossy(), &workspace),
        "alias/value.txt"
    );
    let outside_alias = directory.path().join("outside-alias");
    std::os::windows::fs::symlink_dir(&workspace, &outside_alias).expect("outside alias");
    let outside_request = outside_alias.join("value.txt");
    assert_eq!(
        super::permission_path_label(&outside_request.to_string_lossy(), &workspace),
        super::normalized_path(&outside_request)
    );
    let parent_display =
        super::abbreviated_display_path(&directory.path().to_string_lossy(), &workspace);
    assert_eq!(
        super::abbreviated_display_path(&outside_request.to_string_lossy(), &workspace),
        format!("{parent_display}/outside-alias/value.txt")
    );
    let destination_file = destination.join("a long destination filename.txt");
    std::fs::write(&destination_file, "external").expect("external fixture");
    let short_file = windows_short_path(&destination_file);
    let suffix = short_file.file_name().expect("short leaf");
    let requested = windows_short_path(&workspace).join("alias").join(suffix);
    let expected = format!("alias/{}", suffix.to_string_lossy());
    assert_eq!(
        super::permission_path_label(&requested.to_string_lossy(), &workspace),
        expected
    );
}

#[cfg(windows)]
#[test]
fn windows_display_paths_hide_verbatim_prefixes() {
    assert_eq!(
        super::normalized_path(std::path::Path::new(r"\\?\C:\Users\runneradmin\file.txt")),
        "C:/Users/runneradmin/file.txt"
    );
    assert_eq!(
        super::normalized_path(std::path::Path::new(r"\\?\UNC\server\share\file.txt")),
        "//server/share/file.txt"
    );
}

#[test]
fn absent_write_v7_fingerprint_is_golden() {
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"write arguments with content digest"),
        vec![ApprovalCapability {
            action: PermissionAction::Write,
            operation: PreparedCapabilityOperation::new("write:replace").expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: PermissionAction::Write,
            canonical: PreparedResourceIdentity::new("file:expected-absent").expect("identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                b"anchor-identity\0parent-identity\0basename\0expected-absent\0new-content",
            ),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"held workspace identity"),
    )
    .expect("prepared operation");
    assert_eq!(
        OperationFingerprint::from_prepared_operation(&operation)
            .digest()
            .as_str(),
        "7c3f201fb116f919296a492d3f64b8516bf70adf9cd7202d744d83173a4f4c11"
    );
}

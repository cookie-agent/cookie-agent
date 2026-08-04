#![cfg(unix)]

use std::{
    ffi::CString,
    fs,
    os::unix::{fs::PermissionsExt as _, net::UnixListener},
    path::Path,
};

use cookie_agent_config::{
    AgentRegistry, ConfigError, PermissionAction, PermissionEffect, load_from_roots,
    simple_wildcard_match,
};
use cookie_agent_identity::{AgentId, ProviderId};
use cookie_agent_models::{Catalog, build_model_set};
use tempfile::TempDir;

fn runtime() -> &'static str {
    r#"schema_version = 6

[providers.test]
source = "explicit"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"
auth = { type = "none" }

[providers.test.models."model-one"]
display_name = "Model One"
default_variant = "high"

[providers.test.models."model-one".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 16384
output_tokens = 4096
tool_calling = true
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = true
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {}

[providers.test.models."model-one".variants.high]
operation = "add"
defaults = { temperature = 0.4 }

[providers.test.models."model-one".variants.default]
operation = "add"
defaults = { temperature = 0.2 }
"#
}

fn agent(mode: &str, fallback: &str, delegation: &str, body: &str) -> String {
    format!(
        r#"---
schema: 1
description: Test agent
mode: {mode}
enabled: true
model_fallback: {fallback}
tools: [read, grep, glob]
permissions:
  - {{ id: allow-read, action: read, resource: "*", effect: allow }}
{delegation}---
{body}
"#
    )
}

fn write_layer(root: &Path, config: &str, agents: &[(&str, String)]) {
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(root.join("config.toml"), config).unwrap();
    for (name, contents) in agents {
        fs::write(root.join("agents").join(name), contents).unwrap();
    }
}

#[test]
fn schema6_and_markdown_agents_resolve_omitted_base_and_named_default_exactly() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join(".cookie-agent");
    write_layer(
        &root,
        runtime(),
        &[
            (
                "primary.md",
                agent(
                    "primary",
                    "[{ model: \"test/model-one\" }]",
                    "delegation:\n  agents: [worker]\n  max_depth: 2\n",
                    "Primary prompt.",
                ),
            ),
            (
                "base.md",
                agent(
                    "all",
                    "[{ model: \"test/model-one\", variant: base }]",
                    "",
                    "Base prompt.",
                ),
            ),
            (
                "named.md",
                agent(
                    "all",
                    "[{ model: \"test/model-one\", variant: default }]",
                    "",
                    "Named prompt.",
                ),
            ),
            ("worker.md", agent("subagent", "[]", "", "Worker prompt.")),
        ],
    );
    let loaded = load_from_roots(None, Some(&root)).unwrap();
    let catalog = Catalog::embedded().unwrap();
    let models = build_model_set(&loaded.runtime.providers, &catalog, None).unwrap();
    let registry = loaded.resolve_agents(&models).unwrap();
    let get = |name: &str| registry.get(&AgentId::new(name).unwrap()).unwrap();
    assert_eq!(
        get("primary").resolved_fallback[0]
            .variant
            .as_ref()
            .unwrap()
            .as_str(),
        "high"
    );
    assert!(get("base").resolved_fallback[0].variant.is_none());
    assert_eq!(
        get("named").resolved_fallback[0]
            .variant
            .as_ref()
            .unwrap()
            .as_str(),
        "default"
    );
    assert!(get("primary").runnable_as_root);
    assert!(!get("worker").runnable_as_root);
    assert_eq!(get("primary").document.body, "Primary prompt.\n");
    assert_ne!(
        get("primary").document.document_fingerprint,
        get("primary").document.prompt_fingerprint
    );
}

#[test]
fn provider_and_agent_layers_replace_atomically_by_id() {
    let temp = TempDir::new().unwrap();
    let user = temp.path().join("user");
    let workspace = temp.path().join("workspace");
    write_layer(
        &user,
        runtime(),
        &[(
            "primary.md",
            agent("primary", "[{ model: \"test/model-one\" }]", "", "User."),
        )],
    );
    fs::set_permissions(&user, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(user.join("agents"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(user.join("config.toml"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(
        user.join("agents/primary.md"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let workspace_runtime =
        runtime().replace("https://example.test/v1", "https://workspace.test/v1");
    write_layer(
        &workspace,
        &workspace_runtime,
        &[(
            "primary.md",
            agent(
                "primary",
                "[{ model: \"test/model-one\" }]",
                "",
                "Workspace.",
            ),
        )],
    );
    let loaded = load_from_roots(Some(&user), Some(&workspace)).unwrap();
    assert_eq!(
        loaded.agents[&AgentId::new("primary").unwrap()].body,
        "Workspace.\n"
    );
    let provider = &loaded.runtime.providers[&ProviderId::new("test").unwrap()];
    assert!(format!("{provider:?}").contains("workspace.test"));
}

#[test]
fn old_top_level_paths_and_unsafe_objects_fail_closed() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("bad");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("config.toml"),
        "schema_version = 6\nproviders = {}\nagents = {}\n",
    )
    .unwrap();
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::Toml(_))
    ));

    let linked = temp.path().join("linked");
    std::os::unix::fs::symlink(&root, &linked).unwrap();
    assert!(matches!(
        load_from_roots(None, Some(&linked)),
        Err(ConfigError::UnsafePath)
    ));
}

#[test]
fn wildcard_grammar_has_terminal_space_star_behavior() {
    assert!(simple_wildcard_match("git status *", "git status"));
    assert!(simple_wildcard_match("*", "nested/path"));
    assert!(simple_wildcard_match("file?.rs", "file1.rs"));
    assert!(!simple_wildcard_match("file?.rs", "file12.rs"));
}

#[test]
fn checked_agent_fixtures_protect_root_and_nested_secret_labels() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join(".cookie-agent");
    fs::create_dir_all(root.join("agents")).unwrap();
    fs::write(root.join("config.toml"), runtime()).unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for name in [
        "anthropic.md",
        "chat.md",
        "primary.md",
        "responses.md",
        "worker.md",
    ] {
        fs::copy(
            workspace.join(".cookie-agent/agents").join(name),
            root.join("agents").join(name),
        )
        .unwrap();
    }
    let loaded = load_from_roots(None, Some(&root)).unwrap();
    let effect = |agent: &cookie_agent_config::AgentDocument, resource: &str| {
        agent
            .frontmatter
            .permissions
            .iter()
            .rfind(|rule| {
                rule.action == PermissionAction::Read
                    && simple_wildcard_match(rule.resource.as_str(), resource)
            })
            .map(|rule| rule.effect)
    };
    for agent in loaded.agents.values() {
        for resource in [
            ".env",
            "nested/.env",
            ".env.local",
            "nested/.env.local",
            "store-v1.json",
            "nested/store-v1.json",
            "token-v1",
            "nested/token-v1",
            "id_ed25519",
            "nested/id_ed25519",
            ".netrc",
            "nested/.netrc",
            "application_default_credentials.json",
            "nested/application_default_credentials.json",
        ] {
            assert_eq!(
                effect(agent, resource),
                Some(PermissionEffect::Deny),
                "{} did not deny {resource}",
                agent.id
            );
        }
        for resource in [".env.example", "nested/.env.example"] {
            assert_eq!(
                effect(agent, resource),
                Some(PermissionEffect::Allow),
                "{} did not allow {resource}",
                agent.id
            );
        }
    }
}

#[test]
fn duplicate_model_keys_and_ineligible_delegation_fail_registry_validation() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("layer");
    write_layer(
        &root,
        runtime(),
        &[(
            "primary.md",
            agent(
                "primary",
                "[{ model: \"test/model-one\" }, { model: \"test/model-one\", variant: base }]",
                "",
                "Prompt.",
            ),
        )],
    );
    let loaded = load_from_roots(None, Some(&root)).unwrap();
    let models = build_model_set(
        &loaded.runtime.providers,
        &Catalog::embedded().unwrap(),
        None,
    )
    .unwrap();
    assert!(matches!(
        AgentRegistry::resolve(loaded.agents, &models),
        Err(ConfigError::DuplicateFallbackModel { .. })
    ));
}

#[test]
fn links_hardlinks_private_modes_yaml_aliases_and_agent_interpolation_fail_closed() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("target.toml");
    fs::write(&target, runtime()).unwrap();
    let linked_root = temp.path().join("linked-root");
    fs::create_dir(&linked_root).unwrap();
    std::os::unix::fs::symlink(&target, linked_root.join("config.toml")).unwrap();
    assert!(matches!(
        load_from_roots(None, Some(&linked_root)),
        Err(ConfigError::UnsafePath)
    ));

    let root = temp.path().join("hardlink-root");
    write_layer(
        &root,
        runtime(),
        &[(
            "primary.md",
            agent("primary", "[{ model: \"test/model-one\" }]", "", "Prompt."),
        )],
    );
    fs::hard_link(root.join("agents/primary.md"), root.join("agents/copy.bin")).unwrap();
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::UnsafePath)
    ));

    let user = temp.path().join("user-mode");
    write_layer(&user, runtime(), &[]);
    fs::set_permissions(&user, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        load_from_roots(Some(&user), None),
        Err(ConfigError::UnsafePath)
    ));

    let yaml = temp.path().join("yaml");
    write_layer(
        &yaml,
        runtime(),
        &[(
            "primary.md",
            agent(
                "primary",
                "&chain [{ model: \"test/model-one\" }]",
                "",
                "Prompt.",
            ),
        )],
    );
    assert!(matches!(
        load_from_roots(None, Some(&yaml)),
        Err(ConfigError::AgentFrontmatter(_))
    ));

    let interpolation = temp.path().join("interpolation");
    write_layer(
        &interpolation,
        runtime(),
        &[(
            "primary.md",
            agent(
                "primary",
                "[{ model: \"test/model-one\" }]",
                "",
                "${env:SECRET}",
            ),
        )],
    );
    assert!(matches!(
        load_from_roots(None, Some(&interpolation)),
        Err(ConfigError::AgentFrontmatter(_))
    ));
}

#[test]
fn selected_suffix_changes_only_the_head_variant_and_never_wraps() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("selection");
    write_layer(
        &root,
        runtime(),
        &[(
            "primary.md",
            agent("primary", "[{ model: \"test/model-one\" }]", "", "Prompt."),
        )],
    );
    let loaded = load_from_roots(None, Some(&root)).unwrap();
    let models = build_model_set(
        &loaded.runtime.providers,
        &Catalog::embedded().unwrap(),
        None,
    )
    .unwrap();
    let registry = loaded.resolve_agents(&models).unwrap();
    let primary = registry.get(&AgentId::new("primary").unwrap()).unwrap();
    let selection = cookie_agent_identity::ModelSelection {
        model: "test/model-one".parse().unwrap(),
        variant: None,
    };
    let suffix = primary.selected_suffix(&selection, &models).unwrap();
    assert_eq!(suffix, [selection]);
}

fn assert_unsafe(root: &Path) {
    assert!(matches!(
        load_from_roots(None, Some(root)),
        Err(ConfigError::UnsafePath)
    ));
}

fn make_fifo(path: &Path) {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

#[test]
fn expected_config_fifo_is_rejected_without_blocking() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("fifo-config");
    fs::create_dir(&root).unwrap();
    make_fifo(&root.join("config.toml"));
    assert_unsafe(&root);
}

#[test]
fn expected_config_socket_is_rejected_deterministically() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("socket-config");
    fs::create_dir(&root).unwrap();
    let _socket = UnixListener::bind(root.join("config.toml")).unwrap();
    assert_unsafe(&root);
}

#[test]
fn expected_config_directory_is_rejected_before_reading() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("directory-config");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("config.toml")).unwrap();
    assert_unsafe(&root);
}

#[test]
fn expected_config_hardlink_is_rejected_before_reading() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("hardlink-config");
    fs::create_dir(&root).unwrap();
    let source = temp.path().join("source.toml");
    fs::write(&source, runtime()).unwrap();
    fs::hard_link(source, root.join("config.toml")).unwrap();
    assert_unsafe(&root);
}

#[test]
fn expected_config_device_is_rejected_when_device_creation_is_available() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("device-config");
    fs::create_dir(&root).unwrap();
    let path = CString::new(root.join("config.toml").as_os_str().as_encoded_bytes()).unwrap();
    let result = unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o600, libc::makedev(1, 3)) };
    if result == 0 {
        assert_unsafe(&root);
    } else {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
    }
}

#[test]
fn enumerated_markdown_fifo_is_rejected_without_blocking() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("fifo-agent");
    write_layer(&root, runtime(), &[]);
    make_fifo(&root.join("agents/fifo.md"));
    assert_unsafe(&root);
}

#[test]
fn enumerated_markdown_socket_is_rejected_deterministically() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("socket-agent");
    write_layer(&root, runtime(), &[]);
    let _socket = UnixListener::bind(root.join("agents/socket.md")).unwrap();
    assert_unsafe(&root);
}

#[test]
fn enumerated_markdown_directory_is_rejected_before_reading() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("directory-agent");
    write_layer(&root, runtime(), &[]);
    fs::create_dir(root.join("agents/directory.md")).unwrap();
    assert_unsafe(&root);
}

#[test]
fn enumerated_non_markdown_fifo_is_rejected_without_blocking() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("fifo-enum");
    write_layer(&root, runtime(), &[]);
    make_fifo(&root.join("agents/ignored.bin"));
    assert_unsafe(&root);
}

#[test]
fn enumerated_non_markdown_hardlink_is_rejected() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("hardlink-enum");
    write_layer(&root, runtime(), &[]);
    let source = root.join("agents/source.bin");
    fs::write(&source, b"ignored").unwrap();
    fs::hard_link(&source, root.join("agents/copy.bin")).unwrap();
    assert_unsafe(&root);
}

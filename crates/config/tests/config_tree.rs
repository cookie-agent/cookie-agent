//! The configuration tree: documents read from a user root decode exactly as
//! loading that root does, and a tree edited in memory is validated whole.

use std::fs;

use cookie_agent_config::{
    ConfigDocument, ConfigError, ConfigPath, ConfigTree, load_from_tree, load_from_user_root,
};
use cookie_agent_identity::AgentId;

fn agent(mode: &str, permissions: &str) -> String {
    format!(
        "---\ndescription: Agent\nmode: {mode}\nenabled: true\nmodels: [{{ model: \"custom.test/model\" }}]\npermissions: {permissions}\n---\nPrompt.\n"
    )
}

fn delegating_to(target: &str) -> String {
    agent("primary", &format!("\n  delegate:\n    {target}: allow"))
}

fn path(path: &str) -> ConfigPath {
    ConfigPath::new(path).expect("valid path")
}

fn user_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("root");
    fs::write(
        root.path().join("config.toml"),
        "[loop_warning]\nenabled = false\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("agents/python")).unwrap();
    fs::create_dir_all(root.path().join("agents/empty")).unwrap();
    fs::write(
        root.path().join("agents/worker.md"),
        agent("subagent", "{}"),
    )
    .unwrap();
    fs::write(
        root.path().join("agents/primary.md"),
        delegating_to("worker"),
    )
    .unwrap();
    fs::write(
        root.path().join("agents/python/primary.md"),
        agent("primary", "{}"),
    )
    .unwrap();
    fs::write(root.path().join("agents/notes.txt"), "not an agent").unwrap();
    root
}

#[test]
fn a_tree_decodes_like_its_user_root() {
    let root = user_root();
    let from_disk = load_from_user_root(Some(root.path())).expect("load the root");
    let tree = ConfigTree::read(Some(root.path())).expect("read the tree");
    let from_tree = load_from_tree(&tree).expect("load the tree");

    assert_eq!(
        tree.documents()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        [
            "agents/primary.md",
            "agents/python/primary.md",
            "agents/worker.md",
            "config.toml"
        ]
    );
    assert_eq!(tree.empty_presets().collect::<Vec<_>>(), ["empty"]);
    for loaded in [&from_disk, &from_tree] {
        assert!(!loaded.runtime.loop_warning.enabled);
        assert_eq!(
            loaded
                .agents
                .keys()
                .map(AgentId::as_str)
                .collect::<Vec<_>>(),
            ["primary", "worker"]
        );
        assert_eq!(
            loaded
                .agent_presets
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["empty", "python"]
        );
        assert_eq!(
            loaded.config_paths.user.as_deref(),
            Some(root.path().join("config.toml").as_path())
        );
    }
    assert!(load_from_tree(&ConfigTree::read(None).unwrap()).is_ok());
}

#[test]
fn an_edited_tree_is_validated_whole() {
    let root = user_root();
    let mut tree = ConfigTree::read(Some(root.path())).unwrap();

    // Making a delegation target ineligible breaks its delegator.
    let mut primary_worker = tree.clone();
    primary_worker
        .put(
            path("agents/worker.md"),
            agent("primary", "{}").into_bytes(),
        )
        .unwrap();
    assert!(matches!(
        load_from_tree(&primary_worker),
        Err(ConfigError::IneligibleDelegationTarget { .. })
    ));
    // Changing both together is valid.
    primary_worker
        .put(
            path("agents/primary.md"),
            agent("primary", "{}").into_bytes(),
        )
        .unwrap();
    load_from_tree(&primary_worker).expect("valid with both changed");
    // Removing a delegation target leaves a name a runtime may report
    // later: the engine reports it as a diagnostic, the load succeeds.
    let mut without_worker = tree.clone();
    assert!(without_worker.delete(&path("agents/worker.md")));
    load_from_tree(&without_worker).expect("valid without the target");

    // A bad document anywhere, even in an unselected preset, fails the tree.
    tree.put(path("agents/python/broken.md"), b"no front matter".to_vec())
        .unwrap();
    assert!(load_from_tree(&tree).is_err());
    tree.delete(&path("agents/python/broken.md"));
    tree.put(path("config.toml"), b"[unknown]\n".to_vec())
        .unwrap();
    assert!(load_from_tree(&tree).is_err());
}

#[test]
fn document_paths_are_config_toml_or_agent_documents() {
    assert_eq!(path("config.toml").document(), ConfigDocument::Config);
    assert_eq!(
        path("agents/reviewer.md").document(),
        ConfigDocument::Agent {
            preset: None,
            id: "reviewer"
        }
    );
    assert_eq!(
        path("agents/python/reviewer.md").document(),
        ConfigDocument::Agent {
            preset: Some("python"),
            id: "reviewer"
        }
    );
    for refused in [
        "",
        "tui.toml",
        "/config.toml",
        "agents/reviewer.txt",
        "agents/a/b/reviewer.md",
        "agents/../config.toml",
        "skills/x.md",
    ] {
        assert!(
            matches!(ConfigPath::new(refused), Err(ConfigError::UnsafePath)),
            "{refused:?}"
        );
    }
    assert!(matches!(
        ConfigPath::new("agents/Bad Name.md"),
        Err(ConfigError::AgentFilename(_))
    ));
    assert!(matches!(
        ConfigPath::new("agents/Bad Preset/reviewer.md"),
        Err(ConfigError::AgentPresetName { .. })
    ));
}

#[test]
fn documents_are_bounded_and_fingerprinted() {
    let mut tree = ConfigTree::new(None);
    let agent_limit = 256 * 1024;
    tree.put(path("agents/a.md"), vec![b'x'; agent_limit])
        .expect("at the agent limit");
    assert!(matches!(
        tree.put(path("agents/a.md"), vec![b'x'; agent_limit + 1]),
        Err(ConfigError::TooLarge(_))
    ));
    let config_limit = 1024 * 1024;
    tree.put(path("config.toml"), vec![b' '; config_limit])
        .expect("at the config limit");
    assert!(matches!(
        tree.put(path("config.toml"), vec![b' '; config_limit + 1]),
        Err(ConfigError::TooLarge(_))
    ));

    let fingerprint = ConfigTree::fingerprint(b"text");
    assert!(fingerprint.starts_with("sha256:"));
    assert_eq!(fingerprint.len(), "sha256:".len() + 64);
    let before = tree.revision();
    tree.put(path("agents/b.md"), b"other".to_vec()).unwrap();
    assert_ne!(tree.revision(), before);
    tree.delete(&path("agents/b.md"));
    assert_eq!(tree.revision(), before);
}

fn key(path: &str) -> Vec<String> {
    path.split('.').map(str::to_owned).collect()
}

#[test]
fn key_edits_keep_the_rest_of_config_toml() {
    let mut tree = ConfigTree::new(None);
    tree.put(
        path("config.toml"),
        b"# my settings\n[loop_warning]\nenabled = true # inline note\n\n[model_retry]\nstandard_retries = 1\n"
            .to_vec(),
    )
    .unwrap();
    tree.set_key(
        &key("loop_warning.enabled"),
        Some(toml::Value::Boolean(false)),
    )
    .unwrap();
    tree.set_key(&key("model_retry.standard_retries"), None)
        .unwrap();
    tree.set_key(
        &key("mcp.servers.docs"),
        Some(toml::Value::Table(toml::Table::from_iter([(
            "command".to_owned(),
            toml::Value::String("docs-mcp".into()),
        )]))),
    )
    .unwrap();
    let text = String::from_utf8(tree.get(&path("config.toml")).unwrap().to_vec()).unwrap();
    assert!(text.contains("# my settings"), "{text}");
    assert!(text.contains("enabled = false"), "{text}");
    assert!(!text.contains("standard_retries"), "{text}");
    assert!(text.contains("[mcp.servers.docs]"), "{text}");
    assert!(
        !text.contains("[mcp]\n"),
        "intermediate tables stay implicit: {text}"
    );
    let loaded = load_from_tree(&tree).expect("valid");
    assert!(!loaded.runtime.loop_warning.enabled);
    assert_eq!(
        loaded.mcp_servers["docs"].config.command.as_deref(),
        Some("docs-mcp")
    );

    // Nothing below a value, and no empty key.
    assert!(
        tree.set_key(
            &key("loop_warning.enabled.deeper"),
            Some(toml::Value::Integer(1))
        )
        .is_err()
    );
    assert!(tree.set_key(&[], None).is_err());
}

#[test]
fn a_tree_is_written_together_and_put_back_when_a_document_changed() {
    let root = user_root();
    let original = ConfigTree::read(Some(root.path())).unwrap();
    let mut candidate = original.clone();
    candidate
        .put(
            path("agents/python/reviewer.md"),
            agent("subagent", "{}").into_bytes(),
        )
        .unwrap();
    candidate.delete(&path("agents/primary.md"));
    candidate
        .set_key(
            &key("loop_warning.enabled"),
            Some(toml::Value::Boolean(true)),
        )
        .unwrap();
    load_from_tree(&candidate).expect("valid candidate");

    // A document edited since it was read stops the write, and what was
    // already written is put back.
    fs::write(root.path().join("config.toml"), "# edited elsewhere\n").unwrap();
    assert!(matches!(
        cookie_agent_config::write_tree(&original, &candidate),
        Err(ConfigError::ChangedOnDisk(_))
    ));
    assert!(root.path().join("agents/primary.md").exists());
    assert!(!root.path().join("agents/python/reviewer.md").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("config.toml")).unwrap(),
        "# edited elsewhere\n"
    );

    let original = ConfigTree::read(Some(root.path())).unwrap();
    let mut candidate = original.clone();
    candidate
        .put(
            path("agents/python/reviewer.md"),
            agent("subagent", "{}").into_bytes(),
        )
        .unwrap();
    candidate.delete(&path("agents/primary.md"));
    // Removing a preset's last document removes the preset.
    candidate.delete(&path("agents/python/primary.md"));
    candidate.delete(&path("agents/python/reviewer.md"));
    cookie_agent_config::write_tree(&original, &candidate).expect("write");
    assert!(!root.path().join("agents/primary.md").exists());
    assert!(!root.path().join("agents/python").exists());
    let reread = ConfigTree::read(Some(root.path())).unwrap();
    assert_eq!(reread.revision(), candidate.revision());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut with_agent = reread.clone();
        with_agent
            .put(
                path("agents/new/worker.md"),
                agent("subagent", "{}").into_bytes(),
            )
            .unwrap();
        cookie_agent_config::write_tree(&reread, &with_agent).expect("write");
        let mode = |path: &str| {
            fs::metadata(root.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("agents/new"), 0o700);
        assert_eq!(mode("agents/new/worker.md"), 0o600);
    }
}

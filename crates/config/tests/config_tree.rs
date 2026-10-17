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

    // Removing a delegation target breaks its delegator.
    let mut without_worker = tree.clone();
    assert!(without_worker.delete(&path("agents/worker.md")));
    assert!(matches!(
        load_from_tree(&without_worker),
        Err(ConfigError::UnknownDelegationTarget { .. })
    ));
    // Removing both together is valid.
    assert!(without_worker.delete(&path("agents/primary.md")));
    load_from_tree(&without_worker).expect("valid without both");

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

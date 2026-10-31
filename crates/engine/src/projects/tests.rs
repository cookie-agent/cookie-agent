use std::fs;

use super::*;

const LIMIT: usize = 4096;

fn name(value: &str) -> ProjectName {
    ProjectName::new(value).unwrap()
}

/// A data root whose memory folder exists, as after a first start, and its
/// projects.
fn store() -> (tempfile::TempDir, ProjectStore) {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    let store = ProjectStore::new(data.path(), memory);
    (data, store)
}

fn memory_dir(data: &tempfile::TempDir, project: &str) -> PathBuf {
    data.path().join("memory/projects").join(project)
}

/// The overview's text without its stamp lines.
fn overview(data: &tempfile::TempDir, project: &str) -> String {
    fs::read_to_string(memory_dir(data, project).join("overview.md"))
        .unwrap()
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("created: ") && !line.starts_with("modified: "))
        .collect()
}

fn project_file(data: &tempfile::TempDir, id: &ProjectId) -> PathBuf {
    data.path()
        .join(PROJECTS_DIR)
        .join(id.as_str())
        .join(PROJECT_FILE)
}

fn agents_file(data: &tempfile::TempDir, id: &ProjectId) -> PathBuf {
    data.path()
        .join(PROJECTS_DIR)
        .join(id.as_str())
        .join(AGENTS_FILE)
}

#[test]
fn creating_a_project_writes_its_file_and_an_empty_overview() {
    let (data, store) = store();
    let mut changes = store.subscribe();
    let defaults = ProjectDefaults {
        preset: Some("rust".into()),
        agent: Some(AgentId::new("builder").unwrap()),
        global: false,
        runtimes: vec![ProjectRuntime {
            runtime_id: RuntimeId::new("labpc").unwrap(),
            capability: None,
            workspace: Some("/home/u/web".into()),
        }],
    };
    let created = store
        .create(
            name("web"),
            Some("🍪".into()),
            Some(defaults.clone()),
            LIMIT,
        )
        .unwrap();
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();
    assert_eq!(created.name.as_str(), "web");
    assert_eq!(store.get(&created.id).unwrap(), created);
    assert_eq!(store.list().unwrap(), std::slice::from_ref(&created));
    let file = fs::read_to_string(project_file(&data, &created.id)).unwrap();
    assert!(
        file.starts_with("name = \"web\"\nemoji = \"🍪\"\ncreated = "),
        "{file}"
    );
    assert!(
        file.contains("[defaults]\npreset = \"rust\"\nagent = \"builder\"\nglobal = false"),
        "{file}"
    );
    assert_eq!(created.defaults, Some(defaults));
    assert_eq!(created.revision, revision(file.as_bytes(), None));
    assert_eq!(
        overview(&data, "web"),
        "---\ndescription: Overview of the `web` project, loaded in full into its sessions\n---\n"
    );
    assert_eq!(memory_count(store.memory(), &created), 1);

    // The name is taken now.
    assert!(matches!(
        store.create(name("web"), None, None, LIMIT),
        Err(ProjectError::NameTaken(_))
    ));
    assert!(!changes.has_changed().unwrap());
}

#[test]
fn a_memory_folder_of_no_project_becomes_the_new_projects() {
    let (data, store) = store();
    fs::create_dir_all(memory_dir(&data, "web")).unwrap();
    fs::write(
        memory_dir(&data, "web").join("overview.md"),
        "---\ndescription: Mine\nkeywords: [shop]\n---\nA shop.\n",
    )
    .unwrap();
    store.create(name("web"), None, None, LIMIT).unwrap();
    // The overview keeps its body and keywords and takes the daemon's
    // description.
    assert_eq!(
        overview(&data, "web"),
        "---\ndescription: Overview of the `web` project, loaded in full into its sessions\nkeywords: [shop]\n---\nA shop.\n"
    );
}

#[test]
fn every_change_is_compare_and_swap_on_the_revision() {
    let (data, store) = store();
    let created = store.create(name("web"), None, None, LIMIT).unwrap();
    let change = |emoji: &str| ProjectChange {
        emoji: Some(Some(emoji.to_owned())),
        ..ProjectChange::default()
    };
    let updated = store
        .update(&created.id, &created.revision, change("🚀"), LIMIT)
        .unwrap();
    assert_eq!(updated.emoji.as_deref(), Some("🚀"));
    assert_ne!(updated.revision, created.revision);
    assert_eq!(store.get(&created.id).unwrap(), updated);

    // The revision read before the change is stale now.
    let stale = store.update(&created.id, &created.revision, change("🎉"), LIMIT);
    assert!(
        matches!(&stale, Err(ProjectError::Changed { revision, .. }) if *revision == updated.revision),
        "{stale:?}"
    );
    assert_eq!(store.get(&created.id).unwrap().emoji.as_deref(), Some("🚀"));

    // `AGENTS.md` is part of the revision; an empty text removes it.
    let with_agents = store
        .update(
            &created.id,
            &updated.revision,
            ProjectChange {
                agents_md: Some("Use tabs.\n".into()),
                ..ProjectChange::default()
            },
            LIMIT,
        )
        .unwrap();
    assert_ne!(with_agents.revision, updated.revision);
    let path = agents_file(&data, &created.id);
    assert_eq!(fs::read_to_string(&path).unwrap(), "Use tabs.\n");
    // A hand edit of `AGENTS.md` changes the revision too.
    fs::write(&path, "Use spaces.\n").unwrap();
    let edited = store.get(&created.id).unwrap();
    assert_ne!(edited.revision, with_agents.revision);
    assert!(matches!(
        store.update(&created.id, &with_agents.revision, change("🎉"), LIMIT),
        Err(ProjectError::Changed { .. })
    ));
    let cleared = store
        .update(
            &created.id,
            &edited.revision,
            ProjectChange {
                emoji: Some(None),
                agents_md: Some(String::new()),
                ..ProjectChange::default()
            },
            LIMIT,
        )
        .unwrap();
    assert!(!path.exists());
    assert_eq!((cleared.emoji, cleared.agents_md), (None, None));

    // An `AGENTS.md` over the cap is refused.
    assert!(matches!(
        store.update(
            &created.id,
            &cleared.revision,
            ProjectChange {
                agents_md: Some("x".repeat(MAX_AGENTS_MD_BYTES + 1)),
                ..ProjectChange::default()
            },
            LIMIT
        ),
        Err(ProjectError::Invalid {
            field: "agents_md",
            ..
        })
    ));

    // An unknown project is not found.
    assert!(matches!(
        store.update(
            &ProjectId::new("0000-none").unwrap(),
            "x",
            ProjectChange::default(),
            LIMIT
        ),
        Err(ProjectError::NotFound(_))
    ));
}

#[test]
fn a_rename_moves_the_memory_folder_and_the_overviews_description() {
    let (data, store) = store();
    let web = store.create(name("web"), None, None, LIMIT).unwrap();
    store.create(name("api"), None, None, LIMIT).unwrap();
    fs::write(
        memory_dir(&data, "web").join("release.md"),
        "---\ndescription: Releases\n---\n- tag\n",
    )
    .unwrap();
    fs::write(memory_dir(&data, "web").join("notes.txt"), "x").unwrap();
    let rename = |to: &str| ProjectChange {
        name: Some(name(to)),
        ..ProjectChange::default()
    };

    // Another project's name, or a folder in the way, is taken; nothing
    // moves.
    assert!(matches!(
        store.update(&web.id, &web.revision, rename("api"), LIMIT),
        Err(ProjectError::NameTaken(_))
    ));
    fs::create_dir_all(memory_dir(&data, "stray")).unwrap();
    assert!(matches!(
        store.update(&web.id, &web.revision, rename("stray"), LIMIT),
        Err(ProjectError::NameTaken(_))
    ));
    assert!(memory_dir(&data, "web").join("release.md").is_file());
    assert_eq!(store.get(&web.id).unwrap().name.as_str(), "web");

    let renamed = store
        .update(&web.id, &web.revision, rename("shop"), LIMIT)
        .unwrap();
    assert_eq!(renamed.id, web.id);
    assert_eq!(renamed.name.as_str(), "shop");
    assert!(!memory_dir(&data, "web").exists());
    assert!(memory_dir(&data, "shop").join("release.md").is_file());
    assert!(memory_dir(&data, "shop").join("notes.txt").is_file());
    assert_eq!(
        overview(&data, "shop"),
        "---\ndescription: Overview of the `shop` project, loaded in full into its sessions\n---\n"
    );
    assert_eq!(store.names(), ["api", "shop"]);
}

#[test]
fn deleting_a_project_removes_its_whole_memory_folder_and_its_own() {
    let (data, store) = store();
    let web = store.create(name("web"), None, None, LIMIT).unwrap();
    let web = store
        .update(
            &web.id,
            &web.revision,
            ProjectChange {
                agents_md: Some("rules".into()),
                ..ProjectChange::default()
            },
            LIMIT,
        )
        .unwrap();
    fs::write(
        memory_dir(&data, "web").join("release.md"),
        "---\ndescription: Releases\n---\n",
    )
    .unwrap();
    fs::write(memory_dir(&data, "web").join("notes.txt"), "x").unwrap();
    assert!(matches!(
        store.delete(&web.id, "0000000000000000"),
        Err(ProjectError::Changed { .. })
    ));
    assert!(memory_dir(&data, "web").is_dir());
    assert_eq!(store.delete(&web.id, &web.revision).unwrap(), 2);
    assert!(!memory_dir(&data, "web").exists());
    assert!(!data.path().join("projects").join(web.id.as_str()).exists());
    assert!(store.list().unwrap().is_empty());
    assert!(matches!(store.get(&web.id), Err(ProjectError::NotFound(_))));
    // Nothing is left behind in the registry folder.
    assert_eq!(
        fs::read_dir(data.path().join("projects")).unwrap().count(),
        0
    );
}

#[test]
fn the_upgrade_adopts_every_valid_memory_folder_once() {
    let (data, store) = store();
    let existing = store.create(name("api"), None, None, LIMIT).unwrap();
    for folder in ["web", "shop", "Not-Valid"] {
        fs::create_dir_all(memory_dir(&data, folder)).unwrap();
    }
    fs::write(
        memory_dir(&data, "web").join("build.md"),
        "---\ndescription: Build\n---\n- cargo\n",
    )
    .unwrap();
    let adopted = store.adopt_memory_folders(LIMIT).unwrap();
    assert_eq!(
        adopted
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        ["shop", "web"]
    );
    assert!(
        adopted
            .iter()
            .all(|project| project.defaults.is_none() && project.agents_md.is_none())
    );
    assert_eq!(store.names(), ["api", "shop", "web"]);
    assert!(store.list().unwrap().contains(&existing));
    // Each project has its overview; the memories stay.
    assert!(memory_dir(&data, "web").join("overview.md").is_file());
    assert!(memory_dir(&data, "web").join("build.md").is_file());
    assert!(memory_dir(&data, "shop").join("overview.md").is_file());
    // A second start adopts nothing more.
    assert!(store.adopt_memory_folders(LIMIT).unwrap().is_empty());
    assert_eq!(store.list().unwrap().len(), 3);
}

#[test]
fn the_upgrade_adopts_nothing_while_a_project_does_not_read() {
    let (data, store) = store();
    let broken = data.path().join("projects/0000-broken");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("project.toml"), "name = ").unwrap();
    fs::create_dir_all(memory_dir(&data, "web")).unwrap();
    assert!(store.adopt_memory_folders(LIMIT).unwrap().is_empty());
    // The unreadable project is left out of the list.
    assert!(store.list().unwrap().is_empty());
    // Nor can a name be checked against it.
    assert!(matches!(
        store.create(name("web"), None, None, LIMIT),
        Err(ProjectError::Store(_))
    ));
}

#[test]
fn an_emoji_is_exactly_one_emoji() {
    for valid in [
        "🍪",
        "👍🏽",
        "👩\u{200d}💻",
        "🇯🇵",
        "1\u{fe0f}\u{20e3}",
        "❤\u{fe0f}",
        "🏴\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}",
        "©",
    ] {
        assert_eq!(validate_emoji(valid), Ok(()), "{valid}");
    }
    for invalid in [
        "", "a", "1", "#", ":)", "🍪🍪", "🍪 ", " 🍪", "é", "\u{fe0f}", "\u{200d}", "🍪\n", "日",
    ] {
        assert!(validate_emoji(invalid).is_err(), "{invalid:?}");
    }
    // One cluster, but over 32 bytes.
    let long = "👨\u{200d}👩\u{200d}👧\u{200d}👦\u{200d}👨\u{200d}👩";
    assert!(long.len() > MAX_EMOJI_BYTES);
    assert!(validate_emoji(long).is_err());
}

#[test]
fn defaults_list_each_runtime_once_with_a_real_workspace() {
    let runtime = |workspace: Option<&str>| ProjectRuntime {
        runtime_id: RuntimeId::local(),
        capability: Some(CapabilityName::coding()),
        workspace: workspace.map(str::to_owned),
    };
    let defaults = |runtimes| ProjectDefaults {
        runtimes,
        ..ProjectDefaults::default()
    };
    assert!(validate_defaults(&defaults(vec![runtime(Some("/a")), runtime(Some("/b"))])).is_ok());
    assert!(matches!(
        validate_defaults(&defaults(vec![runtime(Some("/a")), runtime(Some("/a"))])),
        Err(ProjectError::Invalid {
            field: "defaults",
            ..
        })
    ));
    assert!(matches!(
        validate_defaults(&defaults(vec![runtime(Some("  "))])),
        Err(ProjectError::Invalid {
            field: "defaults",
            ..
        })
    ));
    let (_data, store) = store();
    assert!(matches!(
        store.create(name("web"), Some("x".into()), None, LIMIT),
        Err(ProjectError::Invalid { field: "emoji", .. })
    ));
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn a_projects_name_follows_its_changes_and_its_file() {
    let (data, store) = store();
    let web = store.create(name("web"), None, None, LIMIT).unwrap();
    assert_eq!(store.name(&web.id), Some(name("web")));

    // A change through the store is seen at once.
    let renamed = store
        .update(
            &web.id,
            &web.revision,
            ProjectChange {
                name: Some(name("shop")),
                ..ProjectChange::default()
            },
            LIMIT,
        )
        .unwrap();
    assert_eq!(store.name(&web.id), Some(name("shop")));

    // So is one another process makes to the file.
    let file = project_file(&data, &web.id);
    let text = fs::read_to_string(&file).unwrap();
    let edited = text.replace("\"shop\"", "\"storefront\"");
    assert_ne!(edited, text, "the file names the project: {text}");
    let replacement = file.with_extension("edited");
    fs::write(&replacement, edited).unwrap();
    fs::rename(&replacement, &file).unwrap();
    assert_eq!(store.name(&web.id), Some(name("storefront")));

    store
        .delete(&renamed.id, &store.get(&web.id).unwrap().revision)
        .unwrap();
    assert_eq!(store.name(&web.id), None);
}

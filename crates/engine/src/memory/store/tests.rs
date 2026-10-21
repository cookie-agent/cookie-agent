use std::fs;

use super::*;
use crate::memory::agents::AgentMemory;

const LIMIT: usize = 4096;

fn url(value: &str) -> MemoryUrl {
    MemoryUrl::parse(value).unwrap()
}

fn text(description: &str, body: &str) -> String {
    MemoryDocument::new(description, vec!["k".to_owned()], body)
        .unwrap()
        .render()
}

fn no_agents(_: &AgentId) -> AgentMemory {
    AgentMemory::Unknown
}

fn any_agent(_: &AgentId) -> AgentMemory {
    AgentMemory::Registered
}

/// A data root whose memory folder exists, as after a first start.
fn root() -> (tempfile::TempDir, MemoryRoot) {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    (data, memory)
}

fn create(memory: &MemoryRoot, target: &str, body: &str) -> MemoryRevision {
    memory
        .write(
            &url(target),
            &text("d", body),
            None,
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision)
        .unwrap()
}

#[test]
fn a_write_creates_then_replaces_only_at_the_read_revision() {
    let (_data, memory) = root();
    let target = url("project://web/build");
    let first = create(&memory, "project://web/build", "one\n");
    let read = memory.read(&target, LIMIT).unwrap();
    assert_eq!(read.entry.revision, first);
    assert_eq!(read.text, text("d", "one\n"));
    assert_eq!(read.entry.document.as_ref().unwrap().body(), "one\n");
    assert_eq!(first, MemoryRevision::of(text("d", "one\n").as_bytes()));

    // Creating again fails and names the current revision.
    let again = memory
        .write(
            &target,
            &text("d", "x"),
            None,
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision);
    assert!(matches!(again, Err(StoreError::Exists { revision, .. }) if revision == first));

    let second = memory
        .write(
            &target,
            &text("d", "two\n"),
            Some(&first),
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision)
        .unwrap();
    assert_ne!(second, first);
    // The old revision no longer replaces it.
    let stale = memory
        .write(
            &target,
            &text("d", "three\n"),
            Some(&first),
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision);
    assert!(
        matches!(stale, Err(StoreError::RevisionMismatch { current, .. }) if current == second)
    );
    assert_eq!(
        memory.read(&target, LIMIT).unwrap().text,
        text("d", "two\n")
    );

    // Replacing a file that does not exist fails.
    let missing = memory
        .write(
            &url("project://web/other"),
            &text("d", ""),
            Some(&first),
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision);
    assert!(matches!(missing, Err(StoreError::NotFound { .. })));
    assert!(matches!(
        memory.read(&url("project://web/other"), LIMIT),
        Err(StoreError::NotFound { .. })
    ));
}

#[test]
fn a_write_keeps_the_text_it_was_given_and_leaves_no_temporary_file() {
    let (_data, memory) = root();
    let authored = "---\n# a comment\ndescription: 'quoted'\n---\nbody\n";
    let target = url("machine://labpc/gpu");
    memory
        .write(&target, authored, None, Origin::User, no_agents, LIMIT)
        .map(|written| written.revision)
        .unwrap();
    let folder = memory.machine_dir(&MachineName::new("labpc").unwrap());
    let names = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["gpu.md"]);
    assert_eq!(fs::read_to_string(folder.join("gpu.md")).unwrap(), authored);
}

#[test]
fn invalid_or_oversized_writes_are_refused() {
    let (_data, memory) = root();
    let target = url("project://web/a");
    for written in [
        "no frontmatter",
        "---\ndescription: x\nextra: 1\n---\n",
        "---\ndescription: x\n---\n\u{0}",
    ] {
        let result = memory
            .write(&target, written, None, Origin::User, any_agent, LIMIT)
            .map(|written| written.revision);
        assert!(
            matches!(result, Err(StoreError::Invalid { .. })),
            "{written:?}"
        );
    }
    let head = "---\ndescription: x\n---\n";
    let fits = format!("{head}{}", "b".repeat(100 - head.len()));
    assert!(
        memory
            .write(
                &target,
                &format!("{fits}b"),
                None,
                Origin::User,
                any_agent,
                100
            )
            .map(|written| written.revision)
            .is_err()
    );
    assert!(
        memory
            .write(&target, &fits, None, Origin::User, any_agent, 100)
            .map(|written| written.revision)
            .is_ok()
    );
}

#[test]
fn only_the_user_creates_user_categories_and_agent_files_need_an_agent() {
    let (_data, memory) = root();
    let category = url("user://work");
    let refused = memory
        .write(
            &category,
            &text("d", ""),
            None,
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision);
    assert!(matches!(refused, Err(StoreError::Refused(_))));
    assert!(memory.read(&category, LIMIT).is_err());
    memory
        .write(
            &category,
            &text("d", ""),
            None,
            Origin::User,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision)
        .unwrap();

    // The model may edit an existing category.
    let existing = memory.read(&url("user://feedback"), LIMIT).unwrap();
    memory
        .write(
            &url("user://feedback"),
            &text("d", "new"),
            Some(&existing.entry.revision),
            Origin::Model,
            no_agents,
            LIMIT,
        )
        .map(|written| written.revision)
        .unwrap();

    let agent = url("agent://reviewer");
    for origin in [Origin::Model, Origin::User] {
        let refused = memory.write(
            &agent,
            &text("d", ""),
            None,
            origin,
            |id| {
                if id.as_str() == "default" {
                    AgentMemory::Registered
                } else {
                    AgentMemory::Unknown
                }
            },
            LIMIT,
        );
        assert!(matches!(refused, Err(StoreError::Refused(_))), "{origin:?}");
    }
    memory
        .write(
            &agent,
            &text("d", ""),
            None,
            Origin::Model,
            |id| {
                if id.as_str() == "reviewer" {
                    AgentMemory::Registered
                } else {
                    AgentMemory::Unknown
                }
            },
            LIMIT,
        )
        .map(|written| written.revision)
        .unwrap();
}

#[test]
fn deleting_user_and_agent_files_is_for_the_user_only() {
    let (_data, memory) = root();
    let agent = url("agent://reviewer");
    let agent_revision = memory
        .write(
            &agent,
            &text("d", ""),
            None,
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision)
        .unwrap();
    let category = url("user://profile");
    let category_revision = memory.read(&category, LIMIT).unwrap().entry.revision;
    for (target, revision) in [(&agent, &agent_revision), (&category, &category_revision)] {
        let refused = memory.delete(target, revision, Origin::Model);
        assert!(matches!(refused, Err(StoreError::Refused(_))), "{target}");
        assert!(memory.read(target, LIMIT).is_ok());
        memory.delete(target, revision, Origin::User).unwrap();
        assert!(matches!(
            memory.read(target, LIMIT),
            Err(StoreError::NotFound { .. })
        ));
    }
}

/// A model's delete of a user category or an agent file clears its body:
/// the file stays with its frontmatter exactly as written, and listings,
/// the snapshot, and search still show it with its description.
#[test]
fn clearing_a_body_keeps_the_file_and_its_frontmatter_byte_for_byte() {
    use crate::memory::{
        search::Visibility,
        snapshot::{SnapshotLimits, SnapshotRequest},
    };

    let (data, memory) = root();
    let feedback_frontmatter = "---\r\n# Written by hand\r\ndescription: \"Feedback: corrections\"\r\nkeywords:\r\n  - feedback\r\n  - zebra\r\n---\r\n";
    let agent_frontmatter = "---\ndescription: How reviews go\nkeywords: [review, zebra]\n---\n";
    let mut changes = memory.subscribe();
    for (target, frontmatter, body) in [
        (
            "user://feedback",
            feedback_frontmatter,
            "- prefer short answers\r\n",
        ),
        ("agent://reviewer", agent_frontmatter, "- zebra checklist\n"),
    ] {
        let target = url(target);
        let current = memory
            .read(&target, LIMIT)
            .ok()
            .map(|read| read.entry.revision);
        let written = memory
            .write(
                &target,
                &format!("{frontmatter}{body}"),
                current.as_ref(),
                Origin::User,
                any_agent,
                LIMIT,
            )
            .map(|written| written.revision)
            .unwrap();
        changes.mark_unchanged();

        // A stale revision changes nothing.
        let stale = MemoryRevision::of(b"something else");
        assert!(matches!(
            memory.clear_body(&target, &stale, LIMIT),
            Err(StoreError::RevisionMismatch { current, .. }) if current == written
        ));
        assert!(!changes.has_changed().unwrap());

        let cleared = memory.clear_body(&target, &written, LIMIT).unwrap();
        let revision = MemoryRevision::of(frontmatter.as_bytes());
        assert_eq!(cleared, ClearedBody::Cleared(revision.clone()), "{target}");
        let path = data.path().join("memory").join(target.relative_path());
        assert_eq!(fs::read(&path).unwrap(), frontmatter.as_bytes(), "{target}");
        assert!(changes.has_changed().unwrap());
        changes.mark_unchanged();

        // An empty body is left alone.
        assert_eq!(
            memory.clear_body(&target, &revision, LIMIT).unwrap(),
            ClearedBody::AlreadyEmpty(revision.clone())
        );
        assert_eq!(fs::read(&path).unwrap(), frontmatter.as_bytes());
        assert!(!changes.has_changed().unwrap());
    }

    let listing = memory.list(&ListScope::All, LIMIT).unwrap();
    let described = |target: &str| {
        let entry = listing
            .entries
            .iter()
            .find(|entry| entry.url.to_string() == target)
            .unwrap_or_else(|| panic!("{target} is listed"));
        let document = entry.document.as_ref().unwrap();
        assert_eq!(document.body(), "");
        document.description().to_owned()
    };
    assert_eq!(described("user://feedback"), "Feedback: corrections");
    assert_eq!(described("agent://reviewer"), "How reviews go");

    let snapshot = memory
        .snapshot(&SnapshotRequest {
            agent: AgentId::new("reviewer").unwrap(),
            delegated: false,
            writable: true,
            machines: Vec::new(),
            limits: SnapshotLimits {
                max_file_bytes: LIMIT,
                user_bytes: 16 * 1024,
                index_bytes: 16 * 1024,
            },
        })
        .unwrap();
    let feedback = snapshot
        .user
        .iter()
        .find(|category| category.category == "feedback")
        .unwrap();
    assert_eq!(feedback.description, "Feedback: corrections");
    assert_eq!(feedback.keywords, ["feedback", "zebra"]);
    assert_eq!(feedback.body, "");
    let agent = snapshot.agent.as_ref().unwrap().memory.as_ref().unwrap();
    assert_eq!(agent.description, "How reviews go");
    assert_eq!(agent.body, "");
    let text = crate::memory::render::snapshot_turn(&snapshot).unwrap();
    assert!(text.contains("user://feedback"), "{text}");
    // An empty agent memory is left out of the rendered text.
    assert!(!text.contains("About this agent"), "{text}");

    // Search still finds both by their keywords, with no body lines.
    let hits = memory
        .search("zebra", None, 10, LIMIT, |_| Visibility::Full)
        .unwrap();
    let found = hits
        .iter()
        .map(|hit| (hit.url.to_string(), hit.description.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.contains(&("user://feedback".to_owned(), "Feedback: corrections")));
    assert!(found.contains(&("agent://reviewer".to_owned(), "How reviews go")));
    assert!(
        hits.iter()
            .all(|hit| hit.detail.as_ref().unwrap().lines.is_empty())
    );
}

#[test]
fn clearing_needs_an_existing_valid_memory() {
    let (data, memory) = root();
    let ghost = url("agent://ghost");
    assert!(matches!(
        memory.clear_body(&ghost, &MemoryRevision::of(b""), LIMIT),
        Err(StoreError::NotFound { .. })
    ));
    let path = data.path().join("memory/user/broken.md");
    fs::write(&path, "no frontmatter\n").unwrap();
    let revision = MemoryRevision::of(b"no frontmatter\n");
    assert!(matches!(
        memory.clear_body(&url("user://broken"), &revision, LIMIT),
        Err(StoreError::Invalid { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), b"no frontmatter\n");
}

#[test]
fn a_delete_needs_the_current_revision() {
    let (_data, memory) = root();
    let target = url("machine://labpc/gpu");
    let first = create(&memory, "machine://labpc/gpu", "a");
    let stale = MemoryRevision::of(b"something else");
    assert!(matches!(
        memory.delete(&target, &stale, Origin::Model),
        Err(StoreError::RevisionMismatch { current, .. }) if current == first
    ));
    memory.delete(&target, &first, Origin::Model).unwrap();
    assert!(matches!(
        memory.delete(&target, &first, Origin::Model),
        Err(StoreError::NotFound { .. })
    ));
    // A machine keeps its folder; it belongs to the machine registry.
    assert!(
        memory
            .machine_dir(&MachineName::new("labpc").unwrap())
            .is_dir()
    );
}

#[test]
fn a_project_starts_with_its_first_memory_and_ends_with_its_last() {
    let (data, memory) = root();
    let folder = data.path().join("memory/projects/web");
    assert!(!folder.exists());
    let a = create(&memory, "project://web/a", "");
    assert!(folder.is_dir());
    let b = create(&memory, "project://web/b", "");
    memory
        .delete(&url("project://web/a"), &a, Origin::Model)
        .unwrap();
    assert!(folder.is_dir());
    memory
        .delete(&url("project://web/b"), &b, Origin::Model)
        .unwrap();
    assert!(!folder.exists());

    // A file that is not a memory keeps the folder.
    let c = create(&memory, "project://web/c", "");
    fs::write(folder.join("notes.txt"), "x").unwrap();
    memory
        .delete(&url("project://web/c"), &c, Origin::Model)
        .unwrap();
    assert!(folder.join("notes.txt").is_file());
}

#[test]
fn an_edit_replaces_exactly_one_occurrence() {
    let (_data, memory) = root();
    let target = url("project://web/a");
    let first = create(&memory, "project://web/a", "alpha beta\nbeta gamma\n");
    for (old, expected) in [
        ("delta", "does not occur"),
        ("beta", "occurs 2 times"),
        ("", "does not occur"),
    ] {
        let error = memory
            .edit(&target, old, "x", &first, any_agent, LIMIT)
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{old}: {error}");
    }
    // The frontmatter is not part of the body.
    let error = memory
        .edit(&target, "description", "x", &first, any_agent, LIMIT)
        .unwrap_err();
    assert!(matches!(error, StoreError::NoOccurrence { .. }));

    let second = memory
        .edit(&target, "alpha", "ALPHA", &first, any_agent, LIMIT)
        .unwrap();
    let read = memory.read(&target, LIMIT).unwrap();
    assert_eq!(read.entry.revision, second);
    assert_eq!(read.text, text("d", "ALPHA beta\nbeta gamma\n"));

    // Edits need the current revision, keep the file valid, and fit.
    assert!(matches!(
        memory.edit(&target, "gamma", "x", &first, any_agent, LIMIT),
        Err(StoreError::RevisionMismatch { current, .. }) if current == second
    ));
    assert!(matches!(
        memory.edit(&target, "gamma", "\u{7}", &second, any_agent, LIMIT),
        Err(StoreError::Invalid { .. })
    ));
    let size = read.text.len();
    assert!(matches!(
        memory.edit(&target, "gamma", "gammas", &second, any_agent, size),
        Err(StoreError::Invalid { .. })
    ));
    assert!(
        memory
            .edit(&target, "gamma", "gamm", &second, any_agent, size)
            .is_ok()
    );
}

#[test]
fn editing_an_invalid_file_is_refused() {
    let (data, memory) = root();
    let path = data.path().join("memory/projects/web/a.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "no frontmatter alpha").unwrap();
    let revision = MemoryRevision::of(b"no frontmatter alpha");
    assert!(matches!(
        memory.edit(
            &url("project://web/a"),
            "alpha",
            "beta",
            &revision,
            any_agent,
            LIMIT
        ),
        Err(StoreError::Invalid { .. })
    ));
    // Writing the whole file at its revision repairs it.
    memory
        .write(
            &url("project://web/a"),
            &text("d", ""),
            Some(&revision),
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision)
        .unwrap();
}

#[test]
fn listing_reports_invalid_files_and_ignores_what_breaks_the_layout() {
    let (data, memory) = root();
    let base = data.path().join("memory");
    let put = |path: &str, contents: &str| {
        let path = base.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    };
    let valid = text("d", "");
    put("agents/reviewer.md", &valid);
    put("agents/self.md", &valid);
    put("agents/a--b.md", &valid);
    put("user/Upper.md", &valid);
    put("user/notes.txt", &valid);
    put("user/broken.md", "---\ndescription: x\nextra: 1\n---\n");
    put("user/nested/deep.md", &valid);
    put("machines/labpc/gpu.md", &valid);
    put("machines/labpc/sub/deep.md", &valid);
    put("machines/Bad/gpu.md", &valid);
    put("machines/stray.md", &valid);
    put("projects/web/a.md", &valid);
    put(
        "projects/web/big.md",
        &format!("{valid}{}", "x".repeat(LIMIT)),
    );
    put("projects/web_1/a.md", &valid);
    put("stray.txt", "x");

    let listing = memory.list(&ListScope::All, LIMIT).unwrap();
    let entries = listing
        .entries
        .iter()
        .map(|entry| {
            (
                entry.url.to_string(),
                entry.document.as_ref().err().map(|reason| reason.0.clone()),
            )
        })
        .collect::<Vec<_>>();
    let urls = entries
        .iter()
        .map(|(url, _)| url.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        urls,
        [
            "user://broken",
            "user://feedback",
            "user://preferences",
            "user://profile",
            "user://references",
            "agent://reviewer",
            "machine://labpc/gpu",
            "project://web/a",
            "project://web/big",
        ]
    );
    let reason = |target: &str| {
        entries
            .iter()
            .find(|(url, _)| url == target)
            .and_then(|(_, reason)| reason.clone())
    };
    assert!(
        reason("user://broken")
            .unwrap()
            .contains("unknown field `extra`")
    );
    assert!(
        reason("project://web/big")
            .unwrap()
            .contains("max_file_bytes")
    );
    assert_eq!(reason("project://web/a"), None);
    let big = listing
        .entries
        .iter()
        .find(|entry| entry.url.to_string() == "project://web/big")
        .unwrap();
    assert_eq!(big.size, (valid.len() + LIMIT) as u64);
    assert_eq!(
        big.revision,
        MemoryRevision::of(format!("{valid}{}", "x".repeat(LIMIT)).as_bytes())
    );

    let ignored = listing
        .ignored
        .iter()
        .map(|ignored| (ignored.path.as_str(), ignored.reason))
        .collect::<Vec<_>>();
    assert_eq!(
        ignored,
        [
            ("stray.txt", UNKNOWN_TOP),
            ("user/Upper.md", BAD_NAME),
            ("user/nested", NESTED),
            ("user/notes.txt", NOT_MARKDOWN),
            ("agents/a--b.md", BAD_AGENT),
            ("agents/self.md", BAD_AGENT),
            ("machines/Bad", BAD_MACHINE),
            ("machines/labpc/sub", NESTED),
            ("machines/stray.md", NOT_FOLDER),
            ("projects/web_1", BAD_NAME),
        ]
    );
}

#[test]
fn listing_narrows_by_layer_agent_machine_and_project() {
    let (_data, memory) = root();
    for target in [
        "agent://reviewer",
        "agent://default",
        "machine://labpc/gpu",
        "machine://laptop/gpu",
        "project://web/a",
        "project://api/a",
    ] {
        create(&memory, target, "");
    }
    let urls = |scope: ListScope| {
        memory
            .list(&scope, LIMIT)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.url.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        urls(ListScope::Layer(MemoryLayer::Agent)),
        ["agent://default", "agent://reviewer"]
    );
    assert_eq!(
        urls(ListScope::Agent(AgentId::new("reviewer").unwrap())),
        ["agent://reviewer"]
    );
    assert_eq!(
        urls(ListScope::Machine(MachineName::new("labpc").unwrap())),
        ["machine://labpc/gpu"]
    );
    assert_eq!(
        urls(ListScope::Project(MemoryName::new("api").unwrap())),
        ["project://api/a"]
    );
    assert_eq!(urls(ListScope::All).len(), 10);
}

#[test]
fn the_first_start_creates_the_defaults_once_and_never_touches_an_existing_folder() {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    let base = data.path().join("memory");
    for layer in ["user", "agents", "machines", "projects"] {
        assert!(base.join(layer).is_dir(), "{layer}");
    }
    let listing = memory.list(&ListScope::All, LIMIT).unwrap();
    assert!(listing.ignored.is_empty());
    let found = listing
        .entries
        .iter()
        .map(|entry| {
            let document = entry.document.as_ref().unwrap();
            (
                entry.url.to_string(),
                document.description().to_owned(),
                document.keywords().join(","),
                document.body().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let expected = [
        (
            "user://feedback",
            "Corrections and confirmed approaches from the user, each with why it matters",
            "feedback,correction,mistake,approach",
        ),
        (
            "user://preferences",
            "How the user wants work done: style, tools, formats, tone, and things to always or never do",
            "prefer,style,always,never,format",
        ),
        (
            "user://profile",
            "Who the user is: role, expertise, languages, and the machines and accounts they use",
            "user,role,background,expertise",
        ),
        (
            "user://references",
            "Where to find things: URLs, dashboards, documentation, related repositories",
            "link,url,docs,dashboard,repo",
        ),
    ]
    .map(|(url, description, keywords)| {
        (url.to_owned(), description.to_owned(), keywords.to_owned(), String::new())
    });
    assert_eq!(found, expected);
    // Nothing is left behind in the data root.
    let names = fs::read_dir(data.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["memory"]);

    // A second start changes nothing, even with user/ emptied.
    for entry in fs::read_dir(base.join("user")).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    assert!(!memory.create_defaults_if_absent().unwrap());
    assert_eq!(fs::read_dir(base.join("user")).unwrap().count(), 0);

    // An existing folder of any shape is left alone.
    let other = tempfile::tempdir().unwrap();
    fs::create_dir(other.path().join("memory")).unwrap();
    let existing = MemoryRoot::new(other.path());
    assert!(!existing.create_defaults_if_absent().unwrap());
    assert_eq!(
        fs::read_dir(other.path().join("memory")).unwrap().count(),
        0
    );
}

#[test]
fn machine_changes_before_the_first_start_create_no_memory_folder() {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    let lock = memory.lock().unwrap();
    let (from, to) = (
        MachineName::new("box").unwrap(),
        MachineName::new("labpc").unwrap(),
    );
    let _ = memory.rename_machine_folder(&lock, &from, &to).unwrap();
    let _ = memory.merge_machine_folders(&lock, &from, &to).unwrap();
    let _ = memory.remove_empty_machine_folder(&lock, &from).unwrap();
    drop(lock);
    assert!(!data.path().join("memory").exists());
    assert!(memory.create_defaults_if_absent().unwrap());
}

/// Every clone of a root shares one change signal, so a model tool's write
/// reaches the `memory.changed` subscribers too; a refused change signals
/// nothing.
#[test]
fn successful_changes_through_any_clone_signal_subscribers() {
    let (_data, memory) = root();
    let mut changes = memory.subscribe();
    let tool = memory.clone();
    let revision = create(&tool, "project://web/build", "b");
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();

    // A refused write changes nothing and signals nothing.
    let refused = tool
        .write(
            &url("project://web/build"),
            &text("d", "c"),
            None,
            Origin::Model,
            any_agent,
            LIMIT,
        )
        .map(|written| written.revision);
    assert!(matches!(refused, Err(StoreError::Exists { .. })));
    assert!(!changes.has_changed().unwrap());

    let revision = tool
        .edit(
            &url("project://web/build"),
            "b",
            "c",
            &revision,
            any_agent,
            LIMIT,
        )
        .unwrap();
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();
    memory
        .delete(&url("project://web/build"), &revision, Origin::User)
        .unwrap();
    assert!(changes.has_changed().unwrap());
}

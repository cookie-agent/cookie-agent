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

fn any_agent(_: &AgentId) -> AgentMemory {
    AgentMemory::Registered
}

/// When the tests' writes happen, and a time before it.
fn now() -> Timestamp {
    "2026-10-22T12:00:00Z".parse().unwrap()
}

fn earlier() -> Timestamp {
    "2026-10-01T09:12:44Z".parse().unwrap()
}

/// A data root whose memory folder exists, as after a first start.
fn root() -> (tempfile::TempDir, MemoryRoot) {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    (data, memory)
}

fn client(revision: Option<&MemoryRevision>) -> WriteMode<'_> {
    WriteMode::Client(revision)
}

fn model(seen: Option<&MemoryRevision>, new: bool) -> WriteMode<'_> {
    WriteMode::Model { seen, new }
}

fn write_at(
    memory: &MemoryRoot,
    target: &str,
    text: &str,
    mode: WriteMode<'_>,
    limits: &MemoryLimits,
    at: Timestamp,
) -> Result<Written, StoreError> {
    memory.write(&url(target), text, mode, any_agent, limits, at)
}

fn write(
    memory: &MemoryRoot,
    target: &str,
    text: &str,
    mode: WriteMode<'_>,
) -> Result<Written, StoreError> {
    write_at(memory, target, text, mode, &MemoryLimits::all(LIMIT), now())
}

fn edit(
    memory: &MemoryRoot,
    target: &str,
    old: &str,
    new: &str,
) -> Result<MemoryRevision, StoreError> {
    memory
        .edit(
            &url(target),
            old,
            new,
            any_agent,
            &MemoryLimits::all(LIMIT),
            now(),
        )
        .map(|edited| edited.revision)
}

/// Creates `target` with `body` as a client would, at [`earlier`].
fn create(memory: &MemoryRoot, target: &str, body: &str) -> MemoryRevision {
    write_at(
        memory,
        target,
        &text("d", body),
        client(None),
        &MemoryLimits::all(LIMIT),
        earlier(),
    )
    .unwrap()
    .revision
}

fn stamped(description: &str, body: &str, created: Timestamp, modified: Timestamp) -> String {
    MemoryDocument::new(description, vec!["k".to_owned()], body)
        .unwrap()
        .with_stamps(Some(created), Some(modified))
        .render()
}

#[test]
fn a_client_write_creates_then_replaces_only_at_the_read_revision() {
    let (_data, memory) = root();
    let target = url("project://web/build");
    let first = create(&memory, "project://web/build", "one\n");
    let read = memory.read(&target).unwrap();
    assert_eq!(read.entry.revision, first);
    assert_eq!(read.text, stamped("d", "one\n", earlier(), earlier()));
    assert_eq!(read.entry.document.as_ref().unwrap().body(), "one\n");
    assert_eq!(
        (read.entry.created, read.entry.modified),
        (earlier(), earlier())
    );

    // Creating again fails and names the current revision.
    let again = write(
        &memory,
        "project://web/build",
        &text("d", "x"),
        client(None),
    );
    assert!(matches!(again, Err(StoreError::Exists { revision, .. }) if revision == first));

    let second = write(
        &memory,
        "project://web/build",
        &text("d", "two\n"),
        client(Some(&first)),
    )
    .unwrap()
    .revision;
    assert_ne!(second, first);
    // The old revision no longer replaces it.
    let stale = write(
        &memory,
        "project://web/build",
        &text("d", "three\n"),
        client(Some(&first)),
    );
    assert!(
        matches!(stale, Err(StoreError::RevisionMismatch { current, .. }) if current == second)
    );
    // A replaced memory keeps when it was created and is modified now.
    assert_eq!(
        memory.read(&target).unwrap().text,
        stamped("d", "two\n", earlier(), now())
    );

    // Replacing a file that does not exist fails.
    let missing = write(
        &memory,
        "project://web/other",
        &text("d", ""),
        client(Some(&first)),
    );
    assert!(matches!(missing, Err(StoreError::NotFound { .. })));
    assert!(matches!(
        memory.read(&url("project://web/other")),
        Err(StoreError::NotFound { .. })
    ));
}

/// The daemon sets the stamps: what a write gives for them is ignored, and
/// the body stays byte for byte; no temporary file is left.
#[test]
fn a_write_stamps_the_frontmatter_keeps_the_body_and_leaves_no_temporary_file() {
    let (_data, memory) = root();
    let authored =
        "---\n# a comment\ndescription: 'quoted'\ncreated: 1999-01-01T00:00:00Z\n---\nbody\r\n";
    write(&memory, "machine://labpc/gpu", authored, client(None)).unwrap();
    let folder = memory.machine_dir(&MachineName::new("labpc").unwrap());
    let names = fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["gpu.md"]);
    assert_eq!(
        fs::read_to_string(folder.join("gpu.md")).unwrap(),
        format!(
            "---\ndescription: quoted\ncreated: {}\nmodified: {}\n---\nbody\r\n",
            now(),
            now()
        )
    );
}

#[test]
fn invalid_writes_are_refused() {
    let (_data, memory) = root();
    for written in [
        "no frontmatter",
        "---\ndescription: x\nextra: 1\n---\n",
        "---\ndescription: x\n---\n\u{0}",
        "---\ndescription: x\nmodified: soon\n---\n",
    ] {
        let result = write(&memory, "project://web/a", written, client(None));
        assert!(
            matches!(result, Err(StoreError::Invalid { .. })),
            "{written:?}"
        );
    }
}

/// Each kind of memory has its own limit; a change that would leave a
/// memory over it is refused with the current content, and a memory
/// already over it takes only changes that make it smaller (section
/// 16.9.7).
#[test]
fn size_limits_refuse_growth_and_let_an_oversized_memory_shrink() {
    let (data, memory) = root();
    let created = stamped("d", "", now(), now());
    let at = |max| MemoryLimits {
        project_max_bytes: max,
        ..MemoryLimits::all(LIMIT)
    };
    // A body of `x` then `b`s that makes the file `bytes` long.
    let body = |bytes: usize| format!("x{}", "b".repeat(bytes - created.len() - 1));
    let refused = write_at(
        &memory,
        "project://web/a",
        &text("d", &body(101)),
        client(None),
        &at(100),
        now(),
    );
    let Err(error @ StoreError::TooLarge { current: None, .. }) = refused else {
        panic!("{refused:?}");
    };
    assert!(
        error
            .to_string()
            .contains("101 bytes, over its limit of 100 bytes"),
        "{error}"
    );
    let first = write_at(
        &memory,
        "project://web/a",
        &text("d", &body(100)),
        client(None),
        &at(100),
        now(),
    )
    .unwrap()
    .revision;
    // The limit is the kind's: a machine memory of the same size fits its own.
    write_at(
        &memory,
        "machine://labpc/a",
        &text("d", &body(101)),
        client(None),
        &at(100),
        now(),
    )
    .unwrap();

    // Growing past it is refused, showing what the memory holds now.
    let grown = memory.edit(
        &url("project://web/a"),
        "x",
        "xy",
        any_agent,
        &at(100),
        now(),
    );
    let Err(
        error @ StoreError::TooLarge {
            current: Some(_), ..
        },
    ) = grown
    else {
        panic!("{grown:?}");
    };
    assert!(error.to_string().contains("Condense it first"), "{error}");
    assert!(error.to_string().ends_with(&body(100)), "{error}");

    // Lowered under the memory's size, only a smaller memory is accepted.
    let path = data.path().join("memory/projects/web/a.md");
    let size = fs::metadata(&path).unwrap().len() as usize;
    let lowered = at(50);
    let same = write_at(
        &memory,
        "project://web/a",
        &text("d", &body(100)),
        client(Some(&first)),
        &lowered,
        now(),
    );
    assert!(matches!(same, Err(StoreError::TooLarge { .. })), "{same:?}");
    let smaller = memory
        .edit(
            &url("project://web/a"),
            "xb",
            "x",
            any_agent,
            &lowered,
            now(),
        )
        .unwrap()
        .revision;
    assert!((fs::metadata(&path).unwrap().len() as usize) < size);
    write_at(
        &memory,
        "project://web/a",
        &text("d", ""),
        client(Some(&smaller)),
        &lowered,
        now(),
    )
    .unwrap();
}

/// A model replaces a memory only at the revision its session last saw,
/// and its error shows the current content (section 16.9.8).
#[test]
fn a_model_replaces_only_what_its_session_saw() {
    let (_data, memory) = root();
    let first = create(&memory, "user://habits", "- one\n");
    let unseen = write(
        &memory,
        "user://habits",
        &text("Habits", "- two\n"),
        model(None, false),
    );
    let Err(error @ StoreError::Unseen { .. }) = unseen else {
        panic!("{unseen:?}");
    };
    let message = error.to_string();
    assert!(message.contains("this memory changed since you saw it, or you have not read it: read it first, or edit it"), "{message}");
    assert!(
        message.ends_with(&stamped("d", "- one\n", earlier(), earlier())),
        "{message}"
    );
    let stale = MemoryRevision::of(b"older");
    assert!(matches!(
        write(
            &memory,
            "user://habits",
            &text("Habits", "- two\n"),
            model(Some(&stale), false)
        ),
        Err(StoreError::Unseen { .. })
    ));
    let replaced = write(
        &memory,
        "user://habits",
        &text("Habits", "- two\n"),
        model(Some(&first), false),
    )
    .unwrap();
    assert!(!replaced.created);
    // A model may create a user category, and delete one (section 16.9.11).
    let created = write(
        &memory,
        "user://tools",
        &text("Favourite shell aliases", "- jq\n"),
        model(None, false),
    )
    .unwrap();
    assert!(created.created);
    memory.delete(&url("user://tools"), None).unwrap();
    memory.delete(&url("user://feedback"), None).unwrap();
    assert!(matches!(
        memory.read(&url("user://feedback")),
        Err(StoreError::NotFound { .. })
    ));
}

/// A new memory like ones in the same place is refused unless the writer
/// says it is new; one with an existing body is always refused; a weak
/// match is saved and listed (section 16.9.5).
#[test]
fn a_new_memory_is_compared_with_the_memories_around_it() {
    let (_data, memory) = root();
    let release = "---\ndescription: How releases are cut\nkeywords: [release, cargo-dist]\n---\n- tag first\n";
    write(&memory, "project://web/release", release, client(None)).unwrap();
    let elsewhere = "---\ndescription: Elsewhere\nkeywords: [release, cargo-dist, gpu]\n---\n- x\n";
    write(&memory, "machine://labpc/release", elsewhere, client(None)).unwrap();

    let near = "---\ndescription: Publishing\nkeywords: [release, cargo-dist]\n---\n- publish oven-sdk first\n";
    let refused = write(&memory, "project://web/publish", near, model(None, false));
    let Err(error @ StoreError::Similar { .. }) = refused else {
        panic!("{refused:?}");
    };
    let message = error.to_string();
    assert!(
        message.contains("- project://web/release — How releases are cut"),
        "{message}"
    );
    assert!(!message.contains("machine://"), "{message}");
    assert!(message.contains("`new: true`"), "{message}");
    let insisted = write(&memory, "project://web/publish", near, model(None, true)).unwrap();
    assert_eq!(
        insisted
            .near
            .iter()
            .map(|near| near.url.to_string())
            .collect::<Vec<_>>(),
        ["project://web/release"]
    );

    // A weaker match is saved and listed.
    let weak = "---\ndescription: Changelog style\nkeywords: [release, changelog]\n---\n- keep a changelog\n";
    let written = write(&memory, "project://web/changelog", weak, model(None, false)).unwrap();
    assert!(!written.near.is_empty());

    // The same body is refused even when the writer insists.
    let copy = "---\ndescription: Something else\n---\n- tag first\n";
    assert!(matches!(
        write(&memory, "project://web/copy", copy, model(None, true)),
        Err(StoreError::Identical { existing, .. }) if existing.to_string() == "project://web/release"
    ));
    // A client's write is never compared.
    write(&memory, "project://web/copy", copy, client(None)).unwrap();
}

#[test]
fn creating_an_agent_file_needs_an_agent() {
    let (_data, memory) = root();
    let unknown = memory.write(
        &url("agent://ghost"),
        &text("d", ""),
        client(None),
        |_: &AgentId| AgentMemory::Unknown,
        &MemoryLimits::all(LIMIT),
        now(),
    );
    assert!(
        matches!(unknown, Err(StoreError::Refused(_))),
        "{unknown:?}"
    );
    write(&memory, "agent://reviewer", &text("d", ""), client(None)).unwrap();
}

#[test]
fn a_delete_needs_the_current_revision_only_when_given_one() {
    let (_data, memory) = root();
    let target = url("machine://labpc/gpu");
    let first = create(&memory, "machine://labpc/gpu", "a");
    let stale = MemoryRevision::of(b"something else");
    assert!(matches!(
        memory.delete(&target, Some(&stale)),
        Err(StoreError::RevisionMismatch { current, .. }) if current == first
    ));
    memory.delete(&target, Some(&first)).unwrap();
    assert!(matches!(
        memory.delete(&target, None),
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
fn a_project_folder_stays_when_its_last_memory_goes() {
    let (data, memory) = root();
    let folder = data.path().join("memory/projects/web");
    create(&memory, "project://web/a", "");
    assert!(folder.is_dir());
    memory.delete(&url("project://web/a"), None).unwrap();
    // The folder belongs to the project, not to its memories.
    assert!(folder.is_dir());
}

/// Owners that know the agent `default` and the projects `web` and `api`.
struct Owners;

impl MemoryOwners for Owners {
    fn agent(&self, _: &AgentId) -> AgentMemory {
        AgentMemory::Registered
    }

    fn unknown_project(&self, project: &MemoryName) -> Option<Vec<String>> {
        let known = vec!["api".to_owned(), "web".to_owned()];
        (!known.iter().any(|name| name == project.as_str())).then_some(known)
    }
}

#[test]
fn writes_to_a_project_that_does_not_exist_are_refused() {
    let (data, memory) = root();
    let limits = MemoryLimits::all(LIMIT);
    for mode in [client(None), model(None, true)] {
        let refused = memory.write(
            &url("project://shop/build"),
            &text("d", "x"),
            mode,
            Owners,
            &limits,
            now(),
        );
        let Err(StoreError::Refused(reason)) = refused else {
            panic!("{refused:?}");
        };
        assert!(reason.contains("The projects are api, web"), "{reason}");
        assert!(reason.contains("created from a client"), "{reason}");
    }
    assert!(!data.path().join("memory/projects/shop").exists());
    memory
        .write(
            &url("project://web/build"),
            &text("d", "x"),
            client(None),
            Owners,
            &limits,
            now(),
        )
        .unwrap();
    // An edit is a write too.
    fs::create_dir_all(data.path().join("memory/projects/shop")).unwrap();
    fs::write(
        data.path().join("memory/projects/shop/old.md"),
        text("d", "old"),
    )
    .unwrap();
    assert!(matches!(
        memory.edit(
            &url("project://shop/old"),
            "old",
            "new",
            Owners,
            &limits,
            now()
        ),
        Err(StoreError::Refused(_))
    ));
    memory
        .edit(
            &url("project://web/build"),
            "x",
            "y",
            Owners,
            &limits,
            now(),
        )
        .unwrap();
}

#[test]
fn a_project_overview_keeps_the_daemons_description() {
    let (_data, memory) = root();
    let overview = url("project://web/overview");
    let limits = MemoryLimits::all(LIMIT);
    let written = memory
        .write(
            &overview,
            &text("Mine", "A web shop.\n"),
            client(None),
            Owners,
            &limits,
            now(),
        )
        .unwrap();
    let kept = written.kept.expect("the description was kept");
    assert_eq!(
        kept.description.as_deref(),
        Some("Overview of the `web` project, loaded in full into its sessions")
    );
    let read = memory.read(&overview).unwrap();
    let document = read.entry.document.unwrap();
    assert_eq!(
        document.description(),
        "Overview of the `web` project, loaded in full into its sessions"
    );
    assert_eq!(document.keywords(), ["k"]);
    assert_eq!(document.body(), "A web shop.\n");
    let revision = memory
        .edit(&overview, "web", "book", Owners, &limits, now())
        .unwrap();
    let edited = memory.read(&overview).unwrap();
    assert_eq!(edited.entry.revision, revision.revision);
    assert_eq!(
        edited.entry.document.unwrap().description(),
        "Overview of the `web` project, loaded in full into its sessions"
    );
}

/// An edit applies whenever its `old` text occurs exactly once in the
/// current body, whatever else changed (section 16.9.8).
#[test]
fn an_edit_replaces_exactly_one_occurrence() {
    let (_data, memory) = root();
    let target = url("project://web/a");
    create(&memory, "project://web/a", "alpha beta\nbeta gamma\n");
    for (old, expected) in [
        ("delta", "does not occur"),
        ("beta", "occurs 2 times"),
        ("", "does not occur"),
    ] {
        let error = edit(&memory, "project://web/a", old, "x").unwrap_err();
        assert!(error.to_string().contains(expected), "{old}: {error}");
    }
    // The frontmatter is not part of the body.
    let error = edit(&memory, "project://web/a", "description", "x").unwrap_err();
    assert!(matches!(error, StoreError::NoOccurrence { .. }));

    let second = edit(&memory, "project://web/a", "alpha", "ALPHA").unwrap();
    let read = memory.read(&target).unwrap();
    assert_eq!(read.entry.revision, second);
    assert_eq!(
        read.text,
        stamped("d", "ALPHA beta\nbeta gamma\n", earlier(), now())
    );
    // Another change does not stop an edit that does not touch it.
    edit(&memory, "project://web/a", "gamma", "GAMMA").unwrap();
    assert!(matches!(
        edit(&memory, "project://web/a", "gamma", "\u{7}"),
        Err(StoreError::NoOccurrence { .. })
    ));
    assert!(matches!(
        edit(&memory, "project://web/a", "GAMMA", "\u{7}"),
        Err(StoreError::Invalid { .. })
    ));
}

#[test]
fn editing_an_invalid_file_is_refused() {
    let (data, memory) = root();
    let path = data.path().join("memory/projects/web/a.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "no frontmatter alpha").unwrap();
    let revision = MemoryRevision::of(b"no frontmatter alpha");
    assert!(matches!(
        edit(&memory, "project://web/a", "alpha", "beta"),
        Err(StoreError::Invalid { .. })
    ));
    // Writing the whole file at its revision repairs it.
    write(
        &memory,
        "project://web/a",
        &text("d", ""),
        client(Some(&revision)),
    )
    .unwrap();
}

/// A file written before stamps reads with its modification time, and
/// the start stamps it so, adding only the missing lines (section 16.9.6).
#[test]
fn files_without_stamps_get_their_modification_time() {
    let (data, memory) = root();
    let path = data.path().join("memory/machines/labpc/gpu.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let authored = "---\n# kept\ndescription: GPU\nmodified: 2026-10-20T16:03:10Z\n---\nbody\n";
    fs::write(&path, authored).unwrap();
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(std::time::SystemTime::from(earlier()))
        .unwrap();
    drop(file);
    let read = memory.read(&url("machine://labpc/gpu")).unwrap();
    let modified: Timestamp = "2026-10-20T16:03:10Z".parse().unwrap();
    assert_eq!(
        (read.entry.created, read.entry.modified),
        (earlier(), modified)
    );

    let changes = memory.subscribe();
    // The default categories have stamps already.
    assert_eq!(memory.stamp_unstamped().unwrap(), 1);
    assert!(changes.has_changed().unwrap());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!(
            "---\n# kept\ndescription: GPU\nmodified: 2026-10-20T16:03:10Z\ncreated: {}\n---\nbody\n",
            earlier()
        )
    );
    let read = memory.read(&url("machine://labpc/gpu")).unwrap();
    assert_eq!(
        (read.entry.created, read.entry.modified),
        (earlier(), modified)
    );
    assert_eq!(memory.stamp_unstamped().unwrap(), 0);
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
        &format!("{valid}{}", "x".repeat(MAX_FILE_BYTES)),
    );
    put("projects/web_1/a.md", &valid);
    put("stray.txt", "x");

    let listing = memory.list(&ListScope::All).unwrap();
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
            .contains("more than any memory may be")
    );
    assert_eq!(reason("project://web/a"), None);
    let big = listing
        .entries
        .iter()
        .find(|entry| entry.url.to_string() == "project://web/big")
        .unwrap();
    assert_eq!(big.size, (valid.len() + MAX_FILE_BYTES) as u64);
    assert_eq!(
        big.revision,
        MemoryRevision::of(format!("{valid}{}", "x".repeat(MAX_FILE_BYTES)).as_bytes())
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
            .list(&scope)
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
    let listing = memory.list(&ListScope::All).unwrap();
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
    create(&tool, "project://web/build", "b");
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();

    // A refused write changes nothing and signals nothing.
    let refused = write(&tool, "project://web/build", &text("d", "c"), client(None));
    assert!(matches!(refused, Err(StoreError::Exists { .. })));
    assert!(!changes.has_changed().unwrap());

    edit(&tool, "project://web/build", "b", "c").unwrap();
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();
    memory.delete(&url("project://web/build"), None).unwrap();
    assert!(changes.has_changed().unwrap());
}

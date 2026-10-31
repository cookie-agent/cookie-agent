use std::{collections::BTreeMap, fs};

use super::*;
use crate::{file::MemoryRevision, limits::MemoryLimits, store::WriteMode};

const LIMIT: usize = 4096;

/// When the tests' writes happen.
fn now() -> jiff::Timestamp {
    "2026-10-22T12:00:00Z".parse().unwrap()
}

/// `text` without its stamp lines, which sync writes with the time.
fn unstamped(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.starts_with("created: ") && !line.starts_with("modified: "))
        .collect()
}

/// The text of `agent`'s file, without its stamps.
fn file_text(data: &tempfile::TempDir, agent: &str) -> String {
    unstamped(&fs::read_to_string(path(data, agent)).unwrap())
}

fn id(value: &str) -> AgentId {
    AgentId::new(value).unwrap()
}

/// A data root whose memory folder exists, as after a first start.
fn root() -> (tempfile::TempDir, MemoryRoot) {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    (data, memory)
}

fn path(data: &tempfile::TempDir, agent: &str) -> std::path::PathBuf {
    data.path().join(format!("memory/agents/{agent}.md"))
}

fn desired(agents: &[(&str, &str, &'static str)]) -> BTreeMap<AgentId, AgentMemorySpec> {
    agents
        .iter()
        .map(|(agent, description, mode)| {
            (
                id(agent),
                AgentMemorySpec::new(&id(agent), description, mode),
            )
        })
        .collect()
}

#[test]
fn the_description_names_the_agent_on_one_line_within_the_limit() {
    let reviewer = id("reviewer");
    assert_eq!(
        AgentMemorySpec::new(&reviewer, "Reviews code", "subagent").description,
        "Memory of the `reviewer` agent: Reviews code"
    );
    assert_eq!(
        AgentMemorySpec::new(
            &reviewer,
            "  Reviews\ncode,\r\n\ttests \u{7}and\u{85}docs  ",
            "all"
        )
        .description,
        "Memory of the `reviewer` agent: Reviews code, tests and docs"
    );
    assert_eq!(
        AgentMemorySpec::new(&reviewer, " \n", "primary").description,
        "Memory of the `reviewer` agent"
    );
    let prefix = "Memory of the `reviewer` agent: ";
    let fits = "é".repeat(200 - prefix.chars().count());
    assert_eq!(
        AgentMemorySpec::new(&reviewer, &fits, "primary").description,
        format!("{prefix}{fits}")
    );
    let cut = AgentMemorySpec::new(&reviewer, &format!("{fits}x"), "primary").description;
    assert_eq!(cut.chars().count(), 200);
    assert!(cut.ends_with("é…"), "{cut}");
    // Every description the daemon writes is a valid one.
    for description in [cut.as_str(), "Memory of the `reviewer` agent"] {
        assert!(MemoryDocument::new(description, Vec::new(), "").is_ok());
    }
}

#[test]
fn the_daemon_keywords_come_first_and_extras_are_kept() {
    let reviewer = id("reviewer");
    let spec = AgentMemorySpec::new(&reviewer, "Reviews", "subagent");
    let keywords = |existing: &[&str]| {
        spec.keywords(
            &reviewer,
            &existing
                .iter()
                .map(|keyword| (*keyword).to_owned())
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(keywords(&[]), ["reviewer", "subagent"]);
    assert_eq!(
        keywords(&["rust", "Reviewer", "primary", "subagent", "RUST", "msrv"]),
        ["reviewer", "subagent", "rust", "msrv"]
    );
    let many = (0..40).map(|index| format!("k{index}")).collect::<Vec<_>>();
    let kept = spec.keywords(&reviewer, &many);
    assert_eq!(kept.len(), MAX_KEYWORDS);
    assert_eq!(kept[..3], ["reviewer", "subagent", "k0"]);
    // An agent named like its mode gets the one keyword.
    let primary = id("primary");
    assert_eq!(
        AgentMemorySpec::new(&primary, "Main", "primary").keywords(&primary, &[]),
        ["primary"]
    );
}

#[test]
fn a_sync_creates_one_empty_file_per_agent_and_then_does_nothing() {
    let (data, memory) = root();
    let mut changes = memory.subscribe();
    let agents = desired(&[
        ("default", "The general agent", "primary"),
        ("reviewer", "Reviews code", "subagent"),
    ]);
    let sync = memory.sync_agents(|| agents.clone(), LIMIT).unwrap();
    assert_eq!(sync.created, [id("default"), id("reviewer")]);
    assert!(sync.updated.is_empty() && sync.removed.is_empty());
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();
    assert_eq!(
        file_text(&data, "reviewer"),
        "---\ndescription: \"Memory of the `reviewer` agent: Reviews code\"\nkeywords: [reviewer, subagent]\n---\n"
    );
    let read = memory
        .read(&MemoryUrl::Agent {
            agent: id("default"),
        })
        .unwrap();
    let document = read.entry.document.unwrap();
    assert_eq!(
        document.description(),
        "Memory of the `default` agent: The general agent"
    );
    assert_eq!(document.body(), "");

    // Nothing to do writes nothing and signals nothing.
    let before = fs::metadata(path(&data, "reviewer"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(memory.sync_agents(|| agents, LIMIT).unwrap().is_empty());
    assert!(!changes.has_changed().unwrap());
    assert_eq!(
        fs::metadata(path(&data, "reviewer"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}

#[test]
fn a_sync_rewrites_the_frontmatter_and_keeps_the_body_and_extra_keywords() {
    let (data, memory) = root();
    memory
        .sync_agents(
            || desired(&[("reviewer", "Reviews code", "subagent")]),
            LIMIT,
        )
        .unwrap();
    // Someone edits the file by hand: another description, the daemon's
    // keywords gone, an extra one, and a body with CRLF line endings.
    let edited = "---\r\ndescription: Mine\r\nkeywords: [rust]\r\n---\r\n- check twice\r\n";
    fs::write(path(&data, "reviewer"), edited).unwrap();
    let sync = memory
        .sync_agents(
            || desired(&[("reviewer", "Reviews Rust code", "all")]),
            LIMIT,
        )
        .unwrap();
    assert_eq!(sync.updated, [id("reviewer")]);
    assert_eq!(
        file_text(&data, "reviewer"),
        "---\ndescription: \"Memory of the `reviewer` agent: Reviews Rust code\"\nkeywords: [reviewer, all, rust]\n---\n- check twice\r\n"
    );
    // An invalid file is left for the user to fix.
    fs::write(path(&data, "reviewer"), "no frontmatter").unwrap();
    assert!(
        memory
            .sync_agents(|| desired(&[("reviewer", "Other", "all")]), LIMIT)
            .unwrap()
            .is_empty()
    );
    assert_eq!(file_text(&data, "reviewer"), "no frontmatter");
}

#[test]
fn an_agent_that_goes_takes_only_its_untouched_file_with_it() {
    let (data, memory) = root();
    let all = desired(&[
        ("empty", "Untouched", "subagent"),
        ("noted", "Has notes", "subagent"),
        ("tagged", "Has a keyword", "primary"),
        ("primary", "Main", "primary"),
    ]);
    memory.sync_agents(|| all.clone(), LIMIT).unwrap();
    fs::write(
        path(&data, "noted"),
        "---\ndescription: \"Memory of the `noted` agent: Has notes\"\nkeywords: [noted, subagent]\n---\n- a lesson\n",
    )
    .unwrap();
    fs::write(
        path(&data, "tagged"),
        "---\ndescription: \"Memory of the `tagged` agent: Has a keyword\"\nkeywords: [tagged, primary, extra]\n---\n",
    )
    .unwrap();
    // A file the user wrote for an agent that never existed stays too.
    fs::write(
        path(&data, "mine"),
        "---\ndescription: My own notes\nkeywords: [mine, primary]\n---\n",
    )
    .unwrap();
    let mut changes = memory.subscribe();
    changes.mark_unchanged();
    let sync = memory
        .sync_agents(|| desired(&[("noted", "Has notes", "subagent")]), LIMIT)
        .unwrap();
    assert_eq!(sync.removed, [id("empty"), id("primary")]);
    assert!(sync.created.is_empty() && sync.updated.is_empty());
    assert!(changes.has_changed().unwrap());
    for gone in ["empty", "primary"] {
        assert!(!path(&data, gone).exists(), "{gone}");
    }
    for kept in ["noted", "tagged", "mine"] {
        assert!(path(&data, kept).is_file(), "{kept}");
    }
    // Coming back recreates it.
    assert_eq!(
        memory.sync_agents(|| all, LIMIT).unwrap().created,
        [id("empty"), id("primary")]
    );
}

#[test]
fn nothing_is_synced_without_a_memory_folder() {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    let sync = memory
        .sync_agents(|| desired(&[("reviewer", "Reviews", "subagent")]), LIMIT)
        .unwrap();
    assert!(sync.is_empty());
    assert!(!data.path().join("memory").exists());
}

/// Writes keep the daemon's description and keywords of a managed agent
/// file; an agent file the daemon does not keep keeps its description.
#[test]
fn writes_and_edits_keep_what_the_daemon_owns() {
    let (data, memory) = root();
    let agents = desired(&[("reviewer", "Reviews code", "subagent")]);
    memory.sync_agents(|| agents.clone(), LIMIT).unwrap();
    let url = MemoryUrl::Agent {
        agent: id("reviewer"),
    };
    let managed = |_: &AgentId| AgentMemory::Managed(agents[&id("reviewer")].clone());
    let current = memory.read(&url).unwrap().entry.revision;
    let written = memory
        .write(
            &url,
            "---\ndescription: Mine now\nkeywords: [rust]\n---\n- a lesson\r\n",
            WriteMode::Client(Some(&current)),
            managed,
            &MemoryLimits::all(LIMIT),
            now(),
        )
        .unwrap();
    let text = file_text(&data, "reviewer");
    assert_eq!(
        text,
        "---\ndescription: \"Memory of the `reviewer` agent: Reviews code\"\nkeywords: [reviewer, subagent, rust]\n---\n- a lesson\r\n"
    );
    assert_eq!(
        written.revision,
        MemoryRevision::of(&fs::read(path(&data, "reviewer")).unwrap())
    );
    let kept = written.kept.unwrap();
    assert_eq!(
        kept.description.as_deref(),
        Some("Memory of the `reviewer` agent: Reviews code")
    );
    assert_eq!(kept.keywords, ["reviewer", "subagent"]);
    assert_eq!(
        kept.describe(&url),
        "The description of agent://reviewer is the daemon's and cannot be changed, so it stays \"Memory of the `reviewer` agent: Reviews code\". Its keywords always include `reviewer` and `subagent`, so they were added back; other keywords are kept."
    );

    // Text that already matches is written as given.
    let again = memory
        .write(
            &url,
            &text,
            WriteMode::Client(Some(&written.revision)),
            managed,
            &MemoryLimits::all(LIMIT),
            now(),
        )
        .unwrap();
    assert_eq!(again.kept, None);

    // An edit that hand-edited frontmatter slipped past gets it back.
    fs::write(
        path(&data, "reviewer"),
        "---\ndescription: Stale\n---\n- a lesson\n",
    )
    .unwrap();
    memory
        .edit(
            &url,
            "a lesson",
            "two lessons",
            managed,
            &MemoryLimits::all(LIMIT),
            now(),
        )
        .unwrap();
    assert_eq!(
        file_text(&data, "reviewer"),
        "---\ndescription: \"Memory of the `reviewer` agent: Reviews code\"\nkeywords: [reviewer, subagent]\n---\n- two lessons\n"
    );

    // An orphaned file keeps the description it has.
    fs::write(
        path(&data, "gone"),
        "---\ndescription: Old notes\n---\n- kept\n",
    )
    .unwrap();
    let gone = MemoryUrl::Agent { agent: id("gone") };
    let revision = memory.read(&gone).unwrap().entry.revision;
    let written = memory
        .write(
            &gone,
            "---\ndescription: New\nkeywords: [x]\n---\n- changed\n",
            WriteMode::Client(Some(&revision)),
            |_: &cookie_agent_protocol::AgentId| AgentMemory::Unknown,
            &MemoryLimits::all(LIMIT),
            now(),
        )
        .unwrap();
    assert_eq!(
        file_text(&data, "gone"),
        "---\ndescription: Old notes\nkeywords: [x]\n---\n- changed\n"
    );
    assert_eq!(
        written.kept.unwrap().description.as_deref(),
        Some("Old notes")
    );
}

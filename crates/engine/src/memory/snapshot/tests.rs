use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use cookie_agent_protocol::{AgentId, AttachmentName, MachineName};

use super::{ReadRules, SnapshotRequest};
use crate::memory::{MemoryRoot, limits::MemoryLimits, render};

const MAX_FILE: usize = 4096;

/// When the tests' snapshots are taken.
fn now() -> jiff::Timestamp {
    "2026-10-22T12:00:00Z".parse().unwrap()
}

/// Writes `text` at `path` in the memory folder, `age_seconds` old.
fn put(root: &Path, path: &str, text: &str, age_seconds: u64) {
    let path = root.join("memory").join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 - age_seconds))
        .unwrap();
}

fn file(description: &str, body: &str) -> String {
    format!("---\ndescription: {description}\nkeywords: [k]\n---\n{body}")
}

/// A memory folder holding only what each test puts there.
fn root() -> (tempfile::TempDir, MemoryRoot) {
    let data = tempfile::tempdir().unwrap();
    fs::create_dir_all(data.path().join("memory/user")).unwrap();
    let memory = MemoryRoot::new(data.path());
    (data, memory)
}

fn request(delegated: bool, machines: &[(&str, &[&str])], limits: MemoryLimits) -> SnapshotRequest {
    SnapshotRequest {
        project: None,
        agent: AgentId::new("reviewer").unwrap(),
        delegated,
        writable: true,
        machines: machines
            .iter()
            .map(|(machine, attachments)| {
                (
                    MachineName::new(*machine).unwrap(),
                    attachments
                        .iter()
                        .map(|attachment| AttachmentName::new(*attachment).unwrap())
                        .collect(),
                )
            })
            .collect(),
        limits,
        now: now(),
        read: ReadRules::all(),
        rules: None,
    }
}

fn limits(user_bytes: usize, index_bytes: usize) -> MemoryLimits {
    MemoryLimits {
        snapshot_user_bytes: user_bytes,
        snapshot_index_bytes: index_bytes,
        ..MemoryLimits::all(MAX_FILE)
    }
}

#[test]
fn a_root_snapshot_holds_user_agent_and_machines() {
    let (data, memory) = root();
    put(
        data.path(),
        "user/feedback.md",
        &file("Feedback", "- a\n"),
        0,
    );
    put(data.path(), "user/profile.md", &file("Profile", ""), 0);
    put(data.path(), "user/broken.md", "no frontmatter", 0);
    put(
        data.path(),
        "agents/reviewer.md",
        &file("Reviews", "- b\n"),
        0,
    );
    put(data.path(), "agents/default.md", &file("Other", "- c\n"), 0);
    put(data.path(), "machines/labpc/old.md", &file("Old", ""), 20);
    put(data.path(), "machines/labpc/new.md", &file("New", ""), 10);
    put(
        data.path(),
        "machines/other/x.md",
        &file("Elsewhere", ""),
        0,
    );
    put(data.path(), "projects/web/build.md", &file("Build", ""), 0);

    let snapshot = memory
        .snapshot_only(&request(
            false,
            &[("laptop", &["local_web"]), ("labpc", &["labpc_api"])],
            limits(12_288, 4096),
        ))
        .unwrap();
    assert!(!snapshot.delegated && snapshot.writable);
    // Invalid files are skipped; categories are in name order.
    let categories = snapshot
        .user
        .iter()
        .map(|category| category.category.as_str())
        .collect::<Vec<_>>();
    assert_eq!(categories, ["feedback", "profile"]);
    assert!(snapshot.user_omitted.is_empty());
    let agent = snapshot.agent.as_ref().unwrap();
    assert_eq!(agent.agent.as_str(), "reviewer");
    assert_eq!(agent.memory.as_ref().unwrap().body, "- b\n");
    // Machines in session order, memories newest first; other machines
    // and projects are never in the snapshot.
    let machines = snapshot
        .machines
        .iter()
        .map(|machine| {
            (
                machine.machine.as_str(),
                machine
                    .memories
                    .iter()
                    .map(|memory| memory.url.as_str())
                    .collect::<Vec<_>>(),
                machine.omitted,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        machines,
        [
            ("laptop", Vec::new(), 0),
            (
                "labpc",
                vec!["machine://labpc/new", "machine://labpc/old"],
                0
            ),
        ]
    );
}

#[test]
fn a_delegated_snapshot_holds_only_its_agent() {
    let (data, memory) = root();
    put(
        data.path(),
        "user/feedback.md",
        &file("Feedback", "- a\n"),
        0,
    );
    put(data.path(), "machines/labpc/x.md", &file("X", ""), 0);
    let snapshot = memory
        .snapshot_only(&request(true, &[("labpc", &[])], limits(12_288, 4096)))
        .unwrap();
    assert!(snapshot.delegated);
    assert!(snapshot.user.is_empty() && snapshot.user_omitted.is_empty());
    assert!(snapshot.machines.is_empty());
    let agent = snapshot.agent.unwrap();
    assert_eq!(agent.agent.as_str(), "reviewer");
    assert_eq!(agent.memory, None);
    assert!(!agent.omitted);
}

#[test]
fn user_categories_that_do_not_fit_are_named_from_the_first_that_does_not() {
    let (data, memory) = root();
    put(data.path(), "user/a.md", &file("A", &"a".repeat(100)), 0);
    put(data.path(), "user/b.md", &file("B", &"b".repeat(400)), 0);
    put(data.path(), "user/c.md", &file("C", "c"), 0);
    let sized = |fitting: usize, omitted: &[&str]| {
        let snapshot = memory
            .snapshot_only(&request(false, &[], limits(MAX_FILE, 4096)))
            .unwrap();
        let mut user = snapshot.user;
        user.truncate(fitting);
        render::user_section(
            &user,
            &omitted
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            &[],
            Some(now()),
        )
        .len()
    };
    // Exactly the bytes `a` alone needs, with `b` and `c` named: `b` does
    // not fit, so `c`, which would, is omitted too.
    let only_a = sized(1, &["b", "c"]);
    let snapshot = memory
        .snapshot_only(&request(false, &[], limits(only_a, 4096)))
        .unwrap();
    assert_eq!(snapshot.user.len(), 1);
    assert_eq!(snapshot.user[0].category, "a");
    assert_eq!(snapshot.user_omitted, ["b", "c"]);
    assert_eq!(
        render::user_section(&snapshot.user, &snapshot.user_omitted, &[], Some(now())).len(),
        only_a
    );
    // One byte less and nothing fits.
    let snapshot = memory
        .snapshot_only(&request(false, &[], limits(only_a - 1, 4096)))
        .unwrap();
    assert!(snapshot.user.is_empty());
    assert_eq!(snapshot.user_omitted, ["a", "b", "c"]);
}

#[test]
fn machine_memories_past_the_index_limit_are_counted() {
    let (data, memory) = root();
    put(data.path(), "machines/laptop/one.md", &file("One", ""), 30);
    put(data.path(), "machines/labpc/two.md", &file("Two", ""), 20);
    put(
        data.path(),
        "machines/labpc/three.md",
        &file("Three", ""),
        10,
    );
    let machines: &[(&str, &[&str])] = &[("laptop", &[]), ("labpc", &[])];
    let full = memory
        .snapshot_only(&request(false, machines, limits(12_288, 4096)))
        .unwrap()
        .machines;
    // Room for the first machine's memory and the newest of the second.
    let mut fitted = full.clone();
    fitted[1].memories.truncate(1);
    fitted[1].omitted = 1;
    let room = render::machines_section(&fitted, Some(now())).len();
    let snapshot = memory
        .snapshot_only(&request(false, machines, limits(12_288, room)))
        .unwrap();
    assert_eq!(snapshot.machines, fitted);
    assert_eq!(
        snapshot.machines[1].memories[0].url,
        "machine://labpc/three"
    );
    let snapshot = memory
        .snapshot_only(&request(false, machines, limits(12_288, room - 1)))
        .unwrap();
    assert_eq!(snapshot.machines[0].memories.len(), 1);
    assert!(snapshot.machines[1].memories.is_empty());
    assert_eq!(snapshot.machines[1].omitted, 2);
}

/// The agent's file shows whole while its escaped text fits the room the
/// other snapshot limits leave, even over its own limit (section 16.9.7).
#[test]
fn an_agent_file_shows_while_it_fits_the_room_the_other_limits_leave() {
    let (data, memory) = root();
    // 60 `<memory` tags: 420 bytes of body, 600 escaped.
    let body = "<memory".repeat(60);
    let text = file("Reviews", &body);
    put(data.path(), "agents/reviewer.md", &text, 0);
    let fits = render::agent_content_bytes("Reviews", &body);
    assert!(text.len() < fits);
    let room = cookie_agent_config::MemoryConfig::MAX_LIMITS_SUM;
    // Limits that leave `left` bytes of the event's room for the agent.
    let limits_at = |agent_max_bytes, left| MemoryLimits {
        agent_max_bytes,
        ..limits(4096, room - 4096 - left)
    };
    // Over its 100-byte limit, it shows while it fits, with its fullness.
    let shown = memory
        .snapshot(&request(true, &[], limits_at(100, fits)))
        .unwrap();
    let agent = shown.snapshot.agent.unwrap();
    let status = &agent.memory.as_ref().unwrap().status;
    assert!(status.over_limit() && !agent.omitted);
    assert_eq!(status.max_size, Some(100));
    assert_eq!(status.size, Some(text.len() as u64));
    assert_eq!(
        shown
            .shown
            .iter()
            .map(|(url, _)| url.to_string())
            .collect::<Vec<_>>(),
        ["agent://reviewer"]
    );
    let omitted = memory
        .snapshot(&request(true, &[], limits_at(100, fits - 1)))
        .unwrap();
    let agent = omitted.snapshot.agent.unwrap();
    assert!(agent.memory.is_none() && agent.omitted);
    assert!(omitted.shown.is_empty());
}

fn in_project(project: &str, limits: MemoryLimits) -> SnapshotRequest {
    SnapshotRequest {
        project: Some(cookie_agent_protocol::ProjectName::new(project).unwrap()),
        ..request(false, &[("laptop", &[])], limits)
    }
}

#[test]
fn a_project_snapshot_holds_the_overview_whole_and_an_index_of_the_rest() {
    let (data, memory) = root();
    put(
        data.path(),
        "projects/web/overview.md",
        &file("Overview of web", "A web shop.\n"),
        50,
    );
    put(data.path(), "projects/web/old.md", &file("Old", ""), 40);
    put(data.path(), "projects/web/new.md", &file("New", ""), 10);
    put(data.path(), "projects/web/broken.md", "no frontmatter", 0);
    put(data.path(), "projects/api/other.md", &file("Other", ""), 0);
    let snapshot = memory
        .snapshot_only(&in_project("web", limits(12_288, 4096)))
        .unwrap();
    let project = snapshot.project.unwrap();
    assert_eq!(project.project.as_str(), "web");
    let overview = project.overview.unwrap();
    assert_eq!(
        (overview.description.as_str(), overview.body.as_str()),
        ("Overview of web", "A web shop.\n")
    );
    // Newest first; the overview and invalid files are not listed.
    assert_eq!(
        project
            .memories
            .iter()
            .map(|memory| memory.url.as_str())
            .collect::<Vec<_>>(),
        ["project://web/new", "project://web/old"]
    );
    assert_eq!(project.omitted, 0);

    // An empty overview is absent.
    put(
        data.path(),
        "projects/web/overview.md",
        &file("Overview of web", "  \n"),
        50,
    );
    let snapshot = memory
        .snapshot_only(&in_project("web", limits(12_288, 4096)))
        .unwrap();
    assert_eq!(snapshot.project.unwrap().overview, None);
    // A delegated run has no project.
    let delegated = SnapshotRequest {
        delegated: true,
        ..in_project("web", limits(12_288, 4096))
    };
    assert_eq!(memory.snapshot_only(&delegated).unwrap().project, None);
}

#[test]
fn the_overview_shares_the_user_limit_and_the_index_the_machines_limit() {
    let (data, memory) = root();
    put(
        data.path(),
        "user/feedback.md",
        &file("Feedback", "- a\n"),
        0,
    );
    put(
        data.path(),
        "projects/web/overview.md",
        &file("Overview of web", "A web shop.\n"),
        0,
    );
    put(data.path(), "projects/web/a.md", &file("A", ""), 20);
    put(data.path(), "projects/web/b.md", &file("B", ""), 10);
    put(data.path(), "machines/laptop/m.md", &file("M", ""), 0);
    let full = memory
        .snapshot_only(&in_project("web", limits(12_288, 4096)))
        .unwrap();
    let project = full.project.clone().unwrap();
    let overview_bytes = render::project_overview(
        &project.project,
        project.overview.as_ref().unwrap(),
        Some(now()),
    )
    .len();
    let user_bytes = render::user_section(&full.user, &[], &[], Some(now())).len();

    // The overview comes first: with room for it but not for the feedback
    // category as well, the category is named, not shown.
    let snapshot = memory
        .snapshot_only(&in_project(
            "web",
            limits(overview_bytes + user_bytes - 1, 4096),
        ))
        .unwrap();
    assert!(snapshot.project.as_ref().unwrap().overview.is_some());
    assert_eq!(snapshot.user_omitted, ["feedback"]);
    // An overview too long for the user limit is listed instead.
    let snapshot = memory
        .snapshot_only(&in_project("web", limits(overview_bytes - 1, 4096)))
        .unwrap();
    let project = snapshot.project.unwrap();
    assert_eq!(project.overview, None);
    assert_eq!(project.memories[0].url, "project://web/overview");

    // The project's index comes before the machines'.
    let index_bytes = render::project_index(full.project.as_ref().unwrap(), Some(now())).len();
    let snapshot = memory
        .snapshot_only(&in_project("web", limits(12_288, index_bytes)))
        .unwrap();
    assert_eq!(snapshot.project.unwrap().memories.len(), 2);
    assert!(snapshot.machines[0].memories.is_empty());
    assert_eq!(snapshot.machines[0].omitted, 1);
}

/// The run's `memory` rules filter the snapshot (section 7.5): a memory
/// they allow is shown, one they ask for is listed by URL and description
/// only, and one they deny is left out and not counted.
#[test]
fn the_read_rules_show_list_or_leave_out_each_memory() {
    use crate::memory::{search::Visibility, url::MemoryUrl};

    let (data, memory) = root();
    put(
        data.path(),
        "user/feedback.md",
        &file("Feedback", "- a\n"),
        0,
    );
    put(
        data.path(),
        "user/preferences.md",
        &file("Preferences", "- secret preference\n"),
        0,
    );
    put(
        data.path(),
        "agents/reviewer.md",
        &file("How reviews go", "- secret lesson\n"),
        0,
    );
    put(data.path(), "machines/laptop/a.md", &file("A", ""), 10);
    put(
        data.path(),
        "machines/laptop/b.md",
        &file("Hidden B", ""),
        0,
    );
    put(
        data.path(),
        "projects/web/overview.md",
        &file("Overview of web", "A secret shop.\n"),
        0,
    );
    put(data.path(), "projects/web/x.md", &file("X", ""), 5);
    put(data.path(), "projects/web/y.md", &file("Hidden Y", ""), 5);
    let read = ReadRules::new(|url: &MemoryUrl| match url.to_string().as_str() {
        "user://preferences" | "machine://laptop/b" | "project://web/y" => Visibility::Hidden,
        "agent://reviewer" | "project://web/overview" => Visibility::Summary,
        _ => Visibility::Full,
    });
    let snapshot = memory
        .snapshot_only(&SnapshotRequest {
            read: read.clone(),
            ..in_project("web", limits(12_288, 4096))
        })
        .unwrap();

    // A denied category is neither shown, named, nor listed.
    assert_eq!(
        snapshot
            .user
            .iter()
            .map(|category| category.category.as_str())
            .collect::<Vec<_>>(),
        ["feedback"]
    );
    assert!(snapshot.user_omitted.is_empty() && snapshot.user_listed.is_empty());
    // An agent file the rules ask for is listed only.
    let agent = snapshot.agent.clone().unwrap();
    assert_eq!(agent.memory, None);
    assert!(!agent.omitted);
    assert_eq!(
        agent
            .listed
            .as_ref()
            .map(|listed| (listed.url.as_str(), listed.description.as_str())),
        Some(("agent://reviewer", "How reviews go"))
    );
    // A denied machine memory is not counted.
    let laptop = &snapshot.machines[0];
    assert_eq!(
        laptop
            .memories
            .iter()
            .map(|memory| memory.url.as_str())
            .collect::<Vec<_>>(),
        ["machine://laptop/a"]
    );
    assert_eq!(laptop.omitted, 0);
    // The overview the rules ask for joins the index; a denied memory is
    // left out uncounted.
    let project = snapshot.project.clone().unwrap();
    assert_eq!(project.overview, None);
    assert_eq!(
        project
            .memories
            .iter()
            .map(|memory| memory.url.as_str())
            .collect::<Vec<_>>(),
        ["project://web/overview", "project://web/x"]
    );
    assert_eq!(project.omitted, 0);

    let text = render::snapshot_turn(&snapshot).expect("memories to show");
    for secret in [
        "user://preferences",
        "secret preference",
        "secret lesson",
        "A secret shop",
        "Hidden B",
        "Hidden Y",
        "more; search",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(
        text.contains("- agent://reviewer: How reviews go"),
        "{text}"
    );

    // A user category the rules ask for is listed, not shown.
    let asked = ReadRules::new(|url: &MemoryUrl| {
        if url.to_string() == "user://preferences" {
            Visibility::Summary
        } else {
            Visibility::Full
        }
    });
    let snapshot = memory
        .snapshot_only(&SnapshotRequest {
            read: asked,
            ..request(false, &[], limits(12_288, 4096))
        })
        .unwrap();
    assert_eq!(
        snapshot
            .user_listed
            .iter()
            .map(|listed| listed.url.as_str())
            .collect::<Vec<_>>(),
        ["user://preferences"]
    );
    let text = render::snapshot_turn(&snapshot).unwrap();
    assert!(!text.contains("secret preference"), "{text}");
    assert!(text.contains("- user://preferences: Preferences"), "{text}");

    // A delegated run's agent file follows the same rules.
    let delegated = memory
        .snapshot_only(&SnapshotRequest {
            read,
            ..request(true, &[], limits(12_288, 4096))
        })
        .unwrap();
    assert!(delegated.agent.unwrap().listed.is_some());
}

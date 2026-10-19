use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use cookie_agent_protocol::{AgentId, AttachmentName, MachineName};

use super::{SnapshotLimits, SnapshotRequest};
use crate::memory::{MemoryRoot, render};

const MAX_FILE: usize = 4096;

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

fn request(
    delegated: bool,
    machines: &[(&str, &[&str])],
    limits: SnapshotLimits,
) -> SnapshotRequest {
    SnapshotRequest {
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
    }
}

fn limits(user_bytes: usize, index_bytes: usize) -> SnapshotLimits {
    SnapshotLimits {
        max_file_bytes: MAX_FILE,
        user_bytes,
        index_bytes,
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
        .snapshot(&request(
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
        .snapshot(&request(true, &[("labpc", &[])], limits(12_288, 4096)))
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
            .snapshot(&request(false, &[], limits(MAX_FILE, 4096)))
            .unwrap();
        let mut user = snapshot.user;
        user.truncate(fitting);
        render::user_section(
            &user,
            &omitted
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .len()
    };
    // Exactly the bytes `a` alone needs, with `b` and `c` named: `b` does
    // not fit, so `c`, which would, is omitted too.
    let only_a = sized(1, &["b", "c"]);
    let snapshot = memory
        .snapshot(&request(false, &[], limits(only_a, 4096)))
        .unwrap();
    assert_eq!(snapshot.user.len(), 1);
    assert_eq!(snapshot.user[0].category, "a");
    assert_eq!(snapshot.user_omitted, ["b", "c"]);
    assert_eq!(
        render::user_section(&snapshot.user, &snapshot.user_omitted).len(),
        only_a
    );
    // One byte less and nothing fits.
    let snapshot = memory
        .snapshot(&request(false, &[], limits(only_a - 1, 4096)))
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
        .snapshot(&request(false, machines, limits(12_288, 4096)))
        .unwrap()
        .machines;
    // Room for the first machine's memory and the newest of the second.
    let mut fitted = full.clone();
    fitted[1].memories.truncate(1);
    fitted[1].omitted = 1;
    let room = render::machines_section(&fitted).len();
    let snapshot = memory
        .snapshot(&request(false, machines, limits(12_288, room)))
        .unwrap();
    assert_eq!(snapshot.machines, fitted);
    assert_eq!(
        snapshot.machines[1].memories[0].url,
        "machine://labpc/three"
    );
    let snapshot = memory
        .snapshot(&request(false, machines, limits(12_288, room - 1)))
        .unwrap();
    assert_eq!(snapshot.machines[0].memories.len(), 1);
    assert!(snapshot.machines[1].memories.is_empty());
    assert_eq!(snapshot.machines[1].omitted, 2);
}

#[test]
fn an_agent_file_whose_escaped_text_exceeds_the_file_limit_is_omitted() {
    let (data, memory) = root();
    // 60 `<memory` tags: 420 bytes of body, 600 escaped.
    let body = "<memory".repeat(60);
    let text = file("Reviews", &body);
    put(data.path(), "agents/reviewer.md", &text, 0);
    let limits_at = |max_file_bytes| SnapshotLimits {
        max_file_bytes,
        user_bytes: 4096,
        index_bytes: 4096,
    };
    let fits = render::agent_content_bytes("Reviews", &body);
    assert!(text.len() < fits);
    let shown = memory
        .snapshot(&request(true, &[], limits_at(fits)))
        .unwrap()
        .agent
        .unwrap();
    assert!(shown.memory.is_some() && !shown.omitted);
    let omitted = memory
        .snapshot(&request(true, &[], limits_at(fits - 1)))
        .unwrap()
        .agent
        .unwrap();
    assert!(omitted.memory.is_none() && omitted.omitted);
}

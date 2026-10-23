use serde_json::json;

use jiff::Timestamp;

use super::{
    MemoryCall, MemoryTools, PART_B, check, near_output, permission_name, prompt_section,
    read_output, search_output,
};
use crate::{
    memory::{limits::MemoryLimits, search::SearchHit, similar::NearMemory, url::MemoryUrl},
    runtime::memory_runs::{MemoryAccess, MemoryCaller, RelatedMemory},
};

fn error(name: &str, arguments: serde_json::Value) -> String {
    match check(name, arguments) {
        Ok(_) => panic!("{name} accepted its arguments"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn each_tool_declares_its_action() {
    let specs = MemoryTools::specs();
    let declared = specs
        .iter()
        .map(|spec| (spec.name.as_str(), spec.permission_name.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(
        declared,
        [
            ("memory_search", None),
            ("memory_read", Some("memory")),
            ("memory_write", Some("memory_write")),
            ("memory_edit", Some("memory_write")),
            ("memory_delete", Some("memory_write")),
        ]
    );
    for (name, permission) in declared {
        assert_eq!(permission_name(name).unwrap(), permission);
    }
    assert!(permission_name("read").is_err());
}

#[test]
fn search_bounds_its_query_and_limit() {
    assert!(matches!(
        check("memory_search", json!({"query": "a"})),
        Ok(MemoryCall::Search {
            limit: 10,
            layer: None,
            ..
        })
    ));
    assert!(matches!(
        check(
            "memory_search",
            json!({"query": "x".repeat(200), "limit": 25, "layer": "project"})
        ),
        Ok(MemoryCall::Search {
            limit: 25,
            layer: Some(crate::memory::url::MemoryLayer::Project),
            ..
        })
    ));
    assert!(matches!(
        check(
            "memory_search",
            json!({"query": "é".repeat(200), "limit": 1})
        ),
        Ok(MemoryCall::Search { limit: 1, .. })
    ));
    assert!(error("memory_search", json!({"query": ""})).contains("1 to 200 characters"));
    assert!(error("memory_search", json!({"query": "   "})).contains("1 to 200 characters"));
    assert!(
        error("memory_search", json!({"query": "x".repeat(201)})).contains("1 to 200 characters")
    );
    assert!(error("memory_search", json!({"query": "a", "limit": 0})).contains("1 to 25"));
    assert!(error("memory_search", json!({"query": "a", "limit": 26})).contains("1 to 25"));
    assert!(error("memory_search", json!({"query": "a", "layer": "session"})).contains("invalid"));
}

#[test]
fn arguments_are_strict() {
    for (name, arguments) in [
        ("memory_search", json!({"query": "a", "extra": 1})),
        (
            "memory_read",
            json!({"url": "user://profile", "revision": "0"}),
        ),
        (
            "memory_write",
            json!({"url": "project://web/build", "description": "d", "keywords": [], "body": "", "extra": true}),
        ),
        (
            "memory_edit",
            json!({"url": "project://web/build", "old": "a", "new": "b", "revision": "0123456789abcdef"}),
        ),
        (
            "memory_delete",
            json!({"url": "project://web/build", "revision": "0123456789abcdef"}),
        ),
        (
            "memory_write",
            json!({"url": "project://web/build", "description": "d", "body": "", "revision": "0123456789abcdef"}),
        ),
        // `body` is required.
        (
            "memory_write",
            json!({"url": "project://web/build", "description": "d", "keywords": []}),
        ),
    ] {
        assert!(
            check(name, arguments.clone()).is_err(),
            "{name} {arguments}"
        );
    }
}

#[test]
fn urls_and_memories_are_checked_before_the_call_runs() {
    assert!(error("memory_read", json!({"url": "user://Profile"})).contains("user://Profile"));
    assert!(error("memory_read", json!({"url": "file:///etc"})).contains("not a memory URL"));
    assert!(
        error(
            "memory_write",
            json!({"url": "project://web/build", "description": "", "keywords": [], "body": ""})
        )
        .contains("would not be a valid memory")
    );
    // Every layer's edit and delete is accepted, with no revision.
    for url in [
        "user://profile",
        "agent://default",
        "machine://labpc/x",
        "project://web/build",
    ] {
        assert!(
            matches!(
                check("memory_delete", json!({"url": url})),
                Ok(MemoryCall::Delete { .. })
            ),
            "{url}"
        );
        assert!(
            matches!(
                check("memory_edit", json!({"url": url, "old": "a", "new": "b"})),
                Ok(MemoryCall::Edit { .. })
            ),
            "{url}"
        );
    }
}

/// `new: true` says a memory like ones that exist is new, and needs a
/// one-line `reason`; `reason` goes only with it.
#[test]
fn new_and_reason_go_together() {
    let write = |extra: serde_json::Value| {
        let mut arguments = json!({"url": "project://web/build", "description": "d", "body": ""});
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        check("memory_write", arguments)
    };
    assert!(matches!(
        write(json!({"new": true, "reason": "covers the CI side, not the release"})),
        Ok(MemoryCall::Write { new: true, .. })
    ));
    assert!(matches!(
        write(json!({})),
        Ok(MemoryCall::Write { new: false, .. })
    ));
    for (extra, expected) in [
        (json!({"new": true}), "needs a one-line `reason`"),
        (json!({"reason": "x"}), "goes with `new: true`"),
        (json!({"new": true, "reason": " "}), "one line"),
        (json!({"new": true, "reason": "a\nb"}), "one line"),
        (json!({"new": true, "reason": "r".repeat(201)}), "one line"),
    ] {
        let found = write(extra.clone()).err().expect("refused").to_string();
        assert!(found.contains(expected), "{extra}: {found}");
    }
}

fn caller(delegated: bool) -> MemoryCaller {
    let rule = |action, resource: &str, effect| {
        cookie_agent_protocol::PermissionRule::new(
            action,
            cookie_agent_protocol::WildcardPattern::new(resource).unwrap(),
            effect,
        )
    };
    let mut agent =
        crate::test_support::agent_snapshot("builder", cookie_agent_protocol::AgentMode::Primary);
    agent.permissions = vec![
        rule(
            cookie_agent_protocol::PermissionAction::MEMORY,
            "*",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        rule(
            cookie_agent_protocol::PermissionAction::MEMORY_WRITE,
            "project://*",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        rule(
            cookie_agent_protocol::PermissionAction::MEMORY_WRITE,
            "agent://*",
            cookie_agent_protocol::PermissionEffect::Ask,
        ),
    ];
    MemoryCaller {
        agent,
        overlay: cookie_agent_protocol::SessionPermissionOverlay::empty(),
        related: RelatedMemory {
            delegated,
            machines: vec![cookie_agent_protocol::MachineName::new("labpc").unwrap()],
            project: Some("cookie-agent".to_owned()),
        },
    }
}

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

#[test]
fn search_results_say_whether_a_memory_is_related_editable_and_how_old() {
    let now = at("2026-10-22T12:00:00Z");
    let hit = |url: &str, description: &str, days: i64, keywords: &[&str], full| SearchHit {
        url: MemoryUrl::parse(url).unwrap(),
        description: description.into(),
        keywords: keywords
            .iter()
            .map(|keyword| (*keyword).to_owned())
            .collect(),
        modified: now - jiff::SignedDuration::from_hours(24 * days),
        full,
    };
    let hits = [
        hit(
            "project://cookie-agent/release",
            "How releases are cut: gh release create starts it; publish oven-sdk first",
            21,
            &["release", "cargo-dist", "oven-sdk"],
            true,
        ),
        hit(
            "agent://builder",
            "Memory of the `builder` agent: Primary agent for building features",
            5,
            &["builder", "primary"],
            true,
        ),
        hit(
            "agent://reviewer",
            "Memory of the `reviewer` agent: Reviews a diff for bugs and missing tests",
            2,
            &["reviewer", "subagent"],
            true,
        ),
        hit("machine://box/notes", "Notes", 0, &[], true),
        hit(
            "user://preferences",
            "How the user likes to work",
            31,
            &[],
            false,
        ),
    ];
    assert_eq!(
        search_output(&hits, &caller(false), now),
        "project://cookie-agent/release — How releases are cut: gh release create starts it; publish oven-sdk first
  related · editable · 3 weeks old
  keywords: release, cargo-dist, oven-sdk

agent://builder — Memory of the `builder` agent: Primary agent for building features
  related · editable · 5 days old
  keywords: builder, primary

agent://reviewer — Memory of the `reviewer` agent: Reviews a diff for bugs and missing tests
  unrelated · editable · 2 days old
  keywords: reviewer, subagent

machine://box/notes — Notes
  unrelated · read-only · less than a minute old

user://preferences — How the user likes to work
  related · read-only · 1 month old
  (reading it needs approval)"
    );
    // For a delegated run only its own agent file is related.
    let delegated = search_output(&hits[..1], &caller(true), now);
    assert!(delegated.contains("unrelated · editable"), "{delegated}");
    assert_eq!(
        search_output(&[], &caller(false), now),
        "No memories match."
    );
}

#[test]
fn a_read_shows_stamps_and_size_but_no_url_or_revision() {
    let data = tempfile::tempdir().unwrap();
    let memory = crate::memory::MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    let created = at("2026-10-01T09:12:44Z");
    let modified = at("2026-10-20T16:03:10Z");
    let text = crate::memory::file::MemoryDocument::new(
        "How releases are cut",
        vec!["release".to_owned()],
        "- tag first\n",
    )
    .unwrap()
    .with_stamps(Some(created), Some(modified))
    .render();
    let path = data.path().join("memory/projects/cookie-agent/release.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &text).unwrap();
    let url = MemoryUrl::parse("project://cookie-agent/release").unwrap();
    let read = memory.read(&url).unwrap();
    let output = read_output(
        &url,
        &read,
        &caller(false),
        &MemoryLimits::all(2048),
        at("2026-10-22T12:00:00Z"),
    );
    assert_eq!(
        output,
        format!(
            "related: yes
editable: yes
created: 2026-10-01T09:12:44Z (3 weeks ago)
modified: 2026-10-20T16:03:10Z (1 day ago)
size: {} bytes
max size: 2048 bytes
description: How releases are cut
keywords: release

- tag first
",
            text.len()
        )
    );
    let small = read_output(
        &url,
        &read,
        &caller(true),
        &MemoryLimits::all(10),
        at("2026-10-22T12:00:00Z"),
    );
    assert!(small.starts_with("related: no\n"), "{small}");
    assert!(
        small.contains(&format!(
            "size: {} bytes, over its limit\nmax size: 10 bytes",
            text.len()
        )),
        "{small}"
    );
}

#[test]
fn a_saved_memory_lists_the_memories_near_it() {
    let near = [NearMemory {
        url: MemoryUrl::parse("project://web/release").unwrap(),
        description: "How releases are cut".into(),
    }];
    assert_eq!(
        near_output(&near),
        "\n\nMemories near it:\n- project://web/release — How releases are cut\nIf one of these already covers it, edit that and delete this."
    );
}

/// Part A for every run with memory, the delegated variant for a delegated
/// run, and part B only with the write tools (section 16.9.1).
#[test]
fn the_memory_section_has_part_b_only_with_the_write_tools() {
    let access = |available, writable| MemoryAccess {
        available,
        writable,
    };
    assert_eq!(prompt_section(access(false, false), false), None);
    let root = prompt_section(access(true, false), false).unwrap();
    assert!(
        root.starts_with("Memory related to this session is included automatically"),
        "{root}"
    );
    assert!(!root.contains(PART_B));
    let delegated = prompt_section(access(true, true), true).unwrap();
    assert_eq!(
        delegated,
        format!(
            "Your own agent memory is included automatically at the start of each run. Use memory_search for anything else.\n\n{PART_B}"
        )
    );
    assert!(PART_B.contains("\n1. Could a later session re-derive it"));
    assert!(PART_B.ends_with("before you add to it."));
}

/// `keywords` may be left out; `description` too for an agent's memory,
/// whose description the daemon sets, but not for any other.
#[test]
fn keywords_are_optional_and_an_agent_memory_needs_no_description() {
    assert!(matches!(
        check(
            "memory_write",
            json!({"url": "project://web/build", "description": "d", "body": ""})
        ),
        Ok(MemoryCall::Write {
            described: true,
            keyworded: false,
            ..
        })
    ));
    assert!(matches!(
        check(
            "memory_write",
            json!({"url": "agent://reviewer", "body": "- a lesson\n"})
        ),
        Ok(MemoryCall::Write {
            described: false,
            ..
        })
    ));
    for url in [
        "project://web/build",
        "user://profile",
        "machine://labpc/gpu",
    ] {
        assert!(
            error("memory_write", json!({"url": url, "body": ""})).contains("needs a description"),
            "{url}"
        );
    }
}

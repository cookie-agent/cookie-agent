use serde_json::json;

use jiff::Timestamp;

use cookie_agent_protocol::{DiffBlock, DiffChange, DiffLineOp, DisplayBlock, Open};

use super::{
    MemoryCall, MemoryTools, PART_B, check, edit_diff, memory_diff, near_output, permission_name,
    prompt_section, read_output, search_display, search_output, shown_text,
};
use cookie_agent_runtime_api::text_tools::{LineWindow, MAX_LINE_BYTES};

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
            json!({"url": "project://web/build", "oldString": "a", "newString": "b", "revision": "0123456789abcdef"}),
        ),
        // The edit's arguments are `oldString` and `newString`, as `edit`'s.
        (
            "memory_edit",
            json!({"url": "project://web/build", "old": "a", "new": "b"}),
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
    // A bad URL is `invalid_url`, as the tool error kinds name it.
    assert!(matches!(
        check("memory_read", json!({"url": "file:///etc"})),
        Err(cookie_agent_runtime_api::ToolError::InvalidUrl(_))
    ));
    assert!(matches!(
        check(
            "memory_edit",
            json!({"url": "user://feedback", "oldString": "a", "newString": "b"})
        ),
        Ok(MemoryCall::Edit { ref old, ref new, .. }) if old == "a" && new == "b"
    ));
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
                check(
                    "memory_edit",
                    json!({"url": url, "oldString": "a", "newString": "b"})
                ),
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
        // Writes the rules above leave would ask; the rest is read-only.
        rule(
            cookie_agent_protocol::PermissionAction::MEMORY_WRITE,
            "*",
            cookie_agent_protocol::PermissionEffect::Deny,
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
    let (output, page) = read_output(
        &url,
        &read,
        &caller(false),
        &MemoryLimits::all(2048),
        at("2026-10-22T12:00:00Z"),
        window(1, 2000),
    )
    .unwrap();
    assert_eq!((page.start_line, page.total_lines), (1, 1));
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

1: - tag first
",
            text.len()
        )
    );
    let (small, _) = read_output(
        &url,
        &read,
        &caller(true),
        &MemoryLimits::all(10),
        at("2026-10-22T12:00:00Z"),
        window(1, 2000),
    )
    .unwrap();
    assert!(small.starts_with("related: no\n"), "{small}");
    assert!(
        small.contains(&format!(
            "size: {} bytes, over its limit\nmax size: 10 bytes",
            text.len()
        )),
        "{small}"
    );
}

fn window(offset: usize, limit: usize) -> LineWindow {
    LineWindow::new(Some(offset), Some(limit)).unwrap()
}

/// `memory_read` pages a memory's body as `read` pages a file: numbered
/// from 1 under the header, from a 1-based `offset`, with the same line
/// cut and continuation marker.
#[test]
fn a_read_pages_the_body_from_a_one_based_offset() {
    let data = tempfile::tempdir().unwrap();
    let memory = crate::memory::MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    let long = "x".repeat(MAX_LINE_BYTES + 5);
    let text = crate::memory::file::MemoryDocument::new(
        "Notes",
        Vec::new(),
        format!("one\ntwo\nthree\n{long}\n"),
    )
    .unwrap()
    .render();
    let path = data.path().join("memory/user/notes.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &text).unwrap();
    let url = MemoryUrl::parse("user://notes").unwrap();
    let read = memory.read(&url).unwrap();
    let page = |offset, limit| {
        read_output(
            &url,
            &read,
            &caller(false),
            &MemoryLimits::all(1 << 20),
            at("2026-10-22T12:00:00Z"),
            window(offset, limit),
        )
    };
    let body = |output: &str| output.split_once("\n\n").unwrap().1.to_owned();
    // The first page, and where to continue.
    let (first, numbered) = page(1, 2).unwrap();
    assert!(first.contains("\ndescription: Notes\n\n"), "{first}");
    assert_eq!(
        body(&first),
        "1: one\n2: two\n[2 more lines; continue with offset=3]"
    );
    assert_eq!(numbered.next_offset, Some(3));
    // The last page cuts its long line as `read` does, and ends there.
    let (last, numbered) = page(3, 2).unwrap();
    assert_eq!(
        body(&last),
        format!(
            "3: three\n4: {}... [line truncated: 5 more bytes]\n",
            "x".repeat(MAX_LINE_BYTES)
        )
    );
    assert_eq!(numbered.next_offset, None);
    assert_eq!(numbered.excerpt[0], "three");
    assert_eq!(
        page(5, 2).unwrap_err().message(),
        "offset=5 is past the end of user://notes, which has 4 lines"
    );
    assert_eq!(
        error("memory_read", json!({"url": "user://notes", "offset": 0})),
        "tool failed: offset is 1-based: the first line is offset=1"
    );
    assert!(matches!(
        check(
            "memory_read",
            json!({"url": "user://notes", "offset": 3, "limit": 2})
        ),
        Ok(MemoryCall::Read {
            window: LineWindow {
                offset: 3,
                limit: 2
            },
            ..
        })
    ));
}

#[test]
fn the_write_tools_word_their_errors_as_the_file_tools_do() {
    let url = MemoryUrl::parse("user://feedback").unwrap();
    let unseen = super::store_error(crate::memory::store::StoreError::Unseen { url: url.clone() });
    assert!(matches!(
        unseen,
        cookie_agent_runtime_api::ToolError::OperationChanged(_)
    ));
    let several = super::store_error(crate::memory::store::StoreError::Occurrences {
        url: url.clone(),
        old: "y".repeat(300),
        count: 2,
    });
    assert_eq!(
        several.message(),
        format!(
            "`{}…` occurs 2 times in the body of user://feedback; include more context so it occurs once",
            "y".repeat(200)
        )
    );
    let missing = super::store_error(crate::memory::store::StoreError::NotFound { url });
    assert_eq!(missing.message(), "user://feedback does not exist");
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

/// `keywords` is a list, or one comma-separated string, as agents often
/// send a list; a string holding a list's JSON is that list.
#[test]
fn keywords_may_be_one_comma_separated_string() {
    let keywords = |value: serde_json::Value| match check(
        "memory_write",
        json!({"url": "project://web/build", "description": "d", "keywords": value, "body": ""}),
    ) {
        Ok(MemoryCall::Write { document, .. }) => document.keywords().to_vec(),
        Ok(_) => panic!("{value}: not a write"),
        Err(error) => panic!("{value}: {error}"),
    };
    let expected = ["cargo", "release notes", "ci"];
    assert_eq!(keywords(json!(["cargo", "release notes", "ci"])), expected);
    assert_eq!(keywords(json!("cargo, release notes,ci")), expected);
    assert_eq!(keywords(json!(" cargo ,, release notes , ci, ")), expected);
    assert_eq!(
        keywords(json!("[\"cargo\", \"release notes\", \"ci\"]")),
        expected
    );
    assert_eq!(keywords(json!("cargo")), ["cargo"]);
    assert!(keywords(json!("")).is_empty());
    assert!(keywords(json!(" , ")).is_empty());
    // A bracket that is no JSON list is a keyword like any other text.
    assert_eq!(keywords(json!("[draft], cargo")), ["[draft]", "cargo"]);
    // Neither a list nor a string.
    assert!(
        check(
            "memory_write",
            json!({"url": "project://web/build", "description": "d", "keywords": 3, "body": ""})
        )
        .is_err()
    );
    // The keyword bounds hold either way.
    let long = "k".repeat(65);
    assert!(
        check(
            "memory_write",
            json!({"url": "project://web/build", "description": "d", "keywords": format!("cargo, {long}"), "body": ""})
        )
        .is_err()
    );
}

fn only_diff(blocks: &[Open<DisplayBlock>]) -> &DiffBlock {
    match blocks {
        [Open::Known(DisplayBlock::Diff(diff))] => diff,
        other => panic!("expected one diff, got {other:?}"),
    }
}

fn tool_context(root: &std::path::Path) -> cookie_agent_runtime_api::ToolExecutionContext {
    crate::testing::tool_execution_context(
        root.join("artifacts"),
        std::sync::Arc::new(cookie_agent_runtime_api::TurnAgentContext {
            agent: cookie_agent_protocol::AgentId::new("test").unwrap(),
            model: "test/model".parse().unwrap(),
            adapter: cookie_agent_protocol::AdaptorId::OPENAI_CHAT,
            input_media: Vec::new(),
        }),
    )
    .unwrap()
}

/// A memory file of `body`, stamped as a save at `modified` stamps it.
fn stamped(body: &str, modified: &str) -> String {
    crate::memory::file::MemoryDocument::new("Build conventions", vec!["cargo".to_owned()], body)
        .unwrap()
        .with_stamps(Some(at("2026-10-01T09:12:44Z")), Some(at(modified)))
        .render()
}

fn numbered(count: usize) -> String {
    (1..=count).map(|line| format!("line {line}\n")).collect()
}

#[test]
fn a_diff_leaves_out_the_stamps_the_daemon_rewrites() {
    let text = stamped("- run cargo test\n", "2026-10-20T16:03:10Z");
    assert!(text.contains("modified: "), "{text}");
    assert_eq!(
        shown_text(&text),
        "---\ndescription: Build conventions\nkeywords: [cargo]\n---\n- run cargo test\n"
    );
    // A text without frontmatter is shown whole.
    assert_eq!(shown_text("created: x\n"), "created: x\n");
}

/// A memory edit shows the diff the coding runtime's `edit` tool shows for
/// the same text and replacement: the same hunks, with the same unchanged
/// lines of context around the change.
#[tokio::test]
async fn a_memory_edit_diff_is_the_edit_tools_diff() {
    use cookie_agent_runtime_api::ToolProvider as _;

    let root = tempfile::tempdir().unwrap();
    let url = MemoryUrl::parse("project://web/build").unwrap();
    let edited = crate::memory::store::Edited {
        revision: crate::memory::file::MemoryRevision::of(b"after"),
        before: stamped(&numbered(20), "2026-10-20T16:03:10Z"),
        after: stamped(
            &numbered(20).replacen("line 10\n", "ten\n", 1),
            "2026-10-22T12:00:00Z",
        ),
    };
    let context = tool_context(root.path());
    let memory = edit_diff(&url, &edited, "line 10\n", "ten\n", &context).unwrap();
    let memory = only_diff(&memory);

    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("build.md"), shown_text(&edited.before)).unwrap();
    let workspace = cookie_agent_runtime_api::WorkspaceContext::new(&workspace).unwrap();
    let tool = cookie_agent_coding_runtime::edit::EditTool::new(&workspace);
    let prepared = tool
        .prepare(
            cookie_agent_runtime_api::ToolPreparationContext {
                session: cookie_agent_protocol::SessionId::new_v7(),
                run: cookie_agent_protocol::RunId::new_v7(),
                origin: cookie_agent_runtime_api::ToolOrigin::Runtime(std::sync::Arc::new(
                    cookie_agent_runtime_api::RuntimeInfo::coding(
                        cookie_agent_runtime_api::LOCAL_RUNTIME_ID,
                        workspace.clone(),
                    ),
                )),
                turn_context: context.turn_context.clone(),
            },
            cookie_agent_runtime_api::ToolCall {
                id: cookie_agent_protocol::ToolCallId::new_v7(),
                name: "edit".into(),
                arguments: json!({"filePath": "build.md", "oldString": "line 10\n", "newString": "ten\n"}),
            },
        )
        .await
        .unwrap();
    let result = prepared
        .execute_for_test(tool_context(root.path()))
        .await
        .unwrap();
    let edit = only_diff(&result.display);

    assert_eq!(memory.path, "project://web/build");
    assert_eq!(memory.language.as_deref(), Some("markdown"));
    assert_eq!(memory.language, edit.language);
    assert_eq!(memory.change, DiffChange::Modified);
    assert_eq!(memory.change, edit.change);
    assert_eq!(memory.as_of, edit.as_of);
    assert_eq!((memory.added, memory.removed), (1, 1));
    assert_eq!((memory.added, memory.removed), (edit.added, edit.removed));
    assert_eq!(memory.hunks, edit.hunks);
    // Five unchanged lines on each side; the stamps are not part of it.
    let hunk = &memory.hunks[0];
    assert_eq!(
        (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines
        ),
        (9, 11, 9, 11)
    );
    let context_lines = hunk
        .lines
        .iter()
        .filter(|line| line.op == DiffLineOp::Context)
        .count();
    assert_eq!(context_lines, 10);
}

/// An edit that changed more than the replacement, such as an agent file
/// whose frontmatter the daemon rewrote, shows the whole change.
#[test]
fn an_edit_that_changed_the_frontmatter_shows_it_too() {
    let root = tempfile::tempdir().unwrap();
    let url = MemoryUrl::parse("agent://reviewer").unwrap();
    let edited = crate::memory::store::Edited {
        revision: crate::memory::file::MemoryRevision::of(b"after"),
        before: "---\ndescription: Stale\n---\n- a lesson\n".to_owned(),
        after: "---\ndescription: Reviews code\n---\n- two lessons\n".to_owned(),
    };
    let display = edit_diff(
        &url,
        &edited,
        "a lesson",
        "two lessons",
        &tool_context(root.path()),
    )
    .unwrap();
    let diff = only_diff(&display);
    assert_eq!(diff.change, DiffChange::Modified);
    let changed = diff.hunks[0]
        .lines
        .iter()
        .filter(|line| line.op != DiffLineOp::Context)
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>();
    assert!(changed.contains(&"description: Stale"), "{changed:?}");
    assert!(
        changed.contains(&"description: Reviews code"),
        "{changed:?}"
    );
    assert!(changed.contains(&"- two lessons"), "{changed:?}");
    assert!(changed.contains(&"- a lesson"), "{changed:?}");
}

/// A new memory is a created diff, a replaced one a modified diff against
/// what it held, and a deleted one a deleted diff of what it held.
#[test]
fn writes_and_deletes_show_created_modified_and_deleted_diffs() {
    let root = tempfile::tempdir().unwrap();
    let context = tool_context(root.path());
    let url = MemoryUrl::parse("user://preferences").unwrap();
    let first = shown_text(&stamped("- tabs\n", "2026-10-20T16:03:10Z"));
    let second = shown_text(&stamped("- spaces\n", "2026-10-22T12:00:00Z"));
    let summary = |display: &[Open<DisplayBlock>]| {
        let diff = only_diff(display);
        (diff.path.clone(), diff.change, diff.added, diff.removed)
    };
    let created = memory_diff(
        &url,
        cookie_agent_runtime_api::display::TextChange::Created { text: &first },
        &context,
    )
    .unwrap();
    assert_eq!(
        summary(&created),
        ("user://preferences".to_owned(), DiffChange::Created, 5, 0)
    );
    let replaced = memory_diff(
        &url,
        cookie_agent_runtime_api::display::TextChange::Modified {
            old: &first,
            new: &second,
        },
        &context,
    )
    .unwrap();
    assert_eq!(
        summary(&replaced),
        ("user://preferences".to_owned(), DiffChange::Modified, 1, 1)
    );
    let deleted = memory_diff(
        &url,
        cookie_agent_runtime_api::display::TextChange::Deleted { text: &second },
        &context,
    )
    .unwrap();
    assert_eq!(
        summary(&deleted),
        ("user://preferences".to_owned(), DiffChange::Deleted, 0, 5)
    );
    let removed = only_diff(&deleted).hunks[0]
        .lines
        .iter()
        .map(|line| (line.op, line.text.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(removed.last(), Some(&(DiffLineOp::Remove, "- spaces")));
}

/// Search results show as one link per memory, labelled with its
/// description, which a client opens in its Memory view.
#[test]
fn search_results_show_as_links_to_the_memories() {
    let hit = |url: &str, description: &str| SearchHit {
        url: MemoryUrl::parse(url).unwrap(),
        description: description.to_owned(),
        keywords: Vec::new(),
        modified: at("2026-10-20T16:03:10Z"),
        full: true,
    };
    assert_eq!(
        search_display(&[
            hit("project://web/build", "Build conventions"),
            hit("user://preferences", "Preferences"),
        ]),
        [
            Open::Known(DisplayBlock::Link {
                url: "project://web/build".into(),
                label: Some("Build conventions".into()),
            }),
            Open::Known(DisplayBlock::Link {
                url: "user://preferences".into(),
                label: Some("Preferences".into()),
            }),
        ]
    );
    assert_eq!(
        search_display(&[]),
        cookie_agent_protocol::text_display("No memories match.")
    );
}

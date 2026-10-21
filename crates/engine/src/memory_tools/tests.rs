use cookie_agent_protocol::AgentId;
use serde_json::json;

use super::{
    MAX_HIT_LINE_CHARS, MemoryCall, MemoryTools, check, check_delegated_write, hit_line,
    permission_name, search_output,
};
use crate::memory::{
    search::{HitDetail, SearchHit},
    url::MemoryUrl,
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
            json!({"url": "project://web/build", "old": "a", "new": "b"}),
        ),
        ("memory_delete", json!({"url": "project://web/build"})),
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
fn urls_revisions_and_memories_are_checked_before_the_call_runs() {
    assert!(error("memory_read", json!({"url": "user://Profile"})).contains("user://Profile"));
    assert!(error("memory_read", json!({"url": "agent://self"})).contains("agent://self"));
    assert!(error("memory_read", json!({"url": "file:///etc"})).contains("not a memory URL"));
    assert!(
        error(
            "memory_delete",
            json!({"url": "project://web/build", "revision": "nope"})
        )
        .contains("not a memory revision")
    );
    assert!(
        error(
            "memory_write",
            json!({"url": "project://web/build", "description": "", "keywords": [], "body": ""})
        )
        .contains("would not be a valid memory")
    );
    // Every layer's delete is accepted; a user category's or an agent
    // file's clears its body when it runs.
    for url in [
        "user://profile",
        "agent://default",
        "machine://labpc/x",
        "project://web/build",
    ] {
        assert!(
            matches!(
                check(
                    "memory_delete",
                    json!({"url": url, "revision": "0123456789abcdef"})
                ),
                Ok(MemoryCall::Delete { .. })
            ),
            "{url}"
        );
    }
}

#[test]
fn a_delegated_run_writes_only_its_own_agent_file() {
    let agent = AgentId::new("reviewer").unwrap();
    let url = |value: &str| MemoryUrl::parse(value).unwrap();
    assert!(check_delegated_write(&url("agent://reviewer"), &agent).is_ok());
    for other in [
        "agent://default",
        "user://profile",
        "machine://labpc/x",
        "project://web/build",
    ] {
        let refusal = check_delegated_write(&url(other), &agent)
            .expect_err("a delegated write elsewhere")
            .to_string();
        assert!(
            refusal.contains("only its own agent's memory, agent://reviewer"),
            "{refusal}"
        );
    }
}

#[test]
fn search_results_show_what_their_rule_allows() {
    let hits = [
        SearchHit {
            url: MemoryUrl::parse("project://web/build").unwrap(),
            description: "Build conventions".into(),
            detail: Some(HitDetail {
                keywords: vec!["cargo".into(), "msrv".into()],
                lines: vec!["- run cargo test".into()],
            }),
        },
        SearchHit {
            url: MemoryUrl::parse("user://profile").unwrap(),
            description: "Who the user is".into(),
            detail: None,
        },
    ];
    assert_eq!(
        search_output(&hits),
        "project://web/build: Build conventions [keywords: cargo, msrv]\n  | - run cargo test\n\nuser://profile: Who the user is\n  (reading it needs approval)"
    );
    assert_eq!(search_output(&[]), "No memories match.");
}

#[test]
fn long_matching_lines_are_cut() {
    let exact = "x".repeat(MAX_HIT_LINE_CHARS);
    assert_eq!(hit_line(&exact), exact);
    let long = "é".repeat(MAX_HIT_LINE_CHARS + 1);
    assert_eq!(
        hit_line(&long),
        format!("{}…", "é".repeat(MAX_HIT_LINE_CHARS))
    );
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
            json!({"url": "agent://reviewer", "body": "- a lesson\n", "revision": "0123456789abcdef"})
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

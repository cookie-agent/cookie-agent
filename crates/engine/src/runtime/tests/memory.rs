//! Memory as runs meet it (`docs/site/specs/memory.md` sections 6 to 9):
//! the snapshot each run admission records, its place in history, and the
//! memory tools.

use std::{collections::HashMap, time::Duration};

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, MemorySnapshot, PermissionAction, PermissionEffect,
    PermissionRule, RunSelection, RunStartParams, SessionId, SessionPermissionOverlay,
    ToolCallTermination, ToolTerminationOutcome, WildcardPattern,
};

use super::{producers::request_tool_names, support::*};
use crate::memory::{file::MemoryDocument, limits::MemoryLimits, store::WriteMode, url::MemoryUrl};

const LIMIT: usize = 4096;

fn origin() -> EventOrigin {
    EventOrigin::new("client:memory-test").unwrap()
}

/// The rules that give a test agent memory, which has no default
/// (memory spec section 9.2, note of 2026-10-22).
const MEMORY: &str = "{ memory: allow, memory_write: allow }";

fn agent(permissions: &str) -> String {
    format!(
        "---\ndescription: Memory test agent\nmode: primary\nenabled: true\nmodels: [{{ model: \"custom.test/group/model\", variant: base }}]\npermissions: {permissions}\n---\nRemember things.\n"
    )
}

async fn run(
    fixture: &Fixture,
    session: SessionId,
    selection: &RunSelection,
    id: &str,
    input: &str,
) {
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(id).unwrap(),
                selection: selection.clone(),
                content: input.into(),
            },
            origin(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session).await;
}

fn events(fixture: &Fixture, session: SessionId) -> Vec<cookie_agent_protocol::StoredEvent> {
    fixture
        .engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
}

fn snapshots(fixture: &Fixture, session: SessionId) -> Vec<MemorySnapshot> {
    events(fixture, session)
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MemoryLoaded { snapshot } => Some(snapshot.clone()),
            _ => None,
        })
        .collect()
}

fn terminations(fixture: &Fixture, session: SessionId) -> HashMap<String, ToolCallTermination> {
    events(fixture, session)
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => Some((
                termination.owner.model_call_id.to_string(),
                termination.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// The text of each message of a request, in order.
fn message_texts(request: &str) -> Vec<String> {
    request_body(request)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].to_string())
        .collect()
}

/// `text` as it appears inside a JSON string.
fn json_text(text: &str) -> String {
    let quoted = serde_json::Value::String(text.to_owned()).to_string();
    quoted[1..quoted.len() - 1].to_owned()
}

fn memory_tool_names(request: &str) -> Vec<String> {
    request_tool_names(request)
        .into_iter()
        .filter(|name| name.starts_with("memory_"))
        .collect()
}

/// Creates the project `name`, as a client would.
pub(super) async fn create_project(
    fixture: &Fixture,
    name: &str,
) -> cookie_agent_protocol::ProjectInfo {
    fixture
        .engine
        .create_project(cookie_agent_protocol::ProjectCreateParams {
            name: cookie_agent_protocol::ProjectName::new(name).unwrap(),
            emoji: None,
            defaults: None,
        })
        .await
        .unwrap()
}

fn url(value: &str) -> MemoryUrl {
    MemoryUrl::parse(value).unwrap()
}

/// Replaces a memory's text as the user would, from a client.
fn user_write(fixture: &Fixture, target: &str, description: &str, body: &str) {
    let root = fixture.engine.memory_root();
    let current = root.read(&url(target)).ok().map(|read| read.entry.revision);
    root.write(
        &url(target),
        &MemoryDocument::new(description, vec!["zebra".into()], body)
            .unwrap()
            .render(),
        WriteMode::Client(current.as_ref()),
        |_: &cookie_agent_protocol::AgentId| crate::memory::agents::AgentMemory::Registered,
        &MemoryLimits::all(LIMIT),
        jiff::Timestamp::now(),
    )
    .map(|written| written.revision)
    .unwrap();
}

/// The memory changes each transcript line of `session` names, in order.
fn memory_changes(fixture: &Fixture, session: SessionId) -> Vec<String> {
    events(fixture, session)
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MemoryChanged { change, url } => Some(format!("{} {url}", change.verb())),
            _ => None,
        })
        .collect()
}

fn rule(action: PermissionAction, effect: PermissionEffect) -> PermissionRule {
    PermissionRule::new(action, WildcardPattern::new("*").unwrap(), effect)
}

async fn set_overlay(fixture: &Fixture, session: SessionId, rules: Vec<PermissionRule>) {
    fixture
        .engine
        .append(
            session,
            None,
            origin(),
            EventPayload::SessionPermissionOverlaySet {
                overlay: SessionPermissionOverlay { rules },
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_root_run_sees_its_snapshot_after_agents_md_until_something_changes() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent(MEMORY));
    write_private_test_file(
        &fixture._directory.path().join("AGENTS.md"),
        "workspace rules",
    );
    user_write(
        &fixture,
        "user://feedback",
        "Feedback",
        "- prefer short answers\n",
    );
    for input in ["first", "second", "third"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "first", "first").await;
    run(&fixture, session, &selection, "second", "second").await;
    // The user's edits from a client never refresh an open session.
    user_write(
        &fixture,
        "user://feedback",
        "Feedback",
        "- prefer long answers\n",
    );
    run(&fixture, session, &selection, "third", "third").await;
    let requests = captured.await.unwrap();

    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 1, "{taken:?}");
    let snapshot = &taken[0];
    assert!(!snapshot.delegated && snapshot.writable);
    assert_eq!(snapshot.agent.as_ref().unwrap().agent.as_str(), "primary");
    let feedback = snapshot
        .user
        .iter()
        .find(|category| category.category == "feedback")
        .unwrap();
    assert_eq!(feedback.body, "- prefer short answers\n");
    // A session with one coding attachment has its machine.
    let machines = fixture.engine.session_machines(session);
    assert_eq!(machines.len(), 1);
    assert_eq!(snapshot.machines.len(), 1);
    assert_eq!(snapshot.machines[0].machine, machines[0].0);
    assert_eq!(snapshot.machines[0].attachments, machines[0].1);

    let expected_turn = crate::memory::render::snapshot_turn(snapshot).unwrap();
    for request in &requests {
        let texts = message_texts(request);
        assert!(texts[1].contains("# AGENTS.md"), "{texts:?}");
        assert!(texts[2].contains(&json_text(&expected_turn)), "{texts:?}");
        assert!(texts[2].contains("prefer short answers"));
        assert_eq!(
            memory_tool_names(request),
            [
                "memory_delete",
                "memory_edit",
                "memory_read",
                "memory_search",
                "memory_write"
            ]
        );
    }
    assert!(message_texts(&requests[0])[3].contains("first"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn the_snapshot_follows_memory_access_and_write_access() {
    let (endpoint, responses, captured) = scripted_channel_server(4).await;
    // Write rules without a memory rule offer nothing.
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        &agent("{ memory: deny, memory_write: allow }"),
    );
    for input in ["one", "two", "three", "four"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "one", "one").await;
    assert!(snapshots(&fixture, session).is_empty());

    // The overlay gives memory; the agent's own write rule makes it
    // writable.
    set_overlay(
        &fixture,
        session,
        vec![rule(PermissionAction::MEMORY, PermissionEffect::Allow)],
    )
    .await;
    run(&fixture, session, &selection, "two", "two").await;
    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 1);
    assert!(taken[0].writable);

    // Asking before reading: the same access, but the rules changed, so a
    // new snapshot lists the categories instead of showing them
    // (section 7.5).
    set_overlay(
        &fixture,
        session,
        vec![rule(PermissionAction::MEMORY, PermissionEffect::Ask)],
    )
    .await;
    run(&fixture, session, &selection, "three", "three").await;
    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 2, "an overlay change takes a new snapshot");
    assert!(taken[1].writable);
    assert!(taken[1].user.is_empty());
    assert!(!taken[1].user_listed.is_empty());

    // Losing the rule appends an empty snapshot.
    set_overlay(&fixture, session, Vec::new()).await;
    run(&fixture, session, &selection, "four", "four").await;
    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 3);
    assert!(taken[2].is_empty());

    let requests = captured.await.unwrap();
    let has_memory_turn = |request: &String| {
        message_texts(request)
            .iter()
            .any(|text| text.contains("# Memory"))
    };
    assert!(memory_tool_names(&requests[0]).is_empty());
    assert!(!has_memory_turn(&requests[0]));
    for request in &requests[1..3] {
        assert_eq!(
            memory_tool_names(request),
            [
                "memory_delete",
                "memory_edit",
                "memory_read",
                "memory_search",
                "memory_write"
            ]
        );
        assert!(has_memory_turn(request));
    }
    assert!(memory_tool_names(&requests[3]).is_empty());
    assert!(!has_memory_turn(&requests[3]));
    fixture.engine.shutdown().await;
}

/// Gaining write access takes a new snapshot and brings part B, the
/// rubric, into the memory section (section 16.9.1).
#[tokio::test]
async fn gaining_write_access_takes_a_snapshot_and_adds_the_rubric() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        &agent("{ memory: allow, memory_write: { \"project://*\": deny } }"),
    );
    for input in ["one", "two"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "one", "one").await;
    set_overlay(
        &fixture,
        session,
        vec![rule(PermissionAction::MEMORY_WRITE, PermissionEffect::Ask)],
    )
    .await;
    run(&fixture, session, &selection, "two", "two").await;
    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 2);
    assert!(!taken[0].writable && taken[1].writable);
    let requests = captured.await.unwrap();
    assert_eq!(
        memory_tool_names(&requests[0]),
        ["memory_read", "memory_search"]
    );
    for request in &requests {
        assert!(request.contains("tool_instructions provider=\\\"builtin.memory\\\""));
        assert!(request.contains("Memory related to this session is included automatically"));
    }
    assert!(!requests[0].contains("You manage your own memory."));
    assert!(requests[1].contains("You manage your own memory."));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn detaching_global_turns_memory_off_and_attaching_it_back_on() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent(MEMORY));
    for input in ["one", "two", "three"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "one", "one").await;
    let global = fixture
        .engine
        .session_runtimes(session)
        .unwrap()
        .into_iter()
        .find(|attachment| attachment.runtime_id.as_str() == "global")
        .expect("global attachment");
    fixture
        .engine
        .detach_runtime(session, global.name.clone(), origin())
        .await
        .unwrap();
    run(&fixture, session, &selection, "two", "two").await;
    fixture
        .engine
        .attach_runtime(session, "global", None, None, origin())
        .await
        .unwrap();
    run(&fixture, session, &selection, "three", "three").await;
    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 3);
    assert!(!taken[0].is_empty() && taken[1].is_empty() && !taken[2].is_empty());
    let requests = captured.await.unwrap();
    assert!(!requests[1].contains("# Memory"));
    assert!(memory_tool_names(&requests[1]).is_empty());
    assert!(requests[2].contains("# Memory"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_new_agent_takes_a_new_snapshot() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let worker = "---\ndescription: Second root agent\nmode: all\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: allow, memory_write: allow }\n---\nWork.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            &agent(MEMORY),
            None,
            None,
            false,
            None,
            None,
            4_096,
            Some(worker),
        );
    for input in ["one", "two"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "one", "one").await;
    let other = RunSelection {
        agent: cookie_agent_protocol::AgentId::new("worker").unwrap(),
        ..selection.clone()
    };
    run(&fixture, session, &other, "two", "two").await;
    let taken = snapshots(&fixture, session);
    let agents = taken
        .iter()
        .map(|snapshot| snapshot.agent.as_ref().unwrap().agent.to_string())
        .collect::<Vec<_>>();
    assert_eq!(agents, ["primary", "worker"]);
    let requests = captured.await.unwrap();
    // Both agents' files are empty, so neither run's prompt has an agent
    // section.
    assert!(!requests[1].contains("## About this agent"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn the_memory_tools_write_read_edit_search_and_delete() {
    let (endpoint, responses, captured) = scripted_channel_server(10).await;
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent(MEMORY));
    create_project(&fixture, "web").await;
    user_write(&fixture, "user://feedback", "Feedback", "- unseen\n");
    let calls = [
        (
            "start-memory-tools",
            "write-1",
            "memory_write",
            serde_json::json!({"url": "project://web/build", "description": "Build conventions", "keywords": ["cargo"], "body": "- run cargo test\n"}),
        ),
        (
            "write-1",
            "read-1",
            "memory_read",
            serde_json::json!({"url": "project://web/build"}),
        ),
        (
            "read-1",
            "edit-1",
            "memory_edit",
            serde_json::json!({"url": "project://web/build", "old": "cargo test", "new": "cargo nextest"}),
        ),
        (
            "edit-1",
            "search-1",
            "memory_search",
            serde_json::json!({"query": "build"}),
        ),
        (
            "search-1",
            "replace-1",
            "memory_write",
            serde_json::json!({"url": "project://web/build", "description": "Build conventions", "keywords": ["cargo"], "body": "- run cargo nextest --no-fail-fast\n"}),
        ),
        (
            "replace-1",
            "delete-1",
            "memory_delete",
            serde_json::json!({"url": "project://web/build"}),
        ),
        (
            "delete-1",
            "create-user",
            "memory_write",
            serde_json::json!({"url": "user://hobbies", "description": "Hobbies", "keywords": [], "body": ""}),
        ),
        (
            "create-user",
            "delete-user",
            "memory_delete",
            serde_json::json!({"url": "user://profile"}),
        ),
        (
            "delete-user",
            "create-ghost",
            "memory_write",
            serde_json::json!({"url": "agent://ghost", "description": "Nobody", "keywords": [], "body": ""}),
        ),
        (
            "create-ghost",
            "unseen",
            "memory_write",
            serde_json::json!({"url": "user://feedback", "description": "Feedback", "body": "- replaced\n"}),
        ),
    ];
    for (matcher, id, name, arguments) in calls {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                matcher,
                scripted_tool_body(id, name, arguments),
            ))
            .unwrap();
    }
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "\"unseen\"",
            scripted_text_body("done"),
        ))
        .unwrap();
    // A rule that hides the user's memories from the snapshot, so the
    // session has not seen `user://feedback`.
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    set_overlay(
        &fixture,
        session,
        vec![PermissionRule::new(
            PermissionAction::MEMORY,
            WildcardPattern::new("user://feedback").unwrap(),
            PermissionEffect::Ask,
        )],
    )
    .await;
    run(&fixture, session, &selection, "go", "start-memory-tools").await;
    let requests = captured.await.unwrap();
    let ended = terminations(&fixture, session);
    let output = |id: &str| {
        let termination = &ended[id];
        assert_eq!(
            termination.outcome,
            ToolTerminationOutcome::Completed,
            "{termination:?}"
        );
        termination.result.as_ref().unwrap().output.clone()
    };
    let failure = |id: &str| {
        let termination = &ended[id];
        assert_eq!(
            termination.outcome,
            ToolTerminationOutcome::Failed,
            "{termination:?}"
        );
        termination.error.as_ref().unwrap().message.to_string()
    };
    assert_eq!(output("write-1"), "Created project://web/build.");
    let read = output("read-1");
    // No URL and no revision; the session is in no project, so the memory
    // is unrelated.
    assert!(
        read.starts_with("related: no\neditable: yes\ncreated: "),
        "{read}"
    );
    assert!(read.contains("\nmax size: 2048 bytes\ndescription: Build conventions\nkeywords: cargo\n\n- run cargo test\n"), "{read}");
    assert!(!read.contains("revision"), "{read}");
    assert_eq!(output("edit-1"), "Edited project://web/build.");
    assert_eq!(
        output("search-1"),
        "project://web/build — Build conventions\n  unrelated · editable · less than a minute old\n  keywords: cargo"
    );
    // A write of its own counts as seeing the memory.
    assert_eq!(output("replace-1"), "Replaced project://web/build.");
    assert_eq!(output("delete-1"), "Deleted project://web/build.");
    assert!(
        fixture
            .engine
            .memory_root()
            .read(&url("project://web/build"))
            .is_err()
    );
    // The rules alone decide: a model adds and deletes user categories.
    assert_eq!(output("create-user"), "Created user://hobbies.");
    assert_eq!(output("delete-user"), "Deleted user://profile.");
    assert!(
        fixture
            .engine
            .memory_root()
            .read(&url("user://profile"))
            .is_err()
    );
    assert!(failure("create-ghost").contains("names no agent"));
    let unseen = failure("unseen");
    assert!(
        unseen.contains("this memory changed since you saw it, or you have not read it"),
        "{unseen}"
    );
    assert!(unseen.contains("- unseen"), "{unseen}");
    // Every change is one transcript line.
    assert_eq!(
        memory_changes(&fixture, session),
        [
            "Saved project://web/build",
            "Edited project://web/build",
            "Saved project://web/build",
            "Deleted project://web/build",
            "Saved user://hobbies",
            "Deleted user://profile",
        ]
    );
    // Results reach the model.
    assert!(requests[2].contains("max size: 2048 bytes"));
    fixture.engine.shutdown().await;
}

/// A delete is a `memory_write` like any other: a rule that denies the URL
/// refuses it and the file stays as it was.
#[tokio::test]
async fn a_delete_follows_the_memory_write_rules() {
    let (endpoint, responses, _captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        &agent("{ memory: allow, memory_write: { \"user://*\": deny, \"*\": allow } }"),
    );
    user_write(&fixture, "user://feedback", "Feedback", "- keep me\n");
    let path = fixture
        .engine
        .memory_root()
        .path()
        .join(url("user://feedback").relative_path());
    let before = std::fs::read(&path).unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "start-denied",
            scripted_tool_body(
                "denied",
                "memory_delete",
                serde_json::json!({"url": "user://feedback"}),
            ),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "denied",
            scripted_text_body("done"),
        ))
        .unwrap();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "go", "start-denied").await;
    let ended = terminations(&fixture, session);
    let denied = &ended["denied"];
    assert_eq!(denied.outcome, ToolTerminationOutcome::Failed, "{denied:?}");
    assert!(
        denied
            .error
            .as_ref()
            .unwrap()
            .message
            .to_string()
            .contains("tool_denied"),
        "{denied:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(memory_changes(&fixture, session).is_empty());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn search_results_follow_the_memory_rules_and_the_limit_counts_what_is_shown() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        &agent("{ memory: { \"project://*\": allow, \"user://*\": ask, \"machine://*\": deny } }"),
    );
    let root = fixture.engine.memory_root().path().to_owned();
    for (index, target) in [
        "machine://labpc/zebra",
        "user://profile",
        "project://web/zebra",
        "project://api/zebra",
    ]
    .into_iter()
    .enumerate()
    {
        // Newest first: the denied machine memory, then the user one.
        let modified = jiff::Timestamp::now() - Duration::from_secs(100 * index as u64);
        let path = root.join(url(target).relative_path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            MemoryDocument::new("About zebras", vec!["zebra".into()], "zebra line\n")
                .unwrap()
                .with_stamps(Some(modified), Some(modified))
                .render(),
        )
        .unwrap();
    }
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "find",
            scripted_tool_body(
                "search-1",
                "memory_search",
                serde_json::json!({"query": "zebra", "limit": 2}),
            ),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "search-1",
            scripted_text_body("done"),
        ))
        .unwrap();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "find", "find").await;
    captured.await.unwrap();
    let ended = terminations(&fixture, session);
    let search = &ended["search-1"];
    assert_eq!(search.outcome, ToolTerminationOutcome::Completed);
    assert_eq!(
        search.result.as_ref().unwrap().output,
        "user://profile — About zebras\n  related · read-only · 1 minute old\n  (reading it needs approval)\n\nproject://web/zebra — About zebras\n  unrelated · read-only · 3 minutes old\n  keywords: zebra"
    );
    // No approval was asked for the search.
    assert!(
        !events(&fixture, session)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ApprovalRequested { .. }))
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_delegated_run_sees_only_its_agent_and_writes_only_its_file() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent(MEMORY));
    user_write(&fixture, "user://feedback", "Feedback", "- root only\n");
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    let child = create_buffered_delegated_child(&fixture.engine, parent);
    // The daemon made the agent's file; the snapshot shows it, so the
    // session has seen it and may replace it.
    // A delegation gives the child its parent's attachments.
    for runtime in ["global", "local"] {
        fixture
            .engine
            .attach_runtime(child, runtime, None, None, origin())
            .await
            .unwrap();
    }
    for (matcher, body) in [
        (
            "delegated work",
            scripted_tool_body(
                "elsewhere",
                "memory_write",
                serde_json::json!({"url": "project://web/notes", "description": "Notes", "keywords": [], "body": ""}),
            ),
        ),
        (
            "elsewhere",
            scripted_tool_body(
                "own",
                "memory_write",
                serde_json::json!({"url": "agent://primary", "description": "Lessons", "keywords": ["checks"], "body": "- check twice\n"}),
            ),
        ),
        ("\"own\"", scripted_text_body("done")),
    ] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                matcher, body,
            ))
            .unwrap();
    }
    run(&fixture, child, &selection, "delegated", "delegated work").await;
    let requests = captured.await.unwrap();

    let taken = snapshots(&fixture, child);
    assert_eq!(taken.len(), 1);
    let snapshot = &taken[0];
    assert!(snapshot.delegated && snapshot.writable);
    assert!(snapshot.user.is_empty() && snapshot.machines.is_empty());
    assert_eq!(snapshot.agent.as_ref().unwrap().agent.as_str(), "primary");
    assert!(!requests[0].contains("root only"));
    // Its agent's file is still empty: nothing to show, so no memory turn
    // (section 7.5), while the memory tools stay offered.
    assert!(!requests[0].contains("# Memory"));
    assert!(memory_tool_names(&requests[0]).contains(&"memory_write".to_owned()));
    // The permission rules alone decide what a delegated run writes
    // (section 16.9.11): this write reaches the store, which refuses it
    // only because the project does not exist.
    let ended = terminations(&fixture, child);
    let refused = &ended["elsewhere"];
    assert_eq!(refused.outcome, ToolTerminationOutcome::Failed);
    assert!(
        refused
            .error
            .as_ref()
            .unwrap()
            .message
            .to_string()
            .contains("which is no project")
    );
    let own = &ended["own"];
    assert_eq!(own.outcome, ToolTerminationOutcome::Completed, "{own:?}");
    // The write keeps the daemon's description and re-adds its keyword,
    // keeping the agent's own, and says so.
    let read = fixture
        .engine
        .memory_root()
        .read(&url("agent://primary"))
        .unwrap();
    let document = read.entry.document.unwrap();
    assert_eq!(
        document.description(),
        "Memory of the `primary` agent: Memory test agent"
    );
    assert_eq!(document.keywords(), ["primary", "checks"]);
    assert_eq!(document.body(), "- check twice\n");
    assert_eq!(
        own.result.as_ref().unwrap().output,
        "Replaced agent://primary. The description of agent://primary is the daemon's and cannot be changed, so it stays \"Memory of the `primary` agent: Memory test agent\". Its keywords always include `primary`, so they were added back; other keywords are kept."
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_finished_compaction_takes_a_new_snapshot_at_the_next_run() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    // Without write access the memory section stays short, so this small
    // history still has something to compact.
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent("{ memory: allow }"));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "before compaction",
            scripted_text_body("done"),
        ))
        .unwrap();
    run(&fixture, session, &selection, "one", "before compaction").await;
    // The user's edit is seen once a compaction makes room for it.
    user_write(&fixture, "user://feedback", "Feedback", "- edited later\n");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_text_body("the summary"),
        ))
        .unwrap();
    assert!(
        fixture
            .engine
            .compact_session(session, None, origin())
            .await
            .unwrap()
    );
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "after compaction",
            scripted_text_body("done"),
        ))
        .unwrap();
    run(&fixture, session, &selection, "two", "after compaction").await;
    let requests = captured.await.unwrap();

    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 2);
    let feedback = |snapshot: &MemorySnapshot| {
        snapshot
            .user
            .iter()
            .find(|category| category.category == "feedback")
            .unwrap()
            .body
            .clone()
    };
    assert_eq!(feedback(&taken[0]), "");
    assert_eq!(feedback(&taken[1]), "- edited later\n");
    // After the compaction the new snapshot sits before the summary.
    let texts = message_texts(&requests[2]);
    let memory = texts
        .iter()
        .position(|text| text.contains("# Memory"))
        .unwrap();
    let summary = texts
        .iter()
        .position(|text| text.contains("the summary"))
        .unwrap();
    assert!(memory < summary, "{texts:?}");
    assert!(texts[memory].contains("edited later"));
    fixture.engine.shutdown().await;
}

/// Memory is opt-in (section 9.2, note of 2026-10-22): an agent document
/// with no `memory` key gets no memory tools and no snapshot.
#[tokio::test]
async fn an_agent_without_a_memory_key_gets_no_memory_tools_or_snapshot() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, &agent("{ read: allow }"));
    user_write(&fixture, "user://feedback", "Feedback", "- never shown\n");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "hello",
            scripted_text_body("done"),
        ))
        .unwrap();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    run(&fixture, session, &selection, "hello", "hello").await;
    assert!(snapshots(&fixture, session).is_empty());
    let requests = captured.await.unwrap();
    assert!(memory_tool_names(&requests[0]).is_empty());
    assert!(!requests[0].contains("# Memory"));
    assert!(!requests[0].contains("never shown"));
    fixture.engine.shutdown().await;
}

/// The snapshot follows the `memory` rules per URL (section 7.5): a denied
/// category is not in it or its text. Rules that leave nothing to show
/// leave out the whole memory turn while the tools stay offered, and an
/// overlay change takes a new snapshot that brings it back.
#[tokio::test]
async fn the_snapshot_shows_only_what_the_memory_rules_let_the_run_read() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        &agent("{ memory: { \"project://*\": allow } }"),
    );
    user_write(&fixture, "user://feedback", "Feedback", "- keep it short\n");
    user_write(
        &fixture,
        "user://preferences",
        "Preferences",
        "- secret preference\n",
    );
    for input in ["one", "two", "three"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;

    // Only project memories are readable and the session has none: the
    // snapshot is taken but holds nothing to show.
    run(&fixture, session, &selection, "one", "one").await;
    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 1);
    assert!(!taken[0].is_empty());
    assert!(taken[0].user.is_empty() && taken[0].user_listed.is_empty());

    // The overlay lets it read the user's memories but not their
    // preferences: a new snapshot without them.
    set_overlay(
        &fixture,
        session,
        vec![
            PermissionRule::new(
                PermissionAction::MEMORY,
                WildcardPattern::new("user://*").unwrap(),
                PermissionEffect::Allow,
            ),
            PermissionRule::new(
                PermissionAction::MEMORY,
                WildcardPattern::new("user://preferences").unwrap(),
                PermissionEffect::Deny,
            ),
        ],
    )
    .await;
    run(&fixture, session, &selection, "two", "two").await;
    let taken = snapshots(&fixture, session);
    assert_eq!(taken.len(), 2, "an overlay change takes a new snapshot");
    assert!(
        taken[1]
            .user
            .iter()
            .any(|category| category.category == "feedback")
    );
    assert!(
        taken[1]
            .user
            .iter()
            .all(|category| category.category != "preferences")
            && !taken[1].user_omitted.contains(&"preferences".to_owned())
    );

    // Nothing changed: the snapshot is kept.
    run(&fixture, session, &selection, "three", "three").await;
    assert_eq!(snapshots(&fixture, session).len(), 2);

    let requests = captured.await.unwrap();
    assert!(!requests[0].contains("# Memory"), "nothing to show");
    assert!(memory_tool_names(&requests[0]).contains(&"memory_read".to_owned()));
    assert!(requests[1].contains("# Memory"));
    assert!(requests[1].contains("keep it short"));
    assert!(!requests[1].contains("user://preferences"));
    assert!(!requests[1].contains("secret preference"));
    fixture.engine.shutdown().await;
}

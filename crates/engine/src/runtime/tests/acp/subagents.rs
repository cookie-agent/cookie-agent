//! The agent's own subagents: child sessions of its session, whose runs are
//! the subagent's own work.

use cookie_agent_protocol::{
    AcpSessionStart, EventPayload, ProducerOwner, RunOutcome, SessionId, SessionOrigin,
};

use super::*;

/// The session of `parent`'s first ACP subagent, once the engine made it.
async fn subagent_of(engine: &Engine, parent: SessionId) -> SessionId {
    with_watchdog("the subagent's session", async {
        loop {
            if let Ok(children) = engine.children(parent)
                && let Some(child) = children.first()
            {
                return child.session_id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
}

/// How the first run of `session` ended.
async fn first_run(engine: &Engine, session: SessionId) -> RunOutcome {
    let event = await_event(engine, session, "the subagent's run", |event| {
        matches!(&event.payload, EventPayload::RunFinished { .. })
    })
    .await;
    match event.payload {
        EventPayload::RunFinished {
            outcome: Open::Known(outcome),
        } => outcome,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agents_subagent_is_a_child_session_with_its_own_work() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(
        engine,
        session,
        "first",
        "subagent reviewer check the tests",
    )
    .await;
    assert_eq!(final_text(&outcome), "subagent done");
    let child = subagent_of(engine, session).await;
    let outcome = first_run(engine, child).await;
    assert_eq!(final_text(&outcome), "subagent says: check the tests");
    let logged = events(engine, child);
    // A child of the session, recorded as the agent's own subagent.
    assert!(logged.iter().any(|event| matches!(
        &event.payload,
        EventPayload::SessionCreated {
            origin: SessionOrigin::Delegated { parent_session_id, depth: 1, .. },
            ..
        } if *parent_session_id == session
    )));
    assert!(logged.iter().any(|event| matches!(
        &event.payload,
        EventPayload::AcpSessionStarted { record } if record.how == AcpSessionStart::Subagent
    )));
    // Its run started with its task, which never went to the agent.
    assert!(logged.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ProducerMessageAccepted {
            producer_owner: ProducerOwner::AcpAgent { .. },
            description,
            body,
            ..
        } if description.as_str() == "Started with a task"
            && body.as_plain_text() == "check the tests"
    )));
    assert!(logged.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallStarted { start } if start.presentation.title.as_str() == "Read notes.txt"
    )));
    let children = engine.children(session).unwrap();
    assert_eq!(
        children[0].title.as_ref().map(|title| title.as_str()),
        Some("reviewer")
    );
    assert_eq!(prompts(&acp).len(), 1, "the subagent got no prompt");
    // The parent shows the task it gave, naming the subagent; the subagent
    // shows it once, as what its run started with.
    let messages = |session| {
        events(engine, session)
            .into_iter()
            .filter_map(|event| match event.payload {
                EventPayload::AcpSessionUpdated { update } => update.session_message,
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        messages(session),
        [cookie_agent_protocol::AcpSessionMessage {
            direction: cookie_agent_protocol::AcpMessageDirection::Sent,
            peer: Some("reviewer".into()),
            text: "check the tests".into(),
        }]
    );
    assert_eq!(messages(child), []);
    // Its parent got no report of it: the parent agent has its result.
    assert!(!events(engine, session).iter().any(|event| matches!(
        &event.payload,
        EventPayload::ProducerMessageAccepted {
            producer_owner: ProducerOwner::Subagent { .. },
            ..
        }
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_permission_request_is_decided_in_its_session() {
    let document = "---\ndescription: Fixture with rules\nmode: all\nacp: {}\npermissions:\n  bash: { \"git *\": allow, \"*\": deny }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "subagent-ask execute git log").await;
    let child = subagent_of(engine, session).await;
    let outcome = first_run(engine, child).await;
    assert_eq!(final_text(&outcome), "subagent asked: yes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagent_takes_no_messages() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "subagent reviewer look").await;
    let child = subagent_of(engine, session).await;
    first_run(engine, child).await;
    wait_for_session_not_running(engine, child).await;
    let outcome = turn(engine, child, "message", "say hello").await;
    assert!(
        matches!(&outcome, RunOutcome::Failed { error, .. }
            if error.as_str().contains("takes no messages")),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cookies_and_the_agents_own_subagents_show_and_stop_side_by_side() {
    let document = "---\ndescription: Fixture that delegates\nmode: all\nacp: {}\npermissions:\n  delegate: { fixture: allow }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    // One of cookie's subagents, through the bridge, then one of the
    // agent's own, still working.
    let call = format!(
        "mcp call delegate_subagent {}",
        serde_json::json!({
            "description": "Child turn",
            "prompt": "say from the child",
            "agent_type": "fixture",
        })
    );
    let outcome = turn(engine, session, "delegate", &call).await;
    assert!(final_text(&outcome).contains("from the child"));
    turn(engine, session, "native", "subagent-wait waiter").await;
    let native = with_watchdog("the agent's subagent working", async {
        loop {
            // Its run lists as background work once it has started, a
            // moment after its status says so.
            if let Some(child) = engine.children(session).unwrap().into_iter().find(|child| {
                child.title.as_ref().map(|title| title.as_str()) == Some("waiter")
                    && child.status == cookie_agent_protocol::SessionStatus::Running
            }) && !engine.background_calls(session).is_empty()
            {
                return child.session_id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    // Both show under the session.
    let mut titles = engine
        .children(session)
        .unwrap()
        .into_iter()
        .filter_map(|child| child.title.map(|title| title.as_str().to_owned()))
        .collect::<Vec<_>>();
    titles.sort();
    assert_eq!(titles, ["Child turn", "waiter"]);
    assert_eq!(engine.tree(session).unwrap().children.len(), 2);
    // The working one is background work of the session, as a subagent of
    // the agent's own, and Stop stops its turn.
    let calls = engine.background_calls(session);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].tool, "subagent");
    assert_eq!(calls[0].display.as_deref(), Some("waiter"));
    engine
        .stop_background_as_user(session, &calls[0].job)
        .await
        .expect("the subagent's turn stops");
    let outcome = first_run(engine, native).await;
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    // The agent logs what it reads on its own time.
    with_watchdog("the agent told to cancel its subagent", async {
        while !acp.received().iter().any(|message| {
            message["method"] == "session/cancel"
                && message["params"]["sessionId"]
                    .as_str()
                    .is_some_and(|id| id.ends_with("-sub"))
        }) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(engine.background_calls(session).is_empty());
}

/// The cookie tool calls `session` recorded, by the tool's name.
fn cookie_calls(engine: &Engine, session: SessionId) -> Vec<String> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::ToolCallStarted { start } => Some(start.presentation.tool.name.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagent_shows_only_what_its_agent_gives_it() {
    let document =
        "---\ndescription: Fixture with a body\nmode: all\nacp: {}\n---\nYou review changes.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "subagent reviewer look").await;
    let child = subagent_of(engine, session).await;
    first_run(engine, child).await;
    let logged = events(engine, child);
    // No memory snapshot: the agent gives its subagent none of cookie's.
    assert!(
        !logged
            .iter()
            .any(|event| matches!(event.payload, EventPayload::MemoryLoaded { .. }))
    );
    // The context went ahead of the parent's first message, which the
    // subagent does not get.
    assert!(!logged.iter().any(|event| matches!(
        &event.payload,
        EventPayload::AcpSessionUpdated { update } if update.prompt_context.is_some()
    )));
    // Its tools are its parent's bridge's.
    let published = |session| {
        events(engine, session)
            .into_iter()
            .rev()
            .find_map(|event| match event.payload {
                EventPayload::ModelToolsPublished { tool_names, .. } => Some(tool_names),
                _ => None,
            })
    };
    assert!(published(session).is_some());
    assert_eq!(published(child), published(session));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagent_of_an_agent_that_takes_a_system_prompt_inherits_it() {
    let document =
        "---\ndescription: Fixture with a body\nmode: all\nacp: {}\n---\nYou review changes.\n";
    let acp = acp_fixture(
        &[("ACP_FIXTURE_SYSTEM_PROMPT", "1")],
        &[("fixture.md", document)],
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "subagent reviewer look").await;
    let child = subagent_of(engine, session).await;
    first_run(engine, child).await;
    let context = |session| {
        events(engine, session)
            .into_iter()
            .find_map(|event| match event.payload {
                EventPayload::AcpSessionUpdated { update } => update.prompt_context,
                _ => None,
            })
    };
    let parent = context(session).expect("the parent's system prompt");
    assert_eq!(
        parent.last().map(String::as_str),
        Some("You review changes.\n")
    );
    assert_eq!(
        context(child),
        Some(parent),
        "the parent's, as the agent hands it on"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_call_of_cookies_tools_shows_once_in_its_own_session() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(
        engine,
        session,
        "first",
        r#"subagent-mcp memory_search {"query": "notes"}"#,
    )
    .await;
    let child = subagent_of(engine, session).await;
    let outcome = first_run(engine, child).await;
    assert!(
        final_text(&outcome).starts_with("subagent result: "),
        "{outcome:?}"
    );
    assert!(
        final_text(&outcome).ends_with("error: False"),
        "{outcome:?}"
    );
    // Cookie's call, in the subagent's session, and not the agent's report
    // of it there or a call in the parent's.
    assert_eq!(cookie_calls(engine, child), ["memory_search"]);
    assert!(
        !cookie_calls(engine, session).contains(&"memory_search".to_owned()),
        "{:?}",
        cookie_calls(engine, session)
    );
}

/// A subagent's call of cookie's tools, made with `word` (the fixture's
/// `subagent-mcp` names the call it reported, `subagent-mcp-unnamed` does
/// not), while the subagent's runner is held as it reaches the agent's
/// report of the call, as a slow write before it would hold it: the bridge
/// call still goes to the subagent's session, not to the parent's after a
/// wait, and runs there under its rules once the runner goes on.
async fn a_held_subagents_call_runs_in_its_own_session(word: &str) {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let (reached, release) = engine.inner.test_hooks.hold_acp_subagent_tool_call();
    let session = engine.create_session(selection()).unwrap().session_id;
    let run = start(
        engine,
        session,
        "first",
        &format!(r#"{word} memory_search {{"query": "notes"}}"#),
    )
    .await;
    let child = subagent_of(engine, session).await;
    with_watchdog("the subagent's runner held at the call", reached)
        .await
        .expect("the hold is reached");
    // Given its session while the runner that would have reported it is
    // still held.
    let attributed = crate::eventually::eventually_some(
        std::time::Duration::from_secs(30),
        "the bridge call given a session",
        || {
            let attributed = engine.inner.test_hooks.bridge_attributions();
            (!attributed.is_empty()).then_some(attributed)
        },
    )
    .await;
    assert_eq!(attributed, [child]);
    release.notify_one();
    let outcome = first_run(engine, child).await;
    assert!(
        final_text(&outcome).ends_with("error: False"),
        "{outcome:?}"
    );
    assert_eq!(
        final_text(&finished(engine, session, run).await),
        "subagent done"
    );
    assert_eq!(cookie_calls(engine, child), ["memory_search"]);
    assert!(
        !cookie_calls(engine, session).contains(&"memory_search".to_owned()),
        "{:?}",
        cookie_calls(engine, session)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_named_call_waits_for_its_report_not_its_runner() {
    a_held_subagents_call_runs_in_its_own_session("subagent-mcp").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_unnamed_call_goes_by_its_report_not_its_runner() {
    a_held_subagents_call_runs_in_its_own_session("subagent-mcp-unnamed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_call_its_rules_do_not_offer_runs_in_the_parent_and_shows_there_only() {
    // The document denies cron; the parent's session allows it, the
    // subagent's does not.
    let document = "---\ndescription: Fixture without cron\nmode: all\nacp: {}\npermissions:\n  cron: deny\n---\nFixture.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    engine
        .append(
            session,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionPermissionOverlaySet {
                overlay: cookie_agent_protocol::SessionPermissionOverlay {
                    rules: vec![cookie_agent_protocol::PermissionRule::new(
                        cookie_agent_protocol::PermissionAction::CRON,
                        cookie_agent_protocol::WildcardPattern::new("*").unwrap(),
                        cookie_agent_protocol::PermissionEffect::Allow,
                    )],
                },
            },
        )
        .await
        .unwrap();
    turn(engine, session, "first", "subagent-mcp cron_list {}").await;
    let child = subagent_of(engine, session).await;
    let outcome = first_run(engine, child).await;
    assert!(
        final_text(&outcome).ends_with("error: False"),
        "{outcome:?}"
    );
    assert_eq!(cookie_calls(engine, child), Vec::<String>::new());
    assert!(cookie_calls(engine, session).contains(&"cron_list".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_left_a_subagent_working_is_not_pushed_when_it_ends() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection()).unwrap().session_id;
    // The agent's turn ends at once, its own subagent still working.
    turn(engine, session, "native", "subagent-wait waiter").await;
    let job = with_watchdog("the agent's subagent working", async {
        loop {
            if let Some(call) = engine.background_calls(session).into_iter().next() {
                return call.job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    engine
        .stop_background_as_user(session, &job)
        .await
        .expect("the subagent's turn stops");
    let native = subagent_of(engine, session).await;
    first_run(engine, native).await;
    // The subagent is done, and the turn that left it working is still not
    // pushed.
    let late = tokio::time::timeout(std::time::Duration::from_millis(500), signals.recv()).await;
    assert!(
        !matches!(late, Ok(Ok(crate::PushSignal::Settled(_)))),
        "a settle for the turn: {late:?}"
    );
}

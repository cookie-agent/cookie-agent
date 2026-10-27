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
            if let Some(child) = engine.children(session).unwrap().into_iter().find(|child| {
                child.title.as_ref().map(|title| title.as_str()) == Some("waiter")
                    && child.status == cookie_agent_protocol::SessionStatus::Running
            }) {
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
    assert!(acp.received().iter().any(|message| {
        message["method"] == "session/cancel"
            && message["params"]["sessionId"]
                .as_str()
                .is_some_and(|id| id.ends_with("-sub"))
    }));
    assert!(engine.background_calls(session).is_empty());
}

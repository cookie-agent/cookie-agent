//! An ACP agent's live state (`docs/site/specs/store-locks-and-durability.md`
//! section 4.3): a call's refined presentations and the agent's context use
//! reach the clients live, a client that joins mid-call gets the newest of
//! each, and the log stores only the call's final presentation, with its
//! end, and the context use the run ended with.

use cookie_agent_protocol::{
    AcpContextUsage, AcpLiveUpdate, EventPayload, EventSubscriptionMessage, SessionId,
};

use super::*;

/// The live state a client joining `session` now gets, after the events it
/// has read up to `cursor` (`None`: from the start).
async fn joined_live_state(
    engine: &Engine,
    session: SessionId,
    cursor: Option<u64>,
) -> Vec<AcpLiveUpdate> {
    let (_, mut live) = engine.subscribe(session, cursor).await.expect("subscribe");
    let mut updates = Vec::new();
    while let Ok(message) = live.try_recv() {
        if let EventSubscriptionMessage::Transient { event } = message
            && let EventPayload::AcpLiveUpdate { update } = &event.payload
        {
            updates.push(update.clone());
        }
    }
    updates
}

fn stored_contexts(engine: &Engine, session: SessionId) -> Vec<(u64, AcpContextUsage)> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AcpSessionUpdated { update } => update.context.map(|c| (event.seq, c)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_joining_mid_call_gets_the_newest_live_state_and_the_log_only_the_last() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let done = acp.marker("done");
    let run = start(
        engine,
        session,
        "late",
        &format!("late-input-wait {}", done.display()),
    )
    .await;
    let newest = AcpContextUsage {
        used: 900,
        size: Some(200_000),
    };
    // The agent changed the call's presentation twice and reported its
    // context twice: a client joining now gets the newest of each, once.
    let joined = with_watchdog("the agent's newest live state", async {
        loop {
            let joined = joined_live_state(engine, session, None).await;
            let title = joined.iter().find_map(|update| {
                update
                    .tool_presentation
                    .as_ref()
                    .map(|presented| presented.presentation.title.as_str().to_owned())
            });
            if title.as_deref() == Some("Read notes.txt")
                && joined.iter().any(|update| update.context == Some(newest))
            {
                break joined;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    let start = events(engine, session)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::ToolCallStarted { start } => Some(start),
            _ => None,
        })
        .expect("the call's start");
    assert_eq!(start.presentation.title.as_str(), "Tool");
    assert_eq!(
        joined
            .iter()
            .filter_map(|update| update.tool_presentation.as_ref())
            .map(|presented| presented.tool_call_id)
            .collect::<Vec<_>>(),
        [start.tool_call_id]
    );
    assert_eq!(
        joined
            .iter()
            .filter_map(|update| update.context)
            .collect::<Vec<_>>(),
        [newest]
    );
    // A client resuming from the tip gets it too: each is a whole value.
    let tip = events(engine, session).last().map(|event| event.seq);
    assert_eq!(joined_live_state(engine, session, tip).await.len(), 2);
    // Mid-run the log stores neither.
    assert!(!events(engine, session).iter().any(|event| matches!(
        &event.payload,
        EventPayload::AcpSessionUpdated { update }
            if update.tool_presentation.is_some() || update.context.is_some()
    )));

    std::fs::write(&done, "").unwrap();
    let outcome = finished(engine, session, run).await;
    assert_eq!(final_text(&outcome), "read");
    wait_for_session_not_running(engine, session).await;
    let events = events(engine, session);
    // The call's end stores its final presentation.
    let (ended_at, termination) = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => {
                Some((event.seq, termination.clone()))
            }
            _ => None,
        })
        .expect("the call's end");
    let presentation = termination.presentation.expect("its final presentation");
    assert_eq!(presentation.title.as_str(), "Read notes.txt");
    assert_eq!(
        presentation
            .primary_argument
            .as_ref()
            .map(|text| text.as_str()),
        Some("/work/notes.txt")
    );
    assert!(!events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::AcpSessionUpdated { update } if update.tool_presentation.is_some()
    )));
    // The context use, once, after the run's work and before its end.
    let finished_at = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::RunFinished { .. }))
        .map(|event| event.seq)
        .expect("the run's end");
    let contexts = stored_contexts(engine, session);
    assert_eq!(contexts.len(), 1, "{contexts:?}");
    assert_eq!(contexts[0].1, newest);
    assert!(ended_at < contexts[0].0 && contexts[0].0 < finished_at);
    // Nothing stays live once the run ended.
    assert!(joined_live_state(engine, session, None).await.is_empty());

    // A run that ends with the context use stored last stores none.
    let outcome = turn(
        engine,
        session,
        "again",
        &format!("late-input-wait {}", done.display()),
    )
    .await;
    assert_eq!(final_text(&outcome), "read");
    assert_eq!(stored_contexts(engine, session).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_whose_presentation_never_changed_stores_none_with_its_end() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "work", "work").await;
    assert_eq!(final_text(&outcome), "Done.");
    let termination = events(engine, session)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination),
            _ => None,
        })
        .expect("the call's end");
    assert_eq!(termination.presentation, None);
}

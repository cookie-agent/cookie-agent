use std::time::Duration;

use cookie_agent_protocol::{
    ClientForkId, ClientRunId, EventOrigin, EventPayload, InputDelivery, RunStartParams,
    SessionStatus, SessionSubmitInputParams, SessionSubmitInputResult,
};

use super::support::*;
use crate::EngineError;

fn text_response(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":null}]})
    )
}

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

#[tokio::test]
async fn revert_needs_the_current_tip_and_a_retried_fork_returns_the_same_fork() {
    let (endpoint, _captured, _reached, _release) =
        scripted_server_with_delayed_response(vec![text_response("answer")], usize::MAX).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("only").unwrap(),
                selection,
                content: "question".into(),
            },
            origin(),
        )
        .await
        .unwrap();
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "completed run",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    let events = projection.log.events();
    let message = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        .expect("submitted input")
        .seq;
    let tip = projection.meta.last_event_seq;

    // Another client's turn landed after this view's tip: refused.
    assert!(matches!(
        fixture
            .engine
            .revert_session(session.session_id, message - 1, origin(), Some(tip - 1))
            .await,
        Err(EngineError::StaleRevert { current_last_seq, .. }) if current_last_seq == tip
    ));
    let key = ClientForkId::new("fork-once").unwrap();
    let forked = fixture
        .engine
        .fork_session(session.session_id, message, origin(), Some(key.clone()))
        .await
        .expect("fork")
        .session_id;
    assert_eq!(
        fixture
            .engine
            .fork_session(session.session_id, message, origin(), Some(key.clone()))
            .await
            .expect("retried fork")
            .session_id,
        forked
    );
    assert!(matches!(
        fixture
            .engine
            .fork_session(session.session_id, message - 1, origin(), Some(key))
            .await,
        Err(EngineError::ForkIdempotencyConflict)
    ));
    fixture
        .engine
        .revert_session(session.session_id, message - 1, origin(), Some(tip))
        .await
        .expect("revert at the current tip");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_manual_compaction_can_be_cancelled_and_input_waits_for_it() {
    let response = |text: &str| text_response(text);
    let old_assistant = format!("OLD_ASSISTANT {}", "old ".repeat(600));
    let (endpoint, _captured, reached, release) = scripted_server_with_delayed_response(
        vec![
            response(&old_assistant),
            response("RECENT_ASSISTANT"),
            response("never delivered summary"),
            response("after compaction"),
        ],
        2,
    )
    .await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: Cancelled compaction test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest cancelled compaction.\n",
            None,
            None,
            false,
            None,
            None,
            8_192,
            None,
        );
    fixture.engine.shutdown().await;
    fixture.config.runtime.context_compaction.keep_recent_tokens = 300;
    fixture.engine = reopen_engine(&fixture);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    for (client_run_id, input) in [("old", "OLD_USER"), ("recent", "RECENT_USER")] {
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(client_run_id).unwrap(),
                    selection: selection.clone(),
                    content: input.into(),
                },
                origin(),
            )
            .await
            .unwrap();
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
    }
    assert!(!fixture.engine.cancel_compaction(session.session_id));

    let engine = fixture.engine.clone();
    let session_id = session.session_id;
    let compaction = tokio::spawn(async move {
        engine
            .compact_session_result(session_id, None, origin())
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("summarizer request")
        .expect("reached");
    // Input sent meanwhile waits for the compaction rather than failing.
    let engine = fixture.engine.clone();
    let submitted = tokio::spawn(async move {
        engine
            .submit_input(
                SessionSubmitInputParams {
                    session_id,
                    content: "after".into(),
                    delivery: InputDelivery::Steer,
                    selection: None,
                    client_run_id: ClientRunId::new("after").unwrap(),
                    reset_fallback: false,
                },
                origin(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!submitted.is_finished());

    assert!(fixture.engine.cancel_compaction(session_id));
    release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), compaction)
        .await
        .expect("compaction ends")
        .expect("join")
        .expect("compaction result");
    assert!(!result.compacted);
    assert_eq!(
        result.cancellation_reason.as_deref(),
        Some("compaction cancelled")
    );
    let started = tokio::time::timeout(Duration::from_secs(5), submitted)
        .await
        .expect("submission ends")
        .expect("join")
        .expect("submission");
    assert!(matches!(started, SessionSubmitInputResult::Started { .. }));
    fixture.engine.shutdown().await;
}

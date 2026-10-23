//! What a session waits on through its subagents
//! (`docs/site/specs/background-tools.md`, section 12).

use std::sync::Arc;

use cookie_agent_protocol::{
    ClientRunId, EventPayload, ProducerOwner, RunStartParams, SessionId, SessionStatus, ToolCallId,
    WaitingOn,
};

use super::background_tasks::{JOB_AGENT, JobControl, JobProvider};
use super::support::*;
use crate::runtime::producers::ProducerAuthority;

/// How many `<subagent_report>` messages `session` accepted.
fn subagent_reports(projection: &crate::session::SessionProjection) -> usize {
    projection
        .log
        .events()
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                EventPayload::ProducerMessageAccepted {
                    producer_owner: ProducerOwner::Subagent { .. },
                    ..
                }
            )
        })
        .count()
}

/// What `session` waits on as a listing shows it, without loading it.
fn listed_waiting(engine: &crate::Engine, session: SessionId) -> Vec<WaitingOn> {
    engine
        .inner
        .store
        .with_live_waiting(engine.inner.store.summary(session).expect("summary").meta)
        .waiting_on
}

/// A parent whose delegated child ran one turn that left a background
/// `job` call running, after the parent's run for that turn's report ended.
struct ChildJob {
    fixture: Fixture,
    parent: SessionId,
    child: SessionId,
    control: Arc<JobControl>,
    captured: tokio::task::JoinHandle<Vec<String>>,
}

async fn start_child_job() -> ChildJob {
    let (endpoint, sender, captured) = scripted_channel_server(4).await;
    for response in [
        MatchedScriptedResponse::last_message_contains(
            "run the child job",
            scripted_tool_body(
                "child-job",
                "job",
                serde_json::json!({"command": "finish", "background": true}),
            ),
        ),
        MatchedScriptedResponse::last_message_contains(
            "Started in the background",
            scripted_text_body("child turn one"),
        ),
        MatchedScriptedResponse::last_message_contains(
            "background_result",
            scripted_text_body("child turn two"),
        ),
        // The scripted server skips subagent reports when it matches, so
        // the parent's first request ends in its system prompt; it answers
        // the second, a report after an answer, on its own.
        MatchedScriptedResponse::last_message_role(
            "system",
            scripted_text_body("parent heard turn one"),
        ),
    ] {
        sender.send(response).expect("scripted response");
    }
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, JOB_AGENT);
    fixture
        .engine
        .register_base_layer()
        .expect("register the base layer");
    let control = Arc::new(JobControl::default());
    // The base layer, since the buffered child has no attachments.
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(JobProvider {
            control: Arc::clone(&control),
        }),
    );
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("parent")
        .session_id;
    let child = create_buffered_delegated_child(&fixture.engine, parent);
    let child_run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: child,
                client_run_id: ClientRunId::new("child-job").expect("client run id"),
                selection,
                content: "run the child job".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("child run started")
        .run_id;
    with_watchdog("detach point", control.detachable.notified()).await;
    wait_for_run_inactive(&fixture.engine, child_run).await;
    await_projection(
        &fixture.engine,
        parent,
        "the parent heard the child's first turn",
        |projection| subagent_reports(projection) == 1,
    )
    .await;
    wait_for_session_not_running(&fixture.engine, parent).await;
    ChildJob {
        fixture,
        parent,
        child,
        control,
        captured,
    }
}

/// Finishes the child's job and waits until the parent heard the child's
/// follow-up turn and its run for it ended.
async fn finish_child_job(job: ChildJob) -> ChildJob {
    job.control.release.notify_one();
    await_projection(
        &job.fixture.engine,
        job.parent,
        "the parent heard the child's follow-up turn",
        |projection| subagent_reports(projection) == 2,
    )
    .await;
    wait_for_session_not_running(&job.fixture.engine, job.child).await;
    wait_for_session_not_running(&job.fixture.engine, job.parent).await;
    job
}

/// A child that ends its turn while its background call runs still counts
/// for its parent: the parent waits on it as a subagent, though no turn of
/// the child is running and its delegation holds no registration, until the
/// call reported and the child's follow-up turn ended.
#[tokio::test]
async fn a_child_waiting_on_its_background_call_keeps_its_parent_waiting() {
    let job = start_child_job().await;
    let engine = job.fixture.engine.clone();
    assert!(engine.background_calls(job.parent).is_empty());
    assert_eq!(
        engine.get_session(job.child).expect("child").waiting_on,
        [WaitingOn::BackgroundCall]
    );
    assert_eq!(
        engine.get_session(job.parent).expect("parent").waiting_on,
        [WaitingOn::Subagent]
    );

    let mut changes = engine.subscribe_session_changes();
    let job = finish_child_job(job).await;
    assert!(
        engine
            .get_session(job.parent)
            .expect("parent")
            .waiting_on
            .is_empty()
    );
    assert!(
        engine
            .get_session(job.child)
            .expect("child")
            .waiting_on
            .is_empty()
    );
    // Clients heard the parent stop waiting.
    await_upserted(
        &mut changes,
        job.parent,
        "the parent stopped waiting",
        |meta| meta.waiting_on.is_empty() && meta.status != SessionStatus::Running,
    )
    .await;
    with_watchdog("scripted server", job.captured)
        .await
        .expect("requests");
    engine.shutdown().await;
}

/// A grandchild that waits on something live keeps every ancestor waiting
/// on a subagent, and its end releases them all.
#[tokio::test]
async fn a_waiting_grandchild_keeps_the_root_waiting() {
    let (fixture, selection) = custom_fixture();
    let engine = fixture.engine.clone();
    let root = engine.create_session(selection).expect("root").session_id;
    let child = create_buffered_delegated_descendant(&engine, root, root, 1);
    let grandchild = create_buffered_delegated_descendant(&engine, root, child, 2);
    let mut changes = engine.subscribe_session_changes();
    let authority = ProducerAuthority {
        owner: ProducerOwner::Tool {
            tool_call_id: ToolCallId::new_v7(),
        },
        connection_epoch: None,
    };
    let producer = engine
        .register_producer(grandchild, authority.clone())
        .await
        .expect("grandchild background call");

    assert_eq!(
        listed_waiting(&engine, grandchild),
        [WaitingOn::BackgroundCall]
    );
    assert_eq!(listed_waiting(&engine, child), [WaitingOn::Subagent]);
    assert_eq!(listed_waiting(&engine, root), [WaitingOn::Subagent]);
    await_upserted(&mut changes, root, "the root waits", |meta| {
        meta.waiting_on == [WaitingOn::Subagent]
    })
    .await;

    engine
        .unregister_producer(grandchild, authority, producer)
        .await
        .expect("the call ended");
    for session in [grandchild, child, root] {
        assert!(listed_waiting(&engine, session).is_empty(), "{session}");
    }
    await_upserted(&mut changes, root, "the root stopped waiting", |meta| {
        meta.waiting_on.is_empty()
    })
    .await;
    engine.shutdown().await;
}

/// A foreground delegation is unchanged: while its call waits for the
/// child's first turn the parent runs and lists no subagent, and the
/// finished delegation leaves it waiting on nothing.
#[tokio::test]
async fn a_foreground_delegation_leaves_its_parent_waiting_on_nothing() {
    let (endpoint, sender, captured) = scripted_channel_server(3).await;
    sender
        .send(MatchedScriptedResponse::last_message_contains(
            "delegate in the foreground",
            scripted_tool_body(
                "foreground-delegate",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type": "worker",
                    "description": "Foreground child",
                    "prompt": "foreground child work"
                }),
            ),
        ))
        .expect("scripted response");
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = fixture.engine.clone();
    engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: engine.clone(),
        }),
    );
    let parent = engine
        .create_session(selection.clone())
        .expect("parent")
        .session_id;
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent,
                client_run_id: ClientRunId::new("foreground").expect("client run id"),
                selection,
                content: "delegate in the foreground".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("parent run");
    let child = await_child(&engine, parent, "running child", |child| {
        child.status == SessionStatus::Running
    })
    .await
    .session_id;
    // The child's turn is held until its response is sent.
    assert!(engine.background_calls(parent).is_empty());
    assert!(listed_waiting(&engine, parent).is_empty());
    assert!(listed_waiting(&engine, child).is_empty());

    for response in [
        MatchedScriptedResponse::last_message_contains(
            "foreground child work",
            scripted_text_body("foreground child report"),
        ),
        MatchedScriptedResponse::last_message_role("tool", scripted_text_body("parent done")),
    ] {
        sender.send(response).expect("scripted response");
    }
    await_projection(&engine, parent, "parent run completion", |projection| {
        projection.status == SessionStatus::Completed
    })
    .await;
    wait_for_session_not_running(&engine, child).await;
    assert!(
        engine
            .get_session(parent)
            .expect("parent")
            .waiting_on
            .is_empty()
    );
    with_watchdog("scripted server", captured)
        .await
        .expect("requests");
    engine.shutdown().await;
}

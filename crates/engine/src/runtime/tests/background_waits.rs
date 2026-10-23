//! What a session waits on through its subagents, and producer waits that
//! leave a session free to page out (`docs/site/specs/background-tools.md`,
//! sections 12 and 13).

use std::sync::Arc;

use cookie_agent_protocol::{
    ClientRunId, EventPayload, ProducerOwner, RunStartParams, SessionId, SessionStatus, ToolCallId,
    WaitingOn,
};

use super::background_tasks::{JobControl, JobProvider, background_result, start_job_run};
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

/// The parent's agent, which its buffered child shares. Memory stays off:
/// a root run's memory snapshot would follow the system prompt, and the
/// parent's first report request has to end in that prompt for the script.
const CHILD_JOB_AGENT: &str = "---\ndescription: Background call test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: allow\n  memory: deny\n---\nTest background calls.\n";

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
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, CHILD_JOB_AGENT);
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

/// A root whose only pending thing is a running background call pages out
/// after its idle window, lists as waiting meanwhile, and the call's result
/// reloads it and starts the follow-up run.
#[tokio::test]
async fn a_root_waiting_only_on_a_background_call_pages_out_and_reloads_for_its_result() {
    let run = start_job_run(
        serde_json::json!({"command": "finish", "background": true}),
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_result",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    let engine = run.fixture.engine.clone();
    let session = run.session_id;
    with_watchdog("detach point", run.control.detachable.notified()).await;
    wait_for_run_inactive(&engine, run.run_id).await;
    wait_for_session_not_running(&engine, session).await;

    // Recent activity still keeps it; the idle window passed, the waiting
    // call does not.
    let evicted = engine
        .evict_idle_roots_with(std::time::Duration::from_secs(60 * 60))
        .await
        .expect("recent janitor pass");
    assert!(evicted.is_empty(), "{evicted:?}");
    let evicted = engine
        .evict_idle_roots_with(std::time::Duration::ZERO)
        .await
        .expect("idle janitor pass");
    assert_eq!(evicted, vec![session]);
    assert!(!engine.inner.store.is_resident(session));
    let listed = engine
        .list_sessions()
        .into_iter()
        .find(|meta| meta.session_id == session)
        .expect("listed root");
    assert_eq!(listed.waiting_on, [WaitingOn::BackgroundCall]);
    assert!(!engine.inner.store.is_resident(session));

    run.control.release.notify_one();
    let projection = await_projection(&engine, session, "the result reloaded it", |projection| {
        background_result(projection).is_some()
            && projection.runs.len() == 2
            && projection.status == SessionStatus::Completed
    })
    .await;
    assert!(background_result(&projection).is_some());
    let requests = with_watchdog("woken run", run.captured)
        .await
        .expect("requests");
    assert!(
        requests
            .last()
            .is_some_and(|request| request.contains("background_result")),
        "{requests:?}"
    );
    assert!(
        engine
            .get_session(session)
            .expect("root")
            .waiting_on
            .is_empty()
    );
    engine.shutdown().await;
}

/// A delegated child whose only pending thing is its running background
/// call pages out after its idle window while its parent keeps waiting on
/// it; the call's result reloads it, starts its follow-up turn, and that
/// turn reports to the parent.
#[tokio::test]
async fn a_child_waiting_only_on_a_background_call_pages_out_and_reloads_for_its_result() {
    let job = start_child_job().await;
    let engine = job.fixture.engine.clone();
    let evicted = engine
        .evict_idle_subagents_for_test(0, std::time::Duration::ZERO)
        .await
        .expect("janitor pass");
    assert_eq!(evicted, vec![job.child]);
    assert!(!engine.inner.store.is_resident(job.child));
    assert_eq!(
        listed_waiting(&engine, job.child),
        [WaitingOn::BackgroundCall]
    );
    assert_eq!(listed_waiting(&engine, job.parent), [WaitingOn::Subagent]);
    assert!(!engine.inner.store.is_resident(job.child));

    let job = finish_child_job(job).await;
    let child = engine.inner.store.get(job.child).expect("child");
    assert!(background_result(&child).is_some());
    assert_eq!(child.runs.len(), 2);
    assert!(listed_waiting(&engine, job.parent).is_empty());
    with_watchdog("scripted server", job.captured)
        .await
        .expect("requests");
    engine.shutdown().await;
}

/// Producer work about to happen still pins: a background result accepted
/// while its wake is held keeps the root resident until the woken run ends.
#[tokio::test]
async fn a_scheduled_wake_with_an_undelivered_result_keeps_the_root_resident() {
    let run = start_job_run(
        serde_json::json!({"command": "finish", "background": true}),
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_result",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    let engine = run.fixture.engine.clone();
    let session = run.session_id;
    with_watchdog("detach point", run.control.detachable.notified()).await;
    wait_for_run_inactive(&engine, run.run_id).await;
    wait_for_session_not_running(&engine, session).await;
    let (reached, release) = engine.install_producer_wake_hook();
    run.control.release.notify_one();
    with_watchdog("held wake", reached)
        .await
        .expect("wake reached");
    assert!(engine.producers_pin_session(session));
    let evicted = engine
        .evict_idle_roots_with(std::time::Duration::ZERO)
        .await
        .expect("janitor pass");
    assert!(evicted.is_empty(), "{evicted:?}");
    assert!(engine.inner.store.is_resident(session));

    release.notify_one();
    await_projection(&engine, session, "the woken run", |projection| {
        projection.runs.len() == 2 && projection.status == SessionStatus::Completed
    })
    .await;
    with_watchdog("woken run", run.captured)
        .await
        .expect("requests");
    engine.shutdown().await;
}

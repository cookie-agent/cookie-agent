//! Native compaction beside the summary, request-time selection of the
//! window, and the provider record (`docs/site/specs/native-compaction.md`
//! section 8).

use cookie_agent_protocol::{
    CheckpointNativeWindow, ClientRunId, ContextCheckpoint, ContextCheckpointCommit, EventOrigin,
    EventPayload, ModelSelection, NativeCompactionState, Open, RunOutcome, RunSelection,
    RunStartParams, SessionId,
};

use super::support::*;
use crate::Engine;

/// A gateway on its own provider, `custom.gw`, serving the Responses
/// protocol at `endpoint` without a compaction setting, and a model of it
/// that opts out.
fn gateway_provider(endpoint: &str) -> String {
    let capabilities = "input = [\"text\"], output = [\"text\"], context_tokens = 4096, output_tokens = 1024, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = \"unsupported\", media = {}";
    format!(
        r#"
[providers."custom.gw"]
source = "custom"
endpoint = "{endpoint}"
adaptor = "openai-compatible"
auth = {{ method = "bearer-api-key-v1", values = {{ api_key = "gw-secret" }} }}

[providers."custom.gw".models.native]
display_name = "Gateway native"
adaptor_options = {{ request_endpoint = "responses" }}
capabilities = {{ {capabilities} }}

[providers."custom.gw".models.plain]
display_name = "Gateway plain"
adaptor_options = {{ request_endpoint = "responses" }}
capabilities = {{ {capabilities}, compaction = "unsupported" }}
"#
    )
}

fn selection(base: &RunSelection, model: &str) -> RunSelection {
    RunSelection {
        model: ModelSelection {
            model: model.parse().unwrap(),
            variant: None,
        },
        ..base.clone()
    }
}

async fn run_turn(engine: &Engine, session: SessionId, selection: &RunSelection, id: &str) {
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(id).unwrap(),
                selection: selection.clone(),
                content: format!("prompt {id}").into(),
            },
            EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    wait_for_session_not_running(engine, session).await;
}

async fn compact(engine: &Engine, session: SessionId) -> bool {
    engine
        .compact_session_result(session, None, EventOrigin::new("client:rpc").unwrap())
        .await
        .expect("compaction")
        .compacted
}

fn events(engine: &Engine, session: SessionId) -> Vec<EventPayload> {
    engine
        .inner
        .store
        .get(session)
        .expect("projection")
        .log
        .events()
        .iter()
        .map(|event| event.payload.clone())
        .collect()
}

fn commits(engine: &Engine, session: SessionId) -> Vec<ContextCheckpointCommit> {
    events(engine, session)
        .into_iter()
        .filter_map(|payload| match payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some(commit),
            _ => None,
        })
        .collect()
}

fn last_outcome(engine: &Engine, session: SessionId) -> RunOutcome {
    events(engine, session)
        .into_iter()
        .rev()
        .find_map(|payload| match payload {
            EventPayload::RunFinished {
                outcome: Open::Known(outcome),
            } => Some(outcome),
            _ => None,
        })
        .expect("a finished run")
}

fn summary_and_window(commit: &ContextCheckpointCommit) -> (&str, Option<&CheckpointNativeWindow>) {
    match &commit.checkpoint {
        ContextCheckpoint::InternalSummary { checkpoint, native } => {
            (checkpoint.summary(), native.as_ref())
        }
        ContextCheckpoint::NativeWindow { .. } => panic!("a window alone is never written"),
    }
}

/// The body of the last request sent to `/responses`.
fn last_response_request(requests: &RouteRequests) -> String {
    requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(line, _)| line.starts_with("POST /v1/responses "))
        .map(|(_, body)| body.clone())
        .expect("a responses request")
}

fn compact_calls(requests: &RouteRequests) -> usize {
    requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(line, _)| line.starts_with("POST /v1/responses/compact "))
        .count()
}

fn openai_mark(engine: &Engine) -> Option<NativeCompactionState> {
    engine
        .configuration_status()
        .native_compaction
        .iter()
        .find(|mark| mark.provider_id == "openai")
        .map(|mark| mark.state)
}

#[tokio::test]
async fn native_compaction_runs_beside_the_summary() {
    let error_404 = (404, r#"{"error":{"message":"Not found"}}"#.to_owned());
    let error_500 = (500, "{}".to_owned());
    let summary_refused = (
        400,
        r#"{"error":{"message":"unrelated invalid request","type":"invalid_request_error","code":"invalid_request"}}"#
            .to_owned(),
    );
    // (compact answer, summary answer, checkpoint expected, window expected,
    // mark expected)
    for (compact_answer, summary_answer, checkpoint, window, mark) in [
        (
            responses_compacted("cmp_both"),
            responses_text_stream("both summary"),
            true,
            true,
            None,
        ),
        (
            error_404,
            responses_text_stream("summary after 404"),
            true,
            false,
            Some(NativeCompactionState::Unsupported),
        ),
        (
            error_500,
            responses_text_stream("summary after 500"),
            true,
            false,
            None,
        ),
        (
            responses_compacted("cmp_unused"),
            summary_refused,
            false,
            false,
            None,
        ),
    ] {
        let (endpoint, requests) = responses_route_server(
            vec![compact_answer],
            vec![responses_text_stream("first answer"), summary_answer],
        )
        .await;
        let (fixture, selection) = managed_openai_compaction_fixture(&endpoint);
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        run_turn(&fixture.engine, session.session_id, &selection, "first").await;
        assert_eq!(
            compact(&fixture.engine, session.session_id).await,
            checkpoint
        );
        // Both calls went out, whatever either answered.
        assert_eq!(compact_calls(&requests), 1);
        let commits = commits(&fixture.engine, session.session_id);
        assert_eq!(commits.len(), usize::from(checkpoint));
        if let Some(commit) = commits.first() {
            let (summary, native) = summary_and_window(commit);
            assert!(summary.starts_with("summary after") || summary == "both summary");
            assert_eq!(native.is_some(), window);
            if let Some(native) = native {
                assert_eq!(
                    native.source_through_seq,
                    commit.boundaries.input_through_seq
                );
            }
        }
        assert_eq!(openai_mark(&fixture.engine), mark);
        fixture.engine.shutdown().await;
    }
}

/// The labpc failure: a natively compacted session switched to another
/// model failed every request; it now continues on the summary.
#[tokio::test]
async fn a_model_switch_after_native_compaction_continues_on_the_summary() {
    let (endpoint, requests) = responses_route_server(
        vec![responses_compacted("cmp_switch")],
        vec![
            responses_text_stream("first answer"),
            responses_text_stream("SWITCH-SUMMARY"),
            responses_text_stream("answer on the other model"),
            responses_text_stream("answer on the gateway"),
            responses_text_stream("answer back on the first model"),
        ],
    )
    .await;
    let (fixture, base) =
        managed_native_compaction_fixture(&endpoint, &gateway_provider(&endpoint));
    let engine = &fixture.engine;
    let session = engine.create_session(base.clone()).unwrap().session_id;
    run_turn(engine, session, &base, "first").await;
    assert!(compact(engine, session).await);
    assert!(summary_and_window(&commits(engine, session)[0]).1.is_some());

    for (model, id) in [
        ("openai/gpt-other", "other-model"),
        ("custom.gw/native", "other-provider"),
    ] {
        run_turn(engine, session, &selection(&base, model), id).await;
        assert!(
            matches!(last_outcome(engine, session), RunOutcome::Completed { .. }),
            "{model}: {:?}",
            last_outcome(engine, session)
        );
        let body = last_response_request(&requests);
        assert!(body.contains("SWITCH-SUMMARY"), "{model}: {body}");
        assert!(!body.contains("cmp_switch"), "{model}: {body}");
    }
    // Back on the window's own model, the window serves again.
    run_turn(engine, session, &base, "back").await;
    assert!(matches!(
        last_outcome(engine, session),
        RunOutcome::Completed { .. }
    ));
    let body = last_response_request(&requests);
    assert!(body.contains("cmp_switch"), "{body}");
    assert!(!body.contains("SWITCH-SUMMARY"), "{body}");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_rejected_window_fails_its_request_and_marks_the_provider() {
    let rejection = (
        400,
        r#"{"error":{"message":"The encrypted content for item cmp_reject could not be verified. Reason: Encrypted content could not be decrypted or parsed.","type":"invalid_request_error","param":null,"code":"invalid_encrypted_content"}}"#
            .to_owned(),
    );
    let (endpoint, requests) = responses_route_server(
        vec![responses_compacted("cmp_reject")],
        vec![
            responses_text_stream("first answer"),
            responses_text_stream("REJECT-SUMMARY"),
            rejection,
            responses_text_stream("answer on the summary"),
        ],
    )
    .await;
    let (fixture, selection) = managed_openai_compaction_fixture(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    run_turn(engine, session, &selection, "first").await;
    assert!(compact(engine, session).await);
    assert_eq!(openai_mark(engine), None);

    run_turn(engine, session, &selection, "rejected").await;
    assert!(matches!(
        last_outcome(engine, session),
        RunOutcome::Failed { .. }
    ));
    assert!(last_response_request(&requests).contains("cmp_reject"));
    assert_eq!(
        openai_mark(engine),
        Some(NativeCompactionState::RejectsWindows)
    );

    run_turn(engine, session, &selection, "after").await;
    assert!(matches!(
        last_outcome(engine, session),
        RunOutcome::Completed { .. }
    ));
    let body = last_response_request(&requests);
    assert!(body.contains("REJECT-SUMMARY"), "{body}");
    assert!(!body.contains("cmp_reject"), "{body}");
    fixture.engine.shutdown().await;
}

/// A window alone, which builds before 1.29.0 wrote, cannot serve another
/// model: the covered events are summarized once, then the run goes on.
#[tokio::test]
async fn a_window_alone_is_summarized_once_for_another_model() {
    let (endpoint, requests) = responses_route_server(
        vec![responses_compacted("cmp_legacy")],
        vec![
            responses_text_stream("donor answer"),
            responses_text_stream("donor summary"),
            responses_text_stream("first answer"),
            responses_text_stream("LEGACY-SUMMARY"),
            responses_text_stream("answer on the other model"),
        ],
    )
    .await;
    let (fixture, base) = managed_openai_compaction_fixture(&endpoint);
    let engine = &fixture.engine;
    // A window compacted in another session of the same model.
    let donor = engine.create_session(base.clone()).unwrap().session_id;
    run_turn(engine, donor, &base, "donor").await;
    assert!(compact(engine, donor).await);
    let donor_commit = commits(engine, donor).remove(0);
    let window = summary_and_window(&donor_commit).1.unwrap().window.clone();
    // The session holds that window alone, as an earlier build wrote it.
    let session = engine.create_session(base.clone()).unwrap().session_id;
    run_turn(engine, session, &base, "first").await;
    let log = engine.inner.store.log(session).unwrap();
    let last = log.last_event().unwrap().seq;
    let run = log.last_run_started().map(|(_, run, _)| run);
    drop(log);
    let mut legacy = donor_commit.clone();
    legacy.checkpoint = ContextCheckpoint::NativeWindow { window };
    legacy.boundaries = cookie_agent_protocol::ContextCheckpointBoundaries {
        source_from_seq: 1,
        source_through_seq: last,
        recent_from_seq: None,
        input_through_seq: last,
        prior_checkpoint_seq: None,
    };
    legacy.budgets.keep_recent_tokens = 0;
    engine
        .append(
            session,
            run,
            crate::runtime::event_origin("engine:auto-compact"),
            EventPayload::ContextCheckpointCommitted { commit: legacy },
        )
        .await
        .unwrap();

    run_turn(
        engine,
        session,
        &selection(&base, "openai/gpt-other"),
        "other",
    )
    .await;
    assert!(
        matches!(last_outcome(engine, session), RunOutcome::Completed { .. }),
        "{:?}",
        last_outcome(engine, session)
    );
    let commits = commits(engine, session);
    assert_eq!(commits.len(), 2);
    let (summary, _) = summary_and_window(&commits[1]);
    assert_eq!(summary, "LEGACY-SUMMARY");
    let body = last_response_request(&requests);
    assert!(body.contains("LEGACY-SUMMARY"), "{body}");
    assert!(!body.contains("cmp_legacy"), "{body}");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_compatible_gateway_compacts_natively_unless_its_model_opts_out() {
    for (model, compacts) in [("custom.gw/native", true), ("custom.gw/plain", false)] {
        let (endpoint, requests) = responses_route_server(
            vec![responses_compacted("cmp_gateway")],
            vec![
                responses_text_stream("first answer"),
                responses_text_stream("gateway summary"),
            ],
        )
        .await;
        let (fixture, base) =
            managed_native_compaction_fixture(&endpoint, &gateway_provider(&endpoint));
        let selection = selection(&base, model);
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        run_turn(&fixture.engine, session.session_id, &selection, "first").await;
        assert!(compact(&fixture.engine, session.session_id).await);
        assert_eq!(compact_calls(&requests), usize::from(compacts), "{model}");
        let commit = commits(&fixture.engine, session.session_id).remove(0);
        assert_eq!(summary_and_window(&commit).1.is_some(), compacts, "{model}");
        fixture.engine.shutdown().await;
    }
}

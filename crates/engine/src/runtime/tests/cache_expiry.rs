//! Compaction before the prompt cache expires, end to end
//! (`docs/site/specs/cache-aware-idle.md` section 4).

use std::time::Duration;

use cookie_agent_config::CacheExpiryMode;
use cookie_agent_protocol::{
    CheckpointReason, ClientRunId, EventPayload, Open, RunStartParams, SessionId,
};

use super::support::*;

fn reply(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
        serde_json::to_string(text).expect("reply text")
    )
}

/// The reason of `session`'s latest checkpoint, when it has one.
fn checkpoint_reason(
    engine: &crate::runtime::Engine,
    session: SessionId,
) -> Option<Option<Open<CheckpointReason>>> {
    engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .event_snapshot()
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some(commit.reason.clone()),
            _ => None,
        })
}

#[tokio::test]
async fn an_idle_session_compacts_quietly_shortly_before_its_assumed_cache_expires() {
    let (endpoint, _captured, _reached, _release) = scripted_server_with_delayed_response(
        // Long enough that a summary makes the context smaller.
        vec![
            reply(&format!("first answer {}", "detail ".repeat(2_000))),
            reply("checkpoint summary"),
        ],
        usize::MAX,
    )
    .await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: Cache expiry test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: deny, cron: deny, list_runtimes: deny, attach: deny, asset: deny, share: deny, webhook: deny }\n---\nTest compaction before cache expiry.\n",
            None,
            None,
            false,
            None,
            None,
            8_192,
            None,
        );
    fixture.engine.shutdown().await;
    fixture.config.runtime.context_compaction.max_summary_bytes = 256;
    // A conversation this short would otherwise stay whole as the recent
    // tail, leaving nothing to replace.
    fixture.config.runtime.context_compaction.keep_recent_tokens = 0;
    let expiry = &mut fixture.config.runtime.context_compaction.cache_expiry;
    expiry.mode = CacheExpiryMode::Always;
    expiry.fallback_ttl = Duration::from_secs(5);
    expiry.min_tokens = 0;
    fixture.engine = reopen_engine(&fixture);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("cache-expiry").unwrap(),
                selection,
                content: "answer, then go idle".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let stamped = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .event_snapshot()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelUsageRecorded { cache_ttl, .. } => Some(*cache_ttl),
            _ => None,
        })
        .flatten()
        .expect("the usage carries the assumed TTL");
    assert_eq!(stamped.ttl_secs, 5);
    let active_before = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .meta
        .last_activity;
    // The TTL is 5 s and the slack a fifth of it: the compaction starts
    // about 4 s after the request.
    let mut reason = None;
    for _ in 0..200 {
        if let Some(found) = checkpoint_reason(&fixture.engine, session.session_id) {
            reason = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let reason = reason
        .expect("a checkpoint before expiry")
        .expect("with a reason");
    assert!(
        matches!(
            reason,
            Open::Known(CheckpointReason::CacheExpiry {
                next_fire: None,
                job_id: None,
                ..
            })
        ),
        "{reason:?}"
    );
    let meta = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .meta
        .clone();
    assert_eq!(
        meta.last_activity, active_before,
        "the compaction is not activity"
    );
    fixture.engine.shutdown().await;
}

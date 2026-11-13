//! Threads of new-session cron jobs and their thread prompts
//! (`docs/site/specs/webhooks.md` sections 7.4, 7.5, and 8).

use std::time::Duration;

use cookie_agent_protocol::{
    CronCreateParams, CronJobId, CronMaxRuns, CronSchedule, CronTarget, CronTemplate,
    CronUpdateParams, EventPayload, ProducerDeliveryMode, RunSelection, SessionId, ThreadOwnerRef,
    thread_prompt_origin,
};
use jiff::Timestamp;

use super::support::*;
use crate::{Engine, crons::StoredJob};

const THREAD_PROMPT: &str = "You keep one log per thread.";
const PROMPT: &str = "Add the latest entry.";

fn template(selection: &RunSelection, key: &str) -> CronTemplate {
    CronTemplate {
        thread_key: Some(key.into()),
        thread_prompt: Some(THREAD_PROMPT.into()),
        ..CronTemplate::for_agent(selection.agent.clone())
    }
}

fn params(target: CronTarget) -> CronCreateParams {
    CronCreateParams {
        name: Some("Log".into()),
        prompt: PROMPT.into(),
        schedule: CronSchedule::Cron("0 3 * * *".into()),
        max_runs: Some(CronMaxRuns::Unlimited),
        target,
        delivery: None,
        permission_mode: None,
        max_consecutive_failures: None,
        enabled: None,
    }
}

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

async fn await_job(
    engine: &Engine,
    id: &CronJobId,
    description: &str,
    predicate: impl Fn(&StoredJob) -> bool,
) -> StoredJob {
    with_watchdog(description, async {
        loop {
            if let Some(job) = engine.cron_store().job(id)
                && predicate(&job)
            {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

/// Fires job `id` as scheduled at `scheduled`, a time past, and waits for
/// its `fires`th fire's run to end.
async fn fire_at(engine: &Engine, id: &CronJobId, scheduled: Timestamp, fires: usize) -> StoredJob {
    let target = id.clone();
    engine
        .crons_now(move |_, jobs| {
            let job = jobs
                .iter_mut()
                .find(|job| job.id == target)
                .expect("the job");
            job.next_fire = Some(scheduled);
            Ok(())
        })
        .expect("made due");
    engine.cron_tick(Timestamp::now()).await;
    await_job(engine, id, "the fire's run", |job| {
        job.history.len() == fires
            && job
                .history
                .last()
                .is_some_and(|fire| fire.run_end.is_some())
    })
    .await
}

fn answer_fire(responses: &tokio::sync::mpsc::UnboundedSender<MatchedScriptedResponse>) {
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "<scheduled_job id=",
            scripted_text_body("logged"),
        ))
        .unwrap();
}

/// The thread prompts job `id` submitted in `session`.
fn thread_prompts(engine: &Engine, session: SessionId, id: &CronJobId) -> usize {
    let origin = thread_prompt_origin(&ThreadOwnerRef::Cron(id.clone()));
    engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .iter()
        .filter(|event| {
            matches!(&event.payload, EventPayload::UserInputSubmitted { content }
                if content.as_plain_text() == THREAD_PROMPT)
                && event.origin.as_ref() == Some(&origin)
        })
        .count()
}

#[tokio::test]
async fn fires_with_one_key_share_a_session_which_gets_the_thread_prompt_once() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let job = engine
        .create_cron(params(CronTarget::New(template(&selection, "%Y-%m-%d"))))
        .await
        .expect("created");
    assert_eq!(job.delivery, Some(ProducerDeliveryMode::Queue));

    answer_fire(&responses);
    let first = fire_at(engine, &job.id, at("2026-01-10T03:00:00Z"), 1).await;
    let thread = first.history[0].session_id.expect("the thread's session");
    assert_eq!(first.history[0].thread_key.as_deref(), Some("2026-01-10"));
    let meta = engine.get_session(thread).unwrap();
    let cron = meta.cron.as_ref().expect("the session records its job");
    assert_eq!(cron.thread_key.as_deref(), Some("2026-01-10"));
    assert!(
        meta.title
            .as_ref()
            .unwrap()
            .to_string()
            .ends_with("· 2026-01-10")
    );

    // A later fire the same day goes into the thread, and the prompt is
    // not sent again.
    answer_fire(&responses);
    let second = fire_at(engine, &job.id, at("2026-01-10T15:00:00Z"), 2).await;
    assert_eq!(second.history[1].session_id, Some(thread));
    assert_eq!(thread_prompts(engine, thread, &job.id), 1);

    // The next day is a thread of its own.
    answer_fire(&responses);
    let third = fire_at(engine, &job.id, at("2026-01-11T03:00:00Z"), 3).await;
    let next_day = third.history[2].session_id.unwrap();
    assert_ne!(next_day, thread);
    assert_eq!(third.threads.session("2026-01-10"), Some(thread));
    assert_eq!(third.threads.session("2026-01-11"), Some(next_day));
    assert_eq!(thread_prompts(engine, next_day, &job.id), 1);

    let requests = captured.await.expect("model requests");
    // The thread prompt comes first, in the same request as the fire.
    let first_request = &requests[0];
    let prompt_at = first_request
        .find(THREAD_PROMPT)
        .expect("the thread prompt");
    let fire_at_ = first_request.find(PROMPT).expect("the fire");
    assert!(prompt_at < fire_at_);
    assert_eq!(requests[1].matches(THREAD_PROMPT).count(), 1);
    assert_eq!(requests[1].matches(PROMPT).count(), 2);
    assert_eq!(requests[2].matches(THREAD_PROMPT).count(), 1);
}

#[tokio::test]
async fn the_thread_prompt_is_sent_again_after_a_compaction() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    // A short tool list and memory section leave this small history
    // something to compact.
    let (mut fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Thread test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: allow, memory_write: deny, cron: deny, asset: deny, share: deny, webhook: deny }\n---\nKeep logs.\n",
    );
    fixture.engine.shutdown().await;
    fixture.config.runtime.context_compaction.max_summary_bytes = 256;
    // So short a conversation would otherwise stay whole as the recent tail.
    fixture.config.runtime.context_compaction.keep_recent_tokens = 0;
    fixture.engine = reopen_engine(&fixture);
    let engine = &fixture.engine;
    let job = engine
        .create_cron(params(CronTarget::New(template(&selection, "main"))))
        .await
        .expect("created");

    answer_fire(&responses);
    let first = fire_at(engine, &job.id, at("2026-01-10T03:00:00Z"), 1).await;
    let thread = first.history[0].session_id.unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_text_body("the summary"),
        ))
        .unwrap();
    let compacted = engine
        .compact_session_result(
            thread,
            None,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("compacted");
    assert!(compacted.compacted, "{compacted:?}");

    answer_fire(&responses);
    let second = fire_at(engine, &job.id, at("2026-01-11T03:00:00Z"), 2).await;
    assert_eq!(second.history[1].session_id, Some(thread));
    assert_eq!(thread_prompts(engine, thread, &job.id), 2);
    let requests = captured.await.expect("model requests");
    assert_eq!(requests[2].matches(THREAD_PROMPT).count(), 1);
}

#[tokio::test]
async fn a_deleted_thread_leaves_the_map_and_a_run_now_uses_the_current_thread() {
    let (endpoint, responses, _captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let job = engine
        .create_cron(params(CronTarget::New(template(&selection, "main"))))
        .await
        .expect("created");

    answer_fire(&responses);
    let first = fire_at(engine, &job.id, at("2026-01-10T03:00:00Z"), 1).await;
    let thread = first.history[0].session_id.unwrap();

    // A run now goes into the thread of the moment.
    answer_fire(&responses);
    let run = engine.run_cron(job.id.clone()).await.expect("ran");
    assert_eq!(run.session_id, Some(thread));
    await_job(engine, &job.id, "the run now's end", |job| {
        job.history.len() == 2 && job.history[1].run_end.is_some()
    })
    .await;

    let last_seq = engine.get_session(thread).unwrap().last_event_seq;
    engine
        .delete_session(thread, last_seq)
        .await
        .expect("deleted");
    let forgotten = engine.cron_store().job(&job.id).unwrap();
    assert_eq!(forgotten.threads.session("main"), None);

    answer_fire(&responses);
    let third = fire_at(engine, &job.id, at("2026-01-11T03:00:00Z"), 3).await;
    let replacement = third.history[2].session_id.unwrap();
    assert_ne!(replacement, thread);
    assert_eq!(third.threads.session("main"), Some(replacement));
    assert_eq!(thread_prompts(engine, replacement, &job.id), 1);
}

#[tokio::test]
async fn thread_fields_and_delivery_are_checked() {
    let (fixture, selection) = custom_fixture();
    let engine = &fixture.engine;

    // Without a thread key, a new-session job takes no delivery.
    let mut plain = params(CronTarget::New(CronTemplate::for_agent(
        selection.agent.clone(),
    )));
    plain.delivery = Some(ProducerDeliveryMode::Steer);
    assert!(engine.create_cron(plain).await.is_err());

    let mut threaded = params(CronTarget::New(template(&selection, "%G-W%V")));
    threaded.delivery = Some(ProducerDeliveryMode::Steer);
    let job = engine.create_cron(threaded).await.expect("created");
    assert_eq!(job.delivery, Some(ProducerDeliveryMode::Steer));

    for refused in [
        CronTemplate {
            thread_key: Some("%Y-%".into()),
            ..template(&selection, "")
        },
        CronTemplate {
            thread_key: Some("x".repeat(65)),
            ..template(&selection, "")
        },
        CronTemplate {
            thread_prompt: Some("  ".into()),
            ..template(&selection, "main")
        },
    ] {
        assert!(
            engine
                .create_cron(params(CronTarget::New(refused.clone())))
                .await
                .is_err(),
            "{refused:?}"
        );
    }

    // Another format forgets the threads of the old one.
    engine
        .crons_now({
            let id = job.id.clone();
            move |_, jobs| {
                let job = jobs.iter_mut().find(|job| job.id == id).unwrap();
                job.threads
                    .record("2026-W46", SessionId::new_v7(), Timestamp::now());
                Ok(())
            }
        })
        .unwrap();
    let mut update = CronUpdateParams::new(job.id.clone());
    update.target = Some(CronTarget::New(template(&selection, "%Y-%m")));
    let updated = engine.update_cron(update).await.expect("updated");
    assert!(updated.threads.is_empty());
    assert_eq!(updated.delivery, Some(ProducerDeliveryMode::Steer));
}

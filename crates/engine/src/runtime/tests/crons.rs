//! Cron jobs (`docs/site/specs/cron.md`): fires into sessions and into new
//! sessions, exactly once across restarts, skips, failures, waiting,
//! deletion, permissions, and `list_agents`.

use std::time::Duration;

use cookie_agent_protocol::{
    CronActor, CronAgentActor, CronCreateParams, CronJobId, CronMaxRuns, CronSchedule, CronTarget,
    CronTargetKind, CronTemplate, CronUpdateParams, EventPayload, PermissionAction,
    PermissionEffect, PermissionMode, PermissionRule, ProducerDeliveryMode, ProjectCreateParams,
    ProjectDeleteParams, SessionId, SessionStatus, WaitingOn, WildcardPattern,
};
use jiff::Timestamp;

use super::support::*;
use crate::{
    Engine,
    crons::{StoredDisabled, StoredFire, StoredJob, StoredOutcome, StoredSchedule, StoredSkip},
    runtime::cron::api::CronRequester,
};

/// What `session` waits on as a listing shows it, without loading it.
fn listed_waiting(engine: &Engine, session: SessionId) -> Vec<WaitingOn> {
    engine
        .inner
        .store
        .for_clients(engine.inner.store.summary(session).expect("summary").meta)
        .waiting_on
}

fn params(prompt: &str, schedule: CronSchedule, target: CronTarget) -> CronCreateParams {
    CronCreateParams {
        name: None,
        prompt: prompt.into(),
        schedule,
        max_runs: None,
        target,
        delivery: None,
        permission_mode: None,
        max_consecutive_failures: None,
        enabled: None,
    }
}

fn daily() -> CronSchedule {
    CronSchedule::Cron("0 3 * * *".into())
}

/// Waits until job `id` satisfies `predicate`.
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

/// Makes job `id` due now and fires what is due.
async fn fire_now(engine: &Engine, id: &CronJobId) {
    let now = Timestamp::now();
    let target = id.clone();
    engine
        .crons_now(move |_, jobs| {
            let job = jobs
                .iter_mut()
                .find(|job| job.id == target)
                .expect("the job");
            job.next_fire = Some(now);
            Ok(())
        })
        .expect("made due");
    engine.cron_tick(Timestamp::now()).await;
}

fn last_outcome(job: &StoredJob) -> StoredOutcome {
    job.history.last().expect("a fire").outcome.clone()
}

/// The `ProducerMessageAccepted` events of job `id` in `session`.
fn fire_messages(engine: &Engine, session: SessionId, id: &CronJobId) -> usize {
    let owner = crate::runtime::cron::fire::cron_owner(id);
    engine
        .inner
        .store
        .get(session)
        .expect("session")
        .log
        .events()
        .iter()
        .filter(|event| {
            matches!(&event.payload, EventPayload::ProducerMessageAccepted { producer_owner, .. }
                if *producer_owner == owner)
        })
        .count()
}

#[tokio::test]
async fn an_in_session_fire_starts_a_run_marked_as_a_scheduled_job() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let mut create = params(
        "Check the dependencies.",
        CronSchedule::In("1h".into()),
        CronTarget::Session(session),
    );
    create.name = Some("Nightly check".into());
    let job = engine.create_cron(create).await.expect("created");
    assert_eq!(job.delivery, Some(ProducerDeliveryMode::Queue));
    assert_eq!(job.max_runs, CronMaxRuns::Count(1));
    assert!(matches!(job.schedule, CronSchedule::At(_)));
    assert!(listed_waiting(engine, session).contains(&WaitingOn::ScheduledJob));

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "<scheduled_job id=",
            scripted_text_body("checked"),
        ))
        .unwrap();
    fire_now(engine, &job.id).await;
    let fired = await_job(engine, &job.id, "the run's end", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.run_end.is_some())
    })
    .await;
    let fire = fired.history.last().unwrap();
    assert_eq!(fire.outcome, StoredOutcome::Delivered);
    assert_eq!(fire.session_id, Some(session));
    assert_eq!(
        fire.run_end.as_ref().unwrap().status,
        SessionStatus::Completed
    );
    // A one-shot you created is kept, disabled as finished.
    assert_eq!(fired.runs, 1);
    assert!(!fired.enabled);
    assert_eq!(fired.disabled_reason, Some(StoredDisabled::Finished));
    assert!(!listed_waiting(engine, session).contains(&WaitingOn::ScheduledJob));

    let requests = captured.await.expect("model requests");
    let marked = format!(
        "<scheduled_job id=\\\"{}\\\" name=\\\"Nightly check\\\" fired_at=",
        job.id
    );
    assert!(requests[0].contains(&marked), "{}", requests[0]);
    assert!(requests[0].contains("Check the dependencies."));
    assert_eq!(fire_messages(engine, session, &job.id), 1);
}

#[tokio::test]
async fn a_fire_is_skipped_while_the_previous_one_still_waits() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let mut create = params("Poll the queue.", daily(), CronTarget::Session(session));
    create.max_runs = Some(CronMaxRuns::Unlimited);
    create.delivery = Some(ProducerDeliveryMode::Steer);
    let job = engine.create_cron(create).await.expect("created");

    // The first fire's run waits for the model, so its prompt is not
    // consumed yet.
    fire_now(engine, &job.id).await;
    await_projection(engine, session, "the fire's run", |projection| {
        projection.status == SessionStatus::Running
    })
    .await;
    fire_now(engine, &job.id).await;
    let skipped = await_job(engine, &job.id, "the skip", |job| job.history.len() == 2).await;
    assert_eq!(
        last_outcome(&skipped),
        StoredOutcome::Skipped {
            reason: StoredSkip::PreviousFireNotDelivered
        }
    );
    assert_eq!(skipped.runs, 1);
    assert!(skipped.enabled);

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Poll the queue.",
            scripted_text_body("polled"),
        ))
        .unwrap();
    let settled = await_job(engine, &job.id, "the first run's end", |job| {
        job.history[0].run_end.is_some()
    })
    .await;
    assert_eq!(
        settled.history[0].run_end.as_ref().unwrap().status,
        SessionStatus::Completed
    );
    assert_eq!(settled.consecutive_failures, 0);
    assert!(
        settled
            .next_fire
            .is_some_and(|next| next > Timestamp::now())
    );
    assert_eq!(fire_messages(engine, session, &job.id), 1);
}

/// A fire's run that ends before the fire is recorded delivered, when
/// nothing awaited it yet, still records its end and counts it.
#[tokio::test]
async fn a_run_that_ends_before_its_fire_is_recorded_delivered_is_counted() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let mut create = params("Tidy the notes.", daily(), CronTarget::Session(session));
    create.max_runs = Some(CronMaxRuns::Unlimited);
    let job = engine.create_cron(create).await.expect("created");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Tidy the notes.",
            scripted_text_body("tidied"),
        ))
        .unwrap();
    // Installed before the job is due: the engine's own scheduler may claim
    // the fire before this test's tick does, and its delivery is held too.
    let (reached, release) = engine.install_cron_settle_hook();
    let target = job.id.clone();
    engine
        .crons_now(move |_, jobs| {
            let job = jobs.iter_mut().find(|job| job.id == target).unwrap();
            job.consecutive_failures = 2;
            job.next_fire = Some(Timestamp::now());
            Ok(())
        })
        .unwrap();
    let ticking = engine.clone();
    let tick = tokio::spawn(async move { ticking.cron_tick(Timestamp::now()).await });
    with_watchdog("the fire's delivery", reached)
        .await
        .expect("delivered");
    await_projection(engine, session, "the fire's run's end", |projection| {
        projection.status == SessionStatus::Completed
    })
    .await;
    assert_eq!(
        last_outcome(&engine.cron_store().job(&job.id).unwrap()),
        StoredOutcome::Pending
    );
    release.notify_one();
    with_watchdog("the tick", tick).await.expect("ticked");

    // Whichever tick claimed it, the fire settles once its delivery does.
    let settled = await_job(engine, &job.id, "the fire's settling", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.outcome != StoredOutcome::Pending && fire.run_end.is_some())
    })
    .await;
    let fire = settled.history.last().unwrap();
    assert_eq!(fire.outcome, StoredOutcome::Delivered);
    assert_eq!(
        fire.run_end.as_ref().map(|end| end.status),
        Some(SessionStatus::Completed)
    );
    assert_eq!(settled.consecutive_failures, 0);
    assert_eq!(settled.history.len(), 1);
}

/// A store that fails leaves a due fire due, so the scheduler waits before
/// it tries again instead of trying at once.
#[tokio::test]
async fn a_failing_store_backs_the_scheduler_off() {
    use crate::runtime::cron::fire::scheduler_sleep;
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let job = engine
        .create_cron(params(
            "Never fires.",
            daily(),
            CronTarget::Session(session),
        ))
        .await
        .expect("created");
    let target = job.id.clone();
    let now = Timestamp::now();
    engine
        .crons_now(move |_, jobs| {
            jobs.iter_mut()
                .find(|job| job.id == target)
                .unwrap()
                .next_fire = Some(now);
            Ok(())
        })
        .unwrap();
    let path = engine.data_root().join(crate::crons::CRONS_FILE);
    let saved = std::fs::read(&path).expect("the store's file");
    std::fs::write(&path, b"not json").unwrap();
    assert!(engine.cron_tick(Timestamp::now()).await, "the claim fails");
    assert_eq!(
        engine.cron_store().job(&job.id).unwrap().next_fire,
        Some(now)
    );
    std::fs::write(&path, saved).unwrap();

    let second = Duration::from_secs(1);
    let past = Some(now);
    assert_eq!(scheduler_sleep(past, now, None), Some(Duration::ZERO));
    assert_eq!(
        scheduler_sleep(past, now, Some(2 * second)),
        Some(2 * second)
    );
    let ahead = Some(now + jiff::SignedDuration::from_secs(10));
    assert_eq!(
        scheduler_sleep(ahead, now, Some(2 * second)),
        Some(10 * second)
    );
    assert_eq!(scheduler_sleep(None, now, Some(2 * second)), None);
}

/// A fire into a held session is skipped, not a failure
/// (`docs/site/specs/held-sessions.md`).
#[tokio::test]
async fn a_fire_into_a_held_session_is_skipped() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    engine
        .on_actor(session, move |engine| {
            engine.append_direct(
                session,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::AutomaticRunsHeld {
                    reason: cookie_agent_protocol::Open::Known(
                        cookie_agent_protocol::HeldReason::Stopped {},
                    ),
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let job = engine
        .create_cron(params("Poll.", daily(), CronTarget::Session(session)))
        .await
        .expect("created");
    fire_now(engine, &job.id).await;
    let skipped = await_job(engine, &job.id, "the skip", |job| job.history.len() == 1).await;
    assert_eq!(
        last_outcome(&skipped),
        StoredOutcome::Skipped {
            reason: StoredSkip::SessionHeld
        }
    );
    assert_eq!(skipped.consecutive_failures, 0);
    assert_eq!(fire_messages(engine, session, &job.id), 0);
    fixture.engine.shutdown().await;
}

/// While a scheduled restart waits, a due fire is delivered as usual; once
/// the restart is due, a due fire is left unclaimed for the restarted daemon
/// and `crons.run` is refused.
#[tokio::test]
async fn fires_run_while_a_restart_waits_and_wait_for_the_daemon_once_it_is_due() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let job = engine
        .create_cron({
            let mut create = params("Poll.", daily(), CronTarget::Session(session));
            create.max_runs = Some(CronMaxRuns::Unlimited);
            create
        })
        .await
        .expect("created");
    // Something the restart waits for keeps it from being due.
    let held = engine.hold_restart();
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook = std::sync::Arc::clone(&called);
    engine.set_restart_hook(Box::new(move || {
        hook.store(true, std::sync::atomic::Ordering::SeqCst);
    }));
    engine
        .schedule_restart(cookie_agent_protocol::RestartWhen::Idle)
        .expect("scheduled");
    fire_now(engine, &job.id).await;
    let delivered = await_job(engine, &job.id, "the fire", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.outcome == StoredOutcome::Delivered)
    })
    .await;
    assert_eq!(delivered.history.len(), 1);
    assert_eq!(fire_messages(engine, session, &job.id), 1);
    // The fire's run started, and the restart waits for it.
    await_projection(engine, session, "the fire's run", |projection| {
        projection.status == SessionStatus::Running
    })
    .await;
    drop(held);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_text_body("done"),
        ))
        .unwrap();
    with_watchdog("the restart to be due", async {
        while !called.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    fire_now(engine, &job.id).await;
    let unclaimed = engine.cron_store().job(&job.id).expect("the job");
    assert_eq!(unclaimed.history.len(), 1, "{:?}", unclaimed.history);
    assert!(
        unclaimed
            .next_fire
            .is_some_and(|next| next <= Timestamp::now())
    );
    let refused = engine.run_cron(job.id.clone()).await.expect_err("refused");
    assert!(
        matches!(refused, crate::EngineError::Restarting),
        "{refused}"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_fire_recorded_before_a_restart_is_delivered_exactly_once() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let delivered = engine
        .create_cron(params(
            "Delivered before.",
            daily(),
            CronTarget::Session(session),
        ))
        .await
        .unwrap();
    let lost = engine
        .create_cron(params("Never sent.", daily(), CronTarget::Session(session)))
        .await
        .unwrap();
    // Both fires are written as pending; only the first one's prompt
    // reaches the session before the daemon stops.
    let fired_at = Timestamp::now();
    let (delivered_id, lost_id) = (delivered.id.clone(), lost.id.clone());
    engine
        .crons_now(move |_, jobs| {
            for job in jobs.iter_mut() {
                let mut fire =
                    StoredFire::with_outcome(Some(fired_at), fired_at, StoredOutcome::Pending);
                fire.key = Some(format!("{}:{fired_at}", job.id));
                job.history.push(fire);
            }
            Ok(())
        })
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Delivered before.",
            scripted_text_body("done"),
        ))
        .unwrap();
    let job = engine.cron_store().job(&delivered_id).unwrap();
    engine
        .send_fire(
            &job,
            session,
            &format!("{delivered_id}:{fired_at}"),
            fired_at,
            ProducerDeliveryMode::Queue,
        )
        .await
        .expect("sent");
    wait_for_session_not_running(engine, session).await;
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    let delivered = await_job(&reopened, &delivered_id, "the delivered fire", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.outcome == StoredOutcome::Delivered)
    })
    .await;
    assert_eq!(delivered.history.last().unwrap().session_id, Some(session));
    assert_eq!(delivered.runs, 1);
    let lost = await_job(&reopened, &lost_id, "the interrupted fire", |job| {
        !matches!(last_outcome(job), StoredOutcome::Pending)
    })
    .await;
    assert_eq!(
        last_outcome(&lost),
        StoredOutcome::Skipped {
            reason: StoredSkip::Interrupted
        }
    );
    assert_eq!(lost.runs, 0);
    assert_eq!(fire_messages(&reopened, session, &delivered_id), 1);
    assert_eq!(fire_messages(&reopened, session, &lost_id), 0);
    reopened.shutdown().await;
}

#[tokio::test]
async fn without_a_grace_fires_missed_while_the_daemon_was_down_are_recorded_and_not_run() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let root = engine.create_session(selection).unwrap().session_id;
    let session = create_buffered_delegated_child(engine, root);
    let hourly = engine
        .create_cron({
            let mut create = params(
                "Hourly.",
                CronSchedule::Cron("0 * * * *".into()),
                CronTarget::Subagent(session),
            );
            create.max_runs = Some(CronMaxRuns::Unlimited);
            create
        })
        .await
        .unwrap();
    let mine = engine
        .create_cron(params(
            "Once.",
            CronSchedule::In("1h".into()),
            CronTarget::Subagent(session),
        ))
        .await
        .unwrap();
    let agents = engine
        .create_cron_as(
            CronRequester::Agent(CronAgentActor {
                agent: "primary".parse().unwrap(),
                session,
            }),
            params(
                "Once.",
                CronSchedule::In("1h".into()),
                CronTarget::Subagent(session),
            ),
        )
        .await
        .unwrap();
    // The hourly job's next fire was two hours before the current hour;
    // the one-shots' times passed too.
    let hour = Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .with()
        .minute(0)
        .second(0)
        .subsec_nanosecond(0)
        .build()
        .unwrap()
        .timestamp();
    let first_missed = hour - jiff::SignedDuration::from_hours(2);
    let (hourly_id, mine_id, agents_id) = (hourly.id.clone(), mine.id.clone(), agents.id.clone());
    engine
        .crons_now(move |_, jobs| {
            for job in jobs.iter_mut() {
                job.next_fire = Some(first_missed);
                if job.schedule.is_one_shot() {
                    job.schedule = StoredSchedule::At(first_missed);
                }
            }
            Ok(())
        })
        .unwrap();
    fixture.engine.shutdown().await;

    fixture.config.runtime.cron.missed_grace = Duration::ZERO;
    let reopened = reopen_engine(&fixture);
    let hourly = await_job(&reopened, &hourly_id, "the missed fires", |job| {
        !job.history.is_empty()
    })
    .await;
    let [fire] = &hourly.history[..] else {
        panic!("one entry for every missed fire: {:?}", hourly.history);
    };
    assert_eq!(
        fire.outcome,
        StoredOutcome::Skipped {
            reason: StoredSkip::DaemonWasDown
        }
    );
    assert_eq!(fire.scheduled_at, Some(first_missed));
    assert_eq!(fire.missed, Some(3));
    assert!(hourly.enabled);
    assert!(hourly.next_fire.is_some_and(|next| next > Timestamp::now()));
    assert_eq!(hourly.runs, 0);
    // A passed one-shot is finished: yours is kept disabled, an agent's
    // is deleted.
    let mine = await_job(&reopened, &mine_id, "your one-shot", |job| !job.enabled).await;
    assert_eq!(mine.disabled_reason, Some(StoredDisabled::Finished));
    assert!(reopened.cron_store().job(&agents_id).is_none());
    reopened.shutdown().await;
}

/// At start, a job's latest fire missed within `[cron] missed_grace` fires
/// once; the fires before it are recorded as missed, and a fire missed
/// longer ago than the grace is not run.
#[tokio::test]
async fn the_latest_fire_missed_within_the_grace_fires_at_start() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    // A session with a run is saved, so the reopened engine finds it. The
    // fires' runs wait for answers that never come.
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_text_body("done"),
        ))
        .unwrap();
    engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("saved").unwrap(),
                selection,
                content: "Save the session.".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    wait_for_session_not_running(engine, session).await;
    let every_minute = engine
        .create_cron({
            let mut create = params(
                "Every minute.",
                CronSchedule::Cron("* * * * *".into()),
                CronTarget::Session(session),
            );
            create.max_runs = Some(CronMaxRuns::Unlimited);
            create
        })
        .await
        .unwrap();
    let recent = engine
        .create_cron(params(
            "Recent.",
            CronSchedule::In("1h".into()),
            CronTarget::Session(session),
        ))
        .await
        .unwrap();
    let old = engine
        .create_cron(params(
            "Old.",
            CronSchedule::In("1h".into()),
            CronTarget::Session(session),
        ))
        .await
        .unwrap();
    let minute = Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .with()
        .second(0)
        .subsec_nanosecond(0)
        .build()
        .unwrap()
        .timestamp();
    let first_missed = minute - jiff::SignedDuration::from_mins(30);
    let recent_at = minute - jiff::SignedDuration::from_mins(2);
    let old_at = minute - jiff::SignedDuration::from_hours(2);
    let (every_minute_id, recent_id, old_id) =
        (every_minute.id.clone(), recent.id.clone(), old.id.clone());
    let ids = (recent_id.clone(), old_id.clone());
    engine
        .crons_now(move |_, jobs| {
            for job in jobs.iter_mut() {
                let at = if job.id == ids.0 {
                    recent_at
                } else if job.id == ids.1 {
                    old_at
                } else {
                    first_missed
                };
                job.next_fire = Some(at);
                if job.schedule.is_one_shot() {
                    job.schedule = StoredSchedule::At(at);
                }
            }
            Ok(())
        })
        .unwrap();
    fixture.engine.shutdown().await;

    fixture.config.runtime.cron.missed_grace = Duration::from_secs(5 * 60);
    let reopened = reopen_engine(&fixture);
    let every_minute = await_job(&reopened, &every_minute_id, "the caught-up fire", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.outcome == StoredOutcome::Delivered)
    })
    .await;
    let [missed, caught_up, ..] = &every_minute.history[..] else {
        panic!("a missed entry and a fire: {:?}", every_minute.history);
    };
    assert_eq!(
        missed.outcome,
        StoredOutcome::Skipped {
            reason: StoredSkip::DaemonWasDown
        }
    );
    assert_eq!(missed.scheduled_at, Some(first_missed));
    // The latest minute before the start fires; every one before it was
    // missed.
    let caught_up_at = caught_up.scheduled_at.expect("scheduled");
    assert!(caught_up_at >= minute, "{caught_up_at}");
    let minutes = caught_up_at.duration_since(first_missed).as_secs() / 60;
    assert_eq!(missed.missed, Some(u64::try_from(minutes).unwrap()));
    assert!(every_minute.enabled);

    let recent = await_job(&reopened, &recent_id, "the recent one-shot", |job| {
        !job.enabled
    })
    .await;
    let [fire] = &recent.history[..] else {
        panic!("only the caught-up fire: {:?}", recent.history);
    };
    assert_eq!(fire.outcome, StoredOutcome::Delivered);
    assert_eq!(fire.scheduled_at, Some(recent_at));
    assert_eq!(recent.disabled_reason, Some(StoredDisabled::Finished));

    let old = await_job(&reopened, &old_id, "the old one-shot", |job| !job.enabled).await;
    let [fire] = &old.history[..] else {
        panic!("only the missed entry: {:?}", old.history);
    };
    assert_eq!(
        fire.outcome,
        StoredOutcome::Skipped {
            reason: StoredSkip::DaemonWasDown
        }
    );
    assert_eq!(fire.missed, Some(1));
    assert_eq!(fire_messages(&reopened, session, &every_minute_id), 1);
    assert_eq!(fire_messages(&reopened, session, &recent_id), 1);
    assert_eq!(fire_messages(&reopened, session, &old_id), 0);
    reopened.shutdown().await;
}

#[tokio::test]
async fn a_new_session_fire_creates_a_run_session_that_records_its_job() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let template = CronTemplate {
        agent: selection.agent.clone(),
        preset: None,
        model: None,
        permission_rules: vec![PermissionRule::new(
            PermissionAction::BASH,
            WildcardPattern::new("git *").unwrap(),
            PermissionEffect::Allow,
        )],
        runtimes: None,
        global: true,
        cwd: None,
        project: None,
    };
    let mut create = params(
        "Write the daily summary.",
        daily(),
        CronTarget::New(template.clone()),
    );
    create.name = Some("Daily summary".into());
    create.max_runs = Some(CronMaxRuns::Count(5));
    let job = engine.create_cron(create).await.expect("created");
    assert_eq!(job.permission_mode, Some(PermissionMode::AutoApproveN));
    assert_eq!(job.delivery, None);

    fire_now(engine, &job.id).await;
    let fired = await_job(engine, &job.id, "the new session", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.outcome != StoredOutcome::Pending)
    })
    .await;
    assert_eq!(last_outcome(&fired), StoredOutcome::Delivered);
    let run_session = fired.history.last().unwrap().session_id.unwrap();
    let meta = engine.get_session(run_session).unwrap();
    let cron = meta.cron.as_ref().expect("the session records its job");
    assert_eq!(cron.job_id, job.id);
    assert_eq!(cron.fired_at, fired.history.last().unwrap().fired_at);
    let title = meta.title.as_ref().expect("a title").to_string();
    assert!(title.starts_with("Daily summary · "), "{title}");
    assert_eq!(
        engine.permission_mode(run_session),
        PermissionMode::AutoApproveN
    );
    let overlay = engine
        .inner
        .store
        .get(run_session)
        .unwrap()
        .permission_overlay
        .clone();
    assert_eq!(overlay.rules, template.permission_rules);
    assert!(
        engine
            .list_sessions()
            .iter()
            .any(|listed| listed.session_id == run_session)
    );

    // While its run waits for the model, the next fire is skipped.
    await_projection(engine, run_session, "the run session's run", |projection| {
        projection.status == SessionStatus::Running
    })
    .await;
    fire_now(engine, &job.id).await;
    let skipped = await_job(engine, &job.id, "the skip", |job| job.history.len() == 2).await;
    assert_eq!(
        last_outcome(&skipped),
        StoredOutcome::Skipped {
            reason: StoredSkip::PreviousRunStillRunning
        }
    );
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "<scheduled_job id=",
            scripted_text_body("summarized"),
        ))
        .unwrap();
    await_job(engine, &job.id, "the run's end", |job| {
        job.history[0].run_end.is_some()
    })
    .await;
    let requests = captured.await.expect("model requests");
    assert!(requests[0].contains("Write the daily summary."));

    // A mode a client sets is the run sessions' mode; a job's session
    // stays when the job is deleted.
    let mut update = CronUpdateParams::new(job.id.clone());
    update.permission_mode = Some(Some(PermissionMode::Yolo));
    let updated = engine.update_cron(update).await.unwrap();
    assert_eq!(updated.permission_mode, Some(PermissionMode::Yolo));
    engine.delete_cron(job.id.clone()).await.unwrap();
    assert!(engine.get_session(run_session).is_ok());
}

#[tokio::test]
async fn agents_cannot_set_a_mode_or_a_model_and_new_sessions_need_a_root_agent() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let agent = CronRequester::Agent(CronAgentActor {
        agent: selection.agent.clone(),
        session,
    });
    let template = CronTemplate {
        agent: selection.agent.clone(),
        preset: None,
        model: None,
        permission_rules: Vec::new(),
        runtimes: None,
        global: true,
        cwd: None,
        project: None,
    };
    let mut moded = params("Run.", daily(), CronTarget::New(template.clone()));
    moded.permission_mode = Some(PermissionMode::Yolo);
    let refused = engine
        .create_cron_as(agent.clone(), moded)
        .await
        .expect_err("an agent's mode");
    assert!(refused.to_string().contains("permission_mode"), "{refused}");
    let mut pinned = template.clone();
    pinned.model = Some(selection.model.clone());
    let refused = engine
        .create_cron_as(
            agent.clone(),
            params("Run.", daily(), CronTarget::New(pinned)),
        )
        .await
        .expect_err("an agent's model");
    assert!(refused.to_string().contains("model"), "{refused}");

    // Its job runs in auto_approve_n; a subagent-mode agent cannot be a
    // new session's agent.
    let job = engine
        .create_cron_as(
            agent.clone(),
            params("Run.", daily(), CronTarget::New(template.clone())),
        )
        .await
        .unwrap();
    assert_eq!(job.permission_mode, Some(PermissionMode::AutoApproveN));
    assert_eq!(
        job.created_by,
        CronActor::Agent(CronAgentActor {
            agent: selection.agent.clone(),
            session
        })
    );
    let mut update = CronUpdateParams::new(job.id.clone());
    update.permission_mode = Some(None);
    assert!(engine.update_cron_as(agent.clone(), update).await.is_err());
    let mut worker = template.clone();
    worker.agent = "worker".parse().unwrap();
    let refused = engine
        .create_cron(params("Run.", daily(), CronTarget::New(worker)))
        .await
        .expect_err("a subagent-mode agent");
    assert!(
        refused.to_string().contains("cannot run as a root"),
        "{refused}"
    );
}

#[tokio::test]
async fn schedules_and_targets_are_checked_when_saved() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let root = engine.create_session(selection).unwrap().session_id;
    let child = create_buffered_delegated_child(engine, root);
    for (schedule, target, member) in [
        (
            CronSchedule::Cron("0 0 30 2 *".into()),
            CronTarget::Session(root),
            "schedule",
        ),
        (
            CronSchedule::In("366d".into()),
            CronTarget::Session(root),
            "schedule",
        ),
        (
            CronSchedule::At("2020-01-01T00:00:00Z".parse().unwrap()),
            CronTarget::Session(root),
            "schedule",
        ),
        (daily(), CronTarget::Session(child), "target"),
        (daily(), CronTarget::Subagent(root), "target"),
        (daily(), CronTarget::Session(SessionId::new_v7()), "target"),
    ] {
        let refused = engine
            .create_cron(params("Run.", schedule.clone(), target))
            .await
            .expect_err("refused");
        assert!(
            refused.to_string().contains(member),
            "{schedule:?}: {refused}"
        );
    }
    let mut once = params(
        "Run.",
        CronSchedule::In("5m".into()),
        CronTarget::Session(root),
    );
    once.max_runs = Some(CronMaxRuns::Count(2));
    assert!(engine.create_cron(once).await.is_err());
    let mut delivered = params("Run.", daily(), CronTarget::Session(root));
    delivered.permission_mode = Some(PermissionMode::Ask);
    assert!(engine.create_cron(delivered).await.is_err());
}

#[tokio::test]
async fn a_subagents_job_keeps_its_parent_waiting_until_it_is_disabled() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let root = engine.create_session(selection).unwrap().session_id;
    let child = create_buffered_delegated_child(engine, root);
    let job = engine
        .create_cron(params("Wake up.", daily(), CronTarget::Subagent(child)))
        .await
        .unwrap();
    assert_eq!(listed_waiting(engine, child), [WaitingOn::ScheduledJob]);
    assert_eq!(listed_waiting(engine, root), [WaitingOn::Subagent]);

    let mut disable = CronUpdateParams::new(job.id.clone());
    disable.enabled = Some(false);
    let disabled = engine.update_cron(disable).await.unwrap();
    assert!(!disabled.enabled);
    assert!(listed_waiting(engine, child).is_empty());
    assert!(listed_waiting(engine, root).is_empty());

    // Enabling it again waits again, with its failure count reset.
    engine
        .crons_now({
            let id = job.id.clone();
            move |_, jobs| {
                jobs.iter_mut()
                    .find(|job| job.id == id)
                    .unwrap()
                    .consecutive_failures = 2;
                Ok(())
            }
        })
        .unwrap();
    let mut enable = CronUpdateParams::new(job.id.clone());
    enable.enabled = Some(true);
    let enabled = engine.update_cron(enable).await.unwrap();
    assert_eq!(enabled.consecutive_failures, 0);
    assert_eq!(listed_waiting(engine, root), [WaitingOn::Subagent]);
}

#[tokio::test]
async fn deleting_a_session_or_a_project_deletes_its_jobs() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let project = engine
        .create_project(ProjectCreateParams {
            name: "cron-test".parse().unwrap(),
            emoji: None,
            defaults: None,
        })
        .await
        .unwrap();
    let root = engine.create_session(selection.clone()).unwrap().session_id;
    let child = create_buffered_delegated_child(engine, root);
    let other = engine.create_session(selection.clone()).unwrap().session_id;
    let child_job = engine
        .create_cron(params("Child.", daily(), CronTarget::Subagent(child)))
        .await
        .unwrap();
    let other_job = engine
        .create_cron(params("Other.", daily(), CronTarget::Session(other)))
        .await
        .unwrap();
    let project_job = engine
        .create_cron(params(
            "Project.",
            daily(),
            CronTarget::New(CronTemplate {
                agent: selection.agent.clone(),
                preset: None,
                model: None,
                permission_rules: Vec::new(),
                runtimes: None,
                global: true,
                cwd: None,
                project: Some(project.id.clone()),
            }),
        ))
        .await
        .unwrap();

    let last_seq = engine.inner.store.get(root).unwrap().meta.last_event_seq;
    engine
        .delete_session(root, last_seq)
        .await
        .expect("deleted");
    assert!(engine.cron_store().job(&child_job.id).is_none());
    assert!(engine.cron_store().job(&other_job.id).is_some());

    let revision = engine
        .get_project(project.id.clone())
        .await
        .unwrap()
        .project
        .revision;
    engine
        .delete_project(ProjectDeleteParams {
            project: project.id.clone(),
            revision,
        })
        .await
        .expect("project deleted");
    assert!(engine.cron_store().job(&project_job.id).is_none());
    assert!(engine.cron_store().job(&other_job.id).is_some());
}

#[tokio::test]
async fn root_runs_get_the_cron_tools_and_list_agents_by_default() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let policy = frozen_root_policy(&fixture, &selection);
    let names = |policy: &crate::policy::FrozenRunPolicy| {
        engine
            .published_tool_definitions(session, policy)
            .expect("composed")
            .definitions
            .into_iter()
            .map(|definition| definition.name)
            .filter(|name| name.starts_with("cron_") || name == "list_agents")
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&policy),
        [
            "cron_create",
            "cron_delete",
            "cron_list",
            "cron_run",
            "cron_update",
            "list_agents"
        ]
    );
    assert!(policy.agent.permissions.iter().any(|rule| {
        rule.action == PermissionAction::CRON
            && rule.resource.as_str() == "*"
            && rule.effect == PermissionEffect::Allow
    }));

    // With cron denied the cron tools go, cron_list with them; without
    // delegation either, so does list_agents.
    let deny_cron = || {
        PermissionRule::new(
            PermissionAction::CRON,
            WildcardPattern::new("*").unwrap(),
            PermissionEffect::Deny,
        )
    };
    let mut no_cron = policy.clone();
    no_cron
        .agent
        .permissions
        .retain(|rule| rule.action != PermissionAction::CRON);
    no_cron.agent.permissions.push(deny_cron());
    assert!(names(&no_cron).is_empty());

    // Rules for one kind show only that kind's jobs.
    let mut sessions_only = policy.clone();
    sessions_only
        .agent
        .permissions
        .retain(|rule| rule.action != PermissionAction::CRON);
    sessions_only.agent.permissions.push(deny_cron());
    sessions_only.agent.permissions.push(PermissionRule::new(
        PermissionAction::CRON,
        WildcardPattern::new("session").unwrap(),
        PermissionEffect::Ask,
    ));
    assert!(engine.cron_kind_visible_for_test(&sessions_only, session, CronTargetKind::Session));
    assert!(!engine.cron_kind_visible_for_test(&sessions_only, session, CronTargetKind::New));
    assert!(!engine.cron_kind_visible_for_test(&no_cron, session, CronTargetKind::Session));
}

#[tokio::test]
async fn agents_target_only_themselves_and_sessions_below_them() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let root = engine.create_session(selection.clone()).unwrap().session_id;
    let child = create_buffered_delegated_child(engine, root);
    assert_eq!(
        engine
            .cron_target_for_test(root, serde_json::json!("self"))
            .unwrap(),
        CronTarget::Session(root)
    );
    assert_eq!(
        engine
            .cron_target_for_test(child, serde_json::json!("self"))
            .unwrap(),
        CronTarget::Subagent(child)
    );
    // Neither the root above it nor a session elsewhere is below the child
    // (handles.rs tests which sessions are).
    for target in [root.to_string(), SessionId::new_v7().to_string()] {
        let refused = engine
            .cron_target_for_test(child, serde_json::json!(target))
            .expect_err("not below the caller");
        assert!(
            refused
                .to_string()
                .contains("is not a session below this one")
        );
    }

    // A new-session template takes the caller's agent, runtimes, and
    // project by default.
    let CronTarget::New(template) = engine
        .cron_target_for_test(root, serde_json::json!({"new": {}}))
        .unwrap()
    else {
        panic!("a new-session target");
    };
    assert_eq!(template.agent, selection.agent);
    assert!(template.global);
    let attached = engine.session_runtimes(root).unwrap();
    assert_eq!(
        template.runtimes.unwrap().len(),
        attached
            .iter()
            .filter(|attachment| attachment.runtime_id.as_str() != "global")
            .count()
    );
    assert_eq!(template.project, None);
}

/// `list_agents` for a primary agent with declared keys, a `subagent`
/// agent (delegation only: a new-session job cannot run it), and an `all`
/// agent (both uses, so a root block and a subagent block): declared keys in
/// full with `${agent_id}` expanded, default-only actions on one line per
/// block, the defaults once at the end, and the closing line.
#[tokio::test]
async fn list_agents_shows_declared_rules_and_collapses_the_defaults() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let primary = "---\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  write:\n    \"${workspace_dir}/*\": allow\n    \"*\": ask\n  memory_write:\n    \"agent://${agent_id}\": allow\n  delegate:\n    worker: allow\n    helper: allow\n---\nPrimary prompt.\n";
    let helper = "---\ndescription: Helper test agent\nmode: all\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: ask\n---\nHelper prompt.\n";
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        primary,
        Some(("helper.md", helper)),
        None,
        false,
    );
    let policy = frozen_root_policy(&fixture, &selection);
    let runtime = fixture.engine.current_runtime();
    let overlay = cookie_agent_protocol::SessionPermissionOverlay::default();
    let list =
        |filter| crate::agents_tool::render_agent_list(&policy, &overlay, 0, filter, &runtime, &[]);
    assert_eq!(
        list(None),
        "\
helper (all) — Helper test agent
  models: custom.test/group/model
  usable for: delegate, cron
  permissions as a root run:
    bash: ask
    defaults: read, write, webfetch, skill, mcp, delegate, list_runtimes, memory, memory_write, cron, attach, message
  permissions as a subagent:
    bash: ask
    defaults: read, write, webfetch, skill, mcp, delegate, list_runtimes, memory, memory_write, message
primary (primary) — Primary test agent
  models: custom.test/group/model
  usable for: cron
  permissions:
    read: allow
    write: ${workspace_dir}/* allow; * ask
    memory_write: agent://primary allow
    delegate: worker allow; helper allow
    defaults: webfetch, skill, mcp, list_runtimes, memory, cron, attach, message
worker (subagent) — Worker test agent
  models: custom.test/group/model
  usable for: delegate
  permissions:
    defaults: read, write, webfetch, skill, mcp, delegate, memory, memory_write, message

Defaults (for actions an agent document names no key for):
  as a root run:
    read: allow
    write: ${workspace_dir}/* allow; * ask
    webfetch: allow
    skill: allow
    mcp: ask
    delegate: deny
    list_runtimes: allow
    memory: allow
    memory_write: user://* allow; agent://<agent id> allow; machine://${session_machine}/* allow; project://${session_project}/* allow
    cron: allow
    attach: allow
    message: as `delegate`: allow when a delegate rule allows, ask when one asks, else deny
  as a subagent:
    read: allow
    write: ${workspace_dir}/* allow; * ask
    webfetch: allow
    skill: allow
    mcp: ask
    delegate: deny
    list_runtimes: allow (only agents of mode all)
    memory: agent://<agent id> allow
    memory_write: agent://<agent id> allow
    message: allow
  Internal agents are denied every action.
  Variables: ${session_machine} is each machine the session's attachments run on; ${session_project} is the session's project; ${workspace_dir} is the session's workspace.

Whatever no rule matches asks."
    );
    // For delegation only: the subagent-side defaults alone.
    assert_eq!(
        list(Some(crate::agents_tool::Use::Delegate)),
        "\
helper (all) — Helper test agent
  models: custom.test/group/model
  usable for: delegate
  permissions:
    bash: ask
    defaults: read, write, webfetch, skill, mcp, delegate, list_runtimes, memory, memory_write, message
worker (subagent) — Worker test agent
  models: custom.test/group/model
  usable for: delegate
  permissions:
    defaults: read, write, webfetch, skill, mcp, delegate, memory, memory_write, message

Defaults (for actions an agent document names no key for):
  as a subagent:
    read: allow
    write: ${workspace_dir}/* allow; * ask
    webfetch: allow
    skill: allow
    mcp: ask
    delegate: deny
    list_runtimes: allow (only agents of mode all)
    memory: agent://<agent id> allow
    memory_write: agent://<agent id> allow
    message: allow
  Internal agents are denied every action.
  Variables: ${workspace_dir} is the session's workspace.

Whatever no rule matches asks."
    );
    // For cron: only the agents a new-session job can run.
    let cron = list(Some(crate::agents_tool::Use::Cron));
    assert!(cron.starts_with("helper (all)"), "{cron}");
    assert!(cron.contains("\nprimary (primary)"), "{cron}");
    assert!(!cron.contains("worker (subagent)"), "{cron}");
    assert!(!cron.contains("as a subagent"), "{cron}");
}

#[test]
fn failures_count_across_skips_reset_on_completion_and_not_on_cancel() {
    use crate::crons::{StoredRunEnd, StoredTarget};
    let session = SessionId::new_v7();
    let mut job = StoredJob {
        id: CronJobId::new("aaaabbbb").unwrap(),
        name: None,
        prompt: "Run.".into(),
        schedule: StoredSchedule::Cron("0 3 * * *".into()),
        max_runs: CronMaxRuns::Unlimited,
        target: StoredTarget::Session(session),
        delivery: Some(ProducerDeliveryMode::Queue),
        permission_mode: None,
        max_consecutive_failures: None,
        enabled: true,
        disabled_reason: None,
        created_by: CronActor::User,
        created_at: Timestamp::now(),
        next_fire: Some(Timestamp::now()),
        runs: 0,
        consecutive_failures: 0,
        last_fire: None,
        history: Vec::new(),
    };
    let run_with = |job: &mut StoredJob, status: SessionStatus, uncounted: bool| {
        let mut fire = StoredFire::with_outcome(None, Timestamp::now(), StoredOutcome::Delivered);
        fire.session_id = Some(session);
        fire.uncounted = uncounted;
        job.history.push(fire);
        let mut jobs = vec![job.clone()];
        crate::runtime::cron::fire::record_run_end(
            &mut jobs,
            session,
            cookie_agent_protocol::RunId::new_v7(),
            StoredRunEnd {
                status,
                at: Timestamp::now(),
            },
            |_| true,
            3,
        );
        *job = jobs.remove(0);
    };
    run_with(&mut job, SessionStatus::Failed, false);
    job.record(
        StoredFire::skipped(None, Timestamp::now(), StoredSkip::PreviousFireNotDelivered),
        20,
    );
    run_with(&mut job, SessionStatus::Failed, false);
    assert_eq!(job.consecutive_failures, 2);
    run_with(&mut job, SessionStatus::Cancelled, false);
    run_with(&mut job, SessionStatus::Failed, true);
    assert_eq!(job.consecutive_failures, 2);
    run_with(&mut job, SessionStatus::Completed, false);
    assert_eq!(job.consecutive_failures, 0);
    for _ in 0..3 {
        run_with(&mut job, SessionStatus::Failed, false);
    }
    assert!(!job.enabled);
    assert_eq!(
        job.disabled_reason,
        Some(StoredDisabled::Failed { failures: 3 })
    );
}

#[tokio::test]
async fn cron_run_runs_a_new_session_job_as_a_subagent_of_the_caller() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let rules = vec![PermissionRule::new(
        PermissionAction::BASH,
        WildcardPattern::new("git *").unwrap(),
        PermissionEffect::Allow,
    )];
    // The job's agent is `primary`, which no delegation reaches.
    let job = engine
        .create_cron(params(
            "Summarize the day.",
            daily(),
            CronTarget::New(CronTemplate {
                agent: selection.agent.clone(),
                preset: None,
                model: None,
                permission_rules: rules.clone(),
                runtimes: None,
                global: true,
                cwd: None,
                project: None,
            }),
        ))
        .await
        .unwrap();
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    for response in [
        MatchedScriptedResponse::last_message_contains(
            "run the job now",
            scripted_tool_body(
                "cron-run",
                "cron_run",
                serde_json::json!({ "id": job.id.as_str() }),
            ),
        ),
        MatchedScriptedResponse::last_message_contains(
            "<scheduled_job id=",
            scripted_text_body("child summary"),
        ),
        MatchedScriptedResponse::last_message_contains(
            "child summary",
            scripted_text_body("parent done"),
        ),
    ] {
        responses.send(response).unwrap();
    }
    engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("cron-run").unwrap(),
                selection,
                content: "run the job now".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("parent run");
    let child = await_child(engine, session, "the job's child", |_| true).await;
    assert_eq!(child.agent.as_str(), "primary");
    let child_meta = engine.get_session(child.session_id).unwrap();
    assert_eq!(
        child_meta.cron.as_ref().map(|cron| &cron.job_id),
        Some(&job.id)
    );
    assert_eq!(
        engine
            .inner
            .store
            .get(child.session_id)
            .unwrap()
            .permission_overlay
            .rules,
        rules
    );
    let ran = await_job(engine, &job.id, "the run now's end", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.run_end.is_some())
    })
    .await;
    let fire = ran.history.last().unwrap();
    assert!(fire.run_now);
    assert_eq!(fire.session_id, Some(child.session_id));
    assert_eq!(
        fire.run_end.as_ref().unwrap().status,
        SessionStatus::Completed
    );
    // A run now counts toward neither max_runs nor the failure count.
    assert_eq!(ran.runs, 0);
    assert!(ran.enabled);
    wait_for_session_not_running(engine, session).await;
    let requests = captured.await.expect("model requests");
    assert!(requests[1].contains("Summarize the day."));
    assert!(requests[2].contains("child summary"));
}

#[tokio::test]
async fn a_job_does_not_keep_its_session_in_memory_and_its_fire_reloads_it() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    for response in [
        MatchedScriptedResponse::last_message_contains("first turn", scripted_text_body("ok")),
        MatchedScriptedResponse::last_message_contains(
            "Wake the paged-out session.",
            scripted_text_body("awake"),
        ),
    ] {
        responses.send(response).unwrap();
    }
    engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("first").unwrap(),
                selection,
                content: "first turn".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first run");
    wait_for_session_not_running(engine, session).await;
    let mut create = params(
        "Wake the paged-out session.",
        daily(),
        CronTarget::Session(session),
    );
    create.max_runs = Some(CronMaxRuns::Unlimited);
    let job = engine.create_cron(create).await.unwrap();
    settle_session_actor(engine, session).await;
    let evicted = engine
        .evict_idle_roots_with(Duration::ZERO)
        .await
        .expect("idle janitor pass");
    assert_eq!(evicted, vec![session]);
    assert!(!engine.inner.store.is_resident(session));
    let listed = engine
        .list_sessions()
        .into_iter()
        .find(|meta| meta.session_id == session)
        .expect("listed root");
    assert_eq!(listed.waiting_on, [WaitingOn::ScheduledJob]);

    fire_now(engine, &job.id).await;
    let fired = await_job(engine, &job.id, "the fire's run", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.run_end.is_some())
    })
    .await;
    assert_eq!(fired.runs, 1);
    assert!(engine.inner.store.is_resident(session));
    let requests = captured.await.expect("model requests");
    assert!(requests[1].contains("Wake the paged-out session."));
}

/// Starts a run of `session` with `content`.
async fn start(
    engine: &Engine,
    session: SessionId,
    selection: &cookie_agent_protocol::RunSelection,
    id: &str,
    content: &str,
) -> cookie_agent_protocol::RunId {
    engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new(id).unwrap(),
                selection: selection.clone(),
                content: content.into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started")
        .run_id
}

#[tokio::test]
async fn a_queued_fire_waits_for_the_running_turn_and_starts_the_next_run() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let mut create = params(
        "Queued behind the turn.",
        daily(),
        CronTarget::Session(session),
    );
    create.max_runs = Some(CronMaxRuns::Unlimited);
    let job = engine.create_cron(create).await.unwrap();

    // The user's turn waits for the model while the job fires.
    let busy = start(engine, session, &selection, "busy", "the busy turn").await;
    fire_now(engine, &job.id).await;
    let delivered = await_job(engine, &job.id, "the queued fire", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.outcome == StoredOutcome::Delivered)
    })
    .await;
    assert_eq!(delivered.runs, 1);
    let message = delivered.history.last().unwrap().message_id.unwrap();
    let pending = super::producers::producer_projection(engine, session);
    let record = pending
        .messages
        .iter()
        .find(|record| record.message_id == message)
        .expect("the fire's message");
    assert!(!record.consumed && record.admission.is_none(), "{record:?}");

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "the busy turn",
            scripted_text_body("busy done"),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Queued behind the turn.",
            scripted_text_body("queued done"),
        ))
        .unwrap();
    let settled = await_job(engine, &job.id, "the queued fire's run", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.run_end.is_some())
    })
    .await;
    let fire = settled.history.last().unwrap();
    let fire_run = fire.run_id.expect("the run that took the prompt");
    assert_ne!(fire_run, busy, "a queued prompt starts its own run");
    assert_eq!(
        fire.run_end.as_ref().unwrap().status,
        SessionStatus::Completed
    );
    let projection = engine.inner.store.get(session).unwrap();
    assert_eq!(projection.runs[&busy].status, SessionStatus::Completed);
    assert_eq!(projection.runs.len(), 2);
    let requests = captured.await.expect("model requests");
    assert!(!requests[0].contains("Queued behind the turn."));
    assert!(requests[1].contains("Queued behind the turn."));
}

#[tokio::test]
async fn a_steered_fire_joins_the_running_turn_and_follows_its_outcome() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let mut create = params(
        "Steered into the turn.",
        daily(),
        CronTarget::Session(session),
    );
    create.max_runs = Some(CronMaxRuns::Unlimited);
    create.delivery = Some(ProducerDeliveryMode::Steer);
    let job = engine.create_cron(create).await.unwrap();

    let busy = start(engine, session, &selection, "busy", "the busy turn").await;
    fire_now(engine, &job.id).await;
    await_job(engine, &job.id, "the steered fire", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.outcome == StoredOutcome::Delivered)
    })
    .await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "the busy turn",
            scripted_text_body("busy done"),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Steered into the turn.",
            scripted_text_body("steered done"),
        ))
        .unwrap();
    let settled = await_job(engine, &job.id, "the steered fire's run", |job| {
        job.history
            .last()
            .is_some_and(|fire| fire.run_end.is_some())
    })
    .await;
    let fire = settled.history.last().unwrap();
    assert_eq!(
        fire.run_id,
        Some(busy),
        "a steered prompt joins the running turn"
    );
    assert_eq!(
        fire.run_end.as_ref().unwrap().status,
        SessionStatus::Completed
    );
    assert_eq!(settled.consecutive_failures, 0);
    let projection = engine.inner.store.get(session).unwrap();
    assert_eq!(projection.runs.len(), 1);
    let requests = captured.await.expect("model requests");
    assert!(requests[1].contains("Steered into the turn."));
}

fn turn_context(
    policy: &crate::policy::FrozenRunPolicy,
) -> std::sync::Arc<crate::TurnAgentContext> {
    let binding = policy
        .selected_suffix
        .first()
        .expect("frozen binding")
        .clone();
    std::sync::Arc::new(crate::TurnAgentContext {
        agent: policy.agent.agent.clone(),
        model: binding.selection.model.clone(),
        adapter: crate::policy::wire_adapter(binding.descriptor.adapter_id.as_str()),
        input_media: crate::policy::tool_input_media(
            binding.descriptor.adapter_id.as_str(),
            &policy
                .model_capabilities(&binding)
                .expect("model capabilities"),
        ),
    })
}

#[tokio::test]
async fn cron_tools_check_each_target_kind_a_call_touches() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let policy = frozen_root_policy(&fixture, &selection);
    let job = engine
        .create_cron(params("Labelled.", daily(), CronTarget::Session(session)))
        .await
        .unwrap();
    let labels = |name: &'static str, arguments: serde_json::Value| {
        let policy = policy.clone();
        async move {
            let prepared = engine
                .prepare_tool_call(
                    session,
                    cookie_agent_protocol::RunId::new_v7(),
                    crate::ToolCall {
                        id: cookie_agent_protocol::ToolCallId::new_v7(),
                        name: name.into(),
                        arguments,
                    },
                    &policy,
                    turn_context(&policy),
                )
                .await;
            assert_eq!(prepared.permission_name.as_deref(), Some("cron"), "{name}");
            prepared
                .prepared
                .unwrap_or_else(|failure| panic!("{name}: {}", failure.message))
                .policy_labels()
                .iter()
                .map(|label| label.clone().expect("a label"))
                .collect::<Vec<_>>()
        }
    };
    let id = job.id.as_str();
    assert_eq!(
        labels(
            "cron_update",
            serde_json::json!({ "id": id, "target": { "new": {} } })
        )
        .await,
        ["session", "new"]
    );
    assert_eq!(
        labels(
            "cron_update",
            serde_json::json!({ "id": id, "target": "self" })
        )
        .await,
        ["session"]
    );
    assert_eq!(
        labels(
            "cron_update",
            serde_json::json!({ "id": id, "name": "Renamed" })
        )
        .await,
        ["session"]
    );
    assert_eq!(
        labels(
            "cron_create",
            serde_json::json!({
                "prompt": "Run.", "schedule": { "in": "1h" }, "target": { "new": {} }
            })
        )
        .await,
        ["new"]
    );
    assert_eq!(
        labels("cron_delete", serde_json::json!({ "id": id })).await,
        ["session"]
    );
    assert_eq!(
        labels("cron_run", serde_json::json!({ "id": id })).await,
        ["session"]
    );
}

#[tokio::test]
async fn a_rule_that_denies_the_new_kind_refuses_the_change_of_kind() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Cron kinds\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: deny, cron: { session: allow, new: deny } }\n---\nKinds.\n",
    );
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let job = engine
        .create_cron(params(
            "Stay in the session.",
            daily(),
            CronTarget::Session(session),
        ))
        .await
        .unwrap();
    for response in [
        MatchedScriptedResponse::last_message_contains(
            "move the job",
            scripted_tool_body(
                "cron-update",
                "cron_update",
                serde_json::json!({ "id": job.id.as_str(), "target": { "new": {} } }),
            ),
        ),
        MatchedScriptedResponse::last_message_role("tool", scripted_text_body("refused")),
    ] {
        responses.send(response).unwrap();
    }
    start(engine, session, &selection, "move", "move the job").await;
    wait_for_session_not_running(engine, session).await;
    let terminated = engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination.outcome),
            _ => None,
        })
        .expect("the call ended");
    assert_ne!(
        terminated,
        cookie_agent_protocol::ToolTerminationOutcome::Completed
    );
    assert_eq!(
        engine.cron_store().job(&job.id).unwrap().target,
        crate::crons::StoredTarget::Session(session)
    );
    let requests = captured.await.expect("model requests");
    assert!(
        requests[1].contains("deny") || requests[1].contains("denied"),
        "{}",
        requests[1]
    );
}

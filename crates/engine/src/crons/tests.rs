use cookie_agent_protocol::{
    AgentId, CronActor, CronAgentActor, CronJobId, CronMaxRuns, ProducerDeliveryMode, SessionId,
};
use jiff::{Timestamp, tz::TimeZone};

use super::{
    CronStore, StoredDisabled, StoredFire, StoredJob, StoredSchedule, StoredSkip, StoredTarget,
    schedule::{CronExpr, parse_delay},
};

fn at(text: &str) -> Timestamp {
    text.parse().expect("timestamp")
}

fn next(expression: &str, after: &str, zone: &TimeZone) -> Timestamp {
    CronExpr::parse(expression)
        .expect("expression")
        .next_after(at(after), zone)
        .expect("a next match")
}

fn utc(expression: &str, after: &str) -> String {
    next(expression, after, &TimeZone::UTC).to_string()
}

fn new_york() -> TimeZone {
    TimeZone::get("America/New_York").expect("the America/New_York zone")
}

#[test]
fn cron_expressions_refuse_what_cannot_be_read() {
    for (expression, reason) in [
        ("0 3 * *", "five fields"),
        ("0 3 * * * *", "five fields"),
        ("60 * * * *", "outside 0-59"),
        ("* 24 * * *", "outside 0-23"),
        ("* * 0 * *", "outside 1-31"),
        ("* * * 13 *", "outside 1-12"),
        ("* * * * 8", "outside 0-7"),
        ("5-1 * * * *", "runs backwards"),
        ("*/0 * * * *", "the step"),
        ("*/x * * * *", "the step"),
        ("a * * * *", "not a valid minute"),
        ("1,,2 * * * *", "not a valid minute"),
        ("0 0 30 2 *", "no next match"),
        ("0 0 31 4,6,9,11 *", "no next match"),
    ] {
        let error = CronExpr::parse(expression).expect_err(expression);
        assert!(error.contains(reason), "{expression}: {error}");
    }
}

#[test]
fn a_restricted_day_of_week_makes_february_30th_reachable() {
    // Vixie OR: the 30th of February never comes, but Mondays in February do.
    assert_eq!(
        utc("0 0 30 2 1", "2026-10-22T00:00:00Z"),
        "2027-02-01T00:00:00Z"
    );
}

#[test]
fn next_matches_cross_hours_days_and_month_ends() {
    assert_eq!(
        utc("0 3 * * *", "2026-10-22T18:00:00Z"),
        "2026-10-23T03:00:00Z"
    );
    // Strictly after: a time that matches is not its own next match.
    assert_eq!(
        utc("0 3 * * *", "2026-10-23T03:00:00Z"),
        "2026-10-24T03:00:00Z"
    );
    assert_eq!(
        utc("0 3 * * *", "2026-10-23T02:59:59Z"),
        "2026-10-23T03:00:00Z"
    );
    assert_eq!(
        utc("*/15 * * * *", "2026-10-22T10:07:30Z"),
        "2026-10-22T10:15:00Z"
    );
    assert_eq!(
        utc("5/20 * * * *", "2026-10-22T10:25:00Z"),
        "2026-10-22T10:45:00Z"
    );
    assert_eq!(
        utc("5/20 * * * *", "2026-10-22T10:45:00Z"),
        "2026-10-22T11:05:00Z"
    );
    assert_eq!(
        utc("0 0 31 * *", "2026-04-15T00:00:00Z"),
        "2026-05-31T00:00:00Z"
    );
    assert_eq!(
        utc("59 23 31 12 *", "2026-12-31T23:58:00Z"),
        "2026-12-31T23:59:00Z"
    );
    assert_eq!(
        utc("0 0 1 1 *", "2026-12-31T23:59:00Z"),
        "2027-01-01T00:00:00Z"
    );
    assert_eq!(
        utc("0 0 29 2 *", "2026-03-01T00:00:00Z"),
        "2028-02-29T00:00:00Z"
    );
}

#[test]
fn day_of_month_and_day_of_week_combine_as_in_vixie_cron() {
    // 2026-10-08 is a Thursday. Both restricted: the 1st to 7th or any
    // Monday, so the next Monday, not November's first week.
    assert_eq!(
        utc("0 0 1-7 * 1", "2026-10-08T00:00:00Z"),
        "2026-10-12T00:00:00Z"
    );
    // The 13th or a Friday.
    assert_eq!(
        utc("0 0 13 * 5", "2026-10-01T00:00:00Z"),
        "2026-10-02T00:00:00Z"
    );
    // A day-of-week field that starts with `*` restricts with AND: the 1st
    // to 7th that is also a Sunday, Tuesday, Thursday, or Saturday.
    assert_eq!(
        utc("0 0 1-7 * */2", "2026-10-31T00:00:00Z"),
        "2026-11-01T00:00:00Z"
    );
    assert_eq!(
        utc("0 0 1-7 * */2", "2026-11-01T00:00:00Z"),
        "2026-11-03T00:00:00Z"
    );
    // An unrestricted day of month leaves the day of week alone.
    assert_eq!(
        utc("0 0 * * 5", "2026-10-02T00:00:00Z"),
        "2026-10-09T00:00:00Z"
    );
}

#[test]
fn names_and_sunday_as_seven_read_as_numbers() {
    // 2026-10-24 is a Saturday.
    assert_eq!(
        utc("0 9 * * mon-fri", "2026-10-24T00:00:00Z"),
        "2026-10-26T09:00:00Z"
    );
    assert_eq!(
        utc("0 9 * * 7", "2026-10-24T00:00:00Z"),
        "2026-10-25T09:00:00Z"
    );
    assert_eq!(
        utc("0 9 * * SUN", "2026-10-24T00:00:00Z"),
        "2026-10-25T09:00:00Z"
    );
    assert_eq!(
        utc("0 9 1 Jan *", "2026-10-24T00:00:00Z"),
        "2027-01-01T09:00:00Z"
    );
}

#[test]
fn a_local_time_the_clock_skips_fires_where_the_gap_lands() {
    // New York springs forward at 02:00 on 2026-03-08: 02:30 does not
    // exist and maps to 03:30 EDT.
    let zone = new_york();
    assert_eq!(
        next("30 2 * * *", "2026-03-08T05:00:00Z", &zone).to_string(),
        "2026-03-08T07:30:00Z"
    );
    assert_eq!(
        next("30 2 * * *", "2026-03-08T07:30:00Z", &zone).to_string(),
        "2026-03-09T06:30:00Z"
    );
}

#[test]
fn a_local_time_the_clock_repeats_fires_once() {
    // New York falls back at 02:00 on 2026-11-01: 01:30 happens at 05:30Z
    // (EDT) and again at 06:30Z (EST); the job fires at the first only.
    let zone = new_york();
    let first = next("30 1 * * *", "2026-11-01T04:00:00Z", &zone);
    assert_eq!(first.to_string(), "2026-11-01T05:30:00Z");
    assert_eq!(
        next("30 1 * * *", &first.to_string(), &zone).to_string(),
        "2026-11-02T06:30:00Z"
    );
}

#[test]
fn missed_fires_are_counted_between_two_times() {
    let expr = CronExpr::parse("0 * * * *").expect("hourly");
    let zone = TimeZone::UTC;
    assert_eq!(
        expr.count_between(
            at("2026-10-22T00:00:00Z"),
            at("2026-10-22T05:00:00Z"),
            &zone,
            100
        ),
        5
    );
    assert_eq!(
        expr.count_between(
            at("2026-10-22T00:00:00Z"),
            at("2026-10-22T05:00:00Z"),
            &zone,
            3
        ),
        3
    );
    assert_eq!(
        expr.count_between(
            at("2026-10-22T00:00:00Z"),
            at("2026-10-22T00:59:59Z"),
            &zone,
            100
        ),
        0
    );
}

#[test]
fn delays_take_whole_numbers_from_one_second_to_a_year() {
    for (text, seconds) in [
        ("1s", 1),
        ("10m", 600),
        ("2h", 7200),
        ("365d", 365 * 86_400),
    ] {
        assert_eq!(
            parse_delay(text).expect(text).get_seconds(),
            seconds,
            "{text}"
        );
    }
    for text in [
        "0s", "366d", "8761h", "1w", "", "s", "1.5h", "-1s", "+1s", " 1s", "1 s",
    ] {
        assert!(parse_delay(text).is_err(), "{text:?} should be refused");
    }
}

fn job(id: &str, schedule: StoredSchedule) -> StoredJob {
    StoredJob {
        id: CronJobId::new(id).expect("job id"),
        name: None,
        prompt: "Check the build.".into(),
        schedule,
        max_runs: CronMaxRuns::Unlimited,
        target: StoredTarget::Session(SessionId::new_v7()),
        delivery: Some(ProducerDeliveryMode::Queue),
        permission_mode: None,
        max_consecutive_failures: None,
        enabled: true,
        disabled_reason: None,
        created_by: CronActor::User,
        created_at: at("2026-10-22T00:00:00Z"),
        next_fire: Some(at("2026-10-23T03:00:00Z")),
        runs: 0,
        consecutive_failures: 0,
        last_fire: None,
        history: Vec::new(),
    }
}

#[test]
fn one_shot_schedules_fire_only_after_their_time() {
    let schedule = StoredSchedule::At(at("2026-10-22T12:00:00Z"));
    let zone = TimeZone::UTC;
    assert_eq!(
        schedule.next_after(at("2026-10-22T11:59:59Z"), &zone),
        Some(at("2026-10-22T12:00:00Z"))
    );
    assert_eq!(schedule.next_after(at("2026-10-22T12:00:00Z"), &zone), None);
    assert_eq!(
        schedule.count_between(
            at("2026-10-22T11:00:00Z"),
            at("2026-10-22T13:00:00Z"),
            &zone,
            9
        ),
        1
    );
}

#[test]
fn the_history_keeps_the_latest_fires() {
    let mut job = job("aaaaaaa1", StoredSchedule::Cron("0 3 * * *".into()));
    for day in 1..=5 {
        job.record(
            StoredFire::skipped(
                None,
                at(&format!("2026-10-0{day}T03:00:00Z")),
                StoredSkip::FiringDisabled,
            ),
            3,
        );
    }
    let kept = job
        .history
        .iter()
        .map(|fire| fire.fired_at.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        kept,
        [
            "2026-10-03T03:00:00Z",
            "2026-10-04T03:00:00Z",
            "2026-10-05T03:00:00Z"
        ]
    );
}

#[test]
fn failures_disable_a_job_at_its_limit() {
    let mut job = job("aaaaaaa2", StoredSchedule::Cron("0 3 * * *".into()));
    job.count_failure(3);
    job.count_failure(3);
    assert!(job.enabled);
    job.count_failure(3);
    assert!(!job.enabled);
    assert_eq!(
        job.disabled_reason,
        Some(StoredDisabled::Failed { failures: 3 })
    );
    assert_eq!(job.next_fire, None);

    // The job's own limit overrides the configured one.
    let mut job = job_with_limit(1);
    job.count_failure(5);
    assert!(!job.enabled);
    assert_eq!(
        job.disabled_reason,
        Some(StoredDisabled::Failed { failures: 1 })
    );
}

fn job_with_limit(limit: u32) -> StoredJob {
    let mut job = job("aaaaaaa3", StoredSchedule::Cron("0 3 * * *".into()));
    job.max_consecutive_failures = Some(limit);
    job
}

#[test]
fn only_an_enabled_job_with_a_next_fire_keeps_its_session_waiting() {
    let mut job = job("aaaaaaa4", StoredSchedule::Cron("0 3 * * *".into()));
    let session = job.target.session().expect("in-session job");
    assert!(job.waits_on(session));
    assert!(!job.waits_on(SessionId::new_v7()));
    job.disable(StoredDisabled::Disabled {
        by: CronActor::Agent(CronAgentActor {
            agent: AgentId::new("builder").expect("agent"),
            session,
        }),
    });
    assert!(!job.waits_on(session));
}

#[test]
fn the_store_round_trips_and_refuses_unknown_fields() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = CronStore::open(directory.path()).expect("no file yet");
    assert!(store.jobs().is_empty());
    let saved = job("aaaaaaa5", StoredSchedule::Cron("0 3 * * *".into()));
    let session = saved.target.session().expect("in-session job");
    let ((), changed) = store
        .mutate(|jobs| {
            jobs.push(saved.clone());
            Ok(())
        })
        .expect("written");
    assert_eq!(changed.sessions, [session]);
    let reopened = CronStore::open(directory.path()).expect("read back");
    assert_eq!(reopened.jobs(), std::slice::from_ref(&saved));
    assert!(reopened.waits_on(session));
    assert_eq!(reopened.earliest_fire(), Some(at("2026-10-23T03:00:00Z")));

    let path = directory.path().join(super::CRONS_FILE);
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("file")).expect("json");
    value["jobs"][0]["colour"] = serde_json::json!("blue");
    std::fs::write(&path, serde_json::to_vec(&value).expect("json")).expect("rewritten");
    assert!(CronStore::open(directory.path()).is_err());

    value["jobs"][0]
        .as_object_mut()
        .expect("job")
        .remove("colour");
    value["version"] = serde_json::json!(2);
    std::fs::write(&path, serde_json::to_vec(&value).expect("json")).expect("rewritten");
    assert!(CronStore::open(directory.path()).is_err());
}

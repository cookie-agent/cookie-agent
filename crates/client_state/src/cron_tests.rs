use cookie_agent_protocol::{
    CronJob, CronMaxRuns, CronSchedule, CronTarget, ProducerDeliveryMode, SemVer, SessionId,
};
use jiff::{Timestamp, tz::TimeZone};

use super::{
    CronDraft, cron_reading, daemon_has_crons, detail_lines, fire_outcome, job_title, last_outcome,
    parse_max_runs, parse_model_pin, parse_schedule, schedule_reading, state_label, target_label,
    until, update_changes_anything,
};

fn job(value: serde_json::Value) -> CronJob {
    let mut base = serde_json::json!({
        "id": "k3v9q2xa",
        "prompt": "Check for outdated dependencies and open a summary.",
        "schedule": {"cron": "0 3 * * *"},
        "max_runs": "unlimited",
        "target": {"session": "0190a5b2-0000-7000-8000-000000000001"},
        "delivery": "queue",
        "enabled": true,
        "created_by": "user",
        "created_at": "2026-10-22T18:00:00Z"
    });
    for (key, member) in value.as_object().cloned().unwrap_or_default() {
        base[key] = member;
    }
    serde_json::from_value(base).expect("job")
}

fn session(n: u8) -> SessionId {
    format!("0190a5b2-0000-7000-8000-00000000000{n}")
        .parse()
        .expect("session id")
}

#[test]
fn only_a_daemon_of_minor_fifteen_keeps_jobs() {
    assert!(!daemon_has_crons(None));
    assert!(!daemon_has_crons(Some(SemVer::new(1, 14, 0))));
    assert!(daemon_has_crons(Some(SemVer::new(1, 15, 0))));
    assert!(daemon_has_crons(Some(SemVer::new(1, 16, 2))));
    assert!(!daemon_has_crons(Some(SemVer::new(2, 15, 0))));
}

#[test]
fn readings_name_the_shapes_people_write() {
    let cases = [
        ("0 3 * * *", "every day at 03:00"),
        ("30 9,17 * * *", "every day at 09:30 and 17:30"),
        ("0 9 * * 1-5", "every Monday to Friday at 09:00"),
        (
            "0 9 * * mon,wed,fri",
            "every Monday, Wednesday and Friday at 09:00",
        ),
        ("0 9 * * 7", "every Sunday at 09:00"),
        ("*/15 * * * *", "every 15 minutes"),
        ("* * * * *", "every minute"),
        ("5 * * * *", "every hour at :05"),
        ("0 */6 * * *", "every 6 hours at :00"),
        ("0 8 1 * *", "on the 1st of every month at 08:00"),
        (
            "0 8 1,15 * *",
            "on the 1st and 15th of every month at 08:00",
        ),
        ("0 8 22 jan *", "on the 22nd of January at 08:00"),
        ("0 0 * 12 *", "every day in December at 00:00"),
        ("0 8 13 * 5", "on the 13th and every Friday at 08:00"),
        ("*/10 * * * 1", "every 10 minutes, every Monday"),
    ];
    for (expression, reading) in cases {
        assert_eq!(
            cron_reading(expression).as_deref(),
            Some(reading),
            "{expression}"
        );
    }
    assert_eq!(cron_reading("0 9-17 * * *"), None);
    assert_eq!(cron_reading("0 3 * *"), None);
}

#[test]
fn schedule_readings_add_the_run_limit_and_one_shot_times() {
    let utc = TimeZone::UTC;
    let cron = CronSchedule::Cron("0 3 * * *".into());
    assert_eq!(
        schedule_reading(&cron, CronMaxRuns::Unlimited, &utc),
        "every day at 03:00"
    );
    assert_eq!(
        schedule_reading(&cron, CronMaxRuns::Count(1), &utc),
        "every day at 03:00, once"
    );
    assert_eq!(
        schedule_reading(&cron, CronMaxRuns::Count(4), &utc),
        "every day at 03:00, 4 runs"
    );
    let odd = CronSchedule::Cron("0 9-17 * * *".into());
    assert_eq!(
        schedule_reading(&odd, CronMaxRuns::Unlimited, &utc),
        "cron 0 9-17 * * *"
    );
    let at = CronSchedule::At("2026-10-23T03:00:00Z".parse().expect("time"));
    assert_eq!(
        schedule_reading(&at, CronMaxRuns::Count(1), &utc),
        "once at 2026-10-23 03:00"
    );
    let plus_two = TimeZone::fixed(jiff::tz::offset(2));
    assert_eq!(
        schedule_reading(&at, CronMaxRuns::Count(1), &plus_two),
        "once at 2026-10-23 05:00"
    );
}

#[test]
fn a_job_without_a_name_shows_its_prompt_start() {
    assert_eq!(
        job_title(&job(serde_json::json!({"name": "Nightly"}))),
        "Nightly"
    );
    assert_eq!(
        job_title(&job(serde_json::json!({"prompt": "Short one"}))),
        "Short one"
    );
    assert_eq!(
        job_title(&job(
            serde_json::json!({"prompt": "First line\nsecond line"})
        )),
        "First line…"
    );
    let long = "a".repeat(60);
    assert_eq!(
        job_title(&job(serde_json::json!({ "prompt": long }))),
        format!("{}…", "a".repeat(48))
    );
}

#[test]
fn targets_name_their_session_by_title() {
    let target = CronTarget::Session(session(1));
    assert_eq!(target_label(&target, |_| None), "0190a5b2");
    assert_eq!(
        target_label(&target, |_| Some("Build fixes".into())),
        "Build fixes"
    );
    assert_eq!(
        target_label(&CronTarget::Subagent(session(1)), |_| None),
        "subagent 0190a5b2"
    );
    let new = job(serde_json::json!({"target": {"new": {"agent": "builder"}}}));
    assert_eq!(target_label(&new.target, |_| None), "new session · builder");
}

#[test]
fn details_link_the_target_and_the_creating_session() {
    let titles = |id: SessionId| (id == session(1)).then(|| "Build fixes".to_owned());
    let by_agent = job(serde_json::json!({
        "created_by": {"agent": {"agent": "builder", "session": session(2)}}
    }));
    let details = detail_lines(&by_agent, Timestamp::UNIX_EPOCH, &TimeZone::UTC, titles);
    let linked: Vec<_> = details
        .iter()
        .filter_map(|detail| Some((detail.label, detail.value.as_str(), detail.session?)))
        .collect();
    assert_eq!(
        linked,
        [
            ("target", "Build fixes", session(1)),
            ("created in", "0190a5b2", session(2)),
        ]
    );
    let runs = details.iter().find(|detail| detail.label == "runs");
    assert_eq!(
        runs.map(|detail| detail.value.as_str()),
        Some("0, no limit")
    );

    let by_user = detail_lines(
        &job(serde_json::json!({})),
        Timestamp::UNIX_EPOCH,
        &TimeZone::UTC,
        |_| None,
    );
    assert!(by_user.iter().all(|detail| detail.label != "created in"));
}

#[test]
fn states_and_outcomes_read_plainly() {
    assert_eq!(state_label(&job(serde_json::json!({}))), "enabled");
    let failed = job(serde_json::json!({
        "enabled": false,
        "disabled_reason": {"kind": "failed", "failures": 3}
    }));
    assert_eq!(state_label(&failed), "failed 3 times in a row");
    let by_agent = job(serde_json::json!({
        "enabled": false,
        "disabled_reason": {"kind": "disabled", "by": {"agent": {
            "agent": "builder", "session": "0190a5b2-0000-7000-8000-000000000001"}}}
    }));
    assert_eq!(state_label(&by_agent), "disabled by agent builder");
    let future = job(serde_json::json!({
        "enabled": false, "disabled_reason": {"kind": "paused_by_moon"}
    }));
    assert_eq!(state_label(&future), "disabled");

    let fired = job(serde_json::json!({"history": [
        {"scheduled_at": "2026-10-20T03:00:00Z", "fired_at": "2026-10-21T08:00:00Z",
         "outcome": {"kind": "skipped", "reason": "daemon_was_down"}, "missed": 3},
        {"fired_at": "2026-10-21T09:00:00Z", "run_now": true,
         "outcome": {"kind": "delivered"},
         "session_id": "0190a5b2-0000-7000-8000-000000000001",
         "run_end": {"status": "completed", "at": "2026-10-21T09:01:00Z"}},
        {"scheduled_at": "2026-10-22T03:00:00Z", "fired_at": "2026-10-22T03:00:00Z",
         "outcome": {"kind": "failed", "error": "no model is available"}},
        {"scheduled_at": "2026-10-23T03:00:00Z", "fired_at": "2026-10-23T03:00:00Z",
         "outcome": {"kind": "skipped", "reason": "a_reason_from_later"}}
    ]}));
    let outcomes: Vec<String> = fired.history.iter().map(fire_outcome).collect();
    assert_eq!(
        outcomes,
        [
            "skipped: daemon was down (3 missed)",
            "run now · delivered · completed",
            "failed: no model is available",
            "skipped: another reason",
        ]
    );
    assert_eq!(last_outcome(&fired), "skipped");
    assert_eq!(last_outcome(&job(serde_json::json!({}))), "never");
}

#[test]
fn until_counts_down_in_whole_units() {
    let now: Timestamp = "2026-10-23T00:00:00Z".parse().expect("time");
    let at = |offset: i64| Timestamp::from_second(now.as_second() + offset).expect("time");
    assert_eq!(until(at(-5), now), "now");
    assert_eq!(until(at(1), now), "in 1 second");
    assert_eq!(until(at(150), now), "in 2 minutes");
    assert_eq!(until(at(3 * 3600 + 5), now), "in 3 hours");
    assert_eq!(until(at(86_400), now), "in 1 day");
}

#[test]
fn typed_schedules_runs_and_model_pins_parse() {
    assert_eq!(
        parse_schedule("  0  3 * * * "),
        Ok(CronSchedule::Cron("0 3 * * *".into()))
    );
    assert_eq!(parse_schedule("in 20m"), Ok(CronSchedule::In("20m".into())));
    assert_eq!(
        parse_schedule("at 2026-10-23T03:00:00Z"),
        Ok(CronSchedule::At(
            "2026-10-23T03:00:00Z".parse().expect("time")
        ))
    );
    assert!(parse_schedule("in ").is_err());
    assert!(parse_schedule("at tomorrow").is_err());
    assert!(parse_schedule("0 3 * *").is_err());

    assert_eq!(parse_max_runs(""), Ok(None));
    assert_eq!(parse_max_runs("5"), Ok(Some(CronMaxRuns::Count(5))));
    assert_eq!(parse_max_runs("0"), Ok(Some(CronMaxRuns::Unlimited)));
    assert_eq!(parse_max_runs(" -3 "), Ok(Some(CronMaxRuns::Unlimited)));
    assert!(parse_max_runs("unlimited").is_err());
    assert!(parse_max_runs("4294967296").is_err());
    assert!(parse_max_runs("many").is_err());

    assert_eq!(parse_model_pin(""), Ok(None));
    let pin = parse_model_pin("deepseek/deepseek-flash[high]")
        .expect("pin")
        .expect("some");
    assert_eq!(pin.model.to_string(), "deepseek/deepseek-flash");
    assert_eq!(
        pin.variant.map(|variant| variant.to_string()).as_deref(),
        Some("high")
    );
    let base = parse_model_pin("openai/gpt-5[base]")
        .expect("pin")
        .expect("some");
    assert_eq!(base.variant, None);
    assert!(parse_model_pin("no-provider").is_err());
}

#[test]
fn a_session_form_creates_an_in_session_job() {
    let mut draft = CronDraft::for_session(session(1), true);
    draft.prompt = "Summarize the build".into();
    draft.schedule = "in 2h".into();
    draft.max_runs = "7".into();
    draft.delivery = ProducerDeliveryMode::Steer;
    let params = draft.create_params().expect("params");
    assert_eq!(params.target, CronTarget::Subagent(session(1)));
    assert_eq!(params.schedule, CronSchedule::In("2h".into()));
    assert_eq!(params.max_runs, None, "a one-shot sends no run limit");
    assert_eq!(params.delivery, Some(ProducerDeliveryMode::Steer));
    assert_eq!(params.permission_mode, None);
    assert_eq!(params.name, None);

    draft.prompt = "   ".into();
    assert_eq!(
        draft.create_params().map(|_| ()),
        Err("a job needs a prompt".into())
    );
}

#[test]
fn a_new_session_form_sends_its_mode_agent_and_pin() {
    let template = super::bare_template("builder".parse().expect("agent"));
    let mut draft = CronDraft::for_new_session(template);
    draft.name = " Nightly ".into();
    draft.prompt = "Check dependencies".into();
    draft.schedule = "0 3 * * *".into();
    draft.max_runs = "0".into();
    draft.agent = "reviewer".into();
    draft.model = "deepseek/deepseek-flash".into();
    draft.permission_mode = Some(cookie_agent_protocol::PermissionMode::Ask);
    let params = draft.create_params().expect("params");
    let CronTarget::New(template) = &params.target else {
        panic!("a new-session target");
    };
    assert_eq!(template.agent.as_str(), "reviewer");
    assert_eq!(
        template.model.as_ref().map(ToString::to_string).as_deref(),
        Some("deepseek/deepseek-flash[base]")
    );
    assert_eq!(params.name.as_deref(), Some("Nightly"));
    assert_eq!(params.max_runs, Some(CronMaxRuns::Unlimited));
    assert_eq!(params.delivery, None);
    assert_eq!(
        params.permission_mode,
        Some(cookie_agent_protocol::PermissionMode::Ask)
    );
}

#[test]
fn an_edit_sends_only_what_changed() {
    let utc = TimeZone::UTC;
    let existing = job(serde_json::json!({"name": "Nightly"}));
    let mut draft = CronDraft::of_job(&existing, &utc);
    assert_eq!(draft.schedule, "0 3 * * *");
    assert_eq!(draft.max_runs, "0");
    let unchanged = draft.update_params(&existing, &utc).expect("params");
    assert!(!update_changes_anything(&unchanged));

    draft.name = String::new();
    draft.max_runs = "2".into();
    draft.delivery = ProducerDeliveryMode::Steer;
    let update = draft.update_params(&existing, &utc).expect("params");
    assert_eq!(update.name, Some(None), "a cleared name is sent as null");
    assert_eq!(update.max_runs, Some(CronMaxRuns::Count(2)));
    assert_eq!(update.delivery, Some(ProducerDeliveryMode::Steer));
    assert_eq!(update.schedule, None);
    assert_eq!(update.prompt, None);
    assert_eq!(update.target, None);

    let one_shot = job(serde_json::json!({
        "schedule": {"at": "2026-10-23T03:00:00Z"}, "max_runs": 1
    }));
    let draft = CronDraft::of_job(&one_shot, &utc);
    assert_eq!(draft.schedule, "at 2026-10-23T03:00:00Z");
    assert_eq!(draft.max_runs, "");
    let update = draft.update_params(&one_shot, &utc).expect("params");
    assert!(!update_changes_anything(&update));
}

use cookie_agent_protocol::{CronJob, SessionId, SessionMeta, Webhook};
use jiff::Timestamp;

use super::{
    jobs_own_first, next_fire, next_fire_text, session_jobs, session_webhooks, short_until,
    webhooks_own_first,
};

const HERE: &str = "018f0000-0000-7000-8000-000000000001";
const ELSEWHERE: &str = "018f0000-0000-7000-8000-000000000002";

fn meta(value: serde_json::Value) -> SessionMeta {
    let revision = format!("sha256:{}", "a".repeat(64));
    let mut base = serde_json::json!({
        "session_id": HERE,
        "origin": {"type": "root"},
        "short_id": null,
        "cwd_identity": "/workspace",
        "creation_selection": {
            "agent": "primary",
            "model": {"model": "provider/model", "variant": null},
            "preset": null
        },
        "runtime_revision": revision,
        "catalog_revision": revision,
        "provider_state_revision": revision,
        "model_revision": revision,
        "agent_revision": revision,
        "recipe_registry_revision": revision,
        "manifest_revision": revision,
        "title": null,
        "title_updated_seq": 0,
        "last_event_seq": 1,
        "last_activity": "2026-10-21T12:00:00Z",
        "status": "idle",
        "skipped_events": []
    });
    for (key, member) in value.as_object().cloned().unwrap_or_default() {
        base[key] = member;
    }
    serde_json::from_value(base).expect("session metadata")
}

fn job(id: &str, value: serde_json::Value) -> CronJob {
    let mut base = serde_json::json!({
        "id": id,
        "prompt": "Summarize the day.",
        "schedule": {"cron": "0 9 * * *"},
        "max_runs": "unlimited",
        "target": {"session": HERE},
        "enabled": true,
        "created_by": "user",
        "created_at": "2026-10-22T18:00:00Z",
        "next_fire": "2026-11-14T09:00:00Z"
    });
    for (key, member) in value.as_object().cloned().unwrap_or_default() {
        base[key] = member;
    }
    serde_json::from_value(base).expect("job")
}

fn webhook(id: &str, value: serde_json::Value) -> Webhook {
    let mut base = serde_json::json!({
        "id": id,
        "url_token": "Xq3vT9aLk2M",
        "path": "/hooks/Xq3vT9aLk2M",
        "target": {"session": HERE},
        "enabled": true,
        "created_by": "user",
        "created_at": "2026-11-12T18:00:00Z"
    });
    for (key, member) in value.as_object().cloned().unwrap_or_default() {
        base[key] = member;
    }
    serde_json::from_value(base).expect("webhook")
}

fn agent_here() -> serde_json::Value {
    serde_json::json!({"agent": {"agent": "primary", "session": HERE}})
}

#[test]
fn jobs_fall_into_the_first_group_they_fit() {
    let jobs = [
        job("intoaaaa", serde_json::json!({})),
        job(
            "subagent",
            serde_json::json!({"target": {"subagent": HERE}}),
        ),
        job(
            "madehere",
            serde_json::json!({
                "target": {"new": {"agent": "primary"}},
                "created_by": agent_here(),
            }),
        ),
        // Made here and firing into it: it fires into it.
        job("bothhere", serde_json::json!({"created_by": agent_here()})),
        job(
            "started1",
            serde_json::json!({"target": {"new": {"agent": "primary"}}}),
        ),
        job(
            "unrelate",
            serde_json::json!({"target": {"session": ELSEWHERE}}),
        ),
    ];
    let session = meta(serde_json::json!({
        "cron": {"job_id": "started1", "fired_at": "2026-11-13T09:00:00Z"}
    }));
    let groups = session_jobs(&session, &jobs);
    let ids = |list: &[CronJob]| {
        list.iter()
            .map(|job| job.id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&groups.into), ["intoaaaa", "subagent", "bothhere"]);
    assert_eq!(ids(&groups.made_here), ["madehere"]);
    assert_eq!(groups.started_by.unwrap().id.to_string(), "started1");
    assert!(session_jobs(&meta(serde_json::json!({})), &[]).is_empty());
}

#[test]
fn webhooks_fall_into_the_same_groups() {
    let hooks = [
        webhook("intoaaaa", serde_json::json!({})),
        webhook(
            "madehere",
            serde_json::json!({
                "target": {"new": {"agent": "primary"}},
                "created_by": agent_here(),
            }),
        ),
        webhook(
            "started1",
            serde_json::json!({"target": {"new": {"agent": "primary"}}}),
        ),
        webhook(
            "unrelate",
            serde_json::json!({"target": {"session": ELSEWHERE}}),
        ),
    ];
    let session = meta(serde_json::json!({
        "webhook": {"webhook_id": "started1", "event_id": "e1"}
    }));
    let groups = session_webhooks(&session, &hooks);
    assert_eq!(groups.into.len(), 1);
    assert_eq!(groups.made_here[0].id.to_string(), "madehere");
    assert_eq!(groups.len(), 3);
    assert_eq!(groups.started_by.unwrap().id.to_string(), "started1");
}

#[test]
fn the_clock_takes_the_soonest_fire_and_says_queued() {
    let here: SessionId = HERE.parse().unwrap();
    let now: Timestamp = "2026-11-14T08:48:00Z".parse().unwrap();
    assert_eq!(next_fire(here, &[]), None);
    // Disabled, or into another session: no clock.
    let quiet = [
        job(
            "disabled",
            serde_json::json!({"enabled": false, "next_fire": null}),
        ),
        job(
            "elsewher",
            serde_json::json!({"target": {"session": ELSEWHERE}}),
        ),
    ];
    assert_eq!(next_fire(here, &quiet), None);

    let two = [
        job(
            "laterjob",
            serde_json::json!({"next_fire": "2026-11-14T11:00:00Z"}),
        ),
        job("soonerjb", serde_json::json!({})),
    ];
    let clock = next_fire(here, &two).unwrap();
    assert_eq!(clock.at, Some("2026-11-14T09:00:00Z".parse().unwrap()));
    assert_eq!(clock.jobs, 2);
    assert!(!clock.queued);
    assert_eq!(next_fire_text(&clock, now), "12m · 2");

    // A fire delivered without a run yet waits behind the turn.
    let queued = [job(
        "queuedjb",
        serde_json::json!({
            "history": [{"fired_at": "2026-11-14T08:45:00Z", "outcome": {"kind": "delivered"}}],
        }),
    )];
    let clock = next_fire(here, &queued).unwrap();
    assert!(clock.queued);
    assert_eq!(next_fire_text(&clock, now), "queued");
}

#[test]
fn a_short_until_steps_from_seconds_to_days() {
    let now: Timestamp = "2026-11-14T00:00:00Z".parse().unwrap();
    let at = |seconds: i64| {
        now.checked_add(jiff::SignedDuration::from_secs(seconds))
            .unwrap()
    };
    assert_eq!(short_until(at(-5), now), "now");
    assert_eq!(short_until(at(0), now), "now");
    assert_eq!(short_until(at(59), now), "59s");
    assert_eq!(short_until(at(60), now), "1m");
    assert_eq!(short_until(at(3599), now), "59m");
    assert_eq!(short_until(at(3600), now), "1h");
    assert_eq!(short_until(at(86_399), now), "23h");
    assert_eq!(short_until(at(86_400), now), "1d");
}

#[test]
fn a_sessions_own_come_first_in_their_order() {
    let jobs = vec![
        job(
            "otheraaa",
            serde_json::json!({"target": {"session": ELSEWHERE}}),
        ),
        job("ownaaaaa", serde_json::json!({})),
        job(
            "otherbbb",
            serde_json::json!({"target": {"session": ELSEWHERE}}),
        ),
        job(
            "ownbbbbb",
            serde_json::json!({"created_by": agent_here(), "target": {"new": {"agent": "primary"}}}),
        ),
    ];
    let ordered = jobs_own_first(&meta(serde_json::json!({})), jobs);
    let ids: Vec<String> = ordered.iter().map(|job| job.id.to_string()).collect();
    assert_eq!(ids, ["ownaaaaa", "ownbbbbb", "otheraaa", "otherbbb"]);
    let hooks = vec![
        webhook(
            "otheraaa",
            serde_json::json!({"target": {"session": ELSEWHERE}}),
        ),
        webhook("ownaaaaa", serde_json::json!({})),
    ];
    let ordered = webhooks_own_first(&meta(serde_json::json!({})), hooks);
    assert_eq!(ordered[0].id.to_string(), "ownaaaaa");
}

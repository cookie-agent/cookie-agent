use cookie_agent_protocol::{Webhook, WebhookValueSource, WebhookVerify, WebhookVerifyKind};

use super::{
    Condition, Preset, ThreadBy, Tone, conditions, conditions_text, counters, health, preset_of,
    row_tags, source_words, target_words, thread_by, thread_key_text, what_happens,
};
use crate::webhook::{parse_condition, parse_value_source, parse_verify};

fn webhook(value: serde_json::Value) -> Webhook {
    let mut base = serde_json::json!({
        "id": "w7k2m9qa",
        "url_token": "Xq3vT9aLk2M",
        "path": "/hooks/Xq3vT9aLk2M",
        "target": {"session": "0190a5b2-0000-7000-8000-000000000001"},
        "delivery": "queue",
        "enabled": true,
        "created_by": "user",
        "created_at": "2026-11-12T18:00:00Z"
    });
    for (key, member) in value.as_object().cloned().unwrap_or_default() {
        base[key] = member;
    }
    serde_json::from_value(base).expect("webhook")
}

fn event(at: &str, outcome: serde_json::Value, test: bool) -> serde_json::Value {
    serde_json::json!({
        "id": format!("e{}", at.len()),
        "received_at": at,
        "bytes": 2,
        "test": test,
        "outcome": outcome,
    })
}

#[test]
fn health_takes_the_newest_event_tests_included_and_the_last_refusal() {
    let quiet = health(&webhook(serde_json::json!({})));
    assert_eq!(quiet.tone, Tone::Quiet);
    assert_eq!(quiet.last_event, None);

    // A test event is the last event (W2).
    let tested = health(&webhook(serde_json::json!({"history": [
        event("2026-11-12T18:04:00Z", serde_json::json!({"kind": "delivered"}), true),
    ]})));
    assert_eq!(tested.tone, Tone::Good);
    assert!(tested.last_event.unwrap().1.contains("test"));

    // A rejection after the last event, with the check's reason (W3).
    let rejected = webhook(serde_json::json!({
        "verify": {"kind": "bearer"},
        "counts": {"rejected": 2, "last_rejected": "2026-11-12T18:10:00Z",
                   "rate_limited": 1, "last_rate_limited": "2026-11-12T18:06:00Z"},
        "history": [event("2026-11-12T18:04:00Z", serde_json::json!({"kind": "delivered"}), false)],
    }));
    let read = health(&rejected);
    assert_eq!(read.tone, Tone::Warn);
    let refusal = read.refusal.unwrap();
    assert_eq!((refusal.what, refusal.count), ("rejected", 2));
    assert_eq!(
        refusal.reason,
        super::rejection_reason(Some(&WebhookVerify::of_kind(WebhookVerifyKind::Bearer)))
    );

    // An event since the rejection: good again, the rejection still said.
    let later = webhook(serde_json::json!({
        "counts": {"rejected": 1, "last_rejected": "2026-11-12T18:01:00Z"},
        "history": [event("2026-11-12T18:04:00Z", serde_json::json!({"kind": "delivered"}), false)],
    }));
    let read = health(&later);
    assert_eq!(read.tone, Tone::Good);
    assert!(read.refusal.is_some());

    // A failed last event, or failures turning it off, is bad.
    let failed = webhook(serde_json::json!({"history": [
        event("2026-11-12T18:04:00Z", serde_json::json!({"kind": "failed", "error": "x"}), false),
    ]}));
    assert_eq!(health(&failed).tone, Tone::Bad);
    let off = webhook(serde_json::json!({
        "enabled": false,
        "disabled_reason": {"kind": "failed", "failures": 5},
    }));
    assert_eq!(health(&off).tone, Tone::Bad);
}

#[test]
fn tags_say_only_what_the_switch_cannot() {
    assert_eq!(
        row_tags(&webhook(serde_json::json!({"verify": {"kind": "github"}}))).len(),
        0
    );
    // Off by the user: the switch says it.
    let off = webhook(serde_json::json!({
        "verify": {"kind": "github"},
        "enabled": false,
        "disabled_reason": {"kind": "disabled", "by": "user"},
    }));
    assert!(row_tags(&off).is_empty());
    let failed = webhook(serde_json::json!({
        "enabled": false,
        "disabled_reason": {"kind": "failed", "failures": 5},
    }));
    let tags = row_tags(&failed);
    assert_eq!(tags.len(), 2);
    assert!(tags[0].contains('5'));
}

#[test]
fn counters_show_the_four_always_and_the_rest_once_they_happen() {
    let quiet = counters(&webhook(serde_json::json!({})));
    assert_eq!(quiet.len(), 4);
    assert!(
        quiet
            .iter()
            .all(|counter| counter.count == 0 && !counter.warn)
    );
    let busy = counters(&webhook(serde_json::json!({
        "counts": {"accepted": 3, "rejected": 1, "duplicate": 2, "backlog_full": 1},
    })));
    assert_eq!(busy.len(), 6);
    let warned: Vec<u16> = busy
        .iter()
        .filter(|counter| counter.warn)
        .map(|counter| counter.status)
        .collect();
    assert_eq!(warned, [401, 503]);
}

#[test]
fn a_target_reads_as_words() {
    let session = webhook(serde_json::json!({}));
    let words = target_words(&session, &[], |_| Some("Deploy watch".to_owned()));
    assert_eq!(words.session.unwrap().1, "Deploy watch");
    let threaded = webhook(serde_json::json!({"target": {"new": {
        "agent": "builder",
        "thread_key": {"json": "/issue/number"},
    }}}));
    let words = target_words(&threaded, &[], |_| None);
    assert!(words.session.is_none());
    assert!(words.rest.contains("builder"));
    assert!(words.rest.ends_with("issue.number"));
    assert_eq!(
        source_words(&WebhookValueSource::header("X-Thread")),
        "X-Thread header"
    );
    assert_eq!(source_words(&WebhookValueSource::json("/a~1b/c")), "a/b.c");
}

#[test]
fn what_happens_follows_the_target() {
    let into = what_happens(&webhook(serde_json::json!({})), |_| {
        Some("Deploy".to_owned())
    });
    assert_eq!(into.len(), 2);
    assert!(into[0].heading.contains("Deploy"));
    let new = webhook(serde_json::json!({
        "name": "Shop",
        "target": {"new": {
            "agent": "builder",
            "thread_key": {"json": "/issue/number"},
            "thread_prompt": "Triage it.",
            "title": "Shop issues",
        }},
        "max_concurrent": 3,
    }));
    let lines = what_happens(&new, |_| None);
    assert_eq!(lines.len(), 4);
    assert!(lines[0].detail.as_deref().unwrap().contains('3'));
    assert_eq!(lines[1].detail.as_deref(), Some("Triage it."));
    assert!(
        lines[2]
            .detail
            .as_deref()
            .unwrap()
            .starts_with("Shop issues · ")
    );
}

#[test]
fn every_preset_fills_fields_the_form_reads() {
    for preset in Preset::ALL {
        let fields = preset.fields();
        parse_verify(&fields.verify).expect("the check reads");
        for line in fields.filter.lines() {
            parse_condition(line).expect("the filter reads");
        }
        if !fields.thread_key.is_empty() {
            assert!(preset.starts_sessions());
            parse_value_source(&fields.thread_key).expect("the thread key reads");
        }
        crate::cron::parse_session_title(&fields.title).expect("the title reads");
        // A webhook made from the preset is shown as it.
        let verify = parse_verify(&fields.verify).unwrap();
        assert_eq!(preset_of(verify.as_ref()), preset);
    }
}

#[test]
fn one_session_per_reads_the_thread_key_and_writes_it_back() {
    for text in ["json:/issue/number", "header:X-Thread", ""] {
        let (by, value) = thread_by(text);
        assert_eq!(thread_key_text(by, &value), text);
    }
    assert_eq!(thread_by("/issue").0, ThreadBy::Json);
    assert_eq!(thread_by("X-Thread").0, ThreadBy::Header);
}

#[test]
fn a_filter_reads_as_conditions_and_back() {
    let text = "header:X-GitHub-Event=issues,issue_comment\njson:/action=opened";
    let read = conditions(text).unwrap();
    assert_eq!(read.len(), 2);
    assert!(read[0].header && !read[1].header);
    assert_eq!(read[0].values, ["issues", "issue_comment"]);
    assert_eq!(conditions_text(&read), text);
    // An empty row is left out; a half-filled one is kept for the daemon's
    // check to refuse.
    let mut rows = read;
    rows.push(Condition {
        header: true,
        name: String::new(),
        values: Vec::new(),
    });
    rows.push(Condition {
        header: true,
        name: "X-Event".to_owned(),
        values: Vec::new(),
    });
    let written = conditions_text(&rows);
    assert_eq!(written.lines().count(), 3);
    assert!(conditions(&written).is_err());
    assert!(conditions("not a condition").is_err());
}

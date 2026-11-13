use cookie_agent_protocol::{
    EventOrigin, PermissionMode, ProducerDeliveryMode, SemVer, SessionId, ThreadOwnerRef, Webhook,
    WebhookCounts, WebhookSignatureEncoding, WebhookTarget, WebhookTemplate, WebhookValueSource,
    WebhookVerifyKind, thread_prompt_origin,
};
use jiff::tz::TimeZone;

use super::{
    OutcomeKind, WebhookDraft, condition_text, counts, daemon_has_webhooks, detail_lines,
    last_outcome, outcome_kind, parse_condition, parse_limit, parse_value_source, parse_verify,
    thread_prompt_label, update_changes_anything, url_looks_private, value_source_text,
    verify_text, webhook_title, webhook_url,
};

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

fn new_session_webhook() -> Webhook {
    webhook(serde_json::json!({
        "name": "GitHub issues",
        "prompt": "A new event on the issue.",
        "target": {"new": {
            "agent": "builder",
            "thread_key": {"json": "/issue/number"},
            "thread_prompt": "You look after one issue."
        }},
        "verify": {"kind": "github"},
        "filter": [
            {"header": "X-GitHub-Event", "one_of": ["issues", "issue_comment"]},
            {"json": "/action", "one_of": ["opened"]}
        ],
        "headers": ["X-GitHub-Event"],
        "max_per_hour": 60,
        "max_concurrent": 2,
        "permission_mode": "auto_approve_n"
    }))
}

fn session(n: u8) -> SessionId {
    format!("0190a5b2-0000-7000-8000-00000000000{n}")
        .parse()
        .expect("session id")
}

#[test]
fn only_a_daemon_of_minor_ninety_six_keeps_webhooks() {
    assert!(!daemon_has_webhooks(None));
    assert!(!daemon_has_webhooks(Some(SemVer::new(1, 95, 0))));
    assert!(daemon_has_webhooks(Some(SemVer::new(1, 96, 0))));
}

#[test]
fn a_url_is_the_path_below_the_public_base_else_the_page() {
    let path = "/hooks/Xq3vT9aLk2M";
    assert_eq!(
        webhook_url(
            Some("https://hooks.example.com/"),
            "http://127.0.0.1:7419/",
            path
        ),
        "https://hooks.example.com/hooks/Xq3vT9aLk2M"
    );
    assert_eq!(
        webhook_url(None, "http://127.0.0.1:7419/", path),
        "http://127.0.0.1:7419/hooks/Xq3vT9aLk2M"
    );
    assert!(url_looks_private(None, "http://127.0.0.1:7419/"));
    assert!(!url_looks_private(
        Some("https://hooks.example.com"),
        "http://127.0.0.1:7419/"
    ));
    // A configured `.ts.net` base may be a Funnel; the page's is not.
    assert!(!url_looks_private(Some("https://box.tail1.ts.net"), "x"));
    assert!(url_looks_private(None, "https://box.tail1.ts.net/"));
}

#[test]
fn a_title_is_the_name_else_the_id() {
    assert!(webhook_title(&new_session_webhook()).contains("GitHub issues"));
    let unnamed = webhook(serde_json::json!({"name": "  "}));
    assert!(webhook_title(&unnamed).contains("w7k2m9qa"));
}

#[test]
fn value_sources_round_trip_through_their_text() {
    for source in [
        WebhookValueSource::json("/issue/number"),
        WebhookValueSource::header("X-GitHub-Event"),
    ] {
        assert_eq!(parse_value_source(&value_source_text(&source)), Ok(source));
    }
    for refused in ["issue/number", "json:issue", "header:", "header:A B", "/x"] {
        assert!(parse_value_source(refused).is_err(), "{refused}");
    }
}

#[test]
fn conditions_round_trip_and_need_values() {
    let condition = parse_condition("header:X-GitHub-Event = issues, pull_request").unwrap();
    assert_eq!(condition.header.as_deref(), Some("X-GitHub-Event"));
    assert_eq!(condition.one_of, ["issues", "pull_request"]);
    assert_eq!(parse_condition(&condition_text(&condition)), Ok(condition));
    // A value may hold `=` after the first.
    let condition = parse_condition("json:/ref=refs/heads/main").unwrap();
    assert_eq!(condition.one_of, ["refs/heads/main"]);
    assert!(parse_condition("json:/action=").is_err());
    assert!(parse_condition("json:/action").is_err());
}

#[test]
fn checks_round_trip_without_secrets() {
    assert_eq!(parse_verify("  "), Ok(None));
    let hmac = parse_verify("hmac_sha256 header=X-Sig encoding=base64 prefix=v1=")
        .unwrap()
        .unwrap();
    assert_eq!(hmac.kind, WebhookVerifyKind::HmacSha256);
    assert_eq!(hmac.header.as_deref(), Some("X-Sig"));
    assert_eq!(hmac.encoding, Some(WebhookSignatureEncoding::Base64));
    assert_eq!(hmac.prefix.as_deref(), Some("v1="));
    assert_eq!(parse_verify(&verify_text(Some(&hmac))), Ok(Some(hmac)));
    for kind in ["github", "standard", "bearer"] {
        let verify = parse_verify(kind).unwrap().unwrap();
        assert_eq!(verify_text(Some(&verify)), kind);
    }
    for refused in [
        "sha1",
        "hmac_sha256",
        "hmac_sha256 header=X encoding=hex2",
        "github header=X",
        "hmac_sha256 header=X colour=red",
    ] {
        assert!(parse_verify(refused).is_err(), "{refused}");
    }
}

#[test]
fn limits_are_blank_for_the_default_and_whole_numbers_otherwise() {
    assert_eq!(parse_limit("", "x"), Ok(None));
    assert_eq!(parse_limit(" 0 ", "x"), Ok(Some(0)));
    assert_eq!(parse_limit("-1", "x"), Ok(Some(-1)));
    assert_eq!(parse_limit("60", "x"), Ok(Some(60)));
    assert!(parse_limit("1.5", "x").is_err());
}

#[test]
fn counts_list_only_what_happened_in_a_fixed_order() {
    let mut webhook_counts = WebhookCounts::default();
    assert!(counts(&webhook_counts).is_empty());
    webhook_counts.filtered = 3;
    webhook_counts.accepted = 12;
    assert_eq!(counts(&webhook_counts), [("accepted", 12), ("filtered", 3)]);
}

#[test]
fn outcomes_follow_the_event_and_its_run() {
    let with_history =
        |history: serde_json::Value| webhook(serde_json::json!({"history": history}));
    assert_eq!(last_outcome(&with_history(serde_json::json!([]))), None);
    let event = |outcome: serde_json::Value, run_end: Option<&str>| {
        let mut event = serde_json::json!({
            "id": "e3j8x0pq",
            "received_at": "2026-11-12T18:04:11Z",
            "bytes": 10,
            "outcome": outcome,
        });
        if let Some(status) = run_end {
            event["run_id"] = serde_json::json!("0190a5b2-0000-7000-8000-0000000000aa");
            event["run_end"] = serde_json::json!({"status": status, "at": "2026-11-12T18:05:00Z"});
        }
        event
    };
    for (outcome, run_end, kind) in [
        (
            serde_json::json!({"kind": "waiting"}),
            None,
            OutcomeKind::Waiting,
        ),
        (
            serde_json::json!({"kind": "pending"}),
            None,
            OutcomeKind::Pending,
        ),
        (
            serde_json::json!({"kind": "delivered"}),
            None,
            OutcomeKind::Delivered,
        ),
        (
            serde_json::json!({"kind": "delivered"}),
            Some("completed"),
            OutcomeKind::Completed,
        ),
        (
            serde_json::json!({"kind": "delivered"}),
            Some("failed"),
            OutcomeKind::Failed,
        ),
        (
            serde_json::json!({"kind": "failed", "error": "agent missing"}),
            None,
            OutcomeKind::Failed,
        ),
        (
            serde_json::json!({"kind": "forwarded"}),
            None,
            OutcomeKind::Unknown,
        ),
    ] {
        let hook = with_history(serde_json::json!([event(outcome, run_end)]));
        assert_eq!(outcome_kind(&hook.history[0]), kind);
        assert_eq!(last_outcome(&hook), Some(kind));
    }
}

#[test]
fn a_thread_prompt_label_names_its_owner_or_its_id() {
    let owner = ThreadOwnerRef::Webhook(
        cookie_agent_protocol::WebhookId::new("w7k2m9qa".to_owned()).unwrap(),
    );
    let origin = thread_prompt_origin(&owner);
    let named = thread_prompt_label(&origin, |_| Some("GitHub issues".to_owned())).unwrap();
    assert!(named.contains("GitHub issues"));
    let unnamed = thread_prompt_label(&origin, |_| None).unwrap();
    assert!(unnamed.contains("w7k2m9qa"));
    let cron = thread_prompt_origin(&ThreadOwnerRef::Cron(
        cookie_agent_protocol::CronJobId::new("k3v9q2xa".to_owned()).unwrap(),
    ));
    let asked = std::cell::RefCell::new(None);
    thread_prompt_label(&cron, |owner| {
        *asked.borrow_mut() = Some(owner.clone());
        None
    })
    .unwrap();
    assert!(matches!(asked.into_inner(), Some(ThreadOwnerRef::Cron(_))));
    let other = EventOrigin::new("engine:goal".to_owned()).unwrap();
    assert_eq!(thread_prompt_label(&other, |_| None), None);
}

#[test]
fn details_name_the_target_session_and_new_session_fields() {
    let zone = TimeZone::UTC;
    let in_session = webhook(serde_json::json!({}));
    let lines = detail_lines(&in_session, "u", &zone, &[], |_| None);
    let target = lines.iter().find(|line| line.label == "target").unwrap();
    assert_eq!(target.session, Some(session(1)));
    assert!(!lines.iter().any(|line| line.label == "thread key"));

    let threaded = new_session_webhook();
    let lines = detail_lines(&threaded, "u", &zone, &[], |_| None);
    let labels: Vec<&str> = lines.iter().map(|line| line.label).collect();
    for label in [
        "thread key",
        "thread prompt",
        "agent",
        "per hour",
        "at once",
    ] {
        assert!(labels.contains(&label), "{label} in {labels:?}");
    }
    assert_eq!(labels.iter().filter(|label| **label == "filter").count(), 2);
}

#[test]
fn a_webhook_form_round_trips_and_changes_nothing() {
    let hook = new_session_webhook();
    let draft = WebhookDraft::of_webhook(&hook);
    assert!(draft.threaded() && draft.takes_delivery());
    let update = draft.update_params(&hook).unwrap();
    assert!(!update_changes_anything(&update), "{update:?}");

    // Through the text form too.
    let mut reread = draft.clone();
    reread
        .apply_form(&draft.form_text("a new session"))
        .unwrap();
    assert_eq!(reread, draft);
    assert!(!update_changes_anything(
        &reread.update_params(&hook).unwrap()
    ));

    let in_session = webhook(serde_json::json!({"prompt": "Look at it."}));
    let draft = WebhookDraft::of_webhook(&in_session);
    let mut reread = draft.clone();
    reread.apply_form(&draft.form_text("this session")).unwrap();
    assert_eq!(reread, draft);
    assert!(!update_changes_anything(
        &draft.update_params(&in_session).unwrap()
    ));
}

#[test]
fn an_update_sends_only_what_changed() {
    let hook = new_session_webhook();
    let mut draft = WebhookDraft::of_webhook(&hook);
    draft.name = "Issues".to_owned();
    draft.filter.push_str("\njson:/sender/type=User");
    draft.max_per_hour.clear();
    draft.thread_key = "header:X-Thread".to_owned();
    let update = draft.update_params(&hook).unwrap();
    assert_eq!(update.name, Some(Some("Issues".to_owned())));
    assert_eq!(update.filter.as_ref().map(Vec::len), Some(3));
    assert_eq!(update.max_per_hour, Some(None));
    let Some(WebhookTarget::New(template)) = &update.target else {
        panic!("the thread key changes the target: {update:?}");
    };
    assert_eq!(
        template.thread_key,
        Some(WebhookValueSource::header("X-Thread"))
    );
    assert_eq!(
        template.thread_prompt.as_deref(),
        Some("You look after one issue.")
    );
    assert_eq!(update.prompt, None);
    assert_eq!(update.verify, None);
    assert_eq!(update.headers, None);
    assert_eq!(update.max_concurrent, None);
    assert_eq!(update.permission_mode, None);
    assert_eq!(update.delivery, None);

    // A secret typed in is sent with the check, which is otherwise unchanged.
    let mut draft = WebhookDraft::of_webhook(&hook);
    draft.secret = "s3cret".to_owned();
    let update = draft.update_params(&hook).unwrap();
    let verify = update.verify.flatten().unwrap();
    assert_eq!(verify.kind, WebhookVerifyKind::Github);
    assert_eq!(verify.secret.as_deref(), Some("s3cret"));

    // Clearing the check removes it.
    let mut draft = WebhookDraft::of_webhook(&hook);
    draft.verify.clear();
    assert_eq!(draft.update_params(&hook).unwrap().verify, Some(None));
}

#[test]
fn a_create_takes_the_forms_fields_by_target() {
    let mut draft = WebhookDraft::for_session(session(2), true);
    draft.prompt = "Check the build.\n".to_owned();
    draft.delivery = ProducerDeliveryMode::Steer;
    draft.verify = "bearer".to_owned();
    draft.filter = "header:X-Event=failed\n\n".to_owned();
    draft.headers = "X-Event, X-Build".to_owned();
    draft.permission_mode = Some(PermissionMode::Yolo);
    let params = draft.create_params().unwrap();
    assert_eq!(params.target, WebhookTarget::Subagent(session(2)));
    assert_eq!(params.prompt.as_deref(), Some("Check the build."));
    assert_eq!(params.delivery, Some(ProducerDeliveryMode::Steer));
    assert_eq!(params.filter.len(), 1);
    assert_eq!(params.headers, ["X-Event", "X-Build"]);
    // A mode is only for new sessions.
    assert_eq!(params.permission_mode, None);
    draft.thread_key = "json:/n".to_owned();
    assert!(draft.create_params().is_err());
    draft.thread_key.clear();
    draft.max_concurrent = "2".to_owned();
    assert!(draft.create_params().is_err());
    draft.max_concurrent.clear();
    draft.secret = "x".to_owned();
    draft.verify.clear();
    assert!(draft.create_params().is_err());

    let mut draft =
        WebhookDraft::for_new_session(WebhookTemplate::for_agent("builder".parse().unwrap()));
    draft.agent = "reviewer".to_owned();
    draft.model = "openai/gpt[high]".to_owned();
    draft.permission_mode = Some(PermissionMode::Ask);
    let params = draft.create_params().unwrap();
    let WebhookTarget::New(template) = &params.target else {
        panic!("a new-session target: {params:?}");
    };
    assert_eq!(template.agent.as_str(), "reviewer");
    assert!(template.model.is_some());
    // Without a thread key, events start sessions and take no delivery.
    assert_eq!(params.delivery, None);
    assert_eq!(params.permission_mode, Some(PermissionMode::Ask));
    draft.thread_key = "json:/issue/number".to_owned();
    assert_eq!(
        draft.create_params().unwrap().delivery,
        Some(ProducerDeliveryMode::Queue)
    );
}

#[test]
fn a_session_title_is_checked_and_round_trips_through_the_text_form() {
    let mut draft =
        WebhookDraft::for_new_session(WebhookTemplate::for_agent("builder".parse().unwrap()));
    draft.title = "  Shop issues ".to_owned();
    let params = draft.create_params().unwrap();
    let WebhookTarget::New(template) = &params.target else {
        panic!("a new-session target: {params:?}");
    };
    assert_eq!(template.title.as_deref(), Some("Shop issues"));
    assert!(!template.title_auto);

    draft.title_auto = true;
    let mut reread = draft.clone();
    reread
        .apply_form(&draft.form_text("a new session"))
        .unwrap();
    assert_eq!(reread.title.trim(), "Shop issues");
    assert!(reread.title_auto);

    for bad in ["x".repeat(97), "two\u{7}bells".to_owned()] {
        draft.title = bad;
        assert!(draft.create_params().is_err(), "{:?}", draft.title);
    }
    assert!(reread.apply_form("title_auto: maybe\n---\n").is_err());

    let mut in_session = WebhookDraft::for_session(session(1), false);
    in_session.title = "Issues".to_owned();
    assert!(in_session.create_params().is_err());
}

#[test]
fn a_text_form_refuses_what_its_target_cannot_take() {
    let mut in_session = WebhookDraft::for_session(session(1), false);
    let refused = [
        "name: x\nagent: builder\n---\n",
        "name: x\ntitle: Issues\n---\n",
        "name: x\ntitle_auto: yes\n---\n",
        "name: x\n---\nprompt\n--- thread prompt\nonce\n",
        "name: x\nname: y\n---\n",
        "colour: red\n---\n",
        "name: x\n",
        "check: hmac_sha256\n---\n",
        "filter: json:/a\n---\n",
        "delivery: later\n---\n",
    ];
    for text in refused {
        let before = in_session.clone();
        assert!(in_session.apply_form(text).is_err(), "{text:?}");
        assert_eq!(in_session, before, "a refused form changes nothing");
    }
    in_session
        .apply_form(
            "name: CI\nfilter: header:X-Event=failed\nfilter: json:/branch=main\n---\nLook.\n",
        )
        .unwrap();
    assert_eq!(in_session.name, "CI");
    assert_eq!(in_session.filter.lines().count(), 2);
    assert_eq!(in_session.prompt, "Look.");

    let mut new =
        WebhookDraft::for_new_session(WebhookTemplate::for_agent("builder".parse().unwrap()));
    new.apply_form(
        "permission_mode: ask\nthread_key: json:/n\n---\nEvent.\n--- thread prompt\nOnce.\n",
    )
    .unwrap();
    assert_eq!(new.permission_mode, Some(PermissionMode::Ask));
    assert_eq!(new.thread_prompt, "Once.");
    assert!(new.apply_form("permission_mode: sometimes\n---\n").is_err());
}

//! Webhooks (`docs/site/specs/webhooks.md`): receiving requests, the
//! checks in order, delivery into existing sessions and new ones, threads,
//! the backlog, restarts, failures, and deletion.

use std::time::Duration;

use cookie_agent_protocol::{
    CronActor, EventPayload, ProducerDeliveryMode, ProjectCreateParams, ProjectDeleteParams,
    RunSelection, SessionId, SessionStatus, ThreadOwnerRef, WebhookCondition, WebhookCreateParams,
    WebhookId, WebhookTarget, WebhookTemplate, WebhookTestParams, WebhookUpdateParams,
    WebhookValueSource, WebhookVerify, WebhookVerifyKind, thread_prompt_origin,
};

use super::support::*;
use crate::{
    Engine, WebhookRequest,
    webhooks::store::{StoredEventOutcome, StoredWebhook},
};

const THREAD_PROMPT: &str = "You look after one issue.";
const PROMPT: &str = "Act on the event.";

fn request(body: &str, headers: &[(&str, &str)]) -> WebhookRequest {
    let mut all = vec![("Content-Type".to_owned(), "application/json".to_owned())];
    all.extend(
        headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    WebhookRequest {
        method: "POST".into(),
        headers: all,
        body: body.as_bytes().to_vec(),
    }
}

fn github_signature(secret: &str, body: &str) -> String {
    let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = aws_lc_rs::hmac::sign(&key, body.as_bytes());
    format!(
        "sha256={}",
        tag.as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

async fn await_webhook(
    engine: &Engine,
    id: &WebhookId,
    description: &str,
    predicate: impl Fn(&StoredWebhook) -> bool,
) -> StoredWebhook {
    with_watchdog(description, async {
        loop {
            if let Some(webhook) = engine.inner.webhooks.webhook(id)
                && predicate(&webhook)
            {
                return webhook;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

fn outcome_of(webhook: &StoredWebhook, event: &str) -> StoredEventOutcome {
    webhook.event(event).expect("the event").outcome.clone()
}

fn event_id(reply: &crate::WebhookReply) -> String {
    reply.body["event"]
        .as_str()
        .expect("an event ID")
        .to_owned()
}

/// The webhook's messages accepted in `session`.
fn webhook_messages(engine: &Engine, session: SessionId, id: &WebhookId) -> usize {
    let owner = crate::runtime::webhook::deliver::webhook_owner(id);
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

/// The thread prompts webhook `id` submitted in `session`.
fn thread_prompts(engine: &Engine, session: SessionId, id: &WebhookId) -> usize {
    let origin = thread_prompt_origin(&ThreadOwnerRef::Webhook(id.clone()));
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

fn answer_event(responses: &tokio::sync::mpsc::UnboundedSender<MatchedScriptedResponse>) {
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "<webhook_event webhook=",
            scripted_text_body("handled"),
        ))
        .unwrap();
}

fn template(selection: &RunSelection) -> WebhookTemplate {
    WebhookTemplate {
        thread_key: Some(WebhookValueSource::json("/issue")),
        thread_prompt: Some(THREAD_PROMPT.into()),
        ..WebhookTemplate::for_agent(selection.agent.clone())
    }
}

#[tokio::test]
async fn an_event_reaches_its_session_marked_as_a_webhook_event() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let mut create = WebhookCreateParams::new(WebhookTarget::Session(session));
    create.name = Some("CI".into());
    create.prompt = Some(PROMPT.into());
    let created = engine.create_webhook(create).await.expect("created");
    let webhook = created.webhook;
    assert_eq!(webhook.delivery, Some(ProducerDeliveryMode::Queue));
    assert!(webhook.path.ends_with(&webhook.url_token));

    for (index, build) in ["red", "green"].iter().enumerate() {
        answer_event(&responses);
        let reply = engine
            .receive_webhook(
                &webhook.url_token,
                request(
                    &format!(r#"{{"build":"{build}"}}"#),
                    &[
                        ("X-CI-Event", "build"),
                        ("Authorization", "Bearer hidden"),
                        ("X-Other", "not shown"),
                    ],
                ),
            )
            .await;
        assert_eq!(reply.status, 202, "{reply:?}");
        let event = event_id(&reply);
        let done = await_webhook(engine, &webhook.id, "the event's run", |hook| {
            hook.event(&event)
                .is_some_and(|event| event.run_end.is_some())
        })
        .await;
        let stored = done.event(&event).unwrap();
        assert_eq!(stored.outcome, StoredEventOutcome::Delivered);
        assert_eq!(stored.session_id, Some(session));
        assert_eq!(
            stored.run_end.as_ref().unwrap().status,
            SessionStatus::Completed
        );
        assert_eq!(done.counts.accepted, index as u64 + 1);
    }
    assert_eq!(webhook_messages(engine, session, &webhook.id), 2);

    let requests = captured.await.expect("model requests");
    let marked = format!(
        "<webhook_event webhook=\\\"{}\\\" name=\\\"CI\\\"",
        webhook.id
    );
    assert!(requests[0].contains(&marked), "{}", requests[0]);
    assert!(requests[0].contains(PROMPT));
    assert!(requests[0].contains("X-CI-Event: build"));
    assert!(!requests[0].contains("hidden"));
    assert!(!requests[0].contains("not shown"));
    assert!(requests[1].contains("green"));
}

#[tokio::test]
async fn each_check_answers_in_its_order() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let mut create = WebhookCreateParams::new(WebhookTarget::Session(session));
    create.verify = Some(WebhookVerify {
        secret: Some("s3cret".into()),
        ..WebhookVerify::of_kind(WebhookVerifyKind::Github)
    });
    create.filter = vec![WebhookCondition {
        header: Some("X-GitHub-Event".into()),
        json: None,
        one_of: vec!["issues".into()],
    }];
    create.max_per_hour = Some(2);
    let created = engine.create_webhook(create).await.expect("created");
    assert_eq!(created.secret.as_deref(), Some("s3cret"));
    let webhook = created.webhook;
    assert_eq!(webhook.verify.as_ref().unwrap().secret, None);
    let token = webhook.url_token.clone();
    let signed = |body: &str, event: &str, delivery: &str| {
        let signature = github_signature("s3cret", body);
        request(
            body,
            &[
                ("X-GitHub-Event", event),
                ("X-GitHub-Delivery", delivery),
                ("X-Hub-Signature-256", &signature),
            ],
        )
    };

    assert_eq!(
        engine
            .receive_webhook("AAAAAAAAAAA", request("{}", &[]))
            .await
            .status,
        404
    );
    assert_eq!(
        engine
            .receive_webhook(&token, request("{}", &[]))
            .await
            .status,
        401
    );
    let accepted = engine
        .receive_webhook(&token, signed("{}", "issues", "d1"))
        .await;
    assert_eq!(accepted.status, 202);
    assert_eq!(
        engine
            .receive_webhook(&token, signed("{}", "issues", "d1"))
            .await
            .status,
        200
    );
    assert_eq!(
        engine
            .receive_webhook(&token, signed("{}", "push", "d2"))
            .await
            .status,
        204
    );
    assert_eq!(
        engine
            .receive_webhook(&token, signed("{}", "issues", "d3"))
            .await
            .status,
        202
    );
    let limited = engine
        .receive_webhook(&token, signed("{}", "issues", "d4"))
        .await;
    assert_eq!(limited.status, 429);
    assert!(limited.retry_after.is_some_and(|seconds| seconds > 0));

    let listed = engine.list_webhooks().webhooks.remove(0);
    assert_eq!(listed.counts.accepted, 2);
    assert_eq!(listed.counts.rejected, 1);
    assert_eq!(listed.counts.duplicate, 1);
    assert_eq!(listed.counts.filtered, 1);
    assert_eq!(listed.counts.rate_limited, 1);
    assert!(
        listed
            .history
            .iter()
            .all(|event| event.delivery_id.is_some())
    );

    // A disabled webhook's URL is unknown.
    let mut disable = WebhookUpdateParams::new(webhook.id.clone());
    disable.enabled = Some(false);
    engine.update_webhook(disable).await.expect("disabled");
    assert_eq!(
        engine
            .receive_webhook(&token, signed("{}", "issues", "d5"))
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn threads_share_a_session_and_the_backlog_waits_for_a_free_slot() {
    let (endpoint, responses, _captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let mut create = WebhookCreateParams::new(WebhookTarget::New(template(&selection)));
    create.name = Some("Issues".into());
    create.max_concurrent = Some(1);
    let webhook = engine
        .create_webhook(create)
        .await
        .expect("created")
        .webhook;
    assert_eq!(webhook.delivery, Some(ProducerDeliveryMode::Queue));
    let token = webhook.url_token.clone();

    // The first event of issue 1 starts its thread, whose run waits for a
    // model answer.
    let first = event_id(
        &engine
            .receive_webhook(&token, request(r#"{"issue":1,"n":"a"}"#, &[]))
            .await,
    );
    let hook = await_webhook(engine, &webhook.id, "the first event", |hook| {
        outcome_of(hook, &first) == StoredEventOutcome::Delivered
    })
    .await;
    let thread = hook.event(&first).unwrap().session_id.unwrap();
    assert_eq!(hook.threads.session("1"), Some(thread));
    let meta = engine.get_session(thread).unwrap();
    let created_by = meta
        .webhook
        .as_ref()
        .expect("the session records its webhook");
    assert_eq!(created_by.webhook_id, webhook.id);
    assert_eq!(created_by.thread_key.as_deref(), Some("1"));
    assert!(meta.title.unwrap().to_string().ends_with("· 1"));

    // Issue 1's next event goes into its thread whatever the limit.
    let second = event_id(
        &engine
            .receive_webhook(&token, request(r#"{"issue":1,"n":"b"}"#, &[]))
            .await,
    );
    // Issue 2 needs a session of its own and waits for the slot.
    let third = event_id(
        &engine
            .receive_webhook(&token, request(r#"{"issue":2,"n":"c"}"#, &[]))
            .await,
    );
    let hook = await_webhook(engine, &webhook.id, "the second and third", |hook| {
        outcome_of(hook, &second) == StoredEventOutcome::Delivered
            && outcome_of(hook, &third) == StoredEventOutcome::Waiting
    })
    .await;
    assert_eq!(hook.event(&second).unwrap().session_id, Some(thread));
    assert_eq!(engine.list_webhooks().webhooks[0].waiting, 1);

    for _ in 0..3 {
        answer_event(&responses);
    }
    let hook = await_webhook(engine, &webhook.id, "the third's run", |hook| {
        hook.event(&third)
            .is_some_and(|event| event.run_end.is_some())
    })
    .await;
    let other = hook.event(&third).unwrap().session_id.unwrap();
    assert_ne!(other, thread);
    assert_eq!(hook.threads.session("2"), Some(other));
    assert_eq!(webhook_messages(engine, thread, &webhook.id), 2);
    assert_eq!(thread_prompts(engine, thread, &webhook.id), 1);
    assert_eq!(thread_prompts(engine, other, &webhook.id), 1);
}

#[tokio::test]
async fn a_full_backlog_answers_503_and_turning_webhooks_off_answers_404() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.shutdown().await;
    fixture.config.runtime.webhooks.backlog = 1;
    fixture.engine = reopen_engine(&fixture);
    let engine = &fixture.engine;
    let mut create = WebhookCreateParams::new(WebhookTarget::New(WebhookTemplate::for_agent(
        selection.agent.clone(),
    )));
    create.max_concurrent = Some(1);
    let webhook = engine
        .create_webhook(create)
        .await
        .expect("created")
        .webhook;
    let token = webhook.url_token.clone();
    let first = event_id(&engine.receive_webhook(&token, request("{}", &[])).await);
    await_webhook(engine, &webhook.id, "the first's session", |hook| {
        outcome_of(hook, &first) == StoredEventOutcome::Delivered
    })
    .await;
    let second = event_id(&engine.receive_webhook(&token, request("{}", &[])).await);
    await_webhook(engine, &webhook.id, "the second waits", |hook| {
        outcome_of(hook, &second) == StoredEventOutcome::Waiting
    })
    .await;
    let full = engine.receive_webhook(&token, request("{}", &[])).await;
    assert_eq!(full.status, 503);
    assert!(full.retry_after.is_some());

    fixture.engine.shutdown().await;
    fixture.config.runtime.webhooks.enabled = false;
    fixture.engine = reopen_engine(&fixture);
    assert_eq!(
        fixture
            .engine
            .receive_webhook(&token, request("{}", &[]))
            .await
            .status,
        404
    );
    assert!(!fixture.engine.list_webhooks().receiving);
}

#[tokio::test]
async fn an_event_recorded_before_a_restart_is_delivered_exactly_once() {
    let (endpoint, responses, _captured) = scripted_channel_server(2).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let webhook = engine
        .create_webhook(WebhookCreateParams::new(WebhookTarget::Session(session)))
        .await
        .expect("created")
        .webhook;
    answer_event(&responses);
    let delivered = event_id(
        &engine
            .receive_webhook(&webhook.url_token, request(r#"{"n":1}"#, &[]))
            .await,
    );
    await_webhook(engine, &webhook.id, "the first's run", |hook| {
        hook.event(&delivered)
            .is_some_and(|event| event.run_end.is_some())
    })
    .await;
    // The daemon stops after the first event's message was accepted but
    // before it was recorded, and after the second was stored.
    let id = webhook.id.clone();
    let reset = delivered.clone();
    engine
        .webhooks_now(move |_, webhooks| {
            let event = webhooks
                .iter_mut()
                .find(|webhook| webhook.id == id)
                .unwrap()
                .event_mut(&reset)
                .unwrap();
            event.outcome = StoredEventOutcome::Pending;
            event.message_id = None;
            event.run_end = None;
            Ok(())
        })
        .unwrap();
    let stored = engine.inner.webhooks.webhook(&webhook.id).unwrap();
    let pending = engine
        .store_webhook_event(&stored, request(r#"{"n":2}"#, &[]), false)
        .await
        .expect("stored");
    fixture.engine.shutdown().await;

    answer_event(&responses);
    let reopened = reopen_engine(&fixture);
    let done = await_webhook(&reopened, &webhook.id, "both delivered", |hook| {
        outcome_of(hook, &delivered) == StoredEventOutcome::Delivered
            && hook
                .event(&pending)
                .is_some_and(|event| event.run_end.is_some())
    })
    .await;
    assert_eq!(done.event(&pending).unwrap().session_id, Some(session));
    assert_eq!(webhook_messages(&reopened, session, &webhook.id), 2);
    reopened.shutdown().await;
}

#[tokio::test]
async fn a_test_event_skips_the_checks_and_a_large_body_becomes_an_artifact() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let mut create = WebhookCreateParams::new(WebhookTarget::Session(session));
    create.verify = Some(WebhookVerify::of_kind(WebhookVerifyKind::Bearer));
    create.max_per_hour = Some(1);
    let created = engine.create_webhook(create).await.expect("created");
    assert!(created.secret.is_some());
    let webhook = created.webhook;
    let large = format!(r#"{{"log":"{}"}}"#, "x".repeat(40 * 1024));
    answer_event(&responses);
    let tested = engine
        .test_webhook(WebhookTestParams {
            id: webhook.id.clone(),
            body: Some(large),
            content_type: None,
            headers: Vec::new(),
        })
        .await
        .expect("tested");
    let event = tested.event;
    let hook = await_webhook(engine, &webhook.id, "the test's run", |hook| {
        hook.event(&event)
            .is_some_and(|event| event.run_end.is_some())
    })
    .await;
    assert!(hook.event(&event).unwrap().test);
    assert_eq!(hook.counts.accepted, 0);
    assert!(hook.recent.is_empty());
    let requests = captured.await.expect("model requests");
    assert!(
        requests[0].contains("artifact://sha256/"),
        "{}",
        requests[0]
    );
    // Only the start is inline: the inline limit cuts the run of `x`.
    assert!(requests[0].contains(&"x".repeat(1024)));
    assert!(!requests[0].contains(&"x".repeat(20 * 1024)));
}

#[tokio::test]
async fn failed_deliveries_disable_the_webhook_and_test_events_count_for_nothing() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection).unwrap().session_id;
    let mut create = WebhookCreateParams::new(WebhookTarget::Session(session));
    create.max_consecutive_failures = Some(1);
    let webhook = engine
        .create_webhook(create)
        .await
        .expect("created")
        .webhook;
    let stored = engine.inner.webhooks.webhook(&webhook.id).unwrap();
    // An event whose body is gone cannot be delivered.
    for test in [true, false] {
        let event = engine
            .store_webhook_event(&stored, request("{}", &[]), test)
            .await
            .unwrap();
        engine.inner.webhooks.remove_body(&webhook.id, &event);
        engine.pump_webhook(&webhook.id).await;
        let hook = engine.inner.webhooks.webhook(&webhook.id).unwrap();
        assert!(matches!(
            outcome_of(&hook, &event),
            StoredEventOutcome::Failed { .. }
        ));
        assert_eq!(hook.enabled, test);
    }
    let listed = engine.list_webhooks().webhooks.remove(0);
    assert!(matches!(
        listed
            .disabled_reason
            .as_ref()
            .and_then(|reason| reason.known()),
        Some(cookie_agent_protocol::WebhookDisabledReason::Failed { failures: 1 })
    ));
    // Enabling it again resets the count.
    let mut enable = WebhookUpdateParams::new(webhook.id.clone());
    enable.enabled = Some(true);
    let enabled = engine.update_webhook(enable).await.unwrap().webhook;
    assert!(enabled.enabled);
    assert_eq!(enabled.consecutive_failures, 0);
}

#[tokio::test]
async fn definitions_are_checked_and_secrets_rotate() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let mut in_session = WebhookCreateParams::new(WebhookTarget::Session(session));
    in_session.max_concurrent = Some(2);
    assert!(engine.create_webhook(in_session).await.is_err());
    let mut unkeyed = WebhookCreateParams::new(WebhookTarget::New(WebhookTemplate::for_agent(
        selection.agent.clone(),
    )));
    unkeyed.delivery = Some(ProducerDeliveryMode::Steer);
    assert!(engine.create_webhook(unkeyed).await.is_err());
    let mut both = template(&selection);
    both.thread_key = Some(WebhookValueSource {
        header: Some("X-Key".into()),
        json: Some("/key".into()),
    });
    assert!(
        engine
            .create_webhook(WebhookCreateParams::new(WebhookTarget::New(both)))
            .await
            .is_err()
    );
    assert!(
        engine
            .create_webhook(WebhookCreateParams::new(WebhookTarget::Subagent(session)))
            .await
            .is_err()
    );

    let mut create = WebhookCreateParams::new(WebhookTarget::Session(session));
    create.verify = Some(WebhookVerify::of_kind(WebhookVerifyKind::Standard));
    let created = engine.create_webhook(create).await.unwrap();
    let first = created.secret.unwrap();
    assert!(first.starts_with("whsec_"));
    let id = created.webhook.id.clone();
    let rotated = engine
        .rotate_webhook(id.clone(), cookie_agent_protocol::WebhookRotate::Secret)
        .await
        .unwrap();
    let second = rotated.secret.unwrap();
    assert_ne!(first, second);
    assert_eq!(engine.webhook_secret(&id).unwrap(), Some(second));
    let old_token = created.webhook.url_token;
    let moved = engine
        .rotate_webhook(id.clone(), cookie_agent_protocol::WebhookRotate::Url)
        .await
        .unwrap();
    assert_ne!(moved.webhook.url_token, old_token);
    assert!(!engine.webhook_receives(&old_token));
    assert!(engine.webhook_receives(&moved.webhook.url_token));
    // An update of the same kind keeps the secret and returns none.
    let mut same = WebhookUpdateParams::new(id.clone());
    same.verify = Some(Some(WebhookVerify::of_kind(WebhookVerifyKind::Standard)));
    assert_eq!(engine.update_webhook(same).await.unwrap().secret, None);
    assert_eq!(moved.webhook.created_by, CronActor::User);
}

#[tokio::test]
async fn deleting_a_session_or_a_project_deletes_its_webhooks() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let project = engine
        .create_project(ProjectCreateParams {
            name: "webhook-test".parse().unwrap(),
            emoji: None,
            defaults: None,
        })
        .await
        .unwrap();
    let root = engine.create_session(selection.clone()).unwrap().session_id;
    let other = engine.create_session(selection.clone()).unwrap().session_id;
    let rooted = engine
        .create_webhook(WebhookCreateParams::new(WebhookTarget::Session(root)))
        .await
        .unwrap()
        .webhook;
    let kept = engine
        .create_webhook(WebhookCreateParams::new(WebhookTarget::Session(other)))
        .await
        .unwrap()
        .webhook;
    let in_project = engine
        .create_webhook(WebhookCreateParams::new(WebhookTarget::New(
            WebhookTemplate {
                project: Some(project.id.clone()),
                ..WebhookTemplate::for_agent(selection.agent.clone())
            },
        )))
        .await
        .unwrap()
        .webhook;

    let last_seq = engine.inner.store.get(root).unwrap().meta.last_event_seq;
    engine
        .delete_session(root, last_seq)
        .await
        .expect("deleted");
    assert!(engine.inner.webhooks.webhook(&rooted.id).is_none());
    assert!(engine.inner.webhooks.webhook(&kept.id).is_some());

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
    assert!(engine.inner.webhooks.webhook(&in_project.id).is_none());
    assert!(engine.inner.webhooks.webhook(&kept.id).is_some());
}

#[tokio::test]
async fn root_runs_get_the_webhook_tools_asking_and_list_agents_with_them() {
    use cookie_agent_protocol::{
        PermissionAction, PermissionEffect, PermissionRule, WildcardPattern,
    };
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
            .filter(|name| name.starts_with("webhook_") || name == "list_agents")
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&policy),
        [
            "list_agents",
            "webhook_create",
            "webhook_delete",
            "webhook_list",
            "webhook_update"
        ]
    );
    assert!(policy.agent.permissions.iter().any(|rule| {
        rule.action == PermissionAction::WEBHOOK
            && rule.resource.as_str() == "*"
            && rule.effect == PermissionEffect::Ask
    }));

    // Without webhook or cron rules, the webhook tools, webhook_list, and
    // list_agents all go.
    let mut neither = policy.clone();
    neither.agent.permissions.retain(|rule| {
        rule.action != PermissionAction::WEBHOOK && rule.action != PermissionAction::CRON
    });
    for action in [PermissionAction::WEBHOOK, PermissionAction::CRON] {
        neither.agent.permissions.push(PermissionRule::new(
            action,
            WildcardPattern::new("*").unwrap(),
            PermissionEffect::Deny,
        ));
    }
    assert!(names(&neither).is_empty());
}

#[test]
fn delegated_and_internal_runs_get_no_webhook_rule() {
    use cookie_agent_config::AgentMode;
    use cookie_agent_protocol::PermissionEffect;
    let root = crate::permissions::webhook_default_rules(AgentMode::Primary, false);
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].effect, PermissionEffect::Ask);
    assert!(crate::permissions::webhook_default_rules(AgentMode::Primary, true).is_empty());
    assert!(crate::permissions::webhook_default_rules(AgentMode::Internal, false).is_empty());
}

#[tokio::test]
async fn agents_cannot_set_a_mode_or_a_model_and_target_only_their_tree() {
    use cookie_agent_protocol::{CronAgentActor, PermissionMode};

    use crate::runtime::cron::api::CronRequester;

    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let root = engine.create_session(selection.clone()).unwrap().session_id;
    let child = create_buffered_delegated_child(engine, root);
    let agent = CronRequester::Agent(CronAgentActor {
        agent: selection.agent.clone(),
        session: root,
    });
    let mut moded = WebhookCreateParams::new(WebhookTarget::New(template(&selection)));
    moded.permission_mode = Some(PermissionMode::Yolo);
    let refused = engine
        .create_webhook_as(agent.clone(), moded)
        .await
        .expect_err("an agent's mode");
    assert!(refused.to_string().contains("permission_mode"), "{refused}");
    let mut pinned = template(&selection);
    pinned.model = Some(selection.model.clone());
    assert!(
        engine
            .create_webhook_as(
                agent.clone(),
                WebhookCreateParams::new(WebhookTarget::New(pinned))
            )
            .await
            .is_err()
    );
    let made = engine
        .create_webhook_as(
            agent.clone(),
            WebhookCreateParams::new(WebhookTarget::New(template(&selection))),
        )
        .await
        .unwrap();
    assert_eq!(
        made.webhook.permission_mode,
        Some(PermissionMode::AutoApproveN)
    );
    assert!(matches!(made.webhook.created_by, CronActor::Agent(_)));

    assert_eq!(
        engine
            .webhook_target_for_test(root, serde_json::json!("self"))
            .unwrap(),
        WebhookTarget::Session(root)
    );
    assert_eq!(
        engine
            .webhook_target_for_test(child, serde_json::json!("self"))
            .unwrap(),
        WebhookTarget::Subagent(child)
    );
    assert!(
        engine
            .webhook_target_for_test(child, serde_json::json!(root.to_string()))
            .is_err()
    );
    let WebhookTarget::New(made) = engine
        .webhook_target_for_test(
            root,
            serde_json::json!({"new": {"thread_key": {"header": "X-Thread"}}}),
        )
        .unwrap()
    else {
        panic!("a new-session target");
    };
    assert_eq!(made.agent, selection.agent);
    assert_eq!(
        made.thread_key,
        Some(WebhookValueSource::header("X-Thread"))
    );
}

use std::sync::Arc;

use cookie_agent_protocol::{
    ClientRunId, EventPayload, PermissionAction, RunStartParams, SessionStatus,
};

use crate::{Engine, EngineOptions};

use super::support::*;

#[tokio::test]
async fn staged_skill_child_recovers_after_reservation_before_install_restart() {
    let (endpoint, responses, server) = scripted_staged_recovery_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
    let (reserved, release) = fixture.engine.install_skill_fork_reservation_hook();
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("parent");
    let parent_run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("staged-restart-parent").expect("run ID"),
                selection,
                content: "delegate staged restart".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("parent run");
    with_watchdog("reserved fixture completion", reserved)
        .await
        .expect("durable staged reservation");
    let entry = fixture
        .engine
        .inner
        .delegation_events
        .entries()
        .into_iter()
        .find(|entry| entry.request.staged_skill.is_some())
        .expect("staged reservation event");
    let child_id = entry.reservation.child_session_id;
    let before = fixture
        .engine
        .inner
        .store
        .get(child_id)
        .expect("reserved child");
    assert!(before.runs.is_empty());
    assert!(
        !before
            .log
            .events()
            .iter()
            .any(|event| { matches!(event.payload, EventPayload::SkillLoaded { .. }) })
    );

    let snapshot = private_tempdir();
    copy_private_test_tree(
        &fixture._directory.path().join("data"),
        &snapshot.path().join("data"),
    );
    let cwd = fixture._directory.path().to_owned();
    let config = fixture.config.clone();
    let manager = Arc::clone(&fixture.manager);
    let _ = fixture
        .engine
        .cancel_run(
            parent_run.run_id,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await;
    release.notify_one();
    fixture.engine.shutdown().await;
    drop(fixture.engine);

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "Apply the staged skill `restart-skill`.",
            scripted_text_body("recovered child complete"),
        ))
        .expect("child response");
    let reopened = Engine::open(EngineOptions {
        data_dir: snapshot.path().join("data"),
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(&cwd),
        cwd,
        config,
        model_manager: manager,
    })
    .expect("reopen at staged reservation window");
    reopened
        .resume(parent.session_id)
        .await
        .expect("resume parent delegation recovery");
    await_projection(
        &reopened,
        child_id,
        "recovered child completion",
        |child| {
            child.log.events().iter().any(|event| {
                matches!(event.payload, EventPayload::SkillLoaded { ref name, .. } if name == "restart-skill")
            }) && child.status == SessionStatus::Completed
        },
    )
    .await;
    let grants = reopened
        .skill_grants_for_session(child_id, None)
        .expect("reconstructed child grants");
    assert!(grants.rules.iter().any(|rule| {
        rule.action == PermissionAction::BASH && rule.resource.as_str() == "git *"
    }));
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("staged recovery server");
    let child_request = requests
        .iter()
        .find(|request| request.contains("Restart recovered skill body"))
        .expect("child request body");
    assert_eq!(
        child_request
            .matches("Restart recovered skill body")
            .count(),
        1
    );
    reopened.shutdown().await;
}

#[test]
fn project_skill_grants_apply_to_the_contributing_attachment_only() {
    let (fixture, _selection) = custom_fixture();
    let session = cookie_agent_protocol::SessionId::new_v7();
    let grant = |action, resource: &str| cookie_agent_protocol::PermissionRule {
        action,
        runtime: None,
        resource: cookie_agent_protocol::WildcardPattern::new(resource).unwrap(),
        effect: cookie_agent_protocol::PermissionEffect::Allow,
    };
    let labpc = cookie_agent_protocol::AttachmentName::new("labpc_api").unwrap();
    let local = cookie_agent_protocol::AttachmentName::new("local_web").unwrap();
    fixture.engine.install_skill_grants_for_test(
        session,
        "deploy",
        Some(labpc.clone()),
        vec![
            grant(cookie_agent_protocol::PermissionAction::BASH, "make *"),
            grant(cookie_agent_protocol::PermissionAction::SKILL, "release"),
        ],
    );
    fixture.engine.install_skill_grants_for_test(
        session,
        "user-notes",
        None,
        vec![grant(
            cookie_agent_protocol::PermissionAction::BASH,
            "notes *",
        )],
    );
    let granted = |attachment: Option<&cookie_agent_protocol::AttachmentName>| {
        let mut resources = fixture
            .engine
            .skill_grants_for_session(session, attachment)
            .expect("grants")
            .rules
            .into_iter()
            .map(|rule| rule.resource.as_str().to_owned())
            .collect::<Vec<_>>();
        resources.sort();
        resources
    };
    assert_eq!(granted(Some(&labpc)), ["make *", "notes *", "release"]);
    assert_eq!(granted(Some(&local)), ["notes *"]);
    // The base layer sees the project skill's grants for base actions only.
    assert_eq!(granted(None), ["notes *", "release"]);
}

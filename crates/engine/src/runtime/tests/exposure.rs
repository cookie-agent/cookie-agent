//! Session tool groups and skills (`docs/site/specs/session-tool-groups.md`):
//! a session's choices, an agent document's, and a project's defaults, as
//! the tool list, `session.tools.get`, and the skill listing see them.

use cookie_agent_protocol::{
    EventOrigin, ExposureLayer, SkillPattern, SkillRule, SkillState, ToolExposure, ToolGroupRule,
    ToolGroupState,
};

use super::support::*;

fn origin() -> EventOrigin {
    EventOrigin::new("client:exposure-test").unwrap()
}

fn groups(rules: &[(&str, ToolGroupState)]) -> ToolExposure {
    ToolExposure {
        tool_groups: rules
            .iter()
            .map(|(selector, state)| ToolGroupRule {
                selector: selector.parse().unwrap(),
                state: *state,
                from_project: false,
            })
            .collect(),
        skills: Vec::new(),
    }
}

fn tool_names(
    fixture: &Fixture,
    session: cookie_agent_protocol::SessionId,
    selection: &cookie_agent_protocol::RunSelection,
) -> Vec<String> {
    let policy = frozen_root_policy(fixture, selection);
    fixture
        .engine
        .tool_definitions(session, &policy)
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

#[tokio::test]
async fn a_session_choice_turns_a_group_off_and_reset_brings_it_back() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    let listed = fixture.engine.get_session_tools(session).unwrap();
    let memory = listed
        .groups
        .iter()
        .find(|group| group.name == "memory")
        .expect("the memory group");
    assert_eq!(memory.layer, ExposureLayer::Default);
    assert!(tool_names(&fixture, session, &selection).contains(&"memory_read".to_owned()));
    // Fixed groups are not listed.
    assert!(!listed.groups.iter().any(|group| group.name == "artifacts"));

    let changed = fixture
        .engine
        .set_session_tools(
            session,
            groups(&[("memory", ToolGroupState::Off)]),
            origin(),
        )
        .await
        .unwrap();
    let memory = changed
        .groups
        .iter()
        .find(|group| group.name == "memory")
        .unwrap();
    assert_eq!(
        (memory.state, memory.layer, memory.below_layer),
        (
            ToolGroupState::Off,
            ExposureLayer::Session,
            ExposureLayer::Default
        )
    );
    assert!(!tool_names(&fixture, session, &selection).contains(&"memory_read".to_owned()));

    fixture
        .engine
        .set_session_tools(session, ToolExposure::default(), origin())
        .await
        .unwrap();
    assert!(tool_names(&fixture, session, &selection).contains(&"memory_read".to_owned()));
}

#[tokio::test]
async fn a_session_refuses_patterns_and_fixed_groups() {
    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    for rules in [
        groups(&[("mcp:*", ToolGroupState::Off)]),
        groups(&[("artifacts", ToolGroupState::Off)]),
    ] {
        let error = fixture
            .engine
            .set_session_tools(session, rules, origin())
            .await
            .expect_err("refused");
        assert!(
            matches!(error, crate::EngineError::ToolExposure(_)),
            "{error}"
        );
    }
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(session)
            .unwrap()
            .tool_exposure
            .is_empty()
    );
}

#[tokio::test]
async fn any_session_choice_beats_the_agents_rule_and_deferred_forces_deferral() {
    let agent = "---\ndescription: Exposure agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\ntool_groups:\n  memory: on\n  cron: deferred\n---\nWork.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent("http://127.0.0.1:9/v1", agent);
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    let state = |fixture: &Fixture, name: &str| {
        let listed = fixture.engine.get_session_tools(session).unwrap();
        let group = listed
            .groups
            .into_iter()
            .find(|group| group.name == name)
            .unwrap();
        (group.state, group.layer)
    };
    assert_eq!(
        state(&fixture, "memory"),
        (ToolGroupState::On, ExposureLayer::Agent)
    );
    assert_eq!(
        state(&fixture, "cron"),
        (ToolGroupState::Deferred, ExposureLayer::Agent)
    );
    fixture
        .engine
        .set_session_tools(
            session,
            groups(&[("memory", ToolGroupState::Off)]),
            origin(),
        )
        .await
        .unwrap();
    assert_eq!(
        state(&fixture, "memory"),
        (ToolGroupState::Off, ExposureLayer::Session)
    );
}

#[tokio::test]
async fn an_off_skill_is_refused_to_the_model_but_not_to_the_user() {
    let (mut fixture, selection) = custom_fixture();
    let user = fixture._directory.path().join("user-skills");
    for name in ["notes", "deploy"] {
        let directory = user.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        write_private_test_file(
            &directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: The {name} skill.\n---\nDo {name}.\n"),
        );
    }
    fixture.config.skills = cookie_agent_config::load_skill_roots(None, Some(&user), &[]).unwrap();
    fixture.engine.shutdown().await;
    fixture.engine = reopen_engine(&fixture);
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    fixture
        .engine
        .set_session_tools(
            session,
            ToolExposure {
                tool_groups: Vec::new(),
                skills: vec![SkillRule {
                    skill: SkillPattern::new("deploy").unwrap(),
                    state: SkillState::Off,
                    from_project: false,
                }],
            },
            origin(),
        )
        .await
        .unwrap();
    let error = fixture
        .engine
        .get_model_skill(session, "deploy", "")
        .expect_err("off for the model");
    assert!(error.to_string().contains("off in this session"), "{error}");
    fixture
        .engine
        .get_model_skill(session, "notes", "")
        .expect("on for the model");
    fixture
        .engine
        .get_skill(session, "deploy", "")
        .expect("the user may still run it");
    let visible = fixture
        .engine
        .list_skills(session)
        .unwrap()
        .skills
        .into_iter()
        .map(|skill| (skill.name, skill.visible))
        .collect::<Vec<_>>();
    assert_eq!(
        visible,
        [("deploy".to_owned(), false), ("notes".to_owned(), true)]
    );
    let listed = fixture.engine.get_session_tools(session).unwrap();
    let off = listed
        .skills
        .iter()
        .find(|listed| listed.name == "deploy")
        .unwrap();
    assert_eq!(
        (off.state, off.layer),
        (SkillState::Off, ExposureLayer::Session)
    );
}

#[tokio::test]
async fn a_runtime_shows_its_groups_live_or_as_of_the_last_time_it_ran() {
    let (fixture, selection) = custom_fixture();
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        std::sync::Arc::new(TestWriteProvider {
            executed: std::sync::Arc::new(TestFlag::default()),
        }),
    );
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    let local = fixture
        .engine
        .session_runtimes(session)
        .unwrap()
        .into_iter()
        .find(|attachment| attachment.runtime_id.as_str() == "local")
        .expect("a local attachment");
    let workspace = local.workspace.clone().expect("a workspace");
    // Composing the session's tools remembers what `local` offers.
    fixture.engine.get_session_tools(session).unwrap();
    let groups = |workspace: Option<String>, runtime: &str| {
        fixture
            .engine
            .runtime_groups(cookie_agent_protocol::RuntimeGroupsParams {
                runtime_id: runtime.parse().unwrap(),
                capability: Some(cookie_agent_protocol::CapabilityName::coding()),
                workspace,
            })
            .unwrap()
    };
    let live = groups(Some(workspace.clone()), "local");
    assert!(live.known && live.live, "{live:?}");
    assert!(!live.groups.is_empty());
    // A workspace it never opened: what it offered elsewhere, and where.
    let elsewhere = groups(Some("/nowhere/at/all".to_owned()), "local");
    assert!(elsewhere.known && !elsewhere.live, "{elsewhere:?}");
    assert!(elsewhere.as_of.is_some());
    assert!(elsewhere.seen_at.is_some());
    assert!(elsewhere.skills.is_empty());
    // A runtime that never ran is not known.
    assert!(!groups(None, "gpubox").known);
}

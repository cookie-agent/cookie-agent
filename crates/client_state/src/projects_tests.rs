use cookie_agent_protocol::{
    AgentId, AgentRuntime, CapabilityName, ProjectName, SemVer, SessionStatus,
};

use super::*;

fn project(id: &str, name: &str, emoji: Option<&str>) -> ProjectInfo {
    ProjectInfo {
        id: ProjectId::new(id).unwrap(),
        name: ProjectName::new(name).unwrap(),
        emoji: emoji.map(str::to_owned),
        revision: "r1".into(),
        defaults: None,
        has_agents_md: false,
        sessions: 0,
        memories: 1,
    }
}

fn session(name: &str, project: Option<&str>, status: &str, minute: u32) -> SessionMeta {
    let mut meta: SessionMeta = serde_json::from_value(serde_json::json!({
        "session_id": SessionId::new_v7(),
        "origin": {"type": "root"},
        "short_id": name,
        "cwd_identity": "/w",
        "title": null,
        "last_agent_activity": null,
        "creation_selection": {
            "agent": "primary",
            "model": {"model": "custom.local/test", "variant": null},
        },
        "runtime_revision": format!("sha256:{}", "1".repeat(64)),
        "catalog_revision": format!("sha256:{}", "2".repeat(64)),
        "provider_state_revision": format!("sha256:{}", "3".repeat(64)),
        "model_revision": format!("sha256:{}", "4".repeat(64)),
        "agent_revision": format!("sha256:{}", "5".repeat(64)),
        "recipe_registry_revision": format!("sha256:{}", "6".repeat(64)),
        "manifest_revision": format!("sha256:{}", "7".repeat(64)),
        "title_updated_seq": 0,
        "last_event_seq": 1,
        "last_activity": format!("2026-10-20T10:{minute:02}:00Z"),
        "status": status,
        "skipped_events": [],
    }))
    .expect("session meta");
    meta.project = project.map(|id| ProjectId::new(id).unwrap());
    meta
}

fn names(sessions: &[SessionMeta]) -> Vec<&str> {
    sessions
        .iter()
        .map(|session| session.short_id.as_deref().unwrap_or_default())
        .collect()
}

#[test]
fn projects_need_client_protocol_1_8() {
    let version = |minor| Some(SemVer::new(1, minor, 0));
    assert!(!daemon_has_projects(None));
    assert!(!daemon_has_projects(version(7)));
    assert!(daemon_has_projects(version(8)));
    assert!(daemon_has_projects(version(9)));
    assert!(!daemon_has_projects(Some(SemVer::new(2, 8, 0))));
}

#[test]
fn a_project_is_named_with_its_emoji_first() {
    assert_eq!(
        project_label(Some(&project("a", "web", Some("🍪")))),
        "🍪 web"
    );
    assert_eq!(project_label(Some(&project("a", "web", None))), "web");
    assert_eq!(project_label(None), "no project");
}

#[test]
fn sections_follow_their_latest_session_and_no_project_comes_last() {
    let projects = vec![
        project("aaa", "alpha", None),
        project("bbb", "beta", Some("🧪")),
        project("ccc", "gamma", None),
        project("ddd", "delta", None),
    ];
    let mut child = session("child", Some("aaa"), "idle", 59);
    child.origin = serde_json::from_value(serde_json::json!({
        "type": "delegated",
        "parent_session_id": SessionId::new_v7(),
        "parent_run_id": cookie_agent_protocol::RunId::new_v7(),
        "parent_tool_call_id": SessionId::new_v7(),
        "invocation_id": SessionId::new_v7(),
        "root_session_id": SessionId::new_v7(),
        "depth": 1,
    }))
    .expect("delegated origin");
    let sessions = vec![
        session("a-old", Some("aaa"), "idle", 1),
        session("b-new", Some("bbb"), "idle", 30),
        session("loose", None, "idle", 50),
        session("gone", Some("zzz"), "idle", 40),
        session("a-new", Some("aaa"), "completed", 20),
        child,
    ];
    let sections = project_sections(&projects, &sessions);
    let order = sections
        .iter()
        .map(|section| project_label(section.project.as_ref()))
        .collect::<Vec<_>>();
    // beta's latest is 10:30, alpha's 10:20; delta and gamma have none and
    // keep the list's order; "No project" is last though its session is
    // the newest.
    assert_eq!(order, ["🧪 beta", "alpha", "gamma", "delta", "no project"]);
    assert_eq!(names(&sections[1].sessions), ["a-new", "a-old"]);
    // A project that no longer exists lists as no project; a delegated
    // session lists nowhere.
    assert_eq!(names(&sections[4].sessions), ["loose", "gone"]);
    assert_eq!(sections[0].key(), "project:bbb");
    assert_eq!(sections[4].key(), "none");
}

#[test]
fn active_sessions_lead_each_section() {
    let projects = vec![project("aaa", "alpha", None)];
    let sessions = vec![
        session("idle-new", Some("aaa"), "idle", 50),
        session("running-old", Some("aaa"), "running", 5),
        session("tie-a", Some("aaa"), "completed", 30),
        session("tie-b", Some("aaa"), "failed", 30),
        session("loose-idle", None, "idle", 40),
        session("loose-running", None, "running", 1),
    ];
    let sections = project_sections(&projects, &sessions);
    assert_eq!(
        names(&sections[0].sessions),
        ["running-old", "idle-new", "tie-a", "tie-b"]
    );
    assert_eq!(
        names(&sections[1].sessions),
        ["loose-running", "loose-idle"]
    );
    assert_eq!(sections[0].sessions[0].status, SessionStatus::Running);
}

#[test]
fn the_picker_filters_projects_and_keeps_new_project() {
    let projects = vec![
        project("aaa", "web-shop", Some("🛒")),
        project("bbb", "notes", None),
    ];
    let all = project_choices(&projects, "", true);
    assert_eq!(
        all,
        [
            ProjectChoice::Project(ProjectId::new("aaa").unwrap()),
            ProjectChoice::Project(ProjectId::new("bbb").unwrap()),
            ProjectChoice::NoProject,
            ProjectChoice::NewProject,
        ]
    );
    assert_eq!(
        project_choices(&projects, " SHOP ", false),
        [ProjectChoice::Project(ProjectId::new("aaa").unwrap())]
    );
    assert_eq!(
        project_choices(&projects, "🛒", true),
        [
            ProjectChoice::Project(ProjectId::new("aaa").unwrap()),
            ProjectChoice::NewProject
        ]
    );
    assert_eq!(
        project_choices(&projects, "no p", true),
        [ProjectChoice::NoProject, ProjectChoice::NewProject]
    );
}

#[test]
fn an_emoji_is_one_emoji_as_the_daemon_checks() {
    for good in ["🍪", "👩🏽‍💻", "🏳️‍🌈", "❤️", "🇯🇵", "1️⃣"] {
        assert_eq!(validate_emoji(good), Ok(()), "{good}");
    }
    assert_eq!(validate_emoji(""), Err("it is empty".to_owned()));
    assert_eq!(validate_emoji("a"), Err("it is not an emoji".to_owned()));
    assert_eq!(validate_emoji("1"), Err("it is not an emoji".to_owned()));
    assert_eq!(
        validate_emoji("🍪🍪"),
        Err("it is more than one emoji".to_owned())
    );
    assert_eq!(
        validate_emoji("🍪 "),
        Err("it holds a space or a control character".to_owned())
    );
    assert_eq!(validate_emoji("é"), Err("it is not an emoji".to_owned()));
    assert!(validate_emoji(&"👍".repeat(9)).is_err());
}

fn runtime(id: &str, workspace: Option<&str>) -> ProjectRuntime {
    ProjectRuntime {
        runtime_id: RuntimeId::new(id).unwrap(),
        capability: workspace.map(|_| CapabilityName::coding()),
        workspace: workspace.map(str::to_owned),
    }
}

#[test]
fn defaults_attach_when_they_name_runtimes_or_drop_global() {
    let mut defaults = ProjectDefaults {
        agent: Some(AgentId::new("builder").unwrap()),
        ..ProjectDefaults::default()
    };
    assert!(!defaults_attach(&defaults));
    defaults.global = false;
    assert!(defaults_attach(&defaults));
    defaults.global = true;
    defaults.runtimes = vec![runtime("local", Some("/w")), runtime("box", None)];
    assert!(defaults_attach(&defaults));
    let (global, requests) = defaults_requests(&defaults);
    assert!(global);
    assert_eq!(
        requests,
        [
            RuntimeAttachRequest {
                runtime_id: RuntimeId::new("local").unwrap(),
                capability: Some(CapabilityName::coding()),
                workspace: Some("/w".into()),
            },
            RuntimeAttachRequest {
                runtime_id: RuntimeId::new("box").unwrap(),
                capability: None,
                workspace: None,
            },
        ]
    );
}

#[test]
fn the_editor_saves_defaults_only_when_they_say_something() {
    assert_eq!(editor_defaults(true, &[], None, None), None);
    let local = RuntimeAttachRequest {
        runtime_id: RuntimeId::new("local").unwrap(),
        capability: Some(CapabilityName::coding()),
        workspace: Some(" ~/web ".into()),
    };
    let global = RuntimeAttachRequest {
        runtime_id: RuntimeId::global(),
        capability: None,
        workspace: None,
    };
    let saved = editor_defaults(
        false,
        &[global, local.clone(), local],
        Some(&AgentId::new("builder").unwrap()),
        Some("rust"),
    )
    .expect("defaults");
    assert!(!saved.global);
    assert_eq!(saved.runtimes, [runtime("local", Some("~/web"))]);
    assert_eq!(saved.agent.as_ref().map(AgentId::as_str), Some("builder"));
    assert_eq!(saved.preset.as_deref(), Some("rust"));
}

fn agent(id: &str, preset: Option<&str>, root: bool, models: &[&str]) -> AgentDescriptor {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "preset": preset,
        "description": "An agent",
        "mode": "primary",
        "enabled": true,
        "runnable_as_root": root,
        "resolved_fallback": models
            .iter()
            .map(|model| serde_json::json!({"model": model, "variant": null}))
            .collect::<Vec<_>>(),
        "delegation_targets": [],
    }))
    .expect("agent descriptor")
}

fn model(key: &str) -> ModelSelection {
    serde_json::from_value(serde_json::json!({"model": key, "variant": null})).expect("model")
}

#[test]
fn the_default_agent_brings_its_preset_and_its_own_model() {
    let agents = vec![
        agent("primary", None, true, &["custom.local/a"]),
        agent(
            "builder",
            Some("rust"),
            true,
            &["custom.local/gone", "custom.local/b"],
        ),
        agent("reviewer", Some("rust"), false, &["custom.local/b"]),
    ];
    let live = |selection: &ModelSelection| selection.model.as_str() != "custom.local/gone";
    let fallback = Some(model("custom.local/a"));
    let mut info = project("aaa", "web", None);
    assert_eq!(
        project_agent(&info, &agents, live, fallback.clone()),
        ProjectAgent::Unset
    );
    info.defaults = Some(ProjectDefaults {
        agent: Some(AgentId::new("builder").unwrap()),
        preset: Some("rust".into()),
        ..ProjectDefaults::default()
    });
    assert_eq!(
        project_agent(&info, &agents, live, fallback.clone()),
        ProjectAgent::Selected(RunSelection {
            agent: AgentId::new("builder").unwrap(),
            model: model("custom.local/b"),
            preset: Some("rust".into()),
        })
    );
    // An agent that cannot run as root keeps the usual selection.
    info.defaults.as_mut().unwrap().agent = Some(AgentId::new("reviewer").unwrap());
    let ProjectAgent::Unavailable(message) = project_agent(&info, &agents, live, fallback.clone())
    else {
        panic!("reviewer cannot run as root");
    };
    assert_eq!(
        message,
        "Project web names agent reviewer of preset rust, which cannot start a session here; using the usual agent."
    );
    // A preset alone picks its primary, else its first agent.
    info.defaults.as_mut().unwrap().agent = None;
    let ProjectAgent::Selected(selection) = project_agent(&info, &agents, live, fallback) else {
        panic!("the preset has a root agent");
    };
    assert_eq!(selection.agent.as_str(), "builder");
}

#[test]
fn an_acp_default_agent_runs_its_synthetic_model() {
    // The synthetic model is in no catalog, so `live` refuses it.
    let mut claude = agent("claude", None, true, &["acp/claude"]);
    claude.runtimes = vec![AgentRuntime {
        runtime_id: RuntimeId::new("local").unwrap(),
        agent: AgentId::new("claude").unwrap(),
        available: true,
        options: Vec::new(),
        input_media: Vec::new(),
    }];
    let agents = vec![claude];
    let live = |selection: &ModelSelection| selection.model.as_str() != "acp/claude";
    let mut info = project("aaa", "web", None);
    info.defaults = Some(ProjectDefaults {
        agent: Some(AgentId::new("claude").unwrap()),
        ..ProjectDefaults::default()
    });
    assert_eq!(
        project_agent(&info, &agents, live, Some(model("custom.local/a"))),
        ProjectAgent::Selected(RunSelection {
            agent: AgentId::new("claude").unwrap(),
            model: model("acp/claude"),
            preset: None,
        })
    );
}

#[test]
fn the_delete_question_counts_what_goes() {
    let mut info = project("aaa", "web", None);
    info.sessions = 1;
    info.memories = 3;
    assert_eq!(
        delete_question(&info),
        "Delete project web with its 1 session, 3 memories, and AGENTS.md? This cannot be undone."
    );
}

#[test]
fn a_refused_delete_names_its_running_sessions() {
    let mut running = session("busy", Some("aaa"), "running", 1);
    running.title = Some(cookie_agent_protocol::SessionTitle::new("Fix the build").unwrap());
    let other = session("other", Some("aaa"), "running", 2);
    let missing = SessionId::new_v7();
    let ids = [running.session_id, other.session_id];
    let sessions = vec![running, other];
    assert_eq!(
        running_sessions_message(&ids[..1], &sessions),
        "“Fix the build” is running; stop it, then delete the project."
    );
    assert_eq!(
        running_sessions_message(&[ids[0], ids[1], missing], &sessions),
        format!(
            "“Fix the build”, “other”, {missing} are running; stop them, then delete the project."
        )
    );
    let error = ClientError::Rpc(cookie_agent_protocol::JsonRpcError {
        code: -32000,
        message: "Sessions of this project are running, so it cannot be deleted.".into(),
        data: Some(serde_json::json!({
            "code": "project_sessions_running",
            "project": "aaa",
            "session_ids": ids,
        })),
    });
    assert_eq!(running_sessions(&error), ids);
}

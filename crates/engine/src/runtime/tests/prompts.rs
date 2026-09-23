use std::{fs, sync::Arc};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, RunSelection, RunStartParams, Sha256Digest,
};

use crate::{EngineError, PromptSection};

use super::support::*;

#[tokio::test]
async fn tool_prompt_sections_are_ordered_fingerprinted_and_frozen() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    for input in ["first prompt run", "same prompt run", "changed prompt run"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .expect("scripted response");
    }
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let first_provider = TestPromptProvider::new(
        "test.first&local",
        vec![PromptSection {
            title: "First".into(),
            body: "First section\r\nnormalized.".into(),
        }],
    );
    let second_provider = TestPromptProvider::new(
        "test.second",
        vec![PromptSection {
            title: "Second".into(),
            body: "Second section.".into(),
        }],
    );
    fixture
        .engine
        .register_tool_provider(Arc::new(first_provider.clone()));
    fixture
        .engine
        .register_tool_provider(Arc::new(second_provider.clone()));
    let session = fixture.engine.create_session(selection.clone()).unwrap();

    for (index, input) in ["first prompt run", "same prompt run", "changed prompt run"]
        .into_iter()
        .enumerate()
    {
        if index == 2 {
            second_provider.replace_sections(vec![PromptSection {
                title: "Second".into(),
                body: "Changed second section.".into(),
            }]);
        }
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("tool-prompt-{index}")).unwrap(),
                    selection: selection.clone(),
                    input: input.into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("tool prompt run");
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
    }

    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    let snapshots = (0..3)
        .map(|index| {
            projection
                .runs
                .values()
                .find(|run| run.client_run_id.as_str() == format!("tool-prompt-{index}"))
                .expect("run projection")
                .agent
                .clone()
        })
        .collect::<Vec<_>>();
    let first_prompt = &snapshots[0].composed_prompt;
    let first_position = first_prompt.find("First section\nnormalized.").unwrap();
    let second_position = first_prompt.find("Second section.").unwrap();
    assert!(first_position < second_position);
    assert!(first_prompt.contains("provider=\"test.first&amp;local\""));
    assert_eq!(snapshots[0].composed_prompt, snapshots[1].composed_prompt);
    assert_eq!(
        snapshots[0].prompt_fingerprint,
        snapshots[1].prompt_fingerprint
    );
    assert_ne!(
        snapshots[1].prompt_fingerprint,
        snapshots[2].prompt_fingerprint
    );
    assert_eq!(
        snapshots[0].prompt_fingerprint,
        Sha256Digest::of_bytes(snapshots[0].composed_prompt.as_bytes())
    );
    assert!(snapshots[0].composed_prompt.contains("Second section."));
    assert!(
        !snapshots[0]
            .composed_prompt
            .contains("Changed second section.")
    );

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("captured prompt requests");
    assert_eq!(requests.len(), 3);
    let first_request = request_body(&requests[0]).to_string();
    assert!(first_request.contains("First section\\nnormalized."));
    assert!(first_request.contains("Second section."));
    let changed_request = request_body(&requests[2]).to_string();
    assert!(changed_request.contains("Changed second section."));
    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    let replayed = reopened.inner.store.get(session.session_id).unwrap();
    let replayed_first = replayed
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "tool-prompt-0")
        .expect("replayed first run");
    assert_eq!(replayed_first.agent.composed_prompt, *first_prompt);
    assert_eq!(
        replayed_first.agent.prompt_fingerprint,
        snapshots[0].prompt_fingerprint
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn tool_prompt_sections_precede_skills_and_plugin_addenda() {
    let (endpoint, captured) = scripted_model_server().await;
    let primary = "---\ndescription: Prompt order agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  skill: allow\n---\nTest prompt.\n";
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            &endpoint,
            primary,
            None,
            None,
            false,
            None,
            None,
            32_768,
            None,
            "openai-chat",
        );
    let skill_dir = fixture
        ._directory
        .path()
        .join(".cookie-agent/skills/order-skill");
    fs::create_dir_all(&skill_dir).expect("skill directory");
    write_private_test_file(
        &skill_dir.join("SKILL.md"),
        "---\nname: order-skill\ndescription: Order fixture skill\n---\nOrder skill body.\n",
    );
    fixture.config.skills =
        cookie_agent_config::load_skill_roots(None, None, &[fixture._directory.path().to_owned()])
            .expect("fixture skills");
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["agent_before_start"]}"#;
    reopen_with_interception_plugins(
        &mut fixture,
        vec![(
            "prompt-tail".into(),
            interception_plugin(
                "prompt-tail",
                &[
                    ("FIXTURE_CAPABILITIES", capabilities.into()),
                    (
                        "FIXTURE_AGENT_BEFORE_RESULT",
                        r#"{"append_to_system_prompt":"Plugin tail."}"#.into(),
                    ),
                ],
            ),
        )],
    )
    .await;
    fixture
        .engine
        .register_tool_provider(Arc::new(TestPromptProvider::new(
            "test.order",
            vec![PromptSection {
                title: "Order".into(),
                body: "Provider section.".into(),
            }],
        )));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("tool-prompt-order").unwrap(),
                selection,
                input: "check composition order".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("ordered composition run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    let prompt = &projection
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "tool-prompt-order")
        .expect("ordered run")
        .agent
        .composed_prompt;
    let agent = prompt.find("Test prompt.").unwrap();
    let cwd = fixture.engine.inner.store.cwd().display().to_string();
    let working_directory = prompt.find("<working_directory>").unwrap();
    assert!(prompt.contains(&format!("<working_directory>{cwd}</working_directory>")));
    let provider = prompt.find("Provider section.").unwrap();
    let skills = prompt.find("<available_skills>").unwrap();
    let plugin = prompt.find("Plugin tail.").unwrap();
    assert!(agent < working_directory);
    assert!(working_directory < provider && provider < skills && skills < plugin);
    // Every appended block is introduced by the markdown horizontal-rule
    // separator, so block boundaries are visible in the assembled prompt.
    assert!(prompt.contains("---\n\n<working_directory>"));
    assert!(prompt.contains("---\n\n<tool_instructions provider=\"test.order\">"));
    assert!(prompt.contains("---\n\n<available_skills>"));
    assert!(prompt.contains("---\n\nPlugin tail."));
    assert_eq!(
        Sha256Digest::of_bytes(prompt.as_bytes()),
        projection
            .runs
            .values()
            .find(|run| run.client_run_id.as_str() == "tool-prompt-order")
            .unwrap()
            .agent
            .prompt_fingerprint
    );
    with_watchdog("captured fixture completion", captured)
        .await
        .expect("captured ordered request");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn working_directory_section_reports_session_cwd() {
    let (endpoint, captured) = scripted_model_server().await;
    let fixture = synthetic_default_fixture_with_config(None, &endpoint, "").expect("engine");
    let descriptor = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    let selection = RunSelection {
        agent: descriptor.id,
        model: descriptor.resolved_fallback[0].clone(),
        preset: None,
    };
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("working-directory").unwrap(),
                selection,
                input: "report the working directory".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("working-directory run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    let run = projection
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "working-directory")
        .expect("working-directory run");
    let cwd = fixture.engine.inner.store.cwd().display().to_string();
    assert!(
        run.agent
            .composed_prompt
            .contains(&format!("<working_directory>{cwd}</working_directory>"))
    );
    assert_eq!(
        Sha256Digest::of_bytes(run.agent.composed_prompt.as_bytes()),
        run.agent.prompt_fingerprint
    );
    with_watchdog("captured fixture completion", captured)
        .await
        .expect("captured working-directory request");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn invalid_tool_prompt_sections_fail_run_admission() {
    fn section(title: &str, body: String) -> PromptSection {
        PromptSection {
            title: title.into(),
            body,
        }
    }

    let cases = vec![
        (
            "section-budget",
            vec![TestPromptProvider::new(
                "builtin.test",
                vec![section("oversized", "x".repeat(8 * 1024 + 1))],
            )],
            "body exceeds 8192 bytes",
        ),
        (
            "provider-budget",
            vec![TestPromptProvider::new(
                "plugin:test",
                vec![
                    section("one", "x".repeat(6_000)),
                    section("two", "x".repeat(6_000)),
                    section("three", "x".repeat(6_000)),
                ],
            )],
            "provider bodies exceed 16384 bytes",
        ),
        (
            "total-budget",
            ["test.one", "test.two", "test.three"]
                .into_iter()
                .map(|id| {
                    TestPromptProvider::new(
                        id,
                        vec![
                            section("one", "x".repeat(6_000)),
                            section("two", "x".repeat(6_000)),
                        ],
                    )
                })
                .collect(),
            "all provider bodies exceed 32768 bytes",
        ),
        (
            "validation",
            vec![TestPromptProvider::new(
                "test.invalid",
                vec![section("control", "bad\0body".into())],
            )],
            "disallowed control character",
        ),
    ];

    for (name, providers, expected) in cases {
        let (fixture, selection) = custom_fixture_with_endpoint("http://127.0.0.1:9/v1");
        for provider in providers {
            fixture.engine.register_tool_provider(Arc::new(provider));
        }
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        let error = fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("tool-prompt-error-{name}")).unwrap(),
                    selection,
                    input: "must not reach the model".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect_err("invalid section must reject admission");
        assert!(
            matches!(error, EngineError::ToolPrompt(_)),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains(expected), "{error}");
        assert!(
            fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .unwrap()
                .runs
                .is_empty()
        );
        fixture.engine.shutdown().await;
    }

    let primary = format!(
        "---\ndescription: Prompt limit agent\nmode: primary\nenabled: true\nmodels: [{{ model: \"custom.test/group/model\", variant: base }}]\npermissions: {{}}\n---\n{}\n",
        "x".repeat(121 * 1024)
    );
    let (fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent("http://127.0.0.1:9/v1", &primary);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestPromptProvider::new(
            "test.composed_limit",
            vec![section("maximum section", "x".repeat(8 * 1024))],
        )));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let error = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("tool-prompt-composed-limit").unwrap(),
                selection,
                input: "must not reach the model".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect_err("composed prompt limit must reject admission");
    assert!(
        error
            .to_string()
            .contains("composed prompt exceeds 131072 bytes"),
        "{error}"
    );
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .unwrap()
            .runs
            .is_empty()
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn agent_md_discovery_honors_override_addition_missing_disable_and_skip() {
    let (mut fixture, _) = custom_fixture();
    let root = fixture._directory.path();
    let agents = root.join(".cookie-agent").join("agents");
    write_private_test_file(&agents.join("AGENTS.md"), "default AGENTS.md context");
    write_private_test_file(&root.join("AGENTS.md"), "cwd AGENTS.md context");

    let project_source = agents.join("AGENTS.md").to_string_lossy().into_owned();
    let cwd_source = root.join("AGENTS.md").to_string_lossy().into_owned();
    let (entries, skipped) = fixture
        .engine
        .load_agent_md(None)
        .expect("default AGENTS.md context");
    assert_eq!(entries.len(), 2);
    assert!(skipped.is_empty());
    assert_eq!(entries[0].source.as_str(), project_source);
    assert_eq!(entries[0].content, "default AGENTS.md context");
    assert_eq!(entries[1].source.as_str(), cwd_source);
    assert_eq!(entries[1].content, "cwd AGENTS.md context");

    let preset = agents.join("python");
    create_private_test_dir(&preset);
    write_private_test_file(&preset.join("AGENTS.md"), "preset AGENTS.md context");
    let (entries, skipped) = fixture
        .engine
        .load_agent_md(Some("python"))
        .expect("preset AGENTS.md context");
    assert_eq!(entries.len(), 2);
    assert!(skipped.is_empty());
    assert_eq!(
        entries[0].source.as_str(),
        preset.join("AGENTS.md").to_string_lossy()
    );
    assert_eq!(entries[0].content, "preset AGENTS.md context");
    assert!(
        entries
            .iter()
            .all(|entry| entry.content != "default AGENTS.md context")
    );

    write_private_test_file(&root.join("AGENTS.md"), "fresh cwd context");
    assert_eq!(
        fixture.engine.load_agent_md(None).unwrap().0[1].content,
        "fresh cwd context"
    );
    std::fs::remove_file(agents.join("AGENTS.md")).unwrap();
    std::fs::remove_file(preset.join("AGENTS.md")).unwrap();
    std::fs::remove_file(root.join("AGENTS.md")).unwrap();
    let (entries, skipped) = fixture.engine.load_agent_md(None).unwrap();
    assert!(entries.is_empty());
    assert!(skipped.is_empty());

    fixture.engine.shutdown().await;
    fixture.config.runtime.agent_md.enabled = false;
    fixture.engine = reopen_engine(&fixture);
    write_private_test_file(&root.join("AGENTS.md"), "disabled context");
    let (entries, skipped) = fixture.engine.load_agent_md(None).unwrap();
    assert!(entries.is_empty());
    assert!(skipped.is_empty());

    fixture.engine.shutdown().await;
    fixture.config.runtime.agent_md.enabled = true;
    fixture.engine = reopen_engine(&fixture);
    write_private_test_file(&root.join("AGENTS.md"), "oversized context");
    let oversized = root.join("AGENTS.md");
    let oversized_len = crate::runtime::AGENT_MD_MAX_BYTES + 1;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&oversized)
        .unwrap();
    file.set_len(oversized_len).unwrap();
    drop(file);
    let (entries, skipped) = fixture.engine.load_agent_md(None).unwrap();
    assert!(entries.is_empty());
    assert_eq!(skipped.len(), 1);
    assert_eq!(
        skipped[0].path.as_str(),
        oversized.to_string_lossy().as_ref()
    );
    assert_eq!(skipped[0].byte_length, oversized_len);
    let rendered = crate::model_history::agent_md_turn_for_test(&entries);
    assert!(rendered.is_empty());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn consecutive_root_runs_reload_agent_md() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "first agent-md run",
            scripted_text_body("first complete"),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "second agent-md run",
            scripted_text_body("second complete"),
        ))
        .unwrap();
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let context_path = fixture._directory.path().join("AGENTS.md");
    write_private_test_file(&context_path, "run one AGENTS.md context");
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let first = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("agent-md-first").unwrap(),
                selection: selection.clone(),
                input: "first agent-md run".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    write_private_test_file(&context_path, "run two AGENTS.md context");
    let second = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("agent-md-second").unwrap(),
                selection,
                input: "second agent-md run".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("run one AGENTS.md context"));
    assert!(!requests[0].contains("run two AGENTS.md context"));
    assert!(requests[1].contains("run two AGENTS.md context"));
    assert!(!requests[1].contains("run one AGENTS.md context"));

    let contexts = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .into_iter()
        .filter_map(|event| {
            let EventPayload::AgentMdLoaded { entries } = event.payload else {
                return None;
            };
            Some((event.run_id.unwrap(), entries[0].content.clone()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        contexts,
        vec![
            (first.run_id, "run one AGENTS.md context".into()),
            (second.run_id, "run two AGENTS.md context".into()),
        ]
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn root_run_persists_and_replays_agent_md_as_a_user_turn() {
    let (endpoint, captured) = scripted_model_server().await;
    let (mut fixture, mut selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.shutdown().await;
    fixture
        .config
        .agent_presets
        .insert("python".into(), fixture.config.agents.clone());
    fixture.engine = reopen_engine(&fixture);
    selection.preset = Some("python".into());
    let root = fixture._directory.path();
    write_private_test_file(
        &root.join(".cookie-agent/agents/AGENTS.md"),
        "overridden default context",
    );
    create_private_test_dir(&root.join(".cookie-agent/agents/python"));
    write_private_test_file(
        &root.join(".cookie-agent/agents/python/AGENTS.md"),
        "preset replay context",
    );
    write_private_test_file(&root.join("AGENTS.md"), "cwd replay context");
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("agent-md-replay").unwrap(),
                selection,
                input: "run with AGENTS.md context".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("start AGENTS.md context run");
    let request = with_watchdog("captured fixture completion", captured)
        .await
        .expect("captured AGENTS.md context request");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let loaded = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::AgentMdLoaded { .. }))
        .expect("AGENTS.md context event");
    assert_eq!(
        loaded.origin.as_ref().map(|origin| origin.as_str()),
        Some("engine:agent-md")
    );
    let EventPayload::AgentMdLoaded { entries } = &loaded.payload else {
        unreachable!()
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].source.as_str(),
        root.join(".cookie-agent/agents/python/AGENTS.md")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(entries[0].content, "preset replay context");

    let body = request_body(&request);
    let messages = body["messages"].as_array().expect("chat messages");
    assert_eq!(messages[0]["role"], "system");
    let context = messages
        .iter()
        .find(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("<system-reminder>"))
        })
        .expect("AGENTS.md context user turn");
    let content = context["content"].as_str().unwrap();
    assert!(content.contains("# AGENTS.md"));
    assert!(content.contains("OVERRIDE any default behavior"));
    assert!(content.contains("from=\""));
    assert!(content.contains("preset replay context"));
    assert!(!content.contains("overridden default context"));
    assert!(content.contains("cwd replay context"));
    assert!(content.contains("</system-reminder>"));
    assert!(!messages[0].to_string().contains("preset replay context"));
    fixture.engine.shutdown().await;
}

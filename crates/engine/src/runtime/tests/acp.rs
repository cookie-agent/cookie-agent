//! ACP agents (`docs/site/specs/acp-agents.md`): runtime-hosted agents
//! registered as `<runtime>-<agent>`, whose runs the ACP runner takes,
//! against the scripted agent `tests/fixtures/acp_agent.py` on the
//! protocol `local` runtime.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use cookie_agent_protocol::{
    AcpSessionStart, ClientRunId, DisplayBlock, EventOrigin, EventPayload, Open,
    PersistedAssistantPart, RunId, RunOutcome, RunSelection, RunStartParams, SessionId, ToolSource,
    ToolTerminationOutcome,
};

use super::support::*;
use crate::{Engine, EngineOptions};

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp_agent.py")
}

/// An engine whose `local` runtime hosts the scripted ACP agent as
/// `fixture` (`fixture`), started with `env`, and whose user layer
/// has the agent `documents` (file name, text).
struct AcpFixture {
    fixture: Fixture,
    log: PathBuf,
    _markers: PanicResistantTempDir,
}

impl AcpFixture {
    fn engine(&self) -> &Engine {
        &self.fixture.engine
    }

    fn workspace(&self) -> PathBuf {
        self.fixture
            .engine
            .inner
            .runtimes
            .local_workspace_root()
            .to_owned()
    }

    /// The messages the agent received, in order.
    fn received(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("logged message"))
            .collect()
    }

    fn received_methods(&self) -> Vec<String> {
        self.received()
            .iter()
            .filter_map(|message| message.get("method").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .collect()
    }
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}

async fn acp_fixture(env: &[(&str, &str)], documents: &[(&str, &str)]) -> AcpFixture {
    acp_fixture_with(env, documents, "").await
}

async fn acp_fixture_with(
    env: &[(&str, &str)],
    documents: &[(&str, &str)],
    extra_config: &str,
) -> AcpFixture {
    let directory = private_tempdir();
    let markers = private_tempdir();
    let log = markers.path().join("received.jsonl");
    let user = directory.path().join("user");
    create_private_test_dir(&user);
    let agents = user.join("agents");
    create_private_test_dir(&agents);
    let mut variables = vec![(
        "ACP_FIXTURE_LOG".to_owned(),
        log.to_string_lossy().into_owned(),
    )];
    variables.extend(
        env.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    let env_table = variables
        .iter()
        .map(|(name, value)| format!("{name} = {}", toml_string(value)))
        .collect::<Vec<_>>()
        .join(", ");
    let config = format!(
        "{extra_config}\n[runtimes.local.acp.agents.fixture]\ncommand = [{}, {}]\nenv = {{ {env_table} }}\ndescription = \"Fixture agent\"\n",
        toml_string(python_command()),
        toml_string(&fixture_script().to_string_lossy()),
    );
    write_private_test_file(&user.join("config.toml"), config);
    for (name, text) in documents {
        write_private_test_file(&agents.join(name), text);
    }
    let config = cookie_agent_config::load_from_user_root(Some(&user)).expect("configuration");
    let provider_store = directory.path().join("provider-store");
    create_private_test_dir(&provider_store);
    let manager = Arc::new(
        cookie_agent_models::ModelManager::new(
            BTreeMap::new(),
            empty_catalog(),
            cookie_agent_models::provider_store::ProviderStore::open(provider_store)
                .expect("provider store"),
        )
        .expect("empty manager"),
    );
    let workspace = directory.path().join("workspace");
    create_private_test_dir(&workspace);
    let engine = Engine::open_async(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: workspace.clone(),
        config: config.clone(),
        model_manager: Arc::clone(&manager),
        runtimes: cookie_agent_runtime_api::RuntimeOptions {
            local: cookie_agent_runtime_api::LocalRuntime::Protocol(
                cookie_agent_coding_runtime::server::in_process_connector_with_options(
                    cookie_agent_coding_runtime::server::ServeOptions {
                        bridge_command: vec!["cookie-test-bridge".to_owned()],
                        ..Default::default()
                    },
                ),
            ),
            ..cookie_agent_runtime_api::RuntimeOptions::new(&workspace)
        },
    })
    .await
    .expect("engine with ACP agents");
    engine.register_base_layer().expect("the base layer");
    AcpFixture {
        fixture: Fixture {
            _directory: directory,
            engine,
            config,
            manager,
        },
        log,
        _markers: markers,
    }
}

fn agent_id() -> cookie_agent_protocol::AgentId {
    cookie_agent_protocol::AgentId::new("fixture").unwrap()
}

fn selection() -> RunSelection {
    RunSelection {
        agent: agent_id(),
        model: cookie_agent_protocol::acp_model_selection(&agent_id()),
        preset: None,
    }
}

async fn start(engine: &Engine, session: SessionId, key: &str, text: &str) -> RunId {
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(key).unwrap(),
                selection: selection(),
                content: text.into(),
            },
            origin(),
        )
        .await
        .expect("run started")
        .run_id
}

/// Waits for `run` to end and returns how.
async fn finished(engine: &Engine, session: SessionId, run: RunId) -> RunOutcome {
    let event = await_event(engine, session, "the run's end", |event| {
        event.run_id == Some(run) && matches!(event.payload, EventPayload::RunFinished { .. })
    })
    .await;
    match event.payload {
        EventPayload::RunFinished {
            outcome: Open::Known(outcome),
        } => outcome,
        other => panic!("unexpected {other:?}"),
    }
}

/// Runs one prompt `text` in `session` and returns how it ended.
async fn turn(engine: &Engine, session: SessionId, key: &str, text: &str) -> RunOutcome {
    let run = start(engine, session, key, text).await;
    let outcome = finished(engine, session, run).await;
    wait_for_session_not_running(engine, session).await;
    outcome
}

fn final_text(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Completed { final_text } => final_text.clone().unwrap_or_default(),
        other => panic!("the run did not complete: {other:?}"),
    }
}

fn events(engine: &Engine, session: SessionId) -> Vec<cookie_agent_protocol::StoredEvent> {
    engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .to_vec()
}

fn session_starts(
    engine: &Engine,
    session: SessionId,
) -> Vec<cookie_agent_protocol::AcpSessionRecord> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AcpSessionStarted { record } => Some(record),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_agent_is_registered_under_its_runtime() {
    let acp = acp_fixture(&[], &[]).await;
    let runtime = acp.engine().current_runtime();
    let descriptor = runtime
        .result
        .snapshot
        .agents
        .iter()
        .find(|agent| agent.id == agent_id())
        .expect("the agent is listed");
    assert_eq!(descriptor.description, "Fixture agent");
    assert!(descriptor.runnable_as_root);
    let place = descriptor.runtime.as_ref().expect("its runtime");
    assert_eq!(place.runtime_id.as_str(), "local");
    assert_eq!(place.agent.as_str(), "fixture");
    assert!(place.available);
    assert_eq!(descriptor.runtimes, std::slice::from_ref(place));
    assert_eq!(
        descriptor.resolved_fallback,
        [cookie_agent_protocol::acp_model_selection(&agent_id())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_renders_chunks_thoughts_tool_calls_a_plan_and_usage() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "work", "work").await;
    assert_eq!(final_text(&outcome), "Done.");
    let events = events(engine, session);
    let record = session_starts(engine, session)
        .pop()
        .expect("the ACP session");
    assert_eq!(record.how, AcpSessionStart::New);
    assert_eq!(record.runtime_id.as_str(), "local");
    assert_eq!(record.runtime_agent.as_str(), "fixture");
    assert_eq!(record.protocol_version, 1);
    assert_eq!(record.agent_info["name"], "fixture-agent");
    assert_eq!(
        Path::new(&record.cwd),
        cookie_agent_protocol::paths::canonicalize(&acp.workspace()).unwrap()
    );
    let turns = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ModelTurnCommitted {
                turn,
                resolved_model,
                ..
            } => {
                assert_eq!(resolved_model.selection.model.as_str(), "acp/fixture");
                Some(turn.content.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(turns.len(), 2, "{turns:?}");
    assert!(matches!(
        &turns[0][..],
        [
            PersistedAssistantPart::Reasoning { text: thought, .. },
            PersistedAssistantPart::Text { text, .. },
            PersistedAssistantPart::ToolCall { id, .. },
        ] if thought == "planning" && text == "Editing." && id.as_str() == "call-edit"
    ));
    assert!(matches!(
        &turns[1][..],
        [PersistedAssistantPart::Text { text, .. }] if text == "Done."
    ));
    let start = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } => Some(start.clone()),
            _ => None,
        })
        .expect("the tool call");
    assert_eq!(start.presentation.title.as_str(), "Edit notes.txt");
    assert!(matches!(
        start.presentation.tool.source.as_ref().and_then(Open::known),
        Some(ToolSource::Acp { agent, runtime_id })
            if *agent == agent_id() && runtime_id.as_str() == "local"
    ));
    let termination = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination.clone()),
            _ => None,
        })
        .expect("the tool call's end");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Completed);
    let result = termination.result.expect("its result");
    assert!(result.display.iter().any(|block| matches!(
        block,
        Open::Known(DisplayBlock::Diff(diff)) if diff.path == "/work/notes.txt" && diff.added == 1 && diff.removed == 1
    )));
    assert!(result.display.iter().any(|block| matches!(
        block,
        Open::Known(DisplayBlock::Link { label: Some(label), .. }) if label == "/work/notes.txt:2"
    )));
    let plan = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::AcpSessionUpdated { update } => update.plan.clone(),
            _ => None,
        })
        .expect("the plan");
    assert!(matches!(
        plan,
        Open::Known(DisplayBlock::Plan { entries }) if entries.len() == 2
            && entries[1].status == cookie_agent_protocol::PlanEntryStatus::InProgress
    ));
    let usage = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelUsageRecorded {
                usage,
                estimated_cost_pico_usd,
                ..
            } => Some((usage.clone(), *estimated_cost_pico_usd)),
            _ => None,
        })
        .expect("usage");
    assert_eq!(usage.0.input_tokens, Some(10));
    assert_eq!(usage.0.output_tokens, Some(5));
    assert_eq!(usage.1, Some(500_000_000_000));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_session_starts_in_auto_mode_with_the_documents_config_and_commands() {
    let document = "---\ndescription: Fixture with settings\nmode: all\nacp:\n  config: { model: large, effort: high }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    let set = acp
        .received()
        .into_iter()
        .filter(|message| message["method"] == "session/set_config_option")
        .map(|message| {
            (
                message["params"]["configId"].as_str().unwrap().to_owned(),
                message["params"]["value"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        set,
        [
            ("mode".to_owned(), "auto".to_owned()),
            ("model".to_owned(), "large".to_owned())
        ]
    );
    let events = events(engine, session);
    let options = events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::AcpSessionUpdated { update } => update.options.clone(),
            _ => None,
        })
        .expect("the options");
    let current = |id: &str| {
        options
            .iter()
            .find(|option| option.id == id)
            .map(|option| option.current_value.clone())
    };
    assert_eq!(current("mode").as_deref(), Some("auto"));
    assert_eq!(current("model").as_deref(), Some("large"));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::Diagnostic { message, .. } if message.contains("option `effort`")
    )));
    let commands = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::AcpSessionUpdated { update } => update.commands.clone(),
            _ => None,
        })
        .expect("the commands");
    assert_eq!(commands[0].name, "review");
    assert_eq!(commands[0].input_hint.as_deref(), Some("what to review"));
}

#[tokio::test(flavor = "multi_thread")]
async fn modes_without_a_mode_option_start_in_auto_and_an_option_can_be_set() {
    let acp = acp_fixture(&[("ACP_FIXTURE_MODES", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    assert!(
        acp.received()
            .iter()
            .any(|message| message["method"] == "session/set_mode"
                && message["params"]["modeId"] == "auto")
    );
    let result = engine
        .set_session_agent_option(cookie_agent_protocol::SessionAgentOptionSetParams {
            session_id: session,
            option_id: "mode".to_owned(),
            value: "plan".to_owned(),
        })
        .await
        .expect("option set");
    let mode = result
        .options
        .iter()
        .find(|option| option.id == "mode")
        .unwrap();
    assert_eq!(mode.current_value, "plan");
    let refused = engine
        .set_session_agent_option(cookie_agent_protocol::SessionAgentOptionSetParams {
            session_id: session,
            option_id: "model".to_owned(),
            value: "huge".to_owned(),
        })
        .await
        .expect_err("an unknown value is refused");
    assert!(refused.to_string().contains("no value `huge`"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_an_auto_mode_the_agent_keeps_its_default() {
    let acp = acp_fixture(&[("ACP_FIXTURE_NO_AUTO", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    assert!(
        !acp.received_methods()
            .iter()
            .any(|method| method == "session/set_config_option")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_stop_reason_ends_the_run_as_acp_specifies() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for (index, reason) in ["max_tokens", "max_turn_requests", "refusal"]
        .iter()
        .enumerate()
    {
        let outcome = turn(
            engine,
            session,
            &format!("stop-{index}"),
            &format!("stop {reason}"),
        )
        .await;
        assert!(
            matches!(&outcome, RunOutcome::Failed { error, .. } if error.as_str().contains(reason)),
            "{outcome:?}"
        );
    }
    let outcome = turn(engine, session, "stop-cancelled", "stop cancelled").await;
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    let outcome = turn(engine, session, "end", "say fine").await;
    assert_eq!(final_text(&outcome), "fine");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_error_fails_the_run_with_its_message() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "error", "error").await;
    assert!(
        matches!(&outcome, RunOutcome::Failed { error, .. } if error.as_str().contains("Authentication required")),
        "{outcome:?}"
    );
}

/// The moment an ACP run's `RunFinished` arrives (completed, failed, or
/// cancelled by the agent), a command that needs the session idle is
/// accepted: every other round a delete, which minds the engine's active
/// runs, and between them another run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_commands_are_accepted_the_moment_an_acp_run_ends() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let run_end = |event: &cookie_agent_protocol::EventSubscriptionMessage, run: RunId| match event
    {
        cookie_agent_protocol::EventSubscriptionMessage::Event { event }
            if event.run_id == Some(run) =>
        {
            match &event.payload {
                EventPayload::RunFinished {
                    outcome: Open::Known(outcome),
                } => Some(outcome.clone()),
                _ => None,
            }
        }
        _ => None,
    };
    for round in 0..60 {
        let session = engine.create_session(selection()).unwrap().session_id;
        let (_, mut live) = engine.subscribe(session, None).await.expect("subscribe");
        let (text, expected) = [
            ("say hi", "completed"),
            ("error", "failed"),
            ("stop cancelled", "cancelled"),
        ][round / 2 % 3];
        let first = start(engine, session, "first", text).await;
        let outcome = with_watchdog("the run's end", async {
            loop {
                let message = live.recv().await.expect("live subscription");
                if let Some(outcome) = run_end(&message, first) {
                    return outcome;
                }
            }
        })
        .await;
        let ended = match outcome {
            RunOutcome::Completed { .. } => "completed",
            RunOutcome::Failed { .. } => "failed",
            RunOutcome::Cancelled { .. } => "cancelled",
            other => panic!("round {round}: the run ended {other:?}"),
        };
        assert_eq!(ended, expected, "round {round}");
        if round % 2 == 1 {
            let tip = engine
                .inner
                .store
                .get(session)
                .unwrap()
                .log
                .last_event()
                .map_or(0, |event| event.seq);
            if let Err(error) = engine.delete_session(session, tip).await {
                panic!("round {round}: delete after a {ended} run was refused: {error:?}");
            }
            continue;
        }
        let next = engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session,
                    client_run_id: ClientRunId::new("next").unwrap(),
                    selection: selection(),
                    content: "say again".into(),
                },
                origin(),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("round {round}: a run after a {ended} run was refused: {error:?}")
            })
            .run_id;
        with_watchdog("the next run's end", async {
            while run_end(&live.recv().await.expect("live subscription"), next).is_none() {}
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_process_exit_fails_the_run_with_its_stderr_and_the_next_turn_starts_again() {
    let acp = acp_fixture(&[("ACP_FIXTURE_RESUME", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "exit", "exit").await;
    assert!(
        matches!(&outcome, RunOutcome::Failed { error, .. }
            if error.as_str().contains("status 3") && error.as_str().contains("fixture crashed on purpose")),
        "{outcome:?}"
    );
    let outcome = turn(engine, session, "again", "say back").await;
    assert_eq!(final_text(&outcome), "back");
    let starts = session_starts(engine, session);
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].how, AcpSessionStart::Resumed);
    assert_eq!(starts[1].acp_session_id, starts[0].acp_session_id);
}

/// Stops the agent's process, as a daemon or runtime restart would.
async fn stop_process(engine: &Engine) {
    let key = super::super::acp::process::ProcessKey {
        runtime: "local".to_owned(),
        agent: "fixture".to_owned(),
    };
    let process = engine
        .inner
        .acp
        .processes
        .running(&key)
        .expect("the agent runs");
    engine.inner.acp.processes.stop(&process, "test").await;
}

/// The ACP sessions and bridges the engine holds for one cookie session.
fn acp_state_of(engine: &Engine, session: SessionId) -> (bool, usize) {
    let processes = &engine.inner.acp.processes;
    let live = processes.sessions.lock().unwrap().contains_key(&session);
    let bridges = processes
        .bridges
        .lock()
        .unwrap()
        .values()
        .filter(|binding| binding.session == session)
        .count();
    (live, bridges)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_process_and_a_paged_out_session_leave_no_acp_session_behind() {
    let acp = acp_fixture(&[("ACP_FIXTURE_RESUME", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say one").await;
    assert_eq!(acp_state_of(engine, session), (true, 1));
    // A process stopped while idle takes its sessions and their bridges.
    stop_process(engine).await;
    assert_eq!(acp_state_of(engine, session), (false, 0));

    // Paging the session out ends its live ACP session; the next turn
    // resumes it.
    turn(engine, session, "second", "say two").await;
    assert_eq!(acp_state_of(engine, session), (true, 1));
    let evicted = engine
        .evict_idle_roots_with(std::time::Duration::ZERO)
        .await
        .expect("idle janitor pass");
    assert_eq!(evicted, vec![session]);
    assert_eq!(acp_state_of(engine, session), (false, 0));
    let outcome = turn(engine, session, "third", "say three").await;
    assert_eq!(final_text(&outcome), "three");
    let starts = session_starts(engine, session);
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[2].how, AcpSessionStart::Resumed);
    assert_eq!(starts[2].acp_session_id, starts[0].acp_session_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_load_ignores_the_replay() {
    let acp = acp_fixture(&[("ACP_FIXTURE_LOAD", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say one").await;
    stop_process(engine).await;
    let outcome = turn(engine, session, "second", "say two").await;
    assert_eq!(final_text(&outcome), "two");
    let starts = session_starts(engine, session);
    assert_eq!(starts[1].how, AcpSessionStart::Loaded);
    assert!(!events(engine, session).iter().any(|event| matches!(
        &event.payload,
        EventPayload::ModelTurnCommitted { turn, .. } if turn.content.iter().any(|part| matches!(
            part,
            PersistedAssistantPart::Text { text, .. } if text.contains("REPLAYED")
        ))
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_session_is_replaced_and_the_session_is_told() {
    // Every tool hidden: the first prompt carries no tool instructions.
    let document = "---\ndescription: Fixture with a body\nmode: all\nacp: { hide_tools: [\"*\"] }\n---\nYou review changes.\n";
    let acp = acp_fixture(
        &[("ACP_FIXTURE_RESUME", "1"), ("ACP_FIXTURE_LOST", "1")],
        &[("fixture.md", document)],
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let first = turn(engine, session, "first", "echo").await;
    assert!(final_text(&first).ends_with(" | You review changes.\n | echo"));
    let second = turn(engine, session, "second", "echo").await;
    assert_eq!(
        final_text(&second),
        "echo",
        "the body goes with the first prompt only"
    );
    stop_process(engine).await;
    let third = turn(engine, session, "third", "echo").await;
    assert!(
        final_text(&third).ends_with(" | You review changes.\n | echo"),
        "a new ACP session gets the body again"
    );
    let starts = session_starts(engine, session);
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].how, AcpSessionStart::New);
    assert_ne!(starts[1].acp_session_id, starts[0].acp_session_id);
    assert!(events(engine, session).iter().any(|event| matches!(
        &event.payload,
        EventPayload::Diagnostic { message, .. } if message.contains("could not resume")
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_requests_are_decided_by_the_mapped_rules() {
    let document = "---\ndescription: Fixture with rules\nmode: all\nacp: {}\npermissions:\n  bash: { \"git *\": allow, \"*\": deny }\n  read: { \"${workspace_dir}/*\": allow, \"*\": deny }\n  webfetch: deny\n  mcp: { \"exa_*\": allow, \"*\": deny }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let workspace = cookie_agent_protocol::paths::canonicalize(&acp.workspace()).unwrap();
    let inside = workspace.join("notes.txt");
    for (index, (command, expected)) in [
        (
            "permission execute git status".to_owned(),
            "permission: yes",
        ),
        (
            "permission execute rm -rf build".to_owned(),
            "permission: no",
        ),
        (
            format!("permission read {}", inside.display()),
            "permission: yes",
        ),
        ("permission read /etc/passwd".to_owned(), "permission: no"),
        (
            "permission fetch https://example.com".to_owned(),
            "permission: no",
        ),
        // Its own MCP servers' tools, by cookie's MCP naming.
        (
            "permission-tool other mcp__exa__web_search".to_owned(),
            "permission: yes",
        ),
        ("permission-mcp exa search".to_owned(), "permission: yes"),
        (
            "permission-tool other mcp__github__create_issue".to_owned(),
            "permission: no",
        ),
        (
            "permission-tool other create_issue (github MCP Server)".to_owned(),
            "permission: no",
        ),
        // Cookie's own tools: the bridge decides the call itself.
        (
            "permission-tool other mcp__cookie__memory_search".to_owned(),
            "permission: yes",
        ),
        (
            "permission-mcp cookie cron_list".to_owned(),
            "permission: yes",
        ),
        // A command that merely names one of cookie's tools is a command.
        (
            "permission-tool execute mcp__cookie__memory_search".to_owned(),
            "permission: no",
        ),
        ("permission think deeply".to_owned(), "permission: yes"),
    ]
    .into_iter()
    .enumerate()
    {
        let outcome = turn(engine, session, &format!("ask-{index}"), &command).await;
        assert_eq!(final_text(&outcome), expected, "{command}");
    }
    let chosen = acp
        .received()
        .into_iter()
        .filter_map(|message| {
            message["result"]["outcome"]["optionId"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert!(
        chosen
            .iter()
            .all(|option| option == "yes" || option == "no"),
        "{chosen:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unmapped_kind_asks_and_the_approval_answers_it() {
    // Even rules that allow everything leave these to the approval flow.
    let document = "---\ndescription: Fixture allowing all\nmode: all\nacp: {}\npermissions:\n  bash: allow\n  mcp: allow\n  other: allow\n  switch_mode: allow\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for (index, command) in [
        "permission switch_mode plan",
        "permission-tool other Summon the build",
        "permission-tool fly Somewhere new",
    ]
    .into_iter()
    .enumerate()
    {
        let run = start(engine, session, &format!("ask-{index}"), command).await;
        let approval = wait_for_escalated_approval(engine, session).await;
        let resources = approval.request.operation().resources();
        assert_eq!(resources.len(), 1, "{command}");
        assert!(
            matches!(resources[0].capability.as_str(), "switch_mode" | "other"),
            "{command}: {}",
            resources[0].capability.as_str()
        );
        approve_once(engine, &approval, &format!("approve-{index}")).await;
        let outcome = finished(engine, session, run).await;
        assert_eq!(final_text(&outcome), "permission: yes", "{command}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_answers_a_pending_permission_request_cancelled() {
    let document =
        "---\ndescription: Fixture that asks\nmode: all\nacp: {}\npermissions:\n  bash: ask\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let run = start(engine, session, "ask", "ask-wait execute make deploy").await;
    wait_for_escalated_approval(engine, session).await;
    engine.cancel_run(run, origin()).await.expect("cancel");
    let outcome = finished(engine, session, run).await;
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    let received = acp.received();
    assert!(
        received
            .iter()
            .any(|message| message["method"] == "session/cancel")
    );
    assert!(
        received
            .iter()
            .any(|message| message["outcome"] == "cancelled")
    );
    let terminations = events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination.outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminations, [ToolTerminationOutcome::Cancelled]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agents_title_names_the_session() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "title", "title Refactor the parser").await;
    let projection = engine.inner.store.get(session).unwrap();
    assert_eq!(
        projection.meta.title.as_ref().map(|title| title.as_str()),
        Some("Refactor the parser")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_its_runtime_attached_a_session_or_run_is_refused() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let refused = engine
        .create_session_attached(selection(), &[])
        .expect_err("no runtime that offers it");
    assert_eq!(
        refused.to_string(),
        "`fixture` needs a runtime that offers it attached: local"
    );
    // A session that detached it cannot start a run with it.
    let session = engine.create_session(selection()).unwrap().session_id;
    let local = engine
        .session_runtimes(session)
        .unwrap()
        .into_iter()
        .find(|attachment| attachment.runtime_id.as_str() == "local")
        .unwrap();
    engine
        .detach_runtime(session, local.name, origin())
        .await
        .unwrap();
    let refused = engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new("detached").unwrap(),
                selection: selection(),
                content: "say hi".into(),
            },
            origin(),
        )
        .await
        .expect_err("refused before the run starts");
    assert!(
        refused
            .to_string()
            .contains("needs a runtime that offers it attached: local"),
        "{refused}"
    );
    assert!(
        !events(engine, session)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bridge_lists_cookies_tools_without_the_hidden_ones() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "list", "mcp list").await;
    let listed = final_text(&outcome);
    assert!(listed.starts_with("tools: "), "{listed}");
    let tools = listed["tools: ".len()..].split(',').collect::<Vec<_>>();
    for hidden in ["read", "write", "edit", "bash", "webfetch"] {
        assert!(!tools.contains(&hidden), "{hidden} in {tools:?}");
    }

    let document = "---\ndescription: Fixture that sees all\nmode: all\nacp:\n  hide_tools: [\"web*\"]\npermissions:\n  read: { \"${workspace_dir}/*\": allow }\n  bash: deny\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "list", "mcp list").await;
    let listed = final_text(&outcome);
    let tools = listed["tools: ".len()..].split(',').collect::<Vec<_>>();
    assert!(tools.contains(&"read"), "{tools:?}");
    assert!(!tools.contains(&"webfetch"), "{tools:?}");
    assert!(!tools.contains(&"bash"), "denied by the rules: {tools:?}");
    let file = acp.workspace().join("notes.txt");
    std::fs::write(&file, "bridged content\n").unwrap();
    let call = format!(
        "mcp call read {}",
        serde_json::json!({"filePath": file.to_string_lossy()})
    );
    let outcome = turn(engine, session, "call", &call).await;
    let answer = final_text(&outcome);
    assert!(answer.contains("bridged content"), "{answer}");
    assert!(answer.ends_with("error: False"), "{answer}");
    let outcome = turn(
        engine,
        session,
        "hidden",
        "mcp call webfetch {\"url\": \"https://example.com\"}",
    )
    .await;
    assert!(final_text(&outcome).ends_with("error: True"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_acp_agent_takes_a_delegated_subagent_run() {
    let document = "---\ndescription: Fixture that delegates\nmode: all\nacp: {}\npermissions:\n  delegate: { fixture: allow }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let call = format!(
        "mcp call delegate_subagent {}",
        serde_json::json!({
            "description": "Child turn",
            "prompt": "say from the child",
            "agent_type": "fixture",
        })
    );
    let outcome = turn(engine, session, "delegate", &call).await;
    let answer = final_text(&outcome);
    assert!(answer.contains("from the child"), "{answer}");
    assert!(answer.ends_with("error: False"), "{answer}");
    let starts = acp
        .received()
        .into_iter()
        .filter(|message| message["method"] == "session/new")
        .count();
    assert_eq!(
        starts, 2,
        "the parent and the child each have an ACP session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_with_an_acp_agent_is_refused_while_its_runtime_is_away() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let template = engine
        .inner
        .runtimes
        .dial_in_template("labpc")
        .expect("dial-in template");
    let (daemon, runtime) = cookie_agent_protocol::in_process_pair(64);
    let stop = tokio_util::sync::CancellationToken::new();
    let options = cookie_agent_coding_runtime::server::ServeOptions {
        acp_agents: vec![cookie_agent_runtime_api::protocol::AcpAgentSettings {
            id: "codex".to_owned(),
            command: vec![
                python_command().to_owned(),
                fixture_script().to_string_lossy().into_owned(),
            ],
            env: BTreeMap::new(),
            description: "Dial-in fixture".to_owned(),
            enabled: true,
        }],
        stop: stop.clone(),
        ..Default::default()
    };
    let served = tokio::spawn(cookie_agent_coding_runtime::server::serve(
        Box::new(runtime),
        options,
    ));
    let settings = crate::runtimes::runtime_settings(&acp.fixture.config, "labpc");
    template
        .connect(
            cookie_agent_runtime_api::RuntimeConnection::new(Box::new(daemon)),
            &settings,
            Some(Arc::clone(&engine.inner.mcp_hub)),
        )
        .await
        .expect("dial-in runtime connected");
    let id = cookie_agent_protocol::AgentId::new("codex").unwrap();
    let available = |engine: &Engine| {
        engine
            .current_runtime()
            .result
            .snapshot
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .and_then(|agent| agent.runtime.as_ref())
            .map(|runtime| runtime.available)
    };
    with_watchdog("the dial-in agent to register", async {
        while available(engine) != Some(true) {
            engine.refresh_runtime_agents();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    stop.cancel();
    let _ = served.await;
    with_watchdog("the dial-in agent to become unavailable", async {
        while available(engine) != Some(false) {
            engine.refresh_runtime_agents();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    let refused = engine
        .create_session(RunSelection {
            agent: id.clone(),
            model: cookie_agent_protocol::acp_model_selection(&id),
            preset: None,
        })
        .expect_err("refused while the runtime is away");
    assert!(refused.to_string().contains("is unavailable"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cron_fire_reaches_an_acp_session_as_its_prompt() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say ready").await;
    let job = engine
        .create_cron(cookie_agent_protocol::CronCreateParams {
            name: Some("Nightly".into()),
            prompt: "echo".into(),
            schedule: cookie_agent_protocol::CronSchedule::In("1h".into()),
            max_runs: None,
            target: cookie_agent_protocol::CronTarget::Session(session),
            delivery: None,
            permission_mode: None,
            max_consecutive_failures: None,
            enabled: None,
        })
        .await
        .expect("created");
    let target = job.id.clone();
    let now = jiff::Timestamp::now();
    engine
        .crons_now(move |_, jobs| {
            let job = jobs
                .iter_mut()
                .find(|job| job.id == target)
                .expect("the job");
            job.next_fire = Some(now);
            Ok(())
        })
        .expect("made due");
    engine.cron_tick(jiff::Timestamp::now()).await;
    let run = await_event(engine, session, "the fire's run", |event| {
        matches!(event.payload, EventPayload::RunStarted { .. })
            && event.run_id.is_some()
            && events(engine, session)
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
                .count()
                == 2
    })
    .await
    .run_id
    .unwrap();
    let run = events(engine, session)
        .into_iter()
        .rev()
        .find_map(|event| match event.payload {
            EventPayload::RunStarted { .. } => event.run_id,
            _ => None,
        })
        .unwrap_or(run);
    let outcome = finished(engine, session, run).await;
    let echoed = final_text(&outcome);
    assert!(echoed.contains("<scheduled_job id="), "{echoed}");
    assert!(echoed.contains("echo"), "{echoed}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bridge_serves_an_agent_that_connects_while_answering_session_new() {
    let acp = acp_fixture(&[("ACP_FIXTURE_EAGER_MCP", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "eager", "mcp eager").await;
    assert_eq!(final_text(&outcome), "eager: True");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegation_target_no_runtime_reported_yet_is_a_diagnostic_until_one_does() {
    let document = "---\ndescription: Fixture that delegates\nmode: all\nacp: {}\npermissions:\n  delegate: { fixture: allow, codex: allow, \"cod*\": allow, fxture: allow }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let diagnostics = engine.configuration_status().diagnostics;
    for name in [
        "`codex`, which is not known yet",
        "`cod*`, which matches no agent yet",
        "`fxture`, which is not known yet",
    ] {
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains(name)),
            "{name}: {diagnostics:?}"
        );
    }
    let targets = |engine: &Engine| {
        engine
            .current_runtime()
            .result
            .snapshot
            .agents
            .iter()
            .find(|agent| agent.id == agent_id())
            .map(|agent| agent.delegation_targets.clone())
            .unwrap_or_default()
    };
    assert_eq!(targets(engine), [agent_id()]);

    // Delegating to it before its runtime reports it is refused.
    let session = engine.create_session(selection()).unwrap().session_id;
    let call = format!(
        "mcp call delegate_subagent {}",
        serde_json::json!({
            "description": "Child turn",
            "prompt": "say from the child",
            "agent_type": "codex",
        })
    );
    let outcome = turn(engine, session, "early", &call).await;
    let answer = final_text(&outcome);
    assert!(
        answer.contains("no agent `codex` (no connected runtime offers it)"),
        "{answer}"
    );

    // Once the runtime dials in and reports it, it is a target.
    let template = engine
        .inner
        .runtimes
        .dial_in_template("labpc")
        .expect("dial-in template");
    let (daemon, runtime) = cookie_agent_protocol::in_process_pair(64);
    let stop = tokio_util::sync::CancellationToken::new();
    let served = tokio::spawn(cookie_agent_coding_runtime::server::serve(
        Box::new(runtime),
        cookie_agent_coding_runtime::server::ServeOptions {
            acp_agents: vec![cookie_agent_runtime_api::protocol::AcpAgentSettings {
                id: "codex".to_owned(),
                command: vec![
                    python_command().to_owned(),
                    fixture_script().to_string_lossy().into_owned(),
                ],
                env: BTreeMap::new(),
                description: "Dial-in fixture".to_owned(),
                enabled: true,
            }],
            stop: stop.clone(),
            ..Default::default()
        },
    ));
    let settings = crate::runtimes::runtime_settings(&acp.fixture.config, "labpc");
    template
        .connect(
            cookie_agent_runtime_api::RuntimeConnection::new(Box::new(daemon)),
            &settings,
            Some(Arc::clone(&engine.inner.mcp_hub)),
        )
        .await
        .expect("dial-in runtime connected");
    let codex = cookie_agent_protocol::AgentId::new("codex").unwrap();
    with_watchdog("the dial-in agent to become a target", async {
        while !targets(engine).contains(&codex) {
            engine.refresh_runtime_agents();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    let diagnostics = engine.configuration_status().diagnostics;
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(diagnostics[0].contains("`fxture`"), "{diagnostics:?}");
    stop.cancel();
    let _ = served.await;
}

/// The text blocks of each `session/prompt` the agent received.
fn prompts(acp: &AcpFixture) -> Vec<Vec<String>> {
    acp.received()
        .iter()
        .filter(|message| message["method"] == "session/prompt")
        .map(|message| {
            message["params"]["prompt"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str().map(str::to_owned))
                .collect()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_prompt_carries_the_offered_tools_instructions_the_memory_and_the_body() {
    // The memory write tools are hidden: part A of the memory section, not
    // its rubric.
    let document = "---\ndescription: Fixture with context\nmode: all\nacp:\n  hide_tools: [read, write, edit, bash, webfetch, memory_write, memory_edit, memory_delete]\n---\nYou review changes.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let profile = crate::memory::url::MemoryUrl::parse("user://profile").unwrap();
    let current = engine
        .memory_root()
        .read(&profile)
        .ok()
        .map(|read| read.entry.revision);
    engine
        .memory_root()
        .write(
            &profile,
            &crate::memory::file::MemoryDocument::new(
                "Profile",
                vec!["tea".into()],
                "Ada drinks tea.\n",
            )
            .unwrap()
            .render(),
            crate::memory::store::WriteMode::Client(current.as_ref()),
            |_: &cookie_agent_protocol::AgentId| crate::memory::agents::AgentMemory::Registered,
            &crate::memory::limits::MemoryLimits::all(4096),
            jiff::Timestamp::now(),
        )
        .unwrap();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say one").await;
    turn(engine, session, "second", "say two").await;
    let received = prompts(&acp);
    assert_eq!(received.len(), 2);
    let first = &received[0];
    assert_eq!(first.len(), 4, "{first:#?}");
    assert_eq!(
        first[0],
        "<tool_instructions provider=\"builtin.memory\">\nMemory related to this session is included automatically at the start of each session: the user's memory, your own agent memory, the session's project, and the machines its runtimes run on. Use memory_search for anything else, such as another project or a machine this session doesn't use.\n</tool_instructions>"
    );
    assert!(
        first[1].starts_with("<system-reminder>\n# Memory\n"),
        "{}",
        first[1]
    );
    assert!(first[1].contains("Ada drinks tea."), "{}", first[1]);
    assert_eq!(first[2], "You review changes.\n");
    assert_eq!(first[3], "say one");
    assert_eq!(
        received[1],
        ["say two"],
        "later prompts carry the input only"
    );

    // With the write tools offered, the rubric comes too; with no memory
    // tool offered, no memory section.
    let document = "---\ndescription: Fixture with context\nmode: all\nacp: {}\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say one").await;
    let first = &prompts(&acp)[0];
    assert_eq!(first.len(), 3, "no body: {first:#?}");
    assert!(
        first[0].contains("You manage your own memory."),
        "{}",
        first[0]
    );
    assert!(
        first[1].starts_with("<system-reminder>\n# Memory\n"),
        "{}",
        first[1]
    );
    let document = "---\ndescription: Fixture with context\nmode: all\nacp: { hide_tools: [\"memory_*\"] }\n---\nBody.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say one").await;
    let first = &prompts(&acp)[0];
    assert_eq!(first.len(), 3, "{first:#?}");
    assert!(
        first[0].starts_with("<system-reminder>\n# Memory\n"),
        "{}",
        first[0]
    );
    assert_eq!(first[1..], ["Body.\n", "say one"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_steer_reaches_the_running_turn_through_the_agents_extension() {
    let acp = acp_fixture(&[("ACP_FIXTURE_STEERING", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let run = start(engine, session, "wait", "steer-wait").await;
    await_event(engine, session, "the agent waiting", |event| {
        matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    let steered = engine
        .steer(run, "look at notes.txt".into(), origin())
        .await
        .unwrap();
    assert!(steered.accepted);
    let outcome = finished(engine, session, run).await;
    assert_eq!(final_text(&outcome), "steered: look at notes.txt");
    let record = session_starts(engine, session).pop().unwrap();
    assert!(record.steering);
    let events = events(engine, session);
    let submitted = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::UserInputSubmitted { content }
                if matches!(
                    content.parts().first(),
                    Some(Open::Known(cookie_agent_protocol::UserContentPart::Text { text }))
                        if text == "look at notes.txt"
                ) =>
            {
                Some(event.seq)
            }
            _ => None,
        })
        .expect("the steered input");
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::AcpSessionUpdated { update } if update.steered.as_ref()
            .is_some_and(|steered| steered.user_inputs == [submitted])
    )));
    let steers = acp
        .received()
        .into_iter()
        .filter(|message| message["method"] == "_session/steering")
        .collect::<Vec<_>>();
    assert_eq!(steers.len(), 1);
    assert_eq!(
        steers[0]["params"]["prompt"][0]["text"],
        "look at notes.txt"
    );
    assert_eq!(
        steers[0]["params"]["_meta"]["steering"]["idleBehavior"],
        "promptRequired"
    );
    // One round: the steer did not wait for a second prompt.
    assert_eq!(prompts(&acp).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_extension_a_steer_waits_for_the_agents_next_turn() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let run = start(engine, session, "wait", "steer-wait 1").await;
    await_event(engine, session, "the agent waiting", |event| {
        matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    engine
        .steer(run, "say later".into(), origin())
        .await
        .unwrap();
    let outcome = finished(engine, session, run).await;
    assert_eq!(final_text(&outcome), "later");
    assert!(!session_starts(engine, session).pop().unwrap().steering);
    let received = prompts(&acp);
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].last().unwrap(), "steer-wait 1");
    assert_eq!(received[1], ["say later"]);
    assert!(
        !acp.received()
            .iter()
            .any(|message| message["method"] == "_session/steering")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_calls_late_input_updates_its_presentation_and_its_content_renders() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "late", "late-input").await;
    assert_eq!(final_text(&outcome), "read");
    let events = events(engine, session);
    let start = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } => Some(start.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(start.presentation.title.as_str(), "Tool");
    assert!(start.presentation.arguments.is_empty());
    assert_eq!(start.presentation.tool.published_name, "Read");
    let presented = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::AcpSessionUpdated { update } => update.tool_presentation.clone(),
            _ => None,
        })
        .expect("the new presentation");
    assert_eq!(presented.tool_call_id, start.tool_call_id);
    assert_eq!(presented.presentation.title.as_str(), "Read notes.txt");
    assert_eq!(
        presented
            .presentation
            .primary_argument
            .as_ref()
            .map(|text| text.as_str()),
        Some("/work/notes.txt")
    );
    assert_eq!(
        presented
            .presentation
            .hint
            .as_ref()
            .map(|hint| hint.rendered()),
        Some(cookie_agent_protocol::ToolHint::FILE_READ)
    );
    assert_eq!(
        presented.presentation.arguments,
        [Open::Known(DisplayBlock::Json {
            value: serde_json::json!({"file_path": "/work/notes.txt"})
        })]
    );
    let result = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => termination.result.clone(),
            _ => None,
        })
        .unwrap();
    let display = result
        .display
        .iter()
        .filter_map(Open::known)
        .collect::<Vec<_>>();
    assert!(matches!(display[0], DisplayBlock::Markdown { text, .. } if text == "```\none\n```"));
    assert!(matches!(display[1], DisplayBlock::Media { artifact, .. }
        if artifact.media_type == "image/png" && artifact.byte_length == 8));
    assert!(
        matches!(display[2], DisplayBlock::Link { url, label: Some(label) }
        if url == "file:///work/notes.txt" && label == "notes.txt")
    );
    assert_eq!(result.metadata["acp"]["tool_name"], "Read");
}

#[tokio::test(flavor = "multi_thread")]
async fn notices_and_compactions_are_recorded_as_the_agents_notices() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "notices", "notices").await;
    let notices = events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AcpSessionUpdated { update } => update.notice,
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        notices,
        [
            cookie_agent_protocol::AcpNotice {
                severity: cookie_agent_protocol::AcpNoticeSeverity::Warning,
                title: "Rate limited".into(),
                description: Some("Retrying in 1 s.".into()),
            },
            cookie_agent_protocol::AcpNotice {
                severity: cookie_agent_protocol::AcpNoticeSeverity::Info,
                title: "The agent compacted its conversation".into(),
                description: Some("We read notes.txt.".into()),
            },
        ]
    );
    let initialize = acp
        .received()
        .into_iter()
        .find(|message| message["method"] == "initialize")
        .unwrap();
    assert_eq!(
        initialize["params"]["clientCapabilities"]["session"],
        serde_json::json!({"notices": {}, "compaction": {}})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_after_a_steered_one_still_cancels() {
    let acp = acp_fixture(&[("ACP_FIXTURE_STEERING", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let run = start(engine, session, "wait", "steer-wait").await;
    await_event(engine, session, "the agent waiting", |event| {
        matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    engine.steer(run, "go on".into(), origin()).await.unwrap();
    finished(engine, session, run).await;
    wait_for_session_not_running(engine, session).await;
    let run = start(engine, session, "cancel", "wait").await;
    await_event(engine, session, "the agent waiting", |event| {
        event.run_id == Some(run)
            && matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    engine.cancel_run(run, origin()).await.expect("cancel");
    let outcome = finished(engine, session, run).await;
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    assert!(
        acp.received()
            .iter()
            .any(|message| message["method"] == "session/cancel")
    );
}

/// Dials runtime `runtime_id` in, declaring the scripted agent as `agent`,
/// and waits until the engine registers it. Cancel the token to hang up.
async fn dial_in(
    acp: &AcpFixture,
    runtime_id: &str,
    agent: &str,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<String>,
) {
    let engine = acp.engine();
    let template = engine
        .inner
        .runtimes
        .dial_in_template(runtime_id)
        .expect("dial-in template");
    let (daemon, runtime) = cookie_agent_protocol::in_process_pair(64);
    let stop = tokio_util::sync::CancellationToken::new();
    let served = tokio::spawn(cookie_agent_coding_runtime::server::serve(
        Box::new(runtime),
        cookie_agent_coding_runtime::server::ServeOptions {
            acp_agents: vec![cookie_agent_runtime_api::protocol::AcpAgentSettings {
                id: agent.to_owned(),
                command: vec![
                    python_command().to_owned(),
                    fixture_script().to_string_lossy().into_owned(),
                ],
                env: BTreeMap::from([(
                    "ACP_FIXTURE_LOG".to_owned(),
                    acp.log.to_string_lossy().into_owned(),
                )]),
                description: "Dial-in fixture".to_owned(),
                enabled: true,
            }],
            stop: stop.clone(),
            ..Default::default()
        },
    ));
    let settings = crate::runtimes::runtime_settings(&acp.fixture.config, runtime_id);
    template
        .connect(
            cookie_agent_runtime_api::RuntimeConnection::new(Box::new(daemon)),
            &settings,
            Some(Arc::clone(&engine.inner.mcp_hub)),
        )
        .await
        .expect("dial-in runtime connected");
    let id = cookie_agent_protocol::AgentId::new(agent).unwrap();
    let runtime = cookie_agent_protocol::RuntimeId::new(runtime_id).unwrap();
    with_watchdog("the dial-in agent to register", async {
        while !engine
            .current_runtime()
            .result
            .snapshot
            .agents
            .iter()
            .any(|descriptor| {
                descriptor.id == id
                    && descriptor
                        .runtimes
                        .iter()
                        .any(|offer| offer.runtime_id == runtime && offer.available)
            })
        {
            engine.refresh_runtime_agents();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    (stop, served)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_two_runtimes_offer_runs_on_the_one_the_session_attaches() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let (stop, served) = dial_in(&acp, "labpc", "fixture").await;
    let descriptor = engine
        .current_runtime()
        .result
        .snapshot
        .agents
        .iter()
        .find(|agent| agent.id == agent_id())
        .cloned()
        .unwrap();
    assert_eq!(descriptor.runtime, None, "two runtimes offer it");
    assert_eq!(
        descriptor
            .runtimes
            .iter()
            .map(|offer| offer.runtime_id.as_str())
            .collect::<Vec<_>>(),
        ["labpc", "local"]
    );

    // The default session attaches `local`: it runs there, and
    // `list_agents` says so.
    let session = engine.create_session(selection()).unwrap().session_id;
    let listed = final_text(&turn(engine, session, "list", "mcp call list_agents {}").await);
    // The first offering runtime with a description gives it.
    assert!(
        listed.contains("fixture (all, acp) — Dial-in fixture"),
        "{listed}"
    );
    assert!(
        listed.contains("offered by runtimes: labpc, local"),
        "{listed}"
    );
    assert!(
        listed.contains("runs here: yes, on runtime local"),
        "{listed}"
    );
    assert_eq!(
        session_starts(engine, session)
            .pop()
            .unwrap()
            .runtime_id
            .as_str(),
        "local"
    );
    // `list_runtimes` names the ACP agents each runtime offers.
    let runtimes = engine.list_runtimes_text(session).unwrap();
    for id in ["local", "labpc"] {
        let line = runtimes
            .lines()
            .find(|line| {
                line.trim_start()
                    .starts_with(&format!("<runtime id=\"{id}\""))
            })
            .unwrap_or_else(|| panic!("{id}: {runtimes}"));
        assert!(line.ends_with(" acp_agents=\"fixture\">"), "{line}");
    }

    // A second runtime that offers it cannot be attached.
    let elsewhere = private_tempdir();
    let refused = engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(elsewhere.path().to_owned()),
            origin(),
        )
        .await
        .expect_err("a second offering runtime is refused");
    assert!(
        refused.to_string().contains(
            "runtime `labpc` offers ACP agent `fixture`, which this session runs on runtime `local`"
        ),
        "{refused}"
    );

    // Moved to `labpc`, the next turn starts a new ACP session there.
    let local = engine
        .session_runtimes(session)
        .unwrap()
        .into_iter()
        .find(|attachment| attachment.runtime_id.as_str() == "local")
        .unwrap();
    engine
        .detach_runtime(session, local.name, origin())
        .await
        .unwrap();
    engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(elsewhere.path().to_owned()),
            origin(),
        )
        .await
        .unwrap();
    assert_eq!(
        final_text(&turn(engine, session, "moved", "say two").await),
        "two"
    );
    let record = session_starts(engine, session).pop().unwrap();
    assert_eq!(record.runtime_id.as_str(), "labpc");
    assert_eq!(record.how, AcpSessionStart::New);
    assert!(events(engine, session).iter().any(|event| matches!(
        &event.payload,
        EventPayload::Diagnostic { message, .. }
            if message.contains("earlier session is on runtime `local`")
    )));

    // With none of them attached, or with both, it cannot run.
    let none = engine
        .create_session_with_permissions(
            selection(),
            None,
            Vec::new(),
            crate::SessionRuntimes {
                runtimes: Some(Vec::new()),
                global: true,
                cwd: None,
            },
        )
        .await
        .expect_err("no offering runtime attached");
    assert!(
        none.to_string()
            .contains("`fixture` needs a runtime that offers it attached: labpc, local"),
        "{none}"
    );
    let both = engine
        .create_session_with_permissions(
            selection(),
            None,
            Vec::new(),
            crate::SessionRuntimes {
                runtimes: Some(vec![
                    crate::SessionRuntimeRequest {
                        runtime_id: "local".to_owned(),
                        capability: Some("coding".to_owned()),
                        workspace: Some(acp.workspace()),
                    },
                    crate::SessionRuntimeRequest {
                        runtime_id: "labpc".to_owned(),
                        capability: None,
                        workspace: Some(elsewhere.path().to_owned()),
                    },
                ]),
                global: true,
                cwd: None,
            },
        )
        .await
        .expect_err("two offering runtimes attached");
    assert!(
        both.to_string()
            .contains("`fixture` is offered by more than one attached runtime (labpc, local)"),
        "{both}"
    );
    stop.cancel();
    let _ = served.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_document_without_an_acp_block_shadows_a_declaration() {
    let document = "---\ndescription: The daemon's own fixture\nmode: primary\nmodels: [{ model: \"custom.test/a-model\" }]\n---\nHello.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let descriptor = engine
        .current_runtime()
        .result
        .snapshot
        .agents
        .iter()
        .find(|agent| agent.id == agent_id())
        .cloned()
        .expect("the daemon agent");
    assert_eq!(descriptor.description, "The daemon's own fixture");
    assert!(descriptor.runtimes.is_empty());
    assert_eq!(
        engine.configuration_status().diagnostics,
        [
            "ACP agent `fixture` of runtime `local` is ignored: the agent document `fixture` has \
          no `acp` block, so `fixture` is a daemon agent; give the document an `acp: {}` block \
          to make it the ACP agent's"
        ]
    );
}

/// Whether process `pid` is still running (a zombie is not).
#[cfg(unix)]
fn running(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_none_or(|(_, rest)| !rest.starts_with('Z')),
        Err(_) if Path::new("/proc/self/stat").exists() => false,
        Err(_) => std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success()),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_engine_shutdown_leaves_no_agent_process_behind() {
    let markers = private_tempdir();
    let agent = markers.path().join("agent.pid");
    let child = markers.path().join("child.pid");
    let acp = acp_fixture(
        &[
            ("ACP_FIXTURE_PID_FILE", &agent.to_string_lossy()),
            ("ACP_FIXTURE_CHILD_PID_FILE", &child.to_string_lossy()),
        ],
        &[],
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    assert_eq!(
        final_text(&turn(engine, session, "hi", "say hi").await),
        "hi"
    );
    let pids = [agent, child].map(|file| std::fs::read_to_string(file).expect("a pid"));
    for pid in &pids {
        assert!(running(pid), "process {pid} runs before the shutdown");
    }
    // The daemon's SIGTERM, its shutdown, and `daemon.restart` all end here.
    engine.shutdown().await;
    // The agent is reaped before the shutdown returns; its child, killed
    // with its process group, is gone a moment later.
    assert!(!running(&pids[0]), "the agent outlived the shutdown");
    with_watchdog("the agent's processes to be gone", async {
        while pids.iter().any(|pid| running(pid)) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_while_a_steer_is_being_taken_still_cancels_the_agents_turn() {
    let acp = acp_fixture(&[("ACP_FIXTURE_STEERING", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let run = start(engine, session, "wait", "steer-wait").await;
    await_event(engine, session, "the agent waiting", |event| {
        matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    // The steer wakes the round, which stops while it takes the input; the
    // run is cancelled meanwhile.
    let (reached, release) = engine.install_prompt_before_claim_hook_for_test();
    engine.steer(run, "go on".into(), origin()).await.unwrap();
    with_watchdog("the round taking the steer", async {
        reached.await.expect("the round reached the claim");
    })
    .await;
    engine.cancel_run(run, origin()).await.expect("cancel");
    release.notify_one();
    let outcome = finished(engine, session, run).await;
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    let received = acp.received();
    assert!(
        received
            .iter()
            .any(|message| message["method"] == "session/cancel"),
        "the cancel reached the agent"
    );
    assert!(
        !received
            .iter()
            .any(|message| message["method"] == "_session/steering"),
        "no steer went out once the run was cancelled"
    );
}

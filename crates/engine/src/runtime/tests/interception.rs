//! Engine flows through more than one interception plugin: a blocking
//! `tool_before_call` ends the chain, and the other hooks hand each plugin
//! what the plugins before it produced.

use std::{fs, path::Path, sync::Arc};

use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, RunSelection, RunStartParams, SessionId,
    ToolTerminationOutcome,
};

use super::support::*;

const WRITE_ALLOWED_AGENT: &str = "---\ndescription: Interception chain test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nTest interception chains.\n";

fn intercepting(hook: &str) -> String {
    format!(
        r#"{{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["{hook}"]}}"#
    )
}

/// A plugin intercepting `hook` that answers with `result` and, when given,
/// records each interception request it receives to `record`.
fn chained_plugin(
    name: &str,
    hook: &str,
    result_env: &str,
    result: &str,
    record: Option<&Path>,
) -> (String, PluginConfig) {
    let mut env = vec![
        ("FIXTURE_CAPABILITIES", intercepting(hook)),
        (result_env, result.to_owned()),
    ];
    if let Some(record) = record {
        env.push(("FIXTURE_INTERCEPT_FILE", record.display().to_string()));
    }
    (name.to_owned(), interception_plugin(name, &env))
}

/// The params of the single interception request recorded to `path`.
fn recorded_params(path: &Path) -> serde_json::Value {
    let recorded = fs::read_to_string(path).expect("recorded interception");
    let mut lines = recorded.lines();
    let line = lines.next().expect("one recorded interception");
    assert!(lines.next().is_none(), "the hook ran more than once");
    serde_json::from_str::<serde_json::Value>(line).expect("interception JSON")["params"].clone()
}

async fn start(fixture: &Fixture, selection: RunSelection, client_run_id: &str) -> SessionId {
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new(client_run_id).expect("run ID"),
                selection,
                input: "run the chain".into(),
            },
            EventOrigin::new("client:test").expect("origin"),
        )
        .await
        .expect("run");
    session.session_id
}

fn write_termination(
    fixture: &Fixture,
    session_id: SessionId,
) -> cookie_agent_protocol::ToolCallTermination {
    fixture
        .engine
        .inner
        .store
        .get(session_id)
        .expect("projection")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination),
            _ => None,
        })
        .expect("write tool termination")
}

#[tokio::test]
async fn blocking_tool_before_call_hook_skips_later_hooks() {
    let (endpoint, _) = scripted_zero_resource_tool_server().await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, WRITE_ALLOWED_AGENT);
    let markers = tempfile::tempdir().expect("hook markers");
    let blocker_file = markers.path().join("blocker.jsonl");
    let later_file = markers.path().join("later.jsonl");
    reopen_with_interception_plugins(
        &mut fixture,
        vec![
            chained_plugin(
                "blocker",
                "tool_before_call",
                "FIXTURE_TOOL_BEFORE_RESULT",
                r#"{"action":"block","message_to_model":"blocked by the first hook"}"#,
                Some(&blocker_file),
            ),
            chained_plugin(
                "later",
                "tool_before_call",
                "FIXTURE_TOOL_BEFORE_RESULT",
                r#"{"action":"allow"}"#,
                Some(&later_file),
            ),
        ],
    )
    .await;
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session_id = start(&fixture, selection, "blocked-hook-chain").await;
    wait_for_session_not_running(&fixture.engine, session_id).await;

    assert_eq!(recorded_params(&blocker_file)["tool"], "write");
    assert!(!later_file.exists(), "a hook after the block still ran");
    assert!(!executed.is_set(), "the blocked tool executed");
    let termination = write_termination(&fixture, session_id);
    assert_eq!(termination.outcome, ToolTerminationOutcome::Failed);
    assert_eq!(
        termination.error.expect("block error").message.as_str(),
        "blocked by the first hook"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn tool_after_result_hooks_rewrite_in_registry_order() {
    let (endpoint, _) = scripted_zero_resource_tool_server().await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(&endpoint, WRITE_ALLOWED_AGENT);
    let markers = tempfile::tempdir().expect("hook markers");
    let second_file = markers.path().join("second.jsonl");
    reopen_with_interception_plugins(
        &mut fixture,
        vec![
            chained_plugin(
                "first",
                "tool_after_result",
                "FIXTURE_TOOL_AFTER_RESULT",
                r#"{"action":"replace","replacement_content":"first rewrite"}"#,
                None,
            ),
            chained_plugin(
                "second",
                "tool_after_result",
                "FIXTURE_TOOL_AFTER_RESULT",
                r#"{"action":"replace","replacement_content":"second rewrite"}"#,
                Some(&second_file),
            ),
        ],
    )
    .await;
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session_id = start(&fixture, selection, "after-result-chain").await;
    wait_for_tool_execution(&fixture.engine, session_id, &executed).await;
    wait_for_session_not_running(&fixture.engine, session_id).await;

    let second = recorded_params(&second_file);
    assert_eq!(second["result_content"], "first rewrite");
    assert_eq!(second["is_error"], false);
    let termination = write_termination(&fixture, session_id);
    assert_eq!(termination.outcome, ToolTerminationOutcome::Completed);
    assert_eq!(
        termination.result.expect("write result").output,
        "second rewrite"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn agent_before_start_hooks_see_earlier_addenda() {
    let (endpoint, captured) = scripted_model_server().await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let markers = tempfile::tempdir().expect("hook markers");
    let second_file = markers.path().join("second.jsonl");
    reopen_with_interception_plugins(
        &mut fixture,
        vec![
            chained_plugin(
                "first",
                "agent_before_start",
                "FIXTURE_AGENT_BEFORE_RESULT",
                r#"{"append_to_system_prompt":"First plugin addendum."}"#,
                None,
            ),
            chained_plugin(
                "second",
                "agent_before_start",
                "FIXTURE_AGENT_BEFORE_RESULT",
                r#"{"append_to_system_prompt":"Second plugin addendum."}"#,
                Some(&second_file),
            ),
        ],
    )
    .await;
    let session_id = start(&fixture, selection, "agent-start-chain").await;
    wait_for_session_not_running(&fixture.engine, session_id).await;

    let second = recorded_params(&second_file);
    let seen = second["prompt_context"]["system_prompt"]
        .as_str()
        .expect("system prompt seen by the second hook");
    assert!(seen.contains("First plugin addendum."));
    assert!(!seen.contains("Second plugin addendum."));
    let projection = fixture.engine.inner.store.get(session_id).expect("run");
    let prompt = &projection
        .runs
        .values()
        .next()
        .expect("started run")
        .agent
        .composed_prompt;
    let first = prompt
        .find("First plugin addendum.")
        .expect("first addendum");
    let second = prompt
        .find("Second plugin addendum.")
        .expect("second addendum");
    assert!(first < second);
    let request = with_watchdog("captured fixture completion", captured)
        .await
        .expect("captured request");
    assert!(request.contains("First plugin addendum."));
    assert!(request.contains("Second plugin addendum."));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn session_before_compact_hooks_see_earlier_additions() {
    let (endpoint, captured) = scripted_model_server().await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let markers = tempfile::tempdir().expect("hook markers");
    let second_file = markers.path().join("second.jsonl");
    reopen_with_interception_plugins(
        &mut fixture,
        vec![
            chained_plugin(
                "first",
                "session_before_compact",
                "FIXTURE_COMPACT_BEFORE_RESULT",
                r#"{"instructions_override":"first focus","addendum":"first compaction note"}"#,
                None,
            ),
            chained_plugin(
                "second",
                "session_before_compact",
                "FIXTURE_COMPACT_BEFORE_RESULT",
                r#"{"cancel":true,"reason":"second hook saw the first"}"#,
                Some(&second_file),
            ),
        ],
    )
    .await;
    let session_id = start(&fixture, selection, "compact-chain").await;
    wait_for_session_not_running(&fixture.engine, session_id).await;
    with_watchdog("captured fixture completion", captured)
        .await
        .expect("captured request");

    let result = fixture
        .engine
        .compact_session_result(
            session_id,
            Some("caller focus"),
            EventOrigin::new("client:test").expect("origin"),
        )
        .await
        .expect("compaction result");
    assert!(!result.compacted);
    assert_eq!(
        result.cancellation_reason.as_deref(),
        Some("second hook saw the first")
    );
    let second = recorded_params(&second_file);
    assert_eq!(
        second["additions"],
        serde_json::json!(["first compaction note"])
    );
    assert_eq!(second["instructions"], "first focus\nfirst compaction note");
    fixture.engine.shutdown().await;
}

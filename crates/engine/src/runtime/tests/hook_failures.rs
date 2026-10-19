//! A hook whose plugin declares `on_error = block` blocks or refuses the
//! operation it guards when its call times out, the plugin exits, the plugin
//! answers with an error, or the answer is malformed; with `allow` the
//! operation goes on. Either way the session records a diagnostic of the
//! failure's kind.

use std::sync::Arc;

use cookie_agent_config::{ModelRetryConfig, PluginConfig};
use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, ExtensionInterceptionHook, PluginDiagnosticKind,
    RunSelection, RunStartParams, SessionId, SessionStatus, ToolTerminationOutcome,
};

use super::support::*;
use crate::{EngineError, runtime::ModelRetrySleepMode};

const WRITE_ALLOWED_AGENT: &str = "---\ndescription: Hook failure test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nTest hook failures.\n";

#[derive(Clone, Copy, Debug)]
enum Failure {
    Timeout,
    Crash,
    Rejected,
    Malformed,
}

const FAILURES: [Failure; 4] = [
    Failure::Timeout,
    Failure::Crash,
    Failure::Rejected,
    Failure::Malformed,
];

impl Failure {
    fn kind(self) -> PluginDiagnosticKind {
        match self {
            Self::Timeout => PluginDiagnosticKind::InterceptionTimeout,
            Self::Crash => PluginDiagnosticKind::InterceptionCrash,
            Self::Rejected | Self::Malformed => PluginDiagnosticKind::InterceptionRejected,
        }
    }
}

/// How a hook under test is set up: its name, the fixture variable of its
/// result, a result of the hook that does not decode, and the params a call
/// must have to fail (other calls are answered normally).
struct Hook {
    hook: ExtensionInterceptionHook,
    result_env: &'static str,
    malformed: &'static str,
    fail_when: Option<&'static str>,
}

impl Hook {
    const fn new(
        hook: ExtensionInterceptionHook,
        result_env: &'static str,
        malformed: &'static str,
    ) -> Self {
        Self {
            hook,
            result_env,
            malformed,
            fail_when: None,
        }
    }

    /// The `guard` plugin, intercepting the hook with `on_error`, whose
    /// calls of it fail with `failure`.
    fn plugin(&self, failure: Failure, block: bool) -> Vec<(String, PluginConfig)> {
        let capabilities = serde_json::json!({
            "producer_messaging": false, "subscribe_events": [],
            "subscribe_bus": false, "publish_bus": false,
            "intercept": [{
                "hook": self.hook.name(),
                "on_error": if block { "block" } else { "allow" },
            }],
        });
        let mut env = vec![("FIXTURE_CAPABILITIES", capabilities.to_string())];
        match failure {
            Failure::Timeout => env.push(("FIXTURE_INTERCEPT_DELAY_MS", "3000".into())),
            Failure::Crash => env.push(("FIXTURE_CRASH_DURING_INTERCEPT", "1".into())),
            Failure::Rejected => env.push(("FIXTURE_INTERCEPT_ERROR", "policy store down".into())),
            Failure::Malformed => env.push((self.result_env, self.malformed.into())),
        }
        if let Some(fail_when) = self.fail_when {
            env.push(("FIXTURE_FAIL_WHEN", fail_when.into()));
        }
        let mut config = interception_plugin("guard", &env);
        config.interception_timeout_ms = 500;
        config.shutdown_grace_ms = 50;
        vec![("guard".into(), config)]
    }

    /// Checks that `error` is this hook's failure for `guard`.
    fn assert_refused<T: std::fmt::Debug>(&self, result: Result<T, EngineError>, failure: Failure) {
        match result {
            Err(EngineError::PluginHookFailed(refusal)) => {
                assert_eq!(refusal.plugin, "guard");
                assert_eq!(refusal.hook, self.hook);
            }
            other => panic!("{failure:?} of {} did not refuse: {other:?}", self.hook),
        }
    }

    /// Waits for the diagnostic of `failure`, saying whether it blocked.
    async fn assert_diagnostic(
        &self,
        fixture: &Fixture,
        session: SessionId,
        failure: Failure,
        block: bool,
    ) {
        let hook = self.hook.name();
        let outcome = if block {
            "so the operation was blocked"
        } else {
            "and the operation went on"
        };
        await_projection(
            &fixture.engine,
            session,
            "hook failure diagnostic",
            |projection| {
                projection.log.events().iter().any(|event| {
                    matches!(
                        &event.payload,
                        EventPayload::Diagnostic { source, kind, message, .. }
                            if source.plugin_name() == Some("guard")
                                && *kind == failure.kind()
                                && message.starts_with(&format!("the {hook} hook failed, {outcome}"))
                    )
                })
            },
        )
        .await;
    }
}

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").expect("origin")
}

async fn start(
    fixture: &Fixture,
    selection: &RunSelection,
    name: &str,
) -> (
    SessionId,
    Result<cookie_agent_protocol::RunStartResult, EngineError>,
) {
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let started = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new(name).expect("run ID"),
                selection: selection.clone(),
                content: "check this".into(),
            },
            origin(),
        )
        .await;
    (session.session_id, started)
}

fn events(fixture: &Fixture, session: SessionId) -> Vec<EventPayload> {
    fixture
        .engine
        .inner
        .store
        .get(session)
        .expect("projection")
        .log
        .events()
        .into_iter()
        .map(|event| event.payload)
        .collect()
}

fn run_error(fixture: &Fixture, session: SessionId) -> Option<String> {
    events(fixture, session)
        .into_iter()
        .find_map(|payload| match payload {
            EventPayload::RunFinished { outcome } => match outcome.into_known()? {
                cookie_agent_protocol::RunOutcome::Failed { error, .. } => {
                    Some(error.as_str().to_owned())
                }
                _ => None,
            },
            _ => None,
        })
}

fn status(fixture: &Fixture, session: SessionId) -> SessionStatus {
    fixture
        .engine
        .inner
        .store
        .get(session)
        .expect("projection")
        .status
}

/// Hooks that guard a run's admission: a blocking failure refuses
/// `run.start`, and nothing of the run is recorded.
async fn assert_admission_hook(hook: Hook) {
    for failure in FAILURES {
        for block in [true, false] {
            let (endpoint, _captured) = scripted_model_server().await;
            let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
            reopen_with_interception_plugins(&mut fixture, hook.plugin(failure, block)).await;
            let (session, started) = start(&fixture, &selection, "admission").await;
            if block {
                hook.assert_refused(started, failure);
                assert!(
                    !events(&fixture, session)
                        .iter()
                        .any(|payload| matches!(payload, EventPayload::RunStarted { .. })),
                    "{failure:?} of {} started a run",
                    hook.hook
                );
            } else {
                started.expect("run starts");
                wait_for_session_not_running(&fixture.engine, session).await;
                assert_eq!(status(&fixture, session), SessionStatus::Completed);
            }
            hook.assert_diagnostic(&fixture, session, failure, block)
                .await;
            fixture.engine.shutdown().await;
        }
    }
}

#[tokio::test]
async fn user_before_input_failures_refuse_the_input_when_the_plugin_blocks() {
    assert_admission_hook(Hook::new(
        ExtensionInterceptionHook::UserBeforeInput,
        "FIXTURE_USER_BEFORE_INPUT_RESULT",
        r#"{"action":"explode"}"#,
    ))
    .await;
}

#[tokio::test]
async fn agent_before_start_failures_refuse_the_run_when_the_plugin_blocks() {
    assert_admission_hook(Hook::new(
        ExtensionInterceptionHook::AgentBeforeStart,
        "FIXTURE_AGENT_BEFORE_RESULT",
        r#""not a result""#,
    ))
    .await;
}

#[tokio::test]
async fn model_before_select_failures_refuse_the_selection_when_the_plugin_blocks() {
    assert_admission_hook(Hook::new(
        ExtensionInterceptionHook::ModelBeforeSelect,
        "FIXTURE_MODEL_BEFORE_SELECT_RESULT",
        r#"{"action":"explode"}"#,
    ))
    .await;
}

#[tokio::test]
async fn model_before_select_failures_refuse_a_fallback_when_the_plugin_blocks() {
    let hook = Hook {
        fail_when: Some(r#"{"source":"fallback"}"#),
        ..Hook::new(
            ExtensionInterceptionHook::ModelBeforeSelect,
            "FIXTURE_MODEL_BEFORE_SELECT_RESULT",
            r#"{"action":"explode"}"#,
        )
    };
    let retry = ModelRetryConfig {
        standard_retries: 0,
        overload_retries: 0,
        backoff_ceiling_ms: 1,
    };
    for failure in FAILURES {
        for block in [true, false] {
            let (endpoint, _captured) = retry_model_server(vec![
                RetryModelResponse::Status(500),
                RetryModelResponse::Success,
            ])
            .await;
            let (mut fixture, selection) = retry_fixture_with_endpoint(&endpoint, retry).await;
            reopen_with_interception_plugins(&mut fixture, hook.plugin(failure, block)).await;
            fixture
                .engine
                .inner
                .test_hooks
                .model_retry_sleep_hook
                .set_mode(ModelRetrySleepMode::Immediate);
            let (session, started) = start(&fixture, &selection, "fallback").await;
            started.expect("run starts");
            wait_for_session_not_running(&fixture.engine, session).await;
            let fell_back = events(&fixture, session)
                .iter()
                .any(|payload| matches!(payload, EventPayload::ModelFallback { .. }));
            if block {
                assert!(!fell_back, "{failure:?} fell back");
                assert_eq!(status(&fixture, session), SessionStatus::Failed);
                let error = run_error(&fixture, session).expect("run failure");
                assert!(error.contains("`model_before_select` hook"), "{error}");
            } else {
                assert!(fell_back, "{failure:?} did not fall back");
                assert_eq!(status(&fixture, session), SessionStatus::Completed);
            }
            hook.assert_diagnostic(&fixture, session, failure, block)
                .await;
            fixture.engine.shutdown().await;
        }
    }
}

/// Hooks that guard a model attempt: a blocking failure abandons the
/// attempt without a model error and fails the run.
async fn assert_attempt_hook(hook: Hook) {
    for failure in FAILURES {
        for block in [true, false] {
            let (endpoint, _captured) = scripted_model_server().await;
            let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
            reopen_with_interception_plugins(&mut fixture, hook.plugin(failure, block)).await;
            let (session, started) = start(&fixture, &selection, "attempt").await;
            started.expect("run starts");
            wait_for_session_not_running(&fixture.engine, session).await;
            let payloads = events(&fixture, session);
            if block {
                assert_eq!(status(&fixture, session), SessionStatus::Failed);
                assert!(payloads.iter().any(|payload| matches!(
                    payload,
                    EventPayload::AttemptAbandoned {
                        model_error: None,
                        ..
                    }
                )));
                assert!(
                    !payloads
                        .iter()
                        .any(|payload| matches!(payload, EventPayload::ModelTurnCommitted { .. })),
                    "{failure:?} of {} committed a turn",
                    hook.hook
                );
                let error = run_error(&fixture, session).expect("run failure");
                assert!(
                    error.contains(&format!(
                        "plugin `guard` could not check this: its `{}` hook",
                        hook.hook
                    )),
                    "{error}"
                );
            } else {
                assert_eq!(status(&fixture, session), SessionStatus::Completed);
            }
            hook.assert_diagnostic(&fixture, session, failure, block)
                .await;
            fixture.engine.shutdown().await;
        }
    }
}

#[tokio::test]
async fn model_before_request_failures_fail_the_attempt_when_the_plugin_blocks() {
    assert_attempt_hook(Hook::new(
        ExtensionInterceptionHook::ModelBeforeRequest,
        "FIXTURE_MODEL_BEFORE_REQUEST_RESULT",
        r#"{"action":"explode"}"#,
    ))
    .await;
}

#[tokio::test]
async fn message_end_failures_fail_the_attempt_when_the_plugin_blocks() {
    assert_attempt_hook(Hook::new(
        ExtensionInterceptionHook::MessageEnd,
        "FIXTURE_MESSAGE_END_RESULT",
        r#"{"action":"explode"}"#,
    ))
    .await;
}

/// Hooks around a tool call: a blocking failure fails the call with a
/// message naming the plugin, after the call ran (`after`) or instead of it.
async fn assert_tool_hook(hook: Hook, after: bool) {
    for failure in FAILURES {
        for block in [true, false] {
            let (endpoint, _captured) = scripted_zero_resource_tool_server().await;
            let (mut fixture, selection) =
                custom_fixture_with_endpoint_and_primary_agent(&endpoint, WRITE_ALLOWED_AGENT);
            reopen_with_interception_plugins(&mut fixture, hook.plugin(failure, block)).await;
            let executed = Arc::new(TestFlag::default());
            fixture.engine.register_tool_provider(
                crate::ToolLayer::Local,
                Arc::new(TestWriteProvider {
                    executed: Arc::clone(&executed),
                }),
            );
            let (session, started) = start(&fixture, &selection, "tool").await;
            started.expect("run starts");
            wait_for_session_not_running(&fixture.engine, session).await;
            let termination = events(&fixture, session)
                .into_iter()
                .find_map(|payload| match payload {
                    EventPayload::ToolCallTerminated { termination } => Some(termination),
                    _ => None,
                })
                .expect("write termination");
            if block {
                assert_eq!(termination.outcome, ToolTerminationOutcome::Failed);
                assert_eq!(executed.is_set(), after, "{failure:?}");
                let message = termination.error.expect("blocked call error").message;
                let expected = if after {
                    "Tool result withheld: plugin `guard` could not check this"
                } else {
                    "Tool call blocked: plugin `guard` could not check this"
                };
                assert!(message.as_str().starts_with(expected), "{message:?}");
            } else {
                assert_eq!(termination.outcome, ToolTerminationOutcome::Completed);
                assert!(executed.is_set());
            }
            hook.assert_diagnostic(&fixture, session, failure, block)
                .await;
            fixture.engine.shutdown().await;
        }
    }
}

#[tokio::test]
async fn tool_before_call_failures_block_the_call_when_the_plugin_blocks() {
    assert_tool_hook(
        Hook::new(
            ExtensionInterceptionHook::ToolBeforeCall,
            "FIXTURE_TOOL_BEFORE_RESULT",
            r#"{"action":"explode"}"#,
        ),
        false,
    )
    .await;
}

#[tokio::test]
async fn tool_after_result_failures_withhold_the_result_when_the_plugin_blocks() {
    assert_tool_hook(
        Hook::new(
            ExtensionInterceptionHook::ToolAfterResult,
            "FIXTURE_TOOL_AFTER_RESULT",
            r#"{"action":"explode"}"#,
        ),
        true,
    )
    .await;
}

/// What a session hook guards, run on a session that completed a run.
#[derive(Clone, Copy)]
enum SessionOperation {
    Compact,
    Fork,
    Revert,
}

async fn assert_session_hook(hook: Hook, operation: SessionOperation) {
    for failure in FAILURES {
        for block in [true, false] {
            // The run, then the compaction's summary.
            let (endpoint, _captured) = retry_model_server(vec![
                RetryModelResponse::Success,
                RetryModelResponse::Success,
            ])
            .await;
            let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
            reopen_with_interception_plugins(&mut fixture, hook.plugin(failure, block)).await;
            let (session, started) = start(&fixture, &selection, "session").await;
            started.expect("run starts");
            wait_for_session_not_running(&fixture.engine, session).await;
            let last_seq = fixture
                .engine
                .inner
                .store
                .get(session)
                .expect("projection")
                .meta
                .last_event_seq;
            let refused = match operation {
                SessionOperation::Compact => fixture
                    .engine
                    .compact_session_result(session, None, origin())
                    .await
                    .map(drop),
                SessionOperation::Fork => fixture
                    .engine
                    .fork_session(session, last_seq, origin(), None)
                    .await
                    .map(drop),
                SessionOperation::Revert => fixture
                    .engine
                    .revert_session(session, 1, origin(), None)
                    .await
                    .map(drop),
            };
            if block {
                hook.assert_refused(refused, failure);
            } else {
                refused.expect("the operation goes on");
            }
            hook.assert_diagnostic(&fixture, session, failure, block)
                .await;
            fixture.engine.shutdown().await;
        }
    }
}

#[tokio::test]
async fn session_before_compact_failures_refuse_compaction_when_the_plugin_blocks() {
    assert_session_hook(
        Hook::new(
            ExtensionInterceptionHook::SessionBeforeCompact,
            "FIXTURE_COMPACT_BEFORE_RESULT",
            r#""not a result""#,
        ),
        SessionOperation::Compact,
    )
    .await;
}

#[tokio::test]
async fn session_before_fork_failures_refuse_the_fork_when_the_plugin_blocks() {
    assert_session_hook(
        Hook::new(
            ExtensionInterceptionHook::SessionBeforeFork,
            "FIXTURE_SESSION_BEFORE_FORK_RESULT",
            r#"{"action":"explode"}"#,
        ),
        SessionOperation::Fork,
    )
    .await;
}

#[tokio::test]
async fn session_before_revert_failures_refuse_the_revert_when_the_plugin_blocks() {
    assert_session_hook(
        Hook::new(
            ExtensionInterceptionHook::SessionBeforeRevert,
            "FIXTURE_SESSION_BEFORE_REVERT_RESULT",
            r#"{"action":"explode"}"#,
        ),
        SessionOperation::Revert,
    )
    .await;
}

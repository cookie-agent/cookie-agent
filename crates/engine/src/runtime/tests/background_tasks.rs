//! Background calls (`docs/site/specs/background-tools.md`): detaching at the
//! detach point, the immediate and background results, stopping, and kills
//! when the call is reverted.

use std::sync::Arc;

use async_trait::async_trait;

use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, ClientRunId, EventPayload,
    PermissionAction, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, ProducerOwner, RunStartParams, SafeDisplayText, SessionId,
    Sha256Digest, ToolCallId,
};

use crate::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProgress, ToolProvider, ToolSpec,
};

use super::support::*;

/// Shared switches between a test and the calls its provider runs.
#[derive(Default)]
pub(super) struct JobControl {
    /// The call reached its detach point.
    pub(super) detachable: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
    killed: tokio::sync::Notify,
    was_killed: std::sync::atomic::AtomicBool,
}

/// Flags the call as killed when its execution is dropped unfinished.
struct KillFlag(Option<Arc<JobControl>>);

impl Drop for KillFlag {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            control
                .was_killed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            control.killed.notify_one();
        }
    }
}

/// A background-capable `job` tool. `command` picks what a call does:
/// `quick` returns before its detach point, anything else reaches it and
/// runs until released or cancelled.
#[derive(Clone)]
pub(super) struct JobProvider {
    pub(super) control: Arc<JobControl>,
}

struct JobExecutor {
    call_id: ToolCallId,
    mode: String,
    control: Arc<JobControl>,
}

#[async_trait]
impl ToolProvider for JobProvider {
    fn provider_id(&self) -> &'static str {
        "test.job"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            background: true,
            background_default: false,
            collision: Default::default(),
            output: cookie_agent_protocol::ToolOutputDeclaration::Named {
                streams: vec!["stdout".into(), "stderr".into()],
            },
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "job".into(),
            permission_name: Some("bash".into()),
            description: "Run a job".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{"command":{"type":"string"}},
                "required":["command"]
            }),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        match tool_name {
            "job" => Ok(Some("bash")),
            _ => Err(ToolError::execution("job provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        let command = arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing command"))?;
        Ok((Self::get_permission_name(name)?, Some(command.into())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_permission_resource(name, arguments)?
            .1
            .ok_or_else(|| ToolError::execution("missing command"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        if call.arguments.get("background").is_some() {
            return Err(ToolError::execution("the host owns `background`"));
        }
        let command = call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing command"))?
            .to_owned();
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(command.as_bytes()),
            vec![ApprovalCapability {
                action: PermissionAction::BASH,
                operation: PreparedCapabilityOperation::new("bash:execute")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::BASH,
                canonical: PreparedResourceIdentity::new(format!(
                    "command:{}",
                    Sha256Digest::of_bytes(command.as_bytes())
                ))
                .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    command.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"background job context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(JobExecutor {
                call_id: call.id,
                mode: command.clone(),
                control: Arc::clone(&self.control),
            }),
        )?
        .with_policy_labels(vec![command])
    }
}

#[async_trait]
impl PreparedExecutor for JobExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let mut flag = KillFlag(Some(Arc::clone(&self.control)));
        context
            .progress
            .send(ToolProgress {
                tool_call_id: self.call_id,
                message: String::new(),
                display: None,
                output: vec![cookie_agent_protocol::ToolOutputChunk {
                    stream: Some("stdout".into()),
                    text: "job output line\n".into(),
                }],
            })
            .await?;
        if self.mode != "quick" {
            context.detachable(Some("Process 7".into()));
            self.control.detachable.notify_one();
            tokio::select! {
                () = self.control.release.notified() => {}
                () = context.cancellation.cancelled() => {
                    flag.0 = None;
                    return Err(ToolError::execution("job cancelled"));
                }
            }
        }
        flag.0 = None;
        Ok(crate::ToolCompletion::streamed(
            cookie_agent_protocol::PersistedToolResult {
                title: SafeDisplayText::new("Job").expect("title"),
                output: String::new(),
                display: Vec::new(),
                background: None,
                retained_output: None,
                metadata: serde_json::json!({"status": 0}),
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            },
        ))
    }
}

const JOB_AGENT: &str = "---\ndescription: Background call test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: allow\n---\nTest background calls.\n";

pub(super) struct JobRun {
    pub(super) fixture: Fixture,
    pub(super) session_id: SessionId,
    pub(super) run_id: cookie_agent_protocol::RunId,
    pub(super) control: Arc<JobControl>,
    pub(super) captured: tokio::task::JoinHandle<Vec<String>>,
    /// `sessions.changed`, subscribed before the run started.
    changes: tokio::sync::broadcast::Receiver<cookie_agent_protocol::SessionsChangedNotification>,
}

/// Starts a run whose model calls `job` with `arguments`, then answers
/// `responses`.
pub(super) async fn start_job_run(
    arguments: serde_json::Value,
    responses: Vec<MatchedScriptedResponse>,
    expected_requests: usize,
) -> JobRun {
    let (endpoint, sender, captured) = scripted_channel_server(expected_requests).await;
    sender
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body("job-call", "job", arguments),
        ))
        .expect("scripted tool response");
    for response in responses {
        sender.send(response).expect("scripted response");
    }
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, JOB_AGENT);
    fixture
        .engine
        .register_base_layer()
        .expect("register the base layer");
    let control = Arc::new(JobControl::default());
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(JobProvider {
            control: Arc::clone(&control),
        }),
    );
    let session_id = fixture
        .engine
        .create_session(selection.clone())
        .expect("job session")
        .session_id;
    let changes = fixture.engine.subscribe_session_changes();
    let run_id = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id,
                client_run_id: ClientRunId::new("background-call").expect("client run id"),
                selection,
                content: "run the job".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started")
        .run_id;
    JobRun {
        fixture,
        session_id,
        run_id,
        control,
        captured,
        changes,
    }
}

fn call_id(projection: &crate::session::SessionProjection) -> ToolCallId {
    projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } => Some(start.tool_call_id),
            _ => None,
        })
        .expect("started call")
}

fn call_result(projection: &crate::session::SessionProjection) -> Option<String> {
    projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => termination
                .result
                .as_ref()
                .map(|result| result.output.clone()),
            _ => None,
        })
}

pub(super) fn background_result(
    projection: &crate::session::SessionProjection,
) -> Option<(ProducerOwner, String)> {
    projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ProducerMessageAccepted {
                producer_owner: owner @ ProducerOwner::Tool { .. },
                body,
                ..
            } => Some((owner.clone(), body.as_plain_text().into_owned())),
            _ => None,
        })
}

#[tokio::test]
async fn a_call_started_in_the_background_reports_its_foreground_result_later() {
    let run = start_job_run(
        serde_json::json!({"command": "finish", "background": true}),
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_result",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    with_watchdog("detach point", run.control.detachable.notified()).await;
    // The immediate result ends the call and the run goes on without it.
    wait_for_run_inactive(&run.fixture.engine, run.run_id).await;
    let projection = run.fixture.engine.inner.store.get(run.session_id).unwrap();
    let call = call_id(&projection);
    // Clients learn from the start event that the call can be backgrounded.
    assert!(projection.log.events().iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallStarted { start } if start.tool_call_id == call && start.background
    )));
    let job = crate::runtime::background_tasks::job_id(call);
    let immediate = call_result(&projection).expect("immediate result");
    assert!(
        immediate.starts_with(&format!("Started in the background as job {job}.")),
        "{immediate}"
    );
    assert!(immediate.ends_with("\nProcess 7"), "{immediate}");
    assert_eq!(run.fixture.engine.background_calls(run.session_id).len(), 1);

    run.control.release.notify_one();
    let projection = await_projection(
        &run.fixture.engine,
        run.session_id,
        "background result",
        |projection| background_result(projection).is_some(),
    )
    .await;
    let (owner, body) = background_result(&projection).expect("background result");
    assert_eq!(owner, ProducerOwner::Tool { tool_call_id: call });
    assert!(
        body.starts_with(&format!(
            "<background_result job=\"{job}\" tool=\"job\" status=\"completed\">\n"
        )),
        "{body}"
    );
    assert!(body.contains("job output line"), "{body}");
    assert!(body.contains("\"title\":\"Job\""), "{body}");
    assert!(body.ends_with("</background_result>"), "{body}");
    let requests = with_watchdog("woken run", run.captured)
        .await
        .expect("requests");
    // The model was offered the host's `background` argument and
    // `stop_background`, and saw the result once.
    assert!(requests[0].contains("\"background\""), "{}", requests[0]);
    assert!(requests[0].contains("stop_background"), "{}", requests[0]);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.contains(r#"status=\"completed\""#))
            .count(),
        1
    );
    assert!(
        run.fixture
            .engine
            .background_calls(run.session_id)
            .is_empty()
    );
    assert!(
        !run.control
            .was_killed
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}

/// A background call makes its session wait on it from detach until its
/// result is accepted: `sessions.changed` announces both, listings agree in
/// between, and neither the fold nor the `metadata` cache holds it.
#[tokio::test]
async fn a_background_call_is_what_its_session_waits_on_until_it_reports() {
    let mut run = start_job_run(
        serde_json::json!({"command": "finish", "background": true}),
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_result",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    let engine = run.fixture.engine.clone();
    let session_id = run.session_id;
    await_upserted(
        &mut run.changes,
        session_id,
        "waiting on the background call",
        |meta| meta.waiting_on == [cookie_agent_protocol::WaitingOn::BackgroundCall],
    )
    .await;
    // The run ends while the call goes on: the session is waiting, no
    // longer running.
    let announced = await_upserted(
        &mut run.changes,
        session_id,
        "the run ended, still waiting",
        |meta| meta.status != cookie_agent_protocol::SessionStatus::Running,
    )
    .await;
    assert_eq!(
        announced.waiting_on,
        [cookie_agent_protocol::WaitingOn::BackgroundCall]
    );
    wait_for_run_inactive(&engine, run.run_id).await;
    assert_eq!(
        engine.get_session(session_id).expect("session").waiting_on,
        [cookie_agent_protocol::WaitingOn::BackgroundCall]
    );
    assert!(
        engine
            .inner
            .store
            .get(session_id)
            .expect("projection")
            .meta
            .waiting_on
            .is_empty()
    );

    run.control.release.notify_one();
    await_upserted(
        &mut run.changes,
        session_id,
        "the background call reported",
        |meta| meta.waiting_on.is_empty(),
    )
    .await;
    assert!(background_result(&engine.inner.store.get(session_id).unwrap()).is_some());
    with_watchdog("woken run", run.captured)
        .await
        .expect("requests");
    assert!(
        engine
            .get_session(session_id)
            .expect("session")
            .waiting_on
            .is_empty()
    );
}

#[tokio::test]
async fn stop_background_is_published_only_next_to_a_background_capable_tool() {
    // The agent grants the `job` tool's `bash` permission; no request is sent.
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, JOB_AGENT);
    let policy = frozen_root_policy(&fixture, &selection);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    fixture
        .engine
        .register_base_layer()
        .expect("register the base layer");
    let published = |fixture: &Fixture| {
        fixture
            .engine
            .published_tool_definitions(session, &policy)
            .expect("published tools")
            .tools
    };
    assert!(!published(&fixture).contains_key("stop_background"));
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(JobProvider {
            control: Arc::default(),
        }),
    );
    let tools = published(&fixture);
    assert!(tools.contains_key("stop_background"));
    assert_eq!(tools["stop_background"].spec.permission_name, None);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_call_that_finishes_before_its_detach_point_returns_in_the_foreground() {
    let run = start_job_run(
        serde_json::json!({"command": "quick", "background": true}),
        vec![MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("done"),
        )],
        2,
    )
    .await;
    wait_for_run_inactive(&run.fixture.engine, run.run_id).await;
    let projection = run.fixture.engine.inner.store.get(run.session_id).unwrap();
    let result = call_result(&projection).expect("result");
    assert!(result.contains("job output line"), "{result}");
    assert!(background_result(&projection).is_none());
    assert_eq!(run.fixture.engine.inner.background.job_count(), 0);
}

#[tokio::test]
async fn the_user_moves_a_running_call_to_the_background() {
    let run = start_job_run(
        serde_json::json!({"command": "finish"}),
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_result",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    with_watchdog("detach point", run.control.detachable.notified()).await;
    let call = call_id(&run.fixture.engine.inner.store.get(run.session_id).unwrap());
    assert!(run.fixture.engine.request_detach(run.run_id, call));
    wait_for_run_inactive(&run.fixture.engine, run.run_id).await;
    let projection = run.fixture.engine.inner.store.get(run.session_id).unwrap();
    let immediate = call_result(&projection).expect("immediate result");
    assert!(
        immediate.starts_with("User manually moved this call to the background;"),
        "{immediate}"
    );
    let listed = run.fixture.engine.background_calls(run.session_id);
    assert_eq!(listed[0].by, cookie_agent_protocol::BackgroundActor::User);
    assert_eq!(listed[0].display.as_deref(), Some("finish"));
    run.control.release.notify_one();
    await_projection(
        &run.fixture.engine,
        run.session_id,
        "background result",
        |projection| background_result(projection).is_some(),
    )
    .await;
    // A call that is no longer running cannot be moved.
    assert!(!run.fixture.engine.request_detach(run.run_id, call));
}

#[tokio::test]
async fn a_user_stop_ends_the_call_and_it_still_reports() {
    let run = start_job_run(
        serde_json::json!({"command": "finish", "background": true}),
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_result",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    wait_for_run_inactive(&run.fixture.engine, run.run_id).await;
    let call = call_id(&run.fixture.engine.inner.store.get(run.session_id).unwrap());
    let job = crate::runtime::background_tasks::job_id(call);
    run.fixture
        .engine
        .stop_background_as_user(run.session_id, &job)
        .await
        .expect("stop");
    let projection = await_projection(
        &run.fixture.engine,
        run.session_id,
        "stopped result",
        |projection| background_result(projection).is_some(),
    )
    .await;
    let (_, body) = background_result(&projection).expect("background result");
    assert!(body.contains("status=\"stopped\""), "{body}");
    assert!(
        body.contains("User stopped this background call."),
        "{body}"
    );
    assert!(body.contains("job cancelled"), "{body}");
    let error = run
        .fixture
        .engine
        .stop_background_as_user(run.session_id, &job)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(&job), "{error}");
}

#[tokio::test]
async fn reverting_past_the_call_kills_its_background_call() {
    let run = start_job_run(
        serde_json::json!({"command": "finish", "background": true}),
        vec![MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("waiting"),
        )],
        2,
    )
    .await;
    wait_for_run_inactive(&run.fixture.engine, run.run_id).await;
    wait_for_session_not_running(&run.fixture.engine, run.session_id).await;
    run.fixture
        .engine
        .revert_session(
            run.session_id,
            1,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            None,
        )
        .await
        .expect("revert");
    with_watchdog("call killed", run.control.killed.notified()).await;
    settle_session_actor(&run.fixture.engine, run.session_id).await;
    let projection = run.fixture.engine.inner.store.get(run.session_id).unwrap();
    assert!(background_result(&projection).is_none());
}

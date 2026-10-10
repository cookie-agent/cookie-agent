//! Background jobs started by a tool call: delivery once the call commits,
//! and kills when it never commits or is reverted.

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

/// Shared switches between a test and the jobs its provider launches.
#[derive(Default)]
struct JobControl {
    launched: tokio::sync::Notify,
    release: tokio::sync::Notify,
    killed: tokio::sync::Notify,
    was_killed: std::sync::atomic::AtomicBool,
}

/// Flags the job as killed when dropped before it finished.
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

#[derive(Clone)]
struct BackgroundJobProvider {
    control: Arc<JobControl>,
}

struct BackgroundJobExecutor {
    call_id: ToolCallId,
    mode: String,
    control: Arc<JobControl>,
}

#[async_trait]
impl ToolProvider for BackgroundJobProvider {
    fn provider_id(&self) -> &'static str {
        "test.background_job"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "bash".into(),
            permission_name: "bash".into(),
            description: "Start a background job".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{"command":{"type":"string"}},
                "required":["command"]
            }),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "bash" => Ok("bash"),
            _ => Err(ToolError::execution("job provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
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
        let command = call
            .arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing command"))?
            .to_owned();
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(command.as_bytes()),
            vec![ApprovalCapability {
                action: PermissionAction::Bash,
                operation: PreparedCapabilityOperation::new("bash:execute")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Bash,
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
            Box::new(BackgroundJobExecutor {
                call_id: call.id,
                mode: command.clone(),
                control: Arc::clone(&self.control),
            }),
        )?
        .with_policy_labels(vec![command])
    }
}

#[async_trait]
impl PreparedExecutor for BackgroundJobExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let job = context.start_background().await?;
        let progress = job.progress();
        let control = Arc::clone(&self.control);
        let call_id = self.call_id;
        job.launch(async move {
            let mut flag = KillFlag(Some(Arc::clone(&control)));
            progress
                .send(ToolProgress {
                    tool_call_id: call_id,
                    message: String::new(),
                    display: None,
                    output: vec![cookie_agent_protocol::ToolOutputChunk {
                        stream: Some("stdout".into()),
                        text: "job output line\n".into(),
                    }],
                })
                .await
                .expect("job output");
            control.release.notified().await;
            flag.0 = None;
            crate::BackgroundReport {
                summary: "job finished".into(),
                element: "background_job".into(),
                attributes: vec![("from".into(), "7".into())],
                body: "Status: done".into(),
            }
        })?;
        self.control.launched.notify_one();
        if self.mode == "hold" {
            // Never returns on its own: the run is cancelled under it.
            context.cancellation.cancelled().await;
            return Err(ToolError::execution("job start cancelled"));
        }
        Ok(crate::ToolCompletion::single(
            cookie_agent_protocol::PersistedToolResult {
                title: SafeDisplayText::new("Job").expect("title"),
                output: "started background job".into(),
                display: None,
                retained_output: None,
                metadata: serde_json::Value::Null,
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            },
        ))
    }
}

const JOB_AGENT: &str = "---\ndescription: Background job test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: allow\n---\nTest background jobs.\n";

async fn start_job_run(
    mode: &str,
    responses: Vec<MatchedScriptedResponse>,
    expected_requests: usize,
) -> (
    Fixture,
    SessionId,
    cookie_agent_protocol::RunId,
    Arc<JobControl>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let (endpoint, sender, captured) = scripted_channel_server(expected_requests).await;
    sender
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body("job-call", "bash", serde_json::json!({"command": mode})),
        ))
        .expect("scripted tool response");
    for response in responses {
        sender.send(response).expect("scripted response");
    }
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, JOB_AGENT);
    let control = Arc::new(JobControl::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(BackgroundJobProvider {
            control: Arc::clone(&control),
        }));
    let session_id = fixture
        .engine
        .create_session(selection.clone())
        .expect("job session")
        .session_id;
    let run_id = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id,
                client_run_id: ClientRunId::new(format!("background-{mode}"))
                    .expect("client run id"),
                selection,
                input: "start the job".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started")
        .run_id;
    with_watchdog("job launch", control.launched.notified()).await;
    (fixture, session_id, run_id, control, captured)
}

fn job_message(
    projection: &crate::session::SessionProjection,
) -> Option<(
    ProducerOwner,
    String,
    Option<cookie_agent_protocol::RetainedToolOutput>,
)> {
    projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ProducerMessageAccepted {
                producer_owner: owner @ ProducerOwner::Tool { .. },
                body,
                retained_output,
                ..
            } => Some((owner.clone(), body.clone(), retained_output.clone())),
            _ => None,
        })
}

#[tokio::test]
async fn background_job_reports_after_its_call_commits_and_keeps_its_output() {
    let (fixture, session_id, run_id, control, captured) = start_job_run(
        "finish",
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_job",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    wait_for_run_inactive(&fixture.engine, run_id).await;
    control.release.notify_one();
    let projection = await_projection(&fixture.engine, session_id, "job report", |projection| {
        job_message(projection).is_some()
    })
    .await;
    let (owner, body, retained) = job_message(&projection).expect("job report");
    let call_id = projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } => Some(start.tool_call_id),
            _ => None,
        })
        .expect("started call");
    assert_eq!(
        owner,
        ProducerOwner::Tool {
            tool_call_id: call_id
        }
    );
    // The output preview renders exactly like a tool result's.
    assert_eq!(
        body,
        "<background_job from=\"7\">\nStatus: done\n[stdout]\njob output line\n\n[stderr]\n</background_job>"
    );
    let retained = retained.expect("retained job output");
    let digest = retained
        .reference
        .uri
        .strip_prefix("artifact://sha256/")
        .expect("manifest digest");
    // The producer event alone keeps the job's output alive.
    fixture
        .engine
        .inner
        .artifacts
        .collect_garbage(std::time::Duration::ZERO)
        .expect("artifact gc");
    let page = fixture
        .engine
        .read_artifact(session_id, &format!("artifact://{digest}/stdout"), 0, 10)
        .expect("job stdout");
    assert!(page.content.contains("job output line"), "{}", page.content);
    // The report wakes the idle session and reaches the model once.
    let requests = with_watchdog("woken run", captured)
        .await
        .expect("requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.contains("background_job"))
            .count(),
        1
    );
    assert!(!control.was_killed.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn background_job_dies_with_a_call_that_never_commits() {
    let (fixture, session_id, run_id, control, _captured) =
        start_job_run("hold", Vec::new(), 1).await;
    fixture.engine.cancel_run(run_id).await.expect("cancel run");
    with_watchdog("job killed", control.killed.notified()).await;
    wait_for_run_inactive(&fixture.engine, run_id).await;
    settle_session_actor(&fixture.engine, session_id).await;
    let projection = fixture
        .engine
        .inner
        .store
        .get(session_id)
        .expect("projection");
    assert!(job_message(&projection).is_none());
    assert_eq!(fixture.engine.inner.background.job_count(), 0);
}

#[tokio::test]
async fn reverting_past_the_call_kills_its_running_job() {
    let (fixture, session_id, run_id, control, _captured) = start_job_run(
        "finish",
        vec![MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("waiting"),
        )],
        2,
    )
    .await;
    wait_for_run_inactive(&fixture.engine, run_id).await;
    wait_for_session_not_running(&fixture.engine, session_id).await;
    fixture
        .engine
        .revert_session(
            session_id,
            1,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("revert");
    with_watchdog("job killed", control.killed.notified()).await;
    // Releasing the dead job delivers nothing.
    control.release.notify_one();
    settle_session_actor(&fixture.engine, session_id).await;
    let projection = fixture
        .engine
        .inner
        .store
        .get(session_id)
        .expect("projection");
    assert!(job_message(&projection).is_none());
}

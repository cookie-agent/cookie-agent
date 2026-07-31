//! The `delegate` tool bridges a parent tool call to the engine's child-session
//! lifecycle. Child progress remains observable through the child's engine
//! events; this provider returns only its terminal structured result.

use std::collections::BTreeMap;

use async_trait::async_trait;
use cookiecode_config::{AgentType, Config};
use cookiecode_engine::{
    DelegateInvocation, EngineClient, EngineError, SessionToolContext, ToolCall, ToolError,
    ToolInvocationContext, ToolProvider, ToolResult, ToolSpec, journal::JournalError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const CONTEXT_LIMIT_BYTES: usize = 32 * 1024;

/// Configuration-backed provider for the `delegate` tool.
///
/// The invocation's parent identity comes from [`ToolInvocationContext`], so
/// the provider is stateless across parent calls. Parent delegation policy is
/// always read from the engine's frozen session metadata; copied config only
/// supplies delegate-target type eligibility.
pub struct DelegateToolProvider {
    engine: EngineClient,
    target_profiles: BTreeMap<String, TargetProfile>,
}

#[derive(Clone, Copy)]
struct TargetProfile {
    enabled: bool,
    agent_type: AgentType,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    task: String,
    profile: String,
    #[serde(default)]
    context: Vec<ContextEntry>,
    #[serde(default)]
    success_criteria: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_expected_output")]
    expected_output: Option<ExpectedOutput>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum ContextEntry {
    Text(String),
    FileReference(FileReference),
    ArtifactReference(ArtifactReference),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileReference {
    #[serde(rename = "type")]
    kind: FileReferenceKind,
    path: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactReference {
    #[serde(rename = "type")]
    kind: ArtifactReferenceKind,
    id: String,
}

#[derive(Debug, Deserialize, Serialize)]
enum FileReferenceKind {
    #[serde(rename = "file-ref")]
    FileRef,
}

#[derive(Debug, Deserialize, Serialize)]
enum ArtifactReferenceKind {
    #[serde(rename = "artifact-ref")]
    ArtifactRef,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExpectedOutput {
    description: String,
    format: ExpectedOutputFormat,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum ExpectedOutputFormat {
    Text,
    Json,
}

fn deserialize_expected_output<'de, D>(deserializer: D) -> Result<Option<ExpectedOutput>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    ExpectedOutput::deserialize(deserializer).map(Some)
}

impl DelegateToolProvider {
    #[must_use]
    pub fn new(engine: EngineClient, config: &Config) -> Self {
        Self {
            engine,
            target_profiles: config
                .agents
                .iter()
                .map(|(name, profile)| {
                    (
                        name.clone(),
                        TargetProfile {
                            enabled: profile.enabled,
                            agent_type: profile.r#type,
                        },
                    )
                })
                .collect(),
        }
    }

    fn targets_for_session(
        &self,
        session: cookiecode_protocol::SessionId,
    ) -> Result<Vec<String>, ToolError> {
        let delegation = self
            .engine
            .get_session(session)
            .map_err(|error| delegate_error("delegate.session_lookup_failed", error))?
            .profile
            .delegation;
        if !delegation.enabled || !delegation.depth_limit.allows_delegation() {
            return Ok(Vec::new());
        }
        Ok(delegation
            .allowed_profiles
            .into_iter()
            .filter(|name| {
                self.target_profiles.get(name).is_some_and(|profile| {
                    profile.enabled
                        && matches!(profile.agent_type, AgentType::Subagent | AgentType::All)
                })
            })
            .collect())
    }

    fn tool_spec(targets: Vec<String>) -> ToolSpec {
        ToolSpec {
            name: "delegate".into(),
            description: "Delegate a focused objective to an allowed subagent profile.".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "task": { "type": "string", "description": "Focused objective for the child." },
                    "profile": { "type": "string", "enum": targets },
                    "context": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                { "type": "string" },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "const": "file-ref" },
                                        "path": { "type": "string" }
                                    },
                                    "required": ["type", "path"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "const": "artifact-ref" },
                                        "id": { "type": "string" }
                                    },
                                    "required": ["type", "id"],
                                    "additionalProperties": false
                                }
                            ]
                        },
                        "description": "Context entries are limited to 32 KiB when JSON-encoded."
                    },
                    "success_criteria": { "type": "array", "items": { "type": "string" } },
                    "expected_output": {
                        "type": "object",
                        "properties": {
                            "description": { "type": "string" },
                            "format": { "type": "string", "enum": ["text", "json"] }
                        },
                        "required": ["description", "format"],
                        "additionalProperties": false
                    }
                },
                "required": ["task", "profile"]
            }),
        }
    }
}

fn delegate_error(code: &'static str, error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(
        json!({
            "code": code,
            "message": error.to_string(),
        })
        .to_string(),
    )
}

fn admission_error(error: EngineError) -> ToolError {
    let code = match &error {
        EngineError::Journal(JournalError::Corrupt(_)) => "delegate.fingerprint_conflict",
        _ => "delegate.admission_failed",
    };
    delegate_error(code, error)
}

fn validate_context(context: &[ContextEntry]) -> Result<(), ToolError> {
    let bytes = serde_json::to_vec(context)
        .map_err(|error| delegate_error("delegate.validation", error))?;
    if bytes.len() > CONTEXT_LIMIT_BYTES {
        return Err(delegate_error(
            "delegate.context_too_large",
            format!(
                "context is {} bytes; the limit is {CONTEXT_LIMIT_BYTES} bytes",
                bytes.len()
            ),
        ));
    }
    Ok(())
}

#[async_trait]
impl ToolProvider for DelegateToolProvider {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let targets = self.targets_for_session(ctx.session)?;
        Ok((!targets.is_empty())
            .then(|| Self::tool_spec(targets))
            .into_iter()
            .collect())
    }

    async fn invoke(
        &self,
        ctx: ToolInvocationContext,
        call: ToolCall,
    ) -> Result<ToolResult, ToolError> {
        if call.name != "delegate" {
            return Err(delegate_error(
                "delegate.invalid_tool",
                "delegate tool received another tool name",
            ));
        }
        let args: DelegateArgs = serde_json::from_value(call.arguments)
            .map_err(|error| delegate_error("delegate.validation", error))?;
        validate_context(&args.context)?;
        let targets = self.targets_for_session(ctx.session)?;
        if targets.is_empty() {
            return Err(delegate_error(
                "delegate.unavailable",
                "delegation depth limit exhausted or delegation is disabled",
            ));
        }
        if !targets.contains(&args.profile) {
            return Err(delegate_error(
                "delegate.target_not_allowed",
                format!(
                    "profile `{}` is not an allowed delegate target",
                    args.profile
                ),
            ));
        }

        let context = serde_json::to_value(args.context)
            .map_err(|error| delegate_error("delegate.validation", error))?;
        let context = serde_json::from_value(context)
            .map_err(|error| delegate_error("delegate.validation", error))?;
        let expected_output = serde_json::to_value(args.expected_output)
            .map_err(|error| delegate_error("delegate.validation", error))?;

        let handle = self
            .engine
            .delegate_invoke(DelegateInvocation {
                parent_session_id: ctx.session,
                parent_run_id: ctx.run,
                parent_tool_call_id: call.id,
                profile: args.profile,
                task: args.task,
                context,
                success_criteria: args.success_criteria,
                expected_output,
            })
            .await
            .map_err(admission_error)?;

        // `DelegateAwait` cancels the child when this tool future is dropped.
        self.engine
            .await_delegate(handle)
            .await
            .map_err(|error| delegate_error("delegate.await_failed", error))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use cookiecode_config::{
        AgentProfile, DelegationConfig, ModelConfig, ProviderConfig, ProviderType,
    };
    use cookiecode_engine::{Engine, EngineOptions, ProgressSink, events::OutputHub};
    use cookiecode_protocol::{Event, RunId, SessionStatus, ToolCallId};
    use cookiecode_providers::{
        ModelId, NormalizedEvent, Provider, ProviderCapabilities, ProviderError, ProviderRequest,
        StopReason,
    };
    use futures_util::{StreamExt, stream};
    use serde_json::Value;
    use tokio::sync::{Notify, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct ScriptedProvider {
        calls: AtomicUsize,
        block: bool,
        release: Notify,
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn capabilities(&self, _: &ModelId) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn stream(
            &self,
            _: ProviderRequest,
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
            ProviderError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block {
                self.release.notified().await;
            }
            Ok(stream::iter([
                Ok(NormalizedEvent::TextDelta {
                    text: "child report".into(),
                }),
                Ok(NormalizedEvent::Stop {
                    reason: StopReason::EndTurn,
                }),
            ])
            .boxed())
        }
    }

    fn config(root_limit: Option<u32>, worker_delegates: bool) -> Config {
        let mut config = Config::default();
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                kind: ProviderType::OpenAi,
                api_key_env: None,
                base_url: None,
                api: None,
            },
        );
        let model = ModelConfig {
            provider: "test".into(),
            model: "test-model".into(),
        };
        config.agents = BTreeMap::from([
            (
                "root".into(),
                AgentProfile {
                    r#type: AgentType::Primary,
                    models: vec![model.clone()],
                    delegation: DelegationConfig {
                        enabled: true,
                        allowed_profiles: vec!["worker".into()],
                        limit: root_limit,
                    },
                    ..AgentProfile::default()
                },
            ),
            (
                "plain".into(),
                AgentProfile {
                    r#type: AgentType::Primary,
                    models: vec![model.clone()],
                    ..AgentProfile::default()
                },
            ),
            (
                "worker".into(),
                AgentProfile {
                    r#type: AgentType::Subagent,
                    models: vec![model],
                    delegation: DelegationConfig {
                        enabled: worker_delegates,
                        allowed_profiles: vec!["worker".into()],
                        limit: Some(99),
                    },
                    ..AgentProfile::default()
                },
            ),
        ]);
        config
    }

    fn engine(config: &Config, provider: Arc<dyn Provider>) -> (tempfile::TempDir, Engine) {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            providers: HashMap::from([("test".into(), provider)]),
            tools: Vec::new(),
        })
        .expect("open engine");
        (directory, engine)
    }

    async fn pending_delegate_call(
        engine: &Engine,
    ) -> (cookiecode_protocol::SessionId, RunId, ToolCallId) {
        let session = engine
            .create_session(".", "root")
            .expect("create root session")
            .id;
        let run = RunId::new_v7();
        let call = ToolCallId::new_v7();
        engine
            .append(
                session,
                Some(run),
                Event::RunStarted {
                    client_run_id: "parent".into(),
                    input: "delegate".into(),
                },
            )
            .await
            .expect("start parent run");
        engine
            .append(
                session,
                Some(run),
                Event::ToolCallStarted {
                    tool_call_id: call,
                    tool: "delegate".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("start delegate call");
        (session, run, call)
    }

    fn invocation_context(
        session: cookiecode_protocol::SessionId,
        run: RunId,
        call: ToolCallId,
    ) -> ToolInvocationContext {
        let (sender, _) = mpsc::channel(1);
        ToolInvocationContext {
            session,
            run,
            cwd: Default::default(),
            workspace_root: Default::default(),
            progress: ProgressSink::new(sender, OutputHub::new(call, 1024)),
            cancellation: CancellationToken::new(),
            stdin: None,
        }
    }

    fn delegate_call(call: ToolCallId, task: &str) -> ToolCall {
        ToolCall {
            id: call,
            name: "delegate".into(),
            arguments: json!({
                "task": task,
                "profile": "worker",
                "context": ["parent context"],
                "success_criteria": ["done"],
                "expected_output": {"description": "report", "format": "text"}
            }),
        }
    }

    fn error_code(error: ToolError) -> String {
        let ToolError::Failed(content) = error else {
            panic!("expected a failed tool error");
        };
        serde_json::from_str::<Value>(&content).expect("structured tool error")["code"]
            .as_str()
            .expect("error code")
            .to_owned()
    }

    #[tokio::test]
    async fn schema_is_injected_only_for_delegating_profiles_and_uses_subagent_targets() {
        let config = config(None, false);
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: false,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted);
        let delegate = DelegateToolProvider::new(engine.clone(), &config);
        let root = engine.create_session(".", "root").expect("root").id;
        let plain = engine.create_session(".", "plain").expect("plain").id;

        let tools = delegate
            .tools_for_session(&SessionToolContext { session: root })
            .expect("root tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].parameters["properties"]["profile"]["enum"],
            json!(["worker"])
        );
        assert!(
            delegate
                .tools_for_session(&SessionToolContext { session: plain })
                .expect("plain tools")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn schema_allows_all_type_delegate_targets() {
        let mut config = config(None, false);
        config
            .agents
            .get_mut("root")
            .expect("root profile")
            .delegation
            .allowed_profiles
            .push("all-worker".into());
        config.agents.insert(
            "all-worker".into(),
            AgentProfile {
                r#type: AgentType::All,
                models: vec![ModelConfig {
                    provider: "test".into(),
                    model: "test-model".into(),
                }],
                ..AgentProfile::default()
            },
        );
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: false,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted);
        let root = engine.create_session(".", "root").expect("root").id;
        let delegate = DelegateToolProvider::new(engine, &config);

        let tools = delegate
            .tools_for_session(&SessionToolContext { session: root })
            .expect("root tools");
        assert_eq!(
            tools[0].parameters["properties"]["profile"]["enum"],
            json!(["all-worker", "worker"])
        );
    }

    #[tokio::test]
    async fn schema_uses_the_session_frozen_delegation_snapshot() {
        let config = config(None, false);
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: false,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted);
        let root = engine.create_session(".", "root").expect("root").id;
        let mut changed_config = config;
        let delegation = &mut changed_config
            .agents
            .get_mut("root")
            .expect("root profile")
            .delegation;
        delegation.enabled = false;
        delegation.allowed_profiles.clear();
        let delegate = DelegateToolProvider::new(engine, &changed_config);

        let tools = delegate
            .tools_for_session(&SessionToolContext { session: root })
            .expect("frozen root tools");
        assert_eq!(
            tools[0].parameters["properties"]["profile"]["enum"],
            json!(["worker"])
        );
    }

    #[tokio::test]
    async fn depth_limit_decrements_for_children_and_hides_delegate_at_zero() {
        let config = config(Some(2), true);
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: true,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted);
        let delegate = DelegateToolProvider::new(engine.clone(), &config);
        let (parent, run, call) = pending_delegate_call(&engine).await;
        let child = engine
            .delegate_invoke(DelegateInvocation {
                parent_session_id: parent,
                parent_run_id: run,
                parent_tool_call_id: call,
                profile: "worker".into(),
                task: "first child".into(),
                context: Vec::new(),
                success_criteria: Vec::new(),
                expected_output: Value::Null,
            })
            .await
            .expect("start child");

        assert_eq!(
            delegate
                .tools_for_session(&SessionToolContext {
                    session: child.child_session_id,
                })
                .expect("child tools")
                .len(),
            1
        );

        let grandchild_call = ToolCallId::new_v7();
        engine
            .append(
                child.child_session_id,
                Some(child.child_run_id),
                Event::ToolCallStarted {
                    tool_call_id: grandchild_call,
                    tool: "delegate".into(),
                    arguments: Value::Null,
                    provider_tool_call_id: None,
                    provider_protocol: None,
                },
            )
            .await
            .expect("start child delegate call");
        let grandchild = engine
            .delegate_invoke(DelegateInvocation {
                parent_session_id: child.child_session_id,
                parent_run_id: child.child_run_id,
                parent_tool_call_id: grandchild_call,
                profile: "worker".into(),
                task: "grandchild".into(),
                context: Vec::new(),
                success_criteria: Vec::new(),
                expected_output: Value::Null,
            })
            .await
            .expect("start grandchild");
        assert!(
            delegate
                .tools_for_session(&SessionToolContext {
                    session: grandchild.child_session_id,
                })
                .expect("grandchild tools")
                .is_empty()
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn invoke_awaits_and_returns_the_engine_delegate_result() {
        let config = config(None, false);
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: false,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted);
        let delegate = DelegateToolProvider::new(engine.clone(), &config);
        let (session, run, call) = pending_delegate_call(&engine).await;

        let result = delegate
            .invoke(
                invocation_context(session, run, call),
                delegate_call(call, "report"),
            )
            .await
            .expect("delegate result");
        assert_eq!(result.content, "child report");
    }

    #[tokio::test]
    async fn malformed_optional_fields_and_oversized_context_are_rejected() {
        let config = config(None, false);
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: false,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted);
        let delegate = DelegateToolProvider::new(engine.clone(), &config);
        let (session, run, call) = pending_delegate_call(&engine).await;

        let malformed_output = ToolCall {
            id: call,
            name: "delegate".into(),
            arguments: json!({
                "task": "bad output",
                "profile": "worker",
                "expected_output": {"format": "text"}
            }),
        };
        let error = delegate
            .invoke(invocation_context(session, run, call), malformed_output)
            .await
            .expect_err("malformed expected output");
        assert_eq!(error_code(error), "delegate.validation");

        let null_output = ToolCall {
            id: call,
            name: "delegate".into(),
            arguments: json!({
                "task": "null output",
                "profile": "worker",
                "expected_output": null
            }),
        };
        let error = delegate
            .invoke(invocation_context(session, run, call), null_output)
            .await
            .expect_err("null expected output");
        assert_eq!(error_code(error), "delegate.validation");

        let malformed_context = ToolCall {
            id: call,
            name: "delegate".into(),
            arguments: json!({
                "task": "bad context",
                "profile": "worker",
                "context": [{"type": "file-ref"}]
            }),
        };
        let error = delegate
            .invoke(invocation_context(session, run, call), malformed_context)
            .await
            .expect_err("malformed context");
        assert_eq!(error_code(error), "delegate.validation");

        let oversized_context = ToolCall {
            id: call,
            name: "delegate".into(),
            arguments: json!({
                "task": "large context",
                "profile": "worker",
                "context": ["x".repeat(CONTEXT_LIMIT_BYTES)]
            }),
        };
        let error = delegate
            .invoke(invocation_context(session, run, call), oversized_context)
            .await
            .expect_err("oversized context");
        assert_eq!(error_code(error), "delegate.context_too_large");
    }

    #[tokio::test]
    // Guards the engine-owned cancellation path when a tool task is aborted.
    async fn abort_during_delegate_admission_cancels_the_child() {
        let config = config(None, false);
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: true,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted.clone());
        let delegate = Arc::new(DelegateToolProvider::new(engine.clone(), &config));
        let (session, run, call) = pending_delegate_call(&engine).await;
        let task = tokio::spawn({
            let delegate = delegate.clone();
            async move {
                delegate
                    .invoke(
                        invocation_context(session, run, call),
                        delegate_call(call, "block"),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while scripted.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("child did not start");
        // The engine has no public admission-complete hook. Yielding gives the
        // provider task a chance to acquire its DelegateAwait before aborting.
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(child) = engine.children(session).into_iter().next()
                    && child.status == SessionStatus::Cancelled
                {
                    return child.status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropped wait did not cancel child");
        assert_eq!(status, SessionStatus::Cancelled);
    }

    #[tokio::test]
    async fn conflicting_invocations_surface_as_tool_errors() {
        let config = config(None, false);
        let scripted = Arc::new(ScriptedProvider {
            calls: AtomicUsize::new(0),
            block: false,
            release: Notify::new(),
        });
        let (_workspace, engine) = engine(&config, scripted);
        let delegate = DelegateToolProvider::new(engine.clone(), &config);
        let (session, run, call) = pending_delegate_call(&engine).await;

        delegate
            .invoke(
                invocation_context(session, run, call),
                delegate_call(call, "first"),
            )
            .await
            .expect("first delegate invocation");
        let error = delegate
            .invoke(
                invocation_context(session, run, call),
                delegate_call(call, "different"),
            )
            .await
            .expect_err("fingerprint conflict");
        assert_eq!(error_code(error), "delegate.fingerprint_conflict");
    }
}

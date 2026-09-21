//! Tool providers and executors used as doubles by the runtime tests.

use super::*;

pub(crate) fn test_turn_context() -> Arc<TurnAgentContext> {
    Arc::new(TurnAgentContext {
        agent: AgentId::new("test").expect("test agent ID"),
        model: "test/model".parse().expect("test model key"),
        adapter: cookie_agent_protocol::AdaptorId::OpenaiChat,
        adapter_family: cookie_agent_models::adapters::OvenAdapterFamily::OpenaiChat,
        capabilities: cookie_agent_protocol::ModelCapabilities {
            input: BTreeSet::from([cookie_agent_protocol::Modality::Text]),
            output: BTreeSet::from([cookie_agent_protocol::Modality::Text]),
            context_tokens: 8_192,
            output_tokens: 2_048,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: cookie_agent_protocol::ReplayCapability::Optional,
            cancellation: cookie_agent_protocol::CancellationCapability::LocalOnly,
            media: BTreeMap::new(),
        },
    })
}

#[derive(Clone)]
pub(crate) struct TestStreamingBashProvider {
    pub(crate) output_started: Arc<tokio::sync::Notify>,
    pub(crate) stdin_received: Arc<tokio::sync::Notify>,
    pub(crate) cleanup_progress_sent: Arc<tokio::sync::Notify>,
}

pub(crate) struct TestStreamingBashExecutor {
    pub(crate) call_id: ToolCallId,
    pub(crate) command: String,
    pub(crate) output_started: Arc<tokio::sync::Notify>,
    pub(crate) stdin_received: Arc<tokio::sync::Notify>,
    pub(crate) cleanup_progress_sent: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolProvider for TestStreamingBashProvider {
    fn provider_id(&self) -> &'static str {
        "test.streaming_bash"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "bash".into(),
            permission_name: "bash".into(),
            description: "Stream until cancelled".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{
                    "command":{"type":"string"},
                    "interactive":{"type":"boolean"}
                },
                "required":["command","interactive"]
            }),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "bash" => Ok("bash"),
            _ => Err(ToolError::execution(
                "streaming provider received another tool",
            )),
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
            Sha256Digest::of_bytes(b"streaming bash context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(TestStreamingBashExecutor {
                call_id: call.id,
                command: command.clone(),
                output_started: Arc::clone(&self.output_started),
                stdin_received: Arc::clone(&self.stdin_received),
                cleanup_progress_sent: Arc::clone(&self.cleanup_progress_sent),
            }),
        )?
        .with_policy_labels(vec![command])
    }
}

#[async_trait]
impl PreparedExecutor for TestStreamingBashExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let result: Result<cookie_agent_protocol::PersistedToolResult, ToolError> = async move {
        context
            .progress
            .send(ToolProgress {
output: vec![cookie_agent_protocol::ToolOutputChunk { stream: None, text: "authoritative start\n".into() }],
                tool_call_id: self.call_id,
                message: "bash stdout".into(),
                display: Some(if self.command == "timeout" {
                    "stdout before internal timeout".into()
                } else {
                    "before cancellation".into()
                }),
            })
            .await?;
        self.output_started.notify_one();
        if self.command == "timeout" {
            context
                .progress
                .send(ToolProgress {
output: vec![cookie_agent_protocol::ToolOutputChunk { stream: None, text: "authoritative error\n".into() }],
                    tool_call_id: self.call_id,
                    message: "bash stderr".into(),
                    display: Some("stderr before internal timeout".into()),
                })
                .await?;
            return Err(ToolError::execution("bash timed out"));
        }
        let mut stdin = context
            .stdin
            .ok_or_else(|| ToolError::execution("interactive stdin missing"))?;
        let write = stdin
            .recv()
            .await
            .ok_or_else(|| ToolError::execution("interactive stdin closed"))?;
        if write.data != b"input\n" || write.eof {
            return Err(ToolError::execution("unexpected interactive stdin"));
        }
        self.stdin_received.notify_one();
        context.cancellation.cancelled().await;
        if self.command == "wedge" {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        context
            .progress
            .send(ToolProgress {
output: vec![cookie_agent_protocol::ToolOutputChunk { stream: None, text: "authoritative cleanup\n".into() }],
                tool_call_id: self.call_id,
                message: "bash stdout".into(),
                display: Some("during cancellation cleanup".into()),
            })
            .await?;
        self.cleanup_progress_sent.notify_one();
        tokio::time::sleep(if self.command == "wedge" {
            std::time::Duration::from_secs(3)
        } else {
            std::time::Duration::from_millis(25)
        })
        .await;
        if matches!(
            self.command.as_str(),
            "session-shaped" | "null-session-shaped"
        ) {
            return Ok(cookie_agent_protocol::PersistedToolResult {
display: None,
retained_output: None,
                title: cookie_agent_protocol::SafeDisplayText::new("External result").unwrap(),
                output: "external cleanup result".into(),
                metadata: if self.command == "session-shaped" {
                    serde_json::json!({"session_id": context.session, "child_session_id": context.session})
                } else {
                    serde_json::json!({"session_id": null, "child_session_id": null})
                },
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            });
        }
        Err(ToolError::execution("streaming bash cancelled"))
}.await;
        result.map(crate::ToolCompletion::single)
    }
}

#[derive(Default)]
pub(crate) struct ParallelToolState {
    pub(crate) active: AtomicUsize,
    pub(crate) max_active: AtomicUsize,
    pub(crate) started: AtomicUsize,
    pub(crate) started_names: std::sync::Mutex<Vec<String>>,
    pub(crate) completed_names: std::sync::Mutex<Vec<String>>,
    pub(crate) prepare_batches: std::sync::Mutex<Vec<Vec<String>>>,
}

impl ParallelToolState {
    pub(crate) fn enter(self: &Arc<Self>, name: String) -> ParallelToolGuard {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        self.started_names
            .lock()
            .expect("parallel started names lock")
            .push(name);
        ParallelToolGuard {
            state: Arc::clone(self),
        }
    }
}

pub(crate) struct ParallelToolGuard {
    pub(crate) state: Arc<ParallelToolState>,
}

impl Drop for ParallelToolGuard {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub(crate) struct TestParallelToolProvider {
    pub(crate) state: Arc<ParallelToolState>,
    pub(crate) barrier: Option<Arc<tokio::sync::Barrier>>,
}

pub(crate) struct TestOptOutProvider(pub(crate) TestParallelToolProvider);

#[async_trait]
impl ToolProvider for TestOptOutProvider {
    fn provider_id(&self) -> &'static str {
        "test.opt-out"
    }
    fn tools_for_session(&self, context: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let mut tools = self.0.tools_for_session(context)?;
        for tool in &mut tools {
            tool.result_truncation = crate::ToolResultTruncationPolicy::OptOut;
        }
        Ok(tools)
    }
    fn get_permission_name(name: &str) -> Result<&'static str, ToolError> {
        TestParallelToolProvider::get_permission_name(name)
    }
    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        self.0.get_permission_resource(name, arguments)
    }
    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.0.get_display_argument(name, arguments)
    }
    async fn prepare(
        &self,
        context: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        self.0.prepare(context, call).await
    }
}

pub(crate) struct TestParallelToolExecutor {
    pub(crate) name: String,
    pub(crate) delay_ms: u64,
    pub(crate) fail: bool,
    pub(crate) wait_for_cancellation: bool,
    pub(crate) state: Arc<ParallelToolState>,
    pub(crate) barrier: Option<Arc<tokio::sync::Barrier>>,
}

pub(crate) fn parallel_tool_permission(
    name: &str,
) -> Result<(&'static str, PermissionAction), ToolError> {
    match name {
        "parallel_read" => Ok(("read", PermissionAction::Read)),
        "parallel_bash" => Ok(("bash", PermissionAction::Bash)),
        "parallel_write" | "parallel_edit" => Ok(("write", PermissionAction::Write)),
        _ => Err(ToolError::execution(
            "parallel test provider received another tool",
        )),
    }
}

#[async_trait]
impl ToolProvider for TestParallelToolProvider {
    fn provider_id(&self) -> &'static str {
        "test.parallel"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok([
            ("parallel_read", "read"),
            ("parallel_bash", "bash"),
            ("parallel_write", "write"),
            ("parallel_edit", "write"),
        ]
        .into_iter()
        .map(|(name, permission_name)| ToolSpec {
            output: Default::default(),
            concurrency: ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            name: name.into(),
            permission_name: permission_name.into(),
            description: format!("Parallel test tool {name}"),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{
                    "name":{"type":"string"},
                    "delay_ms":{"type":"integer","minimum":0},
                    "fail":{"type":"boolean"},
                    "wait_for_cancellation":{"type":"boolean"},
                    "serialization_key":{"type":"string"}
                },
                "required":["name"]
            }),
        })
        .collect())
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        parallel_tool_permission(tool_name).map(|(permission, _)| permission)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission = Self::get_permission_name(name)?;
        let resource = arguments
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("parallel test tool name is missing"))?;
        Ok((permission, Some(resource.into())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_permission_resource(name, arguments)?
            .1
            .ok_or_else(|| ToolError::execution("parallel test resource is missing"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let (_, action) = parallel_tool_permission(&call.name)?;
        let name = call
            .arguments
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("parallel test tool name is missing"))?
            .to_owned();
        let delay_ms = call
            .arguments
            .get("delay_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let fail = call
            .arguments
            .get("fail")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let wait_for_cancellation = call
            .arguments
            .get("wait_for_cancellation")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let serialization_key = call
            .arguments
            .get("serialization_key")
            .and_then(serde_json::Value::as_str)
            .map(|key| PreparedSerializationKey::new(key.as_bytes()));
        let digest = Sha256Digest::of_bytes(name.as_bytes());
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(call.arguments.to_string().as_bytes()),
            vec![ApprovalCapability {
                action,
                operation: PreparedCapabilityOperation::new(format!("{}:execute", call.name))
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: action,
                canonical: PreparedResourceIdentity::new(format!("test:{digest}"))
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    name.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::RestartStable,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"parallel test execution context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            call.arguments,
            serialization_key,
            Box::new(TestParallelToolExecutor {
                name: name.clone(),
                delay_ms,
                fail,
                wait_for_cancellation,
                state: Arc::clone(&self.state),
                barrier: self.barrier.clone(),
            }),
        )?
        .with_policy_labels(vec![name])
    }

    async fn prepare_parallel(
        &self,
        ctx: ToolPreparationContext,
        calls: Vec<ToolCall>,
    ) -> Vec<Result<PreparedTool, ToolError>> {
        self.state
            .prepare_batches
            .lock()
            .expect("parallel prepare batches lock")
            .push(
                calls
                    .iter()
                    .map(|call| {
                        call.arguments
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    })
                    .collect(),
            );
        let mut prepared = Vec::with_capacity(calls.len());
        for call in calls {
            prepared.push(self.prepare(ctx.clone(), call).await);
        }
        prepared
    }
}

#[async_trait]
impl PreparedExecutor for TestParallelToolExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let result: Result<cookie_agent_protocol::PersistedToolResult, ToolError> = async move {
            let _active = self.state.enter(self.name.clone());
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            if self.wait_for_cancellation {
                context.cancellation.cancelled().await;
            }
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            if self.fail {
                return Err(ToolError::execution(format!("{} failed", self.name)));
            }
            self.state
                .completed_names
                .lock()
                .expect("parallel completed names lock")
                .push(self.name.clone());
            Ok(cookie_agent_protocol::PersistedToolResult {
                display: None,
                retained_output: None,
                title: cookie_agent_protocol::SafeDisplayText::new("parallel test result")
                    .expect("result title"),
                output: format!("{} completed", self.name),
                metadata: serde_json::json!({"name":self.name}),
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            })
        }
        .await;
        result.map(crate::ToolCompletion::single)
    }
}

#[derive(Clone)]
pub(crate) struct TestDelegateProvider {
    pub(crate) engine: Engine,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct TestDelegateArgs {
    pub(crate) agent_type: AgentId,
    pub(crate) prompt: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) background: bool,
    pub(crate) resume_session_id: Option<String>,
    #[serde(default)]
    pub(crate) inherit_context: bool,
}

pub(crate) struct TestDelegateExecutor {
    pub(crate) engine: Engine,
    pub(crate) call_id: ToolCallId,
    pub(crate) args: TestDelegateArgs,
}

#[async_trait]
impl ToolProvider for TestDelegateProvider {
    fn provider_id(&self) -> &'static str {
        "test.delegate"
    }

    fn prompt_sections(&self, ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        let Some(targets) = ctx.prompt_delegate_targets() else {
            return Ok(Vec::new());
        };
        let targets = targets.collect::<Vec<_>>();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut body = String::from("Available subagents:");
        for (id, description) in targets {
            body.push_str(&format!("\n- {id}: {description}"));
        }
        Ok(vec![PromptSection {
            title: "Available subagents".into(),
            body,
        }])
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let targets = self
            .engine
            .delegate_targets(ctx.session)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok((!targets.is_empty())
            .then(|| ToolSpec {
                output: Default::default(),
                concurrency: crate::ToolConcurrency::Parallel,
                result_truncation: Default::default(),
                name: "delegate_subagent".to_owned(),
                permission_name: "delegate".to_owned(),
                description: "Delegate scripted work".to_owned(),
                parameters: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties":{
                        "agent_type":{"type":"string","enum":targets},
                        "prompt":{"type":"string"},
                        "description":{"type":"string"},
                        "background":{"type":"boolean","default":false},
                        "resume_session_id":{"type":"string"},
                        "inherit_context":{"type":"boolean","default":false}
                    },
                    "required":["agent_type","prompt","description"]
                }),
            })
            .into_iter()
            .collect())
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "delegate_subagent" => Ok("delegate"),
            _ => Err(ToolError::execution(
                "delegate provider received another tool",
            )),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        let args: TestDelegateArgs = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok((permission_name, Some(args.agent_type.to_string())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let (_, resource) = self.get_permission_resource(name, arguments)?;
        resource.ok_or_else(|| ToolError::execution("delegate permission resource is missing"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: TestDelegateArgs = serde_json::from_value(call.arguments)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let label = args.agent_type.to_string();
        let label_digest = Sha256Digest::of_bytes(label.as_bytes());
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(
                &serde_json::to_vec(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            ),
            vec![ApprovalCapability {
                action: PermissionAction::Delegate,
                operation: PreparedCapabilityOperation::new("delegate_subagent:spawn")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Delegate,
                canonical: PreparedResourceIdentity::new(format!("agent:{label_digest}"))
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    label.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::RestartStable,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"scripted delegation context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            serde_json::to_value(&args).map_err(|error| ToolError::execution(error.to_string()))?,
            None,
            Box::new(TestDelegateExecutor {
                engine: self.engine.clone(),
                call_id: call.id,
                args,
            }),
        )?
        .with_policy_labels(vec![label])
    }
}

#[async_trait]
impl PreparedExecutor for TestDelegateExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let result: Result<cookie_agent_protocol::PersistedToolResult, ToolError> = async move {
            let TestDelegateExecutor {
                engine,
                call_id,
                args,
            } = *self;
            let staged_restart = args.prompt == "staged restart";
            if staged_restart {
                engine.stage_skill_fork_for_test(
                    call_id,
                    &cookie_agent_protocol::StagedSkillPayload {
                        provenance: cookie_agent_protocol::StagedSkillProvenance::SkillFork,
                        name: "restart-skill".into(),
                        args: String::new(),
                        rendered_body: "Restart recovered skill body".into(),
                        source_path: "/skills/restart-skill/SKILL.md".into(),
                        base_dir: "/skills/restart-skill".into(),
                        supporting_files: Vec::new(),
                        grants: vec![cookie_agent_protocol::PermissionRule {
                            action: PermissionAction::Bash,
                            resource: WildcardPattern::new("git *").expect("grant"),
                            effect: PermissionEffect::Allow,
                        }],
                        model: None,
                    },
                );
            }
            let background = args.background;
            let prompt = if staged_restart {
                "Apply the staged skill `restart-skill`.".into()
            } else {
                args.prompt
            };
            let handle = engine
                .delegate_invoke(DelegateInvocation {
                    parent_session_id: context.session,
                    parent_run_id: context.run,
                    parent_tool_call_id: call_id,
                    agent_type: args.agent_type,
                    description: args.description,
                    prompt,
                    background,
                    resume_session_id: args.resume_session_id,
                    inherit_context: args.inherit_context,
                })
                .await
                .map_err(|error| ToolError::execution(error.to_string()))?;
            if background {
                let metadata = serde_json::json!({"session_id":handle.child_session_id});
                Ok(cookie_agent_protocol::PersistedToolResult {
                    display: None,
                    retained_output: None,
                    title: cookie_agent_protocol::SafeDisplayText::new("Subagent started")
                        .expect("title"),
                    output: format!(
                        "Subagent started. [subagent session {}]",
                        handle.child_session_id
                    ),
                    metadata,
                    truncation: None,
                    attachments: Vec::new(),
                    additional_messages: Vec::new(),
                })
            } else {
                engine
                    .await_delegate(handle)
                    .await
                    .map_err(|error| ToolError::execution(error.to_string()))
            }
        }
        .await;
        result.map(crate::ToolCompletion::single)
    }
}

#[derive(Clone)]
pub(crate) struct TestWriteProvider {
    pub(crate) executed: Arc<TestFlag>,
}

pub(crate) struct TestWriteExecutor {
    pub(crate) executed: Arc<TestFlag>,
}

#[async_trait]
impl ToolProvider for TestWriteProvider {
    fn provider_id(&self) -> &'static str {
        "test.write"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "write".to_owned(),
            permission_name: "write".to_owned(),
            description: "Write a test value".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {"value":{"type":"string"}}
            }),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "write" => Ok("write"),
            _ => Err(ToolError::execution("write provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Ok((
            Self::get_permission_name(name)?,
            Some("approval-test.txt".into()),
        ))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let (_, resource) = self.get_permission_resource(name, arguments)?;
        resource.ok_or_else(|| ToolError::execution("write permission resource is missing"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let label = "approval-test.txt";
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"approval test write arguments"),
            vec![ApprovalCapability {
                action: PermissionAction::Write,
                operation: PreparedCapabilityOperation::new("write:file")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Write,
                canonical: PreparedResourceIdentity::new(label)
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    label.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::RestartStable,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"approval test execution context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(TestWriteExecutor {
                executed: Arc::clone(&self.executed),
            }),
        )
    }
}

#[async_trait]
impl PreparedExecutor for TestWriteExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let result: Result<cookie_agent_protocol::PersistedToolResult, ToolError> = async move {
            self.executed.set();
            Ok(cookie_agent_protocol::PersistedToolResult {
                display: None,
                retained_output: None,
                title: cookie_agent_protocol::SafeDisplayText::new("approval test write")
                    .expect("result title"),
                output: "executed".to_owned(),
                metadata: serde_json::Value::Null,
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            })
        }
        .await;
        result.map(crate::ToolCompletion::single)
    }
}

pub(crate) struct TestAliasProvider {
    pub(crate) executed: Arc<TestFlag>,
}

pub(crate) struct TestAliasExecutor {
    pub(crate) executed: Arc<TestFlag>,
}

#[async_trait]
impl ToolProvider for TestAliasProvider {
    fn provider_id(&self) -> &'static str {
        "test.alias"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "bad_name".to_owned(),
            permission_name: "write".to_owned(),
            description: "Aliased tool that must never execute".to_owned(),
            parameters: serde_json::json!({"type":"object","additionalProperties":true}),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "bad_name" => Ok("write"),
            _ => Err(ToolError::execution("alias provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Ok((Self::get_permission_name(name)?, Some("alias-test".into())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok(self
            .get_permission_resource(name, arguments)?
            .1
            .unwrap_or_default())
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let label = "alias-test";
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"alias test arguments"),
            vec![ApprovalCapability {
                action: PermissionAction::Write,
                operation: PreparedCapabilityOperation::new("alias:execute")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Write,
                canonical: PreparedResourceIdentity::new(label)
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    label.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::RestartStable,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"alias test execution context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(TestAliasExecutor {
                executed: Arc::clone(&self.executed),
            }),
        )
    }
}

#[async_trait]
impl PreparedExecutor for TestAliasExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        self.executed.set();
        Ok(crate::ToolCompletion::single(
            cookie_agent_protocol::PersistedToolResult {
                display: None,
                retained_output: None,
                title: cookie_agent_protocol::SafeDisplayText::new("aliased tool result")
                    .expect("result title"),
                output: "aliased tool executed".to_owned(),
                metadata: serde_json::Value::Null,
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            },
        ))
    }
}

#[derive(Clone)]
pub(crate) struct TestMediaReadProvider;

pub(crate) struct TestMediaReadExecutor {
    pub(crate) path: std::path::PathBuf,
}

#[async_trait]
impl ToolProvider for TestMediaReadProvider {
    fn provider_id(&self) -> &'static str {
        "test.media_read"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "read".into(),
            permission_name: "read".into(),
            description: "Read a test media file".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{"filePath":{"type":"string"}},
                "required":["filePath"]
            }),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "read" => Ok("read"),
            _ => Err(ToolError::execution("read provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let path = arguments
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing filePath"))?;
        Ok((Self::get_permission_name(name)?, Some(path.into())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_permission_resource(name, arguments)?
            .1
            .ok_or_else(|| ToolError::execution("missing filePath"))
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let display = call
            .arguments
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing filePath"))?;
        let path = ctx.cwd.join(display);
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(display.as_bytes()),
            vec![ApprovalCapability {
                action: PermissionAction::Read,
                operation: PreparedCapabilityOperation::new("read:file").unwrap(),
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Read,
                canonical: PreparedResourceIdentity::new(display).unwrap(),
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    display.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"test media read"),
        )
        .unwrap();
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(TestMediaReadExecutor { path }),
        )
    }
}

#[async_trait]
impl PreparedExecutor for TestMediaReadExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let result: Result<cookie_agent_protocol::PersistedToolResult, ToolError> = async move {
            let bytes =
                fs::read(&self.path).map_err(|error| ToolError::execution(error.to_string()))?;
            let mime = crate::approved_media_type(&self.path, &bytes)?
                .ok_or_else(|| ToolError::execution("test read expected media"))?;
            let gate = crate::gate_attachment(
                context.turn_context.adapter_family,
                &context.turn_context.capabilities,
                mime,
                &bytes,
            );
            if let Some(error) = crate::attachment_gate_error(
                gate,
                mime,
                &context.turn_context.model,
                context.turn_context.adapter,
            ) {
                return Err(ToolError::execution(error));
            }
            let attachment = context.retain_attachment(mime, None, &bytes)?;
            Ok(cookie_agent_protocol::PersistedToolResult {
                display: None,
                retained_output: None,
                title: cookie_agent_protocol::SafeDisplayText::new("Read attachment").unwrap(),
                output: format!("Attached {mime}."),
                metadata: serde_json::Value::Null,
                truncation: None,
                attachments: vec![attachment],
                additional_messages: Vec::new(),
            })
        }
        .await;
        result.map(crate::ToolCompletion::single)
    }
}

pub(crate) struct TestToolDefinitionProvider;

#[derive(Clone)]
pub(crate) struct TestPromptProvider {
    pub(crate) id: &'static str,
    pub(crate) sections: Arc<std::sync::Mutex<Vec<PromptSection>>>,
}

impl TestPromptProvider {
    pub(crate) fn new(id: &'static str, sections: Vec<PromptSection>) -> Self {
        Self {
            id,
            sections: Arc::new(std::sync::Mutex::new(sections)),
        }
    }

    pub(crate) fn replace_sections(&self, sections: Vec<PromptSection>) {
        *self
            .sections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = sections;
    }
}

#[async_trait]
impl ToolProvider for TestPromptProvider {
    fn provider_id(&self) -> &'static str {
        self.id
    }

    fn prompt_sections(&self, _ctx: &SessionToolContext) -> Result<Vec<PromptSection>, ToolError> {
        Ok(self
            .sections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(Vec::new())
    }

    fn get_permission_name(_tool_name: &str) -> Result<&'static str, ToolError> {
        Err(ToolError::execution("prompt-only provider has no tools"))
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Err(ToolError::execution("prompt-only provider has no tools"))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Err(ToolError::execution("prompt-only provider has no tools"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution("prompt-only provider has no tools"))
    }
}

pub(crate) struct OrderedToolDefinitionProvider {
    pub(crate) id: &'static str,
    pub(crate) tools: Vec<(&'static str, &'static str)>,
}

#[async_trait]
impl ToolProvider for OrderedToolDefinitionProvider {
    fn provider_id(&self) -> &'static str {
        self.id
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(self
            .tools
            .iter()
            .map(|(name, permission_name)| ToolSpec {
                output: Default::default(),
                concurrency: Default::default(),
                result_truncation: Default::default(),
                name: (*name).into(),
                permission_name: (*permission_name).into(),
                description: format!("Ordered {name} definition"),
                parameters: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties":{}
                }),
            })
            .collect())
    }

    fn get_permission_name(_tool_name: &str) -> Result<&'static str, ToolError> {
        Err(ToolError::execution(
            "ordered definition provider is listing-only",
        ))
    }

    fn get_permission_resource(
        &self,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Err(ToolError::execution(
            "ordered definition provider is listing-only",
        ))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Err(ToolError::execution(
            "ordered definition provider is listing-only",
        ))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution(
            "ordered definition provider is listing-only",
        ))
    }
}

#[async_trait]
impl ToolProvider for TestToolDefinitionProvider {
    fn provider_id(&self) -> &'static str {
        "test.definition"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok([
            ("read", "read"),
            ("write", "write"),
            ("edit", "write"),
            ("bash", "bash"),
            ("delegate", "delegate"),
            ("fixture_mcp", "mcp"),
        ]
        .into_iter()
        .map(|(name, permission_name)| ToolSpec {
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: name.into(),
            permission_name: permission_name.into(),
            description: format!("Test {name} tool definition"),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        })
        .collect())
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "read" => Ok("read"),
            "write" | "edit" => Ok("write"),
            "bash" => Ok("bash"),
            "delegate" => Ok("delegate"),
            "fixture_mcp" => Ok("mcp"),
            _ => Err(ToolError::execution("unknown test tool definition")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Ok((Self::get_permission_name(name)?, Some("test".into())))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok("test".into())
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution(
            "definition-only test provider cannot prepare tools",
        ))
    }
}

pub(crate) struct NamedOutputProvider {
    pub(crate) failed: bool,
}

pub(crate) struct NamedOutputExecutor(pub(crate) ToolCallId, pub(crate) bool);

#[async_trait]
impl ToolProvider for NamedOutputProvider {
    fn provider_id(&self) -> &'static str {
        "test.named-output"
    }
    fn tools_for_session(&self, _: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "named_output".into(),
            permission_name: "read".into(),
            description: "Produce named output".into(),
            parameters: serde_json::json!({"type":"object","additionalProperties":false}),
            concurrency: ToolConcurrency::Parallel,
            result_truncation: Default::default(),
            output: cookie_agent_protocol::ToolOutputDeclaration::Named {
                streams: vec!["results".into(), "diagnostics".into(), "empty".into()],
            },
        }])
    }
    fn get_permission_name(_: &str) -> Result<&'static str, ToolError> {
        Ok("read")
    }
    fn get_permission_resource(
        &self,
        _: &str,
        _: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        Ok(("read", Some("named_output".into())))
    }
    fn get_display_argument(&self, _: &str, _: &serde_json::Value) -> Result<String, ToolError> {
        Ok("named output".into())
    }
    async fn prepare(
        &self,
        _: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"named-output"),
            vec![ApprovalCapability {
                action: PermissionAction::Read,
                operation: PreparedCapabilityOperation::new("test:named-output").unwrap(),
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Read,
                canonical: PreparedResourceIdentity::new(format!(
                    "test:{}",
                    Sha256Digest::of_bytes(b"named_output")
                ))
                .unwrap(),
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    b"named-output",
                ),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"context"),
        )
        .unwrap();
        PreparedTool::new(
            operation,
            call.arguments,
            None,
            Box::new(NamedOutputExecutor(call.id, self.failed)),
        )?
        .with_policy_labels(vec!["named_output".into()])
    }
}

#[async_trait]
impl PreparedExecutor for NamedOutputExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }
    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        for index in 0..4 {
            context
                .progress
                .send(ToolProgress {
                    tool_call_id: self.0,
                    message: "output".into(),
                    display: (index == 0).then(|| "UI_LIVE_ONLY".into()),
                    output: vec![cookie_agent_protocol::ToolOutputChunk {
                        stream: Some("results".into()),
                        text: "x".repeat(32 * 1024),
                    }],
                })
                .await?;
            if index == 1 {
                context
                    .progress
                    .send(ToolProgress {
                        tool_call_id: self.0,
                        message: "diagnostic".into(),
                        display: None,
                        output: vec![cookie_agent_protocol::ToolOutputChunk {
                            stream: Some("diagnostics".into()),
                            text: "diagnostic\n".into(),
                        }],
                    })
                    .await?;
            }
        }
        let mut completion =
            crate::ToolCompletion::streamed(cookie_agent_protocol::PersistedToolResult {
                title: cookie_agent_protocol::SafeDisplayText::new("Named output").unwrap(),
                output: String::new(),
                display: Some("UI_FINAL_ONLY".into()),
                retained_output: None,
                metadata: serde_json::Value::Null,
                truncation: None,
                attachments: Vec::new(),
                additional_messages: Vec::new(),
            });
        completion.failed = self.1;
        Ok(completion)
    }
}

pub(crate) struct DivergentReadProvider {
    pub(crate) raw_resource: Option<String>,
}

pub(crate) struct DivergentReadExecutor;

#[async_trait]
impl ToolProvider for DivergentReadProvider {
    fn provider_id(&self) -> &'static str {
        "test.divergent_read"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "read".into(),
            permission_name: "read".into(),
            description: "Divergent prepared-label test".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{"filePath":{"type":"string"}},
                "required":["filePath"]
            }),
        }])
    }

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        match tool_name {
            "read" => Ok("read"),
            _ => Err(ToolError::execution("read provider received another tool")),
        }
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission_name = Self::get_permission_name(name)?;
        Ok((permission_name, self.raw_resource.clone()))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let (_, resource) = self.get_permission_resource(name, arguments)?;
        resource.ok_or_else(|| ToolError::execution("read permission resource is missing"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let raw = call
            .arguments
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing filePath"))?;
        let prepared_path = format!("canonical/{raw}");
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(prepared_path.as_bytes()),
            vec![ApprovalCapability {
                action: PermissionAction::Read,
                operation: PreparedCapabilityOperation::new("read:file")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Read,
                canonical: PreparedResourceIdentity::new(format!(
                    "file:{}",
                    Sha256Digest::of_bytes(b"divergent-raw")
                ))
                .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"divergent"),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"divergent context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            serde_json::json!({"filePath": prepared_path}),
            None,
            Box::new(DivergentReadExecutor),
        )?
        .with_policy_labels(vec!["divergent-raw".into()])
    }
}

#[async_trait]
impl PreparedExecutor for DivergentReadExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<crate::ToolCompletion, ToolError> {
        let result: Result<cookie_agent_protocol::PersistedToolResult, ToolError> =
            async move { unreachable!("divergence test never executes") }.await;
        result.map(crate::ToolCompletion::single)
    }
}

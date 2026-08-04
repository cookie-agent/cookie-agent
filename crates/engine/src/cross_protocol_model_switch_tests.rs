use std::{fs, os::unix::fs::PermissionsExt as _, sync::Arc};

use async_trait::async_trait;
use cookie_agent_config::load_from_roots;
use cookie_agent_models::{
    AdaptorId as ModelAdaptorId, CredentialStore, ModelCapabilities as CatalogCapabilities,
    ModelSetManager, ScriptedModel, ScriptedStep, TestModelEntry, test_model_set,
};
use cookie_agent_protocol::{
    AdaptorId, AgentId, ClientRunId, EventPayload as Event, ModelSelection, ReplayDisposition,
    RunSelection, RunStartParams, SessionStatus,
};
use oven_sdk::{
    AdapterId, Capability, Finish, FinishReason, LanguageModelDescriptor, ModelCapabilities,
    ModelId, ModelIdentity, NativeContextScope, NativeReplayArtifact, ProviderId, ResourceId,
    StreamPart, TextPart, ToolCallPart, Usage,
};
use tempfile::TempDir;

use crate::{
    Engine, EngineOptions, PreparedTool, SessionToolContext, ToolCall, ToolError,
    ToolPreparationContext, ToolProvider, ToolSpec,
};

const ANTHROPIC_ADAPTER: &str = "oven.anthropic.messages";
const OPENAI_RESPONSES_ADAPTER: &str = "oven.openai.responses";

fn descriptor(provider: &str, model: &str, adapter: &str) -> LanguageModelDescriptor {
    let mut capabilities = ModelCapabilities::conservative();
    capabilities.features.insert(Capability::TOOL_CALLING);
    LanguageModelDescriptor::new(
        ModelIdentity::new(ProviderId::new(provider), ModelId::new(model)).expect("identity"),
        AdapterId::new(adapter),
        capabilities,
    )
    .expect("descriptor")
}

fn text_stream(text: &str, mut finish: Finish) -> ScriptedStep {
    finish.usage = Usage {
        input_tokens: Some(1),
        output_tokens: Some(1),
        ..Usage::default()
    };
    ScriptedStep::stream(
        [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::TextStart {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::TextDelta {
                id: "text".into(),
                delta: text.into(),
                metadata: None,
            },
            StreamPart::TextEnd {
                id: "text".into(),
                metadata: None,
            },
            StreamPart::Finish { finish },
        ]
        .into_iter()
        .map(Ok),
    )
}

fn tool_stream() -> ScriptedStep {
    ScriptedStep::stream(
        [
            StreamPart::StreamStart { warnings: vec![] },
            StreamPart::ToolCall {
                tool_call: ToolCallPart::new(
                    "fixture-read",
                    "read",
                    serde_json::json!({"path":"fixture.txt"}),
                ),
            },
            StreamPart::Finish {
                finish: Finish::new(Usage::default(), FinishReason::ToolCalls),
            },
        ]
        .into_iter()
        .map(Ok),
    )
}

struct FailingReadTool;

#[async_trait]
impl ToolProvider for FailingReadTool {
    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "read".into(),
            description: "fixture read".into(),
            parameters: serde_json::json!({
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"]
            }),
        }])
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution("fixture read failed"))
    }
}

fn selection(model: &str) -> RunSelection {
    RunSelection {
        agent: AgentId::new("primary").expect("agent"),
        model: ModelSelection {
            model: model.parse().expect("model key"),
            variant: None,
        },
    }
}

fn catalog_capabilities() -> CatalogCapabilities {
    serde_json::from_value(serde_json::json!({
        "input":["text"],
        "output":["text"],
        "context_tokens":8192,
        "output_tokens":2048,
        "tool_calling":true,
        "parallel_tool_calls":false,
        "structured_output":false,
        "reasoning":false,
        "temperature":false,
        "top_p":false,
        "seed":false,
        "native_replay":"optional",
        "native_compaction":"unsupported",
        "cancellation":"local_only",
        "media":{}
    }))
    .expect("catalog capabilities")
}

async fn wait_for_terminal(engine: &Engine, session: cookie_agent_protocol::SessionId) {
    for _ in 0..200 {
        if engine.get_session(session).expect("session").status != SessionStatus::Running {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("fixture run did not become terminal");
}

#[tokio::test]
async fn root_run_switches_to_openai_with_complete_replay_evidence() {
    let directory = TempDir::new().expect("tempdir");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private tempdir");
    let root = directory.path().join(".cookie-agent");
    fs::create_dir_all(root.join("agents")).expect("agent directory");
    fs::write(
        root.join("config.toml"),
        r#"schema_version = 6
[session_title]
generate_on_first_turn = false
fallback_to_input_excerpt = true
max_chars = 80
max_input_messages = 4

[providers.dummy]
source = "explicit"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"
auth = { type = "none" }
[providers.dummy.models.unused]
display_name = "Unused"
[providers.dummy.models.unused.capabilities]
input = ["text"]
output = ["text"]
context_tokens = 8192
output_tokens = 2048
tool_calling = false
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = false
top_p = false
seed = false
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {}
"#,
    )
    .expect("config");
    fs::write(
        root.join("agents/primary.md"),
        "---\nschema: 1\ndescription: cross protocol test\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"anthropic/claude-test\" }, { model: \"openai/gpt-test\" }]\ntools: [read]\npermissions: [{ id: allow-read, action: read, resource: \"*\", effect: allow }]\n---\nCross protocol prompt.\n",
    )
    .expect("agent");

    let anthropic_descriptor = descriptor("anthropic", "claude-test", ANTHROPIC_ADAPTER);
    let scope = NativeContextScope::new(
        ProviderId::new("anthropic"),
        ModelId::new("claude-test"),
        ResourceId::new("anthropic-turn").expect("resource"),
    )
    .expect("scope");
    let mut anthropic_finish = Finish::new(Usage::default(), FinishReason::Stop);
    anthropic_finish.native_replay = Some(
        NativeReplayArtifact::new(
            AdapterId::new(ANTHROPIC_ADAPTER),
            scope,
            serde_json::json!({"format":"fixture-anthropic"}),
        )
        .expect("native replay"),
    );
    let anthropic = Arc::new(ScriptedModel::new(
        anthropic_descriptor.clone(),
        [text_stream("anthropic answer", anthropic_finish)],
    ));
    let responses_descriptor = descriptor("openai", "gpt-test", OPENAI_RESPONSES_ADAPTER);
    let responses = Arc::new(ScriptedModel::new(
        responses_descriptor.clone(),
        [
            tool_stream(),
            text_stream(
                "responses answer",
                Finish::new(Usage::default(), FinishReason::Stop),
            ),
        ],
    ));
    let models = test_model_set(vec![
        TestModelEntry {
            key: "anthropic/claude-test".parse().expect("model key"),
            display_name: "Claude Test".into(),
            adapter_id: ModelAdaptorId::Anthropic,
            capabilities: catalog_capabilities(),
            model: anthropic,
            descriptor: anthropic_descriptor,
        },
        TestModelEntry {
            key: "openai/gpt-test".parse().expect("model key"),
            display_name: "GPT Test".into(),
            adapter_id: ModelAdaptorId::OpenaiResponses,
            capabilities: catalog_capabilities(),
            model: responses.clone(),
            descriptor: responses_descriptor,
        },
    ]);
    let loaded = load_from_roots(None, Some(&root)).expect("configuration");
    let manager = Arc::new(ModelSetManager::from_test_model_set(
        models,
        CredentialStore::new(directory.path().join("credentials")),
    ));
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: loaded,
        model_manager: Arc::clone(&manager),
        tools: vec![Arc::new(FailingReadTool)],
    })
    .expect("engine");
    let session = engine
        .create_session(selection("anthropic/claude-test"))
        .expect("session");
    let first = engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("anthropic-turn").expect("client run id"),
            selection: selection("anthropic/claude-test"),
            input: "first message".into(),
        })
        .await
        .expect("anthropic run");
    wait_for_terminal(&engine, session.session_id).await;
    let second = engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("responses-turn").expect("client run id"),
            selection: selection("openai/gpt-test"),
            input: "second message".into(),
        })
        .await
        .expect("responses run");
    wait_for_terminal(&engine, session.session_id).await;

    let projection = engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection");
    assert_eq!(
        projection.runs[&first.run_id].selection,
        selection("anthropic/claude-test")
    );
    assert_eq!(
        projection.runs[&second.run_id].selection,
        selection("openai/gpt-test")
    );
    assert_eq!(
        projection.runs[&second.run_id].agent.fallback_chain[0]
            .resolved
            .adapter_id,
        AdaptorId::OpenaiResponses
    );
    assert!(Arc::ptr_eq(
        &engine
            .historical_run_model_snapshot(second.run_id)
            .expect("second run model snapshot"),
        &manager.current()
    ));
    assert!(projection.log.events().iter().any(|event| matches!(
        &event.payload,
        Event::ModelTurnCommitted {
            resolved_model,
            turn,
            ..
        } if event.run_id == Some(first.run_id)
            && resolved_model.adapter_id == AdaptorId::Anthropic
            && turn.native_replay.as_ref().is_some_and(|artifact| {
                artifact.adapter_id().as_str() == ANTHROPIC_ADAPTER
            })
    )));
    let second_events = projection
        .log
        .events()
        .into_iter()
        .filter(|event| event.run_id == Some(second.run_id))
        .collect::<Vec<_>>();
    let attempts = second_events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::ModelAttemptStarted { resolved_model, .. } => Some(resolved_model),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|resolved_model| {
        resolved_model.selection == selection("openai/gpt-test").model
            && resolved_model.adapter_id == AdaptorId::OpenaiResponses
    }));
    let replay_events = second_events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::ModelReplayEvaluated {
                ordered_decisions, ..
            } => Some(ordered_decisions),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replay_events.len(), 2);
    for replay in &replay_events {
        assert!(matches!(
            replay.as_slice(),
            [
                cookie_agent_protocol::ReplayDecision {
                    history_index: 2,
                    disposition: ReplayDisposition::DiscardedForeignAdapter { found, expected },
                },
                cookie_agent_protocol::ReplayDecision {
                    history_index: 2,
                    disposition: ReplayDisposition::ReconstructedNormalizedHistory,
                }
            ] if found.as_str() == ANTHROPIC_ADAPTER
                && expected.as_str() == OPENAI_RESPONSES_ADAPTER
        ));
    }
    let incompatibility_transitions = second_events
        .iter()
        .filter_map(|event| match &event.payload {
            Event::ModelReplayEvaluated {
                ordered_decisions, ..
            } => Some(ordered_decisions),
            _ => None,
        })
        .flatten()
        .filter(|decision| {
            matches!(
                decision.disposition,
                ReplayDisposition::DiscardedForeignAdapter { .. }
                    | ReplayDisposition::DiscardedForeignModelSelection { .. }
                    | ReplayDisposition::DiscardedForeignVariant { .. }
            )
        })
        .count();
    assert_eq!(incompatibility_transitions, replay_events.len());

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let oven_sdk::HistoryTurn::Assistant(first_turn) = &request.history[2] else {
            panic!("historical assistant turn");
        };
        assert!(first_turn.finish.native_replay.is_none());
        assert!(matches!(
            first_turn.message.content.as_slice(),
            [oven_sdk::AssistantPart::Text(TextPart { text, .. })] if text == "anthropic answer"
        ));
    }

    let reopened = crate::events::EventLog::open(
        engine
            .inner
            .store
            .session_dir(session.session_id)
            .join("events.jsonl"),
        session.session_id,
    )
    .expect("restart replay");
    assert_eq!(reopened.events(), projection.log.events());
    engine.shutdown().await;
}

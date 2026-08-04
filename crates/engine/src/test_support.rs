use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use cookie_agent_config::{
    ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, LoadedConfiguration,
    RuntimeConfig, ServerConfig, SessionTitleConfig, ToolOutputConfig,
};
use cookie_agent_models::{
    Catalog, CredentialStore, FrozenModelBinding, ModelSetManager, ProviderDefinition,
    build_model_set,
};
use cookie_agent_protocol::{
    AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot, ModelKey,
    ModelSelection, ProviderId, RunSelection, Sha256Digest,
};

use crate::{Engine, EngineOptions};

fn provider_definition() -> ProviderDefinition {
    let mut value = serde_json::json!({
        "source": "explicit",
        "endpoint": "https://example.test/v1",
        "adaptor": "openai-compatible",
        "auth": {"type": "none"},
        "models": {
            "arbitrary-model": {
                "display_name": "Arbitrary Model",
                "capabilities": {
                    "input": ["text"],
                    "output": ["text"],
                    "context_tokens": 8192,
                    "output_tokens": 2048,
                    "tool_calling": true,
                    "parallel_tool_calls": true,
                    "structured_output": false,
                    "reasoning": false,
                    "temperature": true,
                    "top_p": true,
                    "seed": true,
                    "native_replay": "optional",
                    "native_compaction": "unsupported",
                    "cancellation": "local_only",
                    "media": {}
                },
                "variants": {
                    "fast": {
                        "operation": "add",
                        "defaults": {"temperature": 0.1}
                    }
                }
            }
        }
    });
    value["models"]["other-model"] = value["models"]["arbitrary-model"].clone();
    value["models"]["other-model"]["display_name"] = serde_json::json!("Other Model");
    serde_json::from_value(value).expect("test provider")
}

fn providers() -> BTreeMap<ProviderId, ProviderDefinition> {
    BTreeMap::from([(
        ProviderId::new("gateway").expect("test provider id"),
        provider_definition(),
    )])
}

pub(crate) fn model_binding() -> FrozenModelBinding {
    model_set()
        .freeze(&model_selection())
        .expect("test model binding")
}

pub(crate) fn model_set() -> cookie_agent_models::ModelSet {
    build_model_set(
        &providers(),
        &Catalog::embedded().expect("embedded catalog"),
        None,
    )
    .expect("test model set")
}

pub(crate) fn model_snapshot() -> Arc<cookie_agent_models::ModelSnapshot> {
    let temp = tempfile::tempdir().expect("model snapshot tempdir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("private model snapshot tempdir");
    }
    ModelSetManager::new(
        providers(),
        Arc::new(Catalog::embedded().expect("embedded catalog")),
        CredentialStore::new(temp.path().join("credentials")),
    )
    .expect("model snapshot manager")
    .current()
}

pub(crate) fn variant_model_binding() -> FrozenModelBinding {
    let mut selection = model_selection();
    selection.variant = Some(cookie_agent_protocol::VariantId::new("fast").expect("variant id"));
    model_set()
        .freeze(&selection)
        .expect("variant model binding")
}

pub(crate) fn model_selection() -> ModelSelection {
    ModelSelection {
        model: "gateway/arbitrary-model"
            .parse::<ModelKey>()
            .expect("test model key"),
        variant: None,
    }
}

pub(crate) fn other_model_selection() -> ModelSelection {
    ModelSelection {
        model: "gateway/other-model"
            .parse::<ModelKey>()
            .expect("other test model key"),
        variant: None,
    }
}

pub(crate) fn run_selection(agent: &str) -> RunSelection {
    RunSelection {
        agent: AgentId::new(agent).expect("test agent id"),
        model: model_selection(),
    }
}

pub(crate) fn agent_snapshot(agent: &str, mode: AgentMode) -> AgentSnapshot {
    let model_binding = model_binding();
    assert_eq!(
        (
            model_binding.descriptor.identity.provider_id.as_str(),
            model_binding.descriptor.identity.model_id.as_str(),
        ),
        (
            model_binding.resolved.provider_id.as_str(),
            model_binding.resolved.model_id.as_str(),
        ),
        "test model descriptor identity"
    );
    let binding = crate::policy::wire_binding(&model_binding).expect("wire test binding");
    AgentSnapshot {
        agent: AgentId::new(agent).expect("test agent id"),
        schema: AgentSchemaVersion::current(),
        mode,
        description: format!("Test {agent} agent"),
        document_source: AgentDocumentSource::Workspace,
        document_fingerprint: Sha256Digest::of_bytes(format!("{agent} document").as_bytes()),
        composed_prompt: format!("You are the {agent} test agent.\n"),
        prompt_fingerprint: Sha256Digest::of_bytes(format!("{agent} prompt").as_bytes()),
        tools: Vec::new(),
        permissions: Vec::new(),
        delegation: None,
        fallback_chain: vec![binding],
        selected_suffix_start: 0,
    }
}

pub(crate) fn engine(root: &Path) -> Engine {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).expect("private test root");
    }
    let providers = providers();
    let manager = ModelSetManager::new(
        providers.clone(),
        Arc::new(Catalog::embedded().expect("embedded catalog")),
        CredentialStore::new(root.join("credentials")),
    )
    .expect("model manager");
    Engine::open(EngineOptions {
        data_dir: root.join("data"),
        cwd: root.to_owned(),
        config: LoadedConfiguration {
            runtime: RuntimeConfig {
                schema_version: ConfigSchemaVersion,
                server: ServerConfig::default(),
                tool_output: ToolOutputConfig::default(),
                approval: ApprovalConfig::default(),
                context_compaction: ContextCompactionConfig::default(),
                session_title: SessionTitleConfig::default(),
                providers,
            },
            agents: BTreeMap::new(),
        },
        model_manager: Arc::new(manager),
        tools: Vec::new(),
    })
    .expect("test engine")
}

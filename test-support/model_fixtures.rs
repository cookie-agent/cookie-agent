//! Model bindings, agent snapshots, and internal agent definitions for the
//! engine's and its store's unit tests, built from a test provider the way
//! the engine builds real ones. Each crate includes this file as its
//! `test_support` module and uses what it needs.
#![allow(dead_code)]

use std::{collections::BTreeMap, sync::Arc};

#[cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt as _};

use cookie_agent_models::{
    CompiledModelRuntime, ModelManager, ProviderDefinition,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    manifests::{build_manifest, frozen_binding},
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    AgentDocumentSource, AgentId, AgentMode, AgentSnapshot, CatalogRevision, FrozenModelBinding,
    ModelSelection, PersistedModelBinding, ProviderModelId, RunSelection, Sha256Digest, VariantId,
};
use jiff::Timestamp;
use tempfile::TempDir;

fn binding_fixture(
    model_id: &str,
) -> (
    std::sync::Arc<CompiledModelRuntime>,
    FrozenModelBinding,
    Option<FrozenModelBinding>,
) {
    let temporary = TempDir::new().expect("test model directory");
    #[cfg(unix)]
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .expect("private test model directory");
    let mut provider: ProviderDefinition = toml::from_str(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-responses"
auth = { method = "bearer-api-key-v1", values = { api_key = "test-secret" } }

[models.test]
display_name = "Test model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 8192, output_tokens = 2048, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "optional", media = {} }
variants = { fast = { generation_options = { temperature = 0.1 } } }
"#,
    )
    .expect("test provider");
    let ProviderDefinition::Custom(custom) = &mut provider else {
        unreachable!("test provider is custom")
    };
    let test_id = ProviderModelId::new("test").expect("test model ID");
    let template = custom.models.get(&test_id).expect("test model").clone();
    for id in ["fallback-zero", "fallback-one", "fallback-two"] {
        custom.models.insert(
            ProviderModelId::new(id).expect("fallback model ID"),
            template.clone(),
        );
    }
    let now = Timestamp::now();
    let catalog = CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "0".repeat(64)))
            .expect("catalog revision"),
        source: CatalogSource::Bootstrap,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Bootstrap,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: Arc::new(BTreeMap::new()),
        canonical_models: Arc::new(BTreeMap::new()),
        quarantine: Arc::new(Vec::new()),
    };
    let manager = ModelManager::new(
        BTreeMap::from([("custom.test".parse().expect("provider ID"), provider)]),
        std::sync::Arc::new(catalog),
        ProviderStore::open(temporary.path().join("provider-store")).expect("provider store"),
    )
    .expect("test manager");
    let runtime = manager.current();
    let manifest =
        build_manifest(runtime.manifest_payload().expect("manifest payload")).expect("manifest");
    let blueprint = manifest
        .payload
        .blueprints
        .iter()
        .find(|blueprint| blueprint.selection.model.model_id().as_str() == model_id)
        .expect("requested test blueprint")
        .clone();
    let base_selection = blueprint.selection.clone();
    let base = frozen_binding(
        manifest.revision.clone(),
        &blueprint,
        base_selection.clone(),
    )
    .expect("base binding");
    let variant = blueprint
        .variants
        .iter()
        .any(|variant| variant.id.as_str() == "fast")
        .then(|| {
            frozen_binding(
                manifest.revision.clone(),
                &blueprint,
                ModelSelection {
                    model: base_selection.model,
                    variant: Some(VariantId::new("fast").expect("variant ID")),
                },
            )
            .expect("variant binding")
        });
    (runtime, base, variant)
}

fn bindings_for(model_id: &str) -> (FrozenModelBinding, Option<FrozenModelBinding>) {
    let (_, base, variant) = binding_fixture(model_id);
    (base, variant)
}

pub(crate) fn model_binding() -> FrozenModelBinding {
    bindings_for("test").0
}

pub(crate) fn model_binding_named(model_id: &str) -> FrozenModelBinding {
    bindings_for(model_id).0
}

pub(crate) fn variant_model_binding() -> FrozenModelBinding {
    bindings_for("test").1.expect("test variant")
}

pub(crate) fn model_runtime_and_binding()
-> (std::sync::Arc<CompiledModelRuntime>, FrozenModelBinding) {
    let (runtime, binding, _) = binding_fixture("test");
    (runtime, binding)
}

pub(crate) fn run_selection(agent: &str) -> RunSelection {
    RunSelection {
        agent: AgentId::new(agent).expect("agent ID"),
        model: model_binding().selection,
        preset: None,
    }
}

/// The form session history stores `binding` in.
pub(crate) fn persisted(binding: &FrozenModelBinding) -> PersistedModelBinding {
    PersistedModelBinding::new(binding, crate::bindings::wire_resolved(binding))
        .expect("persisted test binding")
}

pub(crate) fn agent_snapshot(agent: &str, mode: AgentMode) -> AgentSnapshot {
    let binding = model_binding();
    AgentSnapshot {
        agent: AgentId::new(agent).expect("agent ID"),
        mode,
        description: format!("Test {agent} agent"),
        document_source: AgentDocumentSource::User,
        document_fingerprint: Sha256Digest::of_bytes(format!("{agent} document").as_bytes()),
        composed_prompt: format!("You are the {agent} test agent.\n"),
        prompt_fingerprint: Sha256Digest::of_bytes(format!("{agent} prompt").as_bytes()),
        max_output_tokens: 0,
        permissions: Vec::new(),
        delegation: None,
        fallback_chain: vec![persisted(&binding)],
        selected_suffix_start: 0,
    }
}

/// The manifest revision of a stored test binding.
pub(crate) fn manifest_of(
    binding: &PersistedModelBinding,
) -> cookie_agent_protocol::ModelSnapshotRevision {
    binding
        .decode()
        .expect("test binding decodes")
        .manifest_revision
}

/// Frozen definitions of the three internal agents, each running on the
/// parent's model.
pub(crate) fn internal_agent_definitions()
-> Vec<cookie_agent_protocol::FrozenInternalAgentDefinition> {
    use cookie_agent_protocol::InternalAgentKind;
    [
        (InternalAgentKind::Approval, "approval"),
        (InternalAgentKind::ContextCompaction, "compaction"),
        (InternalAgentKind::SessionTitle, "title"),
    ]
    .into_iter()
    .map(
        |(kind, id)| cookie_agent_protocol::FrozenInternalAgentDefinition {
            kind,
            agent: AgentId::new(id).expect("agent ID"),
            description: format!("Test {id} agent"),
            document_source: AgentDocumentSource::BuiltIn,
            document_fingerprint: Sha256Digest::of_bytes(format!("{id} document").as_bytes()),
            composed_prompt: format!("You are the {id} test agent.\n"),
            prompt_fingerprint: Sha256Digest::of_bytes(format!("{id} prompt").as_bytes()),
            enabled: true,
            max_output_tokens: 0,
            timeout_ms: 0,
            fallbacks: vec![
                cookie_agent_protocol::FrozenInternalAgentFallback::ParentModel { variant: None },
            ],
        },
    )
    .collect()
}

/// `presentation` as `tool_call_started` stores it for base-layer tool
/// `presentation.title`.
pub(crate) fn base_presentation(
    presentation: cookie_agent_protocol::PreparedPresentation,
) -> cookie_agent_protocol::ToolPresentation {
    let name = presentation.title.to_string();
    cookie_agent_protocol::ToolPresentation::new(
        presentation,
        cookie_agent_protocol::ToolIdentity {
            name: name.clone(),
            published_name: name,
            source: Some(cookie_agent_protocol::Open::Known(
                cookie_agent_protocol::ToolSource::Base,
            )),
        },
    )
}

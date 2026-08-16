use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _};

use cookie_agent_models::{
    CompiledModelRuntime, ModelManager, ProviderDefinition,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    manifests::{ModelSnapshotManifestStore, frozen_binding},
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot, CatalogRevision,
    FrozenModelBinding, ModelSelection, ProviderModelId, RunSelection, Sha256Digest, VariantId,
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
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .expect("private test model directory");
    let mut provider: ProviderDefinition = toml::from_str(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-responses"
auth = { method = "bearer-api-key-v1", values = { api_key = "test-secret" } }

[models.test]
display_name = "Test model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 8192, output_tokens = 2048, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "optional", cancellation = "local_only", media = {} }
variants = { fast = { operation = "add", defaults = { temperature = 0.1 } } }
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
        providers: BTreeMap::new(),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    };
    let manager = ModelManager::new(
        BTreeMap::from([("custom.test".parse().expect("provider ID"), provider)]),
        std::sync::Arc::new(catalog),
        ProviderStore::open(temporary.path().join("provider-store")).expect("provider store"),
    )
    .expect("test manager");
    let runtime = manager.current();
    let store =
        ModelSnapshotManifestStore::open_directory(temporary.path().join("model-snapshots"))
            .expect("manifest store");
    let manifest = store
        .write(runtime.manifest_payload().expect("manifest payload"))
        .expect("manifest");
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
    }
}

pub(crate) fn agent_snapshot(agent: &str, mode: AgentMode) -> AgentSnapshot {
    let binding = model_binding();
    AgentSnapshot {
        agent: AgentId::new(agent).expect("agent ID"),
        schema: AgentSchemaVersion::current(),
        mode,
        description: format!("Test {agent} agent"),
        document_source: AgentDocumentSource::Workspace,
        document_fingerprint: Sha256Digest::of_bytes(format!("{agent} document").as_bytes()),
        composed_prompt: format!("You are the {agent} test agent.\n"),
        prompt_fingerprint: Sha256Digest::of_bytes(format!("{agent} prompt").as_bytes()),
        max_output_tokens: 0,
        permissions: Vec::new(),
        delegation: None,
        fallback_chain: vec![binding],
        selected_suffix_start: 0,
    }
}

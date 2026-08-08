use std::{
    fs,
    path::Path,
    sync::{Mutex, OnceLock},
};

use cookie_agent_config::{ConfigError, load_from_roots};
use cookie_agent_identity::ProviderId;
use cookie_agent_models::ProviderDefinition;
use tempfile::TempDir;

fn create_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
}

fn write_config(root: &Path, text: &str) {
    create_dir(root);
    fs::write(root.join("config.toml"), text).unwrap();
}

fn write_agent(root: &Path, name: &str, text: &str) {
    create_dir(&root.join("agents"));
    fs::write(root.join("agents").join(name), text).unwrap();
}

fn agent(description: &str, fallback: &str) -> String {
    format!(
        "---\nschema: 2\ndescription: {description}\nmode: primary\nenabled: true\nmodel_fallback: {fallback}\ntools: []\npermissions: {{}}\n---\nPrompt.\n"
    )
}

const SECRET_SENTINEL: &str = "CONFIG_SECRET_SENTINEL_7f13c4";

fn assert_redacted(error: &ConfigError) {
    let rendered = format!("{error:?}\n{error}");
    assert!(
        !rendered.contains(SECRET_SENTINEL),
        "secret leaked: {rendered}"
    );
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn custom(endpoint: &str) -> String {
    format!(
        r#"schema_version = 9

[providers."custom.test"]
source = "custom"
endpoint = "{endpoint}"
adaptor = "openai-compatible"
auth = {{ method = "no-auth-v1", values = {{}} }}

[providers."custom.test".models."group/model"]
display_name = "Model"

[providers."custom.test".models."group/model".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 4096
output_tokens = 1024
tool_calling = false
parallel_tool_calls = false
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = true
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {{}}
"#
    )
}

#[test]
fn schema9_allows_soft_only_compaction_and_rejects_removed_fields_and_schema8() {
    let temp = TempDir::new().unwrap();
    let omitted = temp.path().join("omitted");
    write_config(
        &omitted,
        "schema_version = 9\n[context_compaction]\nsoft_threshold_percent = 70\n",
    );
    let loaded = load_from_roots(None, Some(&omitted)).unwrap();
    assert!(loaded.runtime.providers.is_empty());
    assert!(loaded.agents.is_empty());
    assert_eq!(loaded.agent_registry().agents().len(), 0);

    let empty = temp.path().join("empty");
    write_config(&empty, "schema_version = 9\nproviders = {}\n");
    assert!(
        load_from_roots(None, Some(&empty))
            .unwrap()
            .runtime
            .providers
            .is_empty()
    );

    let old = temp.path().join("old");
    write_config(&old, "schema_version = 8\n");
    assert!(matches!(
        load_from_roots(None, Some(&old)),
        Err(ConfigError::Toml(_))
    ));

    let removed = temp.path().join("removed");
    write_config(
        &removed,
        "schema_version = 9\n[context_compaction]\ntarget_percent = 50\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&removed)),
        Err(ConfigError::Toml(_))
    ));

    let removed_hard = temp.path().join("removed-hard");
    write_config(
        &removed_hard,
        "schema_version = 9\n[context_compaction]\nhard_threshold_percent = 85\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&removed_hard)),
        Err(ConfigError::Toml(_))
    ));
}

#[test]
fn delegation_runtime_defaults_and_limits_are_strict() {
    let temp = TempDir::new().unwrap();
    let defaults = temp.path().join("delegation-defaults");
    write_config(&defaults, "schema_version = 9\n");
    let loaded = load_from_roots(None, Some(&defaults)).unwrap();
    assert_eq!(loaded.runtime.delegation.max_depth, 3);
    assert_eq!(loaded.runtime.delegation.max_concurrency, None);

    let authored = temp.path().join("delegation-authored");
    write_config(
        &authored,
        "schema_version = 9\n[delegation]\nmax_depth = 5\nmax_concurrency = 2\n",
    );
    let loaded = load_from_roots(None, Some(&authored)).unwrap();
    assert_eq!(loaded.runtime.delegation.max_depth, 5);
    assert_eq!(loaded.runtime.delegation.max_concurrency, Some(2));

    let invalid = temp.path().join("delegation-invalid");
    write_config(
        &invalid,
        "schema_version = 9\n[delegation]\nmax_concurrency = 0\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&invalid)),
        Err(ConfigError::InvalidRuntime)
    ));
}

#[test]
fn authored_agent_registry_retains_slash_model_ids_without_model_compilation() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("agents-only");
    write_config(&root, "schema_version = 9\n");
    write_agent(
        &root,
        "primary.md",
        &agent(
            "Primary",
            "[{ model: \"custom.test/group/model/deep\", variant: high }]",
        ),
    );

    let loaded = load_from_roots(None, Some(&root)).unwrap();
    let registry = loaded.agent_registry();
    let input = registry.materialization_inputs().next().unwrap();
    assert!(input.root_eligible);
    assert_eq!(
        input.document.frontmatter.model_fallback[0]
            .model
            .model_id()
            .as_str(),
        "group/model/deep"
    );
    assert!(matches!(
        input.document.frontmatter.model_fallback[0]
            .variant
            .as_ref()
            .unwrap(),
        cookie_agent_identity::ConfiguredVariantRef::Named(id) if id.as_str() == "high"
    ));
}

#[test]
fn authored_agents_cannot_use_the_built_in_default_id() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("reserved-agent");
    write_config(&root, "schema_version = 9\n");
    write_agent(
        &root,
        "default.md",
        &agent("Reserved", "[{ model: \"custom.test/model\" }]"),
    );

    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::ReservedAgentId(id)) if id.as_str() == "default"
    ));
}

#[test]
fn workspace_agent_replaces_user_agent_before_registry_validation() {
    let temp = TempDir::new().unwrap();
    let user = temp.path().join("user-agents");
    let workspace = temp.path().join("workspace-agents");
    write_config(&user, "schema_version = 9\n");
    write_agent(
        &user,
        "primary.md",
        &agent(
            "Discarded",
            "[{ model: \"openai/group/model\" }, { model: \"openai/group/model\" }]",
        ),
    );
    write_config(&workspace, "schema_version = 9\n");
    write_agent(
        &workspace,
        "primary.md",
        &agent("Workspace", "[{ model: \"openai/group/model\" }]"),
    );

    let loaded = load_from_roots(Some(&user), Some(&workspace)).unwrap();
    let registry = loaded.agent_registry();
    let document = registry
        .get(&cookie_agent_identity::AgentId::new("primary").unwrap())
        .unwrap();
    assert_eq!(document.frontmatter.description, "Workspace");
}

#[test]
fn explicit_source_has_no_alias_or_compatibility_reader() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("explicit");
    write_config(
        &root,
        "schema_version = 9\n[providers.test]\nsource = \"explicit\"\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::Toml(_))
    ));
}

#[test]
fn same_id_workspace_provider_replaces_user_before_provider_decode() {
    let temp = TempDir::new().unwrap();
    let user = temp.path().join("user");
    let workspace = temp.path().join("workspace");
    write_config(
        &user,
        "schema_version = 9\n[providers.\"custom.test\"]\nsource = \"custom\"\nunknown = true\n",
    );
    write_config(&workspace, &custom("https://workspace.example/v1"));

    let loaded = load_from_roots(Some(&user), Some(&workspace)).unwrap();
    let id = ProviderId::new("custom.test").unwrap();
    assert!(matches!(
        loaded.runtime.providers[&id],
        ProviderDefinition::Custom(_)
    ));
}

#[test]
fn interpolation_is_limited_and_static_headers_never_interpolate() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("headers");
    let text = custom("https://example.test/v1").replace(
        "auth = { method = \"no-auth-v1\", values = {} }",
        "auth = { method = \"no-auth-v1\", values = {} }\nheaders = { x-safe = \"${env:SECRET}\" }",
    );
    write_config(&root, &text);
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::Interpolation(_))
    ));
}

#[test]
fn managed_auth_is_mutually_exclusive_and_custom_namespace_is_strict() {
    let temp = TempDir::new().unwrap();
    let conflict = temp.path().join("conflict");
    write_config(
        &conflict,
        "schema_version = 9\n[providers.openai]\nsource = \"models_dev\"\napi_key = \"a\"\nauth_override = { method = \"bearer-api-key-v1\", values = { api_key = \"b\" } }\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&conflict)),
        Err(ConfigError::Provider { .. })
    ));

    let namespace = temp.path().join("namespace");
    write_config(
        &namespace,
        &custom("https://example.test/v1").replace("custom.test", "test"),
    );
    assert!(matches!(
        load_from_roots(None, Some(&namespace)),
        Err(ConfigError::Provider { .. })
    ));
}

#[test]
fn secret_sentinel_is_redacted_on_parse_interpolation_and_unknown_field_errors() {
    let temp = TempDir::new().unwrap();

    let parse = temp.path().join("parse-secret");
    write_config(
        &parse,
        &format!(
            "schema_version = 9\n[providers.openai]\nsource = \"models_dev\"\napi_key = \"{SECRET_SENTINEL}\"\nbroken = [\n"
        ),
    );
    assert_redacted(&load_from_roots(None, Some(&parse)).unwrap_err());

    let interpolation = temp.path().join("interpolation-secret");
    write_config(
        &interpolation,
        &format!(
            "schema_version = 9\n[providers.openai]\nsource = \"models_dev\"\napi_key = \"{SECRET_SENTINEL}-${{env:P1_MISSING_SECRET}}\"\n"
        ),
    );
    let _guard = env_lock();
    unsafe { std::env::remove_var("P1_MISSING_SECRET") };
    assert_redacted(&load_from_roots(None, Some(&interpolation)).unwrap_err());

    let unknown = temp.path().join("unknown-secret");
    write_config(
        &unknown,
        &format!(
            "schema_version = 9\n[providers.openai]\nsource = \"models_dev\"\napi_key = \"ok\"\nunknown_secret = \"{SECRET_SENTINEL}\"\n"
        ),
    );
    assert_redacted(&load_from_roots(None, Some(&unknown)).unwrap_err());
}

#[test]
fn interpolated_secret_is_redacted_on_success_and_configuration_drop() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("success-secret");
    write_config(
        &root,
        "schema_version = 9\n[providers.openai]\nsource = \"models_dev\"\napi_key = \"${env:P1_SUCCESS_SECRET}\"\n",
    );
    let _guard = env_lock();
    unsafe { std::env::set_var("P1_SUCCESS_SECRET", SECRET_SENTINEL) };
    {
        let loaded = load_from_roots(None, Some(&root)).unwrap();
        assert!(!format!("{loaded:?}").contains(SECRET_SENTINEL));
    }
    unsafe { std::env::remove_var("P1_SUCCESS_SECRET") };
}

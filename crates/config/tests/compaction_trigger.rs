use std::{
    fs,
    path::Path,
    sync::{Mutex, OnceLock},
};

use cookie_agent_config::{ConfigError, ContextCompactionTrigger, load_from_roots};
use cookie_agent_identity::AgentId;
use tempfile::TempDir;

fn create_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
}

fn write_config(root: &Path, text: &str) {
    create_dir(root);
    fs::write(root.join("config.toml"), text).unwrap();
}

fn write_agent(root: &Path, name: &str, text: &str) {
    let path = root.join("agents").join(name);
    create_dir(path.parent().unwrap());
    fs::write(path, text).unwrap();
}

fn agent(description: &str, fallback: &str) -> String {
    format!(
        "---\ndescription: {description}\nmode: primary\nenabled: true\nmodels: {fallback}\npermissions: {{}}\n---\nPrompt.\n"
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
        r#"
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
cancellation = "local_only"
media = {{}}
"#
    )
}

#[test]
fn compaction_triggers_are_strict_and_legacy_buffer_is_supported() {
    let temp = TempDir::new().unwrap();
    let legacy = temp.path().join("legacy");
    write_config(
        &legacy,
        "[context_compaction]\nauto = true\nbuffer_tokens = 33000\n",
    );
    let loaded = load_from_roots(None, Some(&legacy)).unwrap();
    assert!(loaded.runtime.context_compaction.auto_compaction);
    assert_eq!(
        loaded.runtime.context_compaction.trigger,
        ContextCompactionTrigger::BufferTokens {
            buffer_tokens: 33_000
        }
    );
    assert!(loaded.runtime.providers.is_empty());
    assert!(loaded.agents.is_empty());
    assert_eq!(loaded.agent_registry().agents().len(), 0);

    let defaults = temp.path().join("defaults");
    write_config(&defaults, "providers = {}\n");
    let loaded = load_from_roots(None, Some(&defaults)).unwrap();
    assert!(loaded.runtime.providers.is_empty());
    assert_eq!(
        loaded.runtime.context_compaction.trigger,
        ContextCompactionTrigger::Percent { percent: 70 }
    );

    let percent = temp.path().join("percent");
    write_config(
        &percent,
        "[context_compaction]\ntrigger = { percent = 80 }\n",
    );
    assert_eq!(
        load_from_roots(None, Some(&percent))
            .unwrap()
            .runtime
            .context_compaction
            .trigger,
        ContextCompactionTrigger::Percent { percent: 80 }
    );

    let fixed = temp.path().join("fixed");
    write_config(
        &fixed,
        "[context_compaction]\ntrigger = { buffer_tokens = 12000 }\n",
    );
    assert_eq!(
        load_from_roots(None, Some(&fixed))
            .unwrap()
            .runtime
            .context_compaction
            .trigger,
        ContextCompactionTrigger::BufferTokens {
            buffer_tokens: 12_000
        }
    );

    for invalid_percent in [0, 100] {
        let invalid = temp
            .path()
            .join(format!("invalid-percent-{invalid_percent}"));
        write_config(
            &invalid,
            &format!("[context_compaction]\ntrigger = {{ percent = {invalid_percent} }}\n"),
        );
        assert!(matches!(
            load_from_roots(None, Some(&invalid)),
            Err(ConfigError::InvalidRuntime)
        ));
    }

    let both = temp.path().join("both");
    write_config(
        &both,
        "[context_compaction]\ntrigger = { percent = 70 }\nbuffer_tokens = 33000\n",
    );
    let error = load_from_roots(None, Some(&both)).unwrap_err();
    assert!(matches!(&error, ConfigError::Toml(_)));
    assert!(error.to_string().contains("cannot both be set"));

    let versioned = temp.path().join("versioned");
    write_config(&versioned, "schema_version = 10\n");
    let error = load_from_roots(None, Some(&versioned)).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::ConfigSchemaRemoved { line: 1, .. }
    ));
    let message = error.to_string();
    assert!(message.contains("config.toml"), "{message}");
    assert!(
        message.contains("remove the schema_version field"),
        "{message}"
    );

    let unknown = temp.path().join("unknown-top-level");
    write_config(&unknown, "unexpected_setting = true\n");
    let error = load_from_roots(None, Some(&unknown)).unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, ConfigError::Toml(_)));
    assert!(message.contains("config.toml"), "{message}");
    assert!(message.contains("line 1"), "{message}");
    assert!(message.contains("unexpected_setting"), "{message}");

    let removed = temp.path().join("removed");
    write_config(
        &removed,
        "[context_compaction]\nsoft_threshold_percent = 70\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&removed)),
        Err(ConfigError::Toml(_))
    ));

    let removed_hard = temp.path().join("removed-hard");
    write_config(
        &removed_hard,
        "[context_compaction]\nmax_native_context_bytes = 2097152\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&removed_hard)),
        Err(ConfigError::Toml(_))
    ));

    let native = temp.path().join("native-compaction");
    write_config(
        &native,
        &custom("https://example.invalid").replace(
            "native_replay = \"unsupported\"",
            "native_replay = \"unsupported\"\nnative_compaction = \"unsupported\"",
        ),
    );
    assert!(matches!(
        load_from_roots(None, Some(&native)),
        Err(ConfigError::Toml(_))
    ));
}

#[test]
fn agent_internal_mode_and_parent_model_are_strict() {
    let temp = TempDir::new().unwrap();

    let normal_parent = temp.path().join("normal-parent");
    write_config(&normal_parent, "");
    write_agent(
        &normal_parent,
        "normal.md",
        "---\ndescription: Normal\nmode: primary\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\npermissions: {}\n---\nNormal.\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&normal_parent)),
        Err(ConfigError::AgentField {
            field: "models",
            ..
        })
    ));

    let legacy_type = temp.path().join("legacy-type");
    write_config(&legacy_type, "");
    write_agent(
        &legacy_type,
        "legacy.md",
        "---\ntype: internal\ndescription: Legacy\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/model\" }]\npermissions: {}\n---\nLegacy.\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&legacy_type)),
        Err(ConfigError::AgentDocument { .. })
    ));

    let internal = temp.path().join("internal");
    write_config(&internal, "");
    write_agent(
        &internal,
        "approval.md",
        "---\ndescription: Internal approval\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 1000, max_output_tokens: 100 }\npermissions: {}\n---\nApprove safely.\n",
    );
    let loaded = load_from_roots(None, Some(&internal)).unwrap();
    let approval = loaded
        .agents
        .get(&AgentId::new("approval").unwrap())
        .unwrap();
    assert_eq!(
        approval.frontmatter.mode,
        cookie_agent_config::AgentMode::Internal
    );

    let delegation = temp.path().join("internal-delegation");
    write_config(&delegation, "");
    write_agent(
        &delegation,
        "approval.md",
        "---\ndescription: Internal approval\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\npermissions: {}\n---\nApprove.\n",
    );
    write_agent(
        &delegation,
        "primary.md",
        "---\ndescription: Primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/model\" }]\npermissions:\n  delegate:\n    approval: allow\n---\nPrimary.\n",
    );
    assert!(matches!(
        load_from_roots(None, Some(&delegation)),
        Err(ConfigError::IneligibleDelegationTarget { .. })
    ));
}

#[test]
fn timeout_ms_is_rejected_for_non_internal_agents() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("non-internal-timeout");
    write_config(&root, "");
    write_agent(
        &root,
        "primary.md",
        "---\ndescription: Primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/model\" }]\nlimits: { timeout_ms: 1000 }\npermissions: {}\n---\nPrimary.\n",
    );

    let error = load_from_roots(None, Some(&root)).unwrap_err();
    assert!(matches!(error, ConfigError::AgentTimeoutInternalOnly(_)));
    assert!(
        error
            .to_string()
            .contains("timeout_ms is only supported for internal agents")
    );
}

#[test]
fn delegation_runtime_defaults_and_limits_are_strict() {
    let temp = TempDir::new().unwrap();
    let defaults = temp.path().join("delegation-defaults");
    write_config(&defaults, "");
    let loaded = load_from_roots(None, Some(&defaults)).unwrap();
    assert_eq!(loaded.runtime.delegation.max_depth, 3);
    assert_eq!(loaded.runtime.delegation.max_concurrency, Some(4));
    assert_eq!(loaded.runtime.delegation.max_resident_subagents, 20);
    assert_eq!(
        loaded.runtime.delegation.idle_eviction_after,
        std::time::Duration::from_secs(60 * 60)
    );

    let authored = temp.path().join("delegation-authored");
    write_config(
        &authored,
        "[delegation]\nmax_depth = 5\nmax_concurrency = 2\nmax_resident_subagents = 7\nidle_eviction_after = \"90m\"\n",
    );
    let loaded = load_from_roots(None, Some(&authored)).unwrap();
    assert_eq!(loaded.runtime.delegation.max_depth, 5);
    assert_eq!(loaded.runtime.delegation.max_concurrency, Some(2));
    assert_eq!(loaded.runtime.delegation.max_resident_subagents, 7);
    assert_eq!(
        loaded.runtime.delegation.idle_eviction_after,
        std::time::Duration::from_secs(90 * 60)
    );

    let invalid = temp.path().join("delegation-invalid");
    write_config(&invalid, "[delegation]\nmax_concurrency = 0\n");
    assert!(matches!(
        load_from_roots(None, Some(&invalid)),
        Err(ConfigError::InvalidRuntime)
    ));
    write_config(&invalid, "[delegation]\nidle_eviction_after = \"later\"\n");
    assert!(load_from_roots(None, Some(&invalid)).is_err());
}

#[test]
fn pricing_defaults_empty_and_rejects_invalid_rates() {
    let temp = TempDir::new().unwrap();
    let defaults = temp.path().join("pricing-defaults");
    write_config(&defaults, "");
    let loaded = load_from_roots(None, Some(&defaults)).unwrap();
    assert!(loaded.runtime.pricing.models.is_empty());

    let authored = temp.path().join("pricing-authored");
    write_config(
        &authored,
        "[pricing.models.\"custom.test/model\"]\ninput_per_million_usd = \"10000.000000000001\"\noutput_per_million_usd = \"5.0\"\nreasoning_per_million_usd = \"7.5\"\ncache_read_per_million_usd = \"0.125\"\ncache_write_per_million_usd = \"0.000000000001\"\n",
    );
    let loaded = load_from_roots(None, Some(&authored)).unwrap();
    let rates = loaded
        .runtime
        .pricing
        .models
        .get(&"custom.test/model".parse().unwrap())
        .unwrap();
    assert_eq!(
        rates.input_per_million_usd.unwrap().value(),
        10_000_000_000_000_001
    );
    assert_eq!(
        rates.output_per_million_usd.unwrap().value(),
        5_000_000_000_000
    );
    assert_eq!(
        rates.reasoning_per_million_usd.unwrap().value(),
        7_500_000_000_000
    );
    assert_eq!(rates.cache_write_per_million_usd.unwrap().value(), 1);

    let invalid = temp.path().join("pricing-invalid");
    write_config(
        &invalid,
        "[pricing.models.\"custom.test/model\"]\ninput_per_million_usd = \"-1.0\"\n",
    );
    assert!(load_from_roots(None, Some(&invalid)).is_err());
}

#[test]
fn authored_agent_registry_retains_slash_model_ids_without_model_compilation() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("agents-only");
    write_config(&root, "");
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
    let cookie_agent_config::AgentModelRef::Model(model) =
        &input.document.frontmatter.models[0].model
    else {
        panic!("expected concrete model fallback");
    };
    assert_eq!(model.model_id().as_str(), "group/model/deep");
    assert!(matches!(
        input.document.frontmatter.models[0]
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
    write_config(&root, "");
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
    write_config(&user, "");
    write_agent(
        &user,
        "primary.md",
        &agent(
            "Discarded",
            "[{ model: \"openai/group/model\" }, { model: \"openai/group/model\" }]",
        ),
    );
    write_config(&workspace, "");
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
    write_config(&root, "[providers.test]\nsource = \"explicit\"\n");
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::Toml(_))
    ));
}

#[test]
fn invalid_shadowed_provider_is_still_a_hard_error() {
    let temp = TempDir::new().unwrap();
    let user = temp.path().join("user");
    let workspace = temp.path().join("workspace");
    write_config(
        &user,
        "[providers.\"custom.test\"]\nsource = \"custom\"\nunknown = true\n",
    );
    write_config(&workspace, &custom("https://workspace.example/v1"));

    let error = load_from_roots(Some(&user), Some(&workspace)).unwrap_err();
    let ConfigError::Toml(message) = error else {
        panic!("expected TOML error for invalid user provider")
    };
    let expected_path = user.join("config.toml").display().to_string();
    assert!(message.contains(&expected_path), "{message}");
    assert!(message.contains("unknown"), "{message}");
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
        "[providers.openai]\nsource = \"models_dev\"\napi_key = \"a\"\nauth_override = { method = \"bearer-api-key-v1\", values = { api_key = \"b\" } }\n",
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
            "[providers.openai]\nsource = \"models_dev\"\napi_key = \"{SECRET_SENTINEL}\"\nbroken = [\n"
        ),
    );
    assert_redacted(&load_from_roots(None, Some(&parse)).unwrap_err());

    let interpolation = temp.path().join("interpolation-secret");
    write_config(
        &interpolation,
        &format!(
            "[providers.openai]\nsource = \"models_dev\"\napi_key = \"{SECRET_SENTINEL}-${{env:P1_MISSING_SECRET}}\"\n"
        ),
    );
    let _guard = env_lock();
    unsafe { std::env::remove_var("P1_MISSING_SECRET") };
    assert_redacted(&load_from_roots(None, Some(&interpolation)).unwrap_err());

    let unknown = temp.path().join("unknown-secret");
    write_config(
        &unknown,
        &format!(
            "[providers.openai]\nsource = \"models_dev\"\napi_key = \"ok\"\nunknown_secret = \"{SECRET_SENTINEL}\"\n"
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
        "[providers.openai]\nsource = \"models_dev\"\napi_key = \"${env:P1_SUCCESS_SECRET}\"\n",
    );
    let _guard = env_lock();
    unsafe { std::env::set_var("P1_SUCCESS_SECRET", SECRET_SENTINEL) };
    {
        let loaded = load_from_roots(None, Some(&root)).unwrap();
        assert!(!format!("{loaded:?}").contains(SECRET_SENTINEL));
    }
    unsafe { std::env::remove_var("P1_SUCCESS_SECRET") };
}

#[test]
fn agent_presets_replace_shared_documents_and_add_agents() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("presets");
    write_config(&root, "");
    write_agent(
        &root,
        "primary.md",
        &agent("Shared primary", "[{ model: \"custom.test/shared\" }]"),
    );
    write_agent(
        &root,
        "shared-only.md",
        &agent("Shared only", "[{ model: \"custom.test/shared\" }]"),
    );
    write_agent(
        &root,
        "python/primary.md",
        "---\ndescription: Python primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/python\" }]\npermissions:\n  delegate:\n    python-only: allow\n---\nPython prompt.\n",
    );
    write_agent(
        &root,
        "python/python-only.md",
        "---\ndescription: Python only\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/python\" }]\npermissions: {}\n---\nPython worker.\n",
    );

    let loaded = load_from_roots(None, Some(&root)).unwrap();
    assert_eq!(loaded.agents.len(), 2);
    assert_eq!(
        loaded.agents[&AgentId::new("primary").unwrap()]
            .frontmatter
            .description,
        "Shared primary"
    );
    let python = &loaded.agent_presets["python"];
    assert_eq!(python.len(), 3);
    assert_eq!(
        python[&AgentId::new("primary").unwrap()]
            .frontmatter
            .description,
        "Python primary"
    );
    assert!(python.contains_key(&AgentId::new("shared-only").unwrap()));
    assert!(python.contains_key(&AgentId::new("python-only").unwrap()));
}

#[test]
fn unused_agent_presets_are_loaded_strictly() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("invalid-unused-preset");
    write_config(&root, "");
    write_agent(&root, "unused/broken.md", "not frontmatter");
    assert!(matches!(
        load_from_roots(None, Some(&root)),
        Err(ConfigError::AgentDocument { .. })
    ));
}

#[test]
fn reserved_agent_rules_apply_inside_presets() {
    let temp = TempDir::new().unwrap();
    let default_root = temp.path().join("preset-default");
    write_config(&default_root, "");
    write_agent(
        &default_root,
        "python/default.md",
        &agent("Reserved", "[{ model: \"custom.test/model\" }]"),
    );
    assert!(matches!(
        load_from_roots(None, Some(&default_root)),
        Err(ConfigError::ReservedAgentId(id)) if id.as_str() == "default"
    ));

    let internal_root = temp.path().join("preset-internal");
    write_config(&internal_root, "");
    write_agent(
        &internal_root,
        "python/approval.md",
        &agent("Approval", "[{ model: \"custom.test/model\" }]"),
    );
    assert!(matches!(
        load_from_roots(None, Some(&internal_root)),
        Err(ConfigError::AgentField { agent, field: "mode" }) if agent.as_str() == "approval"
    ));
}

#[test]
fn preset_names_and_directory_depth_are_strict() {
    let temp = TempDir::new().unwrap();
    let invalid_name = temp.path().join("invalid-preset-name");
    write_config(&invalid_name, "");
    write_agent(
        &invalid_name,
        "Python/primary.md",
        &agent("Primary", "[{ model: \"custom.test/model\" }]"),
    );
    assert!(matches!(
        load_from_roots(None, Some(&invalid_name)),
        Err(ConfigError::AgentPresetName { .. })
    ));

    let nested = temp.path().join("nested-preset");
    write_config(&nested, "");
    write_agent(
        &nested,
        "python/deeper/primary.md",
        &agent("Primary", "[{ model: \"custom.test/model\" }]"),
    );
    assert!(matches!(
        load_from_roots(None, Some(&nested)),
        Err(ConfigError::UnsafePath)
    ));

    let non_markdown = temp.path().join("non-markdown-preset");
    write_config(&non_markdown, "");
    write_agent(&non_markdown, "python/readme.txt", "ignored");
    assert!(matches!(
        load_from_roots(None, Some(&non_markdown)),
        Err(ConfigError::UnsafePath)
    ));
}

#[test]
fn preset_agents_match_shared_crlf_and_byte_limits() {
    let temp = TempDir::new().unwrap();
    let crlf = temp.path().join("preset-crlf");
    write_config(&crlf, "");
    let document = agent("CRLF preset", "[{ model: \"custom.test/model\" }]").replace('\n', "\r\n");
    write_agent(&crlf, "python/primary.md", &document);
    let loaded = load_from_roots(None, Some(&crlf)).unwrap();
    assert_eq!(
        loaded.agent_presets["python"][&AgentId::new("primary").unwrap()].body,
        "Prompt.\n"
    );

    let oversized = temp.path().join("preset-oversized");
    write_config(&oversized, "");
    write_agent(&oversized, "python/primary.md", &"x".repeat(256 * 1024 + 1));
    assert!(matches!(
        load_from_roots(None, Some(&oversized)),
        Err(ConfigError::TooLarge(_))
    ));
}

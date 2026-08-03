use std::{fs, path::PathBuf, sync::Arc};

use cookie_agent_config::{
    ConfigError, DepthLimit, RuleSource, load_layered, simple_wildcard_match,
};
use cookie_agent_models::{
    ModelEntry, ModelSet, RequestDefaults, ScriptedModel, configuration_fingerprint,
};
use oven_sdk::{
    AdapterId, LanguageModelDescriptor, ModelCapabilities, ModelId, ModelIdentity, ProviderId,
};
use tempfile::TempDir;

fn model_block() -> &'static str {
    r#"
[models.test]
provider_id = "test.gateway"
model_id = "arbitrary-model"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"

[models.test.auth]
type = "none"

[models.test.capabilities]
features = []
cancellation = "local_only"
compaction = "unsupported"

[models.test.capabilities.limits]

[models.test.capabilities.modalities]
input = ["text"]
output = ["text"]

[models.test.capabilities.media]
input = {}

[models.test.capabilities.replay]
policy = "never"
capability = "unsupported"
reasoning = false

[models.test.settings]
adapter_id = "cookie.test.chat"
system_message_role = "system"
max_tokens_field = "max_tokens"
stream_usage = false
structured_output = "unsupported"
reasoning_field = "none"
"#
}

fn base_config() -> String {
    format!(
        r#"{}
[agents.primary]
type = "primary"
models = ["test"]
tools = ["read", "bash"]

[agents.primary.delegation]
enabled = true
allowed_profiles = ["worker"]
limit = 3

[agents.worker]
type = "subagent"
tools = ["read"]

[agents.worker.delegation]
enabled = false
limit = 4
"#,
        model_block()
    )
}

fn files(user: &str, workspace: &str) -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().unwrap();
    let user_path = temp.path().join("user.toml");
    let workspace_path = temp.path().join("workspace.toml");
    fs::write(&user_path, user).unwrap();
    fs::write(&workspace_path, workspace).unwrap();
    (temp, user_path, workspace_path)
}

fn load(source: &str) -> cookie_agent_config::Config {
    let (_temp, user, workspace) = files("", source);
    load_layered(Some(&user), Some(&workspace)).unwrap()
}

fn load_error(source: &str) -> ConfigError {
    let (_temp, user, workspace) = files("", source);
    load_layered(Some(&user), Some(&workspace)).unwrap_err()
}

#[test]
fn layers_replace_normal_arrays_and_append_permission_rules_with_provenance() {
    let user = format!(
        r#"{}
[[permissions.rules]]
id = "global-user"
action = "read"
resource = "*"
effect = "allow"

[[agents.primary.permissions.rules]]
id = "profile-user"
action = "bash"
resource = "git status *"
effect = "allow"
"#,
        base_config()
    );
    let workspace = r#"
[server]
port = 8000

[agents.primary]
tools = ["grep"]

[[permissions.rules]]
id = "global-workspace"
action = "write"
resource = "*"
effect = "ask"

[[agents.primary.permissions.rules]]
id = "profile-workspace"
action = "bash"
resource = "git push *"
effect = "deny"
"#;
    let (_temp, user, workspace) = files(&user, workspace);
    let config = load_layered(Some(&user), Some(&workspace)).unwrap();
    assert_eq!(config.server.port, 8000);
    assert_eq!(config.agents["primary"].tools, ["grep"]);
    assert_eq!(
        config
            .permissions
            .rules
            .iter()
            .map(|rule| (rule.id.as_str(), rule.source))
            .collect::<Vec<_>>(),
        [
            ("global-user", RuleSource::User),
            ("global-workspace", RuleSource::Workspace),
        ]
    );
    assert_eq!(
        config.agents["primary"]
            .permissions
            .rules
            .iter()
            .map(|rule| (rule.id.as_str(), rule.source))
            .collect::<Vec<_>>(),
        [
            ("profile-user", RuleSource::User),
            ("profile-workspace", RuleSource::Workspace),
        ]
    );
}

#[test]
fn policy_materialization_freezes_aliases_inheritance_permissions_and_depth() {
    let source = format!(
        r#"{}
[[permissions.rules]]
id = "global"
action = "read"
resource = "*"
effect = "allow"

[[agents.primary.permissions.rules]]
id = "primary"
action = "bash"
resource = "git status *"
effect = "allow"

[[agents.worker.permissions.rules]]
id = "worker"
action = "bash"
resource = "*"
effect = "deny"
"#,
        base_config()
    );
    let config = load(&source);
    let model_set = config.build_model_set().unwrap();
    let primary = config.materialize_policy(&model_set, "primary").unwrap();
    assert_eq!(primary.models[0].alias, "test");
    assert_eq!(primary.delegation.depth_limit, DepthLimit::Finite(3));
    assert_eq!(
        primary
            .permissions
            .rules
            .iter()
            .map(|rule| (rule.id.as_str(), rule.source))
            .collect::<Vec<_>>(),
        [
            ("global", RuleSource::Workspace),
            ("primary", RuleSource::Profile),
        ]
    );

    let worker = config
        .materialize_child_policy(&model_set, "worker", &primary)
        .unwrap();
    assert_eq!(worker.models, primary.models);
    assert_eq!(worker.delegation.depth_limit, DepthLimit::Finite(2));
    assert_eq!(
        worker
            .permissions
            .rules
            .iter()
            .map(|rule| (rule.id.as_str(), rule.source))
            .collect::<Vec<_>>(),
        [
            ("global", RuleSource::Workspace),
            ("worker", RuleSource::Profile),
        ]
    );
}

#[test]
fn disabled_profiles_cannot_materialize_models_tools_or_permissions() {
    let source = format!(
        r#"{}
[agents.disabled]
type = "primary"
enabled = false
models = ["test"]
tools = ["read"]

[[agents.disabled.permissions.rules]]
id = "disabled-read"
action = "read"
resource = "*"
effect = "allow"

[agents.parent]
type = "primary"
models = ["test"]

[agents.child]
type = "subagent"
enabled = false
tools = ["read"]

[[agents.child.permissions.rules]]
id = "disabled-child-read"
action = "read"
resource = "*"
effect = "allow"
"#,
        model_block()
    );
    let config = load(&source);
    let model_set = config.build_model_set().unwrap();

    assert!(matches!(
        config.materialize_policy(&model_set, "disabled"),
        Err(ConfigError::DisabledProfile(profile)) if profile == "disabled"
    ));

    let parent = config.materialize_policy(&model_set, "parent").unwrap();
    assert!(matches!(
        config.materialize_child_policy(&model_set, "child", &parent),
        Err(ConfigError::DisabledProfile(profile)) if profile == "child"
    ));
}

#[test]
fn current_schema_validation_covers_profiles_delegation_permissions_and_limits() {
    let model = model_block();
    let cases = [
        ("[agents.root]\ntype = \"primary\"", "empty model chain"),
        (
            "[agents.root]\ntype = \"primary\"\nmodels = [\"missing\"]",
            "unknown model alias",
        ),
        (
            &format!(
                "{model}\n[agents.root]\ntype = \"primary\"\nmodels = [\"test\"]\n[agents.root.delegation]\nenabled = true"
            ),
            "no allowed profiles",
        ),
        (
            &format!(
                "{model}\n[agents.root]\ntype = \"primary\"\nmodels = [\"test\"]\n[agents.root.delegation]\nallowed_profiles = [\"missing\"]"
            ),
            "allows unknown profile",
        ),
        (
            &format!(
                "{model}\n[agents.root]\ntype = \"primary\"\nmodels = [\"test\"]\n[agents.other]\ntype = \"primary\"\nmodels = [\"test\"]\n[agents.root.delegation]\nallowed_profiles = [\"other\"]"
            ),
            "primary-only",
        ),
        (
            &format!(
                "{model}\n[agents.root]\ntype = \"primary\"\nmodels = [\"test\"]\n[agents.internal]\ntype = \"internal\"\n[agents.root.delegation]\nallowed_profiles = [\"internal\"]"
            ),
            "allows internal profile",
        ),
        (
            "[agents.internal]\ntype = \"internal\"\nenabled = false",
            "internal agents cannot be disabled",
        ),
        (
            "[[permissions.rules]]\nid = \"bad\"\naction = \"bash\"\nresource = \"*\"\neffect = \"sometimes\"",
            "invalid permission effect",
        ),
        ("[tool_output]\nmax_lines = 0", "tool_output.max_lines"),
    ];
    for (source, expected) in cases {
        let error = load_error(source);
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn internal_agent_defaults_are_explicit_bounded_and_inheriting() {
    let config = load("");
    assert_eq!(config.schema_version, 5);
    let internal = &config.internal_agents;
    assert!(internal.approval.models.is_empty());
    assert!(internal.context_compaction.profile.models.is_empty());
    assert!(internal.session_title.profile.models.is_empty());
    assert!(internal.context_compaction.max_summary_bytes <= 2 * 1024 * 1024);
    assert!(internal.context_compaction.max_native_context_bytes <= 2 * 1024 * 1024);
}

#[test]
fn wildcard_and_tool_output_policy_behave_as_documented() {
    assert!(simple_wildcard_match("git status *", "git status"));
    assert!(simple_wildcard_match(
        "git status *",
        "git status --porcelain"
    ));
    assert!(simple_wildcard_match("*", "dir/nested/file"));
    assert!(simple_wildcard_match("file?.txt", "file1.txt"));
    assert!(!simple_wildcard_match("file?.txt", "file12.txt"));

    let source = format!(
        "{}\n[agents.primary]\ntype = \"primary\"\nmodels = [\"test\"]\n[tool_output]\nmax_lines = 7\nmax_bytes = 99",
        model_block()
    );
    let config = load(&source);
    let model_set = config.build_model_set().unwrap();
    let policy = config.materialize_policy(&model_set, "primary").unwrap();
    assert_eq!(policy.result_limits.tool_output_max_lines, 7);
    assert_eq!(policy.result_limits.tool_output_max_bytes, 99);
}

#[test]
fn policy_materialization_rejects_an_installed_static_alias_with_different_behavior() {
    let config = load(&base_config());
    let descriptor = LanguageModelDescriptor::new(
        ModelIdentity::new(
            ProviderId::new("installed.provider"),
            ModelId::new("installed-model"),
        )
        .unwrap(),
        AdapterId::new("installed.adapter"),
        ModelCapabilities::conservative(),
    )
    .unwrap();
    let entry = ModelEntry::new(
        "test",
        Arc::new(ScriptedModel::new(descriptor.clone(), [])),
        RequestDefaults::default(),
    )
    .unwrap();
    let fingerprint = configuration_fingerprint(&config.models).unwrap();
    let model_set = ModelSet::new([("test".into(), entry)], fingerprint).unwrap();

    let error = config
        .materialize_policy(&model_set, "primary")
        .unwrap_err();
    assert!(error.to_string().contains("installed model set"));
}

#[test]
fn schema_v5_internal_agents_accept_exact_catalog_aliases_and_bound_every_policy() {
    let source = r#"
schema_version = 5

[internal_agents.approval]
models = ["anthropic/claude-opus-4-6"]
max_input_tokens = 8000
max_output_tokens = 512
timeout_ms = 15000

[internal_agents.context_compaction]
soft_threshold_percent = 72
hard_threshold_percent = 90
target_percent = 55
max_summary_bytes = 1048576
max_native_context_bytes = 2097152
persistence = "native_preferred"

[internal_agents.context_compaction.profile]
models = ["openai/gpt-5.4"]
max_input_tokens = 32000
max_output_tokens = 4096
timeout_ms = 45000

[internal_agents.session_title.profile]
models = ["cohere/command-a-03-2025"]
max_input_tokens = 2048
max_output_tokens = 64
timeout_ms = 5000

[internal_agents.session_title.policy]
max_chars = 72
max_input_messages = 3
generate_on_first_turn = true
fallback_to_input_excerpt = true
"#;
    let config = load(source);
    assert_eq!(config.schema_version, 5);
    assert_eq!(
        config.internal_agents.approval.models,
        ["anthropic/claude-opus-4-6"]
    );
    assert_eq!(
        config
            .internal_agents
            .context_compaction
            .max_native_context_bytes,
        2 * 1024 * 1024
    );
    assert_eq!(config.internal_agents.session_title.policy.max_chars, 72);
}

#[test]
fn schema_v5_rejects_old_versions_unbounded_internal_persistence_and_connect_secrets() {
    for (source, expected) in [
        ("schema_version = 4", "expected 5"),
        (
            "[internal_agents.context_compaction]\nmax_summary_bytes = 2097153",
            "2 MiB",
        ),
        (
            "[internal_agents.context_compaction]\ntarget_percent = 80\nsoft_threshold_percent = 70\nhard_threshold_percent = 90",
            "thresholds",
        ),
        (
            "[provider_connect]\nprovider_id = \"anthropic\"\nsecret = \"must-not-live-in-config\"",
            "extraction failed",
        ),
    ] {
        let error = load_error(source);
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!format!("{error:?}").contains("must-not-live-in-config"));
    }
}

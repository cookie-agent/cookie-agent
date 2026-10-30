//! `${default_model}`, `${small_model}`, and `${parent_model}` resolved
//! against the configuration, the built-in agents' chains, and reload
//! retargeting.

use cookie_agent_protocol::{AgentDescriptor, AgentId, InternalAgentKind, RunSelection};

use super::support::*;

/// Two models of `custom.test`: `a-model` with variants `zeta`, `alpha`,
/// and `precise` (its default), and `z-model` without variants.
fn fixture_with(primary: Option<&str>) -> Fixture {
    synthetic_default_fixture_with_config(primary, "http://127.0.0.1:9/v1", "").expect("engine")
}

fn project(fixture: &Fixture) -> std::path::PathBuf {
    fixture._directory.path().join(".cookie-agent")
}

async fn write_agent(fixture: &Fixture, name: &str, text: &str) {
    let agents = project(fixture).join("agents");
    if !agents.exists() {
        create_private_test_dir(&agents);
    }
    write_private_test_file(&agents.join(name), text);
    fixture
        .engine
        .reload_configuration()
        .await
        .expect("reloaded configuration");
}

async fn set(fixture: &Fixture, key: &str, value: serde_json::Value) {
    let revision = fixture
        .engine
        .configuration_documents()
        .await
        .expect("documents")
        .revision;
    fixture
        .engine
        .apply_config_changes(
            revision,
            vec![crate::ConfigChange::Set {
                key: vec![key.to_owned()],
                value,
            }],
        )
        .await
        .expect("applied change");
}

fn descriptor(fixture: &Fixture, id: &str) -> AgentDescriptor {
    fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == id && agent.preset.is_none())
        .unwrap_or_else(|| panic!("agent `{id}`"))
}

fn chain(descriptor: &AgentDescriptor) -> Vec<(String, Option<String>)> {
    descriptor
        .resolved_fallback
        .iter()
        .map(|selection| {
            (
                selection.model.to_string(),
                selection
                    .variant
                    .as_ref()
                    .map(|variant| variant.to_string()),
            )
        })
        .collect()
}

fn pair(model: &str, variant: Option<&str>) -> (String, Option<String>) {
    (model.to_owned(), variant.map(str::to_owned))
}

const PRIMARY: &str = "---\ndescription: Primary\nmode: primary\nmodels:\n  - ${default_model}\n  - { model: \"${small_model}\", variant: zeta }\n  - custom.test/z-model\npermissions: {}\n---\nWork.\n";

#[tokio::test]
async fn variables_resolve_with_their_variants_and_entry_overrides() {
    let fixture = fixture_with(Some(PRIMARY));
    set(
        &fixture,
        "default_model",
        serde_json::json!({"model": "custom.test/a-model", "variant": "alpha"}),
    )
    .await;
    let primary = descriptor(&fixture, "primary");
    // `${small_model}` is unset: its entry is skipped and reported.
    assert_eq!(
        chain(&primary),
        [
            pair("custom.test/a-model", Some("alpha")),
            pair("custom.test/z-model", None)
        ]
    );
    assert_eq!(primary.unset_models, ["small_model"]);
    assert!(primary.runnable_as_root);
    assert!(
        fixture
            .engine
            .configuration_status()
            .diagnostics
            .contains(&"agent `primary` skips a `models` entry: `small_model` is not set".into())
    );

    // A bare key means the model's default variant; the entry's variant
    // overrides the one the variable carries.
    set(
        &fixture,
        "default_model",
        serde_json::json!("custom.test/a-model"),
    )
    .await;
    set(
        &fixture,
        "small_model",
        serde_json::json!("custom.test/a-model"),
    )
    .await;
    let primary = descriptor(&fixture, "primary");
    assert_eq!(
        chain(&primary),
        [
            pair("custom.test/a-model", Some("precise")),
            pair("custom.test/z-model", None)
        ],
        "the small model's entry names a model already in the chain"
    );
    assert!(primary.unset_models.is_empty());

    set(
        &fixture,
        "default_model",
        serde_json::json!("custom.test/z-model"),
    )
    .await;
    assert_eq!(
        chain(&descriptor(&fixture, "primary")),
        [
            pair("custom.test/z-model", None),
            pair("custom.test/a-model", Some("zeta")),
        ],
        "changing the field retargets the agent without touching it"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn an_agent_left_with_no_entry_is_unavailable_and_names_the_field() {
    let fixture = fixture_with(Some(
        "---\ndescription: Primary\nmode: primary\nmodels: [\"${default_model}\"]\npermissions: {}\n---\nWork.\n",
    ));
    let primary = descriptor(&fixture, "primary");
    assert!(primary.resolved_fallback.is_empty());
    assert!(!primary.runnable_as_root);
    assert_eq!(primary.unset_models, ["default_model"]);
    assert!(
        fixture
            .engine
            .configuration_status()
            .diagnostics
            .contains(&"agent `primary` has no model: `default_model` is not set".into())
    );

    set(
        &fixture,
        "default_model",
        serde_json::json!("custom.test/z-model"),
    )
    .await;
    let primary = descriptor(&fixture, "primary");
    assert!(primary.runnable_as_root);
    assert_eq!(chain(&primary), [pair("custom.test/z-model", None)]);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_configured_model_that_is_not_available_leaves_the_agent_unavailable() {
    let fixture = fixture_with(Some(
        "---\ndescription: Primary\nmode: primary\nmodels: [\"${default_model}\"]\npermissions: {}\n---\nWork.\n",
    ));
    set(
        &fixture,
        "default_model",
        serde_json::json!("nowhere.test/gone"),
    )
    .await;
    let primary = descriptor(&fixture, "primary");
    assert!(primary.unset_models.is_empty());
    assert!(!primary.runnable_as_root);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn the_built_in_default_agent_prefers_the_default_model() {
    let fixture = fixture_with(None);
    let default = descriptor(&fixture, "default");
    assert_eq!(default.unset_models, ["default_model"]);
    assert_eq!(
        chain(&default),
        [pair("custom.test/a-model", Some("precise"))]
    );

    set(
        &fixture,
        "default_model",
        serde_json::json!({"model": "custom.test/z-model"}),
    )
    .await;
    let default = descriptor(&fixture, "default");
    assert!(default.unset_models.is_empty());
    assert_eq!(
        chain(&default),
        [
            pair("custom.test/z-model", None),
            pair("custom.test/a-model", Some("precise"))
        ]
    );

    set(
        &fixture,
        "default_model",
        serde_json::json!({"model": "custom.test/a-model", "variant": "alpha"}),
    )
    .await;
    assert_eq!(
        chain(&descriptor(&fixture, "default")),
        [pair("custom.test/a-model", Some("alpha"))],
        "the first available model is the default model itself"
    );
    fixture.engine.shutdown().await;
}

fn internal_chain(fixture: &Fixture, id: &str) -> Vec<String> {
    let runtime = fixture.engine.current_runtime();
    let registry = runtime.agents_for_preset(None).expect("shared agents");
    registry
        .get(&AgentId::new(id).expect("agent ID"))
        .expect("internal agent")
        .resolved_fallback
        .iter()
        .map(|fallback| match fallback {
            crate::runtime_snapshot::ResolvedAgentFallback::Selection { selection, .. } => {
                format!("{}:{:?}", selection.model, selection.variant)
            }
            crate::runtime_snapshot::ResolvedAgentFallback::ParentModel { variant, .. } => {
                format!("parent:{variant:?}")
            }
        })
        .collect()
}

#[tokio::test]
async fn approval_and_title_use_the_utility_model_then_the_parent_and_compaction_the_parent() {
    let fixture = fixture_with(None);
    for id in ["approval", "title"] {
        assert_eq!(internal_chain(&fixture, id), ["parent:None"], "{id}");
        assert_eq!(descriptor(&fixture, id).unset_models, ["utility_model"]);
    }
    assert_eq!(internal_chain(&fixture, "compaction"), ["parent:None"]);

    set(
        &fixture,
        "utility_model",
        serde_json::json!("custom.test/z-model"),
    )
    .await;
    for id in ["approval", "title"] {
        assert_eq!(
            internal_chain(&fixture, id),
            ["custom.test/z-model:None", "parent:None"],
            "{id}"
        );
    }
    assert_eq!(internal_chain(&fixture, "compaction"), ["parent:None"]);
    fixture.engine.shutdown().await;
}

fn root_selection(fixture: &Fixture, agent: &str) -> RunSelection {
    let descriptor = descriptor(fixture, agent);
    RunSelection {
        agent: descriptor.id.clone(),
        model: descriptor.resolved_fallback[0].clone(),
        preset: None,
    }
}

fn policy_models(
    policy: &crate::runtime::internal_agents::FrozenInternalAgentPolicy,
) -> Vec<String> {
    policy
        .models
        .iter()
        .map(|binding| {
            format!(
                "{}:{:?}",
                binding.selection.model, binding.selection.variant
            )
        })
        .collect()
}

#[tokio::test]
async fn parent_model_takes_the_entry_variant_when_frozen_and_live() {
    let fixture = fixture_with(Some(
        "---\ndescription: Primary\nmode: primary\nmodels: [custom.test/a-model]\npermissions: {}\n---\nWork.\n",
    ));
    write_agent(
        &fixture,
        "title.md",
        "---\ndescription: Titles\nmode: internal\nmodels:\n  - { model: \"${parent_model}\", variant: alpha }\n---\nName the session.\n",
    )
    .await;
    let selection = root_selection(&fixture, "primary");
    let mut policy = frozen_root_policy(&fixture, &selection);
    let parent = policy.active_suffix(0).first().cloned();
    assert_eq!(
        parent
            .as_ref()
            .and_then(|binding| binding.selection.variant.clone())
            .map(|v| v.to_string()),
        Some("precise".into())
    );
    let live = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::SessionTitle, &policy, parent.as_ref())
        .expect("live title policy");
    assert_eq!(
        policy_models(&live),
        [r#"custom.test/a-model:Some(VariantId("alpha"))"#]
    );

    policy.internal_agents = fixture
        .engine
        .freeze_internal_agent_definitions(&policy)
        .expect("frozen definitions");
    let title = policy
        .internal_agents
        .iter()
        .find(|definition| definition.kind == InternalAgentKind::SessionTitle)
        .expect("title definition");
    assert!(matches!(
        &title.fallbacks[0],
        cookie_agent_protocol::FrozenInternalAgentFallback::ParentModel { variant: Some(_) }
    ));
    let frozen = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::SessionTitle, &policy, parent.as_ref())
        .expect("frozen title policy");
    assert_eq!(policy_models(&frozen), policy_models(&live));

    // A variant the parent's model does not have is skipped.
    write_agent(
        &fixture,
        "title.md",
        "---\ndescription: Titles\nmode: internal\nmodels:\n  - { model: \"${parent_model}\", variant: missing }\n---\nName the session.\n",
    )
    .await;
    let policy = frozen_root_policy(&fixture, &selection);
    let skipped = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::SessionTitle, &policy, parent.as_ref())
        .expect("title policy");
    assert!(skipped.models.is_empty());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn without_a_parent_model_the_reviewer_runs_on_the_utility_model_alone() {
    // An ACP agent's run has no model of its own: its reviewer has only
    // `${utility_model}`, and without it nothing, so `ask` goes to the user.
    let fixture = fixture_with(Some(
        "---\ndescription: Primary\nmode: primary\nmodels: [custom.test/a-model]\npermissions: {}\n---\nWork.\n",
    ));
    let selection = root_selection(&fixture, "primary");
    let policy = frozen_root_policy(&fixture, &selection);
    let reviewer = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::Approval, &policy, None)
        .expect("reviewer policy");
    assert!(reviewer.models.is_empty());

    set(
        &fixture,
        "utility_model",
        serde_json::json!("custom.test/z-model"),
    )
    .await;
    let selection = root_selection(&fixture, "primary");
    let policy = frozen_root_policy(&fixture, &selection);
    let reviewer = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::Approval, &policy, None)
        .expect("reviewer policy");
    assert_eq!(policy_models(&reviewer), ["custom.test/z-model:None"]);
    fixture.engine.shutdown().await;
}

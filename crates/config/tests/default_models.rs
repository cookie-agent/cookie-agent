//! `default_model` and `small_model`, the `${default_model}`,
//! `${small_model}`, and `${parent_model}` variables in `models` entries,
//! and plain-string entries.

use std::{fs, path::Path};

use cookie_agent_config::{
    AgentModelRef, ConfigError, ConfiguredModel, LoadedConfiguration, load_from_user_root,
};
use cookie_agent_identity::{AgentId, ConfiguredVariantRef, VariantId};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn write_agent(root: &Path, name: &str, text: &str) {
    let agents = root.join("agents");
    fs::create_dir_all(&agents).expect("agents directory");
    fs::write(agents.join(name), text).expect("agent document");
}

fn load(root: &tempfile::TempDir) -> Result<LoadedConfiguration, ConfigError> {
    load_from_user_root(Some(root.path()))
}

fn named(variant: &str) -> Option<ConfiguredVariantRef> {
    Some(ConfiguredVariantRef::Named(
        VariantId::new(variant).expect("variant"),
    ))
}

#[test]
fn both_fields_take_a_key_or_a_table() {
    let loaded = load(&root(
        "default_model = { model = \"anthropic/claude-test\", variant = \"high\" }\nsmall_model = \"deepseek/deepseek-flash\"\n",
    ))
    .expect("fields");
    assert_eq!(
        loaded.runtime.default_model,
        Some(ConfiguredModel {
            model: "anthropic/claude-test".parse().unwrap(),
            variant: named("high"),
        })
    );
    assert_eq!(
        loaded.runtime.small_model,
        Some(ConfiguredModel {
            model: "deepseek/deepseek-flash".parse().unwrap(),
            variant: None,
        })
    );

    let table_without_variant =
        load(&root("small_model = { model = \"openai/gpt-test\" }\n")).expect("table");
    assert_eq!(
        table_without_variant.runtime.small_model,
        Some(ConfiguredModel {
            model: "openai/gpt-test".parse().unwrap(),
            variant: None,
        })
    );
    let base = load(&root(
        "default_model = { model = \"openai/gpt-test\", variant = \"base\" }\n",
    ))
    .expect("base variant");
    assert_eq!(
        base.runtime.default_model.unwrap().variant,
        Some(ConfiguredVariantRef::Base)
    );
}

#[test]
fn the_fields_are_absent_unless_written() {
    let loaded = load(&root("[loop_warning]\nenabled = true\n")).expect("config");
    assert_eq!(loaded.runtime.default_model, None);
    assert_eq!(loaded.runtime.small_model, None);
}

#[test]
fn the_fields_are_strict() {
    for config in [
        "default_model = { model = \"openai/gpt-test\", effort = \"high\" }\n",
        "default_model = { variant = \"high\" }\n",
        "small_model = \"not a model key\"\n",
        "small_model = 3\n",
        "small_model = [\"openai/gpt-test\"]\n",
        "default_model = { model = \"openai/gpt-test\", variant = \"\" }\n",
    ] {
        assert!(load(&root(config)).is_err(), "{config:?} must be refused");
    }
}

#[test]
fn a_model_whose_provider_is_not_connected_still_loads() {
    // Availability is a runtime matter: any well-formed key loads.
    let loaded = load(&root("default_model = \"nowhere.test/unknown-model\"\n")).expect("load");
    assert!(loaded.runtime.default_model.is_some());
}

#[test]
fn models_entries_may_be_strings_and_name_the_variables() {
    let config = root("");
    write_agent(
        config.path(),
        "helper.md",
        "---\ndescription: Helper\nmode: subagent\nmodels:\n  - ${default_model}\n  - { model: \"${small_model}\", variant: max }\n  - deepseek/deepseek-flash\n  - { model: deepseek/deepseek-flash-2, variant: base }\n---\nHelp.\n",
    );
    let loaded = load(&config).expect("string entries");
    let models = &loaded.agents[&AgentId::new("helper").unwrap()]
        .frontmatter
        .models;
    let written = models
        .iter()
        .map(|entry| (entry.model.clone(), entry.variant.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        written,
        [
            (AgentModelRef::DefaultModel, None),
            (AgentModelRef::SmallModel, named("max")),
            (
                AgentModelRef::Model("deepseek/deepseek-flash".parse().unwrap()),
                None
            ),
            (
                AgentModelRef::Model("deepseek/deepseek-flash-2".parse().unwrap()),
                Some(ConfiguredVariantRef::Base)
            ),
        ]
    );
    assert_eq!(models[0].model.config_field(), Some("default_model"));
    assert_eq!(models[1].model.config_field(), Some("small_model"));
    assert_eq!(models[2].model.config_field(), None);
}

#[test]
fn other_entry_shapes_stay_errors() {
    for models in [
        "[3]",
        "[{ model: openai/gpt-test, effort: high }]",
        "[[openai/gpt-test]]",
        "[\"not a model\"]",
        "[\"${unknown_model}\"]",
    ] {
        let config = root("");
        write_agent(
            config.path(),
            "helper.md",
            &format!("---\ndescription: Helper\nmode: subagent\nmodels: {models}\n---\nHelp.\n"),
        );
        assert!(load(&config).is_err(), "models: {models} must be refused");
    }
}

#[test]
fn duplicates_compare_entries_as_written() {
    let config = root("default_model = \"openai/gpt-test\"\n");
    write_agent(
        config.path(),
        "primary.md",
        "---\ndescription: Primary\nmode: primary\nmodels: [\"${default_model}\", openai/gpt-test]\n---\nWork.\n",
    );
    load(&config).expect("a variable and the key it names may both appear");

    let repeated = root("");
    write_agent(
        repeated.path(),
        "primary.md",
        "---\ndescription: Primary\nmode: primary\nmodels: [\"${small_model}\", { model: \"${small_model}\", variant: max }]\n---\nWork.\n",
    );
    assert!(matches!(
        load(&repeated),
        Err(ConfigError::AgentField {
            field: "models",
            ..
        })
    ));
}

#[test]
fn parent_model_takes_a_variant_override_but_stays_internal() {
    let internal = root("");
    write_agent(
        internal.path(),
        "title.md",
        "---\ndescription: Titles\nmode: internal\nmodels: [\"${small_model}\", { model: \"${parent_model}\", variant: low }]\n---\n{{default_prompt}}\n",
    );
    let loaded = load(&internal).expect("a variant beside ${parent_model}");
    let models = &loaded.agents[&AgentId::new("title").unwrap()]
        .frontmatter
        .models;
    assert_eq!(models[1].model, AgentModelRef::ParentModel);
    assert_eq!(models[1].variant, named("low"));

    let primary = root("");
    write_agent(
        primary.path(),
        "primary.md",
        "---\ndescription: Primary\nmode: primary\nmodels: [\"${parent_model}\"]\n---\nWork.\n",
    );
    assert!(matches!(
        load(&primary),
        Err(ConfigError::AgentField {
            field: "models",
            ..
        })
    ));
}

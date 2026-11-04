//! The web form's catalog of settings against what the configuration loads:
//! a key it names exists, and a default it shows is the one the daemon uses.

use cookie_agent_client_state::config_form::{FieldDefault, FieldKind, SECTIONS};
use cookie_agent_config::{ConfigPath, ConfigTree, DEFERRED_BUILTIN_GROUPS, load_from_tree};

/// The configuration a `config.toml` setting `key` to `value` loads to, as
/// text; `Err` when it does not load.
fn loaded(key: &[&str], value: Option<&serde_json::Value>) -> Result<String, String> {
    let mut document = toml::Table::new();
    if let Some(value) = value {
        let mut value = toml::Value::try_from(value).map_err(|error| error.to_string())?;
        for name in key.iter().skip(1).rev() {
            value = toml::Value::Table(toml::Table::from_iter([((*name).to_owned(), value)]));
        }
        document.insert(key[0].to_owned(), value);
    }
    let mut tree = ConfigTree::new(None);
    tree.put(
        ConfigPath::config(),
        toml::to_string(&document).unwrap().into_bytes(),
    )
    .map_err(|error| error.to_string())?;
    load_from_tree(&tree)
        .map(|loaded| format!("{:?}", loaded.runtime))
        .map_err(|error| error.to_string())
}

/// A value the configuration takes for a text or list field with no default.
fn sample(path: &str) -> &'static str {
    match path {
        "server.url" => "ws://127.0.0.1:1/ws",
        "server.page_url" | "share.url" => "https://example.com",
        "notifications.contact" => "mailto:me@example.com",
        "agents.default" => "primary",
        "share.listen" => "127.0.0.1:7420",
        "shell_environment.shell" => "/bin/sh",
        other => panic!("no sample for {other}"),
    }
}

#[test]
fn an_advertised_default_is_the_daemons_and_every_key_loads() {
    let unset = loaded(&[], None).expect("an empty file loads");
    for field in SECTIONS.iter().flat_map(|section| section.fields) {
        let path = field.path();
        match field.default {
            FieldDefault::Value(text) => {
                let value = field.parse(text).unwrap().expect("a default value");
                assert_eq!(
                    loaded(field.key, Some(&value)).as_deref(),
                    Ok(unset.as_str()),
                    "{path} = {text} is not the default"
                );
            }
            FieldDefault::Note(_) => {
                let value = match field.kind {
                    FieldKind::Secret => continue,
                    FieldKind::Integer => serde_json::json!(1),
                    FieldKind::Models if path == "default_model" => {
                        serde_json::json!("${small_model}")
                    }
                    FieldKind::Models => serde_json::json!("${default_model}"),
                    FieldKind::Choices(options) => serde_json::json!([options[0]]),
                    _ => serde_json::json!(sample(&path)),
                };
                let result = loaded(field.key, Some(&value));
                assert!(result.is_ok(), "{path}: {result:?}");
            }
        }
        if let FieldKind::Choice(options) = field.kind {
            for option in options {
                let value = serde_json::json!(option);
                assert!(loaded(field.key, Some(&value)).is_ok(), "{path} = {option}");
            }
        }
    }
}

#[test]
fn every_group_that_can_always_load_is_offered() {
    let field = SECTIONS
        .iter()
        .flat_map(|section| section.fields)
        .find(|field| field.path() == "deferred_tools.always_load")
        .unwrap();
    let FieldKind::Choices(options) = field.kind else {
        panic!("always_load is a list of groups");
    };
    assert_eq!(options, DEFERRED_BUILTIN_GROUPS);
}

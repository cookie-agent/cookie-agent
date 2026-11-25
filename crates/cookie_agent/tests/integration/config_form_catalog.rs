//! The web form's catalog of settings against what the configuration loads:
//! a key it names exists, and a default it shows is the one the daemon uses.

use cookie_agent_client_state::config_form::{FieldDefault, FieldKind, SECTIONS, SPEECH_BACKENDS};
use cookie_agent_config::{ConfigPath, ConfigTree, DEFERRED_BUILTIN_GROUPS, load_from_tree};

/// A transcription backend: the rest of `[speech]` needs one to load.
fn backends() -> serde_json::Value {
    serde_json::json!([{ "endpoint": "http://127.0.0.1:8000/v1", "model": "m" }])
}

/// The configuration a `config.toml` setting `key` to `value` loads to, as
/// text; `Err` when it does not load. A key of `[speech]` comes with a
/// backend.
fn loaded(key: &[&str], value: Option<&serde_json::Value>) -> Result<String, String> {
    let mut document = toml::Table::new();
    if key.first() == Some(&"speech") && key != SPEECH_BACKENDS {
        put(&mut document, SPEECH_BACKENDS, &backends())?;
    }
    if let Some(value) = value {
        put(&mut document, key, value)?;
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

/// Sets `key` of `document` to `value`, making the tables on the way.
fn put(document: &mut toml::Table, key: &[&str], value: &serde_json::Value) -> Result<(), String> {
    let (last, parents) = key.split_last().expect("a key");
    let mut table = document;
    for name in parents {
        table = table
            .entry((*name).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("a table on the way");
    }
    let value = toml::Value::try_from(value).map_err(|error| error.to_string())?;
    table.insert((*last).to_owned(), value);
    Ok(())
}

/// A value the configuration takes for a text or list field with no default.
fn sample(path: &str) -> &'static str {
    match path {
        "server.url" => "ws://127.0.0.1:1/ws",
        "server.page_url" | "public.url" => "https://example.com",
        "notifications.contact" => "mailto:me@example.com",
        "agents.default" => "primary",
        "public.listen" => "127.0.0.1:7420",
        "shell_environment.shell" => "/bin/sh",
        "speech.language" => "en",
        "speech.prompt" => "cookie",
        other => panic!("no sample for {other}"),
    }
}

#[test]
fn an_advertised_default_is_the_daemons_and_every_key_loads() {
    for field in SECTIONS.iter().flat_map(|section| section.fields) {
        let path = field.path();
        let unset = loaded(field.key, None).expect("a file without the key loads");
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
                    FieldKind::Backends => backends(),
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

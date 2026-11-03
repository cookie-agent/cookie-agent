//! A plugin's settings: the form it declares, and the values the user set,
//! kept in `<data_root>/plugin-settings/<name>.json`, readable by the user
//! only (`docs/site/specs/plugin-web-ui.md` section 5).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use cookie_agent_protocol::{
    ElicitationContent, ElicitationField, ElicitationForm, ElicitationInput, ElicitationValue,
};

/// The file plugin `name`'s settings are kept in, under `directory`: the
/// name percent-encoded as one path segment.
pub(crate) fn settings_path(directory: &Path, name: &str) -> PathBuf {
    let page = cookie_agent_protocol::plugin_page_path(name);
    let segment = page
        .strip_prefix("plugins/")
        .and_then(|rest| rest.strip_suffix('/'))
        .unwrap_or(name);
    directory.join(format!("{segment}.json"))
}

/// The values stored at `path`; none when the file is missing or cannot be
/// read, which the daemon reports on standard error.
pub(crate) fn load(path: &Path) -> ElicitationContent {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            eprintln!(
                "cookie-agent: ignoring unreadable plugin settings {}: {error}",
                path.display()
            );
            ElicitationContent::new()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ElicitationContent::new(),
        Err(error) => {
            eprintln!(
                "cookie-agent: could not read plugin settings {}: {error}",
                path.display()
            );
            ElicitationContent::new()
        }
    }
}

/// Writes `values` to `path` through a temporary file renamed over it,
/// readable and writable by the user only, in a directory only the user
/// can enter.
pub(crate) fn save(path: &Path, values: &ElicitationContent) -> std::io::Result<()> {
    if let Some(directory) = path.parent() {
        create_private_dir(directory)?;
    }
    let bytes = serde_json::to_vec_pretty(values).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    write_private(&temporary, &bytes)?;
    std::fs::rename(&temporary, path)
}

#[cfg(unix)]
fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
}

#[cfg(not(unix))]
fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Whether `field` is a secret text field.
pub(crate) fn is_secret(field: &ElicitationField) -> bool {
    matches!(
        field.input.known(),
        Some(ElicitationInput::Text { secret: true, .. })
    )
}

/// The values a plugin receives: those of the fields `form` declares, or
/// every stored one before any form is known.
pub(crate) fn for_plugin(
    form: Option<&ElicitationForm>,
    values: &ElicitationContent,
) -> ElicitationContent {
    match form {
        None => values.clone(),
        Some(form) => form
            .fields
            .iter()
            .filter_map(|field| {
                values
                    .get(&field.name)
                    .map(|value| (field.name.clone(), value.clone()))
            })
            .collect(),
    }
}

/// Whether a required field of `form` has no value.
pub(crate) fn missing_required(form: &ElicitationForm, values: &ElicitationContent) -> bool {
    form.fields
        .iter()
        .any(|field| field.required && !values.contains_key(&field.name))
}

/// What a client may see of `values`: the plain ones, and the names of the
/// secret fields that have a value.
pub(crate) fn for_client(
    form: &ElicitationForm,
    values: &ElicitationContent,
) -> (ElicitationContent, Vec<String>) {
    let mut plain = ElicitationContent::new();
    let mut secrets_set = Vec::new();
    for field in &form.fields {
        let Some(value) = values.get(&field.name) else {
            continue;
        };
        if is_secret(field) {
            secrets_set.push(field.name.clone());
        } else {
            plain.insert(field.name.clone(), value.clone());
        }
    }
    (plain, secrets_set)
}

/// `stored` with `changes` applied (`None` removes a value), checked
/// against `form`: every change must name a field of the form, and every
/// field must fit its value afterwards. Values of fields the form no longer
/// declares are kept.
pub(crate) fn apply(
    form: &ElicitationForm,
    stored: &ElicitationContent,
    changes: BTreeMap<String, Option<ElicitationValue>>,
) -> Result<ElicitationContent, String> {
    let mut values = stored.clone();
    for (name, value) in changes {
        if !form.fields.iter().any(|field| field.name == name) {
            return Err(format!("the plugin's settings have no field `{name}`"));
        }
        match value {
            Some(value) => {
                values.insert(name, value);
            }
            None => {
                values.remove(&name);
            }
        }
    }
    for field in &form.fields {
        field.check(values.get(&field.name))?;
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::Open;

    use super::*;

    fn text(name: &str, required: bool, secret: bool) -> ElicitationField {
        ElicitationField {
            name: name.into(),
            title: None,
            description: None,
            required,
            input: Open::Known(ElicitationInput::Text {
                min_length: None,
                max_length: Some(8),
                pattern: None,
                format: None,
                default: None,
                secret,
            }),
        }
    }

    fn form() -> ElicitationForm {
        ElicitationForm {
            title: None,
            description: None,
            fields: vec![
                text("client_id", true, false),
                text("client_secret", true, true),
                text("note", false, false),
            ],
        }
    }

    fn value(text: &str) -> ElicitationValue {
        ElicitationValue::Text(text.into())
    }

    #[test]
    fn changes_merge_into_the_stored_values_and_must_fit_the_form() {
        let stored = ElicitationContent::from([
            ("client_id".to_owned(), value("abc")),
            ("client_secret".to_owned(), value("s3cret")),
            ("retired".to_owned(), value("kept")),
        ]);
        // A secret left out keeps its value; a note is set and cleared.
        let values = apply(
            &form(),
            &stored,
            BTreeMap::from([
                ("client_id".to_owned(), Some(value("xyz"))),
                ("note".to_owned(), Some(value("hi"))),
            ]),
        )
        .unwrap();
        assert_eq!(values["client_id"], value("xyz"));
        assert_eq!(values["client_secret"], value("s3cret"));
        assert_eq!(values["note"], value("hi"));
        assert_eq!(values["retired"], value("kept"));
        let cleared = apply(
            &form(),
            &values,
            BTreeMap::from([("note".to_owned(), None)]),
        )
        .unwrap();
        assert!(!cleared.contains_key("note"));

        // A required field cannot be cleared, a value must fit, and a
        // change must name a field of the form.
        for changes in [
            BTreeMap::from([("client_secret".to_owned(), None)]),
            BTreeMap::from([("client_id".to_owned(), Some(value("much too long")))]),
            BTreeMap::from([("client_id".to_owned(), Some(ElicitationValue::Integer(1)))]),
            BTreeMap::from([("retired".to_owned(), Some(value("x")))]),
        ] {
            assert!(
                apply(&form(), &values, changes.clone()).is_err(),
                "{changes:?}"
            );
        }
    }

    #[test]
    fn clients_see_plain_values_and_which_secrets_are_set() {
        let values = ElicitationContent::from([
            ("client_id".to_owned(), value("abc")),
            ("client_secret".to_owned(), value("s3cret")),
            ("retired".to_owned(), value("kept")),
        ]);
        let (plain, secrets) = for_client(&form(), &values);
        assert_eq!(
            plain,
            ElicitationContent::from([("client_id".to_owned(), value("abc"))])
        );
        assert_eq!(secrets, ["client_secret"]);
        assert!(!missing_required(&form(), &values));
        assert!(missing_required(&form(), &plain));
    }

    #[test]
    fn a_plugin_receives_its_declared_fields_or_everything_before_a_form() {
        let values = ElicitationContent::from([
            ("client_id".to_owned(), value("abc")),
            ("retired".to_owned(), value("kept")),
        ]);
        assert_eq!(for_plugin(None, &values), values);
        assert_eq!(
            for_plugin(Some(&form()), &values),
            ElicitationContent::from([("client_id".to_owned(), value("abc"))])
        );
    }

    #[test]
    fn settings_are_kept_privately_under_an_encoded_name() {
        let directory = tempfile::tempdir().unwrap();
        let path = settings_path(&directory.path().join("plugin-settings"), "my/plugin");
        assert_eq!(path.file_name().unwrap(), "my%2Fplugin.json");
        assert!(load(&path).is_empty());
        let values = ElicitationContent::from([("client_id".to_owned(), value("abc"))]);
        save(&path, &values).unwrap();
        assert_eq!(load(&path), values);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }
}

//! `config.toml` for the Configuration form: its text as the form's sorted
//! settings, and a key the form has no control for as it is written.

use cookie_agent_client_state::config_form::ConfigForm;
use serde_json::Value;

/// The form for the document `text`.
///
/// # Errors
///
/// Where the TOML is malformed. Never the text around it: the file holds
/// credentials.
pub fn form_of(text: &str) -> Result<ConfigForm, String> {
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| match error.span() {
            Some(span) => {
                let start = span.start.min(text.len());
                let line = text.as_bytes()[..start]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count()
                    + 1;
                format!("config.toml has malformed TOML at line {line}")
            }
            None => "config.toml has malformed TOML".to_owned(),
        })?;
    let document = serde_json::to_value(table).map_err(|error| error.to_string())?;
    match document {
        Value::Object(document) => Ok(ConfigForm::build(&document)),
        _ => Err("config.toml is not a table".to_owned()),
    }
}

/// `value` at `key` as the lines `config.toml` would hold.
#[must_use]
pub fn snippet(key: &[String], value: &Value) -> String {
    let mut nested = match toml::Value::try_from(value) {
        Ok(value) => value,
        Err(_) => return value.to_string(),
    };
    for name in key.iter().rev() {
        nested = toml::Value::Table(toml::Table::from_iter([(name.clone(), nested)]));
    }
    toml::to_string(&nested).map_or_else(|_| value.to_string(), |text| text.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_document_sorts_into_the_form() {
        let form = form_of("[cron]\nhistory = 5 # kept\n[experimental]\nplanner = true\n").unwrap();
        assert_eq!(form.field("cron.history").unwrap().value, Some(json!(5)));
        assert_eq!(form.other[0].key, ["experimental"]);
    }

    #[test]
    fn malformed_toml_names_its_line_and_never_the_text_around_it() {
        let error = form_of("[server]\ntoken = \"sekret\"\nbroken = \n").unwrap_err();
        assert!(error.contains("line 3"), "{error}");
        assert!(!error.contains("sekret"), "{error}");
    }

    #[test]
    fn a_snippet_reads_back_as_the_same_value_at_the_same_key() {
        let key = ["a".to_owned(), "b".to_owned()];
        let value = json!({ "percent": 70, "names": ["x", "y"] });
        let text = snippet(&key, &value);
        let table = text.parse::<toml::Table>().unwrap();
        assert_eq!(
            serde_json::to_value(&table["a"]["b"]).unwrap(),
            value,
            "{text}"
        );
    }
}

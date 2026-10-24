//! Questions an agent asks the user (client protocol 1.25.0): ACP
//! elicitation (`docs/site/specs/acp-agents.md`, section 15).
//!
//! An ACP agent's `elicitation/create` becomes an `elicitation_requested`
//! event of the run: a form (fields the user fills in) or a URL (a page the
//! user opens, then confirms). The run waits until the user answers with
//! `elicitation.respond` or the daemon answers for them, which
//! `elicitation_resolved` records: accept (with the form's values), decline,
//! or cancel. A question always goes to the user; no permission rule or mode
//! answers it, except that a session no one attends declines it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AgentId, ElicitationId, Open, SessionId, ToolCallId};

/// The longest question message, form or field title or description, or
/// choice label the daemon records, in bytes.
pub const MAX_ELICITATION_TEXT_BYTES: usize = 16 * 1024;
/// The longest URL of a URL question, in bytes.
pub const MAX_ELICITATION_URL_BYTES: usize = 8 * 1024;
/// The most fields of a form, and the most choices of a field.
pub const MAX_ELICITATION_ITEMS: usize = 256;

/// The payload of `elicitation_requested`: a question for the user. Exactly
/// one of `form` and `url` is set by this daemon; a question with neither
/// is of a kind a newer daemon asks, which a client can only decline or
/// cancel.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ElicitationRequest {
    pub elicitation_id: ElicitationId,
    /// The agent that asks.
    pub agent: AgentId,
    /// The tool call the question belongs to, when the agent named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// What the agent asks, and why.
    pub message: String,
    /// A form to fill in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<ElicitationForm>,
    /// A page to open, such as a sign-in. Accepting means the user agreed
    /// to open it, not that what happens there is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A form: fields in the order the agent gave them.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ElicitationForm {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub fields: Vec<ElicitationField>,
}

/// One field of a form.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ElicitationField {
    /// The member of the answer that carries the field's value.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// An accepted answer must give it a value.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// What the field takes. A kind this build does not know cannot be
    /// filled in.
    pub input: Open<ElicitationInput>,
}

impl ElicitationField {
    /// Checks `value` as the field's value in an accepted answer (`None`
    /// when the answer leaves the field out): a required field needs one,
    /// and it must have the field's type and be within its bounds and
    /// choices. A field of a kind this build does not know takes none. The
    /// error names the field by its label.
    pub fn check(&self, value: Option<&ElicitationValue>) -> Result<(), String> {
        let label = self.label();
        let Some(value) = value else {
            return if self.required {
                Err(format!("{label} is required"))
            } else {
                Ok(())
            };
        };
        let Some(input) = self.input.known() else {
            return Err(format!("{label} cannot be answered here"));
        };
        check_value(label, input, value)
    }

    /// The field's label: its title, else its name.
    #[must_use]
    pub fn label(&self) -> &str {
        self.title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.name)
    }
}

/// What a field takes, and its default.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElicitationInput {
    /// A line of text.
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_length: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_length: Option<u32>,
        /// A regular expression the agent wants the text to match; shown,
        /// not checked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        /// `email`, `uri`, `date`, or `date-time`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// One of the choices.
    Choice {
        choices: Vec<ElicitationChoice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// Any number of the choices, in their order.
    MultiChoice {
        choices: Vec<ElicitationChoice>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_items: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_items: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        default: Vec<String>,
    },
    /// A number.
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
    },
    /// A whole number.
    Integer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        maximum: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
    },
    /// Yes or no.
    Boolean {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
}

crate::open_enum!(tagged ElicitationInput, tag = "type", {
    Text => "text",
    Choice => "choice",
    MultiChoice => "multi_choice",
    Number => "number",
    Integer => "integer",
    Boolean => "boolean",
});

/// One choice of a choice field: the value the answer carries and the
/// label shown.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ElicitationChoice {
    pub value: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One field's value in an answer.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ElicitationValue {
    Boolean(bool),
    Integer(i64),
    Number(f64),
    Text(String),
    List(Vec<String>),
}

/// The values of an accepted form, by field name.
pub type ElicitationContent = BTreeMap<String, ElicitationValue>;

crate::open_enum! {
    /// How a question was answered. An unknown action reads as `Unknown`,
    /// which a client shows as answered.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ElicitationAction {
        /// The user submitted the form, or agreed to open the URL.
        Accept,
        /// The user said no.
        Decline,
        /// The question was dismissed without an answer.
        Cancel,
    }
}

crate::open_enum! {
    /// Who answered a question.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ElicitationAnsweredBy {
        /// The user, with `elicitation.respond`.
        User,
        /// The daemon, for a session no one attends: one whose permission
        /// mode is `auto_approve_n` (a cron run's default), or a headless
        /// `cookie run`. Always `decline`.
        Unattended,
        /// The daemon, as the run was cancelled or ended. Always `cancel`.
        RunEnded,
        /// The agent reported that the URL's interaction finished
        /// (`elicitation/complete`) before the user answered. Always
        /// `accept`.
        Completed,
        /// The daemon, for a form it cannot show: a required field of a
        /// kind it does not know. Always `decline`.
        Unsupported,
    }
}

/// The payload of `elicitation_resolved`: how a question was answered.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ElicitationResolution {
    pub elicitation_id: ElicitationId,
    pub action: ElicitationAction,
    /// The form's values, for an accepted form.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub content: ElicitationContent,
    pub answered_by: ElicitationAnsweredBy,
    /// Why the daemon answered, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `elicitation.respond`: the user's answer to a question still waiting.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationRespondParams {
    pub session_id: SessionId,
    pub elicitation_id: ElicitationId,
    pub action: ElicitationAction,
    /// The form's values, for `accept` of a form; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ElicitationContent>,
}

/// The answer was handed to the agent; `elicitation_resolved` records it.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ElicitationRespondResult {}

/// The client method that answers a question.
pub const ELICITATION_RESPOND_METHOD: &str = "elicitation.respond";

impl ElicitationRequest {
    /// Whether the request is within the daemon's limits: a message, at
    /// most one of a form and a URL, bounded texts, and fields with
    /// distinct, non-empty names.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let text = |value: &str| value.len() <= MAX_ELICITATION_TEXT_BYTES;
        let optional = |value: &Option<String>| value.as_deref().is_none_or(text);
        if !text(&self.message) || (self.form.is_some() && self.url.is_some()) {
            return false;
        }
        if let Some(url) = &self.url
            && (url.is_empty()
                || url.len() > MAX_ELICITATION_URL_BYTES
                || url.chars().any(char::is_control))
        {
            return false;
        }
        let Some(form) = &self.form else {
            return true;
        };
        if !optional(&form.title)
            || !optional(&form.description)
            || form.fields.len() > MAX_ELICITATION_ITEMS
        {
            return false;
        }
        let mut names = std::collections::BTreeSet::new();
        form.fields.iter().all(|field| {
            !field.name.is_empty()
                && text(&field.name)
                && names.insert(field.name.as_str())
                && optional(&field.title)
                && optional(&field.description)
                && match field.input.known() {
                    Some(
                        ElicitationInput::Choice { choices, .. }
                        | ElicitationInput::MultiChoice { choices, .. },
                    ) => {
                        choices.len() <= MAX_ELICITATION_ITEMS
                            && choices.iter().all(|choice| {
                                text(&choice.value)
                                    && text(&choice.title)
                                    && optional(&choice.description)
                            })
                    }
                    _ => true,
                }
        })
    }
}

impl ElicitationForm {
    /// Checks `content` as the accepted answer to the form: every required
    /// field has a value, each value has its field's type and is within its
    /// bounds and choices, and no value names a field the form lacks. A
    /// field of a kind this build does not know takes no value. The error
    /// names the field and says what is wrong.
    pub fn check(&self, content: &ElicitationContent) -> Result<(), String> {
        for name in content.keys() {
            if !self.fields.iter().any(|field| &field.name == name) {
                return Err(format!("the form has no field `{name}`"));
            }
        }
        for field in &self.fields {
            field.check(content.get(&field.name))?;
        }
        Ok(())
    }

    /// Whether the form has a required field this build cannot fill in.
    #[must_use]
    pub fn has_unknown_required_field(&self) -> bool {
        self.fields
            .iter()
            .any(|field| field.required && field.input.is_unknown())
    }
}

fn check_value(
    label: &str,
    input: &ElicitationInput,
    value: &ElicitationValue,
) -> Result<(), String> {
    match (input, value) {
        (
            ElicitationInput::Text {
                min_length,
                max_length,
                format,
                ..
            },
            ElicitationValue::Text(text),
        ) => {
            let length = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
            if min_length.is_some_and(|min| length < min) {
                return Err(format!(
                    "{label} needs at least {} characters",
                    min_length.unwrap_or_default()
                ));
            }
            if max_length.is_some_and(|max| length > max) {
                return Err(format!(
                    "{label} takes at most {} characters",
                    max_length.unwrap_or_default()
                ));
            }
            match format.as_deref() {
                Some(format) if !text_matches_format(text, format) => {
                    Err(format!("{label} must be {}", format_description(format)))
                }
                _ => Ok(()),
            }
        }
        (ElicitationInput::Choice { choices, .. }, ElicitationValue::Text(chosen)) => {
            if choices.iter().any(|choice| &choice.value == chosen) {
                Ok(())
            } else {
                Err(format!("{label} has no choice `{chosen}`"))
            }
        }
        (
            ElicitationInput::MultiChoice {
                choices,
                min_items,
                max_items,
                ..
            },
            ElicitationValue::List(chosen),
        ) => {
            if let Some(unknown) = chosen
                .iter()
                .find(|chosen| !choices.iter().any(|choice| &&choice.value == chosen))
            {
                return Err(format!("{label} has no choice `{unknown}`"));
            }
            let count = chosen.len() as u64;
            if min_items.is_some_and(|min| count < min) {
                return Err(format!(
                    "{label} needs at least {} choices",
                    min_items.unwrap_or_default()
                ));
            }
            if max_items.is_some_and(|max| count > max) {
                return Err(format!(
                    "{label} takes at most {} choices",
                    max_items.unwrap_or_default()
                ));
            }
            Ok(())
        }
        (
            ElicitationInput::Number {
                minimum, maximum, ..
            },
            number,
        ) => {
            let number = match number {
                ElicitationValue::Number(number) => *number,
                #[allow(clippy::cast_precision_loss)]
                ElicitationValue::Integer(number) => *number as f64,
                _ => return Err(format!("{label} must be a number")),
            };
            if !number.is_finite() {
                return Err(format!("{label} must be a number"));
            }
            if minimum.is_some_and(|min| number < min) {
                return Err(format!(
                    "{label} must be at least {}",
                    minimum.unwrap_or_default()
                ));
            }
            if maximum.is_some_and(|max| number > max) {
                return Err(format!(
                    "{label} must be at most {}",
                    maximum.unwrap_or_default()
                ));
            }
            Ok(())
        }
        (
            ElicitationInput::Integer {
                minimum, maximum, ..
            },
            ElicitationValue::Integer(number),
        ) => {
            if minimum.is_some_and(|min| *number < min) {
                return Err(format!(
                    "{label} must be at least {}",
                    minimum.unwrap_or_default()
                ));
            }
            if maximum.is_some_and(|max| *number > max) {
                return Err(format!(
                    "{label} must be at most {}",
                    maximum.unwrap_or_default()
                ));
            }
            Ok(())
        }
        (ElicitationInput::Boolean { .. }, ElicitationValue::Boolean(_)) => Ok(()),
        (ElicitationInput::Text { .. }, _) => Err(format!("{label} must be text")),
        (ElicitationInput::Choice { .. }, _) => Err(format!("{label} must be one of its choices")),
        (ElicitationInput::MultiChoice { .. }, _) => {
            Err(format!("{label} must be a list of its choices"))
        }
        (ElicitationInput::Integer { .. }, _) => Err(format!("{label} must be a whole number")),
        (ElicitationInput::Boolean { .. }, _) => Err(format!("{label} must be yes or no")),
    }
}

/// Whether `text` has string format `format`, checked loosely: an email
/// address has a local part and a domain, a URI a scheme, a date is
/// `YYYY-MM-DD`, and a date-time an RFC 3339 timestamp. An unknown format
/// matches anything.
#[must_use]
pub fn text_matches_format(text: &str, format: &str) -> bool {
    match format {
        "email" => text.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !text.chars().any(char::is_whitespace)
        }),
        "uri" => text.split_once(':').is_some_and(|(scheme, rest)| {
            !rest.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                && !text.chars().any(char::is_whitespace)
        }),
        "date" => text.len() == 10 && text.parse::<jiff::civil::Date>().is_ok(),
        "date-time" => text.parse::<jiff::Timestamp>().is_ok(),
        _ => true,
    }
}

/// What a string format asks for, in words.
#[must_use]
pub fn format_description(format: &str) -> &'static str {
    match format {
        "email" => "an email address",
        "uri" => "a URI",
        "date" => "a date (YYYY-MM-DD)",
        "date-time" => "a date and time (RFC 3339)",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn form() -> ElicitationForm {
        serde_json::from_value(json!({
            "fields": [
                {"name": "name", "required": true, "input": {"type": "text", "min_length": 2, "max_length": 5}},
                {"name": "mail", "input": {"type": "text", "format": "email"}},
                {"name": "plan", "required": true, "input": {"type": "choice", "choices": [
                    {"value": "a", "title": "A"}, {"value": "b", "title": "B"}]}},
                {"name": "tags", "input": {"type": "multi_choice", "max_items": 1, "choices": [
                    {"value": "x", "title": "X"}, {"value": "y", "title": "Y"}]}},
                {"name": "ratio", "input": {"type": "number", "minimum": 0.5, "maximum": 1.5}},
                {"name": "count", "input": {"type": "integer", "minimum": 1}},
                {"name": "ok", "input": {"type": "boolean"}},
                {"name": "later", "input": {"type": "colour"}}
            ]
        }))
        .expect("a form")
    }

    fn content(value: serde_json::Value) -> ElicitationContent {
        serde_json::from_value(value).expect("content")
    }

    #[test]
    fn a_complete_answer_passes() {
        assert_eq!(
            form().check(&content(json!({
                "name": "Ann", "mail": "ann@example.com", "plan": "b", "tags": ["y"],
                "ratio": 1, "count": 3, "ok": true
            }))),
            Ok(())
        );
    }

    #[test]
    fn each_rule_names_the_field() {
        let base = json!({"name": "Ann", "plan": "a"});
        for (change, error) in [
            (json!({"name": null}), "name is required"),
            (json!({"name": "A"}), "name needs at least 2 characters"),
            (
                json!({"name": "Annabel"}),
                "name takes at most 5 characters",
            ),
            (json!({"mail": "ann"}), "mail must be an email address"),
            (json!({"plan": "c"}), "plan has no choice `c`"),
            (json!({"tags": ["x", "y"]}), "tags takes at most 1 choices"),
            (json!({"tags": ["z"]}), "tags has no choice `z`"),
            (json!({"ratio": 2.0}), "ratio must be at most 1.5"),
            (json!({"count": 0}), "count must be at least 1"),
            (json!({"count": 1.5}), "count must be a whole number"),
            (json!({"ok": "yes"}), "ok must be yes or no"),
            (json!({"later": "red"}), "later cannot be answered here"),
            (json!({"extra": 1}), "the form has no field `extra`"),
        ] {
            let mut answer = base.clone();
            for (key, value) in change.as_object().unwrap() {
                if value.is_null() {
                    answer.as_object_mut().unwrap().remove(key);
                } else {
                    answer[key] = value.clone();
                }
            }
            assert_eq!(form().check(&content(answer)), Err(error.to_owned()));
        }
    }

    #[test]
    fn an_unknown_kind_reads_open_and_only_a_required_one_blocks() {
        let mut form = form();
        assert!(form.fields[7].input.is_unknown());
        assert!(!form.has_unknown_required_field());
        form.fields[7].required = true;
        assert!(form.has_unknown_required_field());
        // It round-trips unchanged.
        let written = serde_json::to_value(&form.fields[7]).unwrap();
        assert_eq!(written["input"], json!({"type": "colour"}));
    }

    #[test]
    fn formats_are_checked_loosely() {
        for (text, format, matches) in [
            ("a@b.co", "email", true),
            ("a@b", "email", false),
            ("a b@c.d", "email", false),
            ("https://example.com/x", "uri", true),
            ("example.com", "uri", false),
            ("2026-10-24", "date", true),
            ("2026-13-01", "date", false),
            ("2026-10-24T10:00:00Z", "date-time", true),
            ("2026-10-24", "date-time", false),
            ("anything", "colour", true),
        ] {
            assert_eq!(
                text_matches_format(text, format),
                matches,
                "{text} as {format}"
            );
        }
    }

    #[test]
    fn a_request_has_at_most_one_kind_and_distinct_field_names() {
        let request = |form: Option<serde_json::Value>, url: Option<&str>| {
            let mut value = json!({
                "elicitation_id": ElicitationId::new_v7(),
                "agent": "claude",
                "message": "Which?",
            });
            if let Some(form) = form {
                value["form"] = form;
            }
            if let Some(url) = url {
                value["url"] = json!(url);
            }
            serde_json::from_value::<ElicitationRequest>(value).expect("a request")
        };
        let field = json!({"name": "a", "input": {"type": "boolean"}});
        assert!(request(Some(json!({"fields": [field]})), None).is_valid());
        assert!(request(None, Some("https://example.com")).is_valid());
        assert!(request(None, None).is_valid());
        assert!(!request(Some(json!({"fields": []})), Some("https://example.com")).is_valid());
        assert!(!request(Some(json!({"fields": [field, field]})), None).is_valid());
        assert!(!request(None, Some("")).is_valid());
    }

    #[test]
    fn values_read_by_their_json_type() {
        assert_eq!(
            content(json!({"a": true, "b": 3, "c": 2.5, "d": "x", "e": ["y"]})),
            BTreeMap::from([
                ("a".to_owned(), ElicitationValue::Boolean(true)),
                ("b".to_owned(), ElicitationValue::Integer(3)),
                ("c".to_owned(), ElicitationValue::Number(2.5)),
                ("d".to_owned(), ElicitationValue::Text("x".into())),
                ("e".to_owned(), ElicitationValue::List(vec!["y".into()])),
            ])
        );
    }
}

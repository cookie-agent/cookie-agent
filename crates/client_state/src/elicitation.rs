//! Questions an agent asks the user (client protocol 1.25.0; ACP
//! elicitation, `docs/site/specs/acp-agents.md` section 15): what the
//! clients keep of a question still waiting, the user's draft answer to a
//! form, and the words both clients show.
//!
//! A session's waiting questions come from its `elicitation_requested`
//! events; `elicitation_resolved` or the run's end removes one.

use cookie_agent_protocol::{
    ElicitationAction, ElicitationAnsweredBy, ElicitationContent, ElicitationField,
    ElicitationForm, ElicitationInput, ElicitationRequest, ElicitationResolution, ElicitationValue,
    RunId, SessionId,
};

/// A question waiting for the user's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct QuestionState {
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub request: ElicitationRequest,
}

impl QuestionState {
    /// What kind of question it is.
    #[must_use]
    pub fn kind(&self) -> QuestionKind<'_> {
        match (&self.request.form, &self.request.url) {
            (Some(form), _) => QuestionKind::Form(form),
            (None, Some(url)) => QuestionKind::Url(url),
            (None, None) => QuestionKind::Unsupported,
        }
    }

    /// The heading both clients show: who asks.
    #[must_use]
    pub fn heading(&self) -> String {
        format!("{} asks", self.request.agent)
    }
}

/// A question's kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuestionKind<'a> {
    Form(&'a ElicitationForm),
    Url(&'a str),
    /// A kind a newer daemon asks: it can only be declined or cancelled.
    Unsupported,
}

/// One field's draft value.
#[derive(Clone, Debug, PartialEq)]
pub enum DraftValue {
    /// Text, a number, or a whole number, as typed.
    Text(String),
    /// The chosen choice's index.
    Choice(Option<usize>),
    /// Whether each choice is chosen.
    MultiChoice(Vec<bool>),
    Boolean(bool),
    /// A field of a kind this build cannot show.
    Unsupported,
}

/// The user's draft answer to a form: one value per field, in the form's
/// order, starting from the fields' defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct FormDraft {
    pub values: Vec<DraftValue>,
}

/// A field whose draft value does not fit it, by its index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldProblem {
    pub index: usize,
    pub message: String,
}

impl FormDraft {
    /// A draft of `form` with its fields' defaults.
    #[must_use]
    pub fn new(form: &ElicitationForm) -> Self {
        Self {
            values: form.fields.iter().map(default_value).collect(),
        }
    }

    /// The value field `index` of `form` would answer with: `None` for a
    /// field left empty, an error for a value that does not fit.
    pub fn value(
        &self,
        form: &ElicitationForm,
        index: usize,
    ) -> Result<Option<ElicitationValue>, String> {
        let (Some(field), Some(draft)) = (form.fields.get(index), self.values.get(index)) else {
            return Ok(None);
        };
        let value = match (field.input.known(), draft) {
            (_, DraftValue::Unsupported) | (None, _) => None,
            (Some(ElicitationInput::Text { .. }), DraftValue::Text(text)) => {
                (!text.is_empty()).then(|| ElicitationValue::Text(text.clone()))
            }
            (Some(ElicitationInput::Number { .. }), DraftValue::Text(text)) => match text.trim() {
                "" => None,
                text => Some(ElicitationValue::Number(
                    text.parse::<f64>()
                        .ok()
                        .filter(|number| number.is_finite())
                        .ok_or_else(|| format!("{} must be a number", field.label()))?,
                )),
            },
            (Some(ElicitationInput::Integer { .. }), DraftValue::Text(text)) => match text.trim() {
                "" => None,
                text => Some(ElicitationValue::Integer(
                    text.parse::<i64>()
                        .map_err(|_| format!("{} must be a whole number", field.label()))?,
                )),
            },
            (Some(ElicitationInput::Choice { choices, .. }), DraftValue::Choice(chosen)) => chosen
                .and_then(|index| choices.get(index))
                .map(|choice| ElicitationValue::Text(choice.value.clone())),
            (
                Some(ElicitationInput::MultiChoice { choices, .. }),
                DraftValue::MultiChoice(chosen),
            ) => {
                let values = choices
                    .iter()
                    .zip(chosen)
                    .filter(|(_, chosen)| **chosen)
                    .map(|(choice, _)| choice.value.clone())
                    .collect::<Vec<_>>();
                (!values.is_empty()).then_some(ElicitationValue::List(values))
            }
            (Some(ElicitationInput::Boolean { .. }), DraftValue::Boolean(value)) => {
                Some(ElicitationValue::Boolean(*value))
            }
            _ => None,
        };
        field.check(value.as_ref())?;
        Ok(value)
    }

    /// The answer the draft makes, or the first field that does not fit.
    pub fn content(&self, form: &ElicitationForm) -> Result<ElicitationContent, FieldProblem> {
        let mut content = ElicitationContent::new();
        for (index, field) in form.fields.iter().enumerate() {
            match self.value(form, index) {
                Ok(Some(value)) => {
                    content.insert(field.name.clone(), value);
                }
                Ok(None) => {}
                Err(message) => return Err(FieldProblem { index, message }),
            }
        }
        Ok(content)
    }
}

fn default_value(field: &ElicitationField) -> DraftValue {
    match field.input.known() {
        None => DraftValue::Unsupported,
        Some(ElicitationInput::Text { default, .. }) => {
            DraftValue::Text(default.clone().unwrap_or_default())
        }
        Some(ElicitationInput::Number { default, .. }) => {
            DraftValue::Text(default.map(|number| number.to_string()).unwrap_or_default())
        }
        Some(ElicitationInput::Integer { default, .. }) => {
            DraftValue::Text(default.map(|number| number.to_string()).unwrap_or_default())
        }
        Some(ElicitationInput::Choice { choices, default }) => DraftValue::Choice(
            default
                .as_ref()
                .and_then(|default| choices.iter().position(|choice| &choice.value == default)),
        ),
        Some(ElicitationInput::MultiChoice {
            choices, default, ..
        }) => DraftValue::MultiChoice(
            choices
                .iter()
                .map(|choice| default.contains(&choice.value))
                .collect(),
        ),
        Some(ElicitationInput::Boolean { default }) => {
            DraftValue::Boolean(default.unwrap_or(false))
        }
    }
}

/// What a field asks for beyond its description, in a few words: its
/// bounds, format, and pattern, and whether it is required.
#[must_use]
pub fn field_hint(field: &ElicitationField) -> String {
    let mut parts = Vec::new();
    if field.required {
        parts.push("required".to_owned());
    }
    match field.input.known() {
        None => parts.push("this client cannot fill it in".to_owned()),
        Some(ElicitationInput::Text {
            min_length,
            max_length,
            pattern,
            format,
            ..
        }) => {
            if let Some(format) = format {
                parts.push(cookie_agent_protocol::format_description(format).to_owned());
            }
            match (min_length, max_length) {
                (Some(min), Some(max)) => parts.push(format!("{min} to {max} characters")),
                (Some(min), None) => parts.push(format!("at least {min} characters")),
                (None, Some(max)) => parts.push(format!("at most {max} characters")),
                (None, None) => {}
            }
            if let Some(pattern) = pattern {
                parts.push(format!("matching {pattern}"));
            }
        }
        Some(ElicitationInput::Number {
            minimum, maximum, ..
        }) => parts.extend(bounds("a number", *minimum, *maximum)),
        Some(ElicitationInput::Integer {
            minimum, maximum, ..
        }) => parts.extend(bounds("a whole number", *minimum, *maximum)),
        Some(ElicitationInput::MultiChoice {
            min_items,
            max_items,
            ..
        }) => match (min_items, max_items) {
            (Some(min), Some(max)) => parts.push(format!("choose {min} to {max}")),
            (Some(min), None) => parts.push(format!("choose at least {min}")),
            (None, Some(max)) => parts.push(format!("choose at most {max}")),
            (None, None) => parts.push("choose any".to_owned()),
        },
        Some(ElicitationInput::Choice { .. } | ElicitationInput::Boolean { .. }) => {}
    }
    parts.join(" · ")
}

fn bounds<T: std::fmt::Display>(what: &str, minimum: Option<T>, maximum: Option<T>) -> Vec<String> {
    vec![match (minimum, maximum) {
        (Some(min), Some(max)) => format!("{what} from {min} to {max}"),
        (Some(min), None) => format!("{what}, at least {min}"),
        (None, Some(max)) => format!("{what}, at most {max}"),
        (None, None) => what.to_owned(),
    }]
}

/// The host of `url` (without any user name or port), shown before the
/// user agrees to open it.
#[must_use]
pub fn url_host(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if host.starts_with('[') {
        host.split_once(']').map_or(host, |(inner, _)| &inner[1..])
    } else {
        host.split(':').next().unwrap_or(host)
    };
    (!host.is_empty()).then_some(host)
}

/// What looks off about a URL the user is asked to open.
#[must_use]
pub fn url_warnings(url: &str) -> Vec<&'static str> {
    let mut warnings = Vec::new();
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    match scheme.as_deref() {
        Some("https") => {}
        Some("http") => warnings.push("it is not HTTPS"),
        _ => warnings.push("it is not a web address"),
    }
    let host = url_host(url).unwrap_or_default();
    if host
        .split('.')
        .any(|label| label.to_ascii_lowercase().starts_with("xn--"))
        || !host.is_ascii()
    {
        warnings.push("its host has non-Latin letters (punycode): check it is the site you expect");
    }
    if url
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
    {
        warnings.push("it carries a user name before the host");
    }
    warnings
}

/// The words for how a question was answered: `accepted`, `declined`, or
/// `cancelled`, by whom when the daemon answered, and its reason.
#[must_use]
pub fn resolution_text(resolution: &ElicitationResolution) -> String {
    let action = match resolution.action {
        ElicitationAction::Accept => "accepted",
        ElicitationAction::Decline => "declined",
        ElicitationAction::Cancel => "cancelled",
        ElicitationAction::Unknown => "answered",
    };
    let by = match resolution.answered_by {
        ElicitationAnsweredBy::User => "",
        ElicitationAnsweredBy::Unattended => " (no one attends the session)",
        ElicitationAnsweredBy::RunEnded => " (the run ended)",
        ElicitationAnsweredBy::Completed => " (the agent finished it)",
        ElicitationAnsweredBy::Unsupported => " (it cannot be shown)",
        ElicitationAnsweredBy::Unknown => " (by the daemon)",
    };
    match &resolution.reason {
        Some(reason) => format!("{action}{by}: {reason}"),
        None => format!("{action}{by}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn form() -> ElicitationForm {
        serde_json::from_value(json!({"fields": [
            {"name": "plan", "title": "Plan", "required": true, "input": {"type": "choice",
                "choices": [{"value": "safe", "title": "Safe"}, {"value": "fast", "title": "Fast"}],
                "default": "fast"}},
            {"name": "note", "input": {"type": "text", "max_length": 3}},
            {"name": "tags", "input": {"type": "multi_choice",
                "choices": [{"value": "x", "title": "X"}, {"value": "y", "title": "Y"}], "default": ["y"]}},
            {"name": "count", "input": {"type": "integer", "minimum": 1, "default": 2}},
            {"name": "ratio", "input": {"type": "number"}},
            {"name": "ok", "input": {"type": "boolean"}},
            {"name": "later", "input": {"type": "colour"}}
        ]}))
        .unwrap()
    }

    #[test]
    fn a_draft_starts_from_the_defaults_and_answers_with_what_is_filled_in() {
        let form = form();
        let mut draft = FormDraft::new(&form);
        assert_eq!(
            draft.values,
            [
                DraftValue::Choice(Some(1)),
                DraftValue::Text(String::new()),
                DraftValue::MultiChoice(vec![false, true]),
                DraftValue::Text("2".into()),
                DraftValue::Text(String::new()),
                DraftValue::Boolean(false),
                DraftValue::Unsupported,
            ]
        );
        draft.values[4] = DraftValue::Text("0.5".into());
        assert_eq!(
            draft.content(&form).unwrap(),
            ElicitationContent::from([
                ("plan".to_owned(), ElicitationValue::Text("fast".into())),
                ("tags".to_owned(), ElicitationValue::List(vec!["y".into()])),
                ("count".to_owned(), ElicitationValue::Integer(2)),
                ("ratio".to_owned(), ElicitationValue::Number(0.5)),
                ("ok".to_owned(), ElicitationValue::Boolean(false)),
            ])
        );
    }

    #[test]
    fn the_first_field_that_does_not_fit_is_named() {
        let form = form();
        for (index, value, message) in [
            (0, DraftValue::Choice(None), "Plan is required"),
            (
                1,
                DraftValue::Text("long".into()),
                "note takes at most 3 characters",
            ),
            (
                3,
                DraftValue::Text("1.5".into()),
                "count must be a whole number",
            ),
            (3, DraftValue::Text("0".into()), "count must be at least 1"),
            (4, DraftValue::Text("x".into()), "ratio must be a number"),
        ] {
            let mut draft = FormDraft::new(&form);
            draft.values[index] = value;
            assert_eq!(
                draft.content(&form),
                Err(FieldProblem {
                    index,
                    message: message.to_owned()
                })
            );
        }
    }

    #[test]
    fn hints_name_bounds_formats_and_requirement() {
        let form = form();
        assert_eq!(field_hint(&form.fields[0]), "required");
        assert_eq!(field_hint(&form.fields[1]), "at most 3 characters");
        assert_eq!(field_hint(&form.fields[2]), "choose any");
        assert_eq!(field_hint(&form.fields[3]), "a whole number, at least 1");
        assert_eq!(field_hint(&form.fields[6]), "this client cannot fill it in");
    }

    #[test]
    fn a_urls_host_and_what_looks_off_about_it() {
        assert_eq!(
            url_host("https://user@auth.example.com:8443/x?y#z"),
            Some("auth.example.com")
        );
        assert_eq!(url_host("https://[::1]:80/"), Some("::1"));
        assert_eq!(url_host("mailto:a@b"), None);
        assert!(url_warnings("https://example.com/connect").is_empty());
        assert_eq!(url_warnings("http://example.com"), ["it is not HTTPS"]);
        assert_eq!(
            url_warnings("https://xn--exmple-cua.com/"),
            ["its host has non-Latin letters (punycode): check it is the site you expect"]
        );
        assert_eq!(
            url_warnings("https://example.com@evil.test/"),
            ["it carries a user name before the host"]
        );
        assert_eq!(
            url_warnings("javascript:alert(1)"),
            ["it is not a web address"]
        );
    }
}

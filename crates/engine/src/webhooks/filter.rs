//! A webhook's filter (`docs/site/specs/webhooks.md` section 5.2): every
//! condition must hold for a request to become an event.

use super::request::{ParsedBody, ValueSource, ValueSourceError, WebhookRequest, read_value};

/// The most conditions a filter holds.
pub const MAX_CONDITIONS: usize = 32;
/// The most strings one condition's `one_of` holds.
pub const MAX_ONE_OF: usize = 64;
/// The longest string in a `one_of`.
pub const MAX_VALUE_CHARS: usize = 512;

/// One condition: the value `source` reads is one of `one_of`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Condition {
    pub source: ValueSource,
    pub one_of: Vec<String>,
}

/// Why a filter is refused when a webhook is saved.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum FilterError {
    #[error("a filter holds at most 32 conditions")]
    TooManyConditions,
    #[error("a condition's one_of holds 1 to 64 strings of at most 512 characters")]
    OneOf,
    #[error(transparent)]
    Source(#[from] ValueSourceError),
}

/// Checks a filter's shape.
pub fn validate(conditions: &[Condition]) -> Result<(), FilterError> {
    if conditions.len() > MAX_CONDITIONS {
        return Err(FilterError::TooManyConditions);
    }
    for condition in conditions {
        condition.source.validate()?;
        let valid = (1..=MAX_ONE_OF).contains(&condition.one_of.len())
            && condition
                .one_of
                .iter()
                .all(|value| value.chars().count() <= MAX_VALUE_CHARS);
        if !valid {
            return Err(FilterError::OneOf);
        }
    }
    Ok(())
}

/// Whether `request` passes every condition; an empty filter passes all.
/// Values compare exactly, case included; a missing value, an object, or
/// an array matches nothing.
#[must_use]
pub fn matches(conditions: &[Condition], request: &WebhookRequest, body: &ParsedBody) -> bool {
    conditions.iter().all(|condition| {
        read_value(request, body, &condition.source)
            .is_some_and(|value| condition.one_of.contains(&value))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn condition(source: ValueSource, one_of: &[&str]) -> Condition {
        Condition {
            source,
            one_of: one_of.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn every_condition_must_hold() {
        let request = WebhookRequest {
            headers: vec![("X-GitHub-Event".to_owned(), "issues".to_owned())],
            ..WebhookRequest::default()
        };
        let body = ParsedBody::Json(json!({
            "action": "opened", "pull_request": {"draft": false}, "number": 7, "labels": ["a"],
        }));
        let event = condition(
            ValueSource::Header("x-github-event".into()),
            &["issues", "issue_comment"],
        );
        let action = condition(ValueSource::Json("/action".into()), &["opened"]);
        assert!(matches(&[], &request, &body));
        assert!(matches(&[event.clone(), action.clone()], &request, &body));
        let closed = condition(ValueSource::Json("/action".into()), &["closed"]);
        assert!(!matches(&[event, closed], &request, &body));
        for (pointer, one_of, expected) in [
            ("/pull_request/draft", "false", true),
            ("/number", "7", true),
            ("/number", "07", false),
            ("/action", "Opened", false),
            ("/labels", "a", false),
            ("/missing", "null", false),
        ] {
            let filter = [condition(ValueSource::Json(pointer.into()), &[one_of])];
            assert_eq!(
                matches(&filter, &request, &body),
                expected,
                "{pointer} {one_of}"
            );
        }
        let text = ParsedBody::Text("action=opened".into());
        assert!(!matches(&[action], &request, &text));
    }

    #[test]
    fn filters_are_checked_when_saved() {
        let ok = condition(ValueSource::Json("/action".into()), &["opened"]);
        assert_eq!(validate(std::slice::from_ref(&ok)), Ok(()));
        assert_eq!(
            validate(&vec![ok; MAX_CONDITIONS + 1]),
            Err(FilterError::TooManyConditions)
        );
        assert_eq!(
            validate(&[condition(ValueSource::Json("/a".into()), &[])]),
            Err(FilterError::OneOf)
        );
        let many = vec!["x"; MAX_ONE_OF + 1];
        assert_eq!(
            validate(&[condition(ValueSource::Json("/a".into()), &many)]),
            Err(FilterError::OneOf)
        );
        let long = "x".repeat(MAX_VALUE_CHARS + 1);
        assert_eq!(
            validate(&[condition(ValueSource::Json("/a".into()), &[&long])]),
            Err(FilterError::OneOf)
        );
        assert_eq!(
            validate(&[condition(ValueSource::Json("a".into()), &["x"])]),
            Err(FilterError::Source(ValueSourceError::Pointer))
        );
    }
}

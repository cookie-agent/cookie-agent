//! A webhook request as the HTTP layer read it, its body, and the values a
//! filter or a thread key reads (`docs/site/specs/webhooks.md` sections
//! 5.2, 5.3, 7.1, and 7.4).

use serde_json::Value;

/// The longest delivery ID the daemon keeps; a longer one counts as none.
pub const MAX_DELIVERY_ID_CHARS: usize = 200;
/// The longest thread key; a longer value counts as none (section 7.4).
pub const MAX_THREAD_KEY_CHARS: usize = 200;

/// One request, read in full (the body limit is the HTTP layer's).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WebhookRequest {
    /// The method, such as `POST`.
    pub method: String,
    /// The headers in the order received, with their names as sent. A
    /// value that is not UTF-8 is kept lossily.
    pub headers: Vec<(String, String)>,
    /// The body's bytes.
    pub body: Vec<u8>,
}

impl WebhookRequest {
    /// The first value of the header `name`, matched case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The media type of `Content-Type` in lowercase, without parameters.
    #[must_use]
    pub fn media_type(&self) -> Option<String> {
        let value = self.header("content-type")?;
        let media = value.split(';').next().unwrap_or_default().trim();
        (!media.is_empty()).then(|| media.to_ascii_lowercase())
    }
}

/// What a request's body is, as the filter, a thread key, and the model
/// read it.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedBody {
    /// No body.
    Empty,
    /// A JSON body.
    Json(Value),
    /// A form body (`application/x-www-form-urlencoded`) as a JSON object
    /// of its fields: a field sent once is a string, one sent several
    /// times an array of strings, and a `payload` field holding JSON (as
    /// GitHub's form mode sends) that JSON.
    Form(Value),
    /// UTF-8 text that is neither.
    Text(String),
    /// Anything else, shown only as an artifact.
    Binary,
}

impl ParsedBody {
    /// The JSON a pointer reads: a JSON body, or a form body's object.
    #[must_use]
    pub fn json(&self) -> Option<&Value> {
        match self {
            Self::Json(value) | Self::Form(value) => Some(value),
            Self::Empty | Self::Text(_) | Self::Binary => None,
        }
    }
}

/// Reads `request`'s body by its media type. A body labelled JSON (or
/// `+json`) that does not parse reads as text or binary; an unlabelled
/// body that parses as a JSON object or array reads as JSON.
#[must_use]
pub fn parse_body(request: &WebhookRequest) -> ParsedBody {
    let body = request.body.as_slice();
    if body.is_empty() {
        return ParsedBody::Empty;
    }
    let media = request.media_type();
    let labelled_json = media
        .as_deref()
        .is_some_and(|media| media == "application/json" || media.ends_with("+json"));
    if media.as_deref() == Some("application/x-www-form-urlencoded") {
        return ParsedBody::Form(parse_form(body));
    }
    let unlabelled = media.is_none() && starts_like_json(body);
    if (labelled_json || unlabelled)
        && let Ok(value) = serde_json::from_slice::<Value>(body)
    {
        return ParsedBody::Json(value);
    }
    match std::str::from_utf8(body) {
        Ok(text) if !text.contains('\0') => ParsedBody::Text(text.to_owned()),
        _ => ParsedBody::Binary,
    }
}

fn starts_like_json(body: &[u8]) -> bool {
    body.iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| matches!(byte, b'{' | b'['))
}

fn parse_form(body: &[u8]) -> Value {
    let mut object = serde_json::Map::new();
    for (name, value) in form_urlencoded::parse(body) {
        let value = Value::String(value.into_owned());
        match object.get_mut(name.as_ref()) {
            None => {
                object.insert(name.into_owned(), value);
            }
            Some(Value::Array(values)) => values.push(value),
            Some(first) => {
                let first = std::mem::take(first);
                object.insert(name.into_owned(), Value::Array(vec![first, value]));
            }
        }
    }
    if let Some(Value::String(payload)) = object.get("payload")
        && let Ok(parsed) = serde_json::from_str::<Value>(payload)
    {
        object.insert("payload".to_owned(), parsed);
    }
    Value::Object(object)
}

/// Where a filter condition or a thread key reads its value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueSource {
    /// A header, by name, case-insensitively.
    Header(String),
    /// A JSON Pointer (RFC 6901) into a JSON or form body.
    Json(String),
}

/// Why a value source is refused when a webhook is saved.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ValueSourceError {
    #[error("a header name must be 1 to 128 visible ASCII characters without ':'")]
    HeaderName,
    #[error("a JSON pointer must be empty or start with '/', and be at most 512 characters")]
    Pointer,
}

impl ValueSource {
    /// Checks the source's shape.
    pub fn validate(&self) -> Result<(), ValueSourceError> {
        match self {
            Self::Header(name) => {
                let valid = (1..=128).contains(&name.len())
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() && byte != b':');
                if valid {
                    Ok(())
                } else {
                    Err(ValueSourceError::HeaderName)
                }
            }
            Self::Json(pointer) => {
                if pointer.len() <= 512 && (pointer.is_empty() || pointer.starts_with('/')) {
                    Ok(())
                } else {
                    Err(ValueSourceError::Pointer)
                }
            }
        }
    }
}

/// The value `source` reads from a request, as a string: a header's
/// value, or the JSON a pointer names when it is a scalar (a string as
/// itself; a number, boolean, or null as its JSON text). An object, an
/// array, or nothing reads as `None`.
#[must_use]
pub fn read_value(
    request: &WebhookRequest,
    body: &ParsedBody,
    source: &ValueSource,
) -> Option<String> {
    match source {
        ValueSource::Header(name) => request.header(name).map(str::to_owned),
        ValueSource::Json(pointer) => scalar_text(body.json()?.pointer(pointer)?),
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(_) | Value::Bool(_) | Value::Null => Some(value.to_string()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// A request's thread key (section 7.4): the value `source` reads, unless
/// it is empty, longer than [`MAX_THREAD_KEY_CHARS`], or holds a control
/// character, when the event has none and gets a session of its own.
#[must_use]
pub fn thread_key(
    request: &WebhookRequest,
    body: &ParsedBody,
    source: &ValueSource,
) -> Option<String> {
    read_value(request, body, source).filter(|key| plain_short(key, MAX_THREAD_KEY_CHARS))
}

fn plain_short(value: &str, max_chars: usize) -> bool {
    !value.is_empty() && value.chars().count() <= max_chars && !value.chars().any(char::is_control)
}

/// The sender's own header naming a delivery, which a signature check
/// implies (section 5.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryHeader {
    /// `X-GitHub-Delivery`, for the `github` check.
    GitHub,
    /// `webhook-id`, for the `standard` check.
    Standard,
}

impl DeliveryHeader {
    const fn name(self) -> &'static str {
        match self {
            Self::GitHub => "x-github-delivery",
            Self::Standard => "webhook-id",
        }
    }
}

/// A request's delivery ID (section 5.3): the check's own header, then
/// `Idempotency-Key`. A value that is empty, longer than
/// [`MAX_DELIVERY_ID_CHARS`], or holds a control character is skipped.
#[must_use]
pub fn delivery_id(request: &WebhookRequest, own: Option<DeliveryHeader>) -> Option<String> {
    own.map(DeliveryHeader::name)
        .into_iter()
        .chain(["idempotency-key"])
        .filter_map(|name| request.header(name))
        .map(str::trim)
        .find(|value| plain_short(value, MAX_DELIVERY_ID_CHARS))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request(content_type: Option<&str>, body: &str) -> WebhookRequest {
        WebhookRequest {
            method: "POST".to_owned(),
            headers: content_type
                .map(|value| vec![("Content-Type".to_owned(), value.to_owned())])
                .unwrap_or_default(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn bodies_read_by_media_type() {
        assert_eq!(
            parse_body(&request(
                Some("application/json; charset=utf-8"),
                "{\"a\":1}"
            )),
            ParsedBody::Json(json!({"a": 1}))
        );
        assert_eq!(
            parse_body(&request(Some("application/vnd.github+json"), "[1]")),
            ParsedBody::Json(json!([1]))
        );
        assert_eq!(
            parse_body(&request(None, "  {\"a\":true}")),
            ParsedBody::Json(json!({"a": true}))
        );
        // Unlabelled scalars stay text; a broken JSON label falls back.
        assert_eq!(
            parse_body(&request(None, "42")),
            ParsedBody::Text("42".to_owned())
        );
        assert_eq!(
            parse_body(&request(Some("application/json"), "{oops")),
            ParsedBody::Text("{oops".to_owned())
        );
        assert_eq!(
            parse_body(&request(Some("text/plain"), "{\"a\":1}")),
            ParsedBody::Text("{\"a\":1}".to_owned())
        );
        assert_eq!(
            parse_body(&request(Some("text/plain"), "")),
            ParsedBody::Empty
        );
        let binary = WebhookRequest {
            body: vec![0xff, 0x00, 0x10],
            ..WebhookRequest::default()
        };
        assert_eq!(parse_body(&binary), ParsedBody::Binary);
        let nul = WebhookRequest {
            body: b"a\0b".to_vec(),
            ..WebhookRequest::default()
        };
        assert_eq!(parse_body(&nul), ParsedBody::Binary);
    }

    #[test]
    fn form_bodies_become_objects_with_a_parsed_payload() {
        let body = "payload=%7B%22action%22%3A%22opened%22%7D&tag=a&tag=b&tag=c&name=x+y";
        let parsed = parse_body(&request(Some("application/x-www-form-urlencoded"), body));
        assert_eq!(
            parsed,
            ParsedBody::Form(json!({
                "payload": {"action": "opened"},
                "tag": ["a", "b", "c"],
                "name": "x y",
            }))
        );
        let plain = parse_body(&request(
            Some("application/x-www-form-urlencoded"),
            "payload=not+json",
        ));
        assert_eq!(plain, ParsedBody::Form(json!({"payload": "not json"})));
    }

    #[test]
    fn values_read_scalars_as_strings() {
        let request = WebhookRequest {
            headers: vec![("X-GitHub-Event".to_owned(), "issues".to_owned())],
            ..WebhookRequest::default()
        };
        let body = ParsedBody::Json(json!({
            "s": "text", "n": 12, "f": 1.5, "b": false, "z": null,
            "o": {"k": 1}, "a": [1], "a~b": {"c/d": "escaped"},
        }));
        let read = |source: ValueSource| read_value(&request, &body, &source);
        assert_eq!(
            read(ValueSource::Header("x-github-event".into())).as_deref(),
            Some("issues")
        );
        assert_eq!(read(ValueSource::Header("X-Missing".into())), None);
        for (pointer, expected) in [
            ("/s", Some("text")),
            ("/n", Some("12")),
            ("/f", Some("1.5")),
            ("/b", Some("false")),
            ("/z", Some("null")),
            ("/o", None),
            ("/a", None),
            ("/a/0", Some("1")),
            ("/o/k", Some("1")),
            ("/a~0b/c~1d", Some("escaped")),
            ("/missing", None),
            ("", None),
        ] {
            assert_eq!(
                read(ValueSource::Json(pointer.into())).as_deref(),
                expected,
                "{pointer}"
            );
        }
        let text = ParsedBody::Text("x".into());
        assert_eq!(
            read_value(&request, &text, &ValueSource::Json("/s".into())),
            None
        );
    }

    #[test]
    fn thread_keys_are_short_plain_values() {
        let source = ValueSource::Json("/k".into());
        let key = |value: Value| {
            thread_key(
                &WebhookRequest::default(),
                &ParsedBody::Json(json!({"k": value})),
                &source,
            )
        };
        assert_eq!(key(json!(1234)).as_deref(), Some("1234"));
        assert_eq!(key(json!("a".repeat(200))).map(|key| key.len()), Some(200));
        assert_eq!(
            key(json!("é".repeat(200))).map(|key| key.chars().count()),
            Some(200)
        );
        assert_eq!(key(json!("a".repeat(201))), None);
        assert_eq!(key(json!("")), None);
        assert_eq!(key(json!("a\nb")), None);
    }

    #[test]
    fn delivery_ids_prefer_the_checks_own_header() {
        let request = WebhookRequest {
            headers: vec![
                ("Idempotency-Key".to_owned(), "idem".to_owned()),
                ("X-GitHub-Delivery".to_owned(), "gh".to_owned()),
                ("webhook-id".to_owned(), "  std ".to_owned()),
            ],
            ..WebhookRequest::default()
        };
        assert_eq!(
            delivery_id(&request, Some(DeliveryHeader::GitHub)).as_deref(),
            Some("gh")
        );
        assert_eq!(
            delivery_id(&request, Some(DeliveryHeader::Standard)).as_deref(),
            Some("std")
        );
        assert_eq!(delivery_id(&request, None).as_deref(), Some("idem"));
        let long = WebhookRequest {
            headers: vec![
                ("X-GitHub-Delivery".to_owned(), "x".repeat(201)),
                ("Idempotency-Key".to_owned(), "fallback".to_owned()),
            ],
            ..WebhookRequest::default()
        };
        assert_eq!(
            delivery_id(&long, Some(DeliveryHeader::GitHub)).as_deref(),
            Some("fallback")
        );
        assert_eq!(delivery_id(&WebhookRequest::default(), None), None);
    }

    #[test]
    fn value_sources_are_checked() {
        assert!(
            ValueSource::Header("X-GitHub-Event".into())
                .validate()
                .is_ok()
        );
        assert!(ValueSource::Header(String::new()).validate().is_err());
        assert!(ValueSource::Header("a:b".into()).validate().is_err());
        assert!(ValueSource::Header("a b".into()).validate().is_err());
        assert!(ValueSource::Json(String::new()).validate().is_ok());
        assert!(ValueSource::Json("/issue/number".into()).validate().is_ok());
        assert!(ValueSource::Json("issue".into()).validate().is_err());
        assert!(
            ValueSource::Json(format!("/{}", "a".repeat(512)))
                .validate()
                .is_err()
        );
    }
}

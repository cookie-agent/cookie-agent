//! Test-only: history format 0, which never existed, is format 1 with each
//! `diagnostic`'s `message` stored as `text`. It exercises the conversion
//! machinery before the first real converter does.

use serde_json::Value;

/// A line carrying `"convert_fails": true` fails the conversion, for tests
/// of what a failure leaves behind.
pub(super) fn convert(lines: Vec<Value>) -> Result<Vec<Value>, String> {
    lines
        .into_iter()
        .map(|mut line| {
            if line.get("convert_fails") == Some(&Value::Bool(true)) {
                return Err("the test converter was told to fail".to_owned());
            }
            if let Some(payload) = line
                .get_mut("payload")
                .and_then(Value::as_object_mut)
                .filter(|payload| payload.get("type").and_then(Value::as_str) == Some("diagnostic"))
                && let Some(text) = payload.remove("text")
            {
                payload.insert("message".into(), text);
            }
            Ok(line)
        })
        .collect()
}

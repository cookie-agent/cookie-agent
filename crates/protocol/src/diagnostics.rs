//! Bounded user-facing diagnostics. This is pattern-based redaction, not a guarantee
//! against arbitrary secret echoes. Never pass request headers or credential stores.
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::{DiagnosticText, ModelErrorSummary, SafeErrorMessage};

fn sensitive(key: &str) -> bool {
    let key: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        key.as_str(),
        "headers" | "request" | "requestheaders" | "requestbody"
    ) || key.ends_with("token")
        || [
            "authorization",
            "password",
            "passwd",
            "secret",
            "apikey",
            "credential",
            "cookie",
            "privatekey",
        ]
        .iter()
        .any(|part| key.contains(part))
}

fn scrub_json(value: &mut Value, known_secrets: &[&str]) {
    match value {
        Value::Object(map) => {
            let credential_header = map
                .iter()
                .any(|(key, value)| value.as_str().is_some_and(|name| header_name(key, name)));
            for (key, value) in map {
                if sensitive(key) || (credential_header && key.eq_ignore_ascii_case("value")) {
                    *value = Value::String("[REDACTED]".into());
                } else {
                    scrub_json(value, known_secrets);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                scrub_json(value, known_secrets);
            }
        }
        Value::String(text) => *text = scrub_text(text, known_secrets),
        _ => {}
    }
}

fn header_name(key: &str, value: &str) -> bool {
    key.eq_ignore_ascii_case("name") && sensitive(value)
}

// Return the end of a quoted token, respecting escaped quotes and backslashes.
// An unfinished token consumes the remainder: partial credentials must not leak.
fn quoted_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    bytes.len()
}

// Double quotes delimit JSON strings. Single quotes delimit configuration keys
// only when followed by an assignment; prose apostrophes must not hide fragments.
fn quote_start(value: &str, start: usize) -> bool {
    let bytes = value.as_bytes();
    if bytes[start] == b'"' {
        return true;
    }
    if bytes[start] != b'\''
        || value[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return false;
    }
    let end = quoted_end(bytes, start);
    if end <= start + 1 || bytes[end - 1] != b'\'' {
        return false;
    }
    let suffix = value[end..].trim_start();
    suffix.starts_with([':', '='])
}

fn decoded_token(token: &str) -> Option<String> {
    if token.starts_with('"') {
        serde_json::from_str(token).ok()
    } else {
        token
            .strip_prefix('\'')?
            .strip_suffix('\'')
            .map(str::to_owned)
    }
}

// Collect sibling value ranges in each object before rendering, so value/name
// order does not matter. Unfinished outer objects are finalized at EOF, too.
fn header_value_ranges(value: &str) -> Vec<std::ops::Range<usize>> {
    #[derive(Default)]
    struct Fields {
        credential: bool,
        values: Vec<std::ops::Range<usize>>,
    }
    fn finish(fields: Fields, ranges: &mut Vec<std::ops::Range<usize>>) {
        if fields.credential {
            ranges.extend(fields.values);
        }
    }
    let bytes = value.as_bytes();
    let mut scopes = vec![Fields::default()];
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if quote_start(value, index) {
            index = quoted_end(bytes, index);
        } else if bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' {
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || b"_-".contains(&bytes[index]))
            {
                index += 1;
            }
        } else {
            match bytes[index] {
                b'{' | b'[' => scopes.push(Fields::default()),
                b'}' | b']' if scopes.len() > 1 => finish(scopes.pop().unwrap(), &mut ranges),
                _ => {}
            }
            index += value[index..].chars().next().unwrap().len_utf8();
            continue;
        }
        let token = &value[start..index];
        let decoded = decoded_token(token);
        let key = decoded.as_deref().unwrap_or(token);
        let mut separator = index;
        while separator < bytes.len() && bytes[separator].is_ascii_whitespace() {
            separator += 1;
        }
        if separator == bytes.len() || !matches!(bytes[separator], b':' | b'=') {
            continue;
        }
        let mut value_start = separator + 1;
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        let end = value_end(bytes, value_start);
        let fields = scopes.last_mut().unwrap();
        if key.eq_ignore_ascii_case("value") {
            fields.values.push(value_start..end);
        } else if let Some(name) = decoded_token(&value[value_start..end]) {
            fields.credential |= header_name(key, &name);
        }
        index = value_start;
    }
    for fields in scopes {
        finish(fields, &mut ranges);
    }
    ranges.sort_by_key(|range| range.start);
    ranges
}

fn value_end(bytes: &[u8], start: usize) -> usize {
    if start == bytes.len() {
        return start;
    }
    if matches!(bytes[start], b'"' | b'\'') {
        return quoted_end(bytes, start);
    }
    if matches!(bytes[start], b'{' | b'[') {
        let mut stack = vec![bytes[start]];
        let mut index = start + 1;
        while index < bytes.len() {
            match bytes[index] {
                b'"' | b'\'' => {
                    index = quoted_end(bytes, index);
                    continue;
                }
                b'{' | b'[' => stack.push(bytes[index]),
                b'}' | b']' => {
                    let expected = if bytes[index] == b'}' { b'{' } else { b'[' };
                    if stack.pop() != Some(expected) {
                        return bytes.len();
                    }
                    if stack.is_empty() {
                        return index + 1;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        return bytes.len();
    }
    let mut end = start;
    while end < bytes.len()
        && !bytes[end].is_ascii_whitespace()
        && !b"&,;<>}]\"'".contains(&bytes[end])
    {
        end += 1;
    }
    // Authorization values commonly include a scheme followed by a credential.
    if bytes[start..end].eq_ignore_ascii_case(b"bearer")
        || bytes[start..end].eq_ignore_ascii_case(b"basic")
    {
        while end < bytes.len() && bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && !b"&,;<>}".contains(&bytes[end])
        {
            end += 1;
        }
    }
    end
}

fn replace_known(value: &str, secrets: &[&str]) -> String {
    let mut text = value.to_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "[REDACTED]");
    }
    text
}

// Recognize field assignments even inside prefixed or incomplete JSON. Sensitive
// nested objects/arrays are consumed as a unit; malformed values fail closed.
fn scrub_fields(value: &str, known_secrets: &[&str]) -> String {
    let bytes = value.as_bytes();
    let mut output = String::new();
    let mut index = 0;
    let mut header_values = header_value_ranges(value).into_iter().peekable();
    while index < bytes.len() {
        while header_values
            .peek()
            .is_some_and(|range| range.start < index)
        {
            header_values.next();
        }
        if header_values
            .peek()
            .is_some_and(|range| range.start == index)
        {
            let range = header_values.next().unwrap();
            output.push_str("\"[REDACTED]\"");
            index = range.end;
            continue;
        }
        let start = index;
        let quoted = quote_start(value, index);
        if quoted {
            index = quoted_end(bytes, index);
        } else if bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' {
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || b"_-".contains(&bytes[index]))
            {
                index += 1;
            }
        } else {
            let character = value[index..].chars().next().unwrap();
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        let token = &value[start..index];
        let decoded = if bytes[start] == b'"' {
            serde_json::from_str::<String>(token).ok()
        } else {
            None
        };
        let key = decoded
            .as_deref()
            .unwrap_or_else(|| token.trim_matches(['\'', '"']));
        let mut separator = index;
        while separator < bytes.len() && bytes[separator].is_ascii_whitespace() {
            separator += 1;
        }
        if separator < bytes.len() && matches!(bytes[separator], b':' | b'=') && sensitive(key) {
            let mut value_start = separator + 1;
            while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
                value_start += 1;
            }
            output.push_str(&replace_known(&value[start..value_start], known_secrets));
            output.push_str("\"[REDACTED]\"");
            index = value_end(bytes, value_start);
        } else if let Some(decoded) = decoded {
            // Decode before applying known-value redaction (e.g. review\u002dsecret).
            let scrubbed = scrub_text(&decoded, known_secrets);
            output.push_str(&serde_json::to_string(&scrubbed).expect("string serializes"));
        } else {
            output.push_str(&replace_known(token, known_secrets));
        }
    }
    output
}

fn scrub_text(value: &str, known_secrets: &[&str]) -> String {
    static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
        vec![
            (
                Regex::new(r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9+/_.=~:-]+").unwrap(),
                "$1 [REDACTED]",
            ),
            (Regex::new(r"\bsk-[A-Za-z0-9_-]+").unwrap(), "[REDACTED]"),
            (
                Regex::new(r"(?i)(https?://)[^\s/@]+:[^\s/@]+@").unwrap(),
                "$1[REDACTED]@",
            ),
            (
                Regex::new(r"(?i)([?&](?:key|sig|signature|auth)=)[^\s&#]+").unwrap(),
                "$1[REDACTED]",
            ),
        ]
    });
    let mut text = scrub_fields(value, known_secrets);
    for (pattern, replacement) in PATTERNS.iter() {
        text = pattern.replace_all(&text, *replacement).into_owned();
    }
    replace_known(&text, known_secrets)
}

/// Scrub raw and decoded values before UTF-8 truncation. Preserve LF/tab and
/// remove terminal/format controls. This cannot recognize arbitrary secret echoes.
pub fn sanitize(value: &str, known_secrets: &[&str], maximum: usize) -> String {
    let mut text = if let Ok(mut json) = serde_json::from_str::<Value>(value) {
        scrub_json(&mut json, known_secrets);
        replace_known(
            &serde_json::to_string_pretty(&json).expect("JSON value serializes"),
            known_secrets,
        )
    } else {
        scrub_text(value, known_secrets)
    };
    static CONTROLS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\p{Cc}\p{Cf}&&[^\n\t]]").unwrap());
    text = CONTROLS.replace_all(&text, " ").into_owned();
    if text.trim().is_empty() {
        text = "No error detail was supplied".into();
    }
    const MARKER: &str = "\n[diagnostic truncated]";
    if text.len() > maximum {
        let mut end = maximum.saturating_sub(MARKER.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(&MARKER[..MARKER.len().min(maximum)]);
    }
    text
}

pub fn detail(value: &str) -> DiagnosticText {
    DiagnosticText::new(sanitize(value, &[], DiagnosticText::MAX_BYTES))
        .expect("sanitized diagnostic")
}

pub fn headline(value: &str) -> SafeErrorMessage {
    SafeErrorMessage::new(
        sanitize(value, &[], SafeErrorMessage::MAX_BYTES).replace(['\n', '\t'], " "),
    )
    .expect("sanitized headline")
}

pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    for _ in 0..16 {
        let Some(cause) = source else {
            break;
        };
        let text = cause.to_string();
        if !parts.last().is_some_and(|last| last.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    detail(&parts.join(": ")).to_string()
}

pub fn model(error: &ModelErrorSummary) -> String {
    let mut text = error.message.to_string();
    if let Some(status) = error.http_status {
        text.push_str(&format!(" · HTTP {status}"));
    }
    if let Some(code) = &error.vendor_code {
        text.push_str(&format!(" · code {code}"));
    }
    if let Some(id) = &error.request_id {
        text.push_str(&format!(" · request {id}"));
    }
    text.push_str(&format!(
        " · stage {:?} · retryable {} · {} bytes received",
        error.stage, error.retryable, error.bytes_received
    ));
    if let Some(delay) = error.retry_after_ms {
        text.push_str(&format!(" · retry after {delay}ms"));
    }
    if let Some(body) = &error.response_body {
        text.push_str(&format!("\nResponse body:\n{}", detail(body.as_str())));
    }
    text
}

pub fn run_error(
    error: &SafeErrorMessage,
    model_error: Option<&ModelErrorSummary>,
    model: Option<&crate::ResolvedModelRef>,
) -> String {
    let mut text = error.to_string();
    if let Some(model) = model {
        text.push_str(&format!(
            "\nModel: {} (provider {}, adapter {:?})",
            model.selection.model, model.provider_id, model.adapter_id
        ));
    }
    if let Some(model_error) = model_error {
        text.push_str(&format!("\n{}", self::model(model_error)));
    }
    text
}

pub fn internal(failure: &crate::InternalAgentFailure) -> String {
    match &failure.model_error {
        Some(error) => format!("{}\n{}", failure.message, model(error)),
        None => failure.message.to_string(),
    }
}

pub fn internal_fallback(
    kind: crate::InternalAgentKind,
    from: &crate::InternalAgentBackend,
    to: &crate::InternalAgentBackend,
    attempts: u32,
    failure: &crate::InternalAgentFailure,
) -> String {
    fn backend(value: &crate::InternalAgentBackend) -> String {
        match value {
            crate::InternalAgentBackend::Model { resolved_model } => {
                resolved_model.selection.model.to_string()
            }
            crate::InternalAgentBackend::Builtin { name, .. } => name.to_string(),
        }
    }
    format!(
        "internal agent {kind:?} fallback {} → {} after {attempts} attempt(s): {}",
        backend(from),
        backend(to),
        internal(failure)
    )
}

pub fn tool(termination: &crate::ToolCallTermination) -> String {
    if let Some(error) = &termination.error {
        // The persisted message may already fill its budget. Preserve both its
        // status/context and final cause when adding the error-code prefix.
        return excerpt(
            &format!("{}: {}", error.code, error.message),
            DiagnosticText::MAX_BYTES,
        );
    }
    if let Some(result) = &termination.result {
        return tool_result(result);
    }
    format!(
        "tool ended with {:?}; no diagnostic supplied",
        termination.outcome
    )
}

pub fn tool_result(result: &crate::PersistedToolResult) -> String {
    let mut parts = vec![excerpt(result.title.as_str(), 256)];
    // Keep exit/source context ahead of output, with a separate budget for each
    // diagnostic field so a large error object cannot bury the process status.
    for key in [
        "status",
        "exit_code",
        "exitCode",
        "signal",
        "source",
        "path",
        "filePath",
        "error",
        "message",
    ] {
        if let Some(value) = result.metadata.get(key).filter(|value| !value.is_null()) {
            let value = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned);
            parts.push(format!("{key}: {}", excerpt(&value, 160)));
        }
    }
    let mut sections = tool_sections(result);
    // Named stream headings are emitted by the runtime in declaration order.
    // Give each declared stream a budget, prioritizing diagnostic streams.
    sections.sort_by_key(|(name, _)| match *name {
        "stderr" | "diagnostics" | "errors" | "error" => 0,
        "stdout" => 2,
        _ => 1,
    });
    let used = parts.iter().map(|part| part.len() + 1).sum::<usize>();
    let budget = DiagnosticText::MAX_BYTES.saturating_sub(used);
    if sections.is_empty() {
        parts.push(excerpt(
            result
                .display
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("Tool reported failure without a reason"),
            budget.saturating_sub(1),
        ));
    } else {
        let per_section = budget / sections.len();
        for (name, value) in sections {
            let heading = format!("[{name}]\n");
            parts.push(format!(
                "{heading}{}",
                excerpt(value, per_section.saturating_sub(heading.len() + 1))
            ));
        }
    }
    detail(&parts.join("\n")).to_string()
}

fn tool_sections(result: &crate::PersistedToolResult) -> Vec<(&str, &str)> {
    let mut sections = Vec::new();
    if let Some(retained) = &result.retained_output {
        let mut end = result.output.len();
        for (index, stream) in retained.streams.iter().enumerate().rev() {
            let Some(name) = stream.name.as_deref() else {
                break;
            };
            let marker = if index == 0 {
                format!("[{name}]\n")
            } else {
                format!("\n\n[{name}]\n")
            };
            let Some(start) = result.output[..end].rfind(&marker) else {
                sections.clear();
                break;
            };
            let value = &result.output[start + marker.len()..end];
            if !value.trim().is_empty() {
                sections.push((name, value));
            }
            end = start;
        }
    }
    if sections.is_empty() && !result.output.trim().is_empty() {
        sections.push(("output", &result.output));
    }
    sections
}

// Scrub the whole section before retaining its head and tail. Error summaries
// commonly occur at the end of a noisy stream; clipping must not split secrets.
fn excerpt(value: &str, maximum: usize) -> String {
    let clean = sanitize(value, &[], usize::MAX);
    if clean.len() <= maximum {
        return clean;
    }
    const MARKER: &str = "\n[diagnostic truncated]\n";
    if maximum < MARKER.len() {
        return sanitize(&clean, &[], maximum);
    }
    let mut head = (maximum - MARKER.len()) / 2;
    let mut tail = clean.len() - (maximum - MARKER.len() - head);
    while !clean.is_char_boundary(head) {
        head -= 1;
    }
    while !clean.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{MARKER}{}", &clean[..head], &clean[tail..])
}

/// Only diagnostic fields are selected from RPC data. Debug remains redacted.
pub fn rpc(error: &crate::JsonRpcError) -> String {
    let mut text = format!("{} (RPC {})", error.message, error.code);
    if let Some(Value::Object(data)) = &error.data {
        for key in [
            "message",
            "reason",
            "detail",
            "cause",
            "code",
            "path",
            "line",
            "column",
            "request_id",
            "expected_revision",
            "found_revision",
        ] {
            if let Some(value) = data.get(key).filter(|value| !value.is_null()) {
                let value = value.as_str().map_or_else(
                    || detail(&value.to_string()).to_string(),
                    |value| detail(value).to_string(),
                );
                text.push_str(&format!("\n{key}: {value}"));
            }
        }
    }
    detail(&text).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_header_pairs_are_scrubbed_in_every_supported_container() {
        for name in [
            "Authorization",
            "aUtHoRiZaTiOn",
            "X-Api-Key",
            "Proxy-Authorization",
            "Set-Cookie",
            "X-Auth-Token",
        ] {
            for reversed in [false, true] {
                let object = if reversed {
                    format!(
                        r#"{{"value":"opaque-review-credential","NAME":"{name}","message":"Harmless message"}}"#
                    )
                } else {
                    format!(
                        r#"{{"name":"{name}","VALUE":"opaque-review-credential","message":"Harmless message"}}"#
                    )
                };
                let embedded = serde_json::json!({"message": object}).to_string();
                for input in [
                    object.clone(),
                    format!("gateway: {object}"),
                    format!("gateway: {}", &object[..object.len() - 1]),
                    embedded,
                ] {
                    let output = sanitize(&input, &[], 4096);
                    assert!(!output.contains("opaque-review-credential"), "{output}");
                    assert!(output.contains("Harmless message"), "{output}");
                    assert!(output.contains("[REDACTED]"), "{output}");
                }
            }
        }
        let output = detail(
            r#"gateway: [{"name":"X-Api-Key","value":"opaque-review-credential"},{"name":"Content-Type","value":"application/json","message":"Harmless message"}]"#,
        );
        assert!(!output.as_str().contains("opaque-review-credential"));
        assert!(output.as_str().contains("application/json"));
        assert!(output.as_str().contains("Harmless message"));
        for input in [
            r#"gateway: {"name":"X-Api-Key","value":"opaque-review-credential"#,
            r#"gateway: {'name':'X-Api-Key','value':'opaque-review-credential'}"#,
            r#"gateway: {"name":"Authorization","value":{"parts":["opaque-review-credential""#,
        ] {
            assert!(!detail(input).as_str().contains("opaque-review-credential"));
        }
    }

    #[test]
    fn prose_apostrophes_do_not_hide_credential_fragments_or_escaped_known_values() {
        for input in [
            r#"Couldn't decode upstream response: {"password":"credential-value","message":"Harmless message"}"#,
            r#"Provider's response: {"message":"review\u002dsecret","reason":"Harmless message"}"#,
            r#"Couldn't decode upstream response: {"password":"review\"secret-tail","message":"Harmless message"}"#,
            r#"Provider's response: {"credentials":{"parts":["credential-value"]},"message":"Harmless message"}"#,
            r#"The providers' response: {"password":"credential-value","message":"Harmless message"}"#,
            r#"A stray ' before JSON: {"password":"credential-value","message":"Harmless message"}"#,
            r#"Couldn't decode config: {'password':'review\'secret-tail','message':'Harmless message'}"#,
        ] {
            let output = sanitize(input, &["review-secret"], 4096);
            for secret in [
                "credential-value",
                "review-secret",
                r"review\u002dsecret",
                "secret-tail",
            ] {
                assert!(!output.contains(secret), "{output}");
            }
            assert!(output.contains("Harmless message"), "{output}");
        }
    }

    #[test]
    fn persisted_tool_error_wrapper_preserves_status_and_unicode_final_cause() {
        let result = crate::PersistedToolResult {
            title: crate::SafeDisplayText::new("Generic tool").unwrap(),
            output: format!("{}\nRequired asset missing é", "é".repeat(6000)),
            metadata: serde_json::json!({"status":23}),
            display: None,
            retained_output: None,
            truncation: None,
            attachments: vec![],
            additional_messages: vec![],
        };
        let message = headline(&tool_result(&result));
        assert!(message.as_str().len() >= 4090);
        assert!(message.as_str().ends_with("Required asset missing é"));
        let termination = crate::ToolCallTermination {
            tool_call_id: crate::ToolCallId::new_v7(),
            owner: crate::AssistantToolCallRef {
                model_turn_seq: 1,
                content_index: 0,
                model_call_id: crate::ModelCallId::new("call").unwrap(),
                provider_item_id: None,
            },
            outcome: crate::ToolTerminationOutcome::Failed,
            result: Some(result),
            error: Some(crate::SafeToolError {
                code: crate::SafeCode::new("execution_failed").unwrap(),
                message,
            }),
        };
        let persisted: crate::ToolCallTermination =
            serde_json::from_value(serde_json::to_value(termination).unwrap()).unwrap();
        let output = tool(&persisted);
        assert!(output.contains("status: 23"), "{output}");
        assert!(output.ends_with("Required asset missing é"), "{output}");
        assert!(output.contains("[diagnostic truncated]"));
        assert_eq!(output.matches("execution_failed:").count(), 1);
        assert!(output.len() <= DiagnosticText::MAX_BYTES);
    }

    #[test]
    fn custom_streams_and_generic_output_keep_final_causes_with_status() {
        for named in [false, true] {
            let digest = crate::Sha256Digest::of_bytes(b"fixture");
            let reference = crate::ArtifactReference {
                uri: format!("artifact://sha256/{digest}"),
            };
            let output = format!(
                "[results]\n{}\n\n[trace]\n{}\nCustom final cause",
                "r".repeat(6000),
                "t".repeat(6000)
            );
            let result = crate::PersistedToolResult {
                title: crate::SafeDisplayText::new("Search").unwrap(),
                output,
                display: Some("Finished".into()),
                metadata: serde_json::json!({"status":23,"source":"/work/input"}),
                truncation: None,
                retained_output: named.then(|| crate::RetainedToolOutput {
                    reference: reference.clone(),
                    incomplete: true,
                    streams: ["results", "trace"]
                        .into_iter()
                        .map(|name| crate::RetainedToolStream {
                            name: Some(name.into()),
                            reference: reference.clone(),
                            sha256: digest.clone(),
                            byte_length: 6000,
                            line_count: 1,
                            truncated: false,
                            next_offset: None,
                        })
                        .collect(),
                }),
                attachments: vec![],
                additional_messages: vec![],
            };
            let text = tool_result(&result);
            assert!(text.contains("Custom final cause"), "{text}");
            assert!(text.contains("status: 23"));
            assert!(text.contains("source: /work/input"));
            assert!(text.contains("[diagnostic truncated]"));
            assert!(text.len() <= DiagnosticText::MAX_BYTES);
        }
    }

    #[test]
    fn decoded_nested_json_and_prefixed_strings_scrub_known_values() {
        for input in [
            r#"{"message":"review\u002dsecret","nested":[{"echo":"review-secret"}]}"#,
            r#"gateway: {"message":"review\u002dsecret"}"#,
        ] {
            let text = sanitize(input, &["review-secret"], 4096);
            assert!(!text.contains("review-secret"), "{text}");
            assert!(!text.contains(r"review\u002dsecret"), "{text}");
            assert!(text.contains("[REDACTED]"));
        }
    }

    #[test]
    fn quoted_and_nested_credentials_fail_closed_even_in_incomplete_json() {
        for input in [
            r#"gateway: {"password":"review\"secret-tail","message":"Useful cause"}"#,
            r#"gateway: {"password":"review\\\"secret-tail","message":"Useful cause"}"#,
            r#"gateway: {'password':'review\'secret-tail','message':'Useful cause'}"#,
            r#"gateway: {"pass\u0077ord":"review\"secret-tail","message":"Useful cause"}"#,
            r#"gateway: {"credentials":{"first":"review","second":["secret-tail"]},"message":"Useful cause"}"#,
            r#"gateway: {"credentials":{"first":"review","second":["secret-tail""#,
            r#"gateway: {"credentials":{"first":"review","second":["secret-tail"},"message":"hidden after malformed object"}"#,
            r#"gateway: {"password":"review\"secret-tail"#,
        ] {
            let text = sanitize(input, &[], 4096);
            assert!(!text.contains("review"), "{text}");
            assert!(!text.contains("secret-tail"), "{text}");
            assert!(text.contains("[REDACTED]"), "{text}");
            if input.ends_with("Useful cause\"}") {
                assert!(text.contains("Useful cause"), "{text}");
            }
        }
    }
    #[test]
    fn legacy_failures_and_summaries_read_and_new_diagnostics_round_trip() {
        let old = serde_json::json!({"type":"run_failed", "error":"invalid request"});
        let mut event: crate::EventPayload = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap(), old);
        let summary: ModelErrorSummary = serde_json::from_value(serde_json::json!({"kind":"invalid_request","message":"invalid request","retryable":false,"stage":"response_body","http_status":400,"bytes_received":10,"vendor_code":null,"request_id":null,"retry_after_ms":null})).unwrap();
        assert!(summary.response_body.is_none());
        if let crate::EventPayload::RunFailed { model_error, .. } = &mut event {
            *model_error = Some(ModelErrorSummary {
                response_body: Some(detail("line one\nline two")),
                ..summary
            });
        }
        let wire = serde_json::to_value(&event).unwrap();
        let decoded: crate::EventPayload = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    }
    #[test]
    fn diagnostic_redacts_nested_json_tokens_urls_and_known_values() {
        let text = sanitize(
            r#"{"error":{"message":"bad option","password":"hidden","nested":[{"api_key":"hidden2"}],"echo":"Bearer opaque sk-other https://user:pass@host/?token=query"},"known":"literal"}"#,
            &["literal"],
            4096,
        );
        for secret in [
            "hidden",
            "hidden2",
            "opaque",
            "sk-other",
            "user:pass",
            "=query",
            "literal",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("bad option"));
        assert!(text.contains('\n'));
        serde_json::from_str::<Value>(&text).expect("redacted JSON remains readable JSON");
    }
    #[test]
    fn diagnostic_bounds_utf8_controls_and_preserves_gateway_text() {
        let text = sanitize(
            &format!(
                "<html>Gateway\nfailed</html>\u{1b}\u{202e}{}",
                "é".repeat(4096)
            ),
            &[],
            4096,
        );
        assert!(text.starts_with("<html>Gateway\nfailed</html>"));
        assert!(!text.contains(['\u{1b}', '\u{202e}']));
        assert!(text.len() <= 4096);
        assert!(text.ends_with("[diagnostic truncated]"));
    }

    #[test]
    fn useful_token_limits_survive_redaction() {
        let text = detail(r#"{"max_tokens":1024,"token_count":900,"access_token":"secret-value"}"#);
        let json: Value = serde_json::from_str(text.as_str()).unwrap();
        assert_eq!(json["max_tokens"], 1024);
        assert_eq!(json["token_count"], 900);
        assert_eq!(json["access_token"], "[REDACTED]");
    }

    #[test]
    fn diagnostic_wire_rejects_terminal_controls_and_oversized_text() {
        assert!(DiagnosticText::new("one\ntwo\tthree").is_ok());
        assert!(DiagnosticText::new("escape\u{1b}").is_err());
        assert!(DiagnosticText::new("bidi\u{202e}").is_err());
        assert!(DiagnosticText::new("x".repeat(4097)).is_err());
    }
    #[test]
    fn rpc_display_selects_cause_without_disclosing_arbitrary_data() {
        let error = crate::JsonRpcError {
            code: -32603,
            message: "operation failed".into(),
            data: Some(
                serde_json::json!({"cause":"file missing", "path":"/work/config.toml", "line":7, "credentials":"secret", "request":{"body":"private"}}),
            ),
        };
        let display = rpc(&error);
        assert!(display.contains("file missing"));
        assert!(display.contains("/work/config.toml"));
        assert!(display.contains("line: 7"));
        assert!(!display.contains("private"));
        assert!(!format!("{error:?}").contains("file missing"));
    }
}

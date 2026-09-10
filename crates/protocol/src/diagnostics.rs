//! Bounded user-facing diagnostics. Preserve diagnostic content while removing
//! terminal/format controls and truncating text to the display budget.
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::{DiagnosticText, ModelErrorSummary, SafeErrorMessage};

/// Preserve LF/tab, remove terminal/format controls, and truncate at UTF-8 boundaries.
pub fn sanitize(value: &str, maximum: usize) -> String {
    let mut text = if let Ok(json) = serde_json::from_str::<Value>(value) {
        serde_json::to_string_pretty(&json).expect("JSON value serializes")
    } else {
        value.to_owned()
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
    DiagnosticText::new(sanitize(value, DiagnosticText::MAX_BYTES)).expect("sanitized diagnostic")
}

pub fn headline(value: &str) -> SafeErrorMessage {
    SafeErrorMessage::new(sanitize(value, SafeErrorMessage::MAX_BYTES).replace(['\n', '\t'], " "))
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
        // Preserve both status/context and final cause when adding the prefix.
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
    // Give status/source context and each output field separate budgets.
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
    // Named stream headings come from the runtime in declaration order.
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

// Keep the head and tail: final causes often follow a noisy stream.
fn excerpt(value: &str, maximum: usize) -> String {
    let clean = sanitize(value, usize::MAX);
    if clean.len() <= maximum {
        return clean;
    }
    const MARKER: &str = "\n[diagnostic truncated]\n";
    if maximum < MARKER.len() {
        return sanitize(&clean, maximum);
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
    fn diagnostics_preserve_credentials_in_json_and_prose() {
        let input = r#"{"password":"hidden","nested":[{"api_key":"hidden2"}],"access_token":"literal","echo":"Bearer opaque Basic abc sk-other https://user:pass@host/?token=query&sig=signed","headers":[{"name":"Authorization","value":"credential"}]}"#;
        let text = sanitize(input, 4096);
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            serde_json::from_str::<Value>(input).unwrap()
        );
        for input in [
            format!("gateway: {input}"),
            r#"Couldn't decode response: {"password":"review\"secret-tail"#.into(),
            "Bearer opaque Basic abc sk-other https://user:pass@host/?key=query&sig=signed".into(),
        ] {
            assert_eq!(sanitize(&input, 4096), input);
        }
    }

    #[test]
    fn diagnostic_bounds_utf8_controls_and_preserves_gateway_text() {
        let text = sanitize(
            &format!(
                "<html>Gateway\nfailed</html>\u{1b}\u{202e}{}",
                "é".repeat(4096)
            ),
            4096,
        );
        assert!(text.starts_with("<html>Gateway\nfailed</html>"));
        assert!(!text.contains(['\u{1b}', '\u{202e}']));
        assert!(text.len() <= 4096);
        assert!(text.ends_with("[diagnostic truncated]"));
    }

    #[test]
    fn diagnostic_wire_rejects_terminal_controls_and_oversized_text() {
        assert!(DiagnosticText::new("one\ntwo\tthree").is_ok());
        assert!(DiagnosticText::new("escape\u{1b}").is_err());
        assert!(DiagnosticText::new("bidi\u{202e}").is_err());
        assert!(DiagnosticText::new("x".repeat(4097)).is_err());
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
            let result = crate::PersistedToolResult {
                title: crate::SafeDisplayText::new("Search").unwrap(),
                output: format!(
                    "[results]\n{}\n\n[trace]\n{}\nCustom final cause",
                    "r".repeat(6000),
                    "t".repeat(6000)
                ),
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
    fn rpc_display_selects_cause_and_debug_remains_redacted() {
        let error = crate::JsonRpcError {
            code: -32603,
            message: "operation failed".into(),
            data: Some(
                serde_json::json!({"cause":"password=visible", "path":"/work/config.toml", "line":7}),
            ),
        };
        let display = rpc(&error);
        assert!(display.contains("password=visible"));
        assert!(display.contains("/work/config.toml"));
        assert!(display.contains("line: 7"));
        assert!(!format!("{error:?}").contains("password=visible"));
    }
}

//! Engine-owned retry, fallback, and protocol error policy for Oven models.

use cookie_agent_protocol::{
    ModelErrorKind, ModelErrorStage, ModelErrorSummary, SafeCode, SafeDisplayText,
};
use oven_sdk::{ErrorStage, ModelError, ModelErrorKind as OvenErrorKind};

/// The action the engine takes after one model attempt fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ErrorPolicy {
    RetryEntry,
    AdvanceEntry,
    FailRun,
}

/// Applies the migration plan's explicit error precedence.
#[must_use]
pub(crate) fn classify(error: &ModelError) -> ErrorPolicy {
    match error.kind {
        OvenErrorKind::Abort | OvenErrorKind::ContextLength | OvenErrorKind::NativeContext => {
            ErrorPolicy::FailRun
        }
        OvenErrorKind::Auth
        | OvenErrorKind::PermissionDenied
        | OvenErrorKind::InvalidRequest
        | OvenErrorKind::ModelNotFound
        | OvenErrorKind::Quota
        | OvenErrorKind::Unsupported
        | OvenErrorKind::ContentFilter
        | OvenErrorKind::InvalidToolInput
        | OvenErrorKind::Replay => ErrorPolicy::AdvanceEntry,
        OvenErrorKind::Transport
        | OvenErrorKind::Timeout
        | OvenErrorKind::RateLimited
        | OvenErrorKind::Overload
        | OvenErrorKind::UnexpectedEof => ErrorPolicy::RetryEntry,
        OvenErrorKind::InvalidResponse | OvenErrorKind::Provider | OvenErrorKind::Unknown => {
            if error.retryable {
                ErrorPolicy::RetryEntry
            } else {
                ErrorPolicy::AdvanceEntry
            }
        }
    }
}

#[must_use]
pub(crate) fn summary(error: &ModelError) -> ModelErrorSummary {
    ModelErrorSummary {
        response_body: error.diagnostics.sanitized_body.as_ref().map(|body| {
            let text = if body.truncated() {
                format!("{}\n[upstream body truncated]", body.text())
            } else {
                body.text().to_owned()
            };
            cookie_agent_protocol::diagnostics::detail(&text)
        }),
        kind: error_kind(error.kind),
        message: cookie_agent_protocol::diagnostics::headline(&error.message),
        retryable: error.retryable,
        stage: error_stage(error.diagnostics.stage),
        http_status: error.diagnostics.http_status,
        bytes_received: error.diagnostics.bytes_received,
        vendor_code: error
            .diagnostics
            .vendor_code
            .as_deref()
            .and_then(|value| SafeCode::new(normalize_code(value)).ok()),
        request_id: error.diagnostics.request_id.as_deref().and_then(|value| {
            SafeDisplayText::new(
                cookie_agent_protocol::diagnostics::sanitize(value, SafeDisplayText::MAX_BYTES)
                    .replace(['\n', '\t'], " "),
            )
            .ok()
        }),
        retry_after_ms: error
            .diagnostics
            .retry_after
            .and_then(|duration| u64::try_from(duration.as_millis()).ok()),
    }
}

fn normalize_code(value: &str) -> String {
    let value = cookie_agent_protocol::diagnostics::sanitize(value, SafeCode::MAX_BYTES);
    let mut code = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    code.truncate(SafeCode::MAX_BYTES);
    if code.starts_with(|character: char| {
        !character.is_ascii_lowercase() && !character.is_ascii_digit()
    }) {
        code.insert(0, 'x');
    }
    code
}

const fn error_kind(kind: OvenErrorKind) -> ModelErrorKind {
    match kind {
        OvenErrorKind::Transport => ModelErrorKind::Transport,
        OvenErrorKind::Timeout => ModelErrorKind::Timeout,
        OvenErrorKind::RateLimited => ModelErrorKind::RateLimited,
        OvenErrorKind::Auth => ModelErrorKind::Auth,
        OvenErrorKind::PermissionDenied => ModelErrorKind::PermissionDenied,
        OvenErrorKind::InvalidRequest => ModelErrorKind::InvalidRequest,
        OvenErrorKind::ModelNotFound => ModelErrorKind::ModelNotFound,
        OvenErrorKind::ContextLength => ModelErrorKind::ContextLength,
        OvenErrorKind::Quota => ModelErrorKind::Quota,
        OvenErrorKind::Overload => ModelErrorKind::Overload,
        OvenErrorKind::Unsupported => ModelErrorKind::Unsupported,
        OvenErrorKind::UnexpectedEof => ModelErrorKind::UnexpectedEof,
        OvenErrorKind::InvalidResponse => ModelErrorKind::InvalidResponse,
        OvenErrorKind::InvalidToolInput => ModelErrorKind::InvalidToolInput,
        OvenErrorKind::ContentFilter => ModelErrorKind::ContentFilter,
        OvenErrorKind::Replay => ModelErrorKind::Replay,
        OvenErrorKind::NativeContext => ModelErrorKind::NativeContext,
        OvenErrorKind::Provider => ModelErrorKind::Provider,
        OvenErrorKind::Abort => ModelErrorKind::Abort,
        OvenErrorKind::Unknown => ModelErrorKind::Unknown,
    }
}

const fn error_stage(stage: ErrorStage) -> ModelErrorStage {
    match stage {
        ErrorStage::Unknown => ModelErrorStage::Unknown,
        ErrorStage::RequestValidation => ModelErrorStage::RequestValidation,
        ErrorStage::RequestEncoding => ModelErrorStage::RequestEncoding,
        ErrorStage::Connect => ModelErrorStage::Connect,
        ErrorStage::ResponseHeaders => ModelErrorStage::ResponseHeaders,
        ErrorStage::ResponseBody => ModelErrorStage::ResponseBody,
        ErrorStage::StreamRead => ModelErrorStage::StreamRead,
        ErrorStage::StreamDecode => ModelErrorStage::StreamDecode,
        ErrorStage::StreamEvent => ModelErrorStage::StreamEvent,
        ErrorStage::StreamFinalize => ModelErrorStage::StreamFinalize,
        ErrorStage::ReplayEncode => ModelErrorStage::ReplayEncode,
        ErrorStage::ReplayDecode => ModelErrorStage::ReplayDecode,
        ErrorStage::NativeContextEncode => ModelErrorStage::NativeContextEncode,
        ErrorStage::NativeContextDecode => ModelErrorStage::NativeContextDecode,
        ErrorStage::Middleware => ModelErrorStage::Middleware,
    }
}

#[cfg(test)]
mod tests {
    use oven_sdk::{ModelError, ModelErrorKind};

    use super::{ErrorPolicy, classify};

    #[test]
    fn summary_preserves_provider_diagnostics_and_body() {
        let error = ModelError::invalid_request("invalid request")
            .with_http_status(400)
            .with_vendor_code("bad_parameter")
            .with_request_id("req-400")
            .with_sanitized_body(oven_sdk::SanitizedBody::new(
                r#"{"error":{"message":"unsupported temperature","api_key":"private-value"}}"#,
            ));
        let summary = super::summary(&error);
        let body = summary.response_body.as_ref().unwrap().as_str();
        assert!(body.contains("unsupported temperature"));
        assert!(body.contains("private-value"));
        assert_eq!(summary.http_status, Some(400));
        assert_eq!(summary.request_id.unwrap().as_str(), "req-400");
        assert_eq!(summary.vendor_code.unwrap().as_str(), "bad_parameter");
    }

    #[test]
    fn explicit_terminal_kinds_override_retryability() {
        let error = ModelError::new(ModelErrorKind::ContextLength, "full").with_retryable(true);
        assert_eq!(classify(&error), ErrorPolicy::FailRun);

        let error = ModelError::new(ModelErrorKind::ModelNotFound, "missing").with_retryable(true);
        assert_eq!(classify(&error), ErrorPolicy::AdvanceEntry);
    }
}

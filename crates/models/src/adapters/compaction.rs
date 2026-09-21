//! Provider-specific native compaction request options.

use oven_sdk::CompactionRequest;
use oven_sdk_azure::{AzureOpenAiCompactionOptions, AzureOpenAiCompactionRequestExt as _};
use oven_sdk_openai::{OpenAiResponsesCompactionOptions, OpenAiResponsesCompactionRequestExt as _};

/// Attach provider-native compaction instructions for adapters that support them.
/// Adapters without a native option leave the request untouched.
#[must_use]
pub fn with_native_compaction_instructions(
    request: CompactionRequest,
    adapter_id: &str,
    instructions: Option<String>,
) -> CompactionRequest {
    match adapter_id {
        "oven.openai.responses" => {
            request.with_openai_responses_compaction_options(OpenAiResponsesCompactionOptions {
                instructions,
                ..OpenAiResponsesCompactionOptions::default()
            })
        }
        "oven.azure.openai.responses" => {
            request.with_azure_openai_compaction_options(AzureOpenAiCompactionOptions {
                instructions,
                ..AzureOpenAiCompactionOptions::default()
            })
        }
        _ => request,
    }
}

#[cfg(test)]
mod tests {
    use oven_sdk::{CompactionRequest, Request};

    use super::with_native_compaction_instructions;

    fn request() -> CompactionRequest {
        CompactionRequest::new(Request::new(Vec::new()))
    }

    #[test]
    fn openai_responses_carries_native_instructions() {
        let compaction = with_native_compaction_instructions(
            request(),
            "oven.openai.responses",
            Some("focus".to_owned()),
        );
        assert_eq!(
            compaction.request.provider_options["openai"]["compaction"]["instructions"],
            serde_json::json!("focus")
        );
    }

    #[test]
    fn azure_openai_responses_carries_native_instructions() {
        let compaction = with_native_compaction_instructions(
            request(),
            "oven.azure.openai.responses",
            Some("focus".to_owned()),
        );
        assert_eq!(
            compaction.request.provider_options["azure_openai"]["compaction"]["instructions"],
            serde_json::json!("focus")
        );
    }

    #[test]
    fn adapters_without_a_native_option_leave_the_request_untouched() {
        let compaction = with_native_compaction_instructions(
            request(),
            "oven.anthropic.messages",
            Some("focus".to_owned()),
        );
        assert!(compaction.request.provider_options.is_empty());
    }
}

use super::*;
use crate::{
    anthropic::AnthropicProvider,
    openai::{OpenAiEndpoint, OpenAiProvider},
};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

fn request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: ModelId(model.to_owned()),
        messages: vec![
            ProviderMessage::System {
                content: "Be concise".into(),
            },
            ProviderMessage::User {
                content: vec![ContentPart::Text {
                    text: "hello".into(),
                }],
            },
        ],
        ..ProviderRequest::default()
    }
}

async fn events(provider: &dyn Provider, request: ProviderRequest) -> Vec<NormalizedEvent> {
    provider
        .stream(request)
        .await
        .expect("stream starts")
        .map(|event| event.expect("stream event"))
        .collect()
        .await
}

#[tokio::test]
async fn anthropic_request_headers_and_thinking_usage_are_normalized() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "event: message_start\n",
        "data: {\"message\":{\"usage\":{\"input_tokens\":0,\"cache_read_input_tokens\":12}}}\n\n",
        "event: content_block_start\n",
        "data: {\"index\":0,\"content_block\":{\"type\":\"thinking\",\"signature\":\"sig\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reason\"}}\n\n",
        "event: content_block_start\n",
        "data: {\"index\":1,\"content_block\":{\"type\":\"text\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: message_delta\n",
        "data: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let provider = AnthropicProvider::with_base_url("test-key", server.uri());
    let received = events(&provider, request("kimi-for-coding")).await;
    assert!(matches!(
        received.as_slice(),
        [
            NormalizedEvent::ReasoningDelta { .. },
            NormalizedEvent::TextDelta { .. },
            NormalizedEvent::Usage {
                input_tokens: 12,
                output_tokens: 3,
                cache_read_tokens: 12
            },
            NormalizedEvent::Stop { .. },
        ]
    ));
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0].body_json::<serde_json::Value>().expect("JSON")["stream"],
        true
    );
}

#[tokio::test]
async fn chat_completions_maps_reasoning_and_usage() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"},\"finish_reason\":null}]}\n\n",
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":1}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::with_base_url("test-key", server.uri());
    let received = events(&provider, request("deepseek-v4-flash")).await;
    assert!(
        received.iter().any(
            |event| matches!(event, NormalizedEvent::ReasoningDelta { text } if text == "think")
        )
    );
    assert!(received.iter().any(|event| matches!(
        event,
        NormalizedEvent::Usage {
            input_tokens: 7,
            output_tokens: 2,
            cache_read_tokens: 1
        }
    )));
}

#[tokio::test]
async fn responses_maps_response_events_and_ignores_hosted_tools() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"image_generation\",\"id\":\"img\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"hello\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::with_base_url("test-key", server.uri())
        .with_default_endpoint(OpenAiEndpoint::Responses);
    let received = events(&provider, request("gpt-5.6-luna")).await;
    assert!(matches!(received[0], NormalizedEvent::TextDelta { .. }));
    assert!(
        !received
            .iter()
            .any(|event| matches!(event, NormalizedEvent::ToolCallStart { .. }))
    );
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests[0].body_json::<serde_json::Value>().expect("JSON")["stream"],
        true
    );
}

#[test]
fn http_error_classification_matches_fallback_contract() {
    assert_eq!(
        ProviderError::from_http(reqwest::StatusCode::TOO_MANY_REQUESTS, "rate limited").class(),
        ProviderErrorClass::EntryRetryable
    );
    assert_eq!(
        ProviderError::from_http(reqwest::StatusCode::UNAUTHORIZED, "bad key").class(),
        ProviderErrorClass::EntryTerminal
    );
    assert_eq!(
        ProviderError::from_http(
            reqwest::StatusCode::BAD_REQUEST,
            "maximum context length exceeded"
        )
        .class(),
        ProviderErrorClass::RunTerminal
    );
}

#[derive(Default)]
struct FakeProvider {
    results: Mutex<VecDeque<Result<ProviderResponse, ProviderError>>>,
    calls: Mutex<u32>,
}

impl FakeProvider {
    fn new(results: Vec<Result<ProviderResponse, ProviderError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().expect("calls lock")
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn capabilities(&self, _model: &ModelId) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn stream(
        &self,
        _request: ProviderRequest,
    ) -> Result<
        futures_util::stream::BoxStream<'static, Result<NormalizedEvent, ProviderError>>,
        ProviderError,
    > {
        *self.calls.lock().expect("calls lock") += 1;
        match self
            .results
            .lock()
            .expect("results lock")
            .pop_front()
            .expect("configured result")
        {
            Ok(response) => Ok(stream::iter(response.events.into_iter().map(Ok)).boxed()),
            Err(error) => Err(error),
        }
    }
}

#[tokio::test]
async fn fallback_retries_advances_and_sticks_to_the_advanced_entry() {
    let primary = Arc::new(FakeProvider::new(vec![
        Err(ProviderError::EntryRetryable {
            message: "429".into(),
        }),
        Err(ProviderError::EntryRetryable {
            message: "429".into(),
        }),
        Err(ProviderError::EntryRetryable {
            message: "429".into(),
        }),
    ]));
    let fallback = Arc::new(FakeProvider::new(vec![
        Ok(ProviderResponse {
            events: vec![NormalizedEvent::TextDelta {
                text: "first".into(),
            }],
        }),
        Ok(ProviderResponse {
            events: vec![NormalizedEvent::TextDelta {
                text: "second".into(),
            }],
        }),
    ]));
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    providers.insert("primary".into(), primary.clone());
    providers.insert("fallback".into(), fallback.clone());
    let executor = FallbackExecutor::new(providers).with_retry_policy(2, Duration::ZERO);
    let chain = vec![
        ModelRef {
            provider: "primary".into(),
            model: ModelId("one".into()),
        },
        ModelRef {
            provider: "fallback".into(),
            model: ModelId("two".into()),
        },
    ];
    let mut state = FallbackRunState::default();
    let notices = Arc::new(Mutex::new(Vec::new()));
    executor
        .execute_with_state(&chain, &mut state, |model, _| request(&model.model.0), {
            let notices = notices.clone();
            move |notice| notices.lock().expect("notice lock").push(notice)
        })
        .await
        .expect("fallback response");
    executor
        .execute_with_state(
            &chain,
            &mut state,
            |model, _| request(&model.model.0),
            |_| {},
        )
        .await
        .expect("sticky response");
    assert_eq!(primary.calls(), 3);
    assert_eq!(fallback.calls(), 2);
    assert_eq!(state.entry(), 1);
    assert_eq!(notices.lock().expect("notice lock").len(), 1);
}

#[tokio::test]
async fn fallback_does_not_advance_run_terminal_errors() {
    let primary = Arc::new(FakeProvider::new(vec![Err(ProviderError::RunTerminal {
        message: "context".into(),
    })]));
    let fallback = Arc::new(FakeProvider::new(vec![Ok(ProviderResponse::default())]));
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    providers.insert("primary".into(), primary.clone());
    providers.insert("fallback".into(), fallback.clone());
    let chain = vec![
        ModelRef {
            provider: "primary".into(),
            model: ModelId("one".into()),
        },
        ModelRef {
            provider: "fallback".into(),
            model: ModelId("two".into()),
        },
    ];
    let error = FallbackExecutor::new(providers)
        .with_retry_policy(0, Duration::ZERO)
        .execute(&chain, |model, _| request(&model.model.0), |_| {})
        .await
        .expect_err("run fails");
    assert_eq!(error.class(), ProviderErrorClass::RunTerminal);
    assert_eq!(fallback.calls(), 0);
}

#[ignore = "requires COOKIECODE_TEST_* environment variables"]
#[tokio::test]
async fn live_anthropic_smoke() {
    let provider = AnthropicProvider::with_base_url(
        env("COOKIECODE_TEST_API_KEY"),
        env("COOKIECODE_TEST_BASE_URL"),
    );
    assert!(
        !events(&provider, request(&env("COOKIECODE_TEST_MODEL_ANTHROPIC")))
            .await
            .is_empty()
    );
}

#[ignore = "requires COOKIECODE_TEST_* environment variables"]
#[tokio::test]
async fn live_openai_completions_smoke() {
    let provider = OpenAiProvider::with_base_url(
        env("COOKIECODE_TEST_API_KEY"),
        env("COOKIECODE_TEST_BASE_URL"),
    );
    assert!(
        !events(
            &provider,
            request(&env("COOKIECODE_TEST_MODEL_OPENAI_COMPLETIONS"))
        )
        .await
        .is_empty()
    );
}

#[ignore = "requires COOKIECODE_TEST_* environment variables"]
#[tokio::test]
async fn live_openai_responses_smoke() {
    let provider = OpenAiProvider::with_base_url(
        env("COOKIECODE_TEST_API_KEY"),
        env("COOKIECODE_TEST_BASE_URL"),
    )
    .with_default_endpoint(OpenAiEndpoint::Responses);
    assert!(
        !events(
            &provider,
            request(&env("COOKIECODE_TEST_MODEL_OPENAI_RESPONSES"))
        )
        .await
        .is_empty()
    );
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

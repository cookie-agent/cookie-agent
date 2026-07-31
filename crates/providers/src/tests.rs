use super::*;
use crate::{
    anthropic::AnthropicProvider,
    openai::{OpenAiEndpoint, OpenAiProvider},
    openai_compatible::OpenAiCompatibleProvider,
};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde_json::json;
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

fn replay_request(model: &str, turns: Vec<PersistedTurn>) -> ProviderRequest {
    ProviderRequest {
        model: ModelId(model.to_owned()),
        persisted_turns: turns,
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
        "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reason\",\"citation\":\"kept\"}}\n\n",
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
            NormalizedEvent::TurnOpaque { .. },
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

fn opaque(events: &[NormalizedEvent]) -> AssistantTurnOpaque {
    events
        .iter()
        .find_map(|event| match event {
            NormalizedEvent::TurnOpaque { state } => Some(state.clone()),
            _ => None,
        })
        .expect("assistant opaque artifact")
}

#[tokio::test]
async fn persisted_opaque_history_is_used_in_all_production_request_builders() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "event: response.completed\ndata: {\"response\":{\"status\":\"completed\",\"usage\":{}}}\n\n",
        ))
        .mount(&server)
        .await;
    let assistant = ProviderMessage::Assistant {
        content: vec![],
        tool_calls: vec![],
    };
    let anthro_turn = PersistedTurn {
        message: assistant.clone(),
        opaque: Some(AssistantTurnOpaque {
            provider: ProviderProtocol::AnthropicMessages,
            payload: json!({"message":{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"sig"}]}}),
        }),
    };
    events(
        &AnthropicProvider::with_base_url("key", server.uri()),
        replay_request("kimi", vec![anthro_turn]),
    )
    .await;
    let chat_turn = PersistedTurn {
        message: assistant.clone(),
        opaque: Some(AssistantTurnOpaque {
            provider: ProviderProtocol::OpenAiChatCompletions,
            payload: json!({"message":{"role":"assistant","content":null,"reasoning_details":[{"type":"reasoning","id":"r"}],"tool_calls":[{"id":"call","type":"function","function":{"name":"lookup","arguments":"{\"q\":1}"}}]}}),
        }),
    };
    events(
        &OpenAiProvider::with_base_url("key", server.uri()),
        replay_request("deepseek", vec![chat_turn]),
    )
    .await;
    let response_turn = PersistedTurn {
        message: assistant.clone(),
        opaque: Some(AssistantTurnOpaque {
            provider: ProviderProtocol::OpenAiResponses,
            payload: json!({"items":[{"type":"reasoning","id":"rs","encrypted_content":"cipher"},{"type":"function_call","id":"fc","call_id":"call","name":"lookup","arguments":"{\"q\":1}"}]}),
        }),
    };
    events(
        &OpenAiProvider::with_base_url("key", server.uri())
            .with_default_endpoint(OpenAiEndpoint::Responses),
        replay_request("gpt", vec![response_turn]),
    )
    .await;
    let compatible_turn = PersistedTurn {
        message: assistant,
        opaque: Some(AssistantTurnOpaque {
            provider: ProviderProtocol::OpenAiCompatible,
            payload: json!({"message":{"role":"assistant","content":"native-compatible","tool_calls":[]}}),
        }),
    };
    events(
        &OpenAiCompatibleProvider::new("key", server.uri()),
        replay_request("compatible", vec![compatible_turn]),
    )
    .await;

    let requests = server.received_requests().await.expect("requests");
    let bodies: Vec<_> = requests
        .iter()
        .map(|request| request.body_json::<serde_json::Value>().expect("JSON"))
        .collect();
    assert_eq!(bodies[0]["messages"][0]["content"][0]["signature"], "sig");
    assert_eq!(bodies[1]["messages"][0]["reasoning_details"][0]["id"], "r");
    assert_eq!(bodies[2]["input"][0]["encrypted_content"], "cipher");
    assert_eq!(bodies[3]["messages"][0]["content"], "native-compatible");
}

#[tokio::test]
async fn anthropic_history_replays_signed_interleaved_blocks_verbatim() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "event: message_start\n",
        "data: {\"message\":{\"usage\":{\"input_tokens\":1,\"cache_read_input_tokens\":2}}}\n\n",
        "event: content_block_start\n",
        "data: {\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"sig-\"}}\n\n",
        "event: content_block_start\n",
        "data: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"lookup\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reason\",\"citation\":\"kept\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"end\"}}\n\n",
        "event: content_block_start\n",
        "data: {\"index\":2,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"ciphertext\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":\"END\"},\"usage\":{\"output_tokens\":4}}\n\n"
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let received = events(
        &AnthropicProvider::with_base_url("test-key", server.uri()),
        request("kimi-for-coding"),
    )
    .await;
    let artifact = opaque(&received);
    assert_eq!(artifact.payload["stop_sequence"], "END");
    let history = crate::anthropic::encode_history(&[
        PersistedTurn {
            message: ProviderMessage::User {
                content: vec![ContentPart::Text { text: "ask".into() }],
            },
            opaque: None,
        },
        PersistedTurn {
            message: ProviderMessage::Assistant {
                content: vec![],
                tool_calls: vec![],
            },
            opaque: Some(artifact),
        },
        PersistedTurn {
            message: ProviderMessage::Tool {
                result: ToolResult {
                    tool_call_id: "tool-1".into(),
                    content: "result".into(),
                    is_error: false,
                },
            },
            opaque: None,
        },
        PersistedTurn {
            message: ProviderMessage::User {
                content: vec![ContentPart::Text {
                    text: "continue".into(),
                }],
            },
            opaque: None,
        },
    ]);
    let blocks = history.messages[1]["content"]
        .as_array()
        .expect("content blocks");
    assert_eq!(blocks[0]["signature"], "sig-end");
    assert_eq!(blocks[0]["thinking"], "reason");
    assert_eq!(blocks[0]["citation"], "kept");
    assert_eq!(
        blocks[1],
        json!({"type":"tool_use","id":"tool-1","name":"lookup","input":{"q":"x"}})
    );
    assert_eq!(
        blocks[2],
        json!({"type":"redacted_thinking","data":"ciphertext"})
    );
    assert_eq!(history.messages[2]["content"][0]["tool_use_id"], "tool-1");
    assert_eq!(history.messages[3]["content"][0]["text"], "continue");
}

#[tokio::test]
async fn completions_history_replays_reasoning_and_parallel_calls_verbatim() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan\",\"tool_calls\":[{\"index\":0,\"id\":\"call-a\",\"type\":\"function\",\"function\":{\"name\":\"a\",\"arguments\":\"{\\\"x\\\":\"}},{\"index\":1,\"id\":\"call-b\",\"type\":\"function\",\"function\":{\"name\":\"b\",\"arguments\":\"{\\\"y\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}},{\"index\":1,\"function\":{\"arguments\":\"2}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let received = events(
        &OpenAiProvider::with_base_url("test-key", server.uri()),
        request("deepseek-v4-flash"),
    )
    .await;
    let history = crate::openai::encode_chat_history(
        &[
            PersistedTurn {
                message: ProviderMessage::Assistant {
                    content: vec![],
                    tool_calls: vec![],
                },
                opaque: Some(opaque(&received)),
            },
            PersistedTurn {
                message: ProviderMessage::Tool {
                    result: ToolResult {
                        tool_call_id: "call-a".into(),
                        content: "a-result".into(),
                        is_error: false,
                    },
                },
                opaque: None,
            },
            PersistedTurn {
                message: ProviderMessage::Tool {
                    result: ToolResult {
                        tool_call_id: "call-b".into(),
                        content: "b-result".into(),
                        is_error: false,
                    },
                },
                opaque: None,
            },
            PersistedTurn {
                message: ProviderMessage::User {
                    content: vec![ContentPart::Text {
                        text: "follow up".into(),
                    }],
                },
                opaque: None,
            },
        ],
        ProviderProtocol::OpenAiChatCompletions,
    );
    let message = &history.messages[0];
    assert_eq!(message["reasoning_content"], "plan");
    assert_eq!(message["tool_calls"][0]["id"], "call-a");
    assert_eq!(
        message["tool_calls"][0]["function"]["arguments"],
        "{\"x\":1}"
    );
    assert_eq!(message["tool_calls"][1]["id"], "call-b");
    assert_eq!(
        message["tool_calls"][1]["function"]["arguments"],
        "{\"y\":2}"
    );
    assert_eq!(history.messages[1]["tool_call_id"], "call-a");
    assert_eq!(history.messages[2]["tool_call_id"], "call-b");
    assert_eq!(history.messages[3]["content"], "follow up");
}

#[tokio::test]
async fn responses_history_replays_encrypted_reasoning_and_hosted_items() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"file_search_call\",\"id\":\"hosted_1\",\"status\":\"completed\"}}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"item_id\":\"fc_1\",\"call_id\":\"call_1\",\"delta\":\"{\\\"q\\\":1}\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc\"},{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":1}\"},{\"type\":\"file_search_call\",\"id\":\"hosted_1\",\"status\":\"completed\"}]}}\n\n"
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::with_base_url("test-key", server.uri())
        .with_default_endpoint(OpenAiEndpoint::Responses);
    let received = events(&provider, request("gpt-5.6-luna")).await;
    let history = crate::openai::encode_responses_history(&[
        PersistedTurn {
            message: ProviderMessage::Assistant {
                content: vec![],
                tool_calls: vec![],
            },
            opaque: Some(opaque(&received)),
        },
        PersistedTurn {
            message: ProviderMessage::Tool {
                result: ToolResult {
                    tool_call_id: "call_1".into(),
                    content: "answer".into(),
                    is_error: false,
                },
            },
            opaque: None,
        },
        PersistedTurn {
            message: ProviderMessage::User {
                content: vec![ContentPart::Text {
                    text: "follow up".into(),
                }],
            },
            opaque: None,
        },
    ]);
    assert_eq!(history.messages[0]["encrypted_content"], "enc");
    assert_eq!(history.messages[1]["call_id"], "call_1");
    assert_eq!(history.messages[2]["type"], "file_search_call");
    assert_eq!(history.messages[3]["call_id"], "call_1");
    assert_eq!(history.messages[4]["role"], "user");
}

#[tokio::test]
async fn in_band_sse_errors_are_terminal_for_every_format() {
    let server = MockServer::start().await;
    let error = "event: error\ndata: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad request\"}}\n\n";
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(error))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(error))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(error))
        .mount(&server)
        .await;

    let anthropic = AnthropicProvider::with_base_url("test-key", server.uri());
    let chat = OpenAiProvider::with_base_url("test-key", server.uri());
    let responses = OpenAiProvider::with_base_url("test-key", server.uri())
        .with_default_endpoint(OpenAiEndpoint::Responses);
    for provider in [&anthropic as &dyn Provider, &chat, &responses] {
        let received = provider
            .stream(request("test"))
            .await
            .expect("stream starts")
            .next()
            .await
            .expect("in-band error");
        assert_eq!(
            received.expect_err("error event").class(),
            ProviderErrorClass::EntryTerminal
        );
    }
}

#[tokio::test]
async fn in_band_provider_overload_and_nested_server_errors_are_retryable() {
    let server = MockServer::start().await;
    for (route, body) in [
        (
            "/messages",
            "event: error\ndata: {\"error\":{\"type\":\"overloaded_error\"}}\n\n",
        ),
        (
            "/chat/completions",
            "event: error\ndata: {\"error\":{\"type\":\"server_error\"}}\n\n",
        ),
        (
            "/responses",
            "event: response.failed\ndata: {\"response\":{\"error\":{\"status\":500,\"type\":\"server_error\"}}}\n\n",
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(AnthropicProvider::with_base_url("key", server.uri())),
        Box::new(OpenAiProvider::with_base_url("key", server.uri())),
        Box::new(
            OpenAiProvider::with_base_url("key", server.uri())
                .with_default_endpoint(OpenAiEndpoint::Responses),
        ),
    ];
    for provider in providers {
        let error = provider
            .stream(request("test"))
            .await
            .expect("starts")
            .next()
            .await
            .expect("in-band error")
            .expect_err("error event");
        assert_eq!(error.class(), ProviderErrorClass::EntryRetryable);
    }
}

#[test]
fn incompatible_opaque_state_is_explicitly_discarded() {
    let history = crate::openai::encode_chat_history(
        &[PersistedTurn {
            message: ProviderMessage::Assistant {
                content: vec![ContentPart::Text {
                    text: "normalized".into(),
                }],
                tool_calls: vec![],
            },
            opaque: Some(AssistantTurnOpaque {
                provider: ProviderProtocol::AnthropicMessages,
                payload: json!({"message": {"role": "assistant", "content": []}}),
            }),
        }],
        ProviderProtocol::OpenAiChatCompletions,
    );
    assert!(history.discarded_opaque);
    assert_eq!(history.messages[0]["content"], "normalized");
}

#[test]
fn every_history_encoder_discards_foreign_artifacts() {
    let turn = PersistedTurn {
        message: ProviderMessage::Assistant {
            content: vec![ContentPart::Text {
                text: "normalized".into(),
            }],
            tool_calls: vec![],
        },
        opaque: Some(AssistantTurnOpaque {
            provider: ProviderProtocol::OpenAiChatCompletions,
            payload: json!({"message":{"role":"assistant","content":"foreign"}}),
        }),
    };
    assert!(crate::anthropic::encode_history(std::slice::from_ref(&turn)).discarded_opaque);
    assert!(crate::openai::encode_responses_history(std::slice::from_ref(&turn)).discarded_opaque);
    assert!(crate::openai_compatible::encode_history(&[turn]).discarded_opaque);
}

#[test]
fn provider_error_classification_inspects_body_and_nested_sse_status() {
    for (status, body, class) in [
        (
            reqwest::StatusCode::from_u16(529).expect("529"),
            "overloaded_error",
            ProviderErrorClass::EntryRetryable,
        ),
        (
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            ProviderErrorClass::RunTerminal,
        ),
        (
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            ProviderErrorClass::EntryRetryable,
        ),
        (
            reqwest::StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            ProviderErrorClass::RunTerminal,
        ),
        (
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            ProviderErrorClass::EntryRetryable,
        ),
        (
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "insufficient_quota",
            ProviderErrorClass::EntryTerminal,
        ),
        (
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":{"code":"unknown_error"}}"#,
            ProviderErrorClass::EntryRetryable,
        ),
    ] {
        assert_eq!(ProviderError::from_http(status, body).class(), class);
    }
    assert_eq!(
        ProviderError::from_sse(
            &json!({"response":{"error":{"status":500,"type":"server_error"}}})
        )
        .class(),
        ProviderErrorClass::EntryRetryable
    );
}

#[test]
fn model_terminal_codes_override_http_status_class() {
    for code in [
        "model_not_found",
        "invalid_model",
        "model_does_not_exist",
        "model_doesnt_exist",
        "model_not_exist",
    ] {
        assert_eq!(
            ProviderError::from_http(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"code": code}}).to_string(),
            )
            .class(),
            ProviderErrorClass::EntryTerminal,
        );
    }
}

#[test]
fn normalized_responses_fallback_uses_top_level_items() {
    let history = crate::openai::encode_responses_history(&[
        PersistedTurn {
            message: ProviderMessage::Assistant {
                content: vec![ContentPart::Text {
                    text: "answer".into(),
                }],
                tool_calls: vec![ToolCall {
                    id: "call".into(),
                    name: "lookup".into(),
                    arguments: json!({"q": 1}),
                }],
            },
            opaque: None,
        },
        PersistedTurn {
            message: ProviderMessage::Tool {
                result: ToolResult {
                    tool_call_id: "call".into(),
                    content: "result".into(),
                    is_error: false,
                },
            },
            opaque: None,
        },
    ]);
    assert_eq!(history.messages[0]["type"], "message");
    assert_eq!(history.messages[0]["content"][0]["type"], "output_text");
    assert_eq!(history.messages[1]["type"], "function_call");
    assert_eq!(history.messages[2]["type"], "function_call_output");
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
async fn chat_finalizes_at_usage_or_done_and_rejects_malformed_pending_arguments() {
    let server = MockServer::start().await;
    let ordered = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ordered))
        .mount(&server)
        .await;
    let received = events(
        &OpenAiProvider::with_base_url("key", server.uri()),
        request("chat"),
    )
    .await;
    let usage = received
        .iter()
        .position(|event| matches!(event, NormalizedEvent::Usage { .. }))
        .expect("usage");
    let stop = received
        .iter()
        .position(|event| matches!(event, NormalizedEvent::Stop { .. }))
        .expect("stop");
    assert!(usage < stop);

    let malformed_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"f\",\"arguments\":\"{\"}}]}}]}\n\ndata: [DONE]\n\n",
        ))
        .mount(&malformed_server)
        .await;
    let mut stream = OpenAiProvider::with_base_url("key", malformed_server.uri())
        .stream(request("chat"))
        .await
        .expect("starts");
    let error = loop {
        match stream.next().await {
            Some(Err(error)) => break error,
            Some(Ok(_)) => {}
            None => panic!("malformed argument error"),
        }
    };
    assert_eq!(error.class(), ProviderErrorClass::EntryTerminal);
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

#[tokio::test]
async fn responses_preserve_heterogeneous_items_and_deduplicate_tool_end() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs\",\"encrypted_content\":\"keep\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"message\",\"id\":\"msg\",\"role\":\"assistant\",\"content\":[]}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"web_search_call\",\"id\":\"hosted\",\"status\":\"completed\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n",
        "event: response.function_call_arguments.done\n",
        "data: {\"item_id\":\"fc\",\"call_id\":\"call\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs\",\"encrypted_content\":\"keep\",\"summary\":[]}}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"status\":\"completed\",\"usage\":{}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let received = events(
        &OpenAiProvider::with_base_url("key", server.uri())
            .with_default_endpoint(OpenAiEndpoint::Responses),
        request("responses"),
    )
    .await;
    let state = opaque(&received);
    assert_eq!(state.payload["items"].as_array().expect("items").len(), 4);
    assert_eq!(state.payload["items"][0]["encrypted_content"], "keep");
    assert_eq!(state.payload["items"][1]["id"], "msg");
    assert_eq!(state.payload["items"][2]["id"], "hosted");
    assert_eq!(state.payload["items"][3]["id"], "fc");
    assert_eq!(
        received
            .iter()
            .filter(|event| matches!(event, NormalizedEvent::ToolCallEnd { tool_call_id } if tool_call_id == "call"))
            .count(),
        1
    );
}

#[tokio::test]
async fn chat_accumulates_fragmented_refusal_and_reasoning_details() {
    let server = MockServer::start().await;
    let fixture = concat!(
        "data: {\"choices\":[{\"delta\":{\"refusal\":\"no \",\"reasoning_details\":[{\"type\":\"reasoning\",\"text\":\"first\"}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"refusal\":\"thanks\",\"reasoning_details\":[{\"type\":\"reasoning\",\"text\":\"second\"}]},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
        .mount(&server)
        .await;
    let received = events(
        &OpenAiProvider::with_base_url("key", server.uri()),
        request("chat"),
    )
    .await;
    let message = &opaque(&received).payload["message"];
    assert_eq!(message["refusal"], "no thanks");
    assert_eq!(message["reasoning_details"][0]["text"], "first");
    assert_eq!(message["reasoning_details"][1]["text"], "second");
}

#[tokio::test]
async fn responses_preserves_reasoning_summary_indices_and_incomplete_state() {
    let server = MockServer::start().await;
    let summary_fixture = concat!(
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs\",\"summary\":[]}}\n\n",
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"item_id\":\"rs\",\"summary_index\":1,\"delta\":\"kept\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"status\":\"completed\",\"usage\":{}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(summary_fixture))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::with_base_url("key", server.uri())
        .with_default_endpoint(OpenAiEndpoint::Responses);
    let received = events(&provider, request("responses")).await;
    let state = opaque(&received);
    assert_eq!(state.payload["items"][0]["summary"][1]["text"], "kept");

    let incomplete_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "event: response.incomplete\ndata: {\"response\":{\"output\":[{\"type\":\"reasoning\",\"id\":\"rs\",\"encrypted_content\":\"partial\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":1}},\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        ))
        .mount(&incomplete_server)
        .await;
    let mut stream = OpenAiProvider::with_base_url("key", incomplete_server.uri())
        .with_default_endpoint(OpenAiEndpoint::Responses)
        .stream(request("responses"))
        .await
        .expect("starts");
    let partial = stream.next().await.expect("opaque").expect("opaque event");
    let NormalizedEvent::TurnOpaque { state } = partial else {
        panic!("opaque partial state");
    };
    assert_eq!(state.payload["items"][0]["encrypted_content"], "partial");
    assert!(matches!(
        stream.next().await,
        Some(Ok(NormalizedEvent::Usage {
            input_tokens: 3,
            output_tokens: 2,
            cache_read_tokens: 1
        }))
    ));
    assert!(matches!(
        stream.next().await,
        Some(Ok(NormalizedEvent::Stop {
            reason: StopReason::Length
        }))
    ));
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
async fn model_not_found_5xx_advances_without_retrying_the_entry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {
                "code": "model_not_found",
                "message": "The model does not exist"
            }
        })))
        .mount(&server)
        .await;
    let primary = Arc::new(OpenAiCompatibleProvider::new("key", server.uri()));
    let fallback = Arc::new(FakeProvider::new(vec![Ok(ProviderResponse::default())]));
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    providers.insert("primary".into(), primary);
    providers.insert("fallback".into(), fallback.clone());
    let chain = vec![
        ModelRef {
            provider: "primary".into(),
            model: ModelId("missing".into()),
        },
        ModelRef {
            provider: "fallback".into(),
            model: ModelId("available".into()),
        },
    ];
    let fallbacks = Arc::new(Mutex::new(Vec::new()));
    FallbackExecutor::new(providers)
        .with_retry_policy(2, Duration::ZERO)
        .execute(&chain, |model, _| request(&model.model.0), {
            let fallbacks = fallbacks.clone();
            move |fallback| fallbacks.lock().expect("fallback lock").push(fallback)
        })
        .await
        .expect("fallback succeeds");

    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    assert_eq!(fallback.calls(), 1);
    assert_eq!(fallbacks.lock().expect("fallback lock")[0].attempts, 1);
}

struct PartialFailureProvider {
    calls: Mutex<u32>,
    event: NormalizedEvent,
}

#[async_trait]
impl Provider for PartialFailureProvider {
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
        Ok(stream::iter(vec![
            Ok(self.event.clone()),
            Err(ProviderError::EntryRetryable {
                message: "dropped".into(),
            }),
        ])
        .boxed())
    }
}

#[tokio::test]
async fn fallback_does_not_retry_an_entry_after_meaningful_stream_output() {
    let primary = Arc::new(PartialFailureProvider {
        calls: Mutex::new(0),
        event: NormalizedEvent::TextDelta {
            text: "partial".into(),
        },
    });
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
    FallbackExecutor::new(providers)
        .with_retry_policy(2, Duration::ZERO)
        .execute(&chain, |model, _| request(&model.model.0), |_| {})
        .await
        .expect("fallback succeeds");
    assert_eq!(*primary.calls.lock().expect("calls lock"), 1);
    assert_eq!(fallback.calls(), 1);
}

#[tokio::test]
async fn fallback_does_not_retry_after_reasoning_only_output() {
    let primary = Arc::new(PartialFailureProvider {
        calls: Mutex::new(0),
        event: NormalizedEvent::ReasoningDelta {
            text: "partial reasoning".into(),
        },
    });
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
    FallbackExecutor::new(providers)
        .with_retry_policy(2, Duration::ZERO)
        .execute(&chain, |model, _| request(&model.model.0), |_| {})
        .await
        .expect("fallback succeeds");
    assert_eq!(*primary.calls.lock().expect("calls lock"), 1);
    assert_eq!(fallback.calls(), 1);
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

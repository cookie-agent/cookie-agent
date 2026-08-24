use std::{collections::BTreeMap, sync::Arc};

use futures_util::stream;
use http::{HeaderMap, HeaderName, HeaderValue};
use oven_sdk::{
    AbortSignal, AssistantPart, BoxFuture, Finish, FinishReason, HistoryTurn, InputPart,
    LanguageModel, LanguageModelDescriptor, ModelError, ModelId, ModelIdentity, PartMetadata,
    ProviderId, Request, StreamPart, StreamResponse, SystemPart, Usage,
};
use serde_json::{Value, json};

use crate::{
    ConstructedAdapter,
    adapters::oven::{CLIENT_USER_AGENT, ModelBuildError},
};

const ADAPTER_RECIPE_ID: &str = "oven.openai.responses";

pub(crate) fn build(
    provider_id: &str,
    model_id: &str,
    endpoint: String,
    headers: BTreeMap<String, String>,
    capabilities: oven_sdk::ModelCapabilities,
) -> Result<ConstructedAdapter, ModelBuildError> {
    let descriptor = LanguageModelDescriptor::new(
        ModelIdentity::new(
            ProviderId::new(provider_id.to_owned()),
            ModelId::new(model_id.to_owned()),
        )?,
        oven_sdk::AdapterId::new(ADAPTER_RECIPE_ID),
        capabilities,
    )?;
    let endpoint = url::Url::parse(&endpoint)
        .map_err(|_| ModelError::invalid_request("invalid no-auth Responses endpoint"))?;
    let headers = header_map(headers)?;
    let client = reqwest_oven::Client::builder()
        .user_agent(CLIENT_USER_AGENT)
        .build()
        .map_err(|_| ModelError::transport("could not construct no-auth Responses client"))?;
    Ok(ConstructedAdapter {
        model: Arc::new(NoAuthResponsesModel {
            descriptor,
            endpoint,
            headers,
            client,
        }),
        provider_options: BTreeMap::new(),
    })
}

#[derive(Clone)]
struct NoAuthResponsesModel {
    descriptor: LanguageModelDescriptor,
    endpoint: url::Url,
    headers: HeaderMap,
    client: reqwest_oven::Client,
}

impl LanguageModel for NoAuthResponsesModel {
    fn descriptor(&self) -> &LanguageModelDescriptor {
        &self.descriptor
    }

    fn validate_request(&self, request: &Request) -> Result<(), ModelError> {
        request.validate_for(&self.descriptor.capabilities)?;
        if !request.tools.is_empty() || request.native_context.is_some() {
            return Err(ModelError::unsupported(
                "no-auth OpenAI Responses supports the reviewed text-only profile",
            ));
        }
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        request: Request,
        abort: AbortSignal,
    ) -> BoxFuture<'a, Result<StreamResponse, ModelError>> {
        Box::pin(async move {
            self.validate_request(&request)?;
            if abort.is_aborted() {
                return Err(ModelError::abort("request was aborted before dispatch"));
            }
            let body = encode_request(&request, self.descriptor.identity.model_id.as_str())
                .map_err(|error| *error)?;
            let send = self
                .client
                .post(format!(
                    "{}/responses",
                    self.endpoint.as_str().trim_end_matches('/')
                ))
                .headers(self.headers.clone())
                .json(&body)
                .send();
            let response = tokio::select! {
                response = send => response.map_err(|_| ModelError::transport("no-auth OpenAI Responses request failed"))?,
                _ = abort.aborted() => return Err(ModelError::abort("request was aborted before response headers")),
            };
            if !response.status().is_success() {
                return Err(ModelError::invalid_response(format!(
                    "no-auth OpenAI Responses returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            let body = response
                .text()
                .await
                .map_err(|_| ModelError::transport("could not read no-auth Responses stream"))?;
            let parts = decode_sse(&body).map_err(|error| *error)?;
            Ok(StreamResponse::new(Box::pin(stream::iter(
                parts.into_iter().map(Ok),
            ))))
        })
    }
}

fn encode_request(request: &Request, model_id: &str) -> Result<Value, Box<ModelError>> {
    let mut input = Vec::new();
    for turn in &request.history {
        match turn {
            HistoryTurn::System(message) => {
                let text = message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        SystemPart::Text(part) => Some(part.text.as_str()),
                        SystemPart::Custom(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    input.push(json!({"type":"message","role":"developer","content":[{"type":"input_text","text":text}]}));
                }
            }
            HistoryTurn::User(message) => {
                let content = message
                    .content
                    .iter()
                    .map(|part| match part {
                        InputPart::Text(part) => Ok(json!({"type":"input_text","text":part.text})),
                        InputPart::File(_) | InputPart::Custom(_) => {
                            Err(Box::new(ModelError::unsupported(
                                "no-auth OpenAI Responses supports text input only",
                            )))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if !content.is_empty() {
                    input.push(json!({"type":"message","role":"user","content":content}));
                }
            }
            HistoryTurn::Assistant(turn) => {
                let text = turn
                    .message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantPart::Text(part) => Some(part.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if !text.is_empty() {
                    input.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}));
                }
            }
            HistoryTurn::Tool(_) => {
                return Err(Box::new(ModelError::unsupported(
                    "no-auth OpenAI Responses does not support tool history",
                )));
            }
        }
    }
    let mut body = json!({
        "model": model_id,
        "input": input,
        "stream": true,
        "store": false
    });
    if let Some(value) = request.inference.max_output_tokens {
        body["max_output_tokens"] = value.into();
    }
    if let Some(value) = request.inference.temperature {
        body["temperature"] = value.into();
    }
    if let Some(value) = request.inference.top_p {
        body["top_p"] = value.into();
    }
    Ok(body)
}

fn decode_sse(body: &str) -> Result<Vec<StreamPart>, Box<ModelError>> {
    let mut parts = vec![StreamPart::StreamStart {
        warnings: Vec::new(),
    }];
    let mut text_id = None;
    let mut completed = false;
    for event in body.replace("\r\n", "\n").split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| {
            Box::new(ModelError::invalid_response(
                "invalid no-auth Responses SSE event",
            ))
        })?;
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                let id = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .unwrap_or("response-text")
                    .to_owned();
                if text_id.is_none() {
                    parts.push(StreamPart::TextStart {
                        id: id.clone(),
                        metadata: PartMetadata::default(),
                    });
                    text_id = Some(id.clone());
                }
                parts.push(StreamPart::TextDelta {
                    id,
                    delta: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    metadata: PartMetadata::default(),
                });
            }
            "response.output_text.done" => {
                if let Some(id) = text_id.take() {
                    parts.push(StreamPart::TextEnd {
                        id,
                        metadata: PartMetadata::default(),
                    });
                }
            }
            "response.completed" => {
                if let Some(id) = text_id.take() {
                    parts.push(StreamPart::TextEnd {
                        id,
                        metadata: PartMetadata::default(),
                    });
                }
                let response = value.get("response").unwrap_or(&value);
                let usage = response.get("usage").unwrap_or(&Value::Null);
                parts.push(StreamPart::Finish {
                    finish: Finish::new(
                        Usage {
                            input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
                            output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
                            ..Usage::default()
                        },
                        FinishReason::stop(),
                    ),
                });
                completed = true;
            }
            "response.failed" | "error" => {
                return Err(Box::new(ModelError::invalid_response(
                    "no-auth OpenAI Responses reported an error",
                )));
            }
            _ => {}
        }
    }
    if completed {
        Ok(parts)
    } else {
        Err(Box::new(ModelError::unexpected_eof(
            "no-auth OpenAI Responses ended without response.completed",
        )))
    }
}

fn header_map(values: BTreeMap<String, String>) -> Result<HeaderMap, ModelBuildError> {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ModelBuildError::HeaderName(name.clone()))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| ModelBuildError::HeaderValue(name.to_string()))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

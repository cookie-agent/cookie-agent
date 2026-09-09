//! TUI adapter for the shared protocol client.

pub use cookie_agent_server::{
    ClientDelivery, ClientError, ClientProtocol, load_auth_token as read_daemon_token,
    validate_websocket_url,
};

use std::{ops::Deref, sync::Arc};

use cookie_agent_protocol::Transport;
use cookie_agent_server::{Server, WebSocketTransport, in_process_pair};

/// Display-only protocol client. Raw output is retained by the runtime, not the TUI.
#[derive(Clone)]
pub struct Client(cookie_agent_protocol::Client);

impl Client {
    pub fn connect_stream<T: Transport + 'static>(transport: T) -> Self {
        Self(cookie_agent_protocol::Client::connect_display_stream(
            transport,
        ))
    }

    pub fn connect_in_process(server: Arc<Server>) -> Self {
        let (client, service) = in_process_pair(128);
        tokio::spawn(async move {
            let _ = server.serve_stream(service).await;
        });
        Self::connect_stream(client)
    }

    pub async fn connect_websocket(url: &str) -> Result<Self, ClientError> {
        WebSocketTransport::connect(url)
            .await
            .map(Self::connect_stream)
            .map_err(|error| ClientError::WebSocket(error.to_string()))
    }

    pub async fn connect_websocket_with_token(url: &str, token: &str) -> Result<Self, ClientError> {
        WebSocketTransport::connect_with_token(url, token)
            .await
            .map(Self::connect_stream)
            .map_err(|error| ClientError::WebSocket(error.to_string()))
    }
}

impl Deref for Client {
    type Target = cookie_agent_protocol::Client;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeliveryOutcome, StateStore, ToolStatus};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use cookie_agent_protocol::{EventPayload, MessageFrame, SessionId, StoredEvent, ToolCallId};

    #[tokio::test]
    async fn sustained_raw_output_with_fast_tui_consumer() {
        sustained_raw_output(false).await;
    }

    #[tokio::test]
    async fn sustained_raw_output_with_stalled_tui_consumer() {
        sustained_raw_output(true).await;
    }

    async fn sustained_raw_output(stalled: bool) {
        let session_id = SessionId::new_v7();
        let run_id = cookie_agent_protocol::RunId::new_v7();
        let call_id = ToolCallId::new_v7();
        let owner = cookie_agent_protocol::AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: cookie_agent_protocol::ModelCallId::new("test").unwrap(),
            provider_item_id: None,
        };
        let events = [
            EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    tool_call_id: call_id,
                    owner: owner.clone(),
                    output: Default::default(),
                    presentation: cookie_agent_protocol::ToolCallPresentation {
                        title: cookie_agent_protocol::SafeDisplayText::new("Test").unwrap(),
                        primary_argument: None,
                    },
                    operation_fingerprint: serde_json::from_value(
                        serde_json::json!({"digest": "1".repeat(64)}),
                    )
                    .unwrap(),
                },
            },
            EventPayload::ToolCallProgress {
                tool_call_id: call_id,
                message: cookie_agent_protocol::SafeDisplayText::new("working").unwrap(),
                display: Some("live display".into()),
            },
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id: call_id,
                    owner,
                    outcome: cookie_agent_protocol::ToolTerminationOutcome::Completed,
                    error: None,
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        title: cookie_agent_protocol::SafeDisplayText::new("Test").unwrap(),
                        output: "authoritative preview".into(),
                        display: Some("final display".into()),
                        retained_output: None,
                        metadata: serde_json::json!({}),
                        truncation: None,
                        attachments: Vec::new(),
                        additional_messages: Vec::new(),
                    }),
                },
            },
            EventPayload::RunCompleted { final_text: None },
        ];
        let (transport, mut server) = in_process_pair(1);
        let client = Client::connect_stream(transport);
        let mut receiver = client.subscribe_deliveries().unwrap();
        let consume = move |mut receiver: tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>| async move {
            let mut store = StateStore::default();
            for seq in 1..=4 {
                let delivery = receiver.recv().await.unwrap();
                assert!(matches!(delivery, ClientDelivery::Live { .. }));
                assert_eq!(store.apply_delivery(delivery), DeliveryOutcome::Applied);
                assert_eq!(store.sessions[&session_id].last_seq, seq);
                if seq == 2 {
                    assert_eq!(
                        store.sessions[&session_id].tools[&call_id].detail,
                        "live display"
                    );
                    assert_eq!(
                        store.sessions[&session_id].tools[&call_id].status,
                        ToolStatus::Running
                    );
                }
            }
            assert!(receiver.is_empty());
            assert_eq!(
                store.sessions[&session_id].tools[&call_id].detail,
                "final display"
            );
            assert_eq!(
                store.sessions[&session_id].tools[&call_id].status,
                ToolStatus::Completed
            );
        };
        let consumer = if stalled {
            None
        } else {
            let (_, empty) = tokio::sync::mpsc::unbounded_channel();
            Some(tokio::spawn(consume(std::mem::replace(
                &mut receiver,
                empty,
            ))))
        };
        let producer = tokio::spawn(async move {
            let data = STANDARD.encode(vec![b'x'; 64 * 1024]);
            for (index, payload) in events.into_iter().enumerate() {
                if index == 2 {
                    for chunk in 0..1600 {
                        let delta = cookie_agent_protocol::OutputDelta {
                            call_id,
                            stream: cookie_agent_protocol::OutputStream::Single,
                            byte_offset: chunk * 64 * 1024,
                            data: data.clone(),
                        };
                        server.send(MessageFrame::Value(serde_json::json!({
                            "jsonrpc": "2.0", "method": "events.tool_output_delta", "params": delta
                        }))).await.unwrap();
                        if chunk % 400 == 0 {
                            server.send(MessageFrame::Value(serde_json::json!({
                                "jsonrpc": "2.0", "method": "events.tool_output_snapshot",
                                "params": {"stream": delta.stream, "snapshot": {
                                    "call_id": call_id, "start_offset": delta.byte_offset,
                                    "end_offset": delta.byte_offset + 64 * 1024, "chunks": [delta]
                                }}
                            }))).await.unwrap();
                            server.send(MessageFrame::Value(serde_json::json!({
                                "jsonrpc": "2.0", "method": "events.tool_output_gap",
                                "params": {"call_id": call_id, "stream": delta.stream, "next_offset": delta.byte_offset}
                            }))).await.unwrap();
                        }
                    }
                }
                let event = StoredEvent {
                    engine_version: None,
                    origin: None,
                    session_id,
                    run_id: Some(run_id),
                    seq: index as u64 + 1,
                    timestamp: jiff::Timestamp::now(),
                    payload,
                };
                server.send(MessageFrame::Value(serde_json::json!({
                    "jsonrpc": "2.0", "method": "events.subscription",
                    "params": cookie_agent_protocol::EventSubscriptionMessage::Event { event: Box::new(event) }
                }))).await.unwrap();
            }
            let request = server.recv().await.unwrap().unwrap();
            let MessageFrame::Value(request) = request else {
                panic!("expected request")
            };
            server
                .send(MessageFrame::Value(serde_json::json!({
                    "jsonrpc": "2.0", "id": request["id"], "result": null
                })))
                .await
                .unwrap();
        });
        // The response fences all prior deliveries without relying on sleeps.
        client
            .call::<_, serde_json::Value>("test.fence", &())
            .await
            .unwrap();
        if let Some(consumer) = consumer {
            consumer.await.unwrap();
        } else {
            assert_eq!(
                receiver.len(),
                4,
                "100 MiB raw output must not enter the TUI queue"
            );
            consume(receiver).await;
        }
        producer.await.unwrap();
        client.shutdown();
    }
}

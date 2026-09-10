use super::*;
use cookie_agent_protocol::{
    EventSubscriptionMessage, FrozenModelBinding, MessageFrame, Transport, TransportError,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct TestStream {
    incoming: mpsc::UnboundedReceiver<MessageFrame>,
    sent: mpsc::UnboundedSender<MessageFrame>,
}

#[async_trait::async_trait]
impl Transport for TestStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.sent.send(frame).map_err(|_| TransportError::Closed)
    }
    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

struct Harness {
    app: App,
    requests: mpsc::UnboundedReceiver<Value>,
    responses: mpsc::UnboundedSender<MessageFrame>,
    template: StoredEvent,
    history: Vec<StoredEvent>,
    a: FrozenModelBinding,
    b: FrozenModelBinding,
}

impl Harness {
    async fn new() -> Self {
        // Reuse a real frozen run fixture instead of inventing a second binding schema.
        let template: StoredEvent = serde_json::from_str(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../engine/tests/fixtures/events-current-delegation.jsonl"
            ))
            .lines()
            .nth(1)
            .unwrap(),
        )
        .unwrap();
        let EventPayload::RunStarted {
            selected_suffix,
            selection,
            ..
        } = &template.payload
        else {
            unreachable!()
        };
        let a = selected_suffix[0].clone();
        let b = selected_suffix[1].clone();
        let digest = cookie_agent_protocol::Sha256Digest::of_bytes(b"fallback test");
        let revision = format!("sha256:{digest}");
        let mut models = selected_suffix.iter().map(|binding| {
            serde_json::from_value::<AvailableModelDescriptor>(json!({
                "key":binding.selection.model, "display_name":binding.selection.model.to_string(),
                "capabilities":{"input":["text"],"output":["text"],"context_tokens":8192,"output_tokens":2048,"tool_calling":true,"parallel_tool_calls":true,"structured_output":false,"reasoning":false,"temperature":true,"top_p":true,"seed":false,"native_replay":"unsupported","cancellation":"local_only","media":{}},
                "variants":[],"variant_order":[],"default_variant":null,"behavior_fingerprint":digest,
            })).unwrap()
        }).collect::<Vec<_>>();
        models.sort_by(|left, right| left.key.cmp(&right.key));
        let runtime = serde_json::from_value(json!({
            "snapshot_schema_version":cookie_agent_protocol::RuntimeSnapshotSchemaVersion::current(),
            "recipe_registry_revision":revision,"catalog_revision":revision,"catalog_source":"network",
            "catalog_state":{"stale":false,"provider_quarantine_count":0,"model_quarantine_count":0,"quarantine_digest":digest,"last_error":null},
            "provider_state_revision":revision,"provider_store_generation":1,"model_revision":revision,"agent_revision":revision,"runtime_revision":revision,
            "providers":[],"models":models,"agents":[{"id":selection.agent,"preset":null,"description":"Fixture agent","mode":"primary","enabled":true,"runnable_as_root":true,"resolved_fallback":selected_suffix.iter().map(|binding| binding.selection.clone()).collect::<Vec<_>>(),"delegation_targets":[]}],
        })).unwrap();
        let (responses, incoming) = mpsc::unbounded_channel();
        let (sent, mut outgoing) = mpsc::unbounded_channel();
        let (runs, requests) = mpsc::unbounded_channel();
        let replies = responses.clone();
        tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                let request = match frame {
                    MessageFrame::Value(value) => value,
                    MessageFrame::Text(text) => serde_json::from_str(&text).unwrap(),
                };
                if request["method"] == "run.start" {
                    let _ = runs.send(request);
                } else {
                    let _ = replies.send(MessageFrame::Value(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":"fixture method unavailable"}})));
                }
            }
        });
        let client = Client::connect_stream(TestStream { incoming, sent });
        let mut app = App::new_with_config(client, false, TuiConfig::default(), Theme::default())
            .await
            .unwrap();
        app.install_initial_runtime(runtime);
        app.selected = Some(template.session_id);
        app.draft = Some(selection.clone());
        let mut harness = Self {
            app,
            requests,
            responses,
            template,
            history: vec![],
            a,
            b,
        };
        let run = cookie_agent_protocol::RunId::new_v7();
        let start = harness.started(1, run, ClientRunId::new("previous-run").unwrap());
        let committed = harness.committed(2, run);
        let completed = harness.completed(3, run);
        harness.history = vec![start, committed, completed];
        for event in harness.history.clone() {
            harness.live(event).await;
        }
        assert_eq!(
            harness.app.draft.as_ref().unwrap().model,
            harness.b.selection
        );
        harness
    }

    fn started(&self, seq: u64, run: cookie_agent_protocol::RunId, id: ClientRunId) -> StoredEvent {
        let mut event = self.template.clone();
        event.seq = seq;
        event.run_id = Some(run);
        if let EventPayload::RunStarted {
            client_run_id,
            input_through_seq,
            ..
        } = &mut event.payload
        {
            *client_run_id = id;
            *input_through_seq = seq;
        }
        event
    }

    fn committed(&self, seq: u64, run: cookie_agent_protocol::RunId) -> StoredEvent {
        let mut event = self.template.clone();
        event.seq = seq;
        event.run_id = Some(run);
        event.payload = EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: seq,
            input_through_seq: seq,
            resolved_model: cookie_agent_protocol::ResolvedModelRef {
                selection: self.b.selection.clone(),
                provider_id: self.b.selection.model.provider_id(),
                model_id: self.b.selection.model.model_id(),
                adapter_id: cookie_agent_protocol::AdaptorId::OpenaiResponses,
                selection_fingerprint: self.b.selection_fingerprint.clone(),
            },
            turn: cookie_agent_protocol::PersistedModelTurn {
                content: vec![],
                provider_options: Default::default(),
                finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
                usage: Default::default(),
                response_metadata: Default::default(),
                provider_metadata: Default::default(),
                native_replay: None,
            },
            warnings: vec![],
        };
        event
    }

    fn completed(&self, seq: u64, run: cookie_agent_protocol::RunId) -> StoredEvent {
        let mut event = self.template.clone();
        event.seq = seq;
        event.run_id = Some(run);
        event.payload = EventPayload::RunCompleted { final_text: None };
        event
    }

    async fn live(&mut self, event: StoredEvent) {
        self.app
            .handle_delivery(ClientDelivery::Live {
                message: Box::new(EventSubscriptionMessage::Event {
                    event: Box::new(event),
                }),
                generation: 0,
            })
            .await;
    }

    async fn submit(&mut self) -> (Value, RunStartParams) {
        self.app.submit_prompt("continue".into()).await;
        let request = tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
            .await
            .unwrap()
            .unwrap();
        let params = serde_json::from_value(request["params"].clone()).unwrap();
        (request, params)
    }

    async fn reply(&mut self, request: &Value, accepted: bool) {
        let frame = if accepted {
            json!({"jsonrpc":"2.0","id":request["id"],"result":{"run_id":cookie_agent_protocol::RunId::new_v7()}})
        } else {
            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"run rejected"}})
        };
        self.responses.send(MessageFrame::Value(frame)).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let update = self.app.rpc_updates_rx.recv().await.unwrap();
                let finished = matches!(update, RpcUpdate::RunStartFinished { .. });
                self.app.handle_rpc_update(update);
                if finished {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn replay_only_admission_consumes_reset_and_next_submission_uses_successful_fallback() {
    let mut h = Harness::new().await;
    h.app.set_draft_model(h.a.selection.model.clone());
    let (request, params) = h.submit().await;
    assert!(params.reset_fallback);
    let session_id = h.template.session_id;
    let run = cookie_agent_protocol::RunId::new_v7();
    let mut history = h.history.clone();
    history.extend([
        h.started(4, run, params.client_run_id),
        h.committed(5, run),
        h.completed(6, run),
    ]);
    h.app
        .handle_delivery(ClientDelivery::ReplayStart {
            session_id,
            generation: 1,
            final_seq: 6,
            rebuild: true,
        })
        .await;
    for event in history {
        let seq = event.seq;
        h.app
            .handle_delivery(ClientDelivery::ReplayEvent {
                session_id,
                generation: 1,
                final_seq: 6,
                event: Box::new(event),
            })
            .await;
        if seq < 4 {
            assert!(
                h.app.draft_reset_fallback,
                "historical matching selection is not this admission"
            );
        } else {
            assert!(!h.app.draft_reset_fallback);
        }
        assert_eq!(
            h.app.draft.as_ref().unwrap().model,
            h.a.selection,
            "wait for the replay projection swap"
        );
    }
    h.app
        .handle_delivery(ClientDelivery::ReplayEnd {
            session_id,
            generation: 1,
            final_seq: 6,
        })
        .await;
    assert_eq!(h.app.draft.as_ref().unwrap().model, h.b.selection);
    h.reply(&request, true).await; // The late RPC acknowledgement is idempotent.
    let (_, next) = h.submit().await;
    assert_eq!(next.selection.model, h.b.selection);
    assert!(!next.reset_fallback);
}

#[tokio::test]
async fn rpc_admission_consumes_reset_without_syncing_the_previous_run() {
    let mut h = Harness::new().await;
    h.app.set_draft_model(h.a.selection.model.clone());
    let (request, params) = h.submit().await;
    h.reply(&request, true).await;
    assert!(!h.app.draft_reset_fallback);
    h.app.sync_session_model_draft();
    assert_eq!(h.app.draft.as_ref().unwrap().model, h.a.selection);
    let run = cookie_agent_protocol::RunId::new_v7();
    h.live(h.started(4, run, params.client_run_id)).await;
    h.live(h.committed(5, run)).await;
    h.live(h.completed(6, run)).await;
    let (_, next) = h.submit().await;
    assert_eq!(next.selection.model, h.b.selection);
    assert!(!next.reset_fallback);
}

#[tokio::test]
async fn old_acknowledgements_cannot_consume_newer_choices_or_another_sessions_intent() {
    let mut h = Harness::new().await;
    h.app.set_draft_model(h.a.selection.model.clone());
    let (request, params) = h.submit().await;
    // A later choice can even return to the identical selection: identity alone
    // cannot determine which explicit intent an acknowledgement consumed.
    h.app.set_draft_model(h.b.selection.model.clone());
    h.app.set_draft_model(h.a.selection.model.clone());
    h.reply(&request, true).await;
    assert!(h.app.draft_reset_fallback);
    h.live(h.started(
        4,
        cookie_agent_protocol::RunId::new_v7(),
        params.client_run_id,
    ))
    .await;
    assert!(h.app.draft_reset_fallback);
    h.app.set_selected_session(SessionId::new_v7());
    h.app.set_draft_model(h.a.selection.model.clone());
    let (other_request, other_params) = h.submit().await;
    let generation = h.app.draft_generation;
    h.app.handle_rpc_update(RpcUpdate::RunStartFinished {
        session_id: h.template.session_id,
        client_run_id: other_params.client_run_id.clone(),
        draft_generation: generation,
        reset_fallback: true,
        result: Ok(()),
    });
    assert!(
        h.app.draft_reset_fallback,
        "wrong-session ACK cannot consume an intent even with matching ID/generation"
    );
    h.reply(&other_request, false).await;
    assert!(
        h.app.draft_reset_fallback,
        "rejection keeps the intent armed"
    );
    let (_, retry) = h.submit().await;
    assert!(retry.reset_fallback);
    assert_eq!(retry.selection.model, h.a.selection);
}

use std::{sync::Arc, time::Duration};

use cookie_agent_models::{
    ConfigurationFingerprint, ModelEntry, ModelSet, ModelSetError, RequestDefaults,
    ScriptedCompactionStep, ScriptedModel, ScriptedStep, ScriptedStreamItem,
};
use futures_util::StreamExt as _;
use oven_sdk::{
    AbortSignal, AdapterId, CompactionCapability, CompactionRequest, CompactionResult,
    LanguageModel, LanguageModelDescriptor, ModelCapabilities, ModelError, ModelId, ModelIdentity,
    NativeContextScope, NativeContextWindow, ProviderId, Request, ResourceId, StreamPart,
};

fn descriptor(model: &str) -> LanguageModelDescriptor {
    LanguageModelDescriptor::new(
        ModelIdentity::new(ProviderId::new("scripted"), ModelId::new(model)).unwrap(),
        AdapterId::new("cookie.scripted"),
        ModelCapabilities::conservative(),
    )
    .unwrap()
}

fn compaction_descriptor(model: &str) -> LanguageModelDescriptor {
    let mut capabilities = ModelCapabilities::conservative();
    capabilities.compaction = CompactionCapability::Native;
    LanguageModelDescriptor::new(
        ModelIdentity::new(ProviderId::new("scripted"), ModelId::new(model)).unwrap(),
        AdapterId::new("cookie.scripted"),
        capabilities,
    )
    .unwrap()
}

fn fingerprint(byte: char) -> ConfigurationFingerprint {
    ConfigurationFingerprint::new(byte.to_string().repeat(64)).unwrap()
}

fn entry(alias: &str, model: &str) -> ModelEntry {
    ModelEntry::new(
        alias,
        Arc::new(ScriptedModel::new(descriptor(model), [])),
        RequestDefaults::default(),
    )
    .unwrap()
}

#[test]
fn model_set_rejects_duplicates_and_is_immutable_after_construction() {
    let duplicate = ModelSet::new(
        [
            ("same".into(), entry("same", "one")),
            ("same".into(), entry("same", "two")),
        ],
        fingerprint('a'),
    )
    .unwrap_err();
    assert_eq!(duplicate, ModelSetError::DuplicateAlias("same".into()));

    let set = ModelSet::new([("one".into(), entry("one", "model"))], fingerprint('b')).unwrap();
    let clone = set.clone();
    assert_eq!(set.aliases().collect::<Vec<_>>(), ["one"]);
    assert_eq!(clone.aliases().collect::<Vec<_>>(), ["one"]);
    assert!(set.get("missing").is_none());
}

#[test]
fn frozen_binding_requires_the_exact_set_descriptor_and_defaults() {
    let first = ModelSet::new([("one".into(), entry("one", "model"))], fingerprint('c')).unwrap();
    let binding = first.freeze("one").unwrap();
    assert_eq!(first.resolve(&binding).unwrap().alias(), "one");

    let second = ModelSet::new([("one".into(), entry("one", "model"))], fingerprint('d')).unwrap();
    assert_eq!(
        second.resolve(&binding).unwrap_err(),
        ModelSetError::FingerprintMismatch
    );
}

#[tokio::test]
async fn scripted_model_is_fifo_captures_requests_and_reports_exhaustion() {
    let scripted = ScriptedModel::new(
        descriptor("fifo"),
        [
            ScriptedStep::stream([Ok(StreamPart::StreamStart { warnings: vec![] })]),
            ScriptedStep::error(ModelError::transport("planned")),
        ],
    );
    let first = scripted
        .stream(Request::new(vec![]), AbortSignal::default())
        .await
        .unwrap();
    assert_eq!(first.stream.collect::<Vec<_>>().await.len(), 1);
    assert_eq!(scripted.remaining(), 1);

    let error = scripted
        .stream(Request::new(vec![]), AbortSignal::default())
        .await
        .unwrap_err();
    assert_eq!(error.message, "planned");
    assert_eq!(scripted.requests().len(), 2);

    let exhausted = scripted
        .stream(Request::new(vec![]), AbortSignal::default())
        .await
        .unwrap_err();
    assert_eq!(exhausted.message, "scripted model exhausted");
}

#[tokio::test]
async fn scripted_model_honors_pre_dispatch_abort_without_consuming_a_step() {
    let scripted = ScriptedModel::new(descriptor("abort"), [ScriptedStep::stream([])]);
    let (signal, registration) = AbortSignal::new();
    registration.abort();
    let error = scripted
        .stream(Request::new(vec![]), signal)
        .await
        .unwrap_err();
    assert_eq!(error.kind, oven_sdk::ModelErrorKind::Abort);
    assert_eq!(scripted.remaining(), 1);
    assert!(scripted.requests().is_empty());
}

#[tokio::test]
async fn scripted_model_emits_mid_stream_errors_and_stops() {
    let scripted = ScriptedModel::new(
        descriptor("mid-stream-error"),
        [ScriptedStep::stream([
            Ok(StreamPart::StreamStart { warnings: vec![] }),
            Err(ModelError::transport("stream dropped")),
            Ok(StreamPart::StreamStart {
                warnings: vec!["unreachable".into()],
            }),
        ])],
    );
    let response = scripted
        .stream(Request::new(vec![]), AbortSignal::default())
        .await
        .unwrap();
    let items = response.stream.collect::<Vec<_>>().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_ok());
    assert_eq!(items[1].as_ref().unwrap_err().message, "stream dropped");
    assert_eq!(scripted.requests().len(), 1);
}

#[tokio::test]
async fn scripted_model_observes_abort_during_stream_creation_delay() {
    let scripted = ScriptedModel::new(
        descriptor("creation-abort"),
        [ScriptedStep::delayed_stream(
            Duration::from_secs(60),
            vec![],
        )],
    );
    let (signal, registration) = AbortSignal::new();
    let abort = tokio::spawn(async move {
        tokio::task::yield_now().await;
        registration.abort();
    });
    let error = scripted
        .stream(Request::new(vec![]), signal)
        .await
        .unwrap_err();
    abort.await.unwrap();
    assert_eq!(error.kind, oven_sdk::ModelErrorKind::Abort);
    assert_eq!(scripted.remaining(), 0);
    assert_eq!(scripted.requests().len(), 1);
}

#[tokio::test]
async fn scripted_model_delays_mid_stream_and_observes_abort_after_creation() {
    let scripted = ScriptedModel::new(
        descriptor("stream-abort"),
        [ScriptedStep::delayed_stream(
            Duration::ZERO,
            vec![
                ScriptedStreamItem::item(Ok(StreamPart::StreamStart { warnings: vec![] })),
                ScriptedStreamItem::Delay(Duration::from_secs(60)),
                ScriptedStreamItem::item(Ok(StreamPart::StreamStart {
                    warnings: vec!["unreachable".into()],
                })),
            ],
        )],
    );
    let (signal, registration) = AbortSignal::new();
    let mut response = scripted.stream(Request::new(vec![]), signal).await.unwrap();
    assert!(response.stream.next().await.unwrap().is_ok());
    let abort = tokio::spawn(async move {
        tokio::task::yield_now().await;
        registration.abort();
    });
    let error = response.stream.next().await.unwrap().unwrap_err();
    abort.await.unwrap();
    assert_eq!(error.kind, oven_sdk::ModelErrorKind::Abort);
    assert!(response.stream.next().await.is_none());
}

#[tokio::test]
async fn scripted_model_supports_delayed_pre_stream_errors() {
    let scripted = ScriptedModel::new(
        descriptor("delayed-error"),
        [ScriptedStep::delayed_error(
            Duration::from_millis(1),
            ModelError::timeout("planned timeout"),
        )],
    );
    let error = scripted
        .stream(Request::new(vec![]), AbortSignal::default())
        .await
        .unwrap_err();
    assert_eq!(error.message, "planned timeout");
    assert_eq!(scripted.requests().len(), 1);
}

#[test]
fn request_defaults_fill_only_unset_values_and_namespaces() {
    let defaults = RequestDefaults {
        max_output_tokens: Some(100),
        temperature: Some(0.2),
        top_p: Some(0.9),
        reasoning_effort: Some("high".into()),
        include_raw: true,
        provider_options: [("provider".into(), serde_json::json!({"default": true}))]
            .into_iter()
            .collect(),
    };
    let mut request = Request::new(vec![]);
    request.inference.temperature = Some(0.7);
    request
        .provider_options
        .insert("provider".into(), serde_json::json!({"request": true}));
    let request = defaults.apply(request);
    assert_eq!(request.inference.max_output_tokens, Some(100));
    assert_eq!(request.inference.temperature, Some(0.7));
    assert_eq!(request.inference.top_p, Some(0.9));
    assert_eq!(request.inference.reasoning_effort.as_deref(), Some("high"));
    assert!(request.stream_options.include_raw);
    assert_eq!(
        request.provider_options["provider"],
        serde_json::json!({"request": true})
    );
}

#[tokio::test]
async fn scripted_model_supports_deterministic_native_compaction_and_capture() {
    let scope = NativeContextScope::new(
        ProviderId::new("scripted"),
        ModelId::new("compact"),
        ResourceId::new("fixture").unwrap(),
    )
    .unwrap();
    let window = NativeContextWindow::new(
        AdapterId::new("cookie.scripted"),
        scope,
        serde_json::json!({"fixture": ["native", "context"]}),
    )
    .unwrap();
    let scripted = ScriptedModel::new(compaction_descriptor("compact"), []).with_compactions([
        ScriptedCompactionStep::result(CompactionResult::new(window.clone())),
    ]);
    let request = CompactionRequest::new(Request::new(vec![]));
    assert!(scripted.supports_compaction(&request));
    let result = scripted
        .compact(request.clone(), AbortSignal::default())
        .await
        .unwrap();
    assert_eq!(result.native_context, window);
    assert_eq!(scripted.compaction_requests(), [request]);
    let error = scripted
        .compact(
            CompactionRequest::new(Request::new(vec![])),
            AbortSignal::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "scripted compaction exhausted");
}

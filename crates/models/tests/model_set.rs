#![cfg(feature = "test-support")]

use cookie_agent_models::{ScriptedModel, ScriptedStep};
use futures_util::StreamExt as _;
use oven_sdk::{
    AbortSignal, AdapterId, LanguageModel as _, LanguageModelDescriptor, ModelCapabilities,
    ModelId, ModelIdentity, ProviderId, Request, StreamPart,
};

fn descriptor() -> LanguageModelDescriptor {
    let capabilities: ModelCapabilities = serde_json::from_value(serde_json::json!({
        "features": [],
        "limits": {"context": 4096, "input": null, "output": 1024},
        "modalities": {"input": ["text"], "output": ["text"]},
        "media": {"input": {}},
        "cancellation": "local_only",
        "compaction": "unsupported",
        "replay": {"policy": "never", "capability": "unsupported", "reasoning": false}
    }))
    .unwrap();
    LanguageModelDescriptor::new(
        ModelIdentity::new(ProviderId::new("test"), ModelId::new("group/model")).unwrap(),
        AdapterId::new("test.scripted"),
        capabilities,
    )
    .unwrap()
}

#[tokio::test]
async fn current_test_support_records_requests_without_a_legacy_model_set() {
    let model = ScriptedModel::new(
        descriptor(),
        [ScriptedStep::stream([Ok(StreamPart::StreamStart {
            warnings: Vec::new(),
        })])],
    );
    let mut response = model
        .stream(Request::new(Vec::new()), AbortSignal::default())
        .await
        .unwrap();
    assert!(response.stream.next().await.unwrap().is_ok());
    assert_eq!(model.requests().len(), 1);
}

use cookie_agent_models as models;
use cookie_agent_protocol as protocol;
use serde::Serialize;

fn assert_same_json(left: &impl Serialize, right: &impl Serialize) {
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap()
    );
}

#[test]
fn identical_model_value_types_match_protocol_json() {
    for (model, wire) in [
        (models::Modality::Text, protocol::Modality::Text),
        (models::Modality::Image, protocol::Modality::Image),
        (models::Modality::Audio, protocol::Modality::Audio),
        (models::Modality::Pdf, protocol::Modality::Pdf),
    ] {
        assert_same_json(&model, &wire);
    }
    for (model, wire) in [
        (models::MediaKind::Image, protocol::MediaKind::Image),
        (models::MediaKind::Audio, protocol::MediaKind::Audio),
        (models::MediaKind::Pdf, protocol::MediaKind::Pdf),
    ] {
        assert_same_json(&model, &wire);
    }
    assert_same_json(
        &models::MimeType::new("image/png").unwrap(),
        &protocol::MimeType::new("image/png").unwrap(),
    );
    for (model, wire) in [
        (
            models::ReplayCapability::Unsupported,
            protocol::ReplayCapability::Unsupported,
        ),
        (
            models::ReplayCapability::Optional,
            protocol::ReplayCapability::Optional,
        ),
        (
            models::ReplayCapability::Required,
            protocol::ReplayCapability::Required,
        ),
    ] {
        assert_same_json(&model, &wire);
    }
    for (model, wire) in [
        (
            models::CancellationCapability::LocalOnly,
            protocol::CancellationCapability::LocalOnly,
        ),
        (
            models::CancellationCapability::Provider,
            protocol::CancellationCapability::Provider,
        ),
    ] {
        assert_same_json(&model, &wire);
    }
    assert_same_json(
        &models::FiniteF32::new(0.75).unwrap(),
        &protocol::FiniteF32::new(0.75).unwrap(),
    );
    for (model, wire) in [
        (
            models::ReasoningEffort::None,
            protocol::ReasoningEffort::None,
        ),
        (
            models::ReasoningEffort::Minimal,
            protocol::ReasoningEffort::Minimal,
        ),
        (models::ReasoningEffort::Low, protocol::ReasoningEffort::Low),
        (
            models::ReasoningEffort::Medium,
            protocol::ReasoningEffort::Medium,
        ),
        (
            models::ReasoningEffort::High,
            protocol::ReasoningEffort::High,
        ),
        (
            models::ReasoningEffort::Xhigh,
            protocol::ReasoningEffort::Xhigh,
        ),
        (models::ReasoningEffort::Max, protocol::ReasoningEffort::Max),
        (
            models::ReasoningEffort::Default,
            protocol::ReasoningEffort::Default,
        ),
    ] {
        assert_same_json(&model, &wire);
    }
}

#[test]
fn digest_decoding_is_deliberately_not_equivalent() {
    let valid = serde_json::json!("a".repeat(64));
    assert_eq!(
        serde_json::from_value::<models::Sha256Digest>(valid.clone())
            .unwrap()
            .as_str(),
        serde_json::from_value::<protocol::Sha256Digest>(valid)
            .unwrap()
            .as_str()
    );

    let malformed = serde_json::json!("not-a-digest");
    assert!(serde_json::from_value::<models::Sha256Digest>(malformed.clone()).is_ok());
    assert!(serde_json::from_value::<protocol::Sha256Digest>(malformed).is_err());
}

#[test]
fn request_default_round_trips_are_deliberately_not_equivalent() {
    let model_json = serde_json::to_value(models::RequestDefaults::default()).unwrap();
    let protocol_json = serde_json::to_value(protocol::RequestDefaults::default()).unwrap();
    assert_eq!(model_json, protocol_json);
    assert!(serde_json::from_value::<models::RequestDefaults>(model_json.clone()).is_ok());
    assert!(serde_json::from_value::<protocol::RequestDefaults>(model_json).is_ok());

    let omitted_nullable_fields = serde_json::json!({"stop": []});
    assert!(
        serde_json::from_value::<models::RequestDefaults>(omitted_nullable_fields.clone()).is_ok()
    );
    assert!(serde_json::from_value::<protocol::RequestDefaults>(omitted_nullable_fields).is_err());

    let model_resolved_json =
        serde_json::to_value(models::ResolvedRequestDefaults::default()).unwrap();
    let protocol_resolved_json = serde_json::to_value(protocol::ResolvedRequestDefaults {
        request: protocol::RequestDefaults::default(),
        reasoning: None,
    })
    .unwrap();
    assert_eq!(model_resolved_json, protocol_resolved_json);
    assert!(
        serde_json::from_value::<models::ResolvedRequestDefaults>(model_resolved_json.clone())
            .is_ok()
    );
    assert!(
        serde_json::from_value::<protocol::ResolvedRequestDefaults>(model_resolved_json).is_ok()
    );

    let omitted_reasoning = serde_json::json!({
        "request": protocol_json
    });
    assert!(
        serde_json::from_value::<models::ResolvedRequestDefaults>(omitted_reasoning.clone())
            .is_ok()
    );
    assert!(
        serde_json::from_value::<protocol::ResolvedRequestDefaults>(omitted_reasoning).is_err()
    );

    let nested_omissions = serde_json::json!({
        "request": {"stop": []},
        "reasoning": null
    });
    assert!(
        serde_json::from_value::<models::ResolvedRequestDefaults>(nested_omissions.clone()).is_ok()
    );
    assert!(serde_json::from_value::<protocol::ResolvedRequestDefaults>(nested_omissions).is_err());
}

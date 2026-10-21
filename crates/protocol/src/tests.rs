use std::collections::{BTreeMap, BTreeSet};

use schemars::schema_for;
use serde_json::{Value, json};

use super::*;

fn revision<T>(value: &str) -> T
where
    T: TryFromRevision,
{
    T::from_revision(format!(
        "sha256:{}",
        Sha256Digest::of_bytes(value.as_bytes()).as_str()
    ))
}

#[test]
fn usage_event_cost_round_trips_and_is_required_but_nullable() {
    let resolved_model: ResolvedModelRef = serde_json::from_value(json!({
        "selection": {"model": "openai/gpt-5.6-sol", "variant": null},
        "provider_id": "openai",
        "model_id": "gpt-5.6-sol",
        "adapter_id": "openai-responses",
        "selection_fingerprint": "a".repeat(64),
    }))
    .unwrap();
    let payload = EventPayload::ModelUsageRecorded {
        model_turn_seq: 1,
        agent_id: AgentId::new("primary").unwrap(),
        resolved_model,
        usage: Usage::default(),
        estimated_cost_pico_usd: Some(3_100_000_000),
    };
    let encoded = serde_json::to_value(&payload).unwrap();
    assert_eq!(encoded["estimated_cost_pico_usd"], json!(3_100_000_000_u64));
    assert_eq!(
        serde_json::from_value::<EventPayload>(encoded).unwrap(),
        payload
    );

    let mut unstamped = serde_json::to_value(payload).unwrap();
    unstamped
        .as_object_mut()
        .unwrap()
        .remove("estimated_cost_pico_usd");
    assert!(serde_json::from_value::<EventPayload>(unstamped).is_err());

    let unpriced = EventPayload::ModelUsageRecorded {
        model_turn_seq: 1,
        agent_id: AgentId::new("primary").unwrap(),
        resolved_model: serde_json::from_value(json!({
            "selection": {"model": "openai/gpt-5.6-sol", "variant": null},
            "provider_id": "openai",
            "model_id": "gpt-5.6-sol",
            "adapter_id": "openai-responses",
            "selection_fingerprint": "a".repeat(64),
        }))
        .unwrap(),
        usage: Usage::default(),
        estimated_cost_pico_usd: None,
    };
    assert!(serde_json::to_value(unpriced).unwrap()["estimated_cost_pico_usd"].is_null());
}

trait TryFromRevision: Sized {
    fn from_revision(value: String) -> Self;
}

macro_rules! revision_constructor {
    ($($name:ty),+ $(,)?) => {$(
        impl TryFromRevision for $name {
            fn from_revision(value: String) -> Self { <$name>::new(value).expect("revision") }
        }
    )+};
}
revision_constructor!(
    CatalogRevision,
    RecipeRegistryRevision,
    ProviderStoreRevision,
    ProviderStateRevision,
    ModelRevision,
    ModelSnapshotRevision,
    AgentRevision,
    ConfigRevision,
    RuntimeRevision
);

fn frozen_binding() -> FrozenModelBinding {
    let selection = ModelSelection {
        model: "openai/gpt-5.6-sol".parse().unwrap(),
        variant: None,
    };
    let descriptor = oven_sdk::LanguageModelDescriptor::new(
        oven_sdk::ModelIdentity::new(
            oven_sdk::ProviderId::new("openai"),
            oven_sdk::ModelId::new("gpt-5.6-sol"),
        )
        .unwrap(),
        oven_sdk::AdapterId::new("openai-responses"),
        oven_sdk::ModelCapabilities::conservative(),
    )
    .unwrap();
    FrozenModelBinding {
        manifest_revision: revision("manifest"),
        blueprint_fingerprint: Sha256Digest::of_bytes(b"blueprint"),
        selection,
        source: FrozenProviderSource::Managed {
            provider_recipe: ProviderRecipeId::new("openai.responses.v1").unwrap(),
            source_record_digest: Sha256Digest::of_bytes(b"source"),
            recipe_fingerprint: Sha256Digest::of_bytes(b"recipe"),
            package_claim: "@ai-sdk/openai".into(),
        },
        config_override_fingerprint: Sha256Digest::of_bytes(b"override"),
        credential_binding: FrozenCredentialBinding {
            source: FrozenCredentialSource::ProviderStore,
            auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
            fields: vec![AuthFieldName::new("api_key").unwrap()],
            parameters: BTreeMap::new(),
            owned_headers: vec![HeaderName::new("authorization").unwrap()],
        },
        setup_binding: FrozenSetupBinding {
            setup_recipe: ProviderSetupRecipeId::new("empty-setup-v1").unwrap(),
            values: BTreeMap::new(),
            setup_fingerprint: Sha256Digest::of_bytes(b"setup"),
        },
        endpoint_identity: SafeEndpointIdentity::new("https://api.openai.com/v1").unwrap(),
        provider_recipe: ProviderRecipeId::new("openai.responses.v1").unwrap(),
        protocol_recipe: ProtocolRecipeId::new("oven.openai.responses").unwrap(),
        setup_recipe: ProviderSetupRecipeId::new("empty-setup-v1").unwrap(),
        compiler_version: RecipeCompilerVersion::new("family-registry").unwrap(),
        descriptor,
        defaults: FrozenResolvedRequestDefaults {
            request: FrozenRequestDefaults::default(),
            reasoning: None,
        },
        options: ProviderOptions::OpenAiResponses {
            organization: None,
            project: None,
            store: None,
        },
        static_headers: BTreeMap::new(),
        behavior_fingerprint: Sha256Digest::of_bytes(b"behavior"),
        selection_fingerprint: Sha256Digest::of_bytes(b"selection"),
    }
}

fn persisted(binding: &FrozenModelBinding) -> PersistedModelBinding {
    PersistedModelBinding::new(
        binding,
        ResolvedModelRef {
            selection: binding.selection.clone(),
            provider_id: binding.selection.model.provider_id(),
            model_id: binding.selection.model.model_id(),
            adapter_id: AdaptorId::OPENAI_RESPONSES,
            selection_fingerprint: binding.selection_fingerprint.clone(),
        },
    )
    .unwrap()
}

fn child_agent(binding: FrozenModelBinding) -> AgentSnapshot {
    AgentSnapshot {
        agent: AgentId::new("worker").unwrap(),
        mode: AgentMode::Subagent,
        description: "Delegated worker".into(),
        document_source: AgentDocumentSource::User,
        document_fingerprint: Sha256Digest::of_bytes(b"document"),
        composed_prompt: "Complete the delegated task.".into(),
        prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
        max_output_tokens: 0,
        permissions: Vec::new(),
        delegation: None,
        fallback_chain: vec![persisted(&binding)],
        selected_suffix_start: 0,
    }
}

fn runtime() -> ModelCatalogView {
    ModelCatalogView {
        revision: ViewRevision::new("view-1").unwrap(),
        catalog: CatalogStatus {
            source: CatalogSource::Network,
            stale: false,
            age: CatalogAge::Current,
            provider_quarantine_count: 0,
            model_quarantine_count: 0,
            last_error: None,
        },
        providers: Vec::new(),
        models: Vec::new(),
        agents: Vec::new(),
    }
}

#[test]
fn wire_versions_accept_only_documented_history() {
    // Builds before semver versions sent the exact protocol number.
    for legacy in [50, 49, 48, 34, 10] {
        assert!(
            serde_json::from_value::<ClientHello>(json!({
                "protocol_version": legacy,
                "client": {"kind": "tui", "instance_id": ClientInstanceId::new_v7()},
            }))
            .is_err()
        );
    }
    assert_eq!(
        serde_json::to_value(ClientHello::new(ClientIdentity::generate(
            ClientKind::new("tui").unwrap()
        )))
        .unwrap()["protocol_version"],
        json!(CLIENT_PROTOCOL_VERSION.to_string())
    );
    assert!(serde_json::from_value::<ModelSnapshotManifestSchemaVersion>(json!(2)).is_err());
}

#[test]
fn event_subscription_paging_fields_are_optional_on_the_wire() {
    let session_id = SessionId::new_v7();
    assert_eq!(
        serde_json::to_value(EventsSubscribeParams {
            session_id,
            cursor: Some(4),
            cursor_timestamp: None,
            limit: None,
        })
        .unwrap(),
        json!({"session_id": session_id, "cursor": 4})
    );
    let checked: EventsSubscribeParams = serde_json::from_value(json!({
        "session_id": session_id,
        "cursor": 4,
        "cursor_timestamp": "2026-10-06T00:00:00Z",
    }))
    .unwrap();
    assert_eq!(
        checked.cursor_timestamp,
        Some("2026-10-06T00:00:00Z".parse().unwrap())
    );
    let paged: EventsSubscribeParams =
        serde_json::from_value(json!({"session_id": session_id, "cursor": 4, "limit": 2000}))
            .unwrap();
    assert_eq!(paged.limit.map(std::num::NonZeroU32::get), Some(2000));
    assert!(
        serde_json::from_value::<EventsSubscribeParams>(
            json!({"session_id": session_id, "limit": 0})
        )
        .is_err(),
        "a page holds at least one event"
    );

    let last_page = EventsSubscribeResult {
        events: Vec::new(),
        has_more: false,
        stale_cursor: false,
    };
    assert_eq!(
        serde_json::to_value(&last_page).unwrap(),
        json!({"events": []})
    );
    assert_eq!(
        serde_json::from_value::<EventsSubscribeResult>(json!({"events": []})).unwrap(),
        last_page
    );
    assert_eq!(
        serde_json::to_value(EventsSubscribeResult {
            events: Vec::new(),
            has_more: true,
            stale_cursor: false,
        })
        .unwrap(),
        json!({"events": [], "has_more": true})
    );
    assert_eq!(
        serde_json::to_value(EventsSubscribeResult::stale()).unwrap(),
        json!({"events": [], "stale_cursor": true})
    );
    let rewound: EventSubscriptionMessage = serde_json::from_value(json!({
        "type": "rewound",
        "session_id": session_id,
        "through_seq": 41,
    }))
    .unwrap();
    assert_eq!(
        rewound,
        EventSubscriptionMessage::Rewound {
            session_id,
            through_seq: 41,
        }
    );
}

#[test]
fn video_modality_and_media_kind_round_trip_and_validate_together() {
    assert_eq!(
        serde_json::from_value::<Modality>(json!("video")).unwrap(),
        Modality::Video
    );
    assert_eq!(
        serde_json::to_value(Modality::Video).unwrap(),
        json!("video")
    );
    assert_eq!(
        serde_json::from_value::<MediaKind>(json!("video")).unwrap(),
        MediaKind::Video
    );
    assert_eq!(
        serde_json::to_value(MediaKind::Video).unwrap(),
        json!("video")
    );

    let video = MediaCapability {
        mime_types: BTreeSet::from([MimeType::new("video/mp4").unwrap()]),
        max_bytes: 25 * 1024 * 1024,
        max_count: 2,
    };
    let capabilities = ModelCapabilities {
        input: BTreeSet::from([Modality::Text, Modality::Video]),
        output: BTreeSet::from([Modality::Text]),
        context_tokens: 128_000,
        output_tokens: 8_192,
        tool_calling: true,
        parallel_tool_calls: true,
        structured_output: false,
        reasoning: true,
        temperature: true,
        top_p: true,
        seed: false,
        native_replay: ReplayCapability::Unsupported,
        cancellation: CancellationCapability::LocalOnly,
        media: BTreeMap::from([(MediaKind::Video, video)]),
    };
    assert_eq!(
        serde_json::from_value::<ModelCapabilities>(serde_json::to_value(&capabilities).unwrap())
            .unwrap(),
        capabilities
    );
}

#[test]
fn video_modality_and_media_capability_must_match() {
    let base = json!({
        "input":["text","video"],
        "output":["text"],
        "context_tokens":128000,
        "output_tokens":8192,
        "tool_calling":false,
        "parallel_tool_calls":false,
        "structured_output":false,
        "reasoning":false,
        "temperature":true,
        "top_p":true,
        "seed":false,
        "native_replay":"unsupported",
        "cancellation":"local_only",
        "media":{}
    });
    assert!(serde_json::from_value::<ModelCapabilities>(base.clone()).is_err());

    let mut media_without_modality = base;
    media_without_modality["input"] = json!(["text"]);
    media_without_modality["media"] = json!({
        "video": {
            "mime_types":["video/mp4"],
            "max_bytes":26214400,
            "max_count":2
        }
    });
    assert!(serde_json::from_value::<ModelCapabilities>(media_without_modality).is_err());
}

#[test]
fn run_selection_preset_is_optional_and_uses_the_authored_name_grammar() {
    let selection = json!({
        "agent": "primary",
        "model": {"model": "custom.test/model", "variant": null}
    });
    assert_eq!(
        serde_json::from_value::<RunSelection>(selection.clone())
            .unwrap()
            .preset,
        None
    );
    let mut valid = selection.clone();
    valid["preset"] = json!("python-3");
    assert_eq!(
        serde_json::from_value::<RunSelection>(valid)
            .unwrap()
            .preset
            .as_deref(),
        Some("python-3")
    );
    for invalid in ["", "Python", "python--3", "python_3"] {
        let mut value = selection.clone();
        value["preset"] = json!(invalid);
        assert!(serde_json::from_value::<RunSelection>(value).is_err());
    }
    let mut oversized = selection;
    oversized["preset"] = json!("a".repeat(65));
    assert!(serde_json::from_value::<RunSelection>(oversized).is_err());
}

#[test]
fn usage_rpc_shapes_are_additive_and_nullable_when_unpriced() {
    let session_id = SessionId::new_v7();
    assert_eq!(
        serde_json::to_value(SessionUsageParams { session_id }).unwrap(),
        json!({"session_id": session_id})
    );
    let result = SessionUsageResult {
        session_id,
        usage: UsageRollup {
            input_tokens: 10,
            output_tokens: 2,
            request_count: 1,
            ..UsageRollup::default()
        },
    };
    let value = serde_json::to_value(result).unwrap();
    assert_eq!(value["usage"]["estimated_cost_usd"], json!(null));
    assert_eq!(value["usage"]["request_count"], 1);
    assert!(value["usage"]["by_model"].is_object());
}

#[test]
fn session_tree_usage_result_round_trips() {
    let result = SessionTreeUsageResult {
        session_id: SessionId::new_v7(),
        usage: UsageRollup {
            input_tokens: 42,
            request_count: 2,
            ..UsageRollup::default()
        },
        session_count: 3,
    };
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["session_count"], 3);
    assert_eq!(
        serde_json::from_value::<SessionTreeUsageResult>(value).unwrap(),
        result
    );
}

#[test]
fn agent_snapshots_are_information_and_ignore_unknown_members() {
    let snapshot = child_agent(frozen_binding());
    let mut newer = serde_json::to_value(&snapshot).unwrap();
    newer["schema"] = json!(8);
    newer["future_member"] = json!({"nested": true});
    assert_eq!(
        serde_json::from_value::<AgentSnapshot>(newer).unwrap(),
        snapshot
    );
}

#[test]
fn input_lane_events_store_user_content_and_text_only_events_refuse_files() {
    let validate = |payload: EventPayload| {
        StoredEvent {
            engine_version: None,
            origin: None,
            session_id: SessionId::new_v7(),
            run_id: Some(RunId::new_v7()),
            seq: 2,
            timestamp: jiff::Timestamp::now(),
            payload,
        }
        .validate()
    };
    let artifact = |sha256: String| {
        Open::Known(UserContentPart::File {
            artifact: ArtifactRef {
                sha256,
                byte_length: 482_113,
                media_type: "image/png".into(),
                filename: Some("screenshot.png".into()),
            },
        })
    };
    let with_file = |sha256: String| {
        UserContent::new(vec![
            Open::Known(UserContentPart::Text {
                text: "look".into(),
            }),
            artifact(sha256),
        ])
        .unwrap()
    };
    let submitted = EventPayload::UserInputSubmitted {
        content: with_file("ab".repeat(32)),
    };
    assert!(validate(submitted.clone()).is_ok());
    assert_eq!(
        serde_json::to_value(&submitted).unwrap()["content"][1]["artifact"]["media_type"],
        "image/png"
    );
    assert_eq!(
        validate(EventPayload::UserInputAdmitted {
            content: with_file("AB".repeat(32)),
        }),
        Err(EventSchemaError::InvalidUserContent)
    );
    assert!(
        validate(EventPayload::MessageInjected {
            role: ExtensionMessageRole::User,
            content: with_file("ab".repeat(32)),
        })
        .is_err()
    );
    assert!(
        validate(EventPayload::MessageInjected {
            role: ExtensionMessageRole::User,
            content: "context".into(),
        })
        .is_ok()
    );
    let accepted = |body: UserContent| EventPayload::ProducerMessageAccepted {
        message_id: ProducerMessageId::new_v7(),
        producer_owner: ProducerOwner::external(
            ProducerSource::plugin("jobs").unwrap(),
            ProducerName::default_name(),
        ),
        mode: ProducerDeliveryMode::Queue,
        idempotency_key: ProducerIdempotencyKey::new("body").unwrap(),
        description: SafeDisplayText::new("Job done").unwrap(),
        body,
        reminder: None,
        agent_hop: None,
        retained_output: None,
    };
    assert!(validate(accepted("done".into())).is_ok());
    assert_eq!(
        validate(accepted(with_file("ab".repeat(32)))),
        Err(EventSchemaError::InvalidProducerMessage)
    );
}

#[test]
fn pending_input_events_and_recall_rpc_round_trip() {
    let run_id = RunId::new_v7();
    for payload in [
        EventPayload::UserInputAdmitted {
            content: "pending".into(),
        },
        EventPayload::UserInputRecalled {
            user_input_seq: 1,
            content: "pending".into(),
        },
        EventPayload::UserInputSubmitted {
            content: "pending".into(),
        },
    ] {
        let value = serde_json::to_value(&payload).expect("serialize pending-input event");
        assert_eq!(
            serde_json::from_value::<EventPayload>(value).expect("deserialize pending-input event"),
            payload
        );
    }
    let runless_admission = StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId::new_v7(),
        run_id: None,
        seq: 1,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::UserInputAdmitted {
            content: "queued steer".into(),
        },
    };
    assert!(runless_admission.validate().is_ok());
    let _ = run_id;
    let session_id = SessionId::new_v7();
    let recall = SessionRecallInputParams { session_id, seq: 4 };
    assert_eq!(
        serde_json::from_value::<SessionRecallInputParams>(
            serde_json::to_value(recall).expect("serialize recall params")
        )
        .expect("deserialize recall params"),
        recall
    );
    for (result, wire) in [
        (
            SessionRecallInputResult::Recalled {
                content: "pending".into(),
            },
            json!({"outcome": "recalled", "content": [{"type": "text", "text": "pending"}]}),
        ),
        (SessionRecallInputResult::Gone, json!({"outcome": "gone"})),
        (
            SessionRecallInputResult::NotOwner,
            json!({"outcome": "not_owner"}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&result).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<SessionRecallInputResult>(wire).unwrap(),
            result
        );
    }
    let submit = json!({
        "session_id": session_id,
        "content": [{"type": "text", "text": "after this"}],
        "delivery": "queue",
        "client_run_id": "queued-1",
    });
    let decoded = serde_json::from_value::<SessionSubmitInputParams>(submit.clone())
        .expect("selection and reset_fallback are optional");
    assert_eq!(decoded.delivery, InputDelivery::Queue);
    assert_eq!(decoded.selection, None);
    assert!(!decoded.reset_fallback);
    assert_eq!(serde_json::to_value(&decoded).unwrap(), submit);
    for (result, wire) in [
        (
            SessionSubmitInputResult::Queued { queued_seq: 9 },
            json!({"outcome": "queued", "queued_seq": 9}),
        ),
        (
            SessionSubmitInputResult::Steered {
                run_id,
                user_input_seq: 3,
            },
            json!({"outcome": "steered", "run_id": run_id, "user_input_seq": 3}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&result).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<SessionSubmitInputResult>(wire).unwrap(),
            result
        );
    }
}

#[test]
fn event_origin_grammar_accepts_only_documented_values() {
    for valid in [
        "user".to_owned(),
        "engine:model-loop".to_owned(),
        "plugin:a".to_owned(),
        "client:rpc2".to_owned(),
        format!("engine:{}", "a".repeat(64)),
    ] {
        let origin = EventOrigin::new(valid.clone()).expect("valid event origin");
        assert_eq!(origin.as_str(), valid);
    }
    for invalid in [
        "",
        "User",
        "engine",
        "other:value",
        "engine:",
        "engine:-bad",
        "engine:Bad",
        "engine:bad_slug",
        "engine:bad:slug",
    ] {
        assert_eq!(
            EventOrigin::new(invalid),
            Err(EventSchemaError::InvalidOrigin)
        );
    }
    assert_eq!(
        EventOrigin::new(format!("client:{}", "a".repeat(65))),
        Err(EventSchemaError::InvalidOrigin)
    );
    assert_eq!(
        EventOrigin::new("plugin:fixture").unwrap().plugin_name(),
        Some("fixture")
    );
    assert_eq!(EventOrigin::new("user").unwrap().plugin_name(), None);
    assert_eq!(
        EventOrigin::new("engine:tool-result").unwrap().as_str(),
        "engine:tool-result"
    );
}

fn tool_result_with_additional_messages(additional_messages: Value) -> Value {
    json!({
        "title": "Tool result",
        "output": "done",
        "metadata": null,
        "attachments": [],
        "additional_messages": additional_messages,
    })
}

fn display_result(output: &str, display: Vec<Open<DisplayBlock>>) -> PersistedToolResult {
    PersistedToolResult {
        title: SafeDisplayText::new("Bash").unwrap(),
        output: output.into(),
        display,
        background: None,
        retained_output: None,
        metadata: Value::Null,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    }
}

#[test]
fn tool_result_display_found_in_output_is_stored_as_a_span() {
    let text = |text: &str| vec![Open::Known(DisplayBlock::text(text))];
    let cut = vec![Open::Known(DisplayBlock::Text {
        text: "same".into(),
        omitted_lines: Some(1),
    })];
    let two = [text("same"), text("text")].concat();
    let cases = [
        // A single-output tool shows its whole output.
        (
            "same text",
            text("same text"),
            json!({"display_span": [0, 9]}),
        ),
        // A shell shows its only stream, past the stream heading.
        (
            "[stdout]\né ok\n\n[stderr]\n",
            text("é ok\n"),
            json!({"display_span": [9, 15]}),
        ),
        (
            "[stdout]\nab\n",
            text("merged"),
            json!({"display": [{"type": "text", "text": "merged"}]}),
        ),
        // Only a whole single text block is a span; short texts in a list
        // stay inline.
        (
            "same text",
            cut.clone(),
            json!({"display": [{"type": "text", "text": "same", "omitted_lines": 1}]}),
        ),
        (
            "same text",
            two.clone(),
            json!({"display": [{"type": "text", "text": "same"}, {"type": "text", "text": "text"}]}),
        ),
        (
            "output",
            text(""),
            json!({"display": [{"type": "text", "text": ""}]}),
        ),
        ("output", Vec::new(), json!({"display": []})),
    ];
    for (output, display, expected) in cases {
        let result = display_result(output, display);
        let value = write_stored_history(|| serde_json::to_value(&result)).unwrap();
        for field in ["display", "display_span"] {
            assert_eq!(value.get(field), expected.get(field), "{output:?} {field}");
        }
        assert_eq!(
            serde_json::from_value::<PersistedToolResult>(value).unwrap(),
            result
        );
        // Anywhere but the session log, the display is whole.
        let wire = serde_json::to_value(&result).unwrap();
        assert!(wire.get("display_span").is_none());
        assert_eq!(
            wire["display"],
            serde_json::to_value(&result.display).unwrap()
        );
    }
}

#[test]
fn a_stored_display_keeps_text_that_repeats_the_output_as_ranges_of_it() {
    let stdout =
        "compiling cookie-agent v0.5.0 (crates/agent)\nfinished in 3.2 seconds, all good\n";
    let first = stdout.find('\n').unwrap() + 1;
    let stderr = "warning: \u{1b}[33munused variable\u{1b}[0m in two places\n";
    let lines = [
        "fn main() {",
        "    println!(\"a line long enough to be a span\");",
        "}",
    ];
    let output = format!(
        "[stdout]\n{stdout}\n[stderr]\n{stderr}\n41: {}\n42: {}\n43: {}\n",
        lines[0], lines[1], lines[2]
    );
    let summary = "a summary that the output repeats verbatim";
    let output = format!("{output}{summary}");
    let segment = |stream, text: &str| TerminalSegment {
        stream,
        text: text.into(),
    };
    let display = vec![
        Open::Known(DisplayBlock::Terminal {
            segments: vec![
                segment(TerminalStream::Stdout, &stdout[..first]),
                segment(TerminalStream::Stderr, stderr),
                segment(TerminalStream::Stdout, &stdout[first..]),
                segment(TerminalStream::Stdout, "short"),
                segment(TerminalStream::Stdout, "a segment the output does not hold"),
            ],
            exit_code: Some(0),
            omitted_lines: None,
            full: None,
        }),
        Open::Known(DisplayBlock::FileExcerpt {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            start_line: 41,
            lines: lines.map(str::to_owned).to_vec(),
            total_lines: Some(43),
            omitted_lines: None,
            full: None,
        }),
        // Numbered from another line, the lines are not in the output.
        Open::Known(DisplayBlock::FileExcerpt {
            path: "src/main.rs".into(),
            language: None,
            start_line: 40,
            lines: lines.map(str::to_owned).to_vec(),
            total_lines: None,
            omitted_lines: None,
            full: None,
        }),
        Open::Known(DisplayBlock::markdown(summary)),
        Open::Known(DisplayBlock::code(summary, Some("text".into()))),
        Open::Unknown {
            tag: "chart".into(),
            raw: json!({"type": "chart", "text": summary}),
        },
    ];
    let result = display_result(&output, display);
    let stored = write_stored_history(|| serde_json::to_value(&result)).unwrap();
    let span = |text: &str| {
        let start = output.find(text).unwrap();
        json!([start, start + text.len()])
    };
    let blocks = &stored["display"];
    let segments = &blocks[0]["segments"];
    assert_eq!(
        segments[0],
        json!({"stream": "stdout", "text_span": span(&stdout[..first])})
    );
    assert_eq!(
        segments[1],
        json!({"stream": "stderr", "text_span": span(stderr)})
    );
    let rest = output.find(&stdout[first..]).unwrap();
    assert_eq!(
        segments[2],
        json!({"stream": "stdout", "text_span": [rest, rest + stdout.len() - first]})
    );
    assert_eq!(segments[3]["text"], "short");
    assert_eq!(segments[4]["text"], "a segment the output does not hold");
    assert_eq!(blocks[0]["exit_code"], 0);
    let numbered = format!("41: {}\n42: {}\n43: {}", lines[0], lines[1], lines[2]);
    assert_eq!(blocks[1]["lines_span"], span(&numbered));
    assert!(blocks[1].get("lines").is_none());
    assert!(blocks[2].get("lines_span").is_none());
    assert_eq!(blocks[3]["text_span"], span(summary));
    assert_eq!(blocks[4]["text_span"], span(summary));
    assert_eq!(blocks[4]["language"], "text");
    assert_eq!(blocks[5], json!({"type": "chart", "text": summary}));
    assert_eq!(
        serde_json::from_value::<PersistedToolResult>(stored.clone()).unwrap(),
        result
    );

    for (path, member, broken) in [
        ("/display/0/segments/0", "text_span", json!([0, 100_000])),
        ("/display/0/segments/1", "text", json!("both")),
        ("/display/1", "lines_span", span(summary)),
        ("/display/1", "start_line", json!(40)),
        ("/display/3", "text_span", json!([1])),
    ] {
        let mut stored = stored.clone();
        stored.pointer_mut(path).unwrap()[member] = broken;
        assert!(
            serde_json::from_value::<PersistedToolResult>(stored).is_err(),
            "{path} {member}"
        );
    }
}

#[test]
fn a_backgrounded_result_carries_a_typed_open_state() {
    let mut result = display_result("Started in the background as job job_1.", Vec::new());
    result.background = Some(ToolBackground {
        state: ToolBackgroundState::Job,
        by: BackgroundActor::User,
        job: Some("job_1".into()),
    });
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(
        value["background"],
        json!({"state": "job", "by": "user", "job": "job_1"})
    );
    assert_eq!(
        serde_json::from_value::<PersistedToolResult>(value.clone()).unwrap(),
        result
    );
    let mut future = value.clone();
    future["background"] = json!({"state": "paused", "by": "model", "job": "job_1"});
    assert_eq!(
        serde_json::from_value::<PersistedToolResult>(future)
            .unwrap()
            .background
            .map(|background| background.state),
        Some(ToolBackgroundState::Unknown)
    );
    for invalid in [
        json!({"state": "job", "by": "model"}),
        json!({"state": "self_reporting", "by": "model", "job": "job_1"}),
        json!({"state": "job", "by": "model", "job": ""}),
    ] {
        let mut value = value.clone();
        value["background"] = invalid.clone();
        assert!(
            serde_json::from_value::<PersistedToolResult>(value).is_err(),
            "{invalid}"
        );
    }
    let mut foreground = value;
    foreground.as_object_mut().unwrap().remove("background");
    assert_eq!(
        serde_json::from_value::<PersistedToolResult>(foreground)
            .unwrap()
            .background,
        None
    );
}

#[test]
fn tool_result_display_span_must_be_a_range_of_output() {
    let mut value = serde_json::to_value(display_result("é ok", Vec::new())).unwrap();
    value.as_object_mut().unwrap().remove("display");
    assert!(serde_json::from_value::<PersistedToolResult>(value.clone()).is_err());
    for span in [json!([0, 9]), json!([1, 3]), json!([3, 2])] {
        value["display_span"] = span.clone();
        assert!(
            serde_json::from_value::<PersistedToolResult>(value.clone()).is_err(),
            "{span}"
        );
    }
    value["display_span"] = json!([0, 2]);
    assert_eq!(
        serde_json::from_value::<PersistedToolResult>(value.clone())
            .unwrap()
            .display,
        vec![Open::Known(DisplayBlock::text("é"))]
    );
    value["display"] = json!([{"type": "text", "text": "é"}]);
    assert!(serde_json::from_value::<PersistedToolResult>(value).is_err());
}

#[test]
fn tool_emitted_messages_round_trip_and_empty_results_stay_compact() {
    let digest = Sha256Digest::of_bytes(b"emitted file");
    let result = PersistedToolResult {
        display: Vec::new(),
        background: None,
        retained_output: None,
        title: SafeDisplayText::new("Tool result").unwrap(),
        output: "done".into(),
        metadata: Value::Null,
        attachments: vec![ToolAttachment {
            mime_type: MimeType::new("text/plain").unwrap(),
            filename: Some("result.txt".into()),
            byte_length: 7,
            sha256: Sha256Digest::of_bytes(b"result"),
            reference: ArtifactReference {
                uri: "artifact://sha256/result".into(),
            },
        }],
        additional_messages: vec![
            ToolEmittedMessage::new(
                ToolEmittedMessageRole::System,
                vec![ToolEmittedContent::Text("Use the emitted file.".into())],
            )
            .unwrap(),
            ToolEmittedMessage::new(
                ToolEmittedMessageRole::User,
                vec![ToolEmittedContent::File(ToolAttachment {
                    mime_type: MimeType::new("text/plain").unwrap(),
                    filename: Some("emitted.txt".into()),
                    byte_length: 12,
                    sha256: digest.clone(),
                    reference: ArtifactReference {
                        uri: format!("artifact://sha256/{digest}"),
                    },
                })],
            )
            .unwrap(),
        ],
    };

    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["additional_messages"][0]["role"], "system");
    assert_eq!(
        wire["additional_messages"][0]["content"][0],
        json!({"text": "Use the emitted file."})
    );
    assert_eq!(
        wire["additional_messages"][1]["content"][0]["file"]["byte_length"],
        12
    );
    let round_trip = serde_json::from_value::<PersistedToolResult>(wire).unwrap();
    assert_eq!(round_trip, result);
    assert_eq!(
        round_trip
            .all_attachments()
            .map(|attachment| attachment.byte_length)
            .sum::<u64>(),
        19
    );

    let mut compact = result;
    compact.additional_messages.clear();
    let compact_wire = serde_json::to_value(&compact).unwrap();
    assert!(compact_wire.get("additional_messages").is_none());
    assert!(
        serde_json::from_value::<PersistedToolResult>(compact_wire)
            .unwrap()
            .additional_messages
            .is_empty()
    );
}

#[test]
fn tool_emitted_messages_reject_invalid_roles_content_and_counts() {
    for additional_messages in [
        json!([{"role":"assistant","content":[{"text":"not allowed"}]}]),
        json!([{"role":"user","content":[]}]),
        json!([{"role":"user","content":[{"text":" \n\t"}]}]),
        json!([{"role":"system","content":[{"text":"x".repeat(64 * 1024 + 1)}]}]),
        json!([{"role":"system","content":[
            {"text":"x".repeat(32 * 1024 + 1)},
            {"text":"x".repeat(32 * 1024)},
        ]}]),
        json!([{"role":"user","content":[{"text":"message"}],"future":true}]),
        json!([{"role":"user","content":[{"text":"message","future":true}]}]),
        json!(
            (0..5)
                .map(|_| json!({"role":"user","content":[{"text":"message"}]}))
                .collect::<Vec<_>>()
        ),
    ] {
        assert!(
            serde_json::from_value::<PersistedToolResult>(tool_result_with_additional_messages(
                additional_messages
            ))
            .is_err()
        );
    }

    assert!(ToolEmittedMessage::new(ToolEmittedMessageRole::User, Vec::new()).is_err());
    assert!(
        ToolEmittedMessage::new(
            ToolEmittedMessageRole::System,
            vec![ToolEmittedContent::Text(" ".into())],
        )
        .is_err()
    );
}

#[test]
fn tool_emitted_files_share_the_result_attachment_count_limit() {
    let attachment = json!({
        "mime_type": "text/plain",
        "filename": null,
        "byte_length": 1,
        "sha256": Sha256Digest::of_bytes(b"x"),
        "reference": {"uri": "artifact://sha256/x"},
    });
    let mut result = tool_result_with_additional_messages(json!([{
        "role": "user",
        "content": [{"file": attachment.clone()}, {"file": attachment.clone()}],
    }]));
    result["attachments"] = Value::Array(vec![attachment; 255]);
    assert!(serde_json::from_value::<PersistedToolResult>(result).is_err());
}

#[test]
fn stored_event_origin_is_additive_and_not_payload_degraded() {
    let base = json!({
        "session_id": SessionId::new_v7(),
        "run_id": null,
        "seq": 1,
        "timestamp": jiff::Timestamp::now(),
        "payload": {"type":"user_input_admitted","content":[{"type":"text","text":"legacy"}]}
    });
    let legacy = serde_json::from_value::<StoredEvent>(base.clone()).expect("pre-origin event");
    assert_eq!(legacy.origin, None);
    let serialized = serde_json::to_value(&legacy).expect("serialize legacy event");
    assert!(serialized.get("origin").is_none());
    assert_eq!(
        serde_json::from_value::<StoredEvent>(serialized).expect("round trip legacy event"),
        legacy
    );

    // Stored events are information: a newer build's envelope members are
    // ignored.
    let mut unknown = base.clone();
    unknown["future_envelope_field"] = json!(true);
    assert_eq!(
        serde_json::from_value::<StoredEvent>(unknown).expect("unknown envelope member"),
        legacy
    );

    let mut invalid_origin = base;
    invalid_origin["origin"] = json!("engine:Bad");
    invalid_origin["payload"] =
        json!({"type":"delegate_child_terminated","status":"failed","reason":42});
    let error = serde_json::from_value::<StoredEvent>(invalid_origin)
        .expect_err("invalid origin must fail before payload degradation");
    assert!(error.to_string().contains("event origin is invalid"));
}

#[test]
fn agent_md_event_round_trips_and_enforces_entry_bounds() {
    let event = StoredEvent {
        engine_version: None,
        origin: Some(EventOrigin::new("engine:agent-md").unwrap()),
        session_id: SessionId::new_v7(),
        run_id: Some(RunId::new_v7()),
        seq: 1,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::AgentMdLoaded {
            attachment: crate::AttachmentName::new("local").unwrap(),
            entries: vec![AgentMdEntry {
                source: SafeDisplayText::new("/workspace/AGENTS.md").unwrap(),
                content: "project rules".into(),
                byte_length: 13,
            }],
        },
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(serde_json::from_value::<StoredEvent>(value).unwrap(), event);

    let invalid = StoredEvent {
        payload: EventPayload::AgentMdLoaded {
            attachment: crate::AttachmentName::new("local").unwrap(),
            entries: Vec::new(),
        },
        ..event
    };
    assert_eq!(invalid.validate(), Err(EventSchemaError::InvalidAgentMd));
}

#[test]
fn memory_loaded_round_trips_and_checks_its_shape() {
    let agent = MemorySnapshotAgent {
        agent: crate::AgentId::new("reviewer").unwrap(),
        memory: Some(MemorySnapshotMemory {
            description: "Reviews".into(),
            keywords: vec!["review".into()],
            body: "- bugs first\n".into(),
        }),
        omitted: false,
    };
    let root = MemorySnapshot {
        delegated: false,
        writable: true,
        user: vec![MemorySnapshotCategory {
            category: "profile".into(),
            description: "Who the user is".into(),
            keywords: Vec::new(),
            body: String::new(),
        }],
        user_omitted: vec!["references".into()],
        agent: Some(agent.clone()),
        machines: vec![MemorySnapshotMachine {
            machine: crate::MachineName::new("labpc").unwrap(),
            attachments: vec![crate::AttachmentName::new("labpc").unwrap()],
            memories: vec![MemorySnapshotIndexEntry {
                url: "machine://labpc/scratch".into(),
                description: "Use /scratch".into(),
                keywords: Vec::new(),
            }],
            omitted: 2,
        }],
    };
    let event = |snapshot: MemorySnapshot| StoredEvent {
        engine_version: None,
        origin: Some(EventOrigin::new("engine:memory").unwrap()),
        session_id: SessionId::new_v7(),
        run_id: Some(RunId::new_v7()),
        seq: 1,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::MemoryLoaded { snapshot },
    };
    let stored = event(root.clone());
    assert!(!stored.payload.is_audit());
    assert_eq!(stored.validate(), Ok(()));
    let value = serde_json::to_value(&stored).unwrap();
    assert_eq!(value["payload"]["type"], "memory_loaded");
    assert_eq!(value["payload"]["machines"][0]["omitted"], 2);
    assert_eq!(
        serde_json::from_value::<StoredEvent>(value).unwrap(),
        stored
    );

    // The empty snapshot is the bare event.
    let empty = event(MemorySnapshot::unavailable());
    assert_eq!(empty.validate(), Ok(()));
    assert_eq!(
        serde_json::to_value(&empty.payload).unwrap(),
        json!({"type": "memory_loaded"})
    );

    let invalid = |snapshot: MemorySnapshot| {
        assert_eq!(
            event(snapshot).validate(),
            Err(EventSchemaError::InvalidMemorySnapshot)
        );
    };
    // An empty snapshot holds nothing else.
    invalid(MemorySnapshot {
        agent: None,
        ..root.clone()
    });
    invalid(MemorySnapshot {
        writable: true,
        ..MemorySnapshot::unavailable()
    });
    // A delegated snapshot holds only its agent.
    invalid(MemorySnapshot {
        delegated: true,
        ..root.clone()
    });
    assert_eq!(
        event(MemorySnapshot {
            delegated: true,
            agent: Some(agent.clone()),
            ..MemorySnapshot::default()
        })
        .validate(),
        Ok(())
    );
    // An omitted agent file carries no memory.
    invalid(MemorySnapshot {
        agent: Some(MemorySnapshotAgent {
            omitted: true,
            ..agent
        }),
        ..root.clone()
    });
    // Blank names and descriptions are refused.
    let mut blank = root.clone();
    blank.user[0].description = " ".into();
    invalid(blank);
    // The writer caps the stored size.
    let mut oversized = root;
    oversized.user[0].body = "x".repeat(MemorySnapshot::MAX_JSON_BYTES);
    invalid(oversized);
}

#[test]
fn diagnostics_round_trip_and_bound_their_fields() {
    let diagnostic = |count: Option<u64>, message: &str, detail: Option<Value>| StoredEvent {
        engine_version: None,
        origin: Some(EventOrigin::new("engine:agent-md").unwrap()),
        session_id: SessionId::new_v7(),
        run_id: Some(RunId::new_v7()),
        seq: 2,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::Diagnostic {
            source: DiagnosticSource::engine("agent-md").unwrap(),
            kind: EngineDiagnosticKind::AgentMdTooLarge.label(),
            message: message.into(),
            count,
            detail,
        },
    };
    let event = diagnostic(
        Some(2),
        "AGENTS.md skipped",
        Some(
            json!({"path": "/workspace/AGENTS.md", "byte_length": 3_000_000, "attachment": "labpc_api"}),
        ),
    );
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["payload"]["kind"], "agent_md_too_large");
    assert_eq!(value["payload"]["detail"]["attachment"], "labpc_api");
    assert_eq!(serde_json::from_value::<StoredEvent>(value).unwrap(), event);
    let one = serde_json::to_value(diagnostic(None, "once", None)).unwrap();
    assert!(one["payload"].get("count").is_none() && one["payload"].get("detail").is_none());

    for invalid in [
        diagnostic(Some(0), "zero", None),
        diagnostic(None, " ", None),
        diagnostic(
            None,
            "large",
            Some(json!("x".repeat(MAX_DIAGNOSTIC_DETAIL_BYTES))),
        ),
    ] {
        assert_eq!(invalid.validate(), Err(EventSchemaError::InvalidDiagnostic));
    }
    for source in ["engine:", "client:tui", "plugin", "Engine:x"] {
        assert!(DiagnosticSource::new(source).is_err(), "{source}");
    }
    for kind in ["", "Hook", "1st", "a-b", &"a".repeat(65)] {
        assert!(DiagnosticKind::new(kind).is_err(), "{kind}");
    }
    assert_eq!(
        DiagnosticSource::from(&crate::ProducerSource::runtime("labpc").unwrap()).runtime_id(),
        Some("labpc")
    );
}

#[test]
fn model_tools_event_round_trips_and_bounds_names() {
    let event = StoredEvent {
        engine_version: None,
        origin: Some(EventOrigin::new("engine:model-loop").unwrap()),
        session_id: SessionId::new_v7(),
        run_id: Some(RunId::new_v7()),
        seq: 1,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::ModelToolsPublished {
            attempt_id: AttemptId::new_v7(),
            tool_names: vec!["bash".into(), "read".into()],
        },
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["payload"]["type"], "model_tools_published");
    assert_eq!(serde_json::from_value::<StoredEvent>(value).unwrap(), event);
    assert_eq!(event.validate(), Ok(()));

    for tool_names in [
        vec![String::new()],
        vec!["bad\nname".into()],
        vec!["x".repeat(MAX_MODEL_TOOL_NAME_BYTES + 1)],
        vec!["tool".into(); MAX_MODEL_TOOL_NAMES + 1],
    ] {
        let invalid = StoredEvent {
            payload: EventPayload::ModelToolsPublished {
                attempt_id: AttemptId::new_v7(),
                tool_names,
            },
            ..event.clone()
        };
        assert_eq!(invalid.validate(), Err(EventSchemaError::InvalidModelTools));
    }
    assert!(valid_model_tool_names(&[]));
}

#[test]
fn event_payload_best_effort_defaults_only_optional_fields() {
    let missing = deserialize_event_payload_best_effort(
        &json!({"type":"delegate_child_terminated","status":"failed"}),
    )
    .expect("missing optional field defaults");
    assert_eq!(missing.degraded_fields, ["reason"]);
    assert_eq!(
        missing.payload,
        EventPayload::DelegateChildTerminated {
            status: SessionStatus::Failed,
            reason: None
        }
    );

    let mismatched = deserialize_event_payload_best_effort(&json!({
        "type":"delegate_child_terminated",
        "status":"cancelled",
        "reason": 42
    }))
    .expect("mismatched optional field defaults");
    assert_eq!(mismatched.degraded_fields, ["reason"]);
    assert_eq!(
        mismatched.payload,
        EventPayload::DelegateChildTerminated {
            status: SessionStatus::Cancelled,
            reason: None
        }
    );

    assert!(deserialize_event_payload_best_effort(&json!({"type":"future_event"})).is_err());
    assert!(deserialize_event_payload_best_effort(&json!({"type":"run_finished"})).is_err());
    assert!(
        deserialize_event_payload_best_effort(
            &json!({"type":"run_finished","outcome":{"type":"failed","error":42}})
        )
        .is_err()
    );
    // An outcome from a newer build is kept, not an error.
    let future = deserialize_event_payload_best_effort(
        &json!({"type":"run_finished","outcome":{"type":"paused","until":"later"}}),
    )
    .expect("an unknown outcome reads");
    let EventPayload::RunFinished { outcome } = future.payload else {
        panic!("run_finished");
    };
    assert_eq!(outcome.tag(), "paused");
    assert_eq!(outcome.status(), SessionStatus::Interrupted);

    let tool_call_id = ToolCallId::new_v7();
    let old_progress = deserialize_event_payload_best_effort(&json!({
        "type": "tool_call_progress",
        "tool_call_id": tool_call_id,
        "message": "working"
    }))
    .expect("progress from before output chunks remains readable");
    assert!(old_progress.degraded_fields.is_empty());
    assert_eq!(
        old_progress.payload,
        EventPayload::ToolCallProgress {
            tool_call_id,
            message: SafeDisplayText::new("working").expect("message"),
            display: None,
        }
    );

    let chunk_progress = deserialize_event_payload_best_effort(&json!({
        "type": "tool_call_progress",
        "tool_call_id": tool_call_id,
        "message": "bash stdout",
        "display": {"type": "append", "index": 0, "text": "partial output"}
    }))
    .expect("optional output chunk parses");
    assert!(matches!(
        chunk_progress.payload,
        EventPayload::ToolCallProgress {
            display: Some(Open::Known(DisplayDelta::Append { index: 0, text, stream: None })),
            ..
        } if text == "partial output"
    ));
}

/// Stream output is live-only: a stored envelope never carries it, and a
/// transient one carries nothing else.
#[test]
fn transient_payloads_travel_only_as_transient_events() {
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let delta = EventPayload::TextDelta {
        attempt_id: AttemptId::new_v7(),
        text: "hi".into(),
    };
    assert!(delta.is_transient());
    let stored = StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: Some(run_id),
        seq: 2,
        timestamp: jiff::Timestamp::now(),
        payload: delta.clone(),
    };
    assert_eq!(
        stored.validate(),
        Err(EventSchemaError::StoredTransientPayload)
    );
    let transient = TransientEvent {
        session_id,
        run_id: Some(run_id),
        after_seq: 1,
        timestamp: jiff::Timestamp::now(),
        payload: delta,
    };
    assert_eq!(transient.validate(), Ok(()));
    let message = EventSubscriptionMessage::Transient {
        event: std::sync::Arc::new(transient.clone()),
    };
    let wire = serde_json::to_value(&message).expect("serialize");
    assert_eq!(wire["type"], "transient");
    assert_eq!(wire["event"]["after_seq"], 1);
    assert!(wire["event"].get("seq").is_none());
    assert_eq!(
        serde_json::from_value::<EventSubscriptionMessage>(wire).expect("deserialize"),
        message
    );
    let durable = TransientEvent {
        payload: EventPayload::run_finished(crate::RunOutcome::Completed { final_text: None }),
        ..transient.clone()
    };
    assert_eq!(
        durable.validate(),
        Err(EventSchemaError::NonTransientPayload)
    );
    let unanchored = TransientEvent {
        after_seq: 0,
        ..transient
    };
    assert_eq!(
        unanchored.validate(),
        Err(EventSchemaError::ZeroEventSequence)
    );
}

#[test]
fn session_revert_and_fork_rpc_types_round_trip() {
    let session_id = SessionId::new_v7();
    let revert = SessionRevertParams {
        session_id,
        through_seq: 1,
        expected_last_seq: 6,
    };
    assert_eq!(
        serde_json::from_value::<SessionRevertParams>(serde_json::to_value(revert).unwrap())
            .unwrap(),
        revert
    );
    // A revert without the tip it expects is refused at the wire.
    assert!(
        serde_json::from_value::<SessionRevertParams>(json!({
            "session_id": session_id,
            "through_seq": 1,
        }))
        .is_err()
    );
    let fork = SessionForkParams {
        session_id,
        through_seq: 1,
        client_fork_id: ClientForkId::new("fork-1").unwrap(),
    };
    assert_eq!(
        serde_json::from_value::<SessionForkParams>(serde_json::to_value(&fork).unwrap()).unwrap(),
        fork
    );
}

fn test_resource(binding: &[u8]) -> PreparedApprovalResource {
    PreparedApprovalResource {
        capability: PermissionAction::READ,
        canonical: PreparedResourceIdentity::new(format!(
            "file:{}",
            Sha256Digest::of_bytes(binding)
        ))
        .expect("resource identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(binding),
        binding_lifetime: PreparedBindingLifetime::ProcessLocal,
        boundary: ApprovalBoundary::Exact,
        source: ApprovalResourceSource::PrimaryOperation,
    }
}

#[test]
fn prepared_operation_wire_requires_nonempty_resources_and_accepts_multiple() {
    let capability = ApprovalCapability {
        action: PermissionAction::READ,
        operation: PreparedCapabilityOperation::new("read:file").expect("capability operation"),
    };
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"arguments"),
        vec![capability],
        vec![test_resource(b"first"), test_resource(b"second")],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("multiple resources");
    let valid = serde_json::to_value(operation).expect("operation JSON");
    assert_eq!(valid["resources"].as_array().expect("resources").len(), 2);
    assert!(valid.get("resource").is_none());
    assert!(serde_json::from_value::<PreparedOperationIdentity>(valid.clone()).is_ok());

    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .expect("operation object")
        .remove("resources");
    assert!(serde_json::from_value::<PreparedOperationIdentity>(missing).is_err());

    let mut empty = valid.clone();
    empty["resources"] = json!([]);
    assert!(serde_json::from_value::<PreparedOperationIdentity>(empty).is_err());

    let mut singular = valid;
    let object = singular.as_object_mut().expect("operation object");
    object.remove("resources");
    object.insert("resource".into(), json!(test_resource(b"first")));
    assert!(serde_json::from_value::<PreparedOperationIdentity>(singular).is_err());
}

fn test_evaluation(binding: &[u8]) -> ApprovalEvaluation {
    let resource = test_resource(binding);
    ApprovalEvaluation {
        resource_digest: resource.binding_digest,
        effect: PermissionEffect::Ask,
        trace: DecisionTrace {
            action: PermissionAction::READ,
            normalized_resource: "README.md".into(),
            candidates: Vec::new(),
            effect: PermissionEffect::Ask,
            precedence_reason: "test".into(),
        },
    }
}

#[test]
fn approval_wire_requires_nonempty_evaluations_and_accepts_multiple() {
    let first = b"request-first";
    let second = b"request-second";
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"arguments"),
        vec![ApprovalCapability {
            action: PermissionAction::READ,
            operation: PreparedCapabilityOperation::new("read:file").expect("capability operation"),
        }],
        vec![test_resource(first), test_resource(second)],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("operation");
    let request = ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        crate::ToolCallId::new_v7(),
        ApprovalTrigger::PermissionPolicy,
        operation,
        vec![test_evaluation(first), test_evaluation(second)],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("approval request");
    let valid = serde_json::to_value(request).expect("request JSON");
    assert_eq!(
        valid["evaluations"].as_array().expect("evaluations").len(),
        2
    );
    assert!(valid.get("evaluation").is_none());
    assert!(serde_json::from_value::<ApprovalRequest>(valid.clone()).is_ok());

    // The stored operation fingerprint is trusted, never computed again.
    let mut written_elsewhere = valid.clone();
    written_elsewhere["operation_fingerprint"]["digest"] =
        json!(Sha256Digest::of_bytes(b"another build's canonical bytes"));
    let decoded = serde_json::from_value::<ApprovalRequest>(written_elsewhere).unwrap();
    assert_eq!(
        decoded.operation_fingerprint().digest(),
        &Sha256Digest::of_bytes(b"another build's canonical bytes")
    );

    let mut empty = valid.clone();
    empty["evaluations"] = json!([]);
    assert!(serde_json::from_value::<ApprovalRequest>(empty).is_err());

    let decision = ApprovalInternalDecision {
        decision: ApprovalInternalDecisionKind::Ask,
        source: ApprovalDecisionSource::Policy,
        reason_code: ApprovalReasonCode::PolicyRequiresApproval,
        evaluations: vec![test_evaluation(first), test_evaluation(second)],
    };
    let valid_decision = serde_json::to_value(decision).expect("decision JSON");
    assert!(serde_json::from_value::<ApprovalInternalDecision>(valid_decision.clone()).is_ok());
    let mut empty_decision = valid_decision;
    empty_decision["evaluations"] = json!([]);
    assert!(serde_json::from_value::<ApprovalInternalDecision>(empty_decision).is_err());
}

#[test]
fn checkpoint_validation_allows_predictive_trigger_and_requires_shrink() {
    let checkpoint = InternalSummaryCheckpoint::new(
        "summary".into(),
        InternalAgentInvocationId::new_v7(),
        InternalAgentRunId::new_v7(),
        SummaryByteLimit::new(1024).unwrap(),
    )
    .unwrap();
    let mut commit = ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 1,
            source_through_seq: 2,
            recent_from_seq: Some(2),
            input_through_seq: 2,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 70,
            input_tokens_before: 60,
            input_tokens_after: 2,
            keep_recent_tokens: 20,
            max_summary_bytes: SummaryByteLimit::new(1024).unwrap(),
        },
    };

    assert!(commit.validate().is_ok());
    commit.budgets.input_tokens_after = commit.budgets.input_tokens_before;
    assert_eq!(
        commit.validate(),
        Err(EventSchemaError::InvalidCheckpointBoundaries)
    );
}

#[test]
fn checkpoint_recent_tail_is_required_and_stays_within_source() {
    let checkpoint = InternalSummaryCheckpoint::new(
        "summary".into(),
        InternalAgentInvocationId::new_v7(),
        InternalAgentRunId::new_v7(),
        SummaryByteLimit::new(1024).unwrap(),
    )
    .unwrap();
    let commit = ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 2,
            source_through_seq: 4,
            recent_from_seq: Some(3),
            input_through_seq: 4,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 70,
            input_tokens_before: 60,
            input_tokens_after: 20,
            keep_recent_tokens: 10,
            max_summary_bytes: SummaryByteLimit::new(1024).unwrap(),
        },
    };

    let mut without_tail = serde_json::to_value(&commit).unwrap();
    without_tail["boundaries"]
        .as_object_mut()
        .unwrap()
        .remove("recent_from_seq");
    assert!(serde_json::from_value::<ContextCheckpointCommit>(without_tail).is_err());
    let mut without_budget = serde_json::to_value(&commit).unwrap();
    without_budget["budgets"]
        .as_object_mut()
        .unwrap()
        .remove("keep_recent_tokens");
    assert!(serde_json::from_value::<ContextCheckpointCommit>(without_budget).is_err());
    let mut no_tail = serde_json::to_value(&commit).unwrap();
    no_tail["boundaries"]["recent_from_seq"] = json!(null);
    let decoded: ContextCheckpointCommit = serde_json::from_value(no_tail).unwrap();
    assert_eq!(decoded.boundaries.recent_from_seq, None);

    for recent_from_seq in [Some(2), Some(4), None] {
        let mut candidate = commit.clone();
        candidate.boundaries.recent_from_seq = recent_from_seq;
        assert!(candidate.validate().is_ok());
    }
    for recent_from_seq in [Some(1), Some(5)] {
        let mut candidate = commit.clone();
        candidate.boundaries.recent_from_seq = recent_from_seq;
        assert_eq!(
            candidate.validate(),
            Err(EventSchemaError::InvalidCheckpointBoundaries)
        );
    }
}

#[test]
fn native_checkpoint_rejects_recent_tail_boundary_and_budget() {
    let window = NativeContextWindow::new(
        SafeCode::new("openai-responses").unwrap(),
        Sha256Digest::of_bytes(b"selection"),
        NativeContextScope {
            provider_id: ProviderId::new("openai").unwrap(),
            model_id: ProviderModelId::new("gpt-5").unwrap(),
            resource_id: SafeDisplayText::new("response-1").unwrap(),
        },
        json!({"id": "response-1"}),
    )
    .unwrap();
    let mut commit = ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::NativeWindow { window },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 1,
            source_through_seq: 2,
            recent_from_seq: Some(2),
            input_through_seq: 2,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 70,
            input_tokens_before: 60,
            input_tokens_after: 20,
            keep_recent_tokens: 10,
            max_summary_bytes: SummaryByteLimit::new(1024).unwrap(),
        },
    };

    assert_eq!(
        commit.validate(),
        Err(EventSchemaError::InvalidCheckpointBoundaries)
    );
    commit.boundaries.recent_from_seq = None;
    assert_eq!(
        commit.validate(),
        Err(EventSchemaError::InvalidCheckpointBoundaries)
    );
    commit.budgets.keep_recent_tokens = 0;
    assert!(commit.validate().is_ok());
}

#[test]
fn native_compaction_is_limited_to_responses_recipes() {
    let mut binding = frozen_binding();
    binding.descriptor.capabilities.compaction = oven_sdk::CompactionCapability::Native;
    assert!(binding.validate().is_ok());

    binding.protocol_recipe = ProtocolRecipeId::new("oven.azure.openai.responses").unwrap();
    assert!(binding.validate().is_ok());

    binding.protocol_recipe = ProtocolRecipeId::new("oven.openai.chat").unwrap();
    assert_eq!(
        binding.validate(),
        Err(ModelSchemaError::NativeCompactionUnsupported)
    );
}

#[test]
fn available_model_descriptor_serde_round_trip_preserves_variant_order() {
    let descriptor = AvailableModelDescriptor {
        key: "test/model".parse().expect("model key"),
        display_name: "Test model".into(),
        capabilities: ModelCapabilities {
            input: BTreeSet::from([Modality::Text]),
            output: BTreeSet::from([Modality::Text]),
            context_tokens: 4096,
            output_tokens: 1024,
            tool_calling: false,
            parallel_tool_calls: false,
            structured_output: false,
            reasoning: true,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: ReplayCapability::Unsupported,
            cancellation: CancellationCapability::LocalOnly,
            media: BTreeMap::new(),
        },
        input_media: Vec::new(),
        variants: ["alpha", "zeta"]
            .into_iter()
            .map(|id| AvailableVariantDescriptor {
                id: VariantId::new(id).expect("variant ID"),
                display_name: id.into(),
                origin: VariantOrigin::Explicit,
            })
            .collect(),
        variant_order: ["zeta", "alpha"]
            .into_iter()
            .map(|id| VariantId::new(id).expect("variant ID"))
            .collect(),
        default_variant: None,
    };

    let encoded = serde_json::to_value(&descriptor).expect("serialize model descriptor");
    let decoded: AvailableModelDescriptor =
        serde_json::from_value(encoded).expect("deserialize model descriptor");
    assert_eq!(decoded, descriptor);
}

#[test]
fn session_meta_serde_round_trip_preserves_last_activity() {
    let last_activity = "2026-08-06T12:34:56Z".parse().expect("timestamp");
    let meta = SessionMeta {
        session_id: SessionId::new_v7(),
        origin: SessionOrigin::Root,
        short_id: None,
        cwd_identity: CwdIdentity::new("test-workspace").expect("cwd identity"),
        creation_selection: RunSelection {
            agent: AgentId::new("primary").expect("agent ID"),
            model: ModelSelection {
                model: "openai/gpt-5.6-sol".parse().expect("model key"),
                variant: None,
            },
            preset: None,
        },
        runtime_revision: revision("runtime"),
        catalog_revision: revision("catalog"),
        provider_state_revision: revision("providers"),
        model_revision: revision("models"),
        agent_revision: revision("agents"),
        recipe_registry_revision: revision("recipes"),
        manifest_revision: revision("manifest"),
        title: None,
        title_updated_seq: 0,
        last_event_seq: 1,
        last_activity,
        last_agent_activity: None,
        status: SessionStatus::Idle,
        skipped_events: Vec::new(),
        attachments: vec![RuntimeAttachment {
            name: AttachmentName::new("labpc_api").expect("attachment name"),
            runtime_id: RuntimeId::new("labpc").expect("runtime ID"),
            capability: CapabilityName::coding(),
            kind: RuntimeKind::new("coding").expect("kind"),
            workspace: Some("/srv/api".into()),
        }],
    };

    let encoded = serde_json::to_value(&meta).expect("serialize session metadata");
    assert_eq!(encoded["attachments"][0]["workspace"], "/srv/api");
    let decoded: SessionMeta =
        serde_json::from_value(encoded.clone()).expect("deserialize session metadata");
    assert_eq!(decoded, meta);

    // A daemon before client protocol 1.7.0 sends no attachments.
    let mut older = encoded;
    older.as_object_mut().unwrap().remove("attachments");
    let decoded: SessionMeta = serde_json::from_value(older).expect("older session metadata");
    assert!(decoded.attachments.is_empty());
}

#[test]
fn revisions_use_dynamic_sha256_grammar() {
    let a = CatalogRevision::new(format!("sha256:{}", "a".repeat(64))).unwrap();
    let b = CatalogRevision::new(format!("sha256:{}", "b".repeat(64))).unwrap();
    assert_ne!(a, b);
    for invalid in [
        "sha256:abc".to_owned(),
        format!("sha256:{}", "A".repeat(64)),
        "0".repeat(64),
    ] {
        assert!(CatalogRevision::new(invalid).is_err());
    }
    let schema = serde_json::to_value(schema_for!(CatalogRevision)).unwrap();
    assert_eq!(schema["pattern"], "^sha256:[0-9a-f]{64}$");
    assert!(schema.get("const").is_none());
}

fn internal_approval_decision(
    decision: ApprovalInternalDecisionKind,
    source: ApprovalDecisionSource,
    reason_code: ApprovalReasonCode,
) -> ApprovalInternalDecision {
    ApprovalInternalDecision {
        decision,
        source,
        reason_code,
        evaluations: vec![test_evaluation(b"decision")],
    }
}

#[test]
fn internal_agent_escalation_decision_is_coherent() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Escalate,
            ApprovalDecisionSource::InternalAgent,
            ApprovalReasonCode::Escalated,
        )
        .validate()
        .is_ok()
    );
}

#[test]
fn internal_agent_ask_with_escalated_reason_is_rejected() {
    assert_eq!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Ask,
            ApprovalDecisionSource::InternalAgent,
            ApprovalReasonCode::Escalated,
        )
        .validate(),
        Err(ApprovalSchemaError::ContradictoryDecision)
    );
}

#[test]
fn policy_ask_decision_is_coherent() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Ask,
            ApprovalDecisionSource::Policy,
            ApprovalReasonCode::PolicyRequiresApproval,
        )
        .validate()
        .is_ok()
    );
}

#[test]
fn model_ask_decision_is_coherent() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Ask,
            ApprovalDecisionSource::Model,
            ApprovalReasonCode::ModelRequested,
        )
        .validate()
        .is_ok()
    );
}

#[test]
fn permission_mode_uses_snake_case_wire_values() {
    assert_eq!(
        serde_json::to_value(PermissionMode::AutoApprove).unwrap(),
        json!("auto_approve")
    );
    assert_eq!(
        serde_json::to_value(PermissionMode::AutoApproveN).unwrap(),
        json!("auto_approve_n")
    );
    assert_eq!(
        serde_json::to_value(PermissionMode::AutoApproveY).unwrap(),
        json!("auto_approve_y")
    );
    assert_eq!(
        serde_json::to_value(PermissionMode::Ask).unwrap(),
        json!("ask")
    );
    assert_eq!(
        serde_json::to_value(PermissionMode::Yolo).unwrap(),
        json!("yolo")
    );
    assert_eq!(
        serde_json::from_value::<PermissionMode>(json!("auto_approve_n")).unwrap(),
        PermissionMode::AutoApproveN
    );
    assert_eq!(
        serde_json::from_value::<PermissionMode>(json!("auto_approve_y")).unwrap(),
        PermissionMode::AutoApproveY
    );
    assert_eq!(
        serde_json::from_value::<PermissionMode>(json!("auto_approve")).unwrap(),
        PermissionMode::AutoApprove
    );
}

#[test]
fn permission_mode_reason_codes_and_source_round_trip() {
    for (outcome, reason_code, feedback) in [
        (
            ApprovalFinalOutcome::Rejected,
            ApprovalReasonCode::AutoApproveNRejected,
            Some(ApprovalFeedback {
                message: SafeErrorMessage::new("rejected by auto-approve(N) mode").unwrap(),
            }),
        ),
        (
            ApprovalFinalOutcome::Approved,
            ApprovalReasonCode::AutoApproveYApproved,
            None,
        ),
    ] {
        let decision = ApprovalFinalDecision {
            outcome,
            source: ApprovalDecisionSource::PermissionMode,
            reason_code,
            feedback,
            tree_grant_id: None,
        };
        assert!(decision.validate().is_ok());
        let wire = serde_json::to_value(&decision).unwrap();
        assert_eq!(wire["source"], json!("permission_mode"));
        assert_eq!(
            serde_json::from_value::<ApprovalFinalDecision>(wire).unwrap(),
            decision
        );
    }
}

#[test]
fn yolo_approved_is_coherent_only_for_policy_allow_and_approved() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Allow,
            ApprovalDecisionSource::Policy,
            ApprovalReasonCode::YoloApproved,
        )
        .validate()
        .is_ok()
    );
    assert_eq!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Deny,
            ApprovalDecisionSource::Policy,
            ApprovalReasonCode::YoloApproved,
        )
        .validate(),
        Err(ApprovalSchemaError::ContradictoryDecision)
    );
    let approved = ApprovalFinalDecision {
        outcome: ApprovalFinalOutcome::Approved,
        source: ApprovalDecisionSource::Policy,
        reason_code: ApprovalReasonCode::YoloApproved,
        feedback: None,
        tree_grant_id: None,
    };
    assert!(approved.validate().is_ok());
    assert_eq!(
        ApprovalFinalDecision {
            outcome: ApprovalFinalOutcome::Rejected,
            ..approved
        }
        .validate(),
        Err(ApprovalSchemaError::ContradictoryDecision)
    );
}

#[test]
fn the_catalog_view_ignores_unknown_members_and_is_source_coherent() {
    let mut value = serde_json::to_value(runtime()).unwrap();
    value["unknown"] = json!(true);
    value["catalog"]["unknown"] = json!(true);
    assert_eq!(
        serde_json::from_value::<ModelCatalogView>(value).unwrap(),
        runtime()
    );

    let mut bootstrap = runtime();
    bootstrap.catalog.source = CatalogSource::Bootstrap;
    assert!(bootstrap.validate().is_err());
    bootstrap.catalog.stale = true;
    assert!(bootstrap.validate().is_ok());
}

/// The client view carries one opaque revision and client-facing
/// descriptors only: no internal revisions, counters, schema versions,
/// recipe identities, or fingerprints.
#[test]
fn the_catalog_view_carries_no_internal_identities() {
    let schema = serde_json::to_value(schemars::schema_for!(RuntimeSnapshotResult)).unwrap();
    let text = schema.to_string();
    for internal in [
        "snapshot_schema_version",
        "recipe_registry_revision",
        "catalog_revision",
        "provider_state_revision",
        "provider_store_generation",
        "model_revision",
        "agent_revision",
        "config_revision",
        "runtime_revision",
        "quarantine_digest",
        "behavior_fingerprint",
        "recipe_fingerprint",
        "setup_fingerprint",
        "store_revision",
    ] {
        assert!(!text.contains(internal), "{internal}");
    }
    let revision = &schema["$defs"]["ModelCatalogView"]["properties"]["revision"];
    assert_eq!(revision["type"], "string");
    assert!(revision.get("pattern").is_none(), "{revision}");
}

#[test]
fn runtime_change_reasons_read_sorted_unique_and_uncapped() {
    let unsorted = json!({"previous_revision":null,"snapshot":runtime(),"reasons":["provider_store_reloaded","provider_store_changed","provider_store_reloaded"]});
    let changed = serde_json::from_value::<RuntimeChangedNotification>(unsorted).unwrap();
    assert_eq!(
        changed.reasons,
        [
            RuntimeChangeReason::ProviderStoreChanged,
            RuntimeChangeReason::ProviderStoreReloaded
        ]
    );
    let all = json!({"previous_revision":"view-0","snapshot":runtime(),"reasons":["startup","catalog_refreshed","catalog_fallback","config_reloaded","provider_connected","provider_disconnected","provider_store_changed","provider_store_reloaded","agent_reloaded","resync"]});
    let changed = serde_json::from_value::<RuntimeChangedNotification>(all).unwrap();
    assert_eq!(changed.reasons.len(), 10);
    assert_eq!(changed.previous_revision.unwrap().as_str(), "view-0");
}

#[test]
fn provider_connect_is_boundary_only_and_redacted() {
    let revision = "view-1";
    let json = format!(
        r#"{{"provider_id":"openai","expected_revision":"{revision}","setup_values":{{}},"auth_method":"bearer-api-key-v1","auth_values":{{"api_key":"super-secret"}},"client_connect_id":"connect-1"}}"#
    );
    let params: ProviderConnectParams = serde_json::from_str(&json).unwrap();
    assert_eq!(
        params
            .auth_values
            .get(&AuthFieldName::new("api_key").unwrap()),
        Some("super-secret")
    );
    assert!(!format!("{params:?}").contains("super-secret"));

    let duplicate = format!(
        r#"{{"provider_id":"openai","expected_revision":"{revision}","setup_values":{{}},"auth_method":"bearer-api-key-v1","auth_values":{{"api_key":"one","api_key":"two"}},"client_connect_id":"connect-1"}}"#
    );
    assert!(serde_json::from_str::<ProviderConnectParams>(&duplicate).is_err());
    let unknown = json.replace(
        "\"client_connect_id\"",
        "\"unknown\":true,\"client_connect_id\"",
    );
    assert!(serde_json::from_str::<ProviderConnectParams>(&unknown).is_err());
}

#[test]
fn disconnect_params_and_success_are_exact() {
    let params = ProviderDisconnectParams {
        provider_id: ProviderId::new("openai").unwrap(),
        expected_revision: ViewRevision::new("view-1").unwrap(),
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("request-1").unwrap(),
    };
    let mut value = serde_json::to_value(params).unwrap();
    value["extra"] = json!(1);
    assert!(serde_json::from_value::<ProviderDisconnectParams>(value).is_err());

    let invalid_result = json!({
        "durable_receipt": {"receipt_id":"00000000-0000-0000-0000-000000000001","committed_at":"2026-08-04T00:00:00Z"},
        "provider_id":"openai","disconnected":false,"effective_auth_state":"unavailable","runtime":{"snapshot":runtime()},"replayed":false
    });
    assert!(serde_json::from_value::<ProviderDisconnectResult>(invalid_result).is_err());
}

#[test]
fn frozen_sources_and_credentials_are_strict() {
    assert!(serde_json::from_value::<FrozenCredentialSource>(json!("provider_store")).is_ok());
    assert!(serde_json::from_value::<FrozenCredentialSource>(json!("ambient_env")).is_err());
    let binding = json!({
        "source":"provider_store",
        "auth_method":"bearer-api-key-v1",
        "fields":["z","a"],
        "parameters":{},
        "owned_headers":["authorization"]
    });
    assert!(serde_json::from_value::<FrozenCredentialBinding>(binding).is_err());
    assert!(
        serde_json::from_value::<FrozenProviderSource>(json!({
            "type":"managed",
            "provider_recipe":"openai.responses.v1",
            "source_record_digest":Sha256Digest::of_bytes(b"source"),
            "package_claim":"@ai-sdk/openai"
        }))
        .is_err()
    );
}

#[test]
fn persisted_model_bindings_decode_their_frozen_binding_only_on_demand() {
    let binding = frozen_binding();
    let stored = persisted(&binding);
    assert_eq!(stored.adapter, "openai-responses");
    assert_eq!(stored.provider_model_id.as_str(), "gpt-5.6-sol");
    assert_eq!(stored.decode().unwrap(), binding);

    // A frozen value this build cannot read still loads; only decoding fails.
    let mut encoded = serde_json::to_value(child_agent(binding.clone())).unwrap();
    encoded["fallback_chain"][0]["frozen"] = json!({"descriptor": "from a newer build"});
    let snapshot = serde_json::from_value::<AgentSnapshot>(encoded).unwrap();
    assert_eq!(
        snapshot.fallback_chain[0].decode(),
        Err(ModelSchemaError::UnreadableFrozenBinding)
    );

    // A frozen binding for another model is not the binding the entry names.
    let mut other = binding.clone();
    other.selection_fingerprint = Sha256Digest::of_bytes(b"other selection");
    let mut mismatched = stored.clone();
    mismatched.frozen = serde_json::to_value(&other).unwrap();
    assert_eq!(
        mismatched.decode(),
        Err(ModelSchemaError::UnreadableFrozenBinding)
    );

    // The stored identity must agree with itself.
    let mut encoded = serde_json::to_value(&stored).unwrap();
    encoded["provider_model_id"] = json!("another-model");
    assert!(serde_json::from_value::<PersistedModelBinding>(encoded).is_err());
}

#[test]
fn delegation_reserved_event_roundtrips_current_request() {
    let binding = frozen_binding();
    let manifest_revision = binding.manifest_revision.clone();
    let event = EventPayload::DelegationReserved {
        reservation: DelegationReservation {
            invocation_id: InvocationId::new_v7(),
            parent_session_id: SessionId::new_v7(),
            parent_run_id: RunId::new_v7(),
            parent_tool_call_id: ToolCallId::new_v7(),
            child_session_id: SessionId::new_v7(),
        },
        child_agent: Box::new(child_agent(binding.clone())),
        revisions: Some(RuntimeRevisions {
            runtime_revision: revision("runtime"),
            catalog_revision: revision("catalog"),
            provider_state_revision: revision("providers"),
            model_revision: revision("models"),
            agent_revision: revision("agents"),
            recipe_registry_revision: revision("recipes"),
            manifest_revision,
        }),
        cache_strategies: vec![Some(FrozenCacheStrategy::OpenAi {
            prompt_cache_key: Some("delegated-${session_id}".into()),
            prompt_cache_retention: Some(FrozenOpenAiCacheRetention::TwentyFourHours),
            mode: Some(FrozenOpenAiCacheMode::Explicit),
            ttl: Some(FrozenOpenAiPromptCacheTtl::ThirtyMinutes),
            system: Some(true),
            rolling: Some(true),
        })],
        request_fingerprint: Sha256Digest::of_bytes(b"request"),
        request: DelegateRequestPayload {
            description: "Review implementation".into(),
            prompt: "Review the implementation".into(),
            title: SessionTitle::new("Review implementation").unwrap(),
            resume_session_id: None,
            inherit_context: false,
            seeded_context: Vec::new(),
            background: true,
            staged_skill: None,
        },
    };
    let encoded = serde_json::to_value(&event).unwrap();
    assert_eq!(
        serde_json::from_value::<EventPayload>(encoded.clone()).unwrap(),
        event
    );
    let mut without_strategies = encoded.clone();
    without_strategies
        .as_object_mut()
        .unwrap()
        .remove("cache_strategies");
    assert!(serde_json::from_value::<EventPayload>(without_strategies).is_err());

    // Recorded revisions are complete; absent, the reservation was frozen
    // against the latest the log recorded.
    for field in [
        "manifest_revision",
        "runtime_revision",
        "catalog_revision",
        "provider_state_revision",
        "model_revision",
        "agent_revision",
        "recipe_registry_revision",
    ] {
        let mut missing = encoded.clone();
        missing["revisions"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<EventPayload>(missing).is_err());
    }
    let mut unchanged = encoded.clone();
    unchanged.as_object_mut().unwrap().remove("revisions");
    let EventPayload::DelegationReserved { revisions, .. } =
        serde_json::from_value::<EventPayload>(unchanged).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(revisions, None);
}

#[test]
fn delegation_request_has_only_the_current_strict_shape() {
    let request = json!({
        "description":"Review implementation",
        "prompt":"Review the implementation",
        "title":"Review implementation",
        "resume_session_id":null,
        "inherit_context":true,
        "seeded_context":[{"role":"user","text":"Recent parent context"}],
        "background":true,
        "staged_skill":null
    });
    let decoded: DelegateRequestPayload = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), request);

    let schema = serde_json::to_string(&json_schema_documents()).unwrap();
    assert!(schema.contains("delegation_reserved"));
    assert!(!schema.contains("delegation_started_v5"));
}

#[test]
fn adapter_labels_accept_unknown_adapters_and_reject_malformed_ones() {
    let adapter = serde_json::from_value::<AdaptorId>(json!("acme-messages-v2")).unwrap();
    assert_eq!(adapter.as_str(), "acme-messages-v2");
    assert_eq!(
        serde_json::to_value(&AdaptorId::AZURE_OPENAI_RESPONSES).unwrap(),
        json!("azure-openai-responses")
    );
    for malformed in ["", "Anthropic", "-openai", "openai_chat", &"a".repeat(33)] {
        assert!(
            serde_json::from_value::<AdaptorId>(json!(malformed)).is_err(),
            "{malformed:?}"
        );
    }
}

#[test]
fn open_responses_is_not_a_current_protocol_surface() {
    assert!(
        serde_json::from_value::<ProviderOptions>(
            json!({"type":"open-responses","protocol_mode":"standard"})
        )
        .is_err()
    );
    let schemas = serde_json::to_string(&json_schema_documents()).unwrap();
    assert!(!schemas.contains("open-responses"));
    assert!(!schemas.contains("OpenResponses"));
}

#[test]
fn manifest_decimals_are_normalized_strings_only() {
    for value in [0.0_f32, 0.7, 0.125, 1.25, 2.0] {
        let decimal = NormalizedDecimal::from_f32(value).unwrap();
        let encoded = serde_json::to_value(&decimal).unwrap();
        assert_eq!(encoded, json!(value.to_string()));
        assert_eq!(
            serde_json::from_value::<NormalizedDecimal>(encoded).unwrap(),
            decimal
        );
    }
    for invalid in [json!(0.7), json!("0.70"), json!("7e-1"), json!("NaN")] {
        assert!(serde_json::from_value::<NormalizedDecimal>(invalid).is_err());
    }
    let defaults = FrozenResolvedRequestDefaults {
        request: FrozenRequestDefaults {
            temperature: Some(NormalizedDecimal::from_f32(0.7).unwrap()),
            top_p: Some(NormalizedDecimal::from_f32(0.125).unwrap()),
            max_output_tokens: Some(128),
            stop: Vec::new(),
            seed: None,
            tool_choice: None,
        },
        reasoning: None,
    };
    let encoded = serde_json::to_string(&defaults).unwrap();
    assert!(encoded.contains("\"temperature\":\"0.7\""));
    assert!(encoded.contains("\"top_p\":\"0.125\""));
}

#[test]
fn generated_roots_are_current_only_and_secret_free() {
    let names = json_schema_documents()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    let has = |name: &str| names.iter().any(|schema| schema == name);
    for removed in [
        "CatalogProviderListParams.schema.json",
        "CatalogProviderListResult.schema.json",
        "CatalogModelListParams.schema.json",
        "CatalogModelListResult.schema.json",
        "ModelListParams.schema.json",
        "ModelListResult.schema.json",
        "AgentListParams.schema.json",
        "AgentListResult.schema.json",
        "McpApprovalListParams.schema.json",
        "McpApprovalListResult.schema.json",
        "McpApprovalRespondParams.schema.json",
        "McpApprovalRespondResult.schema.json",
    ] {
        assert!(!has(removed), "obsolete root {removed}");
    }
    for current in [
        "RuntimeSnapshotResult.schema.json",
        "RuntimeChangedNotification.schema.json",
        "ProviderConnectParams.schema.json",
        "ProviderDisconnectParams.schema.json",
        "ProviderDisconnectResult.schema.json",
        "ConfigApplyParams.schema.json",
        "PluginsListResult.schema.json",
        "PrepareParams.schema.json",
        "ExecuteResult.schema.json",
    ] {
        assert!(has(current), "missing current root {current}");
    }
    // The connect params describe the credential fields' shape, never a
    // value.
    let encoded = serde_json::to_string(&json_schema_documents()).unwrap();
    assert!(!encoded.contains("super-secret"));
    let (_, connect) = json_schema_documents()
        .into_iter()
        .find(|(name, _)| name == "ProviderConnectParams.schema.json")
        .unwrap();
    let auth_values = &connect.as_value()["properties"]["auth_values"];
    assert!(auth_values.get("default").is_none() && auth_values.get("examples").is_none());
}

#[test]
fn setup_bounds_and_shared_identity_types_are_enforced() {
    assert!(BoundedSetupString::new("x".repeat(2048)).is_ok());
    assert!(BoundedSetupString::new("x".repeat(2049)).is_err());
    assert!(SetupFieldId::new("resource_name").is_ok());
    assert!(SetupFieldId::new("RESOURCE").is_err());
    assert!(ProviderStoreGeneration::new(0).is_err());
    assert!(ProviderStoreGeneration::new(9_007_199_254_740_992).is_err());
    let mut values = BTreeMap::new();
    values.insert(
        SetupFieldId::new("region").unwrap(),
        SafeSetupValue::String(BoundedSetupString::new("us-east-1").unwrap()),
    );
    assert_eq!(values.len(), 1);
}

#[test]
fn aborted_partial_model_turn_round_trips_through_the_wire_form() {
    let resolved_model: ResolvedModelRef = serde_json::from_value(json!({
        "selection": {"model": "openai/gpt-5.6-sol", "variant": null},
        "provider_id": "openai",
        "model_id": "gpt-5.6-sol",
        "adapter_id": "openai-responses",
        "selection_fingerprint": "a".repeat(64),
    }))
    .unwrap();
    let payload = EventPayload::ModelTurnCommitted {
        attempt_id: AttemptId::new_v7(),
        model_turn_seq: 1,
        resolved_model,
        input_through_seq: 1,
        turn: PersistedModelTurn {
            content: vec![PersistedAssistantPart::Text {
                text: "partial answer".into(),
                metadata: None,
            }],
            provider_options: BTreeMap::new(),
            finish_reason: ModelFinishReason::Aborted,
            usage: Usage::default(),
            response_metadata: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            native_replay: None,
        },
        warnings: vec![],
    };
    let encoded = serde_json::to_value(&payload).unwrap();
    assert_eq!(encoded["type"], json!("model_turn_committed"));
    assert_eq!(encoded["turn"]["finish_reason"], json!("aborted"));
    assert_eq!(
        serde_json::from_value::<EventPayload>(encoded).unwrap(),
        payload
    );
}

#[test]
fn degrading_a_large_payload_decodes_it_a_bounded_number_of_times() {
    // A turn with 4096 parts and 20 000 provider-option leaves, all ahead of
    // the one bad optional leaf: finding it must not decode the payload once
    // per leaf.
    let resolved_model: ResolvedModelRef = serde_json::from_value(json!({
        "selection": {"model": "openai/gpt-5.6-sol", "variant": null},
        "provider_id": "openai",
        "model_id": "gpt-5.6-sol",
        "adapter_id": "openai-responses",
        "selection_fingerprint": "a".repeat(64),
    }))
    .unwrap();
    let payload = EventPayload::ModelTurnCommitted {
        attempt_id: AttemptId::new_v7(),
        model_turn_seq: 1,
        resolved_model,
        input_through_seq: 1,
        turn: PersistedModelTurn {
            content: (0..4096)
                .map(|index| PersistedAssistantPart::Text {
                    text: format!("part {index}"),
                    metadata: None,
                })
                .collect(),
            provider_options: (0..20_000)
                .map(|index| (format!("option-{index}"), json!(index)))
                .collect(),
            finish_reason: ModelFinishReason::Stop,
            usage: Usage {
                output_tokens: Some(7),
                ..Usage::default()
            },
            response_metadata: BTreeMap::new(),
            provider_metadata: BTreeMap::new(),
            native_replay: None,
        },
        warnings: vec![],
    };
    let mut wire = serde_json::to_value(&payload).unwrap();
    wire["turn"]["usage"]["input_tokens"] = json!("many");
    crate::event::DEGRADATION_DECODES.with(|decodes| decodes.set(0));
    let read = deserialize_event_payload_best_effort(&wire).unwrap();
    assert_eq!(read.degraded_fields, vec!["turn.usage.input_tokens"]);
    assert_eq!(read.payload, payload);
    assert!(
        crate::event::DEGRADATION_DECODES.with(std::cell::Cell::get) <= 3,
        "one decode to fail, one per candidate, one to finish"
    );
}

#[test]
fn degrading_finds_the_leaf_serde_names_whatever_its_kind() {
    for (reason, degraded) in [
        (json!(42), true),
        (json!(1.5), true),
        (json!(true), true),
        (json!(["a"]), true),
        (json!({"nested": 1}), true),
        (json!("fine"), false),
    ] {
        let wire =
            json!({"type": "delegate_child_terminated", "status": "failed", "reason": reason});
        let read = deserialize_event_payload_best_effort(&wire).unwrap();
        assert_eq!(read.degraded_fields == ["reason"], degraded, "{wire}");
    }
    // A required field stays an error.
    assert!(
        deserialize_event_payload_best_effort(
            &json!({"type": "delegate_child_terminated", "status": 42, "reason": null})
        )
        .is_err()
    );
}

#[test]
fn bounded_display_text_bounds_bytes_but_permits_controls() {
    assert_eq!(
        BoundedDisplayText::new("line one\nline two\u{1b}")
            .expect("byte-bounded argument")
            .as_str(),
        "line one\nline two\u{1b}"
    );
    assert!(BoundedDisplayText::new("").is_err());
    assert!(BoundedDisplayText::new("x".repeat(BoundedDisplayText::MAX_BYTES)).is_ok());
    assert!(BoundedDisplayText::new("x".repeat(BoundedDisplayText::MAX_BYTES + 1)).is_err());
    assert!(SafeDisplayText::new("line one\n").is_err());
}

#[test]
fn runtime_snapshot_schema_snapshot() {
    insta::assert_json_snapshot!(schema_for!(RuntimeSnapshotResult));
}

fn provider_descriptor_json(unavailable_models: Value) -> Value {
    json!({
        "id": "kimi",
        "display_name": "Kimi",
        "presence": "current",
        "support": {"state": "supported", "reason": null},
        "setup_fields": [],
        "auth_methods": [],
        "configuration": "stored",
        "effective_auth_state": "provider_store",
        "durable_connection": null,
        "quarantine": null,
        "documentation_url": "https://platform.moonshot.ai/docs",
        "environment": ["KIMI_API_KEY"],
        "model_counts": {
            "available": 0,
            "quarantined": 1,
            "unsupported": 1,
            "needs_setup": 0,
            "needs_credentials": 0
        },
        "unavailable_models": unavailable_models
    })
}

#[test]
fn provider_descriptor_carries_sorted_unavailable_models_with_reasons() {
    let quarantined = json!({
        "id": "k2",
        "display_name": "k2",
        "kind": "quarantined",
        "reason": "invalid_catalog_model_record"
    });
    let unsupported = json!({
        "id": "k3",
        "display_name": "Kimi K3",
        "kind": "unsupported",
        "reason": "no_known_protocol_family"
    });
    let descriptor: ProviderDescriptor = serde_json::from_value(provider_descriptor_json(json!([
        quarantined.clone(),
        unsupported.clone()
    ])))
    .expect("sorted descriptor");
    assert_eq!(descriptor.model_counts.unavailable(), 2);
    assert_eq!(
        descriptor.unavailable_models[0].kind,
        ModelUnavailableKind::Quarantined
    );
    assert_eq!(descriptor.environment[0].as_str(), "KIMI_API_KEY");
    assert_eq!(
        serde_json::to_value(&descriptor).unwrap(),
        provider_descriptor_json(json!([quarantined.clone(), unsupported.clone()]))
    );
    assert!(
        serde_json::from_value::<ProviderDescriptor>(provider_descriptor_json(json!([
            unsupported,
            quarantined
        ])))
        .is_err()
    );
    let mut invalid_environment = provider_descriptor_json(json!([]));
    invalid_environment["environment"] = json!(["kimi-key"]);
    assert!(serde_json::from_value::<ProviderDescriptor>(invalid_environment).is_err());
}

#[test]
fn session_create_permission_settings_are_optional_and_strict() {
    let selection = json!({
        "agent": "primary",
        "model": {"model": "gateway/model", "variant": null},
        "preset": null
    });
    let plain: SessionCreateParams =
        serde_json::from_value(json!({ "selection": selection })).expect("plain create");
    assert_eq!(plain.permission_mode, None);
    assert!(plain.permission_rules.is_empty());
    // Unset settings stay off the wire.
    let wire = serde_json::to_value(&plain).expect("serialize");
    assert_eq!(
        wire.as_object().expect("object").keys().collect::<Vec<_>>(),
        ["selection"]
    );

    let configured: SessionCreateParams = serde_json::from_value(json!({
        "selection": selection,
        "permission_mode": "ask",
        "permission_rules": [{"action": "bash", "resource": "git log*", "effect": "allow"}]
    }))
    .expect("configured create");
    assert_eq!(configured.permission_mode, Some(PermissionMode::Ask));
    assert_eq!(
        configured.permission_rules[0].action,
        PermissionAction::BASH
    );

    assert!(
        serde_json::from_value::<SessionCreateParams>(json!({
            "selection": selection,
            "permission_overlay": []
        }))
        .is_err()
    );
    let preview: SessionPermissionPreviewParams =
        serde_json::from_value(json!({ "selection": selection })).expect("preview");
    assert!(preview.rules.is_empty());
}

#[test]
fn the_workspace_document_source_is_gone() {
    assert!(serde_json::from_str::<AgentDocumentSource>("\"workspace\"").is_err());
    assert_eq!(
        serde_json::to_string(&AgentDocumentSource::User).unwrap(),
        "\"user\""
    );
}

#[test]
fn kind_labels_accept_lowercase_names_up_to_sixteen_bytes() {
    for valid in ["coding", "global", "x", "browser-2", "abcdefghijklmnop"] {
        assert_eq!(RuntimeKind::new(valid).unwrap().as_str(), valid);
        assert_eq!(ClientKind::new(valid).unwrap().as_str(), valid);
    }
    for invalid in [
        "",
        "Coding",
        "2fa",
        "-cli",
        "web_ui",
        "a b",
        "abcdefghijklmnopq",
    ] {
        assert!(RuntimeKind::new(invalid).is_err(), "{invalid:?}");
        assert!(ClientKind::new(invalid).is_err(), "{invalid:?}");
    }
    assert!(serde_json::from_value::<RuntimeKind>(json!("Browser")).is_err());
    assert_eq!(
        serde_json::to_value(RuntimeKind::new("browser").unwrap()).unwrap(),
        json!("browser")
    );
}

#[test]
fn every_event_type_is_a_known_tag() {
    let schema = serde_json::to_value(schemars::schema_for!(EventPayload)).unwrap();
    let variants = schema["oneOf"].as_array().expect("tagged variants");
    assert!(!variants.is_empty());
    for variant in variants {
        let tag = variant["properties"]["type"]["const"]
            .as_str()
            .unwrap_or_else(|| panic!("variant without a const tag: {variant}"));
        assert!(EventPayload::is_known_tag(tag), "{tag}");
    }
    assert!(!EventPayload::is_known_tag("future_event"));
}

#[test]
fn audit_events_and_only_they_carry_skippable() {
    let event = |payload| StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId::new_v7(),
        run_id: Some(RunId::new_v7()),
        seq: 2,
        timestamp: jiff::Timestamp::now(),
        payload,
    };
    let audit = event(EventPayload::ModelRequestPrepared {
        attempt_id: AttemptId::new_v7(),
        prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
    });
    let state = event(EventPayload::UserInputSubmitted {
        content: "hello".into(),
    });
    let audit_wire = serde_json::to_value(&audit).unwrap();
    assert_eq!(audit_wire["skippable"], json!(true));
    assert_eq!(
        serde_json::from_value::<StoredEvent>(audit_wire.clone()).unwrap(),
        audit
    );
    let state_wire = serde_json::to_value(&state).unwrap();
    assert!(state_wire.get("skippable").is_none());
    assert_eq!(
        serde_json::from_value::<StoredEvent>(state_wire.clone()).unwrap(),
        state
    );
    assert_eq!(
        StoredEvent::decode_strict(&serde_json::to_vec(&audit_wire).unwrap()),
        Some(audit)
    );

    // An envelope whose `skippable` contradicts its payload's class is not
    // read as that event.
    let mut wrong = audit_wire;
    wrong.as_object_mut().unwrap().remove("skippable");
    assert!(serde_json::from_value::<StoredEvent>(wrong.clone()).is_err());
    assert_eq!(
        StoredEvent::decode_strict(&serde_json::to_vec(&wrong).unwrap()),
        None
    );
    let mut wrong = state_wire;
    wrong["skippable"] = json!(true);
    assert!(serde_json::from_value::<StoredEvent>(wrong).is_err());

    // Only the doom-loop step of an approval is audit.
    let step = |step| EventPayload::approval_step(ApprovalId::new_v7(), step);
    assert!(
        step(ApprovalStep::DoomLoop {
            operation_fingerprint: OperationFingerprint::from_prepared_operation(
                &PreparedOperationIdentity::new(
                    Sha256Digest::of_bytes(b"arguments"),
                    vec![ApprovalCapability {
                        action: PermissionAction::READ,
                        operation: PreparedCapabilityOperation::new("read:file").unwrap(),
                    }],
                    vec![test_resource(b"x")],
                    Sha256Digest::of_bytes(b"context"),
                )
                .unwrap(),
            ),
            repetitions: 4,
        })
        .is_audit()
    );
    assert!(
        !step(ApprovalStep::Escalated {
            reason_code: ApprovalReasonCode::Escalated
        })
        .is_audit()
    );
}

#[test]
fn hook_names_and_event_patterns_are_labels() {
    assert!(HookName::new("tool_before_call").is_ok());
    for name in ["", "Tool", "tool-before", "1st", &"a".repeat(65)] {
        assert!(HookName::new(name).is_err(), "{name}");
    }
    let pattern = |pattern: &str| EventPattern::new(pattern).expect("pattern");
    assert!(pattern("*").matches("text_delta"));
    assert!(pattern("tool_call_*").matches("tool_call_started"));
    assert!(!pattern("tool_call_*").matches("tool_calls"));
    assert!(pattern("run_finished").matches("run_finished"));
    assert!(!pattern("run_finished").matches("run_finished_late"));
    for invalid in ["", "**", "run*_x", "*run", "Run_*", "run-*"] {
        assert!(EventPattern::new(invalid).is_err(), "{invalid}");
    }
}

/// Every enum a client reads in a result is open: a value a newer daemon
/// added reads as its documented fallback, and the schema says so.
#[test]
fn result_enums_read_values_a_newer_daemon_added_as_their_fallbacks() {
    fn read<T: serde::de::DeserializeOwned>(value: &str) -> T {
        serde_json::from_value(json!(value)).unwrap()
    }
    let from_the_future = "from_the_future";
    assert_eq!(
        read::<SessionStatus>(from_the_future),
        SessionStatus::Unknown
    );
    assert_eq!(read::<PluginState>(from_the_future), PluginState::Unknown);
    assert_eq!(
        read::<McpServerState>(from_the_future),
        McpServerState::Unknown
    );
    assert_eq!(
        read::<RuntimeInstanceStatus>(from_the_future),
        RuntimeInstanceStatus::Unknown
    );
    assert_eq!(
        read::<ProducerSourceStatus>(from_the_future),
        ProducerSourceStatus::Unknown
    );
    assert_eq!(
        read::<ApprovalStatus>(from_the_future),
        ApprovalStatus::Cancelled
    );
    assert_eq!(read::<PermissionMode>(from_the_future), PermissionMode::Ask);
    assert_eq!(
        read::<PermissionEffect>(from_the_future),
        PermissionEffect::Ask
    );
    assert_eq!(read::<GoalStatus>(from_the_future), GoalStatus::Cancelled);
    assert_eq!(read::<AgentMode>(from_the_future), AgentMode::Internal);
    assert_eq!(
        read::<ProviderSupportState>(from_the_future),
        ProviderSupportState::Unsupported
    );
    assert_eq!(
        read::<ConfigRestartPart>(from_the_future),
        ConfigRestartPart::Server
    );
    // Known values still read as themselves.
    assert_eq!(read::<PermissionEffect>("allow"), PermissionEffect::Allow);
    assert_eq!(read::<SessionStatus>("running"), SessionStatus::Running);

    let mut changed = serde_json::to_value(RuntimeChangedNotification {
        previous_revision: None,
        snapshot: runtime(),
        reasons: vec![RuntimeChangeReason::Startup],
    })
    .unwrap();
    changed["reasons"] = json!(["startup", "from_the_future", "also_new"]);
    let changed: RuntimeChangedNotification = serde_json::from_value(changed).unwrap();
    assert_eq!(changed.reasons, [RuntimeChangeReason::Startup]);

    for schema in [
        schemars::schema_for!(SessionStatus),
        schemars::schema_for!(PluginState),
        schemars::schema_for!(ApprovalStatus),
        schemars::schema_for!(SessionsChangedNotification),
        schemars::schema_for!(SessionSubmitInputResult),
        schemars::schema_for!(EventSubscriptionMessage),
    ] {
        assert_eq!(
            schema.get(OPEN_SCHEMA_KEY),
            Some(&json!(true)),
            "{schema:?}"
        );
    }
}

/// `code` as if a later minor (9) had added it: a client at minor 8 receives
/// it as text, a client at 9 as it is.
fn code_added_in_nine(kind: &str) -> Option<u32> {
    if kind == "code" {
        Some(9)
    } else {
        DisplayBlock::since(kind)
    }
}

fn code_block() -> DisplayBlock {
    DisplayBlock::Code {
        text: "cargo test".into(),
        language: Some("sh".into()),
        start_line: None,
        omitted_lines: None,
    }
}

fn from_the_future() -> Open<DisplayBlock> {
    serde_json::from_value(json!({"type": "hologram", "frames": 3})).unwrap()
}

#[test]
fn display_blocks_newer_than_the_client_reach_it_as_text_and_unknown_ones_not_at_all() {
    let blocks = vec![
        Open::Known(code_block()),
        Open::Known(DisplayBlock::text("done")),
        from_the_future(),
    ];
    let older = DisplayAudience::with_since(8, code_added_in_nine);
    assert_eq!(
        older.blocks(&blocks),
        Some(vec![
            Open::Known(DisplayBlock::text(code_block().to_text())),
            Open::Known(DisplayBlock::text("done")),
        ])
    );
    let current = DisplayAudience::with_since(9, code_added_in_nine);
    assert_eq!(
        current.blocks(&blocks),
        Some(blocks[..2].to_vec()),
        "only the unknown block goes"
    );
    assert_eq!(current.blocks(&blocks[..2]), None, "nothing to change");
    // Every kind exists from the first minor, so no client of this build
    // receives a downgraded block.
    let this_build = DisplayAudience::new(CLIENT_PROTOCOL_VERSION);
    assert_eq!(this_build.blocks(&blocks[..2]), None);
    for kind in DISPLAY_BLOCK_SINCE.iter().map(|(kind, _)| *kind) {
        assert_eq!(
            DisplayBlock::since(kind),
            Some(FIRST_STABLE_MINOR),
            "{kind}"
        );
    }
}

#[test]
fn events_reach_each_client_with_the_display_blocks_its_version_reads() {
    let termination = ToolCallTermination {
        tool_call_id: ToolCallId::new_v7(),
        owner: AssistantToolCallRef {
            model_turn_seq: 1,
            content_index: 0,
            model_call_id: ModelCallId::new("call").unwrap(),
            provider_item_id: None,
        },
        outcome: ToolTerminationOutcome::Completed,
        result: Some(display_result(
            "cargo test",
            vec![Open::Known(code_block()), from_the_future()],
        )),
        error: None,
    };
    let event = std::sync::Arc::new(StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId::new_v7(),
        run_id: Some(RunId::new_v7()),
        seq: 4,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::ToolCallTerminated { termination },
    });
    let older = DisplayAudience::with_since(8, code_added_in_nine);
    let sent = older.stored(&event);
    let EventPayload::ToolCallTerminated { termination } = &sent.payload else {
        panic!("a termination");
    };
    assert_eq!(
        termination.result.as_ref().unwrap().display,
        [Open::Known(DisplayBlock::text("cargo test"))]
    );
    assert_eq!(sent.seq, event.seq);
    // The stored event keeps what it holds.
    let EventPayload::ToolCallTerminated { termination } = &event.payload else {
        unreachable!();
    };
    assert_eq!(termination.result.as_ref().unwrap().display.len(), 2);

    // An event without blocks to change is shared, not copied.
    let plain = std::sync::Arc::new(StoredEvent {
        payload: EventPayload::UserInputAdmitted {
            content: "hi".into(),
        },
        ..StoredEvent::clone(&event)
    });
    assert!(std::sync::Arc::ptr_eq(&older.stored(&plain), &plain));

    // Live progress that replaces the display is downgraded the same way.
    let progress = EventSubscriptionMessage::Transient {
        event: std::sync::Arc::new(TransientEvent {
            session_id: event.session_id,
            run_id: event.run_id,
            after_seq: 4,
            timestamp: event.timestamp,
            payload: EventPayload::ToolCallProgress {
                tool_call_id: ToolCallId::new_v7(),
                message: SafeDisplayText::new("running").unwrap(),
                display: Some(Open::Known(DisplayDelta::Replace {
                    blocks: vec![Open::Known(code_block())],
                })),
            },
        }),
    };
    let EventSubscriptionMessage::Transient { event: sent } = older.message(progress) else {
        panic!("transient");
    };
    assert!(matches!(
        &sent.payload,
        EventPayload::ToolCallProgress {
            display: Some(Open::Known(DisplayDelta::Replace { blocks })),
            ..
        } if *blocks == [Open::Known(DisplayBlock::text("cargo test"))]
    ));
}

#[test]
fn model_capabilities_drop_modalities_a_newer_daemon_added() {
    let mut value = json!({
        "input": ["text", "smell"], "output": ["text", "hologram"],
        "context_tokens": 4096, "output_tokens": 1024,
        "tool_calling": true, "parallel_tool_calls": false, "structured_output": false,
        "reasoning": false, "temperature": true, "top_p": true, "seed": false,
        "native_replay": "unsupported", "cancellation": "local_only", "media": {},
    });
    let capabilities: ModelCapabilities = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(capabilities.input, [Modality::Text].into_iter().collect());
    assert_eq!(capabilities.output, [Modality::Text].into_iter().collect());
    value["native_replay"] = json!("from_the_future");
    let capabilities: ModelCapabilities = serde_json::from_value(value).unwrap();
    assert_eq!(capabilities.native_replay, ReplayCapability::Unsupported);
}

#[test]
fn approval_records_with_values_a_newer_build_added_read_without_coherence_checks() {
    let future: Value = serde_json::from_str(include_str!("../compat/corpus/future/client.json"))
        .expect("future client fixture");
    let record = future["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["method"] == "approval.list")
        .expect("an approval.list result")["result"]["approvals"][0]
        .clone();
    serde_json::from_value::<ApprovalRecord>(record.clone()).expect("the fixture reads");
    let with = |pointer: &str, value: Value| {
        let mut record = record.clone();
        *record.pointer_mut(pointer).unwrap() = value;
        record
    };
    // A status, a decision kind, and a reason a newer build added read as
    // their fallbacks, whose combination with the rest this build would not
    // write.
    let deferred = serde_json::from_value::<ApprovalRecord>(with("/status", json!("deferred")))
        .expect("an unknown status");
    assert_eq!(deferred.status, ApprovalStatus::Cancelled);
    for (pointer, value) in [
        ("/internal_decision/decision", json!("defer")),
        ("/internal_decision/reason_code", json!("rate_limited")),
        ("/internal_decision/source", json!("committee")),
    ] {
        serde_json::from_value::<ApprovalRecord>(with(pointer, value.clone()))
            .unwrap_or_else(|error| panic!("{pointer} = {value}: {error}"));
    }
    // Known values that contradict each other are still refused.
    assert!(serde_json::from_value::<ApprovalRecord>(with("/status", json!("approved"))).is_err());
    assert!(
        serde_json::from_value::<ApprovalRecord>(with(
            "/internal_decision/decision",
            json!("allow")
        ))
        .is_err()
    );
}

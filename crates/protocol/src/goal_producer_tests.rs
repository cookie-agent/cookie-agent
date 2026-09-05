use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: T) {
    let wire = serde_json::to_value(&value).unwrap();
    assert_eq!(serde_json::from_value::<T>(wire).unwrap(), value);
}

fn strict<T: DeserializeOwned>(value: Value) {
    assert!(serde_json::from_value::<T>(value.clone()).is_ok());
    let mut unknown = value.clone();
    unknown["unexpected"] = json!(true);
    assert!(serde_json::from_value::<T>(unknown).is_err());
    for key in value.as_object().unwrap().keys() {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<T>(missing).is_err(),
            "missing {key}"
        );
    }
}

fn stored(payload: EventPayload, run_id: Option<RunId>) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId::new_v7(),
        run_id,
        seq: 2,
        timestamp: jiff::Timestamp::now(),
        payload,
    }
}

fn item() -> GoalItem {
    GoalItem {
        description: "Verify the root's test results".into(),
        finished: false,
    }
}

#[test]
fn session_goal_and_inspection_rpcs_reject_unknown_missing_and_wrong_fields() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    strict::<SessionGoalGetParams>(json!({"session_id": session_id}));
    strict::<SessionGoalGetResult>(json!({"goal": null}));
    strict::<SessionGoalSetParams>(
        json!({"session_id": session_id, "objective": "Verify release"}),
    );
    let state = json!({"goal_id": goal_id, "objective": "Verify release", "status": "active", "items": [], "revision": 0});
    strict::<GoalState>(state.clone());
    strict::<SessionGoalSetResult>(json!({"goal": state}));
    strict::<SessionGoalLifecycleResult>(json!({"goal": state}));
    strict::<GoalUpdateResult>(json!({"goal": state}));
    for action in ["pause", "resume", "cancel"] {
        strict::<SessionGoalLifecycleParams>(
            json!({"session_id": session_id, "goal_id": goal_id, "expected_revision": 0, "action": action}),
        );
    }
    strict::<SessionProducersParams>(json!({"session_id": session_id}));
    strict::<SessionProducersResult>(json!({"producers": [], "plugin_recovery": []}));
    assert!(serde_json::from_value::<SessionGoalLifecycleParams>(json!({"session_id": session_id, "goal_id": goal_id, "expected_revision": -1, "action": "pause"})).is_err());
    assert!(
        serde_json::from_value::<SessionGoalSetParams>(
            json!({"session_id": session_id, "objective": "Verify release", "selection": true})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionGoalLifecycleParams>(json!({
            "session_id": session_id,
            "goal_id": goal_id,
            "expected_revision": 0,
            "action": "pause",
            "selection": true,
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionGoalSetParams>(
            json!({"session_id": session_id, "objective": 42})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionProducersResult>(
            json!({"producers": null, "plugin_recovery": []})
        )
        .is_err()
    );
}

#[test]
fn goal_rpc_selection_is_optional_nullable_and_round_trips_when_explicit() {
    let session_id = SessionId::new_v7();
    let goal_id = GoalId::new_v7();
    let selection = json!({
        "agent": "primary",
        "model": {"model": "custom.test/model", "variant": null}
    });
    let set_base = json!({"session_id": session_id, "objective": "Verify release"});
    let lifecycle_base = json!({
        "session_id": session_id,
        "goal_id": goal_id,
        "expected_revision": 0,
        "action": "pause",
    });

    for value in [set_base.clone(), {
        let mut value = set_base.clone();
        value["selection"] = Value::Null;
        value
    }] {
        let decoded = serde_json::from_value::<SessionGoalSetParams>(value).unwrap();
        assert_eq!(decoded.selection, None);
        assert!(
            serde_json::to_value(decoded)
                .unwrap()
                .get("selection")
                .is_none()
        );
    }
    let mut explicit_set = set_base;
    explicit_set["selection"] = selection.clone();
    let decoded = serde_json::from_value::<SessionGoalSetParams>(explicit_set.clone()).unwrap();
    assert!(decoded.selection.is_some());
    assert_eq!(serde_json::to_value(decoded).unwrap(), explicit_set);

    for value in [lifecycle_base.clone(), {
        let mut value = lifecycle_base.clone();
        value["selection"] = Value::Null;
        value
    }] {
        let decoded = serde_json::from_value::<SessionGoalLifecycleParams>(value).unwrap();
        assert_eq!(decoded.selection, None);
        assert!(
            serde_json::to_value(decoded)
                .unwrap()
                .get("selection")
                .is_none()
        );
    }
    let mut explicit_lifecycle = lifecycle_base;
    explicit_lifecycle["selection"] = selection;
    let decoded =
        serde_json::from_value::<SessionGoalLifecycleParams>(explicit_lifecycle.clone()).unwrap();
    assert!(decoded.selection.is_some());
    assert_eq!(serde_json::to_value(decoded).unwrap(), explicit_lifecycle);

    for schema in [
        serde_json::to_value(schemars::schema_for!(SessionGoalSetParams)).unwrap(),
        serde_json::to_value(schemars::schema_for!(SessionGoalLifecycleParams)).unwrap(),
    ] {
        assert!(schema["properties"].get("selection").is_some());
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "selection")
        );
    }

    for legacy in [
        json!({
            "type": "goal_activated",
            "goal_id": goal_id,
            "objective": "Verify release",
            "revision": 0,
        }),
        json!({
            "type": "goal_lifecycle_changed",
            "goal_id": goal_id,
            "status": "paused",
            "revision": 1,
        }),
    ] {
        let decoded = serde_json::from_value::<EventPayload>(legacy).unwrap();
        match &decoded {
            EventPayload::GoalActivated { selection, .. }
            | EventPayload::GoalLifecycleChanged { selection, .. } => {
                assert_eq!(*selection, None);
            }
            _ => unreachable!(),
        }
        assert!(
            serde_json::to_value(decoded)
                .unwrap()
                .get("selection")
                .is_none(),
            "old event history stays readable and omission-preserving"
        );
    }
}

#[test]
fn goal_contracts_round_trip_and_preserve_empty_bootstrap() {
    let goal_id = GoalId::new_v7();
    for status in [
        GoalStatus::Active,
        GoalStatus::Paused,
        GoalStatus::Completed,
        GoalStatus::Cancelled,
    ] {
        round_trip(GoalState {
            goal_id,
            objective: "Ship the change".into(),
            status,
            items: vec![item()],
            revision: 3,
        });
    }
    for revision in [0, u64::MAX] {
        let event = stored(
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: vec![],
                revision,
            },
            None,
        );
        assert!(event.validate().is_ok());
        round_trip(event);
    }
    strict::<GoalGetParams>(json!({}));
    strict::<GoalGetResult>(json!({"goal": null}));
    strict::<GoalItem>(serde_json::to_value(item()).unwrap());
    round_trip(GoalUpdateParams {
        items: vec![item(), item()],
    });
    strict::<GoalUpdateParams>(json!({"items":[]}));
    for revision in [
        json!(0),
        json!(u64::MAX),
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(null),
    ] {
        assert!(
            serde_json::from_value::<GoalUpdateParams>(
                json!({"items":[],"expected_revision":revision})
            )
            .is_err()
        );
    }
    for action in ["set", "complete", "completed", "active", "stop"] {
        assert!(serde_json::from_value::<GoalLifecycleAction>(json!(action)).is_err());
    }
    assert!(
        serde_json::from_value::<GoalUpdateParams>(json!({"items":[],"status":"cancelled"}))
            .is_err()
    );
    assert!(serde_json::from_value::<GoalStatus>(json!("blocked")).is_err());
}

#[test]
fn durable_goal_and_message_events_round_trip_with_correct_run_scope() {
    let goal_id = GoalId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let reminder = GoalReminderIdentity {
        goal_id,
        revision: 2,
    };
    for payload in [
        EventPayload::GoalActivated {
            goal_id,
            objective: "Ship the change".into(),
            revision: 0,
            selection: None,
        },
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: vec![item()],
            revision: 1,
        },
        EventPayload::GoalLifecycleChanged {
            goal_id,
            status: GoalStatus::Paused,
            revision: 2,
            selection: None,
        },
        EventPayload::ProducerMessageAccepted {
            message_id,
            producer_owner: ProducerOwner::Goal { goal_id },
            mode: ProducerDeliveryMode::Queue,
            idempotency_key: ProducerIdempotencyKey::new("attempt-1").unwrap(),
            body: "Ship the change\n[ ] Verify the root's test results\nrevision: 2".into(),
            reminder: Some(reminder),
        },
        EventPayload::ProducerMessageDiscarded {
            message_id,
            reminder: Some(reminder),
            producer_owner: None,
        },
    ] {
        let event = stored(payload, None);
        assert!(event.validate().is_ok());
        round_trip(event);
    }
    let run_id = RunId::new_v7();
    for payload in [
        EventPayload::ProducerMessageAdmitted { message_id },
        EventPayload::ProducerMessagesClaimed {
            message_ids: vec![message_id],
        },
        EventPayload::ProducerMessagesReleased { claim_seq: 3 },
        EventPayload::ProducerMessageConsumed { message_id, run_id },
    ] {
        assert_eq!(
            stored(payload.clone(), None).validate(),
            Err(EventSchemaError::MissingRunId)
        );
        round_trip(stored(payload, Some(run_id)));
    }
    assert_eq!(
        stored(
            EventPayload::ProducerMessageConsumed { message_id, run_id },
            Some(RunId::new_v7())
        )
        .validate(),
        Err(EventSchemaError::InvalidProducerMessage)
    );
}

#[test]
fn producer_claim_and_discard_writer_validation_is_strict() {
    let message_id = ProducerMessageId::new_v7();
    let owner = ProducerOwner::Plugin {
        plugin: "issues".into(),
    };
    let real_discard = EventPayload::ProducerMessageDiscarded {
        message_id,
        reminder: None,
        producer_owner: Some(owner.clone()),
    };
    let wire = serde_json::to_value(&real_discard).unwrap();
    assert!(wire.get("reminder").is_none());
    assert!(stored(real_discard, None).validate().is_ok());

    let goal_id = GoalId::new_v7();
    let legacy = json!({
        "type": "producer_message_discarded",
        "message_id": message_id,
        "reminder": {"goal_id": goal_id, "revision": 4}
    });
    assert!(matches!(
        serde_json::from_value::<EventPayload>(legacy).unwrap(),
        EventPayload::ProducerMessageDiscarded {
            reminder: Some(_),
            producer_owner: None,
            ..
        }
    ));
    assert_eq!(
        stored(
            EventPayload::ProducerMessageDiscarded {
                message_id,
                reminder: None,
                producer_owner: None,
            },
            None,
        )
        .validate(),
        Err(EventSchemaError::InvalidProducerMessage)
    );

    let run_id = RunId::new_v7();
    for payload in [
        EventPayload::ProducerMessagesClaimed {
            message_ids: Vec::new(),
        },
        EventPayload::ProducerMessagesClaimed {
            message_ids: vec![message_id, message_id],
        },
        EventPayload::ProducerMessagesReleased { claim_seq: 0 },
    ] {
        assert_eq!(
            stored(payload, Some(run_id)).validate(),
            Err(EventSchemaError::InvalidProducerMessage)
        );
    }
}

#[test]
fn goal_event_writer_rejects_blank_text_but_allows_duplicate_descriptions() {
    let goal_id = GoalId::new_v7();
    for payload in [
        EventPayload::GoalActivated {
            goal_id,
            objective: " \n".into(),
            revision: 0,
            selection: None,
        },
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: vec![GoalItem {
                description: " ".into(),
                ..item()
            }],
            revision: 1,
        },
    ] {
        assert_eq!(
            stored(payload, None).validate(),
            Err(EventSchemaError::InvalidGoalEvent)
        );
    }
    let event = stored(
        EventPayload::GoalChecklistRevised {
            goal_id,
            items: vec![item(), item()],
            revision: 1,
        },
        None,
    );
    assert!(event.validate().is_ok());
    round_trip(event);
}

#[test]
fn goal_update_schema_and_arguments_have_only_the_current_fields() {
    let schema = serde_json::to_value(schemars::schema_for!(GoalUpdateParams)).unwrap();
    assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
    assert_eq!(schema["required"], json!(["items"]));
    for field in ["goal_id", "id", "expected_revision"] {
        let mut arguments = json!({"items":[]});
        arguments[field] = json!(GoalId::new_v7());
        assert!(
            serde_json::from_value::<GoalUpdateParams>(arguments).is_err(),
            "removed {field}"
        );
    }
    let item_schema = &schema["$defs"]["GoalItem"];
    assert_eq!(item_schema["properties"].as_object().unwrap().len(), 2);
    assert_eq!(item_schema["required"], json!(["description", "finished"]));
    assert_eq!(item_schema["additionalProperties"], false);
    for id in [json!("verify"), json!(GoalId::new_v7()), json!(null)] {
        let legacy_item = json!({"id":id,"description":"Verify","finished":false});
        assert!(serde_json::from_value::<GoalItem>(legacy_item.clone()).is_err());
        assert!(
            serde_json::from_value::<GoalUpdateParams>(json!({"items":[legacy_item]})).is_err()
        );
    }
}

#[test]
fn historical_checklist_item_ids_are_ignored_only_by_event_history_decoding() {
    let goal_id = GoalId::new_v7();
    let legacy_item = json!({"id":GoalId::new_v7(),"description":"Verify","finished":false});
    let legacy = json!({"type":"goal_checklist_revised","goal_id":goal_id,"revision":7,"items":vec![legacy_item; 12]});
    assert!(serde_json::from_value::<EventPayload>(legacy.clone()).is_err());
    let read = deserialize_event_payload_best_effort(legacy.clone()).unwrap();
    assert_eq!(read.degraded_fields, ["items[].id"]);
    let EventPayload::GoalChecklistRevised {
        goal_id: decoded_id,
        items,
        revision,
    } = &read.payload
    else {
        panic!("checklist event");
    };
    assert_eq!(*decoded_id, goal_id);
    assert_eq!(*revision, 7);
    assert_eq!(items.len(), 12);
    assert!(
        items
            .iter()
            .all(|item| item.description == "Verify" && !item.finished)
    );

    let mut envelope = serde_json::to_value(stored(read.payload, None)).unwrap();
    envelope["payload"] = legacy.clone();
    let decoded = serde_json::from_value::<StoredEvent>(envelope).unwrap();
    assert!(
        serde_json::to_value(decoded).unwrap()["payload"]["items"][0]
            .get("id")
            .is_none()
    );
    for field in ["unexpected", "finished", "description"] {
        let mut malformed = legacy.clone();
        malformed["items"][0][field] = json!(42);
        assert!(
            deserialize_event_payload_best_effort(malformed).is_err(),
            "must not ignore {field}"
        );
    }
}

#[test]
fn producer_identity_and_send_contracts_are_strict() {
    let session_id = SessionId::new_v7();
    let producer_id = ProducerId::new_v7();
    strict::<ExtensionProducerRegisterParams>(json!({"session_id":session_id}));
    strict::<ExtensionProducerRegisterResult>(json!({"producer_id":producer_id}));
    strict::<ExtensionProducerUnregisterParams>(
        json!({"session_id":session_id,"producer_id":producer_id}),
    );
    strict::<ExtensionProducerUnregisterResult>(json!({}));
    strict::<ExtensionProducerSendResult>(json!({"message_id":ProducerMessageId::new_v7()}));
    for mode in [ProducerDeliveryMode::Steer, ProducerDeliveryMode::Queue] {
        let send = ExtensionProducerSendParams {
            session_id,
            producer_id,
            mode,
            idempotency_key: ProducerIdempotencyKey::new("external-job:42").unwrap(),
            body: "Result\ncomplete".into(),
        };
        let value = serde_json::to_value(&send).unwrap();
        strict::<ExtensionProducerSendParams>(value.clone());
        round_trip(send);
        for field in ["producer_owner", "connection_epoch", "reminder", "priority"] {
            let mut forged = value.clone();
            forged[field] = json!("forged");
            assert!(serde_json::from_value::<ExtensionProducerSendParams>(forged).is_err());
        }
        let mut wrong = value;
        wrong["mode"] = json!("immediate");
        assert!(serde_json::from_value::<ExtensionProducerSendParams>(wrong).is_err());
    }
    for owner in [
        ProducerOwner::Plugin {
            plugin: "issues".into(),
        },
        ProducerOwner::Delegation {
            invocation_id: InvocationId::new_v7(),
        },
        ProducerOwner::Goal {
            goal_id: GoalId::new_v7(),
        },
        ProducerOwner::GoalControl {
            goal_id: GoalId::new_v7(),
        },
    ] {
        strict::<ProducerOwner>(serde_json::to_value(&owner).unwrap());
        round_trip(ProducerRegistration {
            producer_id,
            producer_owner: owner,
            session_id,
            age_ms: 0,
        });
    }
    for value in ["", "bad\nkey"] {
        assert!(ProducerIdempotencyKey::new(value).is_err());
    }
    assert!(ProducerIdempotencyKey::new("k".repeat(256)).is_ok());
    assert!(ProducerIdempotencyKey::new("k".repeat(257)).is_err());
    assert!(serde_json::from_value::<ProducerId>(json!("not-a-uuid")).is_err());
    assert!(serde_json::from_value::<GoalId>(json!("old-goal")).is_err());
}

#[test]
fn goal_control_messages_are_real_steers_not_reminders() {
    let goal_id = GoalId::new_v7();
    let mut payload = EventPayload::ProducerMessageAccepted {
        message_id: ProducerMessageId::new_v7(),
        producer_owner: ProducerOwner::GoalControl { goal_id },
        mode: ProducerDeliveryMode::Steer,
        idempotency_key: ProducerIdempotencyKey::new("goal-control:1").unwrap(),
        body: "The goal was paused by the user.".into(),
        reminder: None,
    };
    let event = stored(payload.clone(), None);
    assert!(event.validate().is_ok());
    round_trip(event);
    assert_eq!(
        serde_json::to_value(&payload).unwrap()["producer_owner"]["type"],
        "goal_control"
    );
    if let EventPayload::ProducerMessageAccepted { reminder, .. } = &mut payload {
        *reminder = Some(GoalReminderIdentity {
            goal_id,
            revision: 1,
        });
    }
    assert_eq!(
        stored(payload, None).validate(),
        Err(EventSchemaError::InvalidProducerMessage)
    );
}

#[test]
fn reminder_metadata_is_not_send_identity_and_is_best_effort_on_read() {
    let goal_id = GoalId::new_v7();
    let reminder = GoalReminderIdentity {
        goal_id,
        revision: 7,
    };
    let make = |message_id, key: &str| EventPayload::ProducerMessageAccepted {
        message_id,
        producer_owner: ProducerOwner::Goal { goal_id },
        mode: ProducerDeliveryMode::Queue,
        idempotency_key: ProducerIdempotencyKey::new(key).unwrap(),
        body: "Establish checklist".into(),
        reminder: Some(reminder),
    };
    let first = make(ProducerMessageId::new_v7(), "continuation-1");
    let second = make(ProducerMessageId::new_v7(), "continuation-2");
    assert_ne!(first, second);
    round_trip(stored(first.clone(), None));
    round_trip(stored(second, None));
    let mut wire = serde_json::to_value(first).unwrap();
    wire["producer_owner"] = json!({"type":"plugin","plugin":"issues"});
    let forged = serde_json::from_value::<EventPayload>(wire.clone()).unwrap();
    assert_eq!(
        stored(forged, None).validate(),
        Err(EventSchemaError::InvalidProducerMessage)
    );
    wire.as_object_mut().unwrap().remove("reminder");
    assert!(
        stored(serde_json::from_value(wire.clone()).unwrap(), None)
            .validate()
            .is_ok()
    );
    wire["reminder"] = json!(42);
    let recovered = deserialize_event_payload_best_effort(wire.clone()).unwrap();
    assert!(!recovered.degraded_fields.is_empty());
    assert!(matches!(
        recovered.payload,
        EventPayload::ProducerMessageAccepted { reminder: None, .. }
    ));
    wire.as_object_mut().unwrap().remove("message_id");
    assert!(deserialize_event_payload_best_effort(wire).is_err());
}

#[test]
fn recovery_handshake_has_only_explicit_completion_outcomes_and_exact_version() {
    strict::<ExtensionRecoveryStartParams>(json!({}));
    let notification = serde_json::to_value(extension_recovery_start_notification()).unwrap();
    assert_eq!(
        notification,
        json!({"jsonrpc": "2.0", "method": "plugin/recovery/start", "params": {}})
    );
    assert!(serde_json::from_value::<Request>(notification).is_err());
    let schema_names = json_schema_documents()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    assert!(schema_names.contains(&"ExtensionRecoveryStartParams.schema.json"));
    assert!(!schema_names.contains(&"ExtensionRecoveryParams.schema.json"));
    assert!(!schema_names.contains(&"ExtensionRecoveryResult.schema.json"));
    strict::<ExtensionRecoveryCompleteResult>(json!({}));
    for outcome in [
        json!({"status":"ready"}),
        json!({"status":"failed","message":"external state unavailable"}),
    ] {
        strict::<ExtensionRecoveryCompleteParams>(json!({"outcome":outcome}));
        assert!(
            serde_json::from_value::<ExtensionRecoveryCompleteParams>(
                json!({"outcome":outcome,"connection_epoch":1})
            )
            .is_err()
        );
    }
    for status in ["starting", "disabled", "complete"] {
        assert!(
            serde_json::from_value::<ExtensionRecoveryCompleteParams>(
                json!({"outcome":{"status":status}})
            )
            .is_err()
        );
    }
    for status in [
        PluginRecoveryStatus::Starting,
        PluginRecoveryStatus::Ready,
        PluginRecoveryStatus::Failed,
        PluginRecoveryStatus::Disabled,
    ] {
        round_trip(PluginRecoveryState {
            plugin: "issues".into(),
            status,
        });
    }
    assert_eq!(EXTENSION_PROTOCOL_VERSION, "0.0.5");
    round_trip(ExtensionProtocolVersion::current());
    assert!(serde_json::from_value::<ExtensionProtocolVersion>(json!("0.0.4")).is_err());
    let params: ExtensionInitializeParams =
        serde_json::from_value(extension_initialize_request("phase1").params.unwrap()).unwrap();
    assert!(
        params.capabilities.producer_messaging,
        "the engine supports explicit producer messaging"
    );
    strict::<ExtensionEngineCapabilities>(serde_json::to_value(params.capabilities).unwrap());
    strict::<ExtensionPluginCapabilities>(
        json!({"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}),
    );
}

use oven_sdk::{ModelErrorKind, SanitizedBody};

use super::*;

const DIGEST: &str = "sha256:provider-config";

fn at(seconds: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_800_000_000 + seconds).unwrap()
}

fn error(status: u16, body: &str) -> ModelError {
    ModelError::new(ModelErrorKind::InvalidRequest, "request failed")
        .with_http_status(status)
        .with_sanitized_body(SanitizedBody::new(body))
}

#[test]
fn only_definitive_compact_answers_mark_the_provider_unsupported() {
    for status in [404, 405, 501] {
        let (state, reason) =
            compact_failure_mark(&error(status, "{}"), false).expect("definitive");
        assert_eq!(state, MarkState::Unsupported);
        assert!(reason.contains(&status.to_string()), "{reason}");
    }
    for body in [
        r#"{"error":{"message":"unknown item type `compaction`"}}"#,
        r#"{"error":{"message":"content keyed `encrypted_content` isn't supported"}}"#,
    ] {
        assert_eq!(
            compact_failure_mark(&error(400, body), false).map(|mark| mark.0),
            Some(MarkState::Unsupported)
        );
        assert_eq!(
            compact_failure_mark(&error(422, body), true).map(|mark| mark.0),
            Some(MarkState::RejectsWindows),
            "a refusal naming the window a seeded call carried concerns the window"
        );
    }
    for transient in [
        error(429, r#"{"error":{"message":"compaction rate limited"}}"#),
        error(500, "{}"),
        error(502, "{}"),
        error(503, "{}"),
        error(401, "{}"),
        error(403, "{}"),
        error(400, r#"{"error":{"message":"unknown parameter: top_k"}}"#),
        ModelError::timeout("compaction response headers timed out"),
        ModelError::transport("connection reset"),
    ] {
        assert_eq!(
            compact_failure_mark(&transient, false),
            None,
            "{transient:?}"
        );
    }
}

#[test]
fn a_refusal_naming_the_window_is_a_rejected_window() {
    let openai = ModelError::new(ModelErrorKind::InvalidRequest, "OpenAI request failed")
        .with_http_status(400)
        .with_vendor_code("invalid_encrypted_content")
        .with_sanitized_body(SanitizedBody::new(
            r#"{"error":{"message":"The encrypted content for item cmp_1 could not be verified.","type":"invalid_request_error","code":"invalid_encrypted_content"}}"#,
        ));
    let reason = rejected_window_reason(&openai).expect("rejected window");
    assert!(reason.contains("invalid_encrypted_content"), "{reason}");
    assert!(
        rejected_window_reason(&error(
            400,
            r#"{"detail":"unknown input item type compaction"}"#
        ))
        .is_some()
    );
    assert_eq!(
        rejected_window_reason(&error(400, r#"{"error":{"message":"bad tool schema"}}"#)),
        None
    );
    assert_eq!(rejected_window_reason(&error(500, "compaction")), None);
}

#[test]
fn entries_expire_after_seven_days() {
    let record = NativeCompactionRecord::default();
    record.mark_at(
        "gateway",
        DIGEST,
        MarkState::Unsupported,
        "404".into(),
        at(0),
    );
    let lifetime = ENTRY_LIFETIME.as_secs();
    assert_eq!(
        record.state_at("gateway", DIGEST, at(lifetime - 1)),
        Some(MarkState::Unsupported)
    );
    assert_eq!(record.marks_at(at(lifetime - 1)).len(), 1);
    assert!(record.marks_at(at(lifetime)).is_empty());
    assert_eq!(record.state_at("gateway", DIGEST, at(lifetime)), None);
    // The expired entry is gone, not merely hidden.
    assert_eq!(record.state_at("gateway", DIGEST, at(0)), None);
}

#[test]
fn a_changed_provider_configuration_drops_its_entry() {
    let record = NativeCompactionRecord::default();
    record.mark_at(
        "gateway",
        DIGEST,
        MarkState::RejectsWindows,
        "400".into(),
        at(0),
    );
    assert_eq!(
        record.state_at("gateway", DIGEST, at(1)),
        Some(MarkState::RejectsWindows)
    );
    assert_eq!(record.state_at("gateway", "sha256:edited", at(2)), None);
    assert_eq!(record.state_at("gateway", DIGEST, at(3)), None);
}

#[test]
fn the_record_survives_a_reopen_and_writes_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let record = NativeCompactionRecord::open(directory.path());
    record.mark_at(
        "gateway",
        DIGEST,
        MarkState::Unsupported,
        "the compact endpoint answered HTTP 404".into(),
        at(0),
    );
    let path = directory.path().join(RECORD_FILE);
    assert!(!path.with_extension("json.tmp").exists());
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["providers"]["gateway"]["state"], "unsupported");
    assert_eq!(written["providers"]["gateway"]["config_digest"], DIGEST);
    let reopened = NativeCompactionRecord::open(directory.path());
    assert_eq!(
        reopened.state_at("gateway", DIGEST, at(1)),
        Some(MarkState::Unsupported)
    );
    let marks = reopened.marks_at(at(90));
    assert_eq!(marks[0].provider_id, "gateway");
    assert_eq!(marks[0].age_seconds, 90);
    assert_eq!(marks[0].reason, "the compact endpoint answered HTTP 404");
}

#[test]
fn an_unreadable_record_counts_as_empty_and_is_rewritten() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(RECORD_FILE);
    std::fs::write(&path, b"{ not json").unwrap();
    let record = NativeCompactionRecord::open(directory.path());
    assert!(record.marks_at(at(0)).is_empty());
    assert_eq!(record.state_at("gateway", DIGEST, at(0)), None);
    record.mark_at(
        "gateway",
        DIGEST,
        MarkState::RejectsWindows,
        "400".into(),
        at(0),
    );
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["providers"]["gateway"]["state"], "rejects_windows");
}

#[test]
fn concurrent_marks_never_tear_the_file_and_all_reach_it() {
    let directory = tempfile::tempdir().unwrap();
    let record = std::sync::Arc::new(NativeCompactionRecord::open(directory.path()));
    let providers = (0..16)
        .map(|index| format!("gateway-{index}"))
        .collect::<Vec<_>>();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(providers.len()));
    let writers = providers
        .iter()
        .cloned()
        .map(|provider| {
            let record = std::sync::Arc::clone(&record);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for round in 0..200 {
                    record.mark_at(
                        &provider,
                        DIGEST,
                        MarkState::Unsupported,
                        format!("round {round}"),
                        at(round),
                    );
                }
            })
        })
        .collect::<Vec<_>>();
    // A reader never finds the file half written.
    let path = directory.path().join(RECORD_FILE);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let path = path.clone();
        let done = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            let mut torn = 0;
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                if let Ok(bytes) = std::fs::read(&path)
                    && serde_json::from_slice::<serde_json::Value>(&bytes).is_err()
                {
                    torn += 1;
                }
            }
            torn
        })
    };
    for writer in writers {
        writer.join().unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(
        reader.join().unwrap(),
        0,
        "a read found the file half written"
    );
    assert!(!path.with_extension("json.tmp").exists());
    let reopened = NativeCompactionRecord::open(directory.path());
    let mut marked = reopened
        .marks_at(at(200))
        .into_iter()
        .map(|mark| (mark.provider_id, mark.reason))
        .collect::<Vec<_>>();
    marked.sort();
    let mut expected = providers
        .into_iter()
        .map(|provider| (provider, "round 199".to_owned()))
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(marked, expected);
}

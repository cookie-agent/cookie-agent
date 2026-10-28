use oven_sdk::ModelErrorKind;

use super::*;

const DIGEST: &str = "sha256:model-config";
const MODEL: &str = "gateway/claude-opus";

fn at(seconds: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_800_000_000 + seconds).unwrap()
}

fn error(kind: ModelErrorKind, status: u16) -> ModelError {
    ModelError::new(kind, "request failed").with_http_status(status)
}

#[test]
fn an_attempted_model_retries_on_any_refused_request() {
    for status in [400, 422] {
        let reason = retry_reason(
            &error(ModelErrorKind::InvalidRequest, status),
            DeferredToolsSupport::Attempted,
        )
        .expect("retried");
        assert!(reason.contains(&status.to_string()), "{reason}");
    }
    for status in [401, 404, 429, 500] {
        assert_eq!(
            retry_reason(
                &error(ModelErrorKind::InvalidRequest, status),
                DeferredToolsSupport::Attempted
            ),
            None
        );
    }
}

#[test]
fn a_verified_model_retries_only_when_oven_names_deferral_unsupported() {
    assert!(
        retry_reason(
            &error(ModelErrorKind::Unsupported, 400),
            DeferredToolsSupport::Native
        )
        .is_some()
    );
    assert_eq!(
        retry_reason(
            &error(ModelErrorKind::InvalidRequest, 400),
            DeferredToolsSupport::Native
        ),
        None
    );
    assert_eq!(
        retry_reason(
            &error(ModelErrorKind::Unsupported, 400),
            DeferredToolsSupport::Unsupported
        ),
        None
    );
}

#[test]
fn a_mark_holds_until_it_expires_or_the_model_changes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let record = DeferredToolsRecord::open(directory.path());
    assert!(!record.rejects_at(MODEL, DIGEST, at(0)));
    record.mark_at(MODEL, DIGEST, "refused".into(), at(0));
    assert!(record.rejects_at(MODEL, DIGEST, at(60)));
    assert!(!record.rejects_at("gateway/other", DIGEST, at(60)));

    // The file carries it to the next daemon.
    let reopened = DeferredToolsRecord::open(directory.path());
    assert!(reopened.rejects_at(MODEL, DIGEST, at(60)));
    let marks = reopened.marks_at(at(60));
    assert_eq!(marks.len(), 1);
    assert_eq!(
        (marks[0].provider_id.as_str(), marks[0].model_id.as_str()),
        ("gateway", "claude-opus")
    );
    assert_eq!(marks[0].age_seconds, 60);

    assert!(!reopened.rejects_at(MODEL, "sha256:changed", at(60)));
    assert!(
        reopened.marks_at(at(60)).is_empty(),
        "a stale entry is dropped"
    );

    reopened.mark_at(MODEL, DIGEST, "refused".into(), at(0));
    assert!(!reopened.rejects_at(MODEL, DIGEST, at(0) + ENTRY_LIFETIME));
}

#[test]
fn an_unreadable_record_is_empty() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join(RECORD_FILE), "not json").expect("write");
    let record = DeferredToolsRecord::open(directory.path());
    assert!(!record.rejects_at(MODEL, DIGEST, at(0)));
}

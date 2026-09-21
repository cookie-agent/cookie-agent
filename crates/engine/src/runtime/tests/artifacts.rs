use std::fs;

use cookie_agent_protocol::{ClientRunId, EventPayload, RunStartParams, Sha256Digest};

use crate::ToolError;

use super::support::*;

#[test]
fn delegated_child_artifacts_are_placed_inside_the_root_tree() {
    let (fixture, selection) = custom_fixture();
    let root = fixture
        .engine
        .create_session(selection)
        .expect("root session");
    let child = create_buffered_delegated_child(&fixture.engine, root.session_id);

    let (reference, digest) = fixture
        .engine
        .inner
        .artifacts
        .retain(child, b"child tool output")
        .expect("retain child artifact");
    let path = fixture.engine.inner.artifacts.blob_path(child, &digest);
    assert_eq!(
        reference.uri,
        format!("artifact://sha256/{digest}"),
        "the URI stays content-addressed with no tree in it"
    );
    assert!(path.is_file(), "the blob was written: {path:?}");
    let root_artifacts = fixture
        .engine
        .inner
        .store
        .resolve_dir(root.session_id)
        .expect("root session directory")
        .join("artifacts");
    assert_eq!(
        path.parent(),
        Some(root_artifacts.as_path()),
        "a child writes into its root's artifact directory"
    );
    assert!(
        !fixture
            .engine
            .inner
            .store
            .resolve_dir(child)
            .expect("child directory")
            .join("artifacts")
            .exists(),
        "the child has no private store of its own"
    );
    assert_eq!(
        fixture
            .engine
            .read_artifact(&format!("artifact://{digest}"), 0, 1)
            .expect("read back")
            .content,
        "child tool output"
    );
}

#[tokio::test]
async fn oversized_webfetch_truncation_notice_exposes_full_artifact_for_public_readback() {
    use crate::{ToolCompletion, runtime::OutputCapture};
    use cookie_agent_protocol::{PersistedToolResult, SafeDisplayText};

    let response = "data: {\"choices\":[{\"delta\":{\"content\":\"complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let (endpoint, captured, _reached, _release) =
        scripted_server_with_delayed_response(vec![response.into()], usize::MAX).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("webfetch-result-readback").unwrap(),
                selection: selection.clone(),
                input: "prepare webfetch result history".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let policy = frozen_root_policy(&fixture, &selection);

    // Construct the response at the tool-result boundary, then use real retention,
    // event persistence, and engine readback rather than pre-seeding an artifact.
    let url = "https://docs.quantumcookie.xyz/large.txt";
    let mut output = format!(
        "final_url: {url}\nstatus_code: 200\ncontent_type: text/plain\ntruncated: false\n\n"
    );
    for line in 0..25_000 {
        output.push_str(&format!("{line:05} {}\n", "x".repeat(100)));
    }
    assert!(output.len() > PersistedToolResult::MAX_OUTPUT_BYTES);
    let metadata = serde_json::json!({
        "url": url, "final_url": url, "status_code": 200,
        "content_type": "text/plain", "truncated": false,
    });
    let result = |output: String| PersistedToolResult {
        display: None,
        retained_output: None,
        title: SafeDisplayText::new("Webfetch output").unwrap(),
        output,
        metadata: metadata.clone(),
        truncation: None,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    };
    let capture = OutputCapture::new(
        fixture.engine.inner.artifacts.clone(),
        crate::test_session_id(),
        Default::default(),
        10,
        1024,
    )
    .await
    .unwrap();
    let bounded = capture
        .finish(ToolCompletion::single(result(output.clone())), false)
        .await
        .unwrap();
    let raw_preview = bounded.output.split_once("\n[Truncated.").unwrap().0;
    assert!(raw_preview.len() <= 1024);
    assert!(output.starts_with(raw_preview));
    assert_eq!(bounded.metadata, metadata);
    let retained = bounded.retained_output.as_ref().unwrap();
    assert_eq!(retained.streams[0].byte_length, output.len() as u64);
    assert_eq!(
        retained.streams[0].line_count,
        output.lines().count() as u64
    );
    let preview = bounded.output.clone();
    append_compaction_tool_history(
        &fixture,
        session.session_id,
        run.run_id,
        policy.selected_suffix.first().unwrap(),
        bounded,
        1,
    );
    capture.release_publication();

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let context = crate::model_history::assemble_model_context(
        &events,
        &fixture.engine.inner.artifacts,
        policy.selected_suffix.first().unwrap(),
        &policy.agent.composed_prompt,
    )
    .unwrap();
    let tool_result = context
        .history
        .iter()
        .find_map(|turn| match turn {
            oven_sdk::HistoryTurn::Tool(message) => message.results.first(),
            _ => None,
        })
        .expect("model-facing tool result");
    let oven_sdk::ToolContent::Mixed(values) = &tool_result.content else {
        panic!("expected preview and structured truncation notice");
    };
    assert!(matches!(&values[0], oven_sdk::ContentValue::Text(text) if text == &preview));
    let oven_sdk::ContentValue::Json(details) = &values[1] else {
        panic!("expected model-facing result metadata");
    };
    assert_eq!(details["metadata"], metadata);
    assert!(details["truncation"].is_null());
    let uri = preview
        .split_once("read(filePath=\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap();
    let artifact_id = cookie_agent_protocol::ArtifactReadPath::parse(uri)
        .unwrap()
        .digest;
    assert!(events.iter().any(|event| matches!(&event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.result.as_ref().and_then(|result| result.retained_output.as_ref())
                .is_some_and(|retained| retained.reference.uri == format!("artifact://sha256/{artifact_id}"))
    )));

    let lines = output.split_inclusive('\n').collect::<Vec<_>>();
    let mut reconstructed = String::new();
    for offset in (0..lines.len()).step_by(2_000) {
        let page = fixture
            .engine
            .read_artifact(uri, offset as u64, 2_000)
            .unwrap();
        assert_eq!(page.source, "artifact");
        assert!(!page.content.is_empty());
        assert!(page.content.len() < PersistedToolResult::MAX_OUTPUT_BYTES);
        assert_eq!(
            page.content,
            lines[offset..lines.len().min(offset + 2_000)].concat()
        );
        assert_eq!(
            page.next_offset_lines,
            (offset + 2_000 < lines.len()).then_some((offset + 2_000) as u64)
        );
        reconstructed.push_str(&page.content);
        // Readback opts out of preview truncation, so each page must fit that path.
        let page =
            crate::runtime::finish_page(ToolCompletion::single(result(page.content))).unwrap();
        assert!(page.retained_output.is_none());
        assert!(page.truncation.is_none());
    }
    assert_eq!(reconstructed, output);
    assert!(reconstructed.len() > preview.len());
    for offset in [lines.len(), lines.len() + 2_000] {
        let page = fixture
            .engine
            .read_artifact(uri, offset as u64, 2_000)
            .unwrap();
        assert!(page.content.is_empty());
        assert_eq!(page.next_offset_lines, None);
    }
    fixture.engine.shutdown().await;
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn retained_tool_result_artifacts_remain_readable_after_elision_and_revert() {
    let root_body = "data: {\"choices\":[{\"delta\":{\"content\":\"complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let (endpoint, captured, _reached, _release) =
        scripted_server_with_delayed_response(vec![root_body.to_owned()], usize::MAX).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("tool-result-readback").unwrap(),
                selection: selection.clone(),
                input: "prepare tool result history".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let policy = frozen_root_policy(&fixture, &selection);
    let binding = policy.selected_suffix.first().unwrap();
    let artifacts = &fixture.engine.inner.artifacts;
    let result =
        |output: &str,
         truncation: Option<cookie_agent_protocol::ToolOutputTruncation>,
         metadata: serde_json::Value| cookie_agent_protocol::PersistedToolResult {
            display: None,
            retained_output: None,
            title: cookie_agent_protocol::SafeDisplayText::new("Historical output").unwrap(),
            output: output.into(),
            metadata,
            truncation,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        };

    let full = "zero\none\ntwo\nthree";
    let (retained, retained_id) = artifacts
        .retain(crate::test_session_id(), full.as_bytes())
        .unwrap();
    let truncated_call = append_compaction_tool_history(
        &fixture,
        session.session_id,
        run.run_id,
        binding,
        result(
            "zero\n",
            Some(cookie_agent_protocol::ToolOutputTruncation {
                original_bytes: full.len() as u64,
                original_lines: 4,
                retained: retained.clone(),
            }),
            serde_json::Value::Null,
        ),
        1,
    );
    let page = fixture
        .engine
        .read_artifact(&format!("artifact://{retained_id}"), 1, 2)
        .unwrap();
    assert_eq!(page.content, "one\ntwo\n");
    assert_eq!(page.next_offset_lines, Some(3));
    assert_eq!(page.source, "artifact");

    let (elided_preview, preview_id) = artifacts
        .retain(crate::test_session_id(), b"zero\n")
        .unwrap();
    fixture
        .engine
        .append_direct(
            session.session_id,
            Some(run.run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ToolOutputElided {
                tool_call_id: truncated_call,
                original_bytes: full.len() as u64,
                retained: elided_preview.clone(),
            },
        )
        .unwrap();
    let page = fixture
        .engine
        .read_artifact(&format!("artifact://{retained_id}"), 2, 2)
        .unwrap();
    assert_eq!(page.content, "two\nthree");
    assert_eq!(page.source, "artifact");
    assert_ne!(retained.uri, elided_preview.uri);
    let page = fixture
        .engine
        .read_artifact(&format!("artifact://{preview_id}"), 0, 1)
        .unwrap();
    assert_eq!(page.content, "zero\n");
    assert_eq!(page.next_offset_lines, None);
    let termination_seq = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination }
                if termination.tool_call_id == truncated_call =>
            {
                Some(event.seq)
            }
            _ => None,
        })
        .unwrap();
    fixture
        .engine
        .append_direct(
            session.session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionReverted {
                through_seq: termination_seq - 1,
            },
        )
        .unwrap();
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert!(!events.iter().any(|event| matches!(&event.payload,
        EventPayload::ToolCallTerminated { termination } if termination.tool_call_id == truncated_call
    )));
    for (artifact_id, expected) in [(&retained_id, full), (&preview_id, "zero\n")] {
        let page = fixture
            .engine
            .read_artifact(&format!("artifact://{artifact_id}"), 0, 10)
            .unwrap();
        assert_eq!(page.content, expected);
        assert_eq!(page.next_offset_lines, None);
    }
    fixture.engine.shutdown().await;
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn artifact_read_uses_bearer_uris_and_bounds_pages() {
    let fixture = fixture();
    let content = "line\n".repeat(2_002);
    let (_, artifact_id) = fixture
        .engine
        .inner
        .artifacts
        .retain(crate::test_session_id(), content.as_bytes())
        .unwrap();
    // There is no session or tool event referencing this artifact.
    let page = fixture
        .engine
        .read_artifact(&format!("artifact://{artifact_id}"), 0, u64::MAX)
        .unwrap();
    assert_eq!(page.content, "line\n".repeat(2_000));
    assert_eq!(page.next_offset_lines, Some(2_000));
    assert_eq!(page.source, "artifact");
    let page = fixture
        .engine
        .read_artifact(&format!("artifact://{artifact_id}"), 2_000, 2)
        .unwrap();
    assert_eq!(page.content, "line\nline\n");
    assert_eq!(page.next_offset_lines, None);
    let page = fixture
        .engine
        .read_artifact(&format!("artifact://{artifact_id}"), u64::MAX, 1)
        .unwrap();
    assert!(page.content.is_empty());
    assert_eq!(page.next_offset_lines, None);
    assert!(
        fixture
            .engine
            .read_artifact(&format!("artifact://{artifact_id}"), 0, 0)
            .is_err()
    );

    let (_, large_id) = fixture
        .engine
        .inner
        .artifacts
        .retain(
            crate::test_session_id(),
            &vec![b'x'; cookie_agent_protocol::PersistedToolResult::MAX_OUTPUT_BYTES + 1],
        )
        .unwrap();
    assert!(matches!(
        fixture
            .engine
            .read_artifact(&format!("artifact://{large_id}"), 0, 1),
        Err(ToolError::ResourceLimit(_))
    ));

    let (stdout, stdout_digest) = fixture
        .engine
        .inner
        .artifacts
        .retain(crate::test_session_id(), b"out\n")
        .unwrap();
    let (stderr, stderr_digest) = fixture
        .engine
        .inner
        .artifacts
        .retain(crate::test_session_id(), b"err-0\nerr-1\n")
        .unwrap();
    let manifest = serde_json::to_vec(&cookie_agent_protocol::ToolOutputManifest {
        streams: vec![
            cookie_agent_protocol::RetainedToolStream {
                name: Some("stdout".into()),
                reference: stdout,
                sha256: Sha256Digest::new(stdout_digest).unwrap(),
                byte_length: 4,
                line_count: 1,
                truncated: false,
                next_offset: None,
            },
            cookie_agent_protocol::RetainedToolStream {
                name: Some("stderr".into()),
                reference: stderr,
                sha256: Sha256Digest::new(stderr_digest).unwrap(),
                byte_length: 12,
                line_count: 2,
                truncated: false,
                next_offset: None,
            },
        ],
    })
    .unwrap();
    let (_, manifest_id) = fixture
        .engine
        .inner
        .artifacts
        .retain(crate::test_session_id(), &manifest)
        .unwrap();
    let page = fixture
        .engine
        .read_artifact(&format!("artifact://{manifest_id}/stderr"), 1, 1)
        .unwrap();
    assert_eq!(page.content, "err-1\n");
    assert_eq!(page.source, "artifact.stderr");
    assert!(
        fixture
            .engine
            .read_artifact(&format!("artifact://{manifest_id}/unknown"), 0, 1)
            .is_err()
    );
    let raw_manifest = fixture
        .engine
        .read_artifact(&format!("artifact://{manifest_id}"), 0, 1)
        .unwrap();
    assert_eq!(raw_manifest.content.as_bytes(), manifest);
    let serialized = serde_json::to_vec(&oven_sdk::ToolContent::Text(
        "ordinary non-Bash output".into(),
    ))
    .unwrap();
    let (_, serialized_id) = fixture
        .engine
        .inner
        .artifacts
        .retain(crate::test_session_id(), &serialized)
        .unwrap();
    let raw_serialized = fixture
        .engine
        .read_artifact(&format!("artifact://{serialized_id}"), 0, 1)
        .unwrap();
    assert_eq!(raw_serialized.content.as_bytes(), serialized);
    for id in [&artifact_id, &serialized_id] {
        for stream in ["stdout", "stderr"] {
            let error = fixture
                .engine
                .read_artifact(&format!("artifact://{id}/{stream}"), 0, 1)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("artifact is not a named-stream manifest")
            );
        }
    }
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn artifact_read_rejects_invalid_missing_and_corrupt_artifacts() {
    let fixture = fixture();
    for id in [
        String::new(),
        format!("artifact://sha256/{}", "a".repeat(64)),
        "A".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(64),
        format!("{}\n", "a".repeat(64)),
        "../outside".into(),
        "%2e%2e%2foutside".into(),
        "artifact://sha256/../outside".into(),
        "artifact://sha256/%2e%2e%2foutside".into(),
        "artifact://sha256//etc/passwd".into(),
        format!("artifact://sha256/{}", "A".repeat(64)),
        format!("artifact://sha256/{}/../outside", "a".repeat(64)),
        format!("artifact://sha256/{}?other", "a".repeat(64)),
        "artifact://sha256/short".into(),
        "file:///etc/passwd".into(),
    ] {
        let error = fixture
            .engine
            .read_artifact(&format!("artifact://{id}"), 0, 1)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("64-character lowercase SHA-256 digest"),
            "{id}: {error}"
        );
    }
    let missing = "0".repeat(64);
    assert!(
        fixture
            .engine
            .read_artifact(&format!("artifact://{missing}"), 0, 1)
            .unwrap_err()
            .to_string()
            .contains("artifact missing")
    );
    let writer = crate::test_session_id();
    let (_, digest) = fixture
        .engine
        .inner
        .artifacts
        .retain(writer, b"original\n")
        .unwrap();
    let stored = fixture.engine.inner.artifacts.blob_path(writer, &digest);
    assert!(stored.is_file(), "write lands in the tree directory");
    fs::write(stored, b"modified\n").unwrap();
    assert!(
        fixture
            .engine
            .read_artifact(&format!("artifact://{digest}"), 0, 1)
            .unwrap_err()
            .to_string()
            .contains("does not match its digest")
    );
    fixture.engine.shutdown().await;
}

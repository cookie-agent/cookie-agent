use super::*;
use cookie_agent_protocol::SafeDisplayText;

async fn append(capture: &OutputCapture, chunks: &[ToolOutputChunk]) -> Result<(), ToolError> {
    let id = cookie_agent_protocol::ToolCallId::new_v7();
    capture
        .emit(
            ToolProgress {
                tool_call_id: id,
                message: String::new(),
                display: None,
                output: chunks.to_vec(),
            },
            OutputHub::new(id, 64 * 1024),
            None,
        )
        .await
}

fn completion(output: ToolCompletionOutput) -> ToolCompletion {
    ToolCompletion {
        output,
        failed: false,
        result: PersistedToolResult {
            title: SafeDisplayText::new("Test output").unwrap(),
            output: String::new(),
            display: Some("final UI display".into()),
            retained_output: None,
            metadata: serde_json::Value::Null,
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        },
    }
}

#[tokio::test]
async fn named_streams_capture_in_declaration_order_with_independent_previews() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        ToolOutputDeclaration::Named {
            streams: vec!["results".into(), "diagnostics".into(), "empty".into()],
        },
        1,
        5,
    )
    .await
    .unwrap();
    for (name, text) in [
        ("diagnostics", "bad\nextra"),
        ("results", "ab"),
        ("results", "cdef\nnext"),
    ] {
        append(
            &capture,
            &[ToolOutputChunk {
                stream: Some(name.into()),
                text: text.into(),
            }],
        )
        .await
        .unwrap();
    }
    let result = capture
        .finish(completion(ToolCompletionOutput::Streamed), false)
        .await
        .unwrap();
    assert_eq!(result.display.as_deref(), Some("final UI display"));
    assert!(!result.output.contains("final UI display"));
    assert!(result.output.starts_with("[results]\nabcde\n[Truncated."));
    assert!(result.output.contains("/results\", offset=0)"));
    assert!(result.output.contains("/diagnostics\", offset=1)"));
    assert!(result.output.ends_with("[empty]\n"));
    let retained = result.retained_output.unwrap();
    assert_eq!(
        retained
            .streams
            .iter()
            .map(|stream| stream.name.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["results", "diagnostics", "empty"]
    );
    for (stream, expected) in retained
        .streams
        .iter()
        .zip(["abcdef\nnext", "bad\nextra", ""])
    {
        assert_eq!(
            store
                .read_paged(crate::test_session_id(), stream.sha256.as_str(), 0, 10)
                .unwrap()
                .content,
            expected
        );
    }
    let manifest_digest = retained
        .reference
        .uri
        .strip_prefix("artifact://sha256/")
        .unwrap();
    let manifest: ToolOutputManifest = serde_json::from_str(
        &store
            .read_paged(crate::test_session_id(), manifest_digest, 0, 1)
            .unwrap()
            .content,
    )
    .unwrap();
    manifest.validate().unwrap();
    assert_eq!(manifest.streams, retained.streams);
}

#[tokio::test]
async fn streamed_completion_rejects_resupply_and_preserves_incomplete_utf8_output() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        ToolOutputDeclaration::Single,
        10,
        3,
    )
    .await
    .unwrap();
    append(
        &capture,
        &[ToolOutputChunk {
            stream: None,
            text: "a\u{20ac}z".into(),
        }],
    )
    .await
    .unwrap();
    assert!(
        capture
            .finish(
                completion(ToolCompletionOutput::Single {
                    text: "duplicate".into()
                }),
                false
            )
            .await
            .is_err()
    );
    let result = capture
        .finish(completion(ToolCompletionOutput::Streamed), true)
        .await
        .unwrap();
    assert!(result.output.starts_with("a\n[Truncated."));
    assert!(result.output.contains("offset=0)"));
    let retained = result.retained_output.unwrap();
    assert!(retained.incomplete);
    assert_eq!(retained.streams[0].byte_length, 5);
    assert_eq!(
        store
            .read_paged(
                crate::test_session_id(),
                retained.streams[0].sha256.as_str(),
                0,
                1
            )
            .unwrap()
            .content,
        "a\u{20ac}z"
    );
    assert!(
        append(
            &capture,
            &[ToolOutputChunk {
                stream: None,
                text: "late".into()
            }]
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn terminal_output_uses_capture_and_opt_out_never_publishes_an_artifact() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let result = finish_page(completion(ToolCompletionOutput::Single {
        text: "one\ntwo\n".into(),
    }))
    .unwrap();
    assert_eq!(result.output, "one\ntwo\n");
    assert!(result.retained_output.is_none());
    assert!(result.truncation.is_none());
    assert_eq!(
        std::fs::read_dir(root.path().join("artifacts"))
            .unwrap()
            .count(),
        0
    );
    assert!(matches!(
        finish_page(completion(ToolCompletionOutput::Single {
            text: "x".repeat(PersistedToolResult::MAX_OUTPUT_BYTES + 1)
        })),
        Err(ToolError::ResourceLimit(_))
    ));
    assert_eq!(
        std::fs::read_dir(root.path().join("artifacts"))
            .unwrap()
            .count(),
        0
    );
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        ToolOutputDeclaration::Single,
        1,
        4,
    )
    .await
    .unwrap();
    let result = capture
        .finish(
            completion(ToolCompletionOutput::Single {
                text: "one\ntwo\n".into(),
            }),
            false,
        )
        .await
        .unwrap();
    assert!(result.output.starts_with("one\n\n[Truncated."));
    let stream = &result.retained_output.as_ref().unwrap().streams[0];
    assert_eq!(stream.next_offset, Some(1));
    assert_eq!(stream.line_count, 2);
    assert_eq!(
        store
            .read_paged(crate::test_session_id(), stream.sha256.as_str(), 1, 1)
            .unwrap()
            .content,
        "two\n"
    );
}

#[tokio::test]
async fn output_publication_is_protected_until_terminal_references_are_persisted() {
    let root = tempfile::tempdir().unwrap();
    let session = crate::test_session_id();
    let store = ArtifactRouter::open(root.path().to_path_buf()).unwrap();
    // Loaded up front, so only the in-flight publication protects the blobs
    // from the first sweep.
    store.note_tree_loaded(session);
    let capture = OutputCapture::new(
        store.clone(),
        session,
        ToolOutputDeclaration::Named {
            streams: vec!["a".into(), "b".into()],
        },
        10,
        100,
    )
    .await
    .unwrap();
    let result = capture
        .finish(
            completion(ToolCompletionOutput::Named {
                streams: vec![
                    ToolOutputChunk {
                        stream: Some("b".into()),
                        text: "two".into(),
                    },
                    ToolOutputChunk {
                        stream: Some("a".into()),
                        text: "one".into(),
                    },
                ],
            }),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .collect_garbage(std::time::Duration::ZERO)
            .unwrap()
            .deleted,
        0
    );
    std::fs::write(
        root.path()
            .join(session.to_string())
            .join(crate::session::EVENTS_FILE),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
    drop(capture);
    let report = store.collect_garbage(std::time::Duration::ZERO).unwrap();
    assert_eq!(report.deleted, 0);
    assert_eq!(report.retained, 3);
}

#[tokio::test]
async fn output_deltas_reject_undeclared_channels_and_oversized_chunks() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(
        store,
        crate::test_session_id(),
        ToolOutputDeclaration::Single,
        1,
        1,
    )
    .await
    .unwrap();
    assert!(
        append(
            &capture,
            &[ToolOutputChunk {
                stream: Some("stdout".into()),
                text: "bad".into()
            }]
        )
        .await
        .is_err()
    );
    assert!(
        append(
            &capture,
            &[ToolOutputChunk {
                stream: None,
                text: "x".repeat(MAX_TOOL_DELTA_BYTES + 1)
            }]
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn absent_final_display_does_not_fall_back_to_authoritative_output() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(store, crate::test_session_id(), Default::default(), 10, 100)
        .await
        .unwrap();
    let mut completion = completion(ToolCompletionOutput::Single {
        text: "model-only output".into(),
    });
    completion.result.display = None;
    let result = capture.finish(completion, false).await.unwrap();
    assert_eq!(result.output, "model-only output");
    assert_eq!(result.display.as_deref(), Some(""));
}

#[tokio::test]
async fn display_budget_does_not_stop_authoritative_capture() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        Default::default(),
        1,
        100,
    )
    .await
    .unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
    let id = cookie_agent_protocol::ToolCallId::new_v7();
    let sink = crate::ProgressSink::with_capture(
        sender,
        crate::events::OutputHub::new(id, 64 * 1024),
        capture.clone(),
    );
    for _ in 0..100 {
        sink.send(crate::ToolProgress {
            tool_call_id: id,
            message: String::new(),
            display: Some("d".repeat(1024)),
            output: vec![ToolOutputChunk {
                stream: None,
                text: "data\n".into(),
            }],
        })
        .await
        .unwrap();
    }
    let mut display_bytes = 0;
    while let Ok(progress) = receiver.try_recv() {
        display_bytes += progress.display.unwrap().len();
    }
    assert_eq!(display_bytes, cookie_agent_protocol::MAX_TOOL_DISPLAY_BYTES);
    let result = capture
        .finish(completion(ToolCompletionOutput::Streamed), false)
        .await
        .unwrap();
    let stream = &result.retained_output.as_ref().unwrap().streams[0];
    assert_eq!(
        store
            .read_paged(crate::test_session_id(), stream.sha256.as_str(), 0, 200)
            .unwrap()
            .content,
        "data\n".repeat(100)
    );
}

#[tokio::test]
async fn progress_backpressure_precedes_acceptance_and_never_drops_output() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        Default::default(),
        10,
        100,
    )
    .await
    .unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let id = cookie_agent_protocol::ToolCallId::new_v7();
    let sink = crate::ProgressSink::with_capture(
        sender,
        crate::events::OutputHub::new(id, 100),
        capture.clone(),
    );
    let delta = |text: &str| crate::ToolProgress {
        tool_call_id: id,
        message: String::new(),
        display: Some(text.into()),
        output: vec![ToolOutputChunk {
            stream: None,
            text: text.into(),
        }],
    };
    sink.send(delta("first\n")).await.unwrap();
    let queued = sink.clone();
    let second = delta("second\n");
    let sending = tokio::spawn(async move { queued.send(second).await });
    tokio::task::yield_now().await;
    assert!(!sending.is_finished());
    receiver.recv().await.unwrap();
    sending.await.unwrap().unwrap();
    receiver.recv().await.unwrap();
    drop(receiver);
    assert!(sink.send(delta("rejected\n")).await.is_err());
    let result = capture
        .finish(completion(ToolCompletionOutput::Streamed), false)
        .await
        .unwrap();
    assert_eq!(
        store
            .read_paged(
                crate::test_session_id(),
                result.retained_output.as_ref().unwrap().streams[0]
                    .sha256
                    .as_str(),
                0,
                10
            )
            .unwrap()
            .content,
        "first\nsecond\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn capture_io_errors_latch_and_preserve_previously_accepted_output() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("artifacts");
    let store = ArtifactRouter::open_flat(directory.clone()).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        Default::default(),
        10,
        100,
    )
    .await
    .unwrap();
    append(
        &capture,
        &[ToolOutputChunk {
            stream: None,
            text: "accepted\n".into(),
        }],
    )
    .await
    .unwrap();
    {
        let mut state = capture.0.state.lock().unwrap();
        let channel = &mut state.channels[0];
        let readonly = File::open(directory.join(&channel.temporary)).unwrap();
        *channel.file.get_mut().unwrap() = readonly;
    }
    assert!(
        append(
            &capture,
            &[ToolOutputChunk {
                stream: None,
                text: "rejected\n".into()
            }]
        )
        .await
        .is_err()
    );
    assert!(
        capture
            .finish(completion(ToolCompletionOutput::Streamed), false)
            .await
            .is_err()
    );
    let result = capture
        .finish(completion(ToolCompletionOutput::Streamed), true)
        .await
        .unwrap();
    let retained = result.retained_output.unwrap();
    assert!(retained.incomplete);
    assert_eq!(
        store
            .read_paged(
                crate::test_session_id(),
                retained.streams[0].sha256.as_str(),
                0,
                10
            )
            .unwrap()
            .content,
        "accepted\n"
    );
}

#[tokio::test]
async fn aggregate_previews_stay_within_the_event_bound() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let names = (0..cookie_agent_protocol::MAX_TOOL_STREAMS)
        .map(|index| format!("stream{index}"))
        .collect::<Vec<_>>();
    let capture = OutputCapture::new(
        store,
        crate::test_session_id(),
        ToolOutputDeclaration::Named {
            streams: names.clone(),
        },
        usize::MAX,
        usize::MAX,
    )
    .await
    .unwrap();
    let output = ToolCompletionOutput::Named {
        streams: names
            .into_iter()
            .map(|name| ToolOutputChunk {
                stream: Some(name),
                text: "x".repeat(400_000),
            })
            .collect(),
    };
    let result = capture.finish(completion(output), false).await.unwrap();
    result.validate().unwrap();
    assert!(result.output.len() <= PersistedToolResult::MAX_OUTPUT_BYTES);
    assert!(
        result
            .retained_output
            .as_ref()
            .unwrap()
            .streams
            .iter()
            .all(|stream| stream.truncated && stream.next_offset == Some(0))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_append_awaiter_cannot_lose_bytes_or_let_finalization_overtake_io() {
    let directory = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        Default::default(),
        10,
        100,
    )
    .await
    .unwrap();
    let (entered, release) = blocking_io::gate(store.io_test_hook(), "capture_write", None);
    let writer = capture.clone();
    let writing = tokio::spawn(async move {
        append(
            &writer,
            &[ToolOutputChunk {
                stream: None,
                text: "accepted before cancellation\n".into(),
            }],
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    assert!(!writing.is_finished());
    assert_eq!(
        tokio::spawn(async { "responsive" }).await.unwrap(),
        "responsive"
    );
    writing.abort();
    assert!(writing.await.unwrap_err().is_cancelled());
    let finishing = tokio::spawn(async move {
        capture
            .finish(completion(ToolCompletionOutput::Streamed), true)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!finishing.is_finished());
    release.send(()).unwrap();
    let result = finishing.await.unwrap().unwrap();
    let retained = result.retained_output.unwrap();
    assert!(retained.incomplete);
    assert_eq!(
        store
            .read_paged(
                crate::test_session_id(),
                retained.streams[0].sha256.as_str(),
                0,
                10
            )
            .unwrap()
            .content,
        "accepted before cancellation\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn capture_finalization_does_not_block_async_workers_or_publication_release() {
    let directory = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        Default::default(),
        10,
        100,
    )
    .await
    .unwrap();
    append(
        &capture,
        &[ToolOutputChunk {
            stream: None,
            text: "complete\n".into(),
        }],
    )
    .await
    .unwrap();
    let (entered, release) = blocking_io::gate(store.io_test_hook(), "capture_finalize", None);
    let owner = capture.clone();
    let finishing = tokio::spawn(async move {
        owner
            .finish(completion(ToolCompletionOutput::Streamed), false)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    assert!(!finishing.is_finished());
    assert_eq!(tokio::spawn(async { 3 }).await.unwrap(), 3);
    // An aborted terminal owner must not wait on the capture's heavy state lock,
    // and an in-flight finalizer must not restore publication protection later.
    capture.release_publication();
    assert!(store.publication().try_write().is_err());
    release.send(()).unwrap();
    assert_eq!(finishing.await.unwrap().unwrap().output, "complete\n");
    assert!(store.publication().try_write().is_ok());
}

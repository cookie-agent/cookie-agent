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
    // A one-line limit keeps no tail, so the head gets every byte.
    let digest = result
        .retained_output
        .as_ref()
        .unwrap()
        .reference
        .as_ref()
        .unwrap()
        .uri
        .strip_prefix("artifact://sha256/")
        .unwrap()
        .to_owned();
    assert_eq!(
        result.output,
        format!(
            "[results]\nabcde\n[… 1 line omitted. Read more: \
             read(filePath=\"artifact://{digest}/results\", offset=0)]\n\n\
             [diagnostics]\nbad\n[… 5 bytes omitted. Read more: \
             read(filePath=\"artifact://{digest}/diagnostics\", offset=1)]\n\n\
             [empty]\n"
        )
    );
    let retained = result.retained_output.unwrap();
    assert_eq!(
        retained
            .streams
            .iter()
            .map(|stream| stream.name.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["results", "diagnostics", "empty"]
    );
    for (stream, expected) in retained.streams.iter().zip(["abcdef\nnext", "bad\nextra"]) {
        assert_eq!(
            store
                .read_paged(
                    crate::test_session_id(),
                    stream.sha256.as_ref().unwrap().as_str(),
                    0,
                    10
                )
                .unwrap()
                .content,
            expected
        );
    }
    // The empty stream fits its preview: nothing stands behind it.
    assert_eq!(retained.streams[2].reference, None);
    assert_eq!(retained.streams[2].sha256, None);
    let manifest_digest = retained
        .reference
        .as_ref()
        .unwrap()
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
    // Head and tail split the byte limit; the middle is counted in bytes when
    // no line break falls there.
    assert!(
        result
            .output
            .starts_with("a\n[… 3 bytes omitted. Read more:")
    );
    assert!(
        result.output.ends_with("offset=0)]\nz"),
        "{}",
        result.output
    );
    let retained = result.retained_output.unwrap();
    assert!(retained.incomplete);
    assert_eq!(retained.streams[0].byte_length, 5);
    assert_eq!(
        store
            .read_paged(
                crate::test_session_id(),
                retained.streams[0].sha256.as_ref().unwrap().as_str(),
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
        8,
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
    assert!(
        result
            .output
            .starts_with("one\n[… 1 line omitted. Read more:")
    );
    let stream = &result.retained_output.as_ref().unwrap().streams[0];
    assert_eq!(stream.next_offset, Some(1));
    assert_eq!(stream.line_count, 2);
    assert_eq!(
        store
            .read_paged(
                crate::test_session_id(),
                stream.sha256.as_ref().unwrap().as_str(),
                1,
                1
            )
            .unwrap()
            .content,
        "two\n"
    );
}

#[tokio::test]
async fn a_revert_sweep_skips_output_whose_terminal_event_is_not_persisted() {
    let root = tempfile::tempdir().unwrap();
    let session = crate::test_session_id();
    let store = ArtifactRouter::open(root.path().to_path_buf()).unwrap();
    let capture = OutputCapture::new(
        store.clone(),
        session,
        ToolOutputDeclaration::Single,
        1,
        100,
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
    let digest = result.retained_output.unwrap().streams[0]
        .sha256
        .as_ref()
        .unwrap()
        .to_string();
    let sweep = |store: &ArtifactRouter| {
        store
            .collect_reverted(session, [digest.clone()].into_iter().collect(), || {
                Ok(Some(std::collections::HashSet::new()))
            })
            .unwrap()
    };
    assert_eq!(sweep(&store), 0, "the publication is still in flight");
    capture.release_publication();
    assert_eq!(sweep(&store), 1);
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
            .read_paged(
                crate::test_session_id(),
                stream.sha256.as_ref().unwrap().as_str(),
                0,
                200
            )
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
    // Both deltas fit the preview, so the result holds them inline.
    assert_eq!(result.output, "first\nsecond\n");
    assert_eq!(result.retained_output.unwrap().streams[0].reference, None);
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
        4,
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
        channel.file = Some(readonly);
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
                retained.streams[0].sha256.as_ref().unwrap().as_str(),
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
        1,
        10,
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
                retained.streams[0].sha256.as_ref().unwrap().as_str(),
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
        4,
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
    let result = finishing.await.unwrap().unwrap();
    assert_eq!(result.retained_output.unwrap().streams[0].byte_length, 9);
    assert!(store.publication().try_write().is_ok());
}

#[tokio::test]
async fn truncated_previews_show_the_first_and_last_lines() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    for (lines, max_bytes) in [(10, 1_000), (10_000, 64)] {
        let capture = OutputCapture::new(
            store.clone(),
            crate::test_session_id(),
            ToolOutputDeclaration::Single,
            4,
            max_bytes,
        )
        .await
        .unwrap();
        // Many small appends roll the bounded tail buffer over repeatedly.
        for line in 0..lines {
            append(
                &capture,
                &[ToolOutputChunk {
                    stream: None,
                    text: format!("line{line}\n"),
                }],
            )
            .await
            .unwrap();
        }
        let result = capture
            .finish(completion(ToolCompletionOutput::Streamed), false)
            .await
            .unwrap();
        let digest = result
            .retained_output
            .as_ref()
            .unwrap()
            .reference
            .as_ref()
            .unwrap()
            .uri
            .strip_prefix("artifact://sha256/")
            .unwrap()
            .to_owned();
        assert_eq!(
            result.output,
            format!(
                "line0\nline1\n[… {} lines omitted. Read more: read(filePath=\"artifact://{digest}\", \
                 offset=2)]\nline{}\nline{}\n",
                lines - 4,
                lines - 2,
                lines - 1
            )
        );
        assert_eq!(
            result.retained_output.unwrap().streams[0].next_offset,
            Some(2)
        );
    }
}

#[tokio::test]
async fn small_chunks_are_written_in_batches() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactRouter::open_flat(root.path().join("artifacts")).unwrap();
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&writes);
    store.io_test_hook().set(Arc::new(move |operation, _| {
        if operation == "capture_write" {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }));
    let capture = OutputCapture::new(
        store.clone(),
        crate::test_session_id(),
        ToolOutputDeclaration::Single,
        1,
        8,
    )
    .await
    .unwrap()
    .with_flush_bytes(64);
    let line = "nineteen bytes ...\n";
    for _ in 0..10 {
        append(
            &capture,
            &[ToolOutputChunk {
                stream: None,
                text: line.into(),
            }],
        )
        .await
        .unwrap();
    }
    // 190 bytes: a write at 76 and 152 buffered bytes, the rest at finish.
    assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 2);
    let result = capture
        .finish(completion(ToolCompletionOutput::Streamed), false)
        .await
        .unwrap();
    assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 3);
    let stream = &result.retained_output.as_ref().unwrap().streams[0];
    assert_eq!(
        store
            .read_paged(
                crate::test_session_id(),
                stream.sha256.as_ref().unwrap().as_str(),
                0,
                20
            )
            .unwrap()
            .content,
        line.repeat(10)
    );
}

#[tokio::test]
async fn output_that_fits_its_preview_is_stored_nowhere_else() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("artifacts");
    let store = ArtifactRouter::open_flat(directory.clone()).unwrap();
    for declaration in [
        ToolOutputDeclaration::Single,
        ToolOutputDeclaration::Named {
            streams: vec!["stdout".into(), "stderr".into()],
        },
    ] {
        let named = declaration != ToolOutputDeclaration::Single;
        let capture = OutputCapture::new(
            store.clone(),
            crate::test_session_id(),
            declaration,
            10,
            100,
        )
        .await
        .unwrap();
        append(
            &capture,
            &[ToolOutputChunk {
                stream: named.then(|| "stdout".into()),
                text: "Edit applied atomically\n".into(),
            }],
        )
        .await
        .unwrap();
        let result = capture
            .finish(completion(ToolCompletionOutput::Streamed), false)
            .await
            .unwrap();
        let expected = if named {
            "[stdout]\nEdit applied atomically\n\n[stderr]\n"
        } else {
            "Edit applied atomically\n"
        };
        assert_eq!(result.output, expected);
        let retained = result.retained_output.unwrap();
        assert_eq!(retained.reference, None);
        assert!(retained.streams.iter().all(|stream| {
            stream.reference.is_none() && stream.sha256.is_none() && !stream.truncated
        }));
        assert_eq!(retained.streams[0].byte_length, 24);
        assert_eq!(retained.streams[0].line_count, 1);
    }
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
}

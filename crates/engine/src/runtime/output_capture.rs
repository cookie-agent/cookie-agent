use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    MAX_TOOL_DELTA_BYTES, PersistedToolResult, RetainedToolOutput, RetainedToolStream,
    Sha256Digest, ToolCompletionOutput, ToolOutputChunk, ToolOutputDeclaration, ToolOutputManifest,
};
use sha2::{Digest as _, Sha256};

use super::{artifacts::ArtifactStore, blocking_io};
use crate::{ToolCompletion, ToolError, ToolProgress, events::OutputHub};

#[derive(Debug)]
struct Channel {
    name: Option<String>,
    temporary: String,
    file: Mutex<File>,
    hash: Sha256,
    bytes: u64,
    newlines: u64,
    ends_with_newline: bool,
    preview: String,
}

#[derive(Debug)]
struct State {
    channels: Vec<Channel>,
    accepted_deltas: bool,
    finalized: bool,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct Publication {
    released: bool,
    guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
}

#[derive(Debug)]
struct Capture {
    store: Arc<ArtifactStore>,
    declaration: ToolOutputDeclaration,
    max_lines: usize,
    max_bytes: usize,
    state: Mutex<State>,
    operations: Arc<tokio::sync::Semaphore>,
    publication: Mutex<Publication>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for channel in std::mem::take(&mut state.channels) {
            let Channel {
                temporary, file, ..
            } = channel;
            drop(file.into_inner().unwrap_or_else(|p| p.into_inner()));
            self.store.discard_capture(&temporary);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OutputCapture(Arc<Capture>);

impl OutputCapture {
    pub(crate) fn release_publication(&self) {
        let mut publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
        publication.released = true;
        publication.guard.take();
    }

    pub(crate) async fn new(
        store: Arc<ArtifactStore>,
        declaration: ToolOutputDeclaration,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<Self, ToolError> {
        blocking_io::run(move || Self::create(store, declaration, max_lines, max_bytes)).await?
    }

    fn create(
        store: Arc<ArtifactStore>,
        declaration: ToolOutputDeclaration,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<Self, ToolError> {
        declaration.validate().map_err(ToolError::execution)?;
        let names = declaration.channels();
        // Leave room for headings and truthful read hints inside the existing event bound.
        let max_bytes =
            max_bytes.min((PersistedToolResult::MAX_OUTPUT_BYTES - 16 * 1024) / names.len());
        let capture = Self(Arc::new(Capture {
            store,
            declaration,
            max_lines,
            max_bytes,
            state: Mutex::new(State {
                channels: Vec::new(),
                accepted_deltas: false,
                finalized: false,
                error: None,
            }),
            operations: Arc::new(tokio::sync::Semaphore::new(1)),
            publication: Mutex::new(Publication::default()),
        }));
        {
            let mut state = capture.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let id = uuid::Uuid::now_v7();
            for (index, name) in names.into_iter().enumerate() {
                let temporary = format!(".capture-{id}-output{index}.tmp");
                let file = capture
                    .0
                    .store
                    .create_capture_file(&temporary)
                    .map_err(|e| ToolError::execution(e.to_string()))?;
                state.channels.push(Channel {
                    name,
                    temporary,
                    file: Mutex::new(file),
                    hash: Sha256::new(),
                    bytes: 0,
                    newlines: 0,
                    ends_with_newline: false,
                    preview: String::new(),
                });
            }
        }
        Ok(capture)
    }

    pub(crate) async fn emit(
        &self,
        mut progress: ToolProgress,
        output: OutputHub,
        permit: Option<tokio::sync::mpsc::OwnedPermit<ToolProgress>>,
    ) -> Result<(), ToolError> {
        let operation = self
            .0
            .operations
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ToolError::execution("output capture closed"))?;
        let capture = self.clone();
        let writes_output = !progress.output.is_empty();
        let emit = move || {
            // The permit travels with the I/O, not its awaiter. Cancellation cannot
            // release it early and let terminal finalization overtake accepted bytes.
            let _operation = operation;
            capture.append_blocking(&progress.output)?;
            for chunk in &progress.output {
                output.emit(
                    cookie_agent_protocol::OutputStream::from_channel(chunk.stream.as_deref()),
                    chunk.text.as_bytes(),
                );
            }
            progress.output.clear();
            if let Some(permit) = permit {
                permit.send(progress);
            }
            #[cfg(test)]
            if writes_output {
                capture
                    .0
                    .store
                    .io_test_hook
                    .run("capture_delivery", "")
                    .map_err(|error| ToolError::execution(error.to_string()))?;
            }
            Ok(())
        };
        if writes_output {
            blocking_io::run(emit).await?
        } else {
            emit()
        }
    }

    fn append_blocking(&self, chunks: &[ToolOutputChunk]) -> Result<(), ToolError> {
        if chunks.len() > cookie_agent_protocol::MAX_TOOL_STREAMS
            || chunks.iter().map(|chunk| chunk.text.len()).sum::<usize>() > MAX_TOOL_DELTA_BYTES
        {
            return Err(ToolError::resource_limit(
                "tool output delta exceeds 64 KiB or 8 chunks",
            ));
        }
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.finalized {
            return Err(ToolError::operation_changed(
                "tool output already finalized",
            ));
        }
        if let Some(error) = &state.error {
            return Err(ToolError::execution(error.clone()));
        }
        for chunk in chunks {
            if !state
                .channels
                .iter()
                .any(|channel| channel.name == chunk.stream)
            {
                return Err(ToolError::execution(
                    "output chunk references an undeclared stream",
                ));
            }
        }
        if !chunks.is_empty() {
            state.accepted_deltas = true;
        }
        for chunk in chunks {
            let channel = state
                .channels
                .iter_mut()
                .find(|channel| channel.name == chunk.stream)
                .expect("validated channel");
            if let Err(error) = self.append_channel(channel, &chunk.text) {
                state.error = Some(error.to_string());
                return Err(error);
            }
        }
        Ok(())
    }

    fn append_channel(&self, channel: &mut Channel, text: &str) -> Result<(), ToolError> {
        #[cfg(test)]
        self.0
            .store
            .io_test_hook
            .run("capture_write", &channel.temporary)
            .map_err(|e| ToolError::execution(e.to_string()))?;
        let file = channel.file.get_mut().unwrap_or_else(|p| p.into_inner());
        if let Err(error) = file.write_all(text.as_bytes()) {
            let _ = file.set_len(channel.bytes);
            let _ = file.seek(SeekFrom::End(0));
            return Err(ToolError::execution(format!(
                "tool output capture failed: {error}"
            )));
        }
        let preview_lines = usize::try_from(channel.newlines).unwrap_or(usize::MAX);
        if channel.preview.len() as u64 == channel.bytes && preview_lines < self.0.max_lines {
            let mut lines = preview_lines;
            let room = self.0.max_bytes.saturating_sub(channel.preview.len());
            let mut end = 0;
            for (offset, character) in text.char_indices() {
                if lines >= self.0.max_lines || offset + character.len_utf8() > room {
                    break;
                }
                end = offset + character.len_utf8();
                if character == '\n' {
                    lines += 1;
                }
            }
            channel.preview.push_str(&text[..end]);
        }
        channel.hash.update(text.as_bytes());
        channel.bytes = channel.bytes.saturating_add(text.len() as u64);
        channel.newlines = channel
            .newlines
            .saturating_add(text.bytes().filter(|byte| *byte == b'\n').count() as u64);
        if !text.is_empty() {
            channel.ends_with_newline = text.ends_with('\n');
        }
        Ok(())
    }

    pub(crate) async fn finish(
        &self,
        completion: ToolCompletion,
        incomplete: bool,
    ) -> Result<PersistedToolResult, ToolError> {
        let operation = self
            .0
            .operations
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ToolError::execution("output capture closed"))?;
        let publication = self.0.store.publication.clone().read_owned().await;
        let capture = self.clone();
        blocking_io::run(move || {
            let _operation = operation;
            capture.finish_blocking(completion, incomplete, publication)
        })
        .await?
    }

    fn finish_blocking(
        &self,
        mut completion: ToolCompletion,
        incomplete: bool,
        publication: tokio::sync::OwnedRwLockReadGuard<()>,
    ) -> Result<PersistedToolResult, ToolError> {
        normalize_result(&mut completion.result)?;
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.finalized {
            return Err(ToolError::operation_changed(
                "tool output already finalized",
            ));
        }
        if !incomplete && let Some(error) = &state.error {
            return Err(ToolError::execution(error.clone()));
        }
        let output = match completion.output {
            ToolCompletionOutput::Streamed => None,
            ToolCompletionOutput::Single { text }
                if self.0.declaration == ToolOutputDeclaration::Single =>
            {
                Some(vec![ToolOutputChunk { stream: None, text }])
            }
            ToolCompletionOutput::Named { streams }
                if matches!(self.0.declaration, ToolOutputDeclaration::Named { .. }) =>
            {
                Some(streams)
            }
            _ => {
                return Err(ToolError::execution(
                    "completion output does not match the tool declaration",
                ));
            }
        };
        if let Some(chunks) = output {
            if state.accepted_deltas {
                return Err(ToolError::execution(
                    "streamed completion must finalize instead of resupplying output",
                ));
            }
            let names = chunks
                .iter()
                .map(|chunk| &chunk.stream)
                .collect::<std::collections::HashSet<_>>();
            if names.len() != chunks.len()
                || chunks.len() != state.channels.len()
                || state
                    .channels
                    .iter()
                    .any(|channel| !names.contains(&channel.name))
            {
                return Err(ToolError::execution(
                    "terminal output must supply each declared channel exactly once",
                ));
            }
            for chunk in chunks {
                let channel = state
                    .channels
                    .iter_mut()
                    .find(|channel| channel.name == chunk.stream)
                    .expect("validated channel");
                if let Err(error) = self.append_channel(channel, &chunk.text) {
                    state.error = Some(error.to_string());
                    return Err(error);
                }
            }
        }
        state.finalized = true;
        let mut streams = Vec::new();
        for channel in &state.channels {
            let (artifact, _) = self
                .0
                .store
                .commit_capture(&channel.temporary, &channel.file)
                .map_err(|e| ToolError::execution(e.to_string()))?;
            if artifact.sha256 != format!("{:x}", channel.hash.clone().finalize()) {
                return Err(ToolError::execution("captured output digest mismatch"));
            }
            let truncated = channel.preview.len() as u64 != channel.bytes;
            streams.push(RetainedToolStream {
                name: channel.name.clone(),
                reference: artifact.reference,
                sha256: Sha256Digest::new(artifact.sha256)
                    .map_err(|e| ToolError::execution(e.to_string()))?,
                byte_length: channel.bytes,
                line_count: channel.newlines
                    + u64::from(channel.bytes > 0 && !channel.ends_with_newline),
                truncated,
                next_offset: truncated.then(|| {
                    channel
                        .preview
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count() as u64
                }),
            });
        }
        let (reference, digest) = if self.0.declaration == ToolOutputDeclaration::Single {
            (streams[0].reference.clone(), streams[0].sha256.to_string())
        } else {
            let manifest = serde_json::to_vec(&ToolOutputManifest {
                streams: streams.clone(),
            })
            .map_err(|e| ToolError::execution(e.to_string()))?;
            self.0
                .store
                .retain(&manifest)
                .map_err(|e| ToolError::execution(e.to_string()))?
        };
        let mut rendered = String::new();
        for (channel, stream) in state.channels.iter().zip(&streams) {
            if let Some(name) = &channel.name {
                if !rendered.is_empty() {
                    rendered.push_str("\n\n");
                }
                rendered.push_str(&format!("[{name}]\n"));
            }
            rendered.push_str(&channel.preview);
            if let Some(offset) = stream.next_offset {
                let suffix = stream
                    .name
                    .as_ref()
                    .map_or_else(String::new, |name| format!("/{name}"));
                rendered.push_str(&format!("\n[Truncated. Read more: read(filePath=\"artifact://{digest}{suffix}\", offset={offset})]"));
            }
        }
        completion.result.output = rendered;
        completion.result.truncation = None;
        completion.result.retained_output = Some(RetainedToolOutput {
            reference,
            streams,
            incomplete,
        });
        completion
            .result
            .validate()
            .map_err(|e| ToolError::execution(e.to_string()))?;
        let mut pending = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
        if !pending.released {
            pending.guard = Some(publication);
        }
        Ok(completion.result)
    }
}

fn normalize_result(result: &mut PersistedToolResult) -> Result<(), ToolError> {
    result.display = Some(crate::tool_api::bounded_tool_display(
        result.display.as_deref().unwrap_or_default(),
    ));
    result
        .validate()
        .map_err(|e| ToolError::execution(e.to_string()))?;
    if !result.output.is_empty() {
        return Err(ToolError::execution(
            "completion must supply output through its typed completion mode",
        ));
    }
    Ok(())
}

pub(crate) fn finish_page(
    mut completion: ToolCompletion,
) -> Result<PersistedToolResult, ToolError> {
    normalize_result(&mut completion.result)?;
    let ToolCompletionOutput::Single { text } = completion.output else {
        return Err(ToolError::execution(
            "self-paginating tools must return a terminal single page",
        ));
    };
    if text.len() > PersistedToolResult::MAX_OUTPUT_BYTES {
        return Err(ToolError::resource_limit(
            "self-paginating output must be within the 2 MiB result limit",
        ));
    }
    completion.result.output = text;
    completion.result.truncation = None;
    completion.result.retained_output = None;
    completion
        .result
        .validate()
        .map_err(|e| ToolError::execution(e.to_string()))?;
    Ok(completion.result)
}

#[cfg(test)]
mod tests {
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
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(
            store.clone(),
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
                    .read_paged(stream.sha256.as_str(), 0, 10)
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
        let manifest: ToolOutputManifest =
            serde_json::from_str(&store.read_paged(manifest_digest, 0, 1).unwrap().content)
                .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.streams, retained.streams);
    }

    #[tokio::test]
    async fn streamed_completion_rejects_resupply_and_preserves_incomplete_utf8_output() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(store.clone(), ToolOutputDeclaration::Single, 10, 3)
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
                .read_paged(retained.streams[0].sha256.as_str(), 0, 1)
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
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
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
        let capture = OutputCapture::new(store.clone(), ToolOutputDeclaration::Single, 1, 4)
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
                .read_paged(stream.sha256.as_str(), 1, 1)
                .unwrap()
                .content,
            "two\n"
        );
    }

    #[tokio::test]
    async fn output_publication_is_protected_until_terminal_references_are_persisted() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(sessions.join("session")).unwrap();
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(
            store.clone(),
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
                .collect_garbage(&sessions, std::time::Duration::ZERO)
                .unwrap()
                .deleted,
            0
        );
        std::fs::write(
            sessions.join("session/events.jsonl"),
            serde_json::to_vec(&result).unwrap(),
        )
        .unwrap();
        drop(capture);
        let report = store
            .collect_garbage(&sessions, std::time::Duration::ZERO)
            .unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.retained, 3);
    }

    #[tokio::test]
    async fn output_deltas_reject_undeclared_channels_and_oversized_chunks() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(store, ToolOutputDeclaration::Single, 1, 1)
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
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(store, Default::default(), 10, 100)
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
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(store.clone(), Default::default(), 1, 100)
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
                .read_paged(stream.sha256.as_str(), 0, 200)
                .unwrap()
                .content,
            "data\n".repeat(100)
        );
    }

    #[tokio::test]
    async fn progress_backpressure_precedes_acceptance_and_never_drops_output() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(store.clone(), Default::default(), 10, 100)
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
        let store = ArtifactStore::open(directory.clone()).unwrap();
        let capture = OutputCapture::new(store.clone(), Default::default(), 10, 100)
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
                .read_paged(retained.streams[0].sha256.as_str(), 0, 10)
                .unwrap()
                .content,
            "accepted\n"
        );
    }

    #[tokio::test]
    async fn aggregate_previews_stay_within_the_event_bound() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(root.path().join("artifacts")).unwrap();
        let names = (0..cookie_agent_protocol::MAX_TOOL_STREAMS)
            .map(|index| format!("stream{index}"))
            .collect::<Vec<_>>();
        let capture = OutputCapture::new(
            store,
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
        let store = ArtifactStore::open(directory.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(store.clone(), Default::default(), 10, 100)
            .await
            .unwrap();
        let (entered, release) = blocking_io::gate(&store, "capture_write", None);
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
                .read_paged(retained.streams[0].sha256.as_str(), 0, 10)
                .unwrap()
                .content,
            "accepted before cancellation\n"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_finalization_does_not_block_async_workers_or_publication_release() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(directory.path().join("artifacts")).unwrap();
        let capture = OutputCapture::new(store.clone(), Default::default(), 10, 100)
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
        let (entered, release) = blocking_io::gate(&store, "capture_finalize", None);
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
        assert!(store.publication.try_write().is_err());
        release.send(()).unwrap();
        assert_eq!(finishing.await.unwrap().unwrap().output, "complete\n");
        assert!(store.publication.try_write().is_ok());
    }
}

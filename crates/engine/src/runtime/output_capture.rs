use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    MAX_TOOL_DELTA_BYTES, PersistedToolResult, RetainedToolOutput, RetainedToolStream, SessionId,
    Sha256Digest, ToolCompletionOutput, ToolOutputChunk, ToolOutputDeclaration, ToolOutputManifest,
};
use sha2::{Digest as _, Sha256};

use super::{artifacts::ArtifactRouter, blocking_io};
use crate::{ToolCompletion, ToolError, ToolProgress, events::OutputHub};
use cookie_agent_protocol::ToolCallId;

/// Tool output capture state owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct OutputState {
    pub(crate) hubs: Mutex<HashMap<ToolCallId, OutputHub>>,
    pub(crate) captures: Mutex<HashMap<ToolCallId, OutputCapture>>,
    pub(crate) finalized_hubs: Mutex<VecDeque<ToolCallId>>,
}

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
    store: Arc<ArtifactRouter>,
    session: SessionId,
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
            self.store.discard_capture(self.session, &temporary);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OutputCapture(Arc<Capture>);

impl OutputCapture {
    fn session(&self) -> cookie_agent_protocol::SessionId {
        self.0.session
    }

    pub(crate) fn release_publication(&self) {
        let mut publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
        publication.released = true;
        publication.guard.take();
    }

    pub(crate) async fn new(
        store: Arc<ArtifactRouter>,
        session: SessionId,
        declaration: ToolOutputDeclaration,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<Self, ToolError> {
        blocking_io::run(move || Self::create(store, session, declaration, max_lines, max_bytes))
            .await?
    }

    fn create(
        store: Arc<ArtifactRouter>,
        session: SessionId,
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
            session,
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
                    .create_capture_file(session, &temporary)
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
                    .io_test_hook()
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
            .io_test_hook()
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
        let publication = self.0.store.publication().read_owned().await;
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
                .commit_capture(self.session(), &channel.temporary, &channel.file)
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
                .retain(self.session(), &manifest)
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
mod tests;

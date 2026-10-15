use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{Seek, SeekFrom, Write},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    ArtifactReadTarget, MAX_TOOL_DELTA_BYTES, PersistedToolResult, RetainedToolOutput,
    RetainedToolStream, SessionId, Sha256Digest, ToolCompletionOutput, ToolOutputChunk,
    ToolOutputDeclaration, ToolOutputManifest,
};
use sha2::{Digest as _, Sha256};

use super::{artifacts::ArtifactRouter, blocking_io};
use crate::{
    ToolCompletion, ToolError, ToolProgress,
    events::OutputHub,
    preview::{self, PreviewLimits},
};
use cookie_agent_protocol::ToolCallId;

/// Tool output capture state owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct OutputState {
    pub(crate) hubs: Mutex<HashMap<ToolCallId, OutputHub>>,
    pub(crate) captures: Mutex<HashMap<ToolCallId, OutputCapture>>,
    pub(crate) finalized_hubs: Mutex<VecDeque<ToolCallId>>,
}

/// Accepted output waits in memory until a channel holds this much, so a
/// tool streaming small chunks costs one blocking write per mebibyte rather
/// than one per chunk. Unit tests write through, chunk by chunk, unless they
/// ask for batching.
const FLUSH_BYTES: usize = if cfg!(test) { 1 } else { 1024 * 1024 };

/// The most of one stream an artifact keeps from its head. A model reads an
/// artifact 2,000 lines a page, so 128 MiB — over a million typical lines — is
/// more than any reader pages through, while a runaway command (`yes`, a
/// binary dumped to stdout) can no longer fill the disk one call at a time.
const MAX_RETAINED_BYTES: u64 = 128 * 1024 * 1024;

/// How much of the output past the limit is kept, as the artifact's last
/// stretch: the end of a long run is where its errors usually are.
const DROPPED_TAIL_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct Channel {
    name: Option<String>,
    /// Name of the spill file, created only once the stream outgrows its
    /// preview: output the preview holds whole needs no artifact.
    temporary: String,
    file: Option<File>,
    /// Accepted bytes not yet written to `file`; empty while the stream fits
    /// its preview.
    pending: Vec<u8>,
    /// Bytes written to `file`; `hash` covers exactly these.
    written: u64,
    hash: Sha256,
    bytes: u64,
    newlines: u64,
    ends_with_newline: bool,
    preview: String,
    /// The most recent output, at most twice the preview byte limit.
    tail: String,
    /// Bytes headed for the artifact (written or pending), at most the
    /// capture's retain limit until the stream is closed.
    retained: u64,
    retained_ends_with_newline: bool,
    /// Bytes past the retain limit, and the most recent of them.
    overflowed: u64,
    overflow: String,
    /// Overflowed bytes the artifact lost for good, known once closed.
    dropped: u64,
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
    limits: PreviewLimits,
    state: Mutex<State>,
    operations: Arc<tokio::sync::Semaphore>,
    publication: Mutex<Publication>,
    flush_bytes: usize,
    retain_limit: u64,
    dropped_tail: usize,
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
            if let Some(file) = file {
                drop(file);
                self.store.discard_capture(self.session, &temporary);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OutputCapture(Arc<Capture>);

/// A capture together with the hub that publishes its output live: the
/// engine's side of a [`crate::ProgressSink`].
#[derive(Debug)]
struct HubCapture {
    capture: OutputCapture,
    output: OutputHub,
}

#[async_trait::async_trait]
impl cookie_agent_runtime_api::ProgressCapture for HubCapture {
    async fn emit(
        &self,
        progress: ToolProgress,
        display: Option<tokio::sync::mpsc::OwnedPermit<ToolProgress>>,
    ) -> Result<(), ToolError> {
        self.capture
            .emit(progress, self.output.clone(), display)
            .await
    }
}

impl OutputCapture {
    /// A progress sink whose output this capture retains while `output`
    /// publishes it live.
    pub(crate) fn progress_sink(
        &self,
        sender: tokio::sync::mpsc::Sender<ToolProgress>,
        output: OutputHub,
    ) -> crate::ProgressSink {
        crate::ProgressSink::with_capture(
            sender,
            Arc::new(HubCapture {
                capture: self.clone(),
                output,
            }),
        )
    }

    fn session(&self) -> cookie_agent_protocol::SessionId {
        self.0.session
    }

    pub(crate) fn release_publication(&self) {
        let mut publication = self.0.publication.lock().unwrap_or_else(|p| p.into_inner());
        publication.released = true;
        publication.guard.take();
    }

    /// A capture whose previews keep half of the lines from each end.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn new(
        store: Arc<ArtifactRouter>,
        session: SessionId,
        declaration: ToolOutputDeclaration,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<Self, ToolError> {
        Self::with_limits(
            store,
            session,
            declaration,
            PreviewLimits::halves(max_lines, max_bytes),
        )
        .await
    }

    pub(crate) async fn with_limits(
        store: Arc<ArtifactRouter>,
        session: SessionId,
        declaration: ToolOutputDeclaration,
        limits: PreviewLimits,
    ) -> Result<Self, ToolError> {
        Self::create(store, session, declaration, limits)
    }

    fn create(
        store: Arc<ArtifactRouter>,
        session: SessionId,
        declaration: ToolOutputDeclaration,
        limits: PreviewLimits,
    ) -> Result<Self, ToolError> {
        declaration.validate().map_err(ToolError::execution)?;
        let names = declaration.channels();
        // Leave room for headings and truthful read hints inside the existing event bound.
        let limits = limits
            .with_byte_ceiling((PersistedToolResult::MAX_OUTPUT_BYTES - 16 * 1024) / names.len());
        let capture = Self(Arc::new(Capture {
            store,
            session,
            declaration,
            limits,
            state: Mutex::new(State {
                channels: Vec::new(),
                accepted_deltas: false,
                finalized: false,
                error: None,
            }),
            operations: Arc::new(tokio::sync::Semaphore::new(1)),
            publication: Mutex::new(Publication::default()),
            flush_bytes: FLUSH_BYTES,
            retain_limit: MAX_RETAINED_BYTES,
            dropped_tail: DROPPED_TAIL_BYTES,
        }));
        {
            let mut state = capture.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let id = uuid::Uuid::now_v7();
            for (index, name) in names.into_iter().enumerate() {
                state.channels.push(Channel {
                    name,
                    temporary: format!(".capture-{id}-output{index}.tmp"),
                    file: None,
                    pending: Vec::new(),
                    written: 0,
                    hash: Sha256::new(),
                    bytes: 0,
                    newlines: 0,
                    ends_with_newline: false,
                    preview: String::new(),
                    tail: String::new(),
                    retained: 0,
                    retained_ends_with_newline: false,
                    overflowed: 0,
                    overflow: String::new(),
                    dropped: 0,
                });
            }
        }
        Ok(capture)
    }

    /// Batch writes until a channel holds `bytes` (tests only).
    #[cfg(test)]
    pub(crate) fn with_flush_bytes(mut self, bytes: usize) -> Self {
        Arc::get_mut(&mut self.0)
            .expect("a new capture is unshared")
            .flush_bytes = bytes;
        self
    }

    /// Keep at most `limit` bytes of a stream's head and `tail` bytes past
    /// it (tests only).
    #[cfg(test)]
    pub(crate) fn with_retain_limit(mut self, limit: u64, tail: usize) -> Self {
        let capture = Arc::get_mut(&mut self.0).expect("a new capture is unshared");
        capture.retain_limit = limit;
        capture.dropped_tail = tail;
        self
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
        let writes_output = !progress.output.is_empty();
        // Accepting is a memory copy; only a full buffer costs a blocking write.
        let flush = self.append(&progress.output)?;
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
        // Tests also hop for output that needs no write, so a hook can hold
        // the delivery the way a slow write would.
        if !(flush || cfg!(test) && writes_output) {
            return Ok(());
        }
        let capture = self.clone();
        blocking_io::run(move || {
            // The permit travels with the I/O, not its awaiter. Cancellation cannot
            // release it early and let terminal finalization overtake accepted bytes.
            let _operation = operation;
            if flush {
                capture.flush_blocking(capture.0.flush_bytes)?;
            }
            #[cfg(test)]
            capture
                .0
                .store
                .io_test_hook()
                .run("capture_delivery", "")
                .map_err(|error| ToolError::execution(error.to_string()))?;
            Ok(())
        })
        .await?
    }

    /// Accepts `chunks` into memory, reporting whether a channel's buffer is
    /// now due to be written.
    fn append(&self, chunks: &[ToolOutputChunk]) -> Result<bool, ToolError> {
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
            self.append_channel(channel, &chunk.text);
        }
        Ok(state
            .channels
            .iter()
            .any(|channel| channel.pending.len() >= self.0.flush_bytes))
    }

    fn append_channel(&self, channel: &mut Channel, text: &str) {
        let limits = self.0.limits;
        let preview_lines = usize::try_from(channel.newlines).unwrap_or(usize::MAX);
        let was_whole = channel.preview.len() as u64 == channel.bytes;
        let previewed = channel.preview.len();
        if was_whole && preview_lines < limits.max_lines {
            let mut lines = preview_lines;
            let room = limits.max_bytes.saturating_sub(channel.preview.len());
            let mut end = 0;
            for (offset, character) in text.char_indices() {
                if lines >= limits.max_lines || offset + character.len_utf8() > room {
                    break;
                }
                end = offset + character.len_utf8();
                if character == '\n' {
                    lines += 1;
                }
            }
            channel.preview.push_str(&text[..end]);
        }
        channel.tail.push_str(text);
        if channel.tail.len() > 2 * limits.max_bytes {
            let mut start = channel.tail.len() - limits.max_bytes;
            while !channel.tail.is_char_boundary(start) {
                start += 1;
            }
            channel.tail.drain(..start);
        }
        channel.bytes = channel.bytes.saturating_add(text.len() as u64);
        channel.newlines = channel
            .newlines
            .saturating_add(text.bytes().filter(|byte| *byte == b'\n').count() as u64);
        if !text.is_empty() {
            channel.ends_with_newline = text.ends_with('\n');
        }
        if channel.preview.len() as u64 != channel.bytes {
            if was_whole {
                // The first byte past the preview: from here on the stream
                // needs an artifact holding all of it, from its first byte.
                let head = channel.preview[..previewed].to_owned();
                self.spill(channel, &head);
            }
            self.spill(channel, text);
        }
    }

    /// Queues `text` for the artifact up to the retain limit; past it, only
    /// a rolling tail survives until the stream is closed.
    fn spill(&self, channel: &mut Channel, text: &str) {
        let mut keep = if channel.overflowed > 0 {
            0
        } else {
            usize::try_from(self.0.retain_limit.saturating_sub(channel.retained))
                .unwrap_or(usize::MAX)
                .min(text.len())
        };
        while !text.is_char_boundary(keep) {
            keep -= 1;
        }
        if keep > 0 {
            channel.pending.extend_from_slice(&text.as_bytes()[..keep]);
            channel.retained = channel.retained.saturating_add(keep as u64);
            channel.retained_ends_with_newline = text[..keep].ends_with('\n');
        }
        let rest = &text[keep..];
        if rest.is_empty() {
            return;
        }
        channel.overflowed = channel.overflowed.saturating_add(rest.len() as u64);
        channel.overflow.push_str(rest);
        if channel.overflow.len() > 2 * self.0.dropped_tail {
            let mut start = channel.overflow.len() - self.0.dropped_tail;
            while !channel.overflow.is_char_boundary(start) {
                start += 1;
            }
            channel.overflow.drain(..start);
        }
    }

    /// Ends an overflowed stream's artifact: a marker line saying how much
    /// was dropped, then the kept tail from its first whole line.
    fn close_overflow(&self, channel: &mut Channel) {
        if channel.overflowed == 0 {
            return;
        }
        let mut start = channel.overflow.len().saturating_sub(self.0.dropped_tail);
        while !channel.overflow.is_char_boundary(start) {
            start += 1;
        }
        let at_line_start = if start == 0 && channel.overflowed == channel.overflow.len() as u64 {
            channel.retained_ends_with_newline
        } else {
            start > 0 && channel.overflow.as_bytes()[start - 1] == b'\n'
        };
        if !at_line_start
            && let Some(newline) = channel.overflow[start..].find('\n')
            && start + newline + 1 < channel.overflow.len()
        {
            start += newline + 1;
        }
        let tail = &channel.overflow[start..];
        channel.dropped = channel.overflowed - tail.len() as u64;
        let marker = format!(
            "{}[… {} bytes not retained: the output passed the {} capture limit]\n",
            if channel.retained_ends_with_newline {
                ""
            } else {
                "\n"
            },
            channel.dropped,
            byte_size(self.0.retain_limit),
        );
        channel.pending.extend_from_slice(marker.as_bytes());
        channel.pending.extend_from_slice(tail.as_bytes());
        channel.overflow = String::new();
    }

    /// Writes out every channel holding at least `threshold` buffered bytes.
    /// A failure latches: the capture accepts nothing more.
    fn flush_blocking(&self, threshold: usize) -> Result<(), ToolError> {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        let state = &mut *state;
        for channel in &mut state.channels {
            if channel.pending.is_empty() || channel.pending.len() < threshold {
                continue;
            }
            if let Err(error) = self.flush_channel(channel) {
                state.error = Some(error.to_string());
                return Err(error);
            }
        }
        Ok(())
    }

    /// Writes a channel's buffer and folds it into the digest. On failure the
    /// buffer is dropped and the file cut back to what was written before.
    fn flush_channel(&self, channel: &mut Channel) -> Result<(), ToolError> {
        let pending = std::mem::take(&mut channel.pending);
        #[cfg(test)]
        self.0
            .store
            .io_test_hook()
            .run("capture_write", &channel.temporary)
            .map_err(|e| ToolError::execution(e.to_string()))?;
        let file = match &mut channel.file {
            Some(file) => file,
            None => channel.file.insert(
                self.0
                    .store
                    .create_capture_file(self.session(), &channel.temporary)
                    .map_err(|e| ToolError::execution(e.to_string()))?,
            ),
        };
        if let Err(error) = file.write_all(&pending) {
            let _ = file.set_len(channel.written);
            let _ = file.seek(SeekFrom::Start(channel.written));
            return Err(ToolError::execution(format!(
                "tool output capture failed: {error}"
            )));
        }
        channel.hash.update(&pending);
        channel.written = channel.written.saturating_add(pending.len() as u64);
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
                self.append_channel(channel, &chunk.text);
            }
        }
        for channel in &mut state.channels {
            self.close_overflow(channel);
        }
        drop(state);
        if let Err(error) = self.flush_blocking(0)
            && !incomplete
        {
            return Err(error);
        }
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        state.finalized = true;
        let mut streams = Vec::new();
        let mut previews = Vec::new();
        for channel in &state.channels {
            let truncated = channel.preview.len() as u64 != channel.bytes;
            // Output the preview holds whole is stored nowhere else. A spill
            // file can be missing only after a failed write of an incomplete
            // result; the preview is then all that remains.
            let (reference, sha256) = if truncated && channel.file.is_some() {
                let sha256 = format!("{:x}", channel.hash.clone().finalize());
                let reference = self
                    .0
                    .store
                    .commit_capture(self.session(), &channel.temporary, &sha256, channel.written)
                    .map_err(|e| ToolError::execution(e.to_string()))?;
                let sha256 =
                    Sha256Digest::new(sha256).map_err(|e| ToolError::execution(e.to_string()))?;
                (Some(reference), Some(sha256))
            } else {
                (None, None)
            };
            let preview = if truncated {
                preview::split(
                    &channel.preview,
                    &channel.tail,
                    channel.tail.len() as u64 == channel.bytes,
                    channel.bytes,
                    self.0.limits,
                )
            } else {
                (channel.preview.as_str(), "")
            };
            streams.push(RetainedToolStream {
                name: channel.name.clone(),
                reference,
                sha256,
                byte_length: channel.bytes,
                line_count: channel.newlines
                    + u64::from(channel.bytes > 0 && !channel.ends_with_newline),
                truncated,
                next_offset: truncated.then(|| preview::newlines(preview.0)),
                dropped_bytes: channel.dropped,
            });
            previews.push(preview);
        }
        let (reference, digest) = if self.0.declaration == ToolOutputDeclaration::Single {
            (
                streams[0].reference.clone(),
                streams[0].sha256.as_ref().map(ToString::to_string),
            )
        } else if streams.iter().any(|stream| stream.reference.is_some()) {
            let manifest = serde_json::to_vec(&ToolOutputManifest {
                streams: streams.clone(),
            })
            .map_err(|e| ToolError::execution(e.to_string()))?;
            let (reference, digest) = self
                .0
                .store
                .retain(self.session(), &manifest)
                .map_err(|e| ToolError::execution(e.to_string()))?;
            (Some(reference), Some(digest))
        } else {
            (None, None)
        };
        let mut rendered = String::new();
        for ((channel, stream), &(head, tail)) in state.channels.iter().zip(&streams).zip(&previews)
        {
            if let Some(name) = &channel.name {
                // One blank line between sections, however the last one ended.
                if !rendered.is_empty() {
                    rendered.push_str(if rendered.ends_with('\n') {
                        "\n"
                    } else {
                        "\n\n"
                    });
                }
                rendered.push_str(&format!("[{name}]\n"));
            }
            let Some(offset) = stream.next_offset else {
                rendered.push_str(head);
                continue;
            };
            let mut read_more = match (&digest, &stream.reference) {
                (Some(digest), Some(_)) => {
                    ArtifactReadTarget::parse(digest, stream.name.as_deref())
                        .map_err(ToolError::execution)?
                        .read_hint(Some(offset))
                }
                _ => "unavailable, the output could not be stored".to_owned(),
            };
            if channel.dropped > 0 {
                read_more.push_str(&format!(
                    "; {} bytes past the {} capture limit were not retained",
                    channel.dropped,
                    byte_size(self.0.retain_limit)
                ));
            }
            rendered.push_str(&preview::render(
                head,
                &preview::marker(
                    &preview::omitted(channel.newlines, channel.bytes, head, tail),
                    &read_more,
                ),
                tail,
            ));
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

/// `bytes` in whole mebibytes when it is one, else in bytes.
fn byte_size(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes}-byte")
    }
}

fn normalize_result(result: &mut PersistedToolResult) -> Result<(), ToolError> {
    result.display = Some(cookie_agent_runtime_api::bounded_tool_display(
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

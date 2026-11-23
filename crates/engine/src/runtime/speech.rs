//! Voice input: recordings sent in chunks and transcribed by the
//! `[speech]` backends, tried in order (`docs/site/specs/voice-input.md`
//! sections 6 and 7).
//!
//! A recording lives in memory, never in a store: it belongs to the
//! connection that began it, and is dropped when `finish` answers, `cancel`
//! is called, the connection closes, or it waits idle for
//! [`IDLE_EXPIRY`]. The daemon's log gets one line per backend tried, with
//! neither the audio nor the words.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use cookie_agent_config::{SpeechBackend, SpeechConfig};
use cookie_agent_models::transcription::{
    self, TranscriptionError, TranscriptionRequest, TranscriptionTarget,
};
use cookie_agent_protocol::{
    ARTIFACT_CHUNK_BYTES, SessionId, SpeechRecording, SpeechStatus, SpeechTranscribeFinishResult,
    UploadId, media_sniff::recording_media_type,
};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

use crate::runtime::{Engine, EngineError};

/// The most recordings one connection may have in progress or
/// transcribing.
pub const MAX_TRANSCRIPTIONS_PER_CONNECTION: usize = 2;
/// How long a recording may wait for its next chunk or its finish.
pub const IDLE_EXPIRY: Duration = Duration::from_secs(120);

/// Why a transcription request was refused or failed. The server answers
/// each with its registry code.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SpeechError {
    #[error("voice input is off: the configuration has no `[speech]`")]
    Unavailable,
    #[error("a recording of {byte_length} bytes; the limit is {max_bytes}")]
    TooLarge { byte_length: u64, max_bytes: u64 },
    #[error("a recording of type {media_type} cannot be transcribed")]
    UnsupportedMedia { media_type: String },
    #[error("no recording of this connection has that ID")]
    NotFound,
    #[error("a chunk at offset {offset}, but the recording holds {expected_offset} bytes")]
    OffsetMismatch { offset: u64, expected_offset: u64 },
    #[error("the recording began with {byte_length} bytes and would hold {received}")]
    SizeMismatch { byte_length: u64, received: u64 },
    #[error("the recording's bytes hash to {actual}")]
    DigestMismatch { actual: String },
    #[error("the connection already has {max} recordings in progress")]
    TooMany { max: usize },
    #[error("{0}")]
    InvalidRequest(String),
    #[error("transcription failed: {reason}")]
    Failed { reason: String },
    #[error("the transcription was cancelled")]
    Cancelled,
}

impl From<SpeechError> for EngineError {
    fn from(error: SpeechError) -> Self {
        Self::Speech(error)
    }
}

#[derive(Debug, Default)]
pub(crate) struct SpeechState {
    held: Mutex<HashMap<UploadId, Held>>,
}

#[derive(Debug)]
struct Held {
    connection: u64,
    session: Option<SessionId>,
    byte_length: u64,
    media_type: String,
    bytes: Vec<u8>,
    hasher: Sha256,
    touched: Instant,
    /// Set once `finish` takes the bytes: the transcription is running,
    /// and `cancel` abandons it through `cancel`.
    running: bool,
    cancel: CancellationToken,
}

/// The status clients see for `speech`: its limits, or `None` while off.
#[must_use]
pub fn speech_status(speech: Option<&SpeechConfig>) -> Option<SpeechStatus> {
    speech.map(|speech| SpeechStatus {
        max_seconds: speech.seconds_limit(),
        max_bytes: speech.bytes_limit(),
        recording: match speech.recording {
            cookie_agent_config::SpeechRecording::Wav => SpeechRecording::Wav,
            cookie_agent_config::SpeechRecording::Compressed => SpeechRecording::Compressed,
        },
    })
}

/// The bare media type of a declared one: `audio/webm;codecs=opus` is
/// `audio/webm`.
fn bare_media_type(declared: &str) -> String {
    declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

impl Engine {
    /// Puts voice input's limits, or its absence, on the daemon status.
    pub(crate) fn publish_speech_status(&self) {
        let config = self.inner.config.load();
        self.inner
            .status
            .set_speech(speech_status(config.runtime.speech.as_ref()));
    }

    fn speech_held(&self) -> std::sync::MutexGuard<'_, HashMap<UploadId, Held>> {
        self.inner
            .speech
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Begins a recording of `byte_length` bytes of `media_type` on
    /// `connection`.
    pub fn begin_transcription(
        &self,
        connection: u64,
        byte_length: u64,
        media_type: &str,
        session: Option<SessionId>,
    ) -> Result<UploadId, EngineError> {
        let config = self.inner.config.load();
        let speech = config
            .runtime
            .speech
            .as_ref()
            .ok_or(SpeechError::Unavailable)?;
        if byte_length == 0 {
            return Err(
                SpeechError::InvalidRequest("a recording has at least one byte".into()).into(),
            );
        }
        if let Some(max_bytes) = speech.bytes_limit()
            && byte_length > max_bytes
        {
            return Err(SpeechError::TooLarge {
                byte_length,
                max_bytes,
            }
            .into());
        }
        let media_type = bare_media_type(media_type);
        if !media_type.starts_with("audio/") {
            return Err(SpeechError::UnsupportedMedia { media_type }.into());
        }
        let mut held = self.speech_held();
        let now = Instant::now();
        held.retain(|_, recording| {
            recording.running || now.duration_since(recording.touched) < IDLE_EXPIRY
        });
        if held
            .values()
            .filter(|recording| recording.connection == connection)
            .count()
            >= MAX_TRANSCRIPTIONS_PER_CONNECTION
        {
            return Err(SpeechError::TooMany {
                max: MAX_TRANSCRIPTIONS_PER_CONNECTION,
            }
            .into());
        }
        let id = UploadId::new(format!("spk_{}", uuid::Uuid::now_v7().simple()))
            .map_err(|error| SpeechError::InvalidRequest(error.to_string()))?;
        held.insert(
            id.clone(),
            Held {
                connection,
                session,
                byte_length,
                media_type,
                bytes: Vec::new(),
                hasher: Sha256::new(),
                touched: now,
                running: false,
                cancel: CancellationToken::new(),
            },
        );
        Ok(id)
    }

    /// Adds the chunk at `offset` to a recording of `connection`.
    pub fn transcription_chunk(
        &self,
        connection: u64,
        id: &UploadId,
        offset: u64,
        chunk: &[u8],
    ) -> Result<(), EngineError> {
        if chunk.is_empty() || chunk.len() as u64 > ARTIFACT_CHUNK_BYTES {
            return Err(SpeechError::InvalidRequest(format!(
                "a chunk carries 1 to {ARTIFACT_CHUNK_BYTES} bytes"
            ))
            .into());
        }
        let mut held = self.speech_held();
        let recording = held
            .get_mut(id)
            .filter(|recording| recording.connection == connection && !recording.running)
            .ok_or(SpeechError::NotFound)?;
        let received = recording.bytes.len() as u64;
        if offset != received {
            return Err(SpeechError::OffsetMismatch {
                offset,
                expected_offset: received,
            }
            .into());
        }
        let total = received + chunk.len() as u64;
        if total > recording.byte_length {
            return Err(SpeechError::SizeMismatch {
                byte_length: recording.byte_length,
                received: total,
            }
            .into());
        }
        if recording.bytes.capacity() == 0 {
            recording.bytes.reserve_exact(
                usize::try_from(recording.byte_length)
                    .unwrap_or(usize::MAX)
                    .min(64 << 20),
            );
        }
        recording.bytes.extend_from_slice(chunk);
        recording.hasher.update(chunk);
        recording.touched = Instant::now();
        Ok(())
    }

    /// Checks a recording of `connection` against `sha256` and transcribes
    /// it with the first `[speech]` backend that answers.
    pub async fn finish_transcription(
        &self,
        connection: u64,
        id: &UploadId,
        sha256: &str,
    ) -> Result<SpeechTranscribeFinishResult, EngineError> {
        let (bytes, declared, session, cancel) = {
            let mut held = self.speech_held();
            let recording = held
                .get_mut(id)
                .filter(|recording| recording.connection == connection && !recording.running)
                .ok_or(SpeechError::NotFound)?;
            let received = recording.bytes.len() as u64;
            if received != recording.byte_length {
                return Err(SpeechError::SizeMismatch {
                    byte_length: recording.byte_length,
                    received,
                }
                .into());
            }
            let actual = format!("{:x}", recording.hasher.clone().finalize());
            if actual != sha256 {
                held.remove(id);
                return Err(SpeechError::DigestMismatch { actual }.into());
            }
            recording.running = true;
            (
                std::mem::take(&mut recording.bytes),
                recording.media_type.clone(),
                recording.session,
                recording.cancel.clone(),
            )
        };
        let result = self
            .transcribe_recording(&bytes, &declared, session, &cancel)
            .await;
        self.speech_held().remove(id);
        result
    }

    async fn transcribe_recording(
        &self,
        bytes: &[u8],
        declared: &str,
        session: Option<SessionId>,
        cancel: &CancellationToken,
    ) -> Result<SpeechTranscribeFinishResult, EngineError> {
        let Some((media_type, extension)) = recording_media_type(bytes) else {
            return Err(SpeechError::UnsupportedMedia {
                media_type: declared.to_owned(),
            }
            .into());
        };
        let config = self.inner.config.load_full();
        let speech = config
            .runtime
            .speech
            .as_ref()
            .ok_or(SpeechError::Unavailable)?;
        let models = &self.current_runtime().models;
        let filename = format!("voice.{extension}");
        let mut failures = Vec::new();
        for backend in &speech.backends {
            let label = backend.label();
            let target = match (&backend.provider, &backend.endpoint) {
                (Some(provider), _) => models.transcription_target(provider, &backend.headers),
                (None, Some(endpoint)) => TranscriptionTarget::endpoint(
                    endpoint,
                    backend.auth.as_ref(),
                    &config.runtime.headers,
                    &backend.headers,
                ),
                (None, None) => Err(TranscriptionError::NotConfigured(label.clone())),
            };
            let started = Instant::now();
            let outcome = match target {
                Err(error) => Err(error),
                Ok(target) => {
                    let request = TranscriptionRequest {
                        audio: bytes,
                        media_type,
                        filename: &filename,
                        model: &backend.model,
                        language: speech.language.as_deref(),
                        prompt: speech.prompt.as_deref(),
                    };
                    tokio::select! {
                        () = cancel.cancelled() => {
                            log_attempt(backend, &label, bytes.len(), media_type, session, started, "cancelled");
                            return Err(SpeechError::Cancelled.into());
                        }
                        outcome = transcription::transcribe(&target, request, backend.timeout()) => outcome,
                    }
                }
            };
            match outcome {
                Ok(transcript) => {
                    log_attempt(
                        backend,
                        &label,
                        bytes.len(),
                        media_type,
                        session,
                        started,
                        "ok",
                    );
                    return Ok(SpeechTranscribeFinishResult {
                        text: transcript.text,
                        backend: label,
                        duration_ms: u64::try_from(transcript.elapsed.as_millis())
                            .unwrap_or(u64::MAX),
                    });
                }
                Err(error) => {
                    log_attempt(
                        backend,
                        &label,
                        bytes.len(),
                        media_type,
                        session,
                        started,
                        &error.to_string(),
                    );
                    failures.push(format!("{label}: {error}"));
                }
            }
        }
        Err(SpeechError::Failed {
            reason: failures.join(" · "),
        }
        .into())
    }

    /// Drops a recording of `connection`, or abandons its transcription.
    pub fn cancel_transcription(&self, connection: u64, id: &UploadId) -> Result<(), EngineError> {
        let mut held = self.speech_held();
        let recording = held
            .get(id)
            .filter(|recording| recording.connection == connection)
            .ok_or(SpeechError::NotFound)?;
        recording.cancel.cancel();
        held.remove(id);
        Ok(())
    }

    /// Drops every recording `connection` has, when it closes.
    pub fn drop_connection_transcriptions(&self, connection: u64) {
        self.speech_held().retain(|_, recording| {
            if recording.connection == connection {
                recording.cancel.cancel();
                false
            } else {
                true
            }
        });
    }
}

fn log_attempt(
    backend: &SpeechBackend,
    label: &str,
    byte_length: usize,
    media_type: &str,
    session: Option<SessionId>,
    started: Instant,
    outcome: &str,
) {
    let session = session
        .map(|id| format!(" session {id}"))
        .unwrap_or_default();
    eprintln!(
        "cookie-agent: speech: {label} ({}) {byte_length} bytes {media_type}{session} in {} ms: {outcome}",
        backend.model,
        started.elapsed().as_millis()
    );
}

#[cfg(test)]
mod tests;

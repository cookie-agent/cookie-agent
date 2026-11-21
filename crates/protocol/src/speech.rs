//! Voice input: a recording sent in chunks, as an upload is, and
//! transcribed by the daemon's `[speech]` backends
//! (`docs/site/specs/voice-input.md` section 7; client protocol 1.98.0).
//!
//! The recording is never stored: the daemon holds it until `finish`
//! answers or `cancel` drops it, and a connection's transcriptions end with
//! it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ARTIFACT_CHUNK_BYTES, SessionId, UploadId};

pub const SPEECH_TRANSCRIBE_BEGIN_METHOD: &str = "speech.transcribe.begin";
pub const SPEECH_TRANSCRIBE_CHUNK_METHOD: &str = "speech.transcribe.chunk";
pub const SPEECH_TRANSCRIBE_FINISH_METHOD: &str = "speech.transcribe.finish";
pub const SPEECH_TRANSCRIBE_CANCEL_METHOD: &str = "speech.transcribe.cancel";

/// The longest base64 text of [`ARTIFACT_CHUNK_BYTES`] bytes.
const MAX_CHUNK_BASE64_LEN: u64 = ARTIFACT_CHUNK_BYTES.div_ceil(3) * 4;

/// Voice input's limits, in [`crate::DaemonStatus::speech`] while the
/// daemon has `[speech]` backends.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SpeechStatus {
    /// The longest recording a client makes, in seconds; absent: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<u64>,
    /// The largest recording the daemon takes, in bytes; absent: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// `speech.transcribe.begin`: a recording of `byte_length` bytes follows.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscribeBeginParams {
    /// The recording's size; at most [`SpeechStatus::max_bytes`].
    #[schemars(range(min = 1))]
    pub byte_length: u64,
    /// The recorder's media type, such as `audio/webm;codecs=opus`; an
    /// `audio/` type.
    #[schemars(length(min = 1, max = 255))]
    pub media_type: String,
    /// The session dictated into, for the daemon's log only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SpeechTranscribeBeginResult {
    pub transcription_id: UploadId,
    /// The most decoded bytes one chunk may carry.
    pub chunk_bytes: u64,
}

/// `speech.transcribe.chunk`: the bytes at `offset`, which must be exactly
/// the bytes received so far; send a chunk once the previous one is
/// answered.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscribeChunkParams {
    pub transcription_id: UploadId,
    pub offset: u64,
    /// Base64 (standard alphabet, padded) of at most `chunk_bytes` bytes.
    #[schemars(length(min = 1, max = MAX_CHUNK_BASE64_LEN))]
    pub data: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SpeechTranscribeChunkResult {}

/// `speech.transcribe.finish`: checks the digest and transcribes, trying
/// each backend in order; it answers once one does, or all failed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscribeFinishParams {
    pub transcription_id: UploadId,
    /// Lowercase hex SHA-256 of every byte sent.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SpeechTranscribeFinishResult {
    /// The words, trimmed; empty when the backend heard none.
    pub text: String,
    /// The backend that answered, as the daemon's log names it.
    pub backend: String,
    /// How long that backend's request took, in milliseconds.
    pub duration_ms: u64,
}

/// `speech.transcribe.cancel`: drops a recording, or abandons its
/// transcription while `finish` waits.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechTranscribeCancelParams {
    pub transcription_id: UploadId,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SpeechTranscribeCancelResult {}

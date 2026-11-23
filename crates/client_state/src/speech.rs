//! Voice input on the client side (`docs/site/specs/voice-input.md`
//! sections 3, 4, and 7): sending a recording to be transcribed, the
//! recorder's format, and the words a dictation adds to a draft.

use cookie_agent_protocol::{
    ARTIFACT_CHUNK_BYTES, SessionId, Sha256Digest, SpeechStatus, SpeechTranscribeBeginParams,
    SpeechTranscribeCancelParams, SpeechTranscribeChunkParams, SpeechTranscribeFinishParams,
    SpeechTranscribeFinishResult, UploadId, errors,
};
use cookie_agent_protocol_transport::{Client, ClientError};

/// The containers a recorder may make, best first: Opus in WebM (Chrome,
/// Firefox), Opus in Ogg, then MP4 audio (Safari).
pub const RECORDER_TYPES: &[&str] = &[
    "audio/webm;codecs=opus",
    "audio/webm",
    "audio/ogg;codecs=opus",
    "audio/mp4",
];

/// The first of [`RECORDER_TYPES`] the browser `supports`; `None` leaves
/// the choice to it.
pub fn recorder_type(supports: impl Fn(&str) -> bool) -> Option<&'static str> {
    RECORDER_TYPES.iter().copied().find(|kind| supports(kind))
}

/// The rate a WAV recording is made at: what speech models listen at.
pub const WAV_SAMPLE_RATE: u32 = 16_000;

/// `samples` at `from` Hz, at `to` Hz: each output sample averages the
/// input it covers when going down (a crude low-pass, enough for speech),
/// and interpolates between neighbours when going up.
#[must_use]
pub fn resample(samples: &[f32], from: f64, to: u32) -> Vec<f32> {
    if samples.is_empty() || from <= 0.0 || to == 0 {
        return Vec::new();
    }
    let ratio = from / f64::from(to);
    if (ratio - 1.0).abs() < 1e-9 {
        return samples.to_vec();
    }
    let count = (samples.len() as f64 / ratio).floor() as usize;
    (0..count)
        .map(|index| {
            let start = index as f64 * ratio;
            if ratio > 1.0 {
                let first = start as usize;
                let last = (((index + 1) as f64 * ratio) as usize).clamp(first + 1, samples.len());
                let window = &samples[first..last];
                window.iter().sum::<f32>() / window.len() as f32
            } else {
                let first = start as usize;
                let fraction = (start - first as f64) as f32;
                let a = samples[first];
                let b = samples.get(first + 1).copied().unwrap_or(a);
                a + (b - a) * fraction
            }
        })
        .collect()
}

/// `samples` (-1 to 1, mono, at `rate` Hz) as a 16-bit PCM WAV file.
#[must_use]
pub fn encode_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_bytes = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX - 36);
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes a second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes a frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits a sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        wav.extend_from_slice(&value.to_le_bytes());
    }
    wav
}

/// Sends `bytes`, a recording of `media_type`, and waits for its words.
/// `begun` gets the transcription's ID as soon as it has one, so the
/// caller can cancel it.
pub async fn transcribe(
    client: &Client,
    bytes: &[u8],
    media_type: &str,
    session_id: Option<SessionId>,
    begun: impl FnOnce(UploadId),
) -> Result<SpeechTranscribeFinishResult, ClientError> {
    let started = client
        .begin_transcription(SpeechTranscribeBeginParams {
            byte_length: bytes.len() as u64,
            media_type: media_type.to_owned(),
            session_id,
        })
        .await?;
    let id = started.transcription_id;
    begun(id.clone());
    let chunk =
        usize::try_from(started.chunk_bytes.clamp(1, ARTIFACT_CHUNK_BYTES)).unwrap_or(usize::MAX);
    let mut offset = 0;
    for piece in bytes.chunks(chunk) {
        use base64::Engine as _;
        let sent = client
            .transcription_chunk(SpeechTranscribeChunkParams {
                transcription_id: id.clone(),
                offset: offset as u64,
                data: base64::engine::general_purpose::STANDARD.encode(piece),
            })
            .await;
        if let Err(error) = sent {
            cancel(client, id).await;
            return Err(error);
        }
        offset += piece.len();
    }
    client
        .finish_transcription(SpeechTranscribeFinishParams {
            transcription_id: id,
            sha256: Sha256Digest::of_bytes(bytes).to_string(),
        })
        .await
}

/// Drops a recording, or abandons its transcription; a recording already
/// gone is no error.
pub async fn cancel(client: &Client, transcription_id: UploadId) {
    let _ = client
        .cancel_transcription(SpeechTranscribeCancelParams { transcription_id })
        .await;
}

/// Whether `error` is a cancellation the user asked for, which says
/// nothing.
#[must_use]
pub fn cancelled(error: &ClientError) -> bool {
    error.is(&errors::SPEECH_CANCELLED)
}

/// What the recording bar says when a transcription fails.
#[must_use]
pub fn failure_text(error: &ClientError) -> String {
    if error.is(&errors::SPEECH_FAILED) {
        let reason = error.error_data().and_then(|data| {
            data.fields
                .get("reason")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        });
        if let Some(reason) = reason {
            return reason;
        }
    }
    if error.is(&errors::SPEECH_UNAVAILABLE) {
        return "voice input was turned off on the daemon".to_owned();
    }
    error.to_string()
}

/// `draft` with `words` dictated at its end, a space between them.
#[must_use]
pub fn add_words(draft: &str, words: &str) -> String {
    let words = words.trim();
    if words.is_empty() {
        return draft.to_owned();
    }
    if draft.is_empty() || draft.ends_with(char::is_whitespace) {
        format!("{draft}{words}")
    } else {
        format!("{draft} {words}")
    }
}

/// A recording's length as the bar shows it, `m:ss`.
#[must_use]
pub fn clock(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The recording limit, if any, the bar shows after the time.
#[must_use]
pub fn limit_text(status: &SpeechStatus) -> Option<String> {
    status
        .max_seconds
        .map(|seconds| format!("/ {}", clock(seconds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictated_words_join_the_draft_with_one_space() {
        assert_eq!(add_words("", "  add a test "), "add a test");
        assert_eq!(
            add_words("Fix it", "and run the check."),
            "Fix it and run the check."
        );
        assert_eq!(add_words("Fix it\n", "then push"), "Fix it\nthen push");
        assert_eq!(add_words("Fix it ", "then push"), "Fix it then push");
        assert_eq!(add_words("Fix it", "   "), "Fix it");
    }

    #[test]
    fn the_clock_shows_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(59), "0:59");
        assert_eq!(clock(60), "1:00");
        assert_eq!(clock(605), "10:05");
        assert_eq!(
            limit_text(&SpeechStatus {
                max_seconds: Some(300),
                ..SpeechStatus::default()
            })
            .as_deref(),
            Some("/ 5:00")
        );
        assert_eq!(limit_text(&SpeechStatus::default()), None);
    }

    #[test]
    fn the_recorder_takes_the_first_type_the_browser_supports() {
        assert_eq!(recorder_type(|_| true), Some("audio/webm;codecs=opus"));
        assert_eq!(
            recorder_type(|kind| kind.starts_with("audio/mp4")),
            Some("audio/mp4")
        );
        assert_eq!(recorder_type(|_| false), None);
    }
}

//! History format conversion (`docs/site/specs/compatibility-v1.md`
//! section 8.7).
//!
//! A log written under an older history format is converted to
//! [`HISTORY_FORMAT`] when a newer build opens it. Each step is a pure
//! function from the old log's JSON lines to the next format's lines, in
//! `v<N>_to_v<N+1>.rs`, registered in [`CONVERTERS`] and kept, so the steps
//! chain: a format-1 log opened by a format-3 build converts 1 to 2 to 3.
//! Converters never use old Rust types; they read and write JSON.
//!
//! A log without a `history_format` predates history formats and is not
//! converted.
//!
//! # Adding the first real converter
//!
//! Format 1 is the format the protocols froze with. When a change cannot
//! follow the additive rules of section 8.4, the history format bumps to 2
//! and this module gains `v1_to_v2.rs`:
//!
//! 1. Bump [`HISTORY_FORMAT`] in `cookie_agent_protocol` to 2.
//! 2. Write `pub(super) fn convert(lines: Vec<Value>) -> Result<Vec<Value>,
//!    String>` in `v1_to_v2.rs`: it reads format-1 lines as JSON (envelopes
//!    and payloads exactly as a format-1 writer stored them, the storage
//!    encodings included, such as a tool result's `display_span`,
//!    `text_span`, and `lines_span`) and returns format-2 lines. It names no
//!    Rust event type, since those are format 2's; a line it does not
//!    understand (an unknown or newer record) is copied unchanged. The chain
//!    stamps `history_format: 2` on `session_created` afterwards.
//! 3. Declare `mod v1_to_v2;` and register `Converter { from: 1, convert:
//!    v1_to_v2::convert }` in [`CONVERTERS`]. Converters are never removed:
//!    a format-1 log opened by a format-3 build converts 1 to 2 to 3.
//! 4. Record new golden trees in `crates/engine/tests/fixtures/history/2/`
//!    with the golden recorder, beside `history/1/`, which stays frozen: its
//!    test then opens each format-1 tree through the converter and compares
//!    the result with the snapshot it was recorded with, wherever format 2
//!    does not change what the snapshot shows (where it does, the test
//!    compares the converted tree with what format 2 defines instead).
//! 5. Describe the format in `docs/site/reference/events.md` (History
//!    formats) and the converter in the compatibility spec's notes.
//!
//! The test-only step from format 0 ([`TEST_CONVERTERS`]) shows the shape of
//! a step and exercises the chain, the tree conversion, and the backups.

use cookie_agent_protocol::HISTORY_FORMAT;
use serde_json::Value;

#[cfg(test)]
mod v0_to_v1;

/// One conversion step, from format `from` to `from + 1`.
pub(crate) struct Converter {
    pub(crate) from: u32,
    /// The log's lines in format `from + 1`. The chain stamps the new
    /// format on `session_created` afterwards.
    pub(crate) convert: fn(Vec<Value>) -> Result<Vec<Value>, String>,
}

/// Every conversion step this build has, by the format it reads.
const CONVERTERS: &[Converter] = &[];

/// Steps only tests use: format 0 exists only to exercise the machinery
/// before the first real converter does.
#[cfg(test)]
const TEST_CONVERTERS: &[Converter] = &[Converter {
    from: 0,
    convert: v0_to_v1::convert,
}];
#[cfg(not(test))]
const TEST_CONVERTERS: &[Converter] = &[];

fn converter(from: u32) -> Option<&'static Converter> {
    CONVERTERS
        .iter()
        .chain(TEST_CONVERTERS)
        .find(|converter| converter.from == from)
}

/// Why a log could not be converted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConvertError {
    #[error("no conversion from history format {0} exists")]
    NoPath(u32),
    #[error("converting history format {from} to {} failed: {message}", from + 1)]
    Failed { from: u32, message: String },
}

/// The history format `session_created`, the first line, declares.
pub(crate) fn history_format(first: &Value) -> Option<u32> {
    first
        .get("payload")
        .filter(|payload| payload.get("type").and_then(Value::as_str) == Some("session_created"))?
        .get("history_format")
        .and_then(Value::as_u64)
        .map(|format| u32::try_from(format).unwrap_or(u32::MAX))
}

/// Whether a log in `format` can be converted to this build's.
pub(crate) fn convertible(format: u32) -> bool {
    format < HISTORY_FORMAT && (format..HISTORY_FORMAT).all(|from| converter(from).is_some())
}

/// Converts a log's lines from `from` to [`HISTORY_FORMAT`], one step at a
/// time.
pub(crate) fn convert(from: u32, mut lines: Vec<Value>) -> Result<Vec<Value>, ConvertError> {
    for format in from..HISTORY_FORMAT {
        let step = converter(format).ok_or(ConvertError::NoPath(format))?;
        lines = (step.convert)(lines).map_err(|message| ConvertError::Failed {
            from: format,
            message,
        })?;
        let creation = lines
            .first_mut()
            .and_then(|line| line.get_mut("payload"))
            .filter(|payload| {
                payload.get("type").and_then(Value::as_str) == Some("session_created")
            })
            .ok_or_else(|| ConvertError::Failed {
                from: format,
                message: "the converted log does not start with session_created".into(),
            })?;
        creation["history_format"] = Value::from(format + 1);
    }
    Ok(lines)
}

/// Converts the complete lines of a log in `from` to [`HISTORY_FORMAT`] and
/// returns the new log's bytes. A line that is not JSON is left out; the
/// original keeps it.
pub(crate) fn convert_bytes(from: u32, bytes: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect::<Vec<_>>();
    let mut converted = Vec::with_capacity(bytes.len());
    for line in convert(from, lines)? {
        serde_json::to_writer(&mut converted, &line).map_err(|error| ConvertError::Failed {
            from,
            message: error.to_string(),
        })?;
        converted.push(b'\n');
    }
    Ok(converted)
}

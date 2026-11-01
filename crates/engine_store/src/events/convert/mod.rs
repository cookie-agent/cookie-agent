//! History format conversion (`docs/site/specs/compatibility-v1.md`
//! section 8.7).
//!
//! A log written under an older history format is converted to
//! [`HISTORY_FORMAT`] when a newer build opens it. Each step is a function
//! from the old log's JSON lines to the next format's lines, in
//! `v<N>_to_v<N+1>.rs`, registered in `CONVERTERS` and kept, so the steps
//! chain: a format-1 log opened by a format-3 build converts 1 to 2 to 3.
//! Converters never use old Rust types; they read and write JSON. A step
//! may move values out of the log into the tree's blobs ([`Blobs`], by
//! digest): the caller writes them to `blobs/` before the converted log
//! (`convert_tree`), or keeps them in memory for a read-only open.
//!
//! A log without a `history_format` predates history formats and is not
//! converted.
//!
//! # Adding a converter
//!
//! When a change cannot follow the additive rules of section 8.4, the
//! history format bumps from `N` to `N + 1` and this module gains
//! `v<N>_to_v<N+1>.rs`, as `v1_to_v2.rs` did for format 2:
//!
//! 1. Bump [`HISTORY_FORMAT`] in `cookie_agent_protocol`.
//! 2. Write `pub(super) fn convert(lines: Vec<Value>, blobs: &mut Blobs)
//!    -> Result<Vec<Value>, String>` in `v<N>_to_v<N+1>.rs`: it reads
//!    format-`N` lines as JSON (envelopes and payloads exactly as a
//!    format-`N` writer stored them, the storage encodings included, such as
//!    a tool result's `display_span`, `text_span`, and `lines_span`, and
//!    format 2's blob references) and returns format-`N + 1` lines. It names
//!    no Rust event type, since those are the newest format's; a line it
//!    does not understand (an unknown or newer record) is copied unchanged.
//!    The chain stamps the new `history_format` on `session_created`
//!    afterwards.
//! 3. Declare the module and register `Converter { from: N, convert }` in
//!    `CONVERTERS`. Converters are never removed.
//! 4. Record new golden trees in `crates/engine/tests/fixtures/history/<N +
//!    1>/` with the golden recorder, beside the older formats' trees, which
//!    stay frozen: their test then opens each older tree through the
//!    converters and compares the result with the snapshot it was recorded
//!    with, wherever the new format does not change what the snapshot shows
//!    (where it does, the test compares the converted tree with what the new
//!    format defines instead).
//! 5. Describe the format in `docs/site/reference/events.md` (History
//!    formats) and the converter in the compatibility spec's notes.
//!
//! The test-only step from format 0 (`TEST_CONVERTERS`) exercises the
//! chain, the tree conversion, and the backups apart from any real format.

use cookie_agent_protocol::HISTORY_FORMAT;
use serde_json::Value;

pub use super::blobs::Blobs;

#[cfg(any(test, feature = "test-support"))]
mod v0_to_v1;
mod v1_to_v2;

/// One conversion step, from format `from` to `from + 1`.
pub struct Converter {
    pub from: u32,
    /// The log's lines in format `from + 1`, with the values it moved to
    /// blobs added to the blobs. The chain stamps the new format on
    /// `session_created` afterwards.
    pub convert: fn(Vec<Value>, &mut Blobs) -> Result<Vec<Value>, String>,
}

/// Every conversion step this build has, by the format it reads.
const CONVERTERS: &[Converter] = &[Converter {
    from: 1,
    convert: v1_to_v2::convert,
}];

/// Steps only tests use: format 0 exists only to exercise the machinery
/// before the first real converter does.
#[cfg(any(test, feature = "test-support"))]
const TEST_CONVERTERS: &[Converter] = &[Converter {
    from: 0,
    convert: v0_to_v1::convert,
}];
#[cfg(not(any(test, feature = "test-support")))]
const TEST_CONVERTERS: &[Converter] = &[];

fn converter(from: u32) -> Option<&'static Converter> {
    CONVERTERS
        .iter()
        .chain(TEST_CONVERTERS)
        .find(|converter| converter.from == from)
}

/// Why a log could not be converted.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("no conversion from history format {0} exists")]
    NoPath(u32),
    #[error("converting history format {from} to {} failed: {message}", from + 1)]
    Failed { from: u32, message: String },
}

/// The history format `session_created`, the first line, declares.
pub fn history_format(first: &Value) -> Option<u32> {
    first
        .get("payload")
        .filter(|payload| payload.get("type").and_then(Value::as_str) == Some("session_created"))?
        .get("history_format")
        .and_then(Value::as_u64)
        .map(|format| u32::try_from(format).unwrap_or(u32::MAX))
}

/// Whether a log in `format` can be converted to this build's.
pub fn convertible(format: u32) -> bool {
    format < HISTORY_FORMAT && (format..HISTORY_FORMAT).all(|from| converter(from).is_some())
}

/// Converts a log's lines from `from` to [`HISTORY_FORMAT`], one step at a
/// time, returning them with the blobs the steps moved values to.
pub fn convert(from: u32, mut lines: Vec<Value>) -> Result<(Vec<Value>, Blobs), ConvertError> {
    let mut blobs = Blobs::new();
    for format in from..HISTORY_FORMAT {
        let step = converter(format).ok_or(ConvertError::NoPath(format))?;
        lines = (step.convert)(lines, &mut blobs).map_err(|message| ConvertError::Failed {
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
    Ok((lines, blobs))
}

/// Converts the complete lines of a log in `from` to [`HISTORY_FORMAT`] and
/// returns the new log's bytes, with the blobs it names that the
/// conversion moved values to. A line that is not JSON is left out; the
/// original keeps it.
pub fn convert_bytes(from: u32, bytes: &[u8]) -> Result<(Vec<u8>, Blobs), ConvertError> {
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect::<Vec<_>>();
    let (lines, blobs) = convert(from, lines)?;
    let mut converted = Vec::with_capacity(bytes.len());
    for line in lines {
        serde_json::to_writer(&mut converted, &line).map_err(|error| ConvertError::Failed {
            from,
            message: error.to_string(),
        })?;
        converted.push(b'\n');
    }
    Ok((converted, blobs))
}

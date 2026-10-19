//! How a session log stores a tool result's display
//! (`docs/site/specs/compatibility-v1.md` section 12.3).
//!
//! Most of a result's display repeats its `output`: a shell's streams, a
//! read's lines, a loaded skill. The log writes such text as a byte range of
//! `output` instead of a second copy, and decoding expands it, so the
//! encoding never leaves the engine: clients, plugins, and runtimes receive
//! whole blocks. It applies only while serializing inside
//! [`crate::write_stored_history`].
//!
//! - A display that is a single whole `text` block found in `output` is the
//!   result's `display_span: [start, end)` in place of `display`.
//! - Otherwise, in each block of `display`: the `text` of a `text`,
//!   `markdown`, or `code` block, and of a `terminal` segment, may be
//!   `text_span: [start, end)`; the `lines` of a `file_excerpt` may be
//!   `lines_span: [start, end)`, a range of `output` holding the lines one
//!   per line, each after its file line number and `": "` (as `read`
//!   numbers them).
//!
//! The writer uses a span only when decoding it gives back the same text,
//! and never in a block of a kind it does not know.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::{DisplayBlock, Open};

/// Texts shorter than this stay inline: a span would not be smaller.
const MIN_SPAN_BYTES: usize = 32;

const TEXT: &str = "text";
const TEXT_SPAN: &str = "text_span";
const LINES: &str = "lines";
const LINES_SPAN: &str = "lines_span";

/// Why a stored display does not expand.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum StoredDisplayError {
    #[error("a display span is not a range of output")]
    NotARange,
    #[error("a display block has both a text and its span")]
    Ambiguous,
    #[error("a file excerpt's lines span does not hold its numbered lines")]
    NotNumbered,
}

/// The stored form of `display`, whose text repeats parts of `output`.
pub(crate) fn encode(
    output: &str,
    display: &[Open<DisplayBlock>],
) -> Result<Vec<Value>, serde_json::Error> {
    // A stream's segments occur in `output` in order, so each search starts
    // where the stream's previous one ended.
    let mut stream_cursors = HashMap::<String, usize>::new();
    display
        .iter()
        .map(|block| {
            let mut value = serde_json::to_value(block)?;
            if let (Open::Known(block), Value::Object(object)) = (block, &mut value) {
                encode_block(output, block, object, &mut stream_cursors);
            }
            Ok(value)
        })
        .collect()
}

fn encode_block(
    output: &str,
    block: &DisplayBlock,
    object: &mut Map<String, Value>,
    stream_cursors: &mut HashMap<String, usize>,
) {
    match block {
        DisplayBlock::Text { text, .. }
        | DisplayBlock::Markdown { text, .. }
        | DisplayBlock::Code { text, .. } => {
            if let Some(span) = span_of(output, text, 0) {
                replace_with_span(object, TEXT, TEXT_SPAN, span);
            }
        }
        DisplayBlock::Terminal { segments, .. } => {
            let Some(Value::Array(stored)) = object.get_mut("segments") else {
                return;
            };
            for (segment, stored) in segments.iter().zip(stored) {
                let Value::Object(stored) = stored else {
                    continue;
                };
                let stream = stored
                    .get("stream")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let cursor = stream_cursors.entry(stream).or_default();
                if let Some(span) = span_of(output, &segment.text, *cursor) {
                    *cursor = span[1];
                    replace_with_span(stored, TEXT, TEXT_SPAN, span);
                }
            }
        }
        DisplayBlock::FileExcerpt {
            start_line, lines, ..
        } => {
            if lines.is_empty() || lines.iter().any(|line| line.contains('\n')) {
                return;
            }
            let numbered = numbered_lines(*start_line, lines);
            if let Some(span) = span_of(output, &numbered, 0) {
                replace_with_span(object, LINES, LINES_SPAN, span);
            }
        }
        DisplayBlock::Diff(_)
        | DisplayBlock::KeyValue { .. }
        | DisplayBlock::Table { .. }
        | DisplayBlock::Json { .. }
        | DisplayBlock::Media { .. }
        | DisplayBlock::Link { .. }
        | DisplayBlock::SessionRef { .. } => {}
    }
}

fn numbered_lines(start_line: u64, lines: &[String]) -> String {
    let mut numbered = String::new();
    for (number, line) in (start_line..).zip(lines) {
        if !numbered.is_empty() {
            numbered.push('\n');
        }
        numbered.push_str(&number.to_string());
        numbered.push_str(": ");
        numbered.push_str(line);
    }
    numbered
}

fn span_of(output: &str, text: &str, from: usize) -> Option<[usize; 2]> {
    if text.len() < MIN_SPAN_BYTES {
        return None;
    }
    let start = from + output.get(from..)?.find(text)?;
    Some([start, start + text.len()])
}

fn replace_with_span(object: &mut Map<String, Value>, field: &str, member: &str, span: [usize; 2]) {
    object.remove(field);
    object.insert(member.to_owned(), Value::from(span.to_vec()));
}

/// Expands the spans of a stored `display` in place.
pub(crate) fn decode(output: &str, display: &mut [Value]) -> Result<(), StoredDisplayError> {
    for block in display {
        let Value::Object(object) = block else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("text" | "markdown" | "code") => expand_text(output, object)?,
            Some("terminal") => {
                if let Some(Value::Array(segments)) = object.get_mut("segments") {
                    for segment in segments {
                        if let Value::Object(segment) = segment {
                            expand_text(output, segment)?;
                        }
                    }
                }
            }
            Some("file_excerpt") => expand_lines(output, object)?,
            _ => {}
        }
    }
    Ok(())
}

/// `output[start..end]` for a stored `[start, end)`.
pub(crate) fn slice<'a>(output: &'a str, span: &Value) -> Result<&'a str, StoredDisplayError> {
    let bound = |index: usize| {
        span.get(index)
            .and_then(Value::as_u64)
            .and_then(|bound| usize::try_from(bound).ok())
    };
    match (span.as_array().map(Vec::len), bound(0), bound(1)) {
        (Some(2), Some(start), Some(end)) => {
            output.get(start..end).ok_or(StoredDisplayError::NotARange)
        }
        _ => Err(StoredDisplayError::NotARange),
    }
}

fn expand_text(output: &str, object: &mut Map<String, Value>) -> Result<(), StoredDisplayError> {
    let Some(span) = object.remove(TEXT_SPAN) else {
        return Ok(());
    };
    if object.contains_key(TEXT) {
        return Err(StoredDisplayError::Ambiguous);
    }
    let text = slice(output, &span)?;
    object.insert(TEXT.to_owned(), Value::from(text));
    Ok(())
}

fn expand_lines(output: &str, object: &mut Map<String, Value>) -> Result<(), StoredDisplayError> {
    let Some(span) = object.remove(LINES_SPAN) else {
        return Ok(());
    };
    if object.contains_key(LINES) {
        return Err(StoredDisplayError::Ambiguous);
    }
    let start_line = object
        .get("start_line")
        .and_then(Value::as_u64)
        .ok_or(StoredDisplayError::NotNumbered)?;
    let lines = (start_line..)
        .zip(slice(output, &span)?.split('\n'))
        .map(|(number, line)| {
            line.strip_prefix(number.to_string().as_str())
                .and_then(|line| line.strip_prefix(": "))
                .map(Value::from)
                .ok_or(StoredDisplayError::NotNumbered)
        })
        .collect::<Result<Vec<_>, _>>()?;
    object.insert(LINES.to_owned(), Value::Array(lines));
    Ok(())
}

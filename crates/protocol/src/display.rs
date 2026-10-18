//! Structured tool display, as specified in
//! `docs/site/specs/compatibility-v1.md` section 12.
//!
//! Tools describe what happened as typed [`DisplayBlock`]s and clients choose
//! how to render them. Display never enters model history. Nothing on the
//! wire uses these types yet; tool results, progress, and presentations
//! switch to them in later phases.

use std::{borrow::Cow, fmt::Write as _};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ArtifactRef, AttachmentName, CapabilityName, Open, RuntimeId, SessionId, human_bytes};

/// The serialized size all display blocks of one result share.
pub const DISPLAY_BUDGET_BYTES: usize = 64 * 1024;

/// The first client protocol minor of the frozen 1.x line, which every
/// block kind of this build is part of.
pub const FIRST_STABLE_MINOR: u32 = 0;

/// The client protocol minor that introduced each block kind. The daemon
/// replaces a block whose kind is newer than a client's negotiated minor
/// with a `text` block of its [`DisplayBlock::to_text`] rendering.
pub const DISPLAY_BLOCK_SINCE: &[(&str, u32)] = &[
    ("text", FIRST_STABLE_MINOR),
    ("markdown", FIRST_STABLE_MINOR),
    ("code", FIRST_STABLE_MINOR),
    ("file_excerpt", FIRST_STABLE_MINOR),
    ("diff", FIRST_STABLE_MINOR),
    ("terminal", FIRST_STABLE_MINOR),
    ("key_value", FIRST_STABLE_MINOR),
    ("table", FIRST_STABLE_MINOR),
    ("json", FIRST_STABLE_MINOR),
    ("media", FIRST_STABLE_MINOR),
    ("link", FIRST_STABLE_MINOR),
    ("session_ref", FIRST_STABLE_MINOR),
];

/// One typed unit of tool presentation.
///
/// Text fields allow `\n` and `\t` as their only control characters;
/// `terminal` segment text also allows SGR colour sequences. Every line
/// number is a 1-based file line number. `full` is present only on a
/// truncated block and names its complete content; the `omitted_*` members
/// count the whole units a cut left out.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DisplayBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omitted_lines: Option<u64>,
    },
    /// CommonMark.
    Markdown {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omitted_lines: Option<u64>,
    },
    Code {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(range(min = 1))]
        start_line: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omitted_lines: Option<u64>,
    },
    /// Consecutive lines of a file.
    FileExcerpt {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[schemars(range(min = 1))]
        start_line: u64,
        lines: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_lines: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omitted_lines: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full: Option<ArtifactRef>,
    },
    Diff(DiffBlock),
    /// Output of a process.
    Terminal {
        segments: Vec<TerminalSegment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omitted_lines: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        full: Option<ArtifactRef>,
    },
    KeyValue {
        entries: Vec<KeyValueEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omitted_entries: Option<u64>,
    },
    /// Every row has one cell per column.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        omitted_rows: Option<u64>,
    },
    Json {
        value: Value,
    },
    Media {
        artifact: ArtifactRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alt: Option<String>,
    },
    Link {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    SessionRef {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        short_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

crate::open_enum!(tagged DisplayBlock, tag = "type");

/// A change to one file, with real file line numbers.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DiffBlock {
    pub path: String,
    /// The path before a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub change: DiffChange,
    /// Empty when the change has no line diff, such as a binary or very
    /// large file; `added` and `removed` still count its lines.
    pub hunks: Vec<DiffHunk>,
    /// Lines added by the whole change, omitted hunks included.
    pub added: u64,
    /// Lines removed by the whole change, omitted hunks included.
    pub removed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<DiffOmitted>,
    pub as_of: DiffAsOf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full: Option<ArtifactRef>,
}

crate::open_enum! {
    /// How a file changed. An unknown change reads as `modified`.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum DiffChange {
        Created,
        Modified,
        Deleted,
        Renamed,
    }
}

/// Whether a diff shows what a call will do (`prepare`) or what it did
/// (`execute`); the two differ when the file changed in between.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffAsOf {
    Prepare,
    Execute,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DiffOmitted {
    pub hunks: u64,
    pub lines: u64,
}

/// A run of changed lines and their context. `old_lines` counts its
/// `context` and `remove` lines, `new_lines` its `context` and `add` lines.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DiffHunk {
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DiffLine {
    pub op: DiffLineOp,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineOp {
    Context,
    Add,
    Remove,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct TerminalSegment {
    pub stream: TerminalStream,
    /// May contain SGR colour sequences.
    pub text: String,
}

crate::open_enum! {
    /// The stream a terminal segment came from. An unknown stream reads as
    /// `stdout`.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum TerminalStream {
        Stdout,
        Stderr,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct KeyValueEntry {
    pub key: String,
    pub value: String,
}

/// A display block failed validation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DisplayBlockError {
    #[error("{0} contains a control character other than newline or tab")]
    ControlCharacter(&'static str),
    #[error("{0} must be at least 1")]
    LineNumber(&'static str),
    #[error("a diff hunk's line counts do not match its lines")]
    HunkCounts,
    #[error("a table row does not have one cell per column")]
    TableShape,
    #[error("an artifact reference is invalid: {0}")]
    Artifact(crate::ContentError),
}

impl DisplayBlock {
    /// The block's `type` tag.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Markdown { .. } => "markdown",
            Self::Code { .. } => "code",
            Self::FileExcerpt { .. } => "file_excerpt",
            Self::Diff(_) => "diff",
            Self::Terminal { .. } => "terminal",
            Self::KeyValue { .. } => "key_value",
            Self::Table { .. } => "table",
            Self::Json { .. } => "json",
            Self::Media { .. } => "media",
            Self::Link { .. } => "link",
            Self::SessionRef { .. } => "session_ref",
        }
    }

    /// A `text` block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            omitted_lines: None,
        }
    }

    /// The client minor that introduced `kind`, if this build knows it.
    #[must_use]
    pub fn since(kind: &str) -> Option<u32> {
        DISPLAY_BLOCK_SINCE
            .iter()
            .find(|(known, _)| *known == kind)
            .map(|(_, minor)| *minor)
    }

    /// The block as a client at `client_minor` can read it: unchanged, or a
    /// `text` block of its rendering when its kind is newer.
    #[must_use]
    pub fn downgrade(self, client_minor: u32) -> Self {
        let since = Self::since(self.kind());
        self.downgrade_from(since, client_minor)
    }

    fn downgrade_from(self, since: Option<u32>, client_minor: u32) -> Self {
        match since {
            Some(since) if since > client_minor => Self::text(self.to_text()),
            _ => self,
        }
    }

    /// Checks the block as a writer must before sending it.
    pub fn validate(&self) -> Result<(), DisplayBlockError> {
        let text = |value: &str, what| {
            if is_display_safe(value) {
                Ok(())
            } else {
                Err(DisplayBlockError::ControlCharacter(what))
            }
        };
        let optional = |value: &Option<String>, what| {
            value.as_deref().map_or(Ok(()), |value| text(value, what))
        };
        let artifact = |value: Option<&ArtifactRef>| {
            value.map_or(Ok(()), |artifact| {
                artifact.validate().map_err(DisplayBlockError::Artifact)
            })
        };
        match self {
            Self::Text { text: body, .. } | Self::Markdown { text: body, .. } => text(body, "text"),
            Self::Code {
                text: body,
                language,
                start_line,
                ..
            } => {
                if *start_line == Some(0) {
                    return Err(DisplayBlockError::LineNumber("start_line"));
                }
                text(body, "text")?;
                optional(language, "language")
            }
            Self::FileExcerpt {
                path,
                language,
                start_line,
                lines,
                full,
                ..
            } => {
                if *start_line == 0 {
                    return Err(DisplayBlockError::LineNumber("start_line"));
                }
                text(path, "path")?;
                optional(language, "language")?;
                for line in lines {
                    if line.contains('\n') {
                        return Err(DisplayBlockError::ControlCharacter("an excerpt line"));
                    }
                    text(line, "an excerpt line")?;
                }
                artifact(full.as_ref())
            }
            Self::Diff(diff) => {
                text(&diff.path, "path")?;
                optional(&diff.old_path, "old_path")?;
                optional(&diff.language, "language")?;
                for hunk in &diff.hunks {
                    let count = |ops: &[DiffLineOp]| {
                        hunk.lines
                            .iter()
                            .filter(|line| ops.contains(&line.op))
                            .count() as u64
                    };
                    if count(&[DiffLineOp::Context, DiffLineOp::Remove]) != hunk.old_lines
                        || count(&[DiffLineOp::Context, DiffLineOp::Add]) != hunk.new_lines
                    {
                        return Err(DisplayBlockError::HunkCounts);
                    }
                    for line in &hunk.lines {
                        if line.text.contains('\n') {
                            return Err(DisplayBlockError::ControlCharacter("a diff line"));
                        }
                        text(&line.text, "a diff line")?;
                    }
                }
                artifact(diff.full.as_ref())
            }
            Self::Terminal { segments, full, .. } => {
                for segment in segments {
                    if !is_terminal_safe(&segment.text) {
                        return Err(DisplayBlockError::ControlCharacter("terminal text"));
                    }
                }
                artifact(full.as_ref())
            }
            Self::KeyValue { entries, .. } => entries.iter().try_for_each(|entry| {
                text(&entry.key, "a key")?;
                text(&entry.value, "a value")
            }),
            Self::Table { columns, rows, .. } => {
                for column in columns {
                    text(column, "a column")?;
                }
                for row in rows {
                    if row.len() != columns.len() {
                        return Err(DisplayBlockError::TableShape);
                    }
                    for cell in row {
                        text(cell, "a cell")?;
                    }
                }
                Ok(())
            }
            Self::Json { .. } => Ok(()),
            Self::Media {
                artifact: media,
                alt,
            } => {
                artifact(Some(media))?;
                optional(alt, "alt")
            }
            Self::Link { url, label } => {
                text(url, "url")?;
                optional(label, "label")
            }
            Self::SessionRef {
                short_id, label, ..
            } => {
                optional(short_id, "short_id")?;
                optional(label, "label")
            }
        }
    }

    /// Strips every control character and sequence the block may not
    /// carry, as the daemon does with blocks it receives from peers.
    pub fn sanitize(&mut self) {
        fn clean(value: &mut String) {
            if let Cow::Owned(cleaned) = sanitize_display_text(value) {
                *value = cleaned;
            }
        }
        fn clean_line(value: &mut String) {
            clean(value);
            if value.contains('\n') {
                *value = value.replace('\n', " ");
            }
        }
        fn clean_optional(value: &mut Option<String>) {
            if let Some(value) = value {
                clean(value);
            }
        }
        match self {
            Self::Text { text, .. } | Self::Markdown { text, .. } => clean(text),
            Self::Code { text, language, .. } => {
                clean(text);
                clean_optional(language);
            }
            Self::FileExcerpt {
                path,
                language,
                lines,
                ..
            } => {
                clean(path);
                clean_optional(language);
                lines.iter_mut().for_each(clean_line);
            }
            Self::Diff(diff) => {
                clean(&mut diff.path);
                clean_optional(&mut diff.old_path);
                clean_optional(&mut diff.language);
                for hunk in &mut diff.hunks {
                    for line in &mut hunk.lines {
                        clean_line(&mut line.text);
                    }
                }
            }
            Self::Terminal { segments, .. } => {
                for segment in segments {
                    if let Cow::Owned(cleaned) = sanitize_terminal_text(&segment.text) {
                        segment.text = cleaned;
                    }
                }
            }
            Self::KeyValue { entries, .. } => {
                for entry in entries {
                    clean(&mut entry.key);
                    clean(&mut entry.value);
                }
            }
            Self::Table { columns, rows, .. } => {
                columns.iter_mut().for_each(clean);
                rows.iter_mut().flatten().for_each(clean);
            }
            Self::Json { .. } => {}
            Self::Media { alt, .. } => clean_optional(alt),
            Self::Link { url, label } => {
                clean(url);
                clean_optional(label);
            }
            Self::SessionRef {
                short_id, label, ..
            } => {
                clean_optional(short_id);
                clean_optional(label);
            }
        }
    }

    /// The canonical text rendering of the block: what the CLI prints and
    /// what a client older than the block's kind receives instead.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        match self {
            Self::Text {
                text,
                omitted_lines,
            }
            | Self::Markdown {
                text,
                omitted_lines,
            }
            | Self::Code {
                text,
                omitted_lines,
                ..
            } => {
                out.push_str(text);
                omitted_note(&mut out, *omitted_lines, "line");
            }
            Self::FileExcerpt {
                path,
                start_line,
                lines,
                omitted_lines,
                ..
            } => {
                out.push_str(path);
                let last = start_line + lines.len().saturating_sub(1) as u64;
                let width = last.to_string().len();
                for (offset, line) in lines.iter().enumerate() {
                    let number = start_line + offset as u64;
                    let _ = write!(out, "\n{number:>width$}  {line}");
                }
                omitted_note(&mut out, *omitted_lines, "line");
            }
            Self::Diff(diff) => diff_text(diff, &mut out),
            Self::Terminal {
                segments,
                exit_code,
                omitted_lines,
                ..
            } => {
                for segment in segments {
                    out.push_str(&strip_sgr(&segment.text));
                }
                omitted_note(&mut out, *omitted_lines, "line");
                if let Some(code) = exit_code {
                    new_line(&mut out);
                    let _ = write!(out, "[exit code {code}]");
                }
            }
            Self::KeyValue {
                entries,
                omitted_entries,
            } => {
                for entry in entries {
                    new_line(&mut out);
                    let _ = write!(out, "{}: {}", entry.key, entry.value);
                }
                omitted_note(&mut out, *omitted_entries, "entry");
            }
            Self::Table {
                columns,
                rows,
                omitted_rows,
            } => {
                table_text(columns, rows, &mut out);
                omitted_note(&mut out, *omitted_rows, "row");
            }
            Self::Json { value } => {
                out = serde_json::to_string_pretty(value).unwrap_or_default();
            }
            Self::Media { artifact, alt } => {
                let _ = write!(
                    out,
                    "[{}: {}, {}]",
                    artifact.top_level_type(),
                    artifact.display_name(),
                    human_bytes(artifact.byte_length)
                );
                if let Some(alt) = alt {
                    let _ = write!(out, " {alt}");
                }
            }
            Self::Link { url, label } => match label {
                Some(label) => {
                    let _ = write!(out, "{label} <{url}>");
                }
                None => out.push_str(url),
            },
            Self::SessionRef {
                session_id,
                short_id,
                label,
            } => {
                match short_id {
                    Some(short_id) => {
                        let _ = write!(out, "session {short_id}");
                    }
                    None => {
                        let _ = write!(out, "session {session_id}");
                    }
                }
                if let Some(label) = label {
                    let _ = write!(out, ": {label}");
                }
            }
        }
        out
    }

    /// The block cut to at most `limit` serialized bytes, keeping its first
    /// whole units (lines, hunks, entries, or rows) and counting the rest in
    /// its `omitted_*` member. `None` when not even the empty block fits or
    /// the block has no units to cut.
    fn cut_to(&self, limit: usize) -> Option<Self> {
        let units = self.unit_count()?;
        let fits = |kept: usize| {
            let cut = self.keep_units(kept);
            (serialized_len(&cut) <= limit).then_some(cut)
        };
        let mut best = fits(0)?;
        let (mut low, mut high) = (1, units);
        while low <= high {
            let middle = low + (high - low) / 2;
            match fits(middle) {
                Some(cut) => {
                    best = cut;
                    low = middle + 1;
                }
                None => high = middle - 1,
            }
        }
        Some(best)
    }

    fn unit_count(&self) -> Option<usize> {
        match self {
            Self::Text { text, .. } | Self::Markdown { text, .. } | Self::Code { text, .. } => {
                Some(text.split_inclusive('\n').count())
            }
            Self::FileExcerpt { lines, .. } => Some(lines.len()),
            Self::Diff(diff) => Some(diff.hunks.len()),
            Self::Terminal { segments, .. } => Some(
                segments
                    .iter()
                    .map(|segment| segment.text.split_inclusive('\n').count())
                    .sum(),
            ),
            Self::KeyValue { entries, .. } => Some(entries.len()),
            Self::Table { rows, .. } => Some(rows.len()),
            Self::Json { .. }
            | Self::Media { .. }
            | Self::Link { .. }
            | Self::SessionRef { .. } => None,
        }
    }

    /// The block with only its first `kept` units.
    fn keep_units(&self, kept: usize) -> Self {
        fn omitted(previous: Option<u64>, dropped: usize) -> Option<u64> {
            let total = previous.unwrap_or(0) + dropped as u64;
            (total > 0).then_some(total)
        }
        fn head(text: &str, kept: usize) -> (String, usize) {
            let pieces = text.split_inclusive('\n').collect::<Vec<_>>();
            let kept = kept.min(pieces.len());
            (pieces[..kept].concat(), pieces.len() - kept)
        }
        let mut cut = self.clone();
        match &mut cut {
            Self::Text {
                text,
                omitted_lines,
            }
            | Self::Markdown {
                text,
                omitted_lines,
            }
            | Self::Code {
                text,
                omitted_lines,
                ..
            } => {
                let (kept_text, dropped) = head(text, kept);
                *text = kept_text;
                *omitted_lines = omitted(*omitted_lines, dropped);
            }
            Self::FileExcerpt {
                lines,
                omitted_lines,
                ..
            } => {
                let dropped = lines.len().saturating_sub(kept);
                lines.truncate(kept);
                *omitted_lines = omitted(*omitted_lines, dropped);
            }
            Self::Diff(diff) => {
                let dropped = diff.hunks.split_off(kept.min(diff.hunks.len()));
                if !dropped.is_empty() {
                    let previous = diff.omitted.unwrap_or(DiffOmitted { hunks: 0, lines: 0 });
                    diff.omitted = Some(DiffOmitted {
                        hunks: previous.hunks + dropped.len() as u64,
                        lines: previous.lines
                            + dropped
                                .iter()
                                .map(|hunk| hunk.lines.len() as u64)
                                .sum::<u64>(),
                    });
                }
            }
            Self::Terminal {
                segments,
                omitted_lines,
                ..
            } => {
                let mut remaining = kept;
                let mut dropped = 0;
                segments.retain_mut(|segment| {
                    let (kept_text, segment_dropped) = head(&segment.text, remaining);
                    remaining -= segment.text.split_inclusive('\n').count() - segment_dropped;
                    dropped += segment_dropped;
                    segment.text = kept_text;
                    !segment.text.is_empty()
                });
                *omitted_lines = omitted(*omitted_lines, dropped);
            }
            Self::KeyValue {
                entries,
                omitted_entries,
            } => {
                let dropped = entries.len().saturating_sub(kept);
                entries.truncate(kept);
                *omitted_entries = omitted(*omitted_entries, dropped);
            }
            Self::Table {
                rows, omitted_rows, ..
            } => {
                let dropped = rows.len().saturating_sub(kept);
                rows.truncate(kept);
                *omitted_rows = omitted(*omitted_rows, dropped);
            }
            Self::Json { .. }
            | Self::Media { .. }
            | Self::Link { .. }
            | Self::SessionRef { .. } => {}
        }
        cut
    }
}

/// The text of several blocks, one after another. Blocks this build does
/// not know render as nothing.
#[must_use]
pub fn display_text(blocks: &[Open<DisplayBlock>]) -> String {
    blocks
        .iter()
        .filter_map(Open::known)
        .map(DisplayBlock::to_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fits the blocks of one result into `budget` serialized bytes (normally
/// [`DISPLAY_BUDGET_BYTES`]). Blocks that fit stay whole. The first that
/// does not is cut to whole units and marked `omitted`, leaving room for the
/// blocks after it when they fit whole (such as a short status after a long
/// output); each later block is kept, cut, or dropped the same way with what
/// remains. Setting `full` on a cut block is the caller's job, since only it
/// knows where the complete content lives.
#[must_use]
pub fn fit_display_budget(blocks: Vec<DisplayBlock>, budget: usize) -> Vec<DisplayBlock> {
    if serialized_len(&blocks) <= budget {
        return blocks;
    }
    let sizes = blocks.iter().map(serialized_len).collect::<Vec<_>>();
    // `[` and `]`, plus a comma before every block but the first.
    let mut used = 2;
    let mut kept = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.into_iter().enumerate() {
        let separator = usize::from(!kept.is_empty());
        let Some(remaining) = budget.checked_sub(used + separator) else {
            break;
        };
        let block = if sizes[index] <= remaining {
            Some(block)
        } else {
            let later = sizes[index + 1..]
                .iter()
                .map(|size| size.saturating_add(1))
                .fold(0_usize, usize::saturating_add);
            block.cut_to(remaining.checked_sub(later).unwrap_or(remaining))
        };
        if let Some(block) = block {
            used += separator + serialized_len(&block);
            kept.push(block);
        }
    }
    kept
}

fn serialized_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn new_line(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn omitted_note(out: &mut String, omitted: Option<u64>, unit: &str) {
    if let Some(count) = omitted.filter(|count| *count > 0) {
        new_line(out);
        let plural = match (count, unit) {
            (1, _) => unit.to_owned(),
            (_, "entry") => "entries".to_owned(),
            _ => format!("{unit}s"),
        };
        let _ = write!(out, "… {count} more {plural}");
    }
}

fn diff_text(diff: &DiffBlock, out: &mut String) {
    let old = diff.old_path.as_deref().unwrap_or(&diff.path);
    let (old, new) = match diff.change {
        DiffChange::Created => ("/dev/null", diff.path.as_str()),
        DiffChange::Deleted => (old, "/dev/null"),
        DiffChange::Modified | DiffChange::Renamed | DiffChange::Unknown => {
            (old, diff.path.as_str())
        }
    };
    if diff.hunks.is_empty() && diff.omitted.is_none() {
        let change = match diff.change {
            DiffChange::Created => "created",
            DiffChange::Deleted => "deleted",
            DiffChange::Renamed => "renamed",
            DiffChange::Modified | DiffChange::Unknown => "modified",
        };
        let _ = write!(
            out,
            "{}: {change} (+{} -{})",
            diff.path, diff.added, diff.removed
        );
        return;
    }
    let _ = write!(out, "--- {old}\n+++ {new}");
    for hunk in &diff.hunks {
        let _ = write!(
            out,
            "\n@@ -{} +{} @@",
            hunk_range(hunk.old_start, hunk.old_lines),
            hunk_range(hunk.new_start, hunk.new_lines)
        );
        for line in &hunk.lines {
            let marker = match line.op {
                DiffLineOp::Context => ' ',
                DiffLineOp::Add => '+',
                DiffLineOp::Remove => '-',
            };
            let _ = write!(out, "\n{marker}{}", line.text);
        }
    }
    if let Some(omitted) = diff.omitted.filter(|omitted| omitted.hunks > 0) {
        let hunks = if omitted.hunks == 1 { "hunk" } else { "hunks" };
        let lines = if omitted.lines == 1 { "line" } else { "lines" };
        let _ = write!(
            out,
            "\n… {} more {hunks} ({} {lines})",
            omitted.hunks, omitted.lines
        );
    }
}

/// A unified diff range: `start,count`, or `start` alone for one line.
fn hunk_range(start: u64, lines: u64) -> String {
    if lines == 1 {
        start.to_string()
    } else {
        format!("{start},{lines}")
    }
}

fn table_text(columns: &[String], rows: &[Vec<String>], out: &mut String) {
    let width = |text: &str| text.chars().count();
    let mut widths = columns
        .iter()
        .map(|column| width(column))
        .collect::<Vec<_>>();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(column) = widths.get_mut(index) {
                *column = (*column).max(width(cell));
            }
        }
    }
    let line = |cells: &mut dyn Iterator<Item = &str>, out: &mut String| {
        new_line(out);
        let mut text = String::new();
        for (index, cell) in cells.enumerate() {
            if index > 0 {
                text.push_str("  ");
            }
            let pad = widths
                .get(index)
                .copied()
                .unwrap_or(0)
                .saturating_sub(width(cell));
            text.push_str(cell);
            text.extend(std::iter::repeat_n(' ', pad));
        }
        out.push_str(text.trim_end());
    };
    line(&mut columns.iter().map(String::as_str), out);
    let rules = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>();
    line(&mut rules.iter().map(String::as_str), out);
    for row in rows {
        line(&mut row.iter().map(String::as_str), out);
    }
}

/// Whether `text` has no control characters but newline and tab.
#[must_use]
pub fn is_display_safe(text: &str) -> bool {
    !text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
}

/// Whether `text` is display-safe apart from SGR colour sequences.
#[must_use]
pub fn is_terminal_safe(text: &str) -> bool {
    matches!(sanitize_terminal_text(text), Cow::Borrowed(_))
}

/// `text` without escape sequences or control characters other than
/// newline and tab.
#[must_use]
pub fn sanitize_display_text(text: &str) -> Cow<'_, str> {
    sanitize(text, false)
}

/// Like [`sanitize_display_text`], but SGR colour sequences
/// (`ESC [ <params> m`) are kept.
#[must_use]
pub fn sanitize_terminal_text(text: &str) -> Cow<'_, str> {
    sanitize(text, true)
}

/// `text` with its SGR colour sequences removed, for plain-text output of
/// terminal segments.
#[must_use]
pub fn strip_sgr(text: &str) -> Cow<'_, str> {
    sanitize(text, false)
}

fn sanitize(text: &str, keep_sgr: bool) -> Cow<'_, str> {
    let unsafe_character =
        |character: char| character.is_control() && !matches!(character, '\n' | '\t');
    let Some(first) = text.find(unsafe_character) else {
        return Cow::Borrowed(text);
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..first]);
    let mut rest = &text[first..];
    let mut changed = false;
    while let Some(character) = rest.chars().next() {
        if character != '\u{1b}' {
            if unsafe_character(character) {
                changed = true;
            } else {
                out.push(character);
            }
            rest = &rest[character.len_utf8()..];
            continue;
        }
        let length = escape_length(rest);
        let sequence = &rest[..length];
        if keep_sgr && is_sgr(sequence) {
            out.push_str(sequence);
        } else {
            changed = true;
        }
        rest = &rest[length..];
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// The byte length of the escape sequence at the start of `text`, which
/// starts with `ESC`: a CSI sequence through its final byte, an OSC, DCS,
/// SOS, PM, or APC string through its terminator, or `ESC` and one
/// character. An unterminated sequence runs to the end of `text`.
fn escape_length(text: &str) -> usize {
    let bytes = text.as_bytes();
    match bytes.get(1) {
        None => 1,
        // Parameter and intermediate bytes, then one final byte.
        Some(b'[') => bytes
            .iter()
            .skip(2)
            .position(|byte| !(0x20..=0x3f).contains(byte))
            .map_or(bytes.len(), |end| {
                let end = end + 2;
                if (0x40..=0x7e).contains(&bytes[end]) {
                    end + 1
                } else {
                    end
                }
            }),
        Some(b']' | b'P' | b'X' | b'^' | b'_') => {
            let mut index = 2;
            while index < bytes.len() {
                match bytes[index] {
                    0x07 => return index + 1,
                    0x1b if bytes.get(index + 1) == Some(&b'\\') => return index + 2,
                    _ => index += 1,
                }
            }
            bytes.len()
        }
        Some(_) => 1 + text[1..].chars().next().map_or(0, char::len_utf8),
    }
}

/// Whether `sequence` is exactly `ESC [ <digits and ;> m`.
fn is_sgr(sequence: &str) -> bool {
    sequence
        .strip_prefix("\u{1b}[")
        .and_then(|rest| rest.strip_suffix('m'))
        .is_some_and(|parameters| {
            parameters
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
        })
}

/// A change to the live display of an executing call.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DisplayDelta {
    /// Appends `text` to block `index`, which is a `text` or `terminal`
    /// block. A terminal append extends the last segment when it is of
    /// `stream` (default `stdout`), or starts a new segment.
    Append {
        index: u32,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<TerminalStream>,
    },
    /// Replaces the whole live list.
    Replace { blocks: Vec<Open<DisplayBlock>> },
}

crate::open_enum!(tagged DisplayDelta, tag = "type");

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DisplayDeltaError {
    #[error("the delta names block {0}, which does not exist")]
    NoSuchBlock(u32),
    #[error("block {0} is neither a text nor a terminal block")]
    NotAppendable(u32),
    #[error("a stream applies to terminal blocks only")]
    StreamOnText,
}

impl DisplayDelta {
    /// Applies the delta to the live display `blocks`.
    pub fn apply(self, blocks: &mut Vec<Open<DisplayBlock>>) -> Result<(), DisplayDeltaError> {
        match self {
            Self::Replace {
                blocks: replacement,
            } => {
                *blocks = replacement;
                Ok(())
            }
            Self::Append {
                index,
                text,
                stream,
            } => {
                let block = blocks
                    .get_mut(index as usize)
                    .ok_or(DisplayDeltaError::NoSuchBlock(index))?;
                match block {
                    Open::Known(DisplayBlock::Text { text: body, .. }) => {
                        if stream.is_some() {
                            return Err(DisplayDeltaError::StreamOnText);
                        }
                        body.push_str(&text);
                    }
                    Open::Known(DisplayBlock::Terminal { segments, .. }) => {
                        let stream = stream.unwrap_or(TerminalStream::Stdout);
                        match segments.last_mut() {
                            Some(last) if last.stream == stream => last.text.push_str(&text),
                            _ => segments.push(TerminalSegment { stream, text }),
                        }
                    }
                    _ => return Err(DisplayDeltaError::NotAppendable(index)),
                }
                Ok(())
            }
        }
    }
}

crate::snake_label!(
    /// What a tool is for, used only for icons and grouping. The daemon never
    /// branches on it, and a client renders a hint it does not know as
    /// [`ToolHint::GENERIC`].
    ToolHint,
    "tool hint",
    "What a tool is for, such as `file_edit`."
);

impl ToolHint {
    pub const SHELL: &'static str = "shell";
    pub const FILE_READ: &'static str = "file_read";
    pub const FILE_WRITE: &'static str = "file_write";
    pub const FILE_EDIT: &'static str = "file_edit";
    pub const FETCH: &'static str = "fetch";
    pub const SEARCH: &'static str = "search";
    pub const DELEGATE: &'static str = "delegate";
    pub const MESSAGE: &'static str = "message";
    pub const SKILL: &'static str = "skill";
    pub const GOAL: &'static str = "goal";
    pub const GENERIC: &'static str = "generic";
    /// Every documented hint.
    pub const DOCUMENTED: &'static [&'static str] = &[
        Self::SHELL,
        Self::FILE_READ,
        Self::FILE_WRITE,
        Self::FILE_EDIT,
        Self::FETCH,
        Self::SEARCH,
        Self::DELEGATE,
        Self::MESSAGE,
        Self::SKILL,
        Self::GOAL,
        Self::GENERIC,
    ];

    /// The hint as a client renders it: itself when documented, `generic`
    /// otherwise.
    #[must_use]
    pub fn rendered(&self) -> &str {
        if Self::DOCUMENTED.contains(&self.as_str()) {
            self.as_str()
        } else {
            Self::GENERIC
        }
    }
}

/// Where a published tool comes from.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSource {
    /// The daemon's base layer.
    Base,
    Runtime {
        runtime_id: RuntimeId,
        attachment: AttachmentName,
        capability: CapabilityName,
    },
    Plugin {
        plugin: String,
    },
    Mcp {
        server: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_id: Option<RuntimeId>,
    },
}

crate::open_enum!(tagged ToolSource, tag = "type");

/// The tool a call goes to, as the daemon knows it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ToolIdentity {
    /// The name its source knows the tool by.
    pub name: String,
    /// The name the model sees.
    pub published_name: String,
    pub source: Open<ToolSource>,
}

/// The presentation a runtime or plugin supplies from prepare.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct PreparedPresentation {
    pub title: String,
    pub hint: ToolHint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_argument: Option<String>,
    /// The call's arguments; clients fall back to the raw model arguments
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<Open<DisplayBlock>>>,
    /// What the call will do, such as a diff as of prepare.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<Open<DisplayBlock>>,
}

/// How clients show a tool call: the prepared presentation and the tool the
/// daemon resolved.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ToolPresentation {
    pub title: String,
    pub hint: ToolHint,
    pub tool: ToolIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_argument: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<Open<DisplayBlock>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<Open<DisplayBlock>>,
}

impl ToolPresentation {
    #[must_use]
    pub fn new(prepared: PreparedPresentation, tool: ToolIdentity) -> Self {
        let PreparedPresentation {
            title,
            hint,
            primary_argument,
            arguments,
            preview,
        } = prepared;
        Self {
            title,
            hint,
            tool,
            primary_argument,
            arguments,
            preview,
        }
    }
}

#[cfg(test)]
#[path = "display_tests.rs"]
mod tests;

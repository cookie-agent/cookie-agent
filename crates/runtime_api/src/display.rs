//! What file-changing tools show clients, shared by the coding runtime's
//! tools and the engine's memory tools: language labels, file diffs with
//! real line numbers and five lines of context, and fitting a block to the
//! display budget (`docs/site/specs/compatibility-v1.md`
//! section 12.5).

use std::path::Path;

use crate::{ToolArtifacts, ToolError};
use cookie_agent_protocol::SessionId;
use cookie_agent_protocol::{
    ArtifactRef, DISPLAY_BUDGET_BYTES, DiffAsOf, DiffBlock, DiffChange, DiffHunk, DiffLine,
    DiffLineOp, DisplayBlock, fit_display_budget,
};

/// Unchanged lines a diff shows on each side of a change.
pub const CONTEXT_LINES: usize = 5;

/// What a block cut to the budget leaves for its `full` reference.
const FULL_RESERVE_BYTES: usize = 1024;

/// The language label clients highlight a file of `path` with: a fenced code
/// block's name for the extensions this table knows, else the extension
/// itself (lowercase, when it is a plain word).
pub fn language_for_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let special = match name {
        "Makefile" | "makefile" | "GNUmakefile" => Some("makefile"),
        "Dockerfile" => Some("dockerfile"),
        "CMakeLists.txt" => Some("cmake"),
        _ => None,
    };
    if let Some(language) = special {
        return Some(language.to_owned());
    }
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() || extension.is_empty() {
        return None;
    }
    let extension = extension.to_ascii_lowercase();
    let known = match extension.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "cpp",
        "cs" => "csharp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "sh",
        "ps1" => "powershell",
        "toml" => "toml",
        "yml" | "yaml" => "yaml",
        "json" | "jsonc" => "json",
        "xml" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "scala" => "scala",
        "zig" => "zig",
        "nix" => "nix",
        "dart" => "dart",
        "r" => "r",
        "tex" => "latex",
        _ => "",
    };
    if !known.is_empty() {
        return Some(known.to_owned());
    }
    (extension.len() <= 16 && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then_some(extension)
}

/// The lines of `text`, each with its line terminator; a trailing newline
/// does not start another line.
pub fn raw_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A line as a diff shows it: without its terminator.
fn shown(line: &str) -> String {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line).to_owned()
}

/// One run of changed lines: `removed` replaced `old_start` (a 0-based index
/// into the old lines) onward, and `added` stands at `new_start` in the new
/// lines. Lines keep their terminators.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineChange<'a> {
    pub old_start: usize,
    pub removed: Vec<&'a str>,
    pub new_start: usize,
    pub added: Vec<String>,
}

/// A byte range of whole lines, and the replaced ranges inside it.
type Region = (usize, usize, Vec<(usize, usize)>);

/// The line changes of replacing each byte range of `matches` (in order,
/// not overlapping) in `text` with `new`. Each replacement covers the whole
/// lines it touches, replacements whose lines overlap are taken together,
/// and lines a replacement leaves as they were at either end are not part
/// of its change.
pub fn replacement_changes<'a>(
    text: &'a str,
    matches: &[(usize, usize)],
    new: &str,
) -> Vec<LineChange<'a>> {
    let starts = line_starts(text);
    let line_of = |offset: usize| starts.partition_point(|start| *start <= offset) - 1;
    // The end of the line holding `offset`, past its newline.
    let region_end = |offset: usize| {
        if offset >= text.len() {
            text.len()
        } else {
            starts
                .get(line_of(offset) + 1)
                .copied()
                .unwrap_or(text.len())
        }
    };
    // Regions of whole lines, each with the replacements inside it.
    let mut regions: Vec<Region> = Vec::new();
    for &(start, end) in matches {
        let region_start = starts[line_of(start)];
        let end_of_region = region_end(end);
        match regions.last_mut() {
            Some((_, last_end, members)) if region_start < *last_end => {
                *last_end = (*last_end).max(end_of_region);
                members.push((start, end));
            }
            _ => regions.push((region_start, end_of_region, vec![(start, end)])),
        }
    }
    let mut changes = Vec::with_capacity(regions.len());
    let mut delta = 0_isize;
    for (region_start, region_end, members) in regions {
        let mut replaced = String::new();
        let mut cursor = region_start;
        for (start, end) in members {
            replaced.push_str(&text[cursor..start]);
            replaced.push_str(new);
            cursor = end;
        }
        replaced.push_str(&text[cursor..region_end]);
        let old = raw_lines(&text[region_start..region_end]);
        let added = raw_lines(&replaced);
        let first = line_of(region_start);
        if let Some(change) = trimmed_change(first, first, &old, &added) {
            let change = LineChange {
                new_start: change.old_start.saturating_add_signed(delta),
                ..change
            };
            delta += change.added.len() as isize - change.removed.len() as isize;
            changes.push(change);
        }
    }
    changes
}

/// The change of `old` (at `old_start`) to `new` (at `new_start`) without
/// the lines they begin and end with in common; `None` when nothing differs.
fn trimmed_change<'a>(
    old_start: usize,
    new_start: usize,
    old: &[&'a str],
    new: &[&str],
) -> Option<LineChange<'a>> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let removed = old[prefix..old.len() - suffix].to_vec();
    let added = new[prefix..new.len() - suffix]
        .iter()
        .map(|line| (*line).to_owned())
        .collect::<Vec<_>>();
    (!removed.is_empty() || !added.is_empty()).then_some(LineChange {
        old_start: old_start + prefix,
        removed,
        new_start: new_start + prefix,
        added,
    })
}

/// Byte offsets where each line of `text` starts.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(memchr::memchr_iter(b'\n', text.as_bytes()).map(|index| index + 1));
    if starts.last() == Some(&text.len()) {
        starts.pop();
    }
    if starts.is_empty() {
        // An empty text still has a place for its first line.
        starts.push(0);
    }
    starts
}

/// The line changes from `old` to `new`, by a line diff that gives up on an
/// optimal result after a second.
pub fn line_diff_changes<'a>(old: &'a str, new: &str) -> Vec<LineChange<'a>> {
    let old_lines = raw_lines(old);
    let new_lines = raw_lines(new);
    let ops = similar::capture_diff_slices_deadline(
        similar::Algorithm::Myers,
        &old_lines,
        &new_lines,
        Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
    );
    let mut changes: Vec<LineChange<'a>> = Vec::new();
    for op in ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == similar::DiffTag::Equal {
            continue;
        }
        let removed = old_lines[old_range.clone()].to_vec();
        let added = new_lines[new_range.clone()]
            .iter()
            .map(|line| (*line).to_owned())
            .collect::<Vec<_>>();
        match changes.last_mut() {
            // A deletion followed by an insertion at the same place is one
            // change.
            Some(last) if last.old_start + last.removed.len() == old_range.start => {
                last.removed.extend(removed);
                last.added.extend(added);
            }
            _ => changes.push(LineChange {
                old_start: old_range.start,
                removed,
                new_start: new_range.start,
                added,
            }),
        }
    }
    changes
}

/// The hunks of `changes` (in order) to a file of `old` lines: each change
/// with [`CONTEXT_LINES`] of context on each side, changes whose context
/// overlaps or touches in one hunk, and 1-based file line numbers. A side
/// with no lines in a hunk starts at the line before it (0 at the top), as a
/// unified diff numbers it.
pub fn hunks(old: &[&str], changes: &[LineChange<'_>]) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut index = 0;
    while index < changes.len() {
        let mut last = index;
        while let Some(next) = changes.get(last + 1) {
            let end = changes[last].old_start + changes[last].removed.len();
            if next.old_start.saturating_sub(end) > 2 * CONTEXT_LINES {
                break;
            }
            last += 1;
        }
        let first = &changes[index];
        let start = first.old_start.saturating_sub(CONTEXT_LINES);
        let tail = changes[last].old_start + changes[last].removed.len();
        let end = (tail + CONTEXT_LINES).min(old.len());
        let mut lines = Vec::new();
        let context = |lines: &mut Vec<DiffLine>, range: std::ops::Range<usize>| {
            lines.extend(old[range].iter().map(|line| DiffLine {
                op: DiffLineOp::Context,
                text: shown(line),
            }));
        };
        let mut cursor = start;
        let (mut removed, mut added) = (0, 0);
        for change in &changes[index..=last] {
            context(&mut lines, cursor..change.old_start);
            lines.extend(change.removed.iter().map(|line| DiffLine {
                op: DiffLineOp::Remove,
                text: shown(line),
            }));
            lines.extend(change.added.iter().map(|line| DiffLine {
                op: DiffLineOp::Add,
                text: shown(line),
            }));
            removed += change.removed.len();
            added += change.added.len();
            cursor = change.old_start + change.removed.len();
        }
        context(&mut lines, cursor..end);
        let old_lines = end - start;
        let new_lines = old_lines - removed + added;
        let new_start = (start + first.new_start).saturating_sub(first.old_start);
        let number = |start: usize, count: usize| start as u64 + u64::from(count > 0);
        hunks.push(DiffHunk {
            old_start: number(start, old_lines),
            old_lines: old_lines as u64,
            new_start: number(new_start, new_lines),
            new_lines: new_lines as u64,
            lines,
        });
        index = last + 1;
    }
    hunks
}

/// A diff of `path` from `old` lines by `changes`.
pub fn diff_block(
    path: &Path,
    change: DiffChange,
    old: &[&str],
    changes: &[LineChange<'_>],
    as_of: DiffAsOf,
) -> DiffBlock {
    DiffBlock {
        path: path.display().to_string(),
        old_path: None,
        language: language_for_path(path),
        change,
        hunks: hunks(old, changes),
        added: changes.iter().map(|change| change.added.len() as u64).sum(),
        removed: changes
            .iter()
            .map(|change| change.removed.len() as u64)
            .sum(),
        omitted: None,
        as_of,
        full: None,
    }
}

/// Where a cut block's complete content is kept: the session's artifact
/// store, under `filename`.
pub struct Retain<'a> {
    pub artifacts: &'a dyn ToolArtifacts,
    pub session: SessionId,
    pub filename: String,
}

impl<'a> Retain<'a> {
    /// `session`'s store, for the diff of a file of `path`.
    pub fn diff_of(artifacts: &'a dyn ToolArtifacts, session: SessionId, path: &Path) -> Self {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            artifacts,
            session,
            filename: format!("{name}.diff"),
        }
    }
}

/// `block` sanitized and fit to the display budget. A cut block of an
/// executed call also names its complete content: its
/// [`DisplayBlock::to_text`] before the cut, retained as `retain` says. A
/// preview has no store to retain it in.
pub fn fit_block(
    mut block: DisplayBlock,
    retain: Option<Retain<'_>>,
) -> Result<DisplayBlock, ToolError> {
    block.sanitize();
    let budget = DISPLAY_BUDGET_BYTES - FULL_RESERVE_BYTES;
    let complete = block.clone();
    let mut fitted = fit_display_budget(vec![pre_cut(block, budget)], budget);
    let Some(mut fitted) = fitted.pop() else {
        return Ok(complete);
    };
    if fitted.is_cut()
        && let Some(retain) = retain
    {
        let text = complete.to_text();
        let (_, sha256) = retain.artifacts.retain(retain.session, text.as_bytes())?;
        let filename = (!retain.filename.is_empty()
            && retain.filename.len() <= ArtifactRef::MAX_FILENAME_BYTES
            && !retain.filename.chars().any(char::is_control))
        .then_some(retain.filename);
        fitted.set_full(ArtifactRef {
            sha256,
            byte_length: text.len() as u64,
            media_type: "text/plain".into(),
            filename,
        });
    }
    Ok(fitted)
}

/// `block` with the units far past `budget` already left out, so the exact
/// fit has little to measure: a long diff or excerpt keeps the units whose
/// text alone fits the budget, the rest counted as omitted.
fn pre_cut(block: DisplayBlock, budget: usize) -> DisplayBlock {
    match block {
        DisplayBlock::Diff(mut diff) => {
            let mut used = 0;
            let kept = diff
                .hunks
                .iter()
                .take_while(|hunk| {
                    used += hunk
                        .lines
                        .iter()
                        .map(|line| line.text.len() + 16)
                        .sum::<usize>();
                    used <= budget
                })
                .count();
            // One more than fits, so the exact fit decides about it.
            let dropped = diff.hunks.split_off((kept + 1).min(diff.hunks.len()));
            if !dropped.is_empty() {
                let omitted = diff
                    .omitted
                    .get_or_insert(cookie_agent_protocol::DiffOmitted { hunks: 0, lines: 0 });
                omitted.hunks += dropped.len() as u64;
                omitted.lines += dropped
                    .iter()
                    .map(|hunk| hunk.lines.len() as u64)
                    .sum::<u64>();
            }
            DisplayBlock::Diff(diff)
        }
        DisplayBlock::FileExcerpt {
            path,
            language,
            start_line,
            mut lines,
            total_lines,
            omitted_lines,
            full,
        } => {
            let mut used = 0;
            let kept = lines
                .iter()
                .take_while(|line| {
                    used += line.len() + 4;
                    used <= budget
                })
                .count();
            let dropped = lines.len().saturating_sub(kept + 1);
            lines.truncate(kept + 1);
            let omitted = omitted_lines.unwrap_or(0) + dropped as u64;
            DisplayBlock::FileExcerpt {
                path,
                language,
                start_line,
                lines,
                total_lines,
                omitted_lines: (omitted > 0).then_some(omitted),
                full,
            }
        }
        block => block,
    }
}

/// How a text document changed, for [`text_diff`].
#[derive(Clone, Copy, Debug)]
pub enum TextChange<'a> {
    /// A new document of `text`.
    Created { text: &'a str },
    /// `text` with each byte range of `ranges` (in order, not overlapping)
    /// replaced by `new`, as the `edit` tool computes it.
    Replaced {
        text: &'a str,
        ranges: &'a [(usize, usize)],
        new: &'a str,
    },
    /// `old` replaced whole by `new`, as the `write` tool computes it.
    Modified { old: &'a str, new: &'a str },
    /// A removed document that held `text`.
    Deleted { text: &'a str },
}

/// The diff of a text document labelled `label` changing as `change` says,
/// built as the `edit` and `write` tools build theirs: real line numbers,
/// five unchanged lines on each side of a change, and the label's
/// language unless `language` names one.
#[must_use]
pub fn text_diff(
    label: &str,
    language: Option<String>,
    change: TextChange<'_>,
    as_of: DiffAsOf,
) -> DisplayBlock {
    let path = Path::new(label);
    let (kind, old, changes) = match change {
        TextChange::Created { text } => (DiffChange::Created, "", line_diff_changes("", text)),
        TextChange::Replaced { text, ranges, new } => (
            DiffChange::Modified,
            text,
            replacement_changes(text, ranges, new),
        ),
        TextChange::Modified { old, new } => {
            (DiffChange::Modified, old, line_diff_changes(old, new))
        }
        TextChange::Deleted { text } => (DiffChange::Deleted, text, line_diff_changes(text, "")),
    };
    let mut block = diff_block(path, kind, &raw_lines(old), &changes, as_of);
    if language.is_some() {
        block.language = language;
    }
    DisplayBlock::Diff(block)
}

/// An executed call's `block` fit to the display budget, as the `edit` and
/// `write` tools fit theirs: a cut block names its complete content, kept in
/// `session`'s artifact store as `filename`.
pub fn fit_executed_block(
    block: DisplayBlock,
    artifacts: &dyn ToolArtifacts,
    session: SessionId,
    filename: String,
) -> Result<DisplayBlock, ToolError> {
    fit_block(
        block,
        Some(Retain {
            artifacts,
            session,
            filename,
        }),
    )
}

#[cfg(test)]
#[path = "display/tests.rs"]
mod tests;

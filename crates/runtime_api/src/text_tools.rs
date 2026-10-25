//! What the file tools and the memory tools share: the numbered, 1-based
//! pages of text that `read`, `memory_read`, and `read_artifact` return,
//! and the wording of their results and errors.
//!
//! A page is a [`LineWindow`] of lines: `offset` is the 1-based number of
//! its first line and `limit` how many lines it holds at most. Each line is
//! returned as `N: text`, numbered from 1, cut after [`MAX_LINE_BYTES`] with
//! a `... [line truncated: N more bytes]` marker, and the page stops before
//! it outgrows its byte budget. A page that leaves lines out ends with a
//! continuation marker naming the `offset` to read next
//! ([`NumberedPage::continuation`]).

use std::{borrow::Cow, io::BufRead};

use cookie_agent_protocol::{DISPLAY_BUDGET_BYTES, DisplayBlock, PersistedToolResult};

use crate::ToolError;

/// Lines one page holds when a call gives no `limit`.
pub const DEFAULT_LIMIT: usize = 2_000;
/// Longest line returned in full; the rest of a longer line is replaced by a
/// marker, so one huge minified line cannot fill or fail the result.
pub const MAX_LINE_BYTES: usize = 4 * 1024;
/// Output budget for one page of text, under the 2 MiB result limit.
pub const MAX_PAGE_BYTES: usize = PersistedToolResult::MAX_OUTPUT_BYTES - 64 * 1024;
/// The most characters of a call's text an error repeats.
pub const MAX_ECHO_CHARS: usize = 200;

/// The lines a call asks for: from line `offset`, numbered from 1, at most
/// `limit` of them. Directory pages count entries the same way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineWindow {
    /// The first line, 1-based.
    pub offset: usize,
    /// The most lines the page holds; positive.
    pub limit: usize,
}

impl LineWindow {
    /// The window a call's `offset` and `limit` give, from line 1 and
    /// [`DEFAULT_LIMIT`] lines when they are left out. An `offset` of 0 and a
    /// `limit` of 0 are refused.
    pub fn new(offset: Option<usize>, limit: Option<usize>) -> Result<Self, ToolError> {
        let offset = offset.unwrap_or(1);
        if offset == 0 {
            return Err(ToolError::execution(
                "offset is 1-based: the first line is offset=1",
            ));
        }
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err(ToolError::execution("limit must be positive"));
        }
        Ok(Self { offset, limit })
    }

    /// The zero-based index of the first line.
    #[must_use]
    pub const fn first_index(&self) -> usize {
        self.offset - 1
    }

    /// The zero-based index just past the last line the window can hold.
    #[must_use]
    pub const fn end_index(&self) -> usize {
        self.first_index().saturating_add(self.limit)
    }

    /// Fails when the window starts past the last of `total` items of
    /// `target`, counted as `noun` (`line` or `entry`). Line 1 of nothing is
    /// an empty page, not an error.
    pub fn check_in_range(&self, total: usize, noun: &str, target: &str) -> Result<(), ToolError> {
        if self.offset > 1 && self.offset > total {
            return Err(ToolError::execution(format!(
                "offset={} is past the end of {target}, which has {}",
                self.offset,
                counted(total, noun)
            )));
        }
        Ok(())
    }

    /// The continuation marker of a page of `total` items, counted as
    /// `noun`, that this window cuts short; `None` when the page reaches the
    /// last item.
    #[must_use]
    pub fn remaining_marker(&self, total: usize, noun: &str) -> Option<String> {
        let end = self.end_index();
        (total > end).then(|| {
            format!(
                "[{} more {}; continue with offset={}]",
                total - end,
                noun_for(total - end, noun),
                end + 1
            )
        })
    }
}

/// `count` followed by `noun` (`line`, `entry`, `byte`, ...), plural unless
/// `count` is 1.
#[must_use]
pub fn counted(count: usize, noun: &str) -> String {
    format!("{count} {}", noun_for(count, noun))
}

/// `noun` as `count` of it reads: singular for 1, else plural.
fn noun_for(count: usize, noun: &str) -> Cow<'_, str> {
    if count == 1 {
        return Cow::Borrowed(noun);
    }
    Cow::Owned(match noun.strip_suffix('y') {
        Some(stem) => format!("{stem}ies"),
        None => format!("{noun}s"),
    })
}

/// Why a page could not be read.
#[derive(Debug)]
pub enum PageError {
    Io(std::io::Error),
    /// A line on the page is not UTF-8.
    NotText,
}

/// One page of numbered lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NumberedPage {
    /// The page's lines, each `N: text\n`.
    pub text: String,
    /// The page's first line, 1-based.
    pub start_line: usize,
    /// The page's lines as clients see them, without their numbers, up to
    /// the display budget.
    pub excerpt: Vec<String>,
    /// Lines of the page past the display budget.
    pub excerpt_omitted: u64,
    /// The lines of the whole text.
    pub total_lines: usize,
    /// The first line left out, 1-based, when lines follow the page.
    pub next_offset: Option<usize>,
    /// Whether the page stopped at its byte budget before its `limit`.
    pub cut: bool,
}

impl NumberedPage {
    /// The marker that ends a page leaving lines out: where to continue,
    /// and why the page stopped. `None` when the page reaches the last line.
    #[must_use]
    pub fn continuation(&self) -> Option<String> {
        let next = self.next_offset?;
        Some(if self.cut {
            format!("[page cut at the result size limit; continue with offset={next}]")
        } else {
            let remaining = self.total_lines.saturating_sub(next - 1);
            format!(
                "[{remaining} more {}; continue with offset={next}]",
                noun_for(remaining, "line")
            )
        })
    }

    /// Fails when the page starts past the last line of `target`.
    pub fn check_in_range(&self, target: &str) -> Result<(), ToolError> {
        LineWindow {
            offset: self.start_line,
            limit: 1,
        }
        .check_in_range(self.total_lines, "line", target)
    }

    /// The numbered lines followed by the continuation marker, if any, on a
    /// line of its own.
    #[must_use]
    pub fn text_with_continuation(&self) -> String {
        match self.continuation() {
            Some(marker) => format!("{}{marker}", self.text),
            None => self.text.clone(),
        }
    }

    /// The page as clients show it: an excerpt of `path` in `language`.
    #[must_use]
    pub fn excerpt_block(&self, path: String, language: Option<String>) -> DisplayBlock {
        DisplayBlock::FileExcerpt {
            path,
            language,
            start_line: self.start_line as u64,
            lines: self.excerpt.clone(),
            total_lines: Some(self.total_lines as u64),
            omitted_lines: (self.excerpt_omitted > 0).then_some(self.excerpt_omitted),
            full: None,
        }
    }
}

/// Builds a [`NumberedPage`] from text fed in chunks of any size, in one
/// streaming pass that also counts the text's lines.
#[derive(Debug)]
pub struct LinePager {
    window: LineWindow,
    /// The most bytes the numbered lines may take.
    budget: usize,
    /// Whether a line that is not UTF-8 is shown with replacement
    /// characters instead of failing the page.
    lossy: bool,
    text: String,
    /// The current line's bytes, up to [`MAX_LINE_BYTES`].
    line: Vec<u8>,
    /// Bytes of the current line beyond [`MAX_LINE_BYTES`].
    cut: usize,
    /// The first line, zero-based, left out because the page reached its
    /// budget.
    stopped_at: Option<usize>,
    excerpt: Vec<String>,
    excerpt_bytes: usize,
    excerpt_omitted: u64,
    /// Lines ended so far, and whether bytes follow the last newline.
    lines: usize,
    partial: bool,
}

impl LinePager {
    /// A pager of `window` whose numbered lines take at most `budget`
    /// bytes; a line that is not UTF-8 fails the page.
    #[must_use]
    pub fn new(window: LineWindow, budget: usize) -> Self {
        Self {
            window,
            budget,
            lossy: false,
            text: String::new(),
            line: Vec::new(),
            cut: 0,
            stopped_at: None,
            excerpt: Vec::new(),
            excerpt_bytes: 0,
            excerpt_omitted: 0,
            lines: 0,
            partial: false,
        }
    }

    /// The same pager, showing a line that is not UTF-8 with replacement
    /// characters instead of failing.
    #[must_use]
    pub fn lossy(mut self) -> Self {
        self.lossy = true;
        self
    }

    fn wanted(&self) -> bool {
        self.lines >= self.window.first_index()
            && self.lines < self.window.end_index()
            && self.stopped_at.is_none()
    }

    /// Feeds the next bytes of the text.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), PageError> {
        let mut rest = chunk;
        while !rest.is_empty() {
            let wanted = self.wanted();
            if !wanted && self.lines >= self.window.first_index() {
                // Past the page: only count what remains.
                self.lines += memchr::memchr_iter(b'\n', rest).count();
                self.partial = rest.last() != Some(&b'\n');
                return Ok(());
            }
            let newline = memchr::memchr(b'\n', rest);
            if wanted {
                self.keep(&rest[..newline.unwrap_or(rest.len())]);
            }
            match newline {
                Some(at) => {
                    if wanted {
                        self.finish_line(true)?;
                    }
                    self.lines += 1;
                    self.partial = false;
                    rest = &rest[at + 1..];
                }
                None => {
                    self.partial = true;
                    rest = &[];
                }
            }
        }
        Ok(())
    }

    /// The page, once the whole text was fed.
    pub fn finish(mut self) -> Result<NumberedPage, PageError> {
        if self.partial {
            if self.wanted() {
                self.finish_line(false)?;
            }
            self.lines += 1;
        }
        let next_offset = match self.stopped_at {
            Some(index) => Some(index + 1),
            None => (self.lines > self.window.end_index()).then(|| self.window.end_index() + 1),
        };
        Ok(NumberedPage {
            text: self.text,
            start_line: self.window.offset,
            excerpt: self.excerpt,
            excerpt_omitted: self.excerpt_omitted,
            total_lines: self.lines,
            next_offset,
            cut: self.stopped_at.is_some(),
        })
    }

    fn keep(&mut self, segment: &[u8]) {
        let take = MAX_LINE_BYTES
            .saturating_sub(self.line.len())
            .min(segment.len());
        self.line.extend_from_slice(&segment[..take]);
        self.cut += segment.len() - take;
    }

    fn finish_line(&mut self, terminated: bool) -> Result<(), PageError> {
        if terminated && self.cut == 0 && self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let line = std::mem::take(&mut self.line);
        let text: Cow<'_, str> = match std::str::from_utf8(&line) {
            Ok(text) => Cow::Borrowed(text),
            // A cut can split the last character; drop its leading bytes.
            Err(error) if self.cut > 0 && error.error_len().is_none() => {
                self.cut += line.len() - error.valid_up_to();
                Cow::Borrowed(
                    std::str::from_utf8(&line[..error.valid_up_to()]).expect("valid prefix"),
                )
            }
            Err(_) if self.lossy => String::from_utf8_lossy(&line),
            Err(_) => return Err(PageError::NotText),
        };
        let number = self.lines + 1;
        let rendered = if self.cut == 0 {
            format!("{number}: {text}\n")
        } else {
            format!(
                "{number}: {text}... [line truncated: {} more bytes]\n",
                self.cut
            )
        };
        if self.text.len() + rendered.len() > self.budget {
            self.stopped_at = Some(self.lines);
        } else {
            self.text.push_str(&rendered);
            // The excerpt line is the numbered line without its number.
            let shown = &rendered[number.to_string().len() + 2..rendered.len() - 1];
            if self.excerpt_bytes + shown.len() <= DISPLAY_BUDGET_BYTES {
                self.excerpt_bytes += shown.len();
                self.excerpt.push(shown.to_owned());
            } else {
                self.excerpt_omitted += 1;
            }
        }
        self.line = line;
        self.line.clear();
        self.cut = 0;
        Ok(())
    }
}

/// The page `window` of everything `reader` holds, its numbered lines in
/// at most `budget` bytes.
pub fn page_reader(
    reader: &mut impl BufRead,
    window: LineWindow,
    budget: usize,
) -> Result<NumberedPage, PageError> {
    let mut pager = LinePager::new(window, budget);
    loop {
        let chunk = reader.fill_buf().map_err(PageError::Io)?;
        if chunk.is_empty() {
            break;
        }
        let consumed = chunk.len();
        pager.push(chunk)?;
        reader.consume(consumed);
    }
    pager.finish()
}

/// The page `window` of `text`, its numbered lines in at most `budget`
/// bytes.
#[must_use]
pub fn page_text(text: &str, window: LineWindow, budget: usize) -> NumberedPage {
    let mut pager = LinePager::new(window, budget);
    pager
        .push(text.as_bytes())
        .and_then(|()| pager.finish())
        .expect("a str is UTF-8")
}

/// `text` as an error repeats it: its first [`MAX_ECHO_CHARS`] characters,
/// and an ellipsis when there are more.
#[must_use]
pub fn echo(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(MAX_ECHO_CHARS) {
        Some((at, _)) => Cow::Owned(format!("{}…", &text[..at])),
        None => Cow::Borrowed(text),
    }
}

/// The error of an edit whose `old` text occurs `count` times, other than
/// once, in `target`; `replace_all` adds that the edit may replace every
/// occurrence instead, for a tool that can.
#[must_use]
pub fn occurrence_error(old: &str, count: usize, target: &str, replace_all: bool) -> String {
    format!(
        "`{}` occurs {count} {} in {target}; include more context so it occurs once{}",
        echo(old),
        if count == 1 { "time" } else { "times" },
        if replace_all {
            " or set replaceAll"
        } else {
            ""
        }
    )
}

/// The result of a write that created (or replaced) `target` with `bytes`
/// bytes.
#[must_use]
pub fn wrote(created: bool, target: &str, bytes: usize) -> String {
    format!(
        "{} {target} ({}).",
        if created { "Created" } else { "Replaced" },
        counted(bytes, "byte")
    )
}

/// The result of an edit of `target` that made `replacements` replacements.
#[must_use]
pub fn edited(target: &str, replacements: usize) -> String {
    format!("Edited {target}: {}.", counted(replacements, "replacement"))
}

/// The error of a call naming a `target` that is not there.
#[must_use]
pub fn not_found(target: &str) -> String {
    format!("{target} does not exist")
}

#[cfg(test)]
mod tests;

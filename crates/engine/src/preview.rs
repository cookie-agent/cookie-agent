//! Head-and-tail previews of output too long to show whole. Tool output
//! streams and subagent reports truncate the same way: the first lines, a
//! marker naming what was left out and how to read it, then the last lines.

use cookie_agent_config::{SubagentOutputConfig, ToolOutputConfig};

/// When a preview truncates and how much of each end it keeps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviewLimits {
    /// Output within both limits is shown whole.
    pub(crate) max_lines: usize,
    pub(crate) max_bytes: usize,
    /// Lines kept from each end of truncated output. The byte limit is split
    /// between the ends in the same ratio.
    pub(crate) head_lines: usize,
    pub(crate) tail_lines: usize,
}

impl PreviewLimits {
    /// Limits that keep half of the lines from each end.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn halves(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            max_lines,
            max_bytes,
            head_lines: max_lines - max_lines / 2,
            tail_lines: max_lines / 2,
        }
    }

    /// The same split with `max_bytes` capped at `ceiling`.
    pub(crate) fn with_byte_ceiling(self, ceiling: usize) -> Self {
        Self {
            max_bytes: self.max_bytes.min(ceiling),
            ..self
        }
    }

    fn tail_bytes(self) -> usize {
        let lines = self.head_lines + self.tail_lines;
        if lines == 0 {
            return 0;
        }
        usize::try_from(self.max_bytes as u128 * self.tail_lines as u128 / lines as u128)
            .unwrap_or(self.max_bytes)
    }

    fn head_bytes(self) -> usize {
        self.max_bytes - self.tail_bytes()
    }
}

impl From<&ToolOutputConfig> for PreviewLimits {
    fn from(config: &ToolOutputConfig) -> Self {
        let (head_lines, tail_lines) = config.kept_lines();
        Self {
            max_lines: config.max_lines,
            max_bytes: config.max_bytes,
            head_lines,
            tail_lines,
        }
    }
}

impl From<&SubagentOutputConfig> for PreviewLimits {
    fn from(config: &SubagentOutputConfig) -> Self {
        let (head_lines, tail_lines) = config.kept_lines();
        Self {
            max_lines: config.max_lines,
            max_bytes: config.max_bytes,
            head_lines,
            tail_lines,
        }
    }
}

/// The two ends a truncated output shows. `head_source` is a prefix of the
/// output and `tail_source` a suffix; `tail_source_is_whole` says the suffix
/// starts at the output's first byte. `total_bytes` is the output's length.
pub(crate) fn split<'a>(
    head_source: &'a str,
    tail_source: &'a str,
    tail_source_is_whole: bool,
    total_bytes: u64,
    limits: PreviewLimits,
) -> (&'a str, &'a str) {
    let head = head_within(head_source, limits.head_lines, limits.head_bytes());
    let mut tail = tail_within(
        tail_source,
        tail_source_is_whole,
        limits.tail_lines,
        limits.tail_bytes(),
    );
    // Never repeat what the head already shows.
    let tail_start = total_bytes.saturating_sub(tail.len() as u64);
    if let Some(overlap) = (head.len() as u64).checked_sub(tail_start) {
        tail = &tail[usize::try_from(overlap)
            .unwrap_or(tail.len())
            .min(tail.len())..];
    }
    (head, tail)
}

/// How much of an output with `total_newlines` line breaks and `total_bytes`
/// bytes lies between `head` and `tail`: in lines when any line break falls
/// there, otherwise in bytes.
pub(crate) fn omitted(total_newlines: u64, total_bytes: u64, head: &str, tail: &str) -> String {
    let lines = total_newlines.saturating_sub(newlines(head) + newlines(tail));
    let (count, unit) = if lines > 0 {
        (lines, "line")
    } else {
        (
            total_bytes.saturating_sub((head.len() + tail.len()) as u64),
            "byte",
        )
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

/// The line that stands for the omitted middle and says how to read it.
pub(crate) fn marker(omitted: &str, read_more: &str) -> String {
    format!("[… {omitted} omitted. Read more: {read_more}]")
}

/// Joins a truncated output's ends around its marker, one line break apart.
pub(crate) fn render(head: &str, marker: &str, tail: &str) -> String {
    let mut text = head.to_owned();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(marker);
    if !tail.is_empty() {
        text.push('\n');
        text.push_str(tail);
    }
    text
}

pub(crate) fn newlines(text: &str) -> u64 {
    text.bytes().filter(|byte| *byte == b'\n').count() as u64
}

/// The longest prefix within `max_lines` lines and `max_bytes` bytes.
pub(crate) fn head_within(text: &str, max_lines: usize, max_bytes: usize) -> &str {
    let mut lines = 0;
    let mut end = 0;
    for (offset, character) in text.char_indices() {
        if lines >= max_lines || offset + character.len_utf8() > max_bytes {
            break;
        }
        end = offset + character.len_utf8();
        if character == '\n' {
            lines += 1;
        }
    }
    &text[..end]
}

/// The last whole lines of `text` within `max_lines` lines and `max_bytes`
/// bytes. A line cut by the byte limit is dropped unless it is all there is.
fn tail_within(text: &str, starts_at_line: bool, max_lines: usize, max_bytes: usize) -> &str {
    if max_lines == 0 || max_bytes == 0 {
        return "";
    }
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let at_line_start = if start == 0 {
        starts_at_line
    } else {
        text.as_bytes()[start - 1] == b'\n'
    };
    if !at_line_start
        && let Some(newline) = text[start..].find('\n')
        && start + newline + 1 < text.len()
    {
        start += newline + 1;
    }
    let tail = &text[start..];
    let body = tail.strip_suffix('\n').unwrap_or(tail);
    let mut seen = 0;
    for (index, byte) in body.bytes().enumerate().rev() {
        if byte == b'\n' {
            seen += 1;
            if seen == max_lines {
                return &tail[index + 1..];
            }
        }
    }
    tail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_lines: usize, max_bytes: usize, head: usize, tail: usize) -> PreviewLimits {
        PreviewLimits {
            max_lines,
            max_bytes,
            head_lines: head,
            tail_lines: tail,
        }
    }

    #[test]
    fn ends_follow_their_line_counts_and_share_bytes_in_that_ratio() {
        let text = (0..10)
            .map(|line| format!("line{line}\n"))
            .collect::<String>();
        let (head, tail) = split(
            &text,
            &text,
            true,
            text.len() as u64,
            limits(5, 1_000, 3, 1),
        );
        assert_eq!((head, tail), ("line0\nline1\nline2\n", "line9\n"));
        assert_eq!(omitted(10, text.len() as u64, head, tail), "6 lines");
        // 30 bytes split 3:1 leaves the tail 7 bytes; they start inside
        // line8, whose cut part is dropped.
        let (head, tail) = split(&text, &text, true, text.len() as u64, limits(5, 30, 3, 1));
        assert_eq!((head, tail), ("line0\nline1\nline2\n", "line9\n"));
        // A head-only split spends every byte on the head.
        let (head, tail) = split(&text, &text, true, text.len() as u64, limits(4, 9, 4, 0));
        assert_eq!((head, tail), ("line0\nlin", ""));
        assert_eq!(omitted(10, text.len() as u64, head, tail), "9 lines");
    }

    #[test]
    fn a_line_without_breaks_is_counted_in_bytes() {
        let text = "a€bcdefz";
        let (head, tail) = split(text, text, true, text.len() as u64, limits(1, 4, 1, 1));
        assert_eq!((head, tail), ("a", "fz"));
        assert_eq!(omitted(0, text.len() as u64, head, tail), "7 bytes");
        assert_eq!(render(head, "[…]", tail), "a\n[…]\nfz");
        assert_eq!(render("", "[…]", ""), "[…]");
    }
}

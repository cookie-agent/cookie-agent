//! The row budget of a tool's output, as in the TUI: an expanded call shows
//! its first lines and says how many more there are; a click on that shows
//! up to the larger budget. The budget counts lines as the blocks show
//! them, whatever their kind, and never looks at the tool.

use cookie_agent_protocol::{DisplayBlock, Open};

/// Lines an expanded call shows before "… N more lines (click to expand)".
pub const OUTPUT_LINES: usize = 64;
/// Lines it shows once that is clicked.
pub const ALL_OUTPUT_LINES: usize = 1024;

/// The lines `block` shows.
pub fn block_lines(block: &DisplayBlock) -> usize {
    match block {
        DisplayBlock::Text { text, .. }
        | DisplayBlock::Markdown { text, .. }
        | DisplayBlock::Code { text, .. } => text_lines(text),
        DisplayBlock::FileExcerpt { lines, .. } => lines.len() + 1,
        DisplayBlock::Terminal { segments, .. } => text_lines(
            &segments
                .iter()
                .map(|segment| segment.text.as_str())
                .collect::<String>(),
        ),
        DisplayBlock::Diff(diff) => {
            1 + diff
                .hunks
                .iter()
                .map(|hunk| 1 + hunk.lines.len())
                .sum::<usize>()
        }
        DisplayBlock::Json { value } => {
            text_lines(&serde_json::to_string_pretty(value).unwrap_or_default())
        }
        DisplayBlock::KeyValue { entries, .. } => entries.len(),
        DisplayBlock::Table { rows, .. } => 1 + rows.len(),
        _ => 1,
    }
}

fn text_lines(text: &str) -> usize {
    text.trim_end_matches('\n').lines().count().max(1)
}

/// The first `keep` lines of `text`.
fn first_lines(text: &str, keep: usize) -> String {
    text.lines().take(keep).collect::<Vec<_>>().join("\n")
}

/// `block` cut to its first `keep` lines (`keep` is less than its lines).
fn cut(block: &DisplayBlock, keep: usize) -> DisplayBlock {
    let mut block = block.clone();
    match &mut block {
        DisplayBlock::Text { text, .. }
        | DisplayBlock::Markdown { text, .. }
        | DisplayBlock::Code { text, .. } => *text = first_lines(text, keep),
        DisplayBlock::FileExcerpt { lines, .. } => lines.truncate(keep.saturating_sub(1)),
        DisplayBlock::Terminal { segments, .. } => {
            let mut left = keep;
            let mut kept = Vec::new();
            for segment in segments.iter() {
                if left == 0 {
                    break;
                }
                let mut segment = segment.clone();
                let breaks = segment.text.match_indices('\n').map(|(at, _)| at);
                if let Some(at) = breaks.clone().nth(left - 1) {
                    segment.text.truncate(at + 1);
                    left = 0;
                } else {
                    left -= breaks.count();
                }
                kept.push(segment);
            }
            *segments = kept;
        }
        DisplayBlock::Diff(diff) => {
            let mut left = keep.saturating_sub(1);
            for hunk in &mut diff.hunks {
                let room = left.saturating_sub(1);
                hunk.lines.truncate(room);
                left = left.saturating_sub(1 + hunk.lines.len());
            }
            diff.hunks.retain(|hunk| !hunk.lines.is_empty());
        }
        DisplayBlock::Json { value } => {
            let text = first_lines(
                &serde_json::to_string_pretty(value).unwrap_or_default(),
                keep,
            );
            block = DisplayBlock::Code {
                text,
                language: Some("json".into()),
                start_line: None,
                omitted_lines: None,
            };
        }
        DisplayBlock::KeyValue { entries, .. } => entries.truncate(keep),
        DisplayBlock::Table { rows, .. } => rows.truncate(keep.saturating_sub(1)),
        _ => {}
    }
    block
}

/// `blocks` within `budget` lines: whole blocks while they fit, the first
/// lines of the one that does not, and none after. Also the lines left
/// out, for "… N more lines".
pub fn within_budget(
    blocks: &[Open<DisplayBlock>],
    budget: usize,
) -> (Vec<Open<DisplayBlock>>, usize) {
    let mut left = budget;
    let mut omitted = 0;
    let mut shown = Vec::new();
    for block in blocks {
        let Open::Known(known) = block else {
            shown.push(block.clone());
            continue;
        };
        let lines = block_lines(known);
        if lines <= left {
            left -= lines;
            shown.push(block.clone());
        } else if left > 0 {
            shown.push(Open::Known(cut(known, left)));
            omitted += lines - left;
            left = 0;
        } else {
            omitted += lines;
        }
    }
    (shown, omitted)
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{DisplayBlock, Open, TerminalSegment, TerminalStream};

    use super::{block_lines, within_budget};

    fn text(lines: usize) -> Open<DisplayBlock> {
        let text = (1..=lines)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        Open::Known(DisplayBlock::text(text))
    }

    #[test]
    fn output_within_the_budget_shows_whole() {
        let blocks = [text(10), text(20)];
        let (shown, omitted) = within_budget(&blocks, 64);
        assert_eq!(shown, blocks);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn output_past_the_budget_is_cut_and_counted() {
        let blocks = [text(50), text(30), text(5)];
        let (shown, omitted) = within_budget(&blocks, 64);
        assert_eq!(shown.len(), 2);
        let Open::Known(cut) = &shown[1] else {
            panic!("a known block")
        };
        assert_eq!(block_lines(cut), 14);
        assert_eq!(omitted, 16 + 5);
    }

    #[test]
    fn terminal_output_is_cut_at_a_line_break() {
        let block = DisplayBlock::Terminal {
            segments: vec![
                TerminalSegment {
                    stream: TerminalStream::Stdout,
                    text: "a\nb\nc\n".into(),
                },
                TerminalSegment {
                    stream: TerminalStream::Stderr,
                    text: "d\ne\n".into(),
                },
            ],
            exit_code: Some(0),
            omitted_lines: None,
            full: None,
        };
        assert_eq!(block_lines(&block), 5);
        let (shown, omitted) = within_budget(&[Open::Known(block)], 4);
        assert_eq!(omitted, 1);
        let Open::Known(DisplayBlock::Terminal { segments, .. }) = &shown[0] else {
            panic!("a terminal block")
        };
        assert_eq!(segments[1].text, "d\n");
    }
}

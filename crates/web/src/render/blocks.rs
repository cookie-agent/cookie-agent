//! Display blocks as HTML, one renderer per kind. A renderer never looks at
//! which tool made a block; a block of a kind this build does not know
//! renders as nothing, as in the TUI (the daemon already sends an older
//! client a newer kind as its `text` fallback).

use cookie_agent_client_state::{FullContentState, session_actions::block_full};
use cookie_agent_protocol::{
    DiffBlock, DiffChange, DiffLineOp, DisplayBlock, KeyValueEntry, Open, TerminalSegment,
    TerminalStream,
};

use super::{escape, markdown::markdown_html, push_escaped, safe_link, sgr};

/// Where a block sits, for the actions it offers.
#[derive(Clone, Copy, Debug)]
pub struct BlockPlace<'a> {
    /// The tool call whose display holds the block; `None` for blocks that
    /// are no call's result, such as an approval's preview.
    pub call_id: Option<&'a str>,
    /// The block's index in its display.
    pub index: usize,
    /// The block's loaded complete content, when it was cut and asked for.
    pub full: Option<&'a FullContentState>,
}

impl BlockPlace<'_> {
    /// A place with no call and no complete content.
    pub const fn plain(index: usize) -> Self {
        Self {
            call_id: None,
            index,
            full: None,
        }
    }
}

const PRE: &str = "overflow-x-auto whitespace-pre rounded border border-toasted bg-cream p-2 font-mono text-sm dark:border-ember dark:bg-night";
const NOTE: &str = "text-xs text-ink dark:text-sand";

/// A stored file shown inline: a placeholder the page fills with the
/// artifact's image, video, audio, or download link once it is read.
pub fn media_html(sha256: &str, media_type: &str, byte_length: u64, label: Option<&str>) -> String {
    format!(
        "<div class=\"media\" data-media=\"{}\" data-media-type=\"{}\" data-alt=\"{}\"><span class=\"{NOTE}\">{} ({}, {})</span></div>",
        escape(sha256),
        escape(media_type),
        escape(label.unwrap_or_default()),
        escape(label.unwrap_or("media")),
        escape(media_type),
        escape(&cookie_agent_protocol::human_bytes(byte_length)),
    )
}

/// A plan as a checklist: each step with its status (done, in progress,
/// pending) and a high or low priority.
pub fn plan_html(entries: &[cookie_agent_protocol::PlanEntry]) -> String {
    use cookie_agent_protocol::{PlanEntryPriority, PlanEntryStatus};
    if entries.is_empty() {
        return format!("<div class=\"plan {NOTE}\">no steps</div>");
    }
    let mut html = String::from("<ul class=\"plan flex flex-col gap-0.5 text-sm\">");
    for entry in entries {
        let (icon, state, text) = match entry.status {
            PlanEntryStatus::Completed => (
                icondata::FaSquareCheckRegular,
                "completed",
                "line-through opacity-70",
            ),
            PlanEntryStatus::InProgress => (
                icondata::FaSquareCaretRightRegular,
                "in_progress",
                "font-semibold",
            ),
            PlanEntryStatus::Pending | PlanEntryStatus::Unknown => {
                (icondata::FaSquareRegular, "pending", "")
            }
        };
        let priority = match entry.priority {
            Some(PlanEntryPriority::High) => {
                format!(" <span class=\"{NOTE} text-caramel dark:text-maple\">high</span>")
            }
            Some(PlanEntryPriority::Low) => format!(" <span class=\"{NOTE}\">low</span>"),
            Some(PlanEntryPriority::Medium | PlanEntryPriority::Unknown) | None => String::new(),
        };
        html.push_str(&format!(
            "<li class=\"flex items-start gap-2\" data-plan-status=\"{state}\">{}<span class=\"sr-only\">{}:</span><span class=\"min-w-0 break-words {text}\">{}</span>{priority}</li>",
            crate::icons::svg(icon, "mt-0.5 shrink-0"),
            state.replace('_', " "),
            escape(&entry.content),
        ));
    }
    html.push_str("</ul>");
    html
}

/// Every block of a display, in order.
pub fn display_html<'a>(
    blocks: &[Open<DisplayBlock>],
    place: impl Fn(usize) -> BlockPlace<'a>,
) -> String {
    let mut html = String::new();
    for (index, block) in blocks.iter().enumerate() {
        if let Open::Known(block) = block {
            html.push_str(&block_html(block, place(index)));
        }
    }
    html
}

/// One block.
pub fn block_html(block: &DisplayBlock, place: BlockPlace<'_>) -> String {
    let body = match block {
        DisplayBlock::Text {
            text,
            omitted_lines,
        } => format!(
            "<pre class=\"{}\">{}</pre>{}",
            PRE.replace("whitespace-pre ", "whitespace-pre-wrap "),
            escape(text),
            omitted_note(*omitted_lines, "lines"),
        ),
        DisplayBlock::Markdown {
            text,
            omitted_lines,
        } => format!(
            "<div class=\"markdown\">{}</div>{}",
            markdown_html(text),
            omitted_note(*omitted_lines, "lines")
        ),
        DisplayBlock::Code {
            text,
            language,
            start_line,
            omitted_lines,
        } => format!(
            "{}{}",
            numbered_code(
                text.lines(),
                start_line.unwrap_or(1),
                start_line.is_some(),
                language.as_deref()
            ),
            omitted_note(*omitted_lines, "lines"),
        ),
        DisplayBlock::FileExcerpt {
            path,
            language,
            start_line,
            lines,
            total_lines,
            omitted_lines,
            ..
        } => {
            let end = start_line + lines.len().saturating_sub(1) as u64;
            let range = match total_lines {
                Some(total) => format!("lines {start_line}–{end} of {total}"),
                None => format!("lines {start_line}–{end}"),
            };
            format!(
                "<div class=\"{NOTE} font-mono\">{} · {range}</div>{}{}",
                escape(path),
                numbered_code(
                    lines.iter().map(String::as_str),
                    *start_line,
                    true,
                    language.as_deref().or_else(|| extension(path))
                ),
                omitted_note(*omitted_lines, "lines"),
            )
        }
        DisplayBlock::Diff(diff) => diff_html(diff),
        DisplayBlock::Terminal {
            segments,
            exit_code,
            omitted_lines,
            ..
        } => terminal_html(segments, *exit_code, *omitted_lines),
        DisplayBlock::KeyValue {
            entries,
            omitted_entries,
        } => key_value_html(entries, *omitted_entries),
        DisplayBlock::Table {
            columns,
            rows,
            omitted_rows,
        } => table_html(columns, rows, *omitted_rows),
        DisplayBlock::Plan { entries } => plan_html(entries),
        DisplayBlock::Json { value } => format!(
            "<pre class=\"{PRE}\"><code class=\"language-json\">{}</code></pre>",
            escape(&serde_json::to_string_pretty(value).unwrap_or_default())
        ),
        DisplayBlock::Media { artifact, alt } => media_html(
            &artifact.sha256,
            &artifact.media_type,
            artifact.byte_length,
            alt.as_deref().or(artifact.filename.as_deref()),
        ),
        DisplayBlock::Link { url, label } => {
            let text = escape(label.as_deref().unwrap_or(url));
            if safe_link(url) {
                format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"text-caramel underline hover:text-espresso dark:text-maple dark:hover:text-latte\">{text}</a>",
                    escape(url)
                )
            } else {
                format!("<span>{text} ({})</span>", escape(url))
            }
        }
        DisplayBlock::SessionRef {
            session_id,
            short_id,
            label,
        } => format!(
            "<button type=\"button\" data-action=\"open-session\" data-session=\"{session_id}\" class=\"rounded border border-crust px-2 py-1 font-mono text-sm hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke\">{}</button>",
            escape(
                label
                    .as_deref()
                    .or(short_id.as_deref())
                    .unwrap_or("open session")
            )
        ),
    };
    format!(
        "<div class=\"block my-1\" data-kind=\"{}\">{body}{}</div>",
        block.kind(),
        full_content_html(block, place)
    )
}

/// What a cut block with a `full` artifact offers: a button for its complete
/// content, and that content once loaded.
fn full_content_html(block: &DisplayBlock, place: BlockPlace<'_>) -> String {
    let (Some(call_id), Some(_)) = (place.call_id, block_full(block)) else {
        return String::new();
    };
    let action = |label: &str| {
        format!(
            "<button type=\"button\" data-action=\"full\" data-call=\"{}\" data-block=\"{}\" class=\"mt-1 min-h-11 rounded border border-crust px-3 py-1 text-sm hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke\">{label}</button>",
            escape(call_id),
            place.index
        )
    };
    match place.full {
        None => action("Show the complete output"),
        Some(FullContentState::Loading) => {
            format!("<div class=\"{NOTE}\">loading the complete output…</div>")
        }
        Some(FullContentState::Failed(error)) => format!(
            "<div class=\"{NOTE}\">could not read the complete output: {}</div>{}",
            escape(error),
            action("Try again")
        ),
        Some(FullContentState::Loaded { text, truncated }) => format!(
            "<pre class=\"{PRE}\">{}</pre>{}{}",
            escape(text),
            if *truncated {
                format!("<div class=\"{NOTE}\">only the first part is shown</div>")
            } else {
                String::new()
            },
            action("Hide the complete output")
        ),
    }
}

fn omitted_note(omitted: Option<u64>, unit: &str) -> String {
    match omitted {
        Some(count) if count > 0 => format!("<div class=\"{NOTE}\">… {count} {unit} omitted</div>"),
        _ => String::new(),
    }
}

fn extension(path: &str) -> Option<&str> {
    path.rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.contains('/'))
}

/// Code lines in one highlightable `<code>`, behind a gutter of line
/// numbers from `first` when `numbered`.
fn numbered_code<'a>(
    lines: impl Iterator<Item = &'a str>,
    first: u64,
    numbered: bool,
    language: Option<&str>,
) -> String {
    let lines = lines.collect::<Vec<_>>();
    let class = language
        .map(|language| {
            format!(
                " class=\"language-{}\"",
                escape(&language.to_ascii_lowercase())
            )
        })
        .unwrap_or_default();
    let code = escape(&lines.join("\n"));
    if !numbered {
        return format!("<pre class=\"{PRE}\"><code{class}>{code}</code></pre>");
    }
    let gutter = (0..lines.len() as u64)
        .map(|offset| (first + offset).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<div class=\"flex overflow-x-auto rounded border border-toasted bg-cream font-mono text-sm dark:border-ember dark:bg-night\"><pre class=\"select-none border-r border-toasted p-2 text-right text-ink dark:border-ember dark:text-sand\">{gutter}</pre><pre class=\"flex-1 whitespace-pre p-2\"><code{class}>{code}</code></pre></div>"
    )
}

/// A diff with the file's real line numbers: old on the left, new on the
/// right, and the hunks' context as the producer kept it.
pub fn diff_html(diff: &DiffBlock) -> String {
    let change = match diff.change {
        DiffChange::Created => "created",
        DiffChange::Deleted => "deleted",
        DiffChange::Renamed => "renamed",
        DiffChange::Modified | DiffChange::Unknown => "modified",
    };
    let path = match &diff.old_path {
        Some(old) => format!("{} → {}", escape(old), escape(&diff.path)),
        None => escape(&diff.path),
    };
    let mut html = format!(
        "<div class=\"{NOTE} font-mono\">{path} · {change} · <span class=\"text-green-700 dark:text-green-400\">+{}</span> <span class=\"text-red-700 dark:text-red-400\">−{}</span></div>",
        diff.added, diff.removed
    );
    if diff.hunks.is_empty() {
        html.push_str(&format!(
            "<div class=\"{NOTE}\">no line changes to show</div>"
        ));
    }
    html.push_str("<div class=\"overflow-x-auto rounded border border-toasted font-mono text-sm dark:border-ember\"><table class=\"diff w-full border-collapse\">");
    for hunk in &diff.hunks {
        html.push_str(&format!(
            "<tr class=\"bg-parchment text-ink dark:bg-char dark:text-sand\"><td colspan=\"3\" class=\"px-2\">@@ -{},{} +{},{} @@</td></tr>",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ));
        let (mut old, mut new) = (hunk.old_start, hunk.new_start);
        for line in &hunk.lines {
            let (old_number, new_number, sign, class) = match line.op {
                DiffLineOp::Context => {
                    let numbers = (Some(old), Some(new));
                    old += 1;
                    new += 1;
                    (numbers.0, numbers.1, ' ', "")
                }
                DiffLineOp::Remove => {
                    old += 1;
                    (
                        Some(old - 1),
                        None,
                        '-',
                        " class=\"bg-red-100 dark:bg-red-950\"",
                    )
                }
                DiffLineOp::Add => {
                    new += 1;
                    (
                        None,
                        Some(new - 1),
                        '+',
                        " class=\"bg-green-100 dark:bg-green-950\"",
                    )
                }
            };
            let number =
                |number: Option<u64>| number.map(|number| number.to_string()).unwrap_or_default();
            html.push_str(&format!(
                "<tr{class}><td class=\"select-none px-2 text-right text-ink dark:text-sand\">{}</td><td class=\"select-none px-2 text-right text-ink dark:text-sand\">{}</td><td class=\"whitespace-pre px-2\">{sign}{}</td></tr>",
                number(old_number),
                number(new_number),
                escape(&line.text)
            ));
        }
    }
    html.push_str("</table></div>");
    if let Some(omitted) = diff.omitted {
        html.push_str(&format!(
            "<div class=\"{NOTE}\">… {} hunks, {} lines omitted</div>",
            omitted.hunks, omitted.lines
        ));
    }
    html
}

fn terminal_html(
    segments: &[TerminalSegment],
    exit_code: Option<i32>,
    omitted_lines: Option<u64>,
) -> String {
    let mut out = format!("<pre class=\"terminal {PRE}\">");
    let mut stdout = sgr::Rendition::default();
    let mut stderr = sgr::Rendition::default();
    for segment in segments {
        match segment.stream {
            TerminalStream::Stderr => {
                out.push_str("<span class=\"stderr\">");
                sgr::push_sgr_html(&mut out, &segment.text, &mut stderr);
                out.push_str("</span>");
            }
            TerminalStream::Stdout | TerminalStream::Unknown => {
                sgr::push_sgr_html(&mut out, &segment.text, &mut stdout);
            }
        }
    }
    out.push_str("</pre>");
    out.push_str(&omitted_note(omitted_lines, "lines"));
    if let Some(code) = exit_code {
        out.push_str(&format!("<div class=\"{NOTE}\">exit code {code}</div>"));
    }
    out
}

fn key_value_html(entries: &[KeyValueEntry], omitted: Option<u64>) -> String {
    let mut html = String::from("<dl class=\"grid grid-cols-[auto_1fr] gap-x-3 text-sm\">");
    for entry in entries {
        html.push_str("<dt class=\"font-bold\">");
        push_escaped(&mut html, &entry.key);
        html.push_str("</dt><dd class=\"break-all\">");
        push_escaped(&mut html, &entry.value);
        html.push_str("</dd>");
    }
    html.push_str("</dl>");
    html.push_str(&omitted_note(omitted, "entries"));
    html
}

fn table_html(columns: &[String], rows: &[Vec<String>], omitted: Option<u64>) -> String {
    let mut html = String::from(
        "<div class=\"overflow-x-auto\"><table class=\"border-collapse text-sm\"><thead><tr>",
    );
    for column in columns {
        html.push_str("<th class=\"border border-toasted px-2 text-left dark:border-ember\">");
        push_escaped(&mut html, column);
        html.push_str("</th>");
    }
    html.push_str("</tr></thead><tbody>");
    for row in rows {
        html.push_str("<tr>");
        for cell in row {
            html.push_str("<td class=\"border border-toasted px-2 dark:border-ember\">");
            push_escaped(&mut html, cell);
            html.push_str("</td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div>");
    html.push_str(&omitted_note(omitted, "rows"));
    html
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        ArtifactRef, DiffAsOf, DiffBlock, DiffChange, DiffHunk, DiffLine, DiffLineOp, DisplayBlock,
        KeyValueEntry, Open, SessionId, TerminalSegment, TerminalStream,
    };

    use super::{BlockPlace, block_html, display_html};

    fn html(block: DisplayBlock) -> String {
        block_html(&block, BlockPlace::plain(0))
    }

    fn artifact() -> ArtifactRef {
        serde_json::from_value(serde_json::json!({
            "sha256": "a".repeat(64),
            "byte_length": 2048,
            "media_type": "image/png",
            "filename": "cat.png",
        }))
        .expect("artifact")
    }

    #[test]
    fn a_plan_is_a_checklist_with_each_steps_status() {
        use cookie_agent_protocol::{PlanEntry, PlanEntryPriority, PlanEntryStatus};
        let plan = html(DisplayBlock::Plan {
            entries: vec![
                PlanEntry {
                    content: "Read <it>".into(),
                    status: PlanEntryStatus::Completed,
                    priority: None,
                },
                PlanEntry {
                    content: "Edit it".into(),
                    status: PlanEntryStatus::InProgress,
                    priority: Some(PlanEntryPriority::High),
                },
                PlanEntry {
                    content: "Test it".into(),
                    status: PlanEntryStatus::Pending,
                    priority: None,
                },
            ],
        });
        let statuses = plan
            .match_indices("data-plan-status=\"")
            .map(|(at, marker)| {
                plan[at + marker.len()..]
                    .split('"')
                    .next()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        assert_eq!(statuses, ["completed", "in_progress", "pending"]);
        assert!(plan.contains("Read &lt;it&gt;"), "{plan}");
        assert!(plan.contains("line-through"), "{plan}");
        assert!(plan.contains(">high</span>"), "{plan}");
        assert!(
            plan.contains("<span class=\"sr-only\">in progress:</span>"),
            "{plan}"
        );
        assert!(
            html(DisplayBlock::Plan {
                entries: Vec::new()
            })
            .contains("no steps")
        );
    }

    #[test]
    fn every_kind_has_its_own_markup_and_escapes_its_text() {
        let text = html(DisplayBlock::Text {
            text: "<b>x</b>".into(),
            omitted_lines: Some(3),
        });
        assert!(text.contains("data-kind=\"text\""), "{text}");
        assert!(text.contains("&lt;b&gt;x&lt;/b&gt;"), "{text}");
        assert!(text.contains("3 lines omitted"), "{text}");

        let markdown = html(DisplayBlock::markdown("**bold** <i>raw</i>"));
        assert!(markdown.contains("<strong>bold</strong>"), "{markdown}");
        assert!(markdown.contains("&lt;i&gt;raw&lt;/i&gt;"), "{markdown}");

        let code = html(DisplayBlock::Code {
            text: "a\nb".into(),
            language: Some("Rust".into()),
            start_line: Some(7),
            omitted_lines: None,
        });
        assert!(code.contains("class=\"language-rust\""), "{code}");
        assert!(code.contains(">7\n8</pre>"), "{code}");

        let excerpt = html(DisplayBlock::FileExcerpt {
            path: "src/main.rs".into(),
            language: None,
            start_line: 10,
            lines: vec!["one".into(), "two".into()],
            total_lines: Some(40),
            omitted_lines: None,
            full: None,
        });
        assert!(
            excerpt.contains("src/main.rs · lines 10–11 of 40"),
            "{excerpt}"
        );
        assert!(excerpt.contains("language-rs"), "{excerpt}");
        assert!(excerpt.contains(">10\n11</pre>"), "{excerpt}");

        let key_value = html(DisplayBlock::KeyValue {
            entries: vec![KeyValueEntry {
                key: "status".into(),
                value: "200 <ok>".into(),
            }],
            omitted_entries: None,
        });
        assert!(
            key_value.contains("<dt class=\"font-bold\">status</dt>"),
            "{key_value}"
        );
        assert!(key_value.contains("200 &lt;ok&gt;"), "{key_value}");

        let table = html(DisplayBlock::Table {
            columns: vec!["name".into()],
            rows: vec![vec!["a&b".into()]],
            omitted_rows: Some(2),
        });
        assert!(table.contains(">name</th>"), "{table}");
        assert!(table.contains(">a&amp;b</td>"), "{table}");
        assert!(table.contains("2 rows omitted"), "{table}");

        let json = html(DisplayBlock::Json {
            value: serde_json::json!({"a": "<x>"}),
        });
        assert!(json.contains("language-json"), "{json}");
        assert!(json.contains("&quot;&lt;x&gt;&quot;"), "{json}");

        let media = html(DisplayBlock::Media {
            artifact: artifact(),
            alt: Some("a cat".into()),
        });
        assert!(
            media.contains(&format!("data-media=\"{}\"", "a".repeat(64))),
            "{media}"
        );
        assert!(media.contains("data-media-type=\"image/png\""), "{media}");

        let link = html(DisplayBlock::Link {
            url: "https://example.org".into(),
            label: Some("Docs".into()),
        });
        assert!(link.contains("href=\"https://example.org\""), "{link}");
        assert!(link.contains(">Docs</a>"), "{link}");
        let unsafe_link = html(DisplayBlock::Link {
            url: "javascript:alert(1)".into(),
            label: None,
        });
        assert!(!unsafe_link.contains("href"), "{unsafe_link}");

        let session = SessionId::new_v7();
        let session_ref = html(DisplayBlock::SessionRef {
            session_id: session,
            short_id: Some("abc".into()),
            label: None,
        });
        assert!(
            session_ref.contains("data-action=\"open-session\""),
            "{session_ref}"
        );
        assert!(
            session_ref.contains(&format!("data-session=\"{session}\"")),
            "{session_ref}"
        );
        assert!(session_ref.contains(">abc</button>"), "{session_ref}");
    }

    #[test]
    fn diffs_number_lines_as_the_file_does() {
        let diff = html(DisplayBlock::Diff(DiffBlock {
            path: "a.txt".into(),
            old_path: None,
            language: None,
            change: DiffChange::Modified,
            hunks: vec![DiffHunk {
                old_start: 40,
                old_lines: 3,
                new_start: 41,
                new_lines: 3,
                lines: vec![
                    DiffLine {
                        op: DiffLineOp::Context,
                        text: "same".into(),
                    },
                    DiffLine {
                        op: DiffLineOp::Remove,
                        text: "old".into(),
                    },
                    DiffLine {
                        op: DiffLineOp::Add,
                        text: "new".into(),
                    },
                    DiffLine {
                        op: DiffLineOp::Context,
                        text: "<tail>".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            omitted: None,
            as_of: DiffAsOf::Execute,
            full: None,
        }));
        assert!(diff.contains("a.txt · modified"), "{diff}");
        assert!(diff.contains("@@ -40,3 +41,3 @@"), "{diff}");
        let row = |old: &str, new: &str, text: &str| {
            format!(
                "<td class=\"select-none px-2 text-right text-ink dark:text-sand\">{old}</td><td class=\"select-none px-2 text-right text-ink dark:text-sand\">{new}</td><td class=\"whitespace-pre px-2\">{text}</td>"
            )
        };
        for expected in [
            row("40", "41", " same"),
            row("41", "", "-old"),
            row("", "42", "+new"),
            row("42", "43", " &lt;tail&gt;"),
        ] {
            assert!(diff.contains(&expected), "{expected}\n{diff}");
        }
    }

    #[test]
    fn terminals_keep_streams_apart_and_style_sgr() {
        let terminal = html(DisplayBlock::Terminal {
            segments: vec![
                TerminalSegment {
                    stream: TerminalStream::Stdout,
                    text: "\u{1b}[32mok\u{1b}[0m\n".into(),
                },
                TerminalSegment {
                    stream: TerminalStream::Stderr,
                    text: "warning <x>\n".into(),
                },
            ],
            exit_code: Some(1),
            omitted_lines: None,
            full: None,
        });
        assert!(
            terminal.contains("<span class=\"sgr-fg-2\">ok</span>"),
            "{terminal}"
        );
        assert!(
            terminal.contains("<span class=\"stderr\">warning &lt;x&gt;"),
            "{terminal}"
        );
        assert!(terminal.contains("exit code 1"), "{terminal}");
    }

    #[test]
    fn a_cut_block_offers_its_complete_content_and_unknown_blocks_show_nothing() {
        let blocks: Vec<Open<DisplayBlock>> = vec![
            Open::Known(DisplayBlock::Terminal {
                segments: Vec::new(),
                exit_code: None,
                omitted_lines: Some(100),
                full: Some(artifact()),
            }),
            serde_json::from_value(serde_json::json!({"type": "hologram", "beam": 1}))
                .expect("open block"),
        ];
        let html = display_html(&blocks, |index| BlockPlace {
            call_id: Some("call-1"),
            index,
            full: None,
        });
        assert!(html.contains("data-action=\"full\""), "{html}");
        assert!(
            html.contains("data-call=\"call-1\" data-block=\"0\""),
            "{html}"
        );
        assert!(!html.contains("hologram"), "{html}");
        assert_eq!(html.matches("data-kind=").count(), 1, "{html}");
    }
}

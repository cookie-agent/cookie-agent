//! The output an ACP agent streams for the commands it runs: its v2
//! agent-owned terminals.
//!
//! A tool call names its terminal in its content (`type: terminal`,
//! `terminalId`). `terminal_update` gives the terminal's `command`, an
//! `output` snapshot (base64 bytes that replace what came before), and,
//! once it ended, `exitStatus` (`exitCode`, `signal`);
//! `terminal_output_chunk` appends base64 bytes. Updates can come before
//! the call names the terminal. The client runs nothing: v2 has no client
//! terminals.
//!
//! Cookie shows the output as its own `bash` tool shows a command's: a
//! `terminal` display block that grows while the command runs (appends to
//! the call's live display), a live output hub for subscribers, and, at the
//! end, the output retained as the call's result through an output capture,
//! with an artifact holding what the preview cuts.

use cookie_agent_protocol::{
    DisplayBlock, MAX_TOOL_DELTA_BYTES, MAX_TOOL_DISPLAY_BYTES, OutputStream, PersistedToolResult,
    SessionId, TerminalSegment, TerminalStream, ToolCallId, ToolCompletionOutput, ToolOutputChunk,
    ToolOutputDeclaration,
};
use serde_json::Value;

use super::super::{Engine, output_capture::OutputCapture};
use crate::{ToolCompletion, ToolProgress, events::OutputHub};

/// How a command ended, as `exitStatus` said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Exit {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

/// What to apply to a call's terminal.
#[derive(Debug, Default)]
pub(super) struct TerminalMeta {
    /// The terminal's ID.
    pub id: Option<String>,
    /// Output to append.
    pub output: Option<String>,
    pub exit: Option<Exit>,
}

impl TerminalMeta {
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.output.is_none() && self.exit.is_none()
    }

    /// Adds `later` to what is waiting: its output after this one's, its
    /// exit in place of this one's.
    pub fn absorb(&mut self, later: Self) {
        self.id = later.id.or(self.id.take());
        if let Some(output) = later.output {
            self.output
                .get_or_insert_with(String::new)
                .push_str(&output);
        }
        if later.exit.is_some() {
            self.exit = later.exit;
        }
    }
}

/// What one `terminal_update` or `terminal_output_chunk` says.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TerminalUpdate {
    pub id: String,
    /// The decoded bytes: the whole output so far for a snapshot, else the
    /// bytes to append.
    pub output: Option<String>,
    /// `output` replaces what came before.
    pub snapshot: bool,
    pub exit: Option<Exit>,
}

impl TerminalUpdate {
    /// Reads update `kind` (`terminal_update` or `terminal_output_chunk`).
    pub(super) fn read(kind: &str, update: &Value) -> Option<Self> {
        use base64::Engine as _;
        let id = update
            .get("terminalId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())?
            .to_owned();
        let decode = |data: &Value| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_str()?)
                .ok()?;
            Some(String::from_utf8_lossy(&bytes).into_owned())
        };
        match kind {
            "terminal_output_chunk" => Some(Self {
                id,
                output: update.get("data").and_then(decode),
                snapshot: false,
                exit: None,
            }),
            "terminal_update" => Some(Self {
                id,
                output: update
                    .get("output")
                    .and_then(|output| output.get("data"))
                    .and_then(decode),
                snapshot: true,
                exit: update
                    .get("exitStatus")
                    .filter(|exit| exit.is_object())
                    .map(|exit| Exit {
                        code: exit
                            .get("exitCode")
                            .and_then(Value::as_i64)
                            .and_then(|code| i32::try_from(code).ok()),
                        signal: exit
                            .get("signal")
                            .and_then(Value::as_str)
                            .filter(|signal| !signal.is_empty())
                            .map(str::to_owned),
                    }),
            }),
            _ => None,
        }
    }

    /// What to apply, given that `written` bytes of output came before: a
    /// snapshot adds only what extends them.
    pub(super) fn meta(self, written: usize) -> TerminalMeta {
        let output = match self.output {
            Some(output) if self.snapshot => output
                .get(written..)
                .filter(|added| !added.is_empty())
                .map(str::to_owned),
            output => output.filter(|output| !output.is_empty()),
        };
        TerminalMeta {
            id: Some(self.id),
            output,
            exit: self.exit,
        }
    }
}

/// The terminal a tool call's content names.
pub(super) fn named(content: Option<&Vec<Value>>) -> Option<String> {
    content?.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("terminal"))
            .then(|| item.get("terminalId").and_then(Value::as_str))
            .flatten()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}

/// The output a call's terminal shows, up to the display budget.
#[derive(Debug, Default)]
struct Shown {
    text: String,
    truncated: bool,
    /// Lines past the budget, a last unfinished one included once the
    /// block is drawn.
    dropped_lines: u64,
    dropped_open: bool,
}

impl Shown {
    /// Adds `text`, already sanitized; returns the part the display kept.
    fn append<'a>(&mut self, text: &'a str) -> &'a str {
        let room = MAX_TOOL_DISPLAY_BYTES.saturating_sub(self.text.len());
        let end = if self.truncated {
            0
        } else if text.len() <= room {
            text.len()
        } else {
            cut(text, room)
        };
        let (kept, rest) = text.split_at(end);
        self.text.push_str(kept);
        if !rest.is_empty() {
            self.truncated = true;
            self.dropped_lines += rest.bytes().filter(|byte| *byte == b'\n').count() as u64;
            self.dropped_open = !rest.ends_with('\n');
        }
        kept
    }
}

/// The longest start of sanitized terminal text at most `limit` bytes long
/// that splits neither a character nor an SGR sequence.
fn cut(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    match text[..end].rfind('\u{1b}') {
        Some(start) if !text[start..end].contains('m') => start,
        _ => end,
    }
}

/// `text` in pieces of at most `limit` bytes, split at character
/// boundaries.
fn pieces(text: &str, limit: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = cut(rest, limit);
        if end == 0 {
            end = rest.len().min(limit);
            while !rest.is_char_boundary(end) {
                end += 1;
            }
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces
}

/// One call's terminal: what it shows, and the capture and hub its output
/// goes to.
pub(super) struct Terminal {
    pub id: String,
    tool_call_id: ToolCallId,
    capture: Option<OutputCapture>,
    hub: OutputHub,
    shown: Shown,
    pub exit: Option<Exit>,
    /// Whether any output arrived.
    pub written: bool,
}

impl Terminal {
    /// Opens the terminal of cookie tool call `tool_call_id`: its live hub,
    /// and the capture that retains its output.
    pub async fn open(
        engine: &Engine,
        session: SessionId,
        tool_call_id: ToolCallId,
        id: String,
        limits: crate::preview::PreviewLimits,
    ) -> Self {
        let hub = engine
            .inner
            .output
            .hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(tool_call_id)
            .or_insert_with(|| OutputHub::new(tool_call_id, 64 * 1024))
            .clone();
        hub.declare(&ToolOutputDeclaration::Single);
        let capture = OutputCapture::with_limits(
            engine.inner.artifacts.clone(),
            session,
            ToolOutputDeclaration::Single,
            limits,
        )
        .await
        .ok();
        if let Some(capture) = &capture {
            engine
                .inner
                .output
                .captures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(tool_call_id, capture.clone());
        }
        Self {
            id,
            tool_call_id,
            capture,
            hub,
            shown: Shown::default(),
            exit: None,
            written: false,
        }
    }

    /// Takes the command's output `data`: the capture retains it, the hub
    /// publishes it, and the display shows what fits. Returns what the
    /// display kept, sanitized, in pieces small enough for one append each.
    pub async fn write(&mut self, data: &str) -> Vec<String> {
        if data.is_empty() {
            return Vec::new();
        }
        self.written = true;
        for piece in pieces(data, MAX_TOOL_DELTA_BYTES) {
            let progress = ToolProgress {
                tool_call_id: self.tool_call_id,
                message: String::new(),
                display: None,
                output: vec![ToolOutputChunk {
                    stream: None,
                    text: piece.to_owned(),
                }],
            };
            let retained = match &self.capture {
                Some(capture) => capture.emit(progress, self.hub.clone(), None).await.is_ok(),
                None => false,
            };
            if !retained {
                // Without a capture the hub still shows it live.
                self.hub
                    .emit(OutputStream::from_channel(None), piece.as_bytes());
            }
        }
        let sanitized = cookie_agent_protocol::sanitize_terminal_text(data);
        let kept = self.shown.append(&sanitized);
        pieces(kept, cookie_agent_protocol::DisplayDelta::MAX_APPEND_BYTES)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Whether the call shows the terminal: once it has output or an exit.
    pub fn shows(&self) -> bool {
        self.written || self.exit.is_some()
    }

    /// The display blocks of the terminal: its output with the exit code,
    /// and a line for a command a signal ended.
    pub fn blocks(&self) -> Vec<DisplayBlock> {
        let omitted =
            self.shown.dropped_lines + u64::from(self.shown.truncated && self.shown.dropped_open);
        let mut blocks = vec![DisplayBlock::Terminal {
            segments: if self.shown.text.is_empty() {
                Vec::new()
            } else {
                vec![TerminalSegment {
                    stream: TerminalStream::Stdout,
                    text: self.shown.text.clone(),
                }]
            },
            exit_code: self.exit.as_ref().and_then(|exit| exit.code),
            omitted_lines: (omitted > 0).then_some(omitted),
            full: None,
        }];
        if let Some(signal) = self.exit.as_ref().and_then(|exit| exit.signal.as_deref()) {
            blocks.push(DisplayBlock::text(format!(
                "The command was terminated by signal {signal}."
            )));
        }
        blocks
    }

    /// What a failed call says when its content says nothing.
    pub fn failure(&self) -> Option<String> {
        let exit = self.exit.as_ref()?;
        match (&exit.signal, exit.code) {
            (Some(signal), _) => Some(format!("the command was terminated by signal {signal}")),
            (None, Some(code)) => Some(format!("the command exited with code {code}")),
            (None, None) => None,
        }
    }

    /// Ends the terminal with the call: `result` gets the retained output
    /// as its output (a preview, with where to read the rest) and the
    /// output's record, and the hub ends. `incomplete` when the call ended
    /// before the command did.
    pub async fn finish(
        self,
        engine: &Engine,
        mut result: PersistedToolResult,
        incomplete: bool,
    ) -> PersistedToolResult {
        let fallback = result.clone();
        let finished = match &self.capture {
            Some(capture) => {
                result.output.clear();
                capture
                    .finish(
                        ToolCompletion {
                            output: ToolCompletionOutput::Streamed,
                            result,
                            failed: false,
                        },
                        incomplete,
                    )
                    .await
                    .ok()
            }
            None => None,
        };
        if self.hub.finalize_once() {
            engine.retain_finalized_output_hub(self.tool_call_id);
        }
        finished.unwrap_or(fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_updates_carry_snapshots_chunks_and_the_exit() {
        let update = TerminalUpdate::read(
            "terminal_update",
            &serde_json::json!({
                "terminalId": "t1", "command": "make",
                "output": {"data": "b25lCnR3bwo="},
                "exitStatus": {"exitCode": 2, "signal": null},
            }),
        )
        .unwrap();
        assert!(update.snapshot);
        assert_eq!(update.output.as_deref(), Some("one\ntwo\n"));
        // A snapshot adds only what extends the output already shown.
        let meta = update.meta(4);
        assert_eq!(meta.output.as_deref(), Some("two\n"));
        assert_eq!(
            meta.exit,
            Some(Exit {
                code: Some(2),
                signal: None
            })
        );
        let chunk = TerminalUpdate::read(
            "terminal_output_chunk",
            &serde_json::json!({"terminalId": "t1", "data": "dGhyZWUK"}),
        )
        .unwrap();
        assert_eq!(chunk.meta(100).output.as_deref(), Some("three\n"));
        assert_eq!(
            TerminalUpdate::read("terminal_update", &serde_json::json!({})),
            None,
            "a terminal needs its ID"
        );
    }

    #[test]
    fn a_call_names_its_terminal_in_its_content() {
        let content = vec![
            serde_json::json!({"type": "content", "content": {"type": "text", "text": "x"}}),
            serde_json::json!({"type": "terminal", "terminalId": "t9"}),
        ];
        assert_eq!(named(Some(&content)).as_deref(), Some("t9"));
        assert_eq!(named(None), None);
    }

    #[test]
    fn the_display_keeps_output_up_to_its_budget_and_counts_the_rest() {
        let mut shown = Shown::default();
        let line = "x".repeat(1023) + "\n";
        let mut kept = 0;
        for _ in 0..(MAX_TOOL_DISPLAY_BYTES / 1024) {
            kept += shown.append(&line).len();
        }
        assert_eq!(kept, MAX_TOOL_DISPLAY_BYTES);
        assert_eq!(shown.append("more\nand an open line"), "");
        assert!(shown.truncated);
        assert_eq!(shown.dropped_lines, 1);
        assert!(shown.dropped_open);
    }

    #[test]
    fn pieces_split_at_character_boundaries() {
        let text = "é".repeat(5);
        let split = pieces(&text, 3);
        assert!(split.iter().all(|piece| piece.len() <= 3));
        assert_eq!(split.concat(), text);
    }
}

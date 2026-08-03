//! Render scheduling and terminal restoration helpers for the event loop.

use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags},
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode},
};

#[derive(Debug)]
pub(super) struct RenderScheduler {
    dirty: bool,
    immediate: bool,
    last_draw: Option<Instant>,
}

impl Default for RenderScheduler {
    fn default() -> Self {
        Self {
            dirty: true,
            immediate: true,
            last_draw: None,
        }
    }
}

impl RenderScheduler {
    pub(super) const FRAME_INTERVAL: Duration = Duration::from_millis(33);
    pub(super) fn mark_stream(&mut self) {
        self.dirty = true;
    }
    pub(super) fn mark_immediate(&mut self) {
        self.dirty = true;
        self.immediate = true;
    }
    pub(super) fn should_draw(&self, now: Instant) -> bool {
        self.dirty
            && (self.immediate
                || self
                    .last_draw
                    .is_none_or(|last| now.duration_since(last) >= Self::FRAME_INTERVAL))
    }
    pub(super) fn drew(&mut self, now: Instant) {
        self.dirty = false;
        self.immediate = false;
        self.last_draw = Some(now);
    }
}

#[derive(Default)]
pub(super) struct TerminalRestore {
    pub(super) raw_mode: bool,
    pub(super) alternate_screen: bool,
    pub(super) mouse_capture: bool,
    pub(super) bracketed_paste: bool,
    pub(super) keyboard_enhancement: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalCleanup {
    DisableMouseCapture,
    DisableBracketedPaste,
    PopKeyboardEnhancement,
    LeaveAlternateScreen,
    ShowCursor,
}

impl TerminalRestore {
    pub(super) fn cleanup_steps(&self) -> Vec<TerminalCleanup> {
        let mut steps = Vec::new();
        if self.mouse_capture {
            steps.push(TerminalCleanup::DisableMouseCapture);
        }
        if self.bracketed_paste {
            steps.push(TerminalCleanup::DisableBracketedPaste);
        }
        if self.keyboard_enhancement {
            steps.push(TerminalCleanup::PopKeyboardEnhancement);
        }
        if self.alternate_screen {
            steps.extend([
                TerminalCleanup::LeaveAlternateScreen,
                TerminalCleanup::ShowCursor,
            ]);
        }
        steps
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        for step in self.cleanup_steps() {
            match step {
                TerminalCleanup::DisableMouseCapture => {
                    let _ = execute!(stdout, DisableMouseCapture);
                }
                TerminalCleanup::DisableBracketedPaste => {
                    let _ = execute!(stdout, DisableBracketedPaste);
                }
                TerminalCleanup::PopKeyboardEnhancement => {
                    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
                }
                TerminalCleanup::LeaveAlternateScreen => {
                    let _ = execute!(stdout, LeaveAlternateScreen);
                }
                TerminalCleanup::ShowCursor => {
                    let _ = execute!(stdout, Show);
                }
            }
        }
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
    }
}

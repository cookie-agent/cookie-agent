//! Render scheduling and terminal restoration helpers for the event loop.

use std::{
    io, panic,
    sync::{Mutex, Once, OnceLock},
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

#[derive(Clone, Copy, Default)]
struct TerminalState {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
    keyboard_enhancement: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalCleanup {
    DisableMouseCapture,
    DisableBracketedPaste,
    PopKeyboardEnhancement,
    LeaveAlternateScreen,
    ShowCursor,
    DisableRawMode,
}

static TERMINAL_STATE: OnceLock<Mutex<TerminalState>> = OnceLock::new();
static INSTALL_PANIC_HOOK: Once = Once::new();

fn terminal_state() -> &'static Mutex<TerminalState> {
    TERMINAL_STATE.get_or_init(|| Mutex::new(TerminalState::default()))
}

#[derive(Default)]
pub(super) struct TerminalRestore;

impl TerminalRestore {
    pub(super) fn raw_mode_enabled(&mut self) {
        terminal_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .raw_mode = true;
    }

    pub(super) fn alternate_screen_entered(&mut self) {
        terminal_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .alternate_screen = true;
    }

    pub(super) fn mouse_capture_enabled(&mut self) {
        terminal_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mouse_capture = true;
    }

    pub(super) fn bracketed_paste_enabled(&mut self) {
        terminal_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bracketed_paste = true;
    }

    pub(super) fn keyboard_enhancement_enabled(&mut self) {
        terminal_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keyboard_enhancement = true;
    }
}

fn cleanup_steps(state: TerminalState) -> Vec<TerminalCleanup> {
    let mut steps = Vec::new();
    if state.mouse_capture {
        steps.push(TerminalCleanup::DisableMouseCapture);
    }
    if state.bracketed_paste {
        steps.push(TerminalCleanup::DisableBracketedPaste);
    }
    if state.keyboard_enhancement {
        steps.push(TerminalCleanup::PopKeyboardEnhancement);
    }
    if state.alternate_screen {
        steps.extend([
            TerminalCleanup::LeaveAlternateScreen,
            TerminalCleanup::ShowCursor,
        ]);
    }
    if state.raw_mode {
        steps.push(TerminalCleanup::DisableRawMode);
    }
    steps
}

fn cleanup_terminal_state_then<R>(
    state: &Mutex<TerminalState>,
    mut run_step: impl FnMut(TerminalCleanup),
    after_cleanup: impl FnOnce() -> R,
) -> R {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let active = std::mem::take(&mut *state);
    for step in cleanup_steps(active) {
        run_step(step);
    }
    let result = after_cleanup();
    drop(state);
    result
}

fn cleanup_terminal_then<R>(after_cleanup: impl FnOnce() -> R) -> R {
    let mut stdout = io::stdout();
    cleanup_terminal_state_then(
        terminal_state(),
        |step| match step {
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
            TerminalCleanup::DisableRawMode => {
                let _ = disable_raw_mode();
            }
        },
        after_cleanup,
    )
}

fn cleanup_terminal() {
    cleanup_terminal_then(|| ());
}

pub(super) fn install_terminal_panic_hook() {
    INSTALL_PANIC_HOOK.call_once(|| {
        let original_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            cleanup_terminal_then(|| original_hook(panic_info));
        }));
    });
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        cleanup_terminal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_is_idempotent_and_honors_active_flags() {
        let state = Mutex::new(TerminalState {
            raw_mode: true,
            alternate_screen: true,
            mouse_capture: true,
            bracketed_paste: true,
            keyboard_enhancement: true,
        });
        let mut steps = Vec::new();

        cleanup_terminal_state_then(&state, |step| steps.push(step), || ());
        cleanup_terminal_state_then(&state, |step| steps.push(step), || ());

        assert_eq!(
            steps,
            [
                TerminalCleanup::DisableMouseCapture,
                TerminalCleanup::DisableBracketedPaste,
                TerminalCleanup::PopKeyboardEnhancement,
                TerminalCleanup::LeaveAlternateScreen,
                TerminalCleanup::ShowCursor,
                TerminalCleanup::DisableRawMode,
            ]
        );
    }

    #[test]
    fn cleanup_serializes_through_the_after_cleanup_callback() {
        use std::{sync::Arc, time::Duration};

        let state = Arc::new(Mutex::new(TerminalState {
            alternate_screen: true,
            ..TerminalState::default()
        }));
        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let first_state = Arc::clone(&state);
        let first = std::thread::spawn(move || {
            cleanup_terminal_state_then(
                &first_state,
                |_| {},
                || {
                    first_entered_tx.send(()).expect("signal first callback");
                    release_first_rx.recv().expect("release first callback");
                },
            );
        });
        first_entered_rx.recv().expect("first callback entered");

        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let (second_entered_tx, second_entered_rx) = std::sync::mpsc::channel();
        let second_state = Arc::clone(&state);
        let second = std::thread::spawn(move || {
            second_started_tx.send(()).expect("signal second attempt");
            cleanup_terminal_state_then(
                &second_state,
                |_| {},
                || {
                    second_entered_tx.send(()).expect("signal second callback");
                },
            );
        });
        second_started_rx.recv().expect("second cleanup attempted");
        assert!(
            second_entered_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );

        release_first_tx.send(()).expect("release first cleanup");
        first.join().expect("first cleanup thread");
        second.join().expect("second cleanup thread");
        second_entered_rx.recv().expect("second callback entered");
    }

    #[test]
    fn cleanup_lock_is_released_when_the_callback_panics() {
        let state = Mutex::new(TerminalState::default());
        let panic = std::panic::catch_unwind(|| {
            cleanup_terminal_state_then(&state, |_| {}, || panic!("original hook panicked"));
        });
        assert!(panic.is_err());

        let mut callback_ran = false;
        cleanup_terminal_state_then(&state, |_| {}, || callback_ran = true);
        assert!(callback_ran);
    }

    #[test]
    fn installed_panic_hook_restores_before_chaining_and_is_idempotent() {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "ui::events::tests::panic_hook_subprocess_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("COOKIE_AGENT_PANIC_HOOK_TEST", "1")
            .output()
            .expect("run panic-hook subprocess");
        assert!(
            output.status.success(),
            "panic-hook subprocess failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8(output.stdout).expect("UTF-8 subprocess stdout");
        let leave_screen = stdout
            .find("\u{1b}[?1049l")
            .expect("leave alternate screen");
        let first_hook = stdout
            .find("ORIGINAL HOOK: first worker panic")
            .expect("first original hook output");
        assert!(leave_screen < first_hook);
        assert_eq!(stdout.matches("\u{1b}[?1049l").count(), 1);
        assert_eq!(stdout.matches("ORIGINAL HOOK:").count(), 2);
        assert!(stdout.contains("ORIGINAL HOOK: second worker panic"));
        assert!(stdout.contains("HELPER COMPLETE"));
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn panic_hook_subprocess_helper() {
        if std::env::var_os("COOKIE_AGENT_PANIC_HOOK_TEST").is_none() {
            return;
        }
        panic::set_hook(Box::new(|panic_info| {
            let message = panic_info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .unwrap_or("unknown panic");
            println!("ORIGINAL HOOK: {message}");
        }));
        install_terminal_panic_hook();
        let mut restore = TerminalRestore;
        restore.alternate_screen_entered();

        assert!(
            std::thread::spawn(|| panic!("first worker panic"))
                .join()
                .is_err()
        );
        assert!(
            std::thread::spawn(|| panic!("second worker panic"))
                .join()
                .is_err()
        );
        drop(restore);
        println!("HELPER COMPLETE");
    }
}

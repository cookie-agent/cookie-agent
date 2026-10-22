//! Render scheduling and terminal restoration helpers for the event loop.

use std::{
    io, panic,
    sync::{Mutex, Once, OnceLock},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
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
    /// Whether a frame is waiting to be drawn.
    pub(super) fn pending(&self) -> bool {
        self.dirty
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

/// Hands the terminal back, as an external editor needs it; see
/// [`resume_terminal`].
pub(super) fn suspend_terminal() {
    cleanup_terminal();
}

/// Takes the terminal again after [`suspend_terminal`], in the modes the TUI
/// runs in.
pub(super) fn resume_terminal() -> io::Result<()> {
    let mut stdout = io::stdout();
    let mut restore = std::mem::ManuallyDrop::new(TerminalRestore);
    enable_raw_mode()?;
    restore.raw_mode_enabled();
    execute!(stdout, EnterAlternateScreen)?;
    restore.alternate_screen_entered();
    execute!(stdout, EnableMouseCapture)?;
    restore.mouse_capture_enabled();
    // Best effort, as at start: a console without ANSI support refuses it.
    if execute!(stdout, EnableBracketedPaste).is_ok() {
        restore.bracketed_paste_enabled();
    }
    if enable_keyboard_enhancement(&mut stdout) {
        restore.keyboard_enhancement_enabled();
    }
    Ok(())
}

/// Asks the terminal to disambiguate escape codes, and says whether it did.
/// Best effort: Windows' console API refuses the request ("Keyboard
/// progressive enhancement not implemented for the legacy Windows API"),
/// and needs none, since it reports keys as distinct events already.
pub(super) fn enable_keyboard_enhancement(stdout: &mut io::Stdout) -> bool {
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
}

/// What the event loop does when the process is stopped and continued
/// (Unix job control). A stop the TUI did not ask for (`kill -STOP`, or a
/// suspended editor stopping the whole process group) leaves the terminal in
/// whatever state the shell put it in, so a SIGCONT re-enters the TUI's
/// modes and redraws. Ctrl+Z in the TUI hands the terminal back first and
/// re-enters it as soon as it is continued, so the SIGCONT that follows
/// needs nothing more.
#[derive(Debug, Default)]
pub(super) struct JobControl {
    /// The TUI stopped itself and already re-entered its modes; the next
    /// SIGCONT is that continue.
    resumed_self: bool,
}

/// One job-control step for the event loop to take.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JobStep {
    /// Re-enter the terminal modes (leaving them first, so nothing is
    /// pushed twice) and redraw everything.
    Reenter,
    /// Nothing to do.
    Nothing,
}

impl JobControl {
    /// The TUI stopped itself (Ctrl+Z) and was continued; it re-entered its
    /// modes on its own.
    pub(super) fn resumed_after_suspend(&mut self) {
        self.resumed_self = true;
    }

    /// The process was continued (SIGCONT).
    pub(super) fn continued(&mut self) -> JobStep {
        if std::mem::take(&mut self.resumed_self) {
            JobStep::Nothing
        } else {
            JobStep::Reenter
        }
    }
}

/// SIGCONT, as a stream the event loop selects on. Elsewhere than Unix it
/// never fires.
pub(super) struct ContinueSignal {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

impl ContinueSignal {
    /// Listens for SIGCONT; a listener that cannot be installed leaves job
    /// control to the shell. Must be called inside a Tokio runtime.
    pub(super) fn new() -> Self {
        #[cfg(unix)]
        {
            let kind =
                tokio::signal::unix::SignalKind::from_raw(rustix::process::Signal::CONT.as_raw());
            Self {
                signal: tokio::signal::unix::signal(kind).ok(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    pub(super) async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some(signal) = &mut self.signal {
            signal.recv().await;
            return;
        }
        std::future::pending::<()>().await;
    }
}

/// Hands the terminal back and stops the TUI's process group, as Ctrl+Z
/// does in a program that is not in raw mode; returns once the shell
/// continues it. A no-op elsewhere than Unix.
pub(super) fn suspend_process() -> io::Result<()> {
    #[cfg(unix)]
    {
        suspend_terminal();
        rustix::process::kill_current_process_group(rustix::process::Signal::TSTP)?;
    }
    Ok(())
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
    fn a_continue_reenters_the_terminal_unless_the_tui_stopped_itself() {
        let mut jobs = JobControl::default();
        // Stopped from outside (`kill -STOP`, an editor's Ctrl+Z): every
        // continue re-enters.
        assert_eq!(jobs.continued(), JobStep::Reenter);
        assert_eq!(jobs.continued(), JobStep::Reenter);
        // Ctrl+Z re-entered on its own once continued; only that one
        // continue is skipped.
        jobs.resumed_after_suspend();
        assert_eq!(jobs.continued(), JobStep::Nothing);
        assert_eq!(jobs.continued(), JobStep::Reenter);
    }

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

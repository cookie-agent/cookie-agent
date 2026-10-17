//! The signals that ask a cookie agent process to stop.

/// Listens for the signals that ask the process to stop: Ctrl-C (`SIGINT`),
/// and on Unix `SIGTERM` (`kill`, systemd, container runtimes) and `SIGHUP`
/// (its terminal closed); on Windows Ctrl-C, Ctrl-Break, a closed console
/// window, logoff, and system shutdown.
///
/// Creating it replaces each signal's default action for the rest of the
/// process, so a signal that arrives while nothing listens is lost: keep one
/// alive for as long as the process should stay stoppable.
pub struct StopSignals {
    #[cfg(unix)]
    signals: [tokio::signal::unix::Signal; 3],
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    ctrl_close: tokio::signal::windows::CtrlClose,
    #[cfg(windows)]
    ctrl_logoff: tokio::signal::windows::CtrlLogoff,
    #[cfg(windows)]
    ctrl_shutdown: tokio::signal::windows::CtrlShutdown,
}

impl StopSignals {
    /// Installs the handlers. Must be called inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when a handler cannot be installed.
    pub fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                signals: [
                    signal(SignalKind::interrupt())?,
                    signal(SignalKind::terminate())?,
                    signal(SignalKind::hangup())?,
                ],
            })
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows;
            Ok(Self {
                ctrl_c: windows::ctrl_c()?,
                ctrl_break: windows::ctrl_break()?,
                ctrl_close: windows::ctrl_close()?,
                ctrl_logoff: windows::ctrl_logoff()?,
                ctrl_shutdown: windows::ctrl_shutdown()?,
            })
        }
    }

    /// Waits for the next stop signal.
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let [interrupt, terminate, hangup] = &mut self.signals;
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
                _ = hangup.recv() => {}
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                _ = self.ctrl_c.recv() => {}
                _ = self.ctrl_break.recv() => {}
                _ = self.ctrl_close.recv() => {}
                _ = self.ctrl_logoff.recv() => {}
                _ = self.ctrl_shutdown.recv() => {}
            }
        }
    }
}

//! How a dropped connection is bridged (`docs/site/specs/runtimes.md`
//! section 6.4).
//!
//! The same policy applies to MCP servers (in the daemon's global runtime and
//! in coding runtimes) and to runtime instances: when a connected one drops,
//! its host reconnects it (restarting the process for stdio servers and
//! runtimes) a bounded number of times with growing backoff. While it reconnects its tools stay published and calls fail fast
//! with `runtime_unavailable`, so a brief drop never changes the tool list.
//! When every attempt fails, the caller drops the tools and reports the
//! failure to clients.
//!
//! [`ReconnectPolicy::run`] owns only the attempt loop; the caller supplies
//! one connection attempt and decides what the outcome means for its state.

use std::{future::Future, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

/// The waits before each reconnect attempt; their count is the number of
/// attempts.
#[derive(Clone, Debug)]
pub struct ReconnectPolicy {
    delays: Arc<[Duration]>,
}

impl Default for ReconnectPolicy {
    /// Three attempts after 1 s, 2 s, and 4 s.
    fn default() -> Self {
        let backoff =
            cookie_agent_identity::Backoff::new(Duration::from_secs(1), Duration::from_secs(4));
        Self::with_delays(backoff.delays().take(3).collect::<Vec<_>>())
    }
}

/// How a reconnect loop ended.
#[derive(Debug, Eq, PartialEq)]
pub enum ReconnectOutcome<T> {
    /// An attempt succeeded.
    Reconnected(T),
    /// Every attempt failed; `last_error` is the final attempt's reason.
    Exhausted { attempts: usize, last_error: String },
    /// The loop was cancelled (shutdown, or the connection was replaced).
    Cancelled,
}

impl ReconnectPolicy {
    /// A policy that waits `delays[i]` before attempt `i + 1`.
    pub fn with_delays(delays: impl Into<Arc<[Duration]>>) -> Self {
        Self {
            delays: delays.into(),
        }
    }

    /// Runs up to one attempt per delay, waiting that delay before each.
    /// `attempt` receives the 1-based attempt number and returns the
    /// connection or a human-readable reason.
    pub async fn run<T, F, Fut>(
        &self,
        cancel: &CancellationToken,
        mut attempt: F,
    ) -> ReconnectOutcome<T>
    where
        F: FnMut(usize) -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let mut last_error = String::new();
        for (index, delay) in self.delays.iter().enumerate() {
            tokio::select! {
                () = tokio::time::sleep(*delay) => {}
                () = cancel.cancelled() => return ReconnectOutcome::Cancelled,
            }
            let result = tokio::select! {
                result = attempt(index + 1) => result,
                () = cancel.cancelled() => return ReconnectOutcome::Cancelled,
            };
            match result {
                Ok(value) => return ReconnectOutcome::Reconnected(value),
                Err(error) => last_error = error,
            }
        }
        ReconnectOutcome::Exhausted {
            attempts: self.delays.len(),
            last_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use tokio_util::sync::CancellationToken;

    use super::{ReconnectOutcome, ReconnectPolicy};

    fn quick(attempts: usize) -> ReconnectPolicy {
        ReconnectPolicy::with_delays(vec![Duration::from_millis(1); attempts])
    }

    #[tokio::test]
    async fn the_first_successful_attempt_ends_the_loop() {
        let calls = AtomicUsize::new(0);
        let outcome = quick(3)
            .run(&CancellationToken::new(), |attempt| {
                calls.fetch_add(1, Ordering::Relaxed);
                async move {
                    if attempt == 2 {
                        Ok(attempt)
                    } else {
                        Err(format!("attempt {attempt} failed"))
                    }
                }
            })
            .await;
        assert_eq!(outcome, ReconnectOutcome::Reconnected(2));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn exhaustion_reports_every_attempt_and_the_last_reason() {
        let outcome = quick(2)
            .run(&CancellationToken::new(), |attempt| async move {
                Err::<(), _>(format!("attempt {attempt} failed"))
            })
            .await;
        assert_eq!(
            outcome,
            ReconnectOutcome::Exhausted {
                attempts: 2,
                last_error: "attempt 2 failed".into()
            }
        );
    }

    #[tokio::test]
    async fn cancellation_stops_waiting_between_attempts() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = ReconnectPolicy::with_delays(vec![Duration::from_secs(3600)])
            .run(&cancel, |_| async { Ok::<_, String>(()) })
            .await;
        assert_eq!(outcome, ReconnectOutcome::Cancelled);
    }
}

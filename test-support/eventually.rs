//! Waiting in tests for something to happen, by checking for it, instead of
//! sleeping a fixed time and hoping it did.
//!
//! Crates include this file as a module (`#[path]`), as they do
//! `config_harness.rs`. The checks run on tokio's clock: under
//! `start_paused` time the deadline is virtual like every other timer.

#![allow(dead_code)]

use std::{fmt::Display, future::Future, time::Duration};

/// How often a condition is checked again.
const POLL: Duration = Duration::from_millis(10);

/// Waits until `condition` holds, and panics naming `what` once `timeout`
/// passed without it.
pub async fn eventually(
    timeout: Duration,
    what: impl Display,
    mut condition: impl FnMut() -> bool,
) {
    eventually_some(timeout, what, || condition().then_some(())).await;
}

/// Waits until `probe` returns a value, and returns it; panics naming `what`
/// once `timeout` passed without one.
pub async fn eventually_some<T>(
    timeout: Duration,
    what: impl Display,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    eventually_async(timeout, what, || std::future::ready(probe())).await
}

/// [`eventually_some`] for a probe that has to await.
pub async fn eventually_async<T, F: Future<Output = Option<T>>>(
    timeout: Duration,
    what: impl Display,
    mut probe: impl FnMut() -> F,
) -> T {
    let found = tokio::time::timeout(timeout, async {
        loop {
            if let Some(value) = probe().await {
                return value;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("{what}: not within {timeout:?}"))
}

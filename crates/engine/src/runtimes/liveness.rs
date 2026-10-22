//! Telling a quiet runtime connection from a dead one.
//!
//! A connection can die without closing: a machine that sleeps or loses its
//! network leaves a half-open WebSocket, and a hung `ssh` keeps its pipes
//! open. Nothing arrives on either, and a request sent over one would wait
//! for good. So the daemon watches every runtime connection it keeps
//! ([`watch`]): once nothing has arrived for [`Liveness::ping_after`] it
//! sends `runtime/ping`, and once nothing has arrived for
//! [`Liveness::dead_after`] it ends the connection, which fails the
//! requests waiting on it as `runtime_unavailable` and makes the registry
//! reconnect or fail the runtime's instances. Any frame counts, an older
//! runtime's `unknown_method` answer to the ping included.
//!
//! A request that outlives its deadline marks the connection suspect
//! ([`Probe`]): the daemon pings it at once and ends it unless something
//! arrives within [`Liveness::probe_within`]. Handing out an instance of a
//! connection checks it the same way first ([`verify`]).

use std::{sync::Arc, time::Duration};

use cookie_agent_protocol::diagnostics::duration;
use cookie_agent_runtime_api::{
    protocol::{Empty, method},
    rpc::{Peer, RpcError},
};
use tokio::{sync::Notify, time::Instant};

/// How the daemon checks a runtime connection's liveness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Liveness {
    /// How long a connection may be quiet before the daemon pings it.
    pub(crate) ping_after: Duration,
    /// How long a connection may go without any frame before the daemon
    /// takes it for dead.
    pub(crate) dead_after: Duration,
    /// How long a suspect connection gets to answer a ping.
    pub(crate) probe_within: Duration,
}

impl Liveness {
    /// What every runtime connection the daemon keeps uses: a ping after
    /// 15 s of quiet, dead after 45 s of silence, and 10 s for a suspect
    /// connection to answer.
    pub(crate) const DEFAULT: Self = Self {
        ping_after: Duration::from_secs(15),
        dead_after: Duration::from_secs(45),
        probe_within: Duration::from_secs(10),
    };

    /// Why a connection that stayed silent was ended.
    fn silent(&self) -> String {
        format!("nothing arrived from it for {}", duration(self.dead_after))
    }

    /// Why a suspect connection that did not answer was ended.
    fn unanswered(&self) -> String {
        format!(
            "it did not answer a liveness check within {}",
            duration(self.probe_within)
        )
    }
}

/// Wakes a connection's watch to check it at once.
#[derive(Clone, Default)]
pub(crate) struct Probe(Arc<Notify>);

impl Probe {
    /// Marks the connection suspect, as when a request outlived its
    /// deadline.
    pub(crate) fn suspect(&self) {
        self.0.notify_one();
    }
}

/// Sends `runtime/ping` without waiting for the answer: whatever arrives
/// counts as hearing from the runtime.
fn ping(peer: &Peer, within: Duration) {
    let peer = peer.clone();
    tokio::spawn(async move {
        let _ = tokio::time::timeout(
            within,
            peer.request::<_, serde::de::IgnoredAny>(method::PING, &Empty {}),
        )
        .await;
    });
}

/// Watches `peer` until it closes: pings it once it has been quiet for
/// `liveness.ping_after`, and ends it once it has been silent for
/// `liveness.dead_after`, or `liveness.probe_within` after `probe` marked it
/// suspect without anything arriving.
pub(crate) async fn watch(peer: Peer, liveness: Liveness, probe: Probe) {
    // A ping sent since the last frame arrived.
    let mut pinged: Option<Instant> = None;
    // A probe in flight: when it was sent and when it gives up.
    let mut probing: Option<(Instant, Instant)> = None;
    loop {
        let heard = peer.last_heard();
        let now = Instant::now();
        if pinged.is_some_and(|sent| heard >= sent) {
            pinged = None;
        }
        if probing.is_some_and(|(sent, _)| heard >= sent) {
            probing = None;
        }
        if now >= heard + liveness.dead_after {
            peer.close_with(liveness.silent());
            return;
        }
        if probing.is_some_and(|(_, deadline)| now >= deadline) {
            peer.close_with(liveness.unanswered());
            return;
        }
        if pinged.is_none() && now >= heard + liveness.ping_after {
            ping(&peer, liveness.dead_after);
            pinged = Some(now);
        }
        let mut wake = heard + liveness.dead_after;
        if pinged.is_none() {
            wake = wake.min(heard + liveness.ping_after);
        }
        if let Some((_, deadline)) = probing {
            wake = wake.min(deadline);
        }
        tokio::select! {
            _ = peer.closed() => return,
            () = tokio::time::sleep_until(wake) => {}
            () = probe.0.notified() => {
                if probing.is_none() {
                    let now = Instant::now();
                    ping(&peer, liveness.probe_within);
                    probing = Some((now, now + liveness.probe_within));
                }
            }
        }
    }
}

/// Checks that `peer` is live before something new depends on it: one
/// heard from within `liveness.ping_after` is; another must answer a ping
/// within `liveness.probe_within`, or it is ended. The error is why the
/// connection is not live.
pub(crate) async fn verify(peer: &Peer, liveness: Liveness) -> Result<(), String> {
    if peer.is_closed() {
        return Err("its connection ended".to_owned());
    }
    if peer.last_heard().elapsed() < liveness.ping_after {
        return Ok(());
    }
    let sent = Instant::now();
    let answer = tokio::time::timeout(
        liveness.probe_within,
        peer.request::<_, serde::de::IgnoredAny>(method::PING, &Empty {}),
    )
    .await;
    match answer {
        Ok(Err(RpcError::Closed(reason))) => Err(reason),
        // Any answer, an error included, came from a live runtime.
        Ok(_) => Ok(()),
        Err(_) if peer.last_heard() > sent => Ok(()),
        Err(_) => {
            let reason = liveness.unanswered();
            peer.close_with(reason.clone());
            Err(reason)
        }
    }
}

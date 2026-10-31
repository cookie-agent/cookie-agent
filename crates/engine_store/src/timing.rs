//! Lock waits and log syncs, timed for the measurement step of the store
//! locks spec (`docs/site/specs/store-locks-and-durability.md`, section 5).
//!
//! Every wait and sync is counted per [`Meter`]. One over [`SLOW`] is
//! logged as it happens, naming what it was for; [`take_summary`] reports
//! each meter's count, total, and longest since it was last called.

use std::{
    fmt::Write as _,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// A wait or a sync this long is logged on its own.
pub const SLOW: Duration = Duration::from_millis(250);

/// What is timed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Meter {
    /// A command's wait for the engine's residency lock.
    ResidencyWait,
    /// How long the residency lock was held: a command opening its session,
    /// or the idle janitor draining a session before evicting it.
    ResidencyHold,
    /// A structural change's wait for the store's mutation lock, exclusive.
    MutationExclusiveWait,
    /// A session write's wait for the store's mutation lock, shared.
    MutationSharedWait,
    /// A session write's wait for its session's slot.
    SessionSlotWait,
    /// One `fdatasync` of a session log.
    LogSync,
}

const METERS: [Meter; 6] = [
    Meter::ResidencyWait,
    Meter::ResidencyHold,
    Meter::MutationExclusiveWait,
    Meter::MutationSharedWait,
    Meter::SessionSlotWait,
    Meter::LogSync,
];

impl Meter {
    fn name(self) -> &'static str {
        match self {
            Self::ResidencyWait => "residency lock wait",
            Self::ResidencyHold => "residency lock hold",
            Self::MutationExclusiveWait => "store lock wait (exclusive)",
            Self::MutationSharedWait => "store lock wait (shared)",
            Self::SessionSlotWait => "session slot wait",
            Self::LogSync => "log sync",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

struct Counters {
    count: AtomicU64,
    total_micros: AtomicU64,
    max_micros: AtomicU64,
    slow: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: Counters = Counters {
    count: AtomicU64::new(0),
    total_micros: AtomicU64::new(0),
    max_micros: AtomicU64::new(0),
    slow: AtomicU64::new(0),
};

static COUNTERS: [Counters; METERS.len()] = [ZERO; METERS.len()];

/// Records one wait or sync of `meter` that took `elapsed`, and logs it when
/// it took [`SLOW`] or longer, with what `subject` names.
pub fn record(meter: Meter, elapsed: Duration, subject: impl FnOnce() -> String) {
    let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    let counters = &COUNTERS[meter.index()];
    counters.count.fetch_add(1, Ordering::Relaxed);
    counters.total_micros.fetch_add(micros, Ordering::Relaxed);
    counters.max_micros.fetch_max(micros, Ordering::Relaxed);
    if elapsed >= SLOW {
        counters.slow.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "cookie-agent: slow {}: {} ms ({})",
            meter.name(),
            elapsed.as_millis(),
            subject()
        );
    }
}

/// Runs `work` and records how long it took as `meter`.
pub fn time<T>(meter: Meter, subject: impl FnOnce() -> String, work: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let value = work();
    record(meter, started.elapsed(), subject);
    value
}

/// Each meter's count, total, longest, and slow count since the last call,
/// one line, or `None` when nothing was recorded. Resets the counters.
pub fn take_summary() -> Option<String> {
    let mut line = String::new();
    for meter in METERS {
        let counters = &COUNTERS[meter.index()];
        let count = counters.count.swap(0, Ordering::Relaxed);
        let total = counters.total_micros.swap(0, Ordering::Relaxed);
        let max = counters.max_micros.swap(0, Ordering::Relaxed);
        let slow = counters.slow.swap(0, Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        if !line.is_empty() {
            line.push_str("; ");
        }
        let _ = write!(
            line,
            "{}: {count}, {} ms in all, longest {} ms, {slow} slow",
            meter.name(),
            total / 1000,
            max / 1000
        );
    }
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Meter, record, take_summary};

    /// The counters are global and other tests of this crate record log
    /// syncs, so this uses the residency meters, which only the engine
    /// records.
    #[test]
    fn a_summary_reports_each_meter_and_resets() {
        record(Meter::ResidencyHold, Duration::from_millis(3), String::new);
        record(Meter::ResidencyHold, Duration::from_millis(7), String::new);
        record(Meter::ResidencyWait, Duration::from_millis(300), || {
            "a session".into()
        });
        let summary = take_summary().expect("a summary");
        assert!(
            summary.contains("residency lock hold: 2, 10 ms in all, longest 7 ms, 0 slow"),
            "{summary}"
        );
        assert!(
            summary.contains("residency lock wait: 1, 300 ms in all, longest 300 ms, 1 slow"),
            "{summary}"
        );
        let after = take_summary().unwrap_or_default();
        assert!(!after.contains("residency"), "{after}");
    }
}

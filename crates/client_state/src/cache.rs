//! How hot a session's prompt cache is, for the cache badge both clients
//! show (`docs/site/specs/cache-aware-idle.md` section 4.5). The daemon
//! stamps each request's TTL and plans the compaction before expiry; this
//! only counts down.

use std::time::Duration;

use cookie_agent_protocol::{CacheTtlSource, CacheTtlStamp, PlannedCacheCompaction};
use jiff::Timestamp;

/// The prompt cache of a session's latest request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheClock {
    /// When the request went out.
    pub sent_at: Timestamp,
    pub stamp: CacheTtlStamp,
}

/// How much of its TTL a cache has left.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheHeat {
    /// More than half.
    Hot,
    /// A fifth to a half.
    Warm,
    /// Less than a fifth.
    Cooling,
    /// None: the next request writes the cache again.
    Expired,
}

/// What the cache badge shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheBadge {
    /// A compaction before expiry is running.
    Compacting,
    /// A run is in progress: the cache is in use, with no countdown.
    InUse { assumed: bool },
    /// The cache never expires.
    Forever { assumed: bool },
    /// Counting down.
    Counting {
        heat: CacheHeat,
        /// Time left; zero once expired.
        left: Duration,
        /// The TTL is assumed or declared, not set on the request.
        assumed: bool,
        /// A compaction before expiry is planned.
        planned: bool,
    },
}

impl CacheClock {
    /// When the cache expires; `None` when it never does.
    #[must_use]
    pub fn expires_at(&self) -> Option<Timestamp> {
        self.stamp.expires_at(self.sent_at)
    }

    /// The badge at `now`, with the daemon's `plan`, while a run is
    /// `running` or not.
    #[must_use]
    pub fn badge(
        &self,
        now: Timestamp,
        running: bool,
        plan: Option<&PlannedCacheCompaction>,
    ) -> CacheBadge {
        let assumed = self.stamp.source != CacheTtlSource::Set;
        if plan.is_some_and(|plan| plan.running) {
            return CacheBadge::Compacting;
        }
        if running {
            return CacheBadge::InUse { assumed };
        }
        let Some(expires_at) = self.expires_at() else {
            return CacheBadge::Forever { assumed };
        };
        let left = Duration::try_from(expires_at.duration_since(now)).unwrap_or_default();
        let ttl = Duration::from_secs(self.stamp.ttl_secs);
        CacheBadge::Counting {
            heat: heat(left, ttl),
            left,
            assumed,
            planned: plan.is_some(),
        }
    }
}

/// The heat of a cache with `left` of its `ttl` to go.
#[must_use]
pub fn heat(left: Duration, ttl: Duration) -> CacheHeat {
    if left.is_zero() || ttl.is_zero() {
        return CacheHeat::Expired;
    }
    // Compared in whole milliseconds: `left / ttl` against a half and a
    // fifth.
    let left = left.as_millis();
    let ttl = ttl.as_millis();
    if left * 2 > ttl {
        CacheHeat::Hot
    } else if left * 5 > ttl {
        CacheHeat::Warm
    } else {
        CacheHeat::Cooling
    }
}

/// The countdown's text: minutes, rounded up (`41m`); whole hours above 90
/// minutes, rounded to the nearest (`11h`); seconds in the last minute
/// (`45s`).
#[must_use]
pub fn countdown(left: Duration) -> String {
    let seconds = left.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds > 90 * 60 {
        return format!("{}h", (seconds + 30 * 60) / 3_600);
    }
    format!("{}m", seconds.div_ceil(60))
}

/// The badge's text: the countdown with `~` before an assumed TTL,
/// `compacting`, `∞`, or `cache expired`; empty while in use.
#[must_use]
pub fn badge_text(badge: &CacheBadge) -> String {
    match badge {
        CacheBadge::Compacting => "compacting".into(),
        CacheBadge::InUse { .. } => String::new(),
        CacheBadge::Forever { .. } => "∞".into(),
        CacheBadge::Counting {
            heat: CacheHeat::Expired,
            ..
        } => "cache expired".into(),
        CacheBadge::Counting { left, assumed, .. } => {
            format!("{}{}", if *assumed { "~" } else { "" }, countdown(*left))
        }
    }
}

/// Where a TTL came from, for the tooltip and the usage card.
#[must_use]
pub fn source_label(source: CacheTtlSource) -> &'static str {
    match source {
        CacheTtlSource::Set => "set on the request",
        CacheTtlSource::Declared => "declared in the configuration",
        CacheTtlSource::Assumed => "assumed",
    }
}

/// A TTL's text: `1h`, `30m`, `90s`, or `never expires`.
#[must_use]
pub fn ttl_label(ttl_secs: u64) -> String {
    match ttl_secs {
        0 => "never expires".into(),
        seconds if seconds % 3_600 == 0 => format!("{}h", seconds / 3_600),
        seconds if seconds % 60 == 0 => format!("{}m", seconds / 60),
        seconds => format!("{seconds}s"),
    }
}

/// `at` in `zone` as `HH:MM`, the date left out for today's.
fn clock_time(at: Timestamp, now: Timestamp, zone: &jiff::tz::TimeZone) -> String {
    let time = zone.to_datetime(at);
    if time.date() == zone.to_datetime(now).date() {
        format!("{:02}:{:02}", time.hour(), time.minute())
    } else {
        crate::cron::local_minute(at, zone)
    }
}

/// The lines of the badge's tooltip and of the usage panel's cache card:
/// its state; the request, its TTL and where it came from, and expiry;
/// and the planned compaction, with the scheduled prompt that decided it.
#[must_use]
pub fn detail_lines(
    clock: &CacheClock,
    badge: &CacheBadge,
    plan: Option<&PlannedCacheCompaction>,
    now: Timestamp,
    zone: &jiff::tz::TimeZone,
) -> Vec<String> {
    let mut lines = vec![match badge {
        CacheBadge::Compacting => "Compacting before the prompt cache expires".to_owned(),
        CacheBadge::InUse { .. } => "Prompt cache in use".to_owned(),
        CacheBadge::Forever { .. } => "Prompt cache warm; it never expires".to_owned(),
        CacheBadge::Counting {
            heat: CacheHeat::Expired,
            ..
        } => "Prompt cache expired: the next prompt caches the context again".to_owned(),
        CacheBadge::Counting { heat, left, .. } => format!(
            "Prompt cache {} · {} left",
            match heat {
                CacheHeat::Hot => "hot",
                CacheHeat::Warm => "warm",
                _ => "cooling",
            },
            countdown(*left)
        ),
    }];
    let mut request = format!(
        "Last request {} · {} TTL ({})",
        clock_time(clock.sent_at, now, zone),
        ttl_label(clock.stamp.ttl_secs),
        source_label(clock.stamp.source)
    );
    if let Some(expires_at) = clock.expires_at() {
        request.push_str(&format!(" · expires {}", clock_time(expires_at, now, zone)));
    }
    lines.push(request);
    if let Some(plan) = plan.filter(|plan| !plan.running) {
        lines.push(match plan.next_fire {
            Some(fire) => format!(
                "Next scheduled prompt at {}, after the cache expires: compacts at {}",
                clock_time(fire, now, zone),
                clock_time(plan.at, now, zone)
            ),
            None => format!(
                "Compacts at {}, before the cache expires",
                clock_time(plan.at, now, zone)
            ),
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(ttl_secs: u64, source: CacheTtlSource) -> CacheClock {
        CacheClock {
            sent_at: Timestamp::from_second(1_800_000_000).unwrap(),
            stamp: CacheTtlStamp { ttl_secs, source },
        }
    }

    fn after(clock: &CacheClock, seconds: i64) -> Timestamp {
        clock
            .sent_at
            .checked_add(jiff::SignedDuration::from_secs(seconds))
            .unwrap()
    }

    #[test]
    fn heat_changes_at_a_half_and_a_fifth_of_the_ttl_and_at_expiry() {
        let ttl = Duration::from_secs(1_000);
        assert_eq!(heat(Duration::from_secs(501), ttl), CacheHeat::Hot);
        assert_eq!(heat(Duration::from_secs(500), ttl), CacheHeat::Warm);
        assert_eq!(heat(Duration::from_secs(201), ttl), CacheHeat::Warm);
        assert_eq!(heat(Duration::from_secs(200), ttl), CacheHeat::Cooling);
        assert_eq!(heat(Duration::from_millis(1), ttl), CacheHeat::Cooling);
        assert_eq!(heat(Duration::ZERO, ttl), CacheHeat::Expired);
    }

    #[test]
    fn the_countdown_counts_minutes_hours_and_its_last_seconds() {
        assert_eq!(countdown(Duration::from_secs(45)), "45s");
        assert_eq!(countdown(Duration::from_secs(60)), "1m");
        assert_eq!(countdown(Duration::from_secs(40 * 60 + 1)), "41m");
        assert_eq!(countdown(Duration::from_secs(90 * 60)), "90m");
        assert_eq!(countdown(Duration::from_secs(90 * 60 + 1)), "2h");
        assert_eq!(countdown(Duration::from_secs(11 * 3_600 + 20 * 60)), "11h");
    }

    #[test]
    fn the_badge_follows_the_run_the_plan_and_the_clock() {
        let set = clock(3_600, CacheTtlSource::Set);
        let plan = PlannedCacheCompaction {
            at: after(&set, 3_300),
            expires_at: after(&set, 3_600),
            next_fire: None,
            job_id: None,
            running: false,
        };
        let counting = set.badge(after(&set, 1_140), false, Some(&plan));
        assert_eq!(
            counting,
            CacheBadge::Counting {
                heat: CacheHeat::Hot,
                left: Duration::from_secs(2_460),
                assumed: false,
                planned: true,
            }
        );
        assert_eq!(badge_text(&counting), "41m");
        assert_eq!(
            set.badge(after(&set, 10), true, None),
            CacheBadge::InUse { assumed: false }
        );
        let running = PlannedCacheCompaction {
            running: true,
            ..plan
        };
        assert_eq!(
            set.badge(after(&set, 3_400), false, Some(&running)),
            CacheBadge::Compacting
        );
        assert!(matches!(
            set.badge(after(&set, 3_601), false, None),
            CacheBadge::Counting {
                heat: CacheHeat::Expired,
                ..
            }
        ));
        let assumed = clock(1_800, CacheTtlSource::Assumed);
        assert!(matches!(
            assumed.badge(after(&assumed, 360), false, None),
            CacheBadge::Counting { assumed: true, .. }
        ));
        let forever = clock(0, CacheTtlSource::Declared);
        assert_eq!(
            forever.badge(after(&forever, 99_999), false, None),
            CacheBadge::Forever { assumed: true }
        );
    }

    #[test]
    fn the_details_name_the_plan_only_while_it_waits() {
        let set = clock(3_600, CacheTtlSource::Set);
        let now = after(&set, 60);
        let zone = jiff::tz::TimeZone::UTC;
        let plan = PlannedCacheCompaction {
            at: after(&set, 3_300),
            expires_at: after(&set, 3_600),
            next_fire: Some(after(&set, 7_200)),
            job_id: None,
            running: false,
        };
        let badge = set.badge(now, false, Some(&plan));
        assert_eq!(detail_lines(&set, &badge, Some(&plan), now, &zone).len(), 3);
        assert_eq!(detail_lines(&set, &badge, None, now, &zone).len(), 2);
        let running = PlannedCacheCompaction {
            running: true,
            ..plan
        };
        let compacting = set.badge(now, false, Some(&running));
        assert_eq!(
            detail_lines(&set, &compacting, Some(&running), now, &zone).len(),
            2
        );
    }

    #[test]
    fn ttls_read_in_their_largest_whole_unit() {
        assert_eq!(ttl_label(3_600), "1h");
        assert_eq!(ttl_label(1_800), "30m");
        assert_eq!(ttl_label(90), "90s");
        assert_eq!(ttl_label(0), "never expires");
    }
}

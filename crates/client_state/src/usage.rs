//! Usage figures every client shows the same way: token counts with
//! thousands separators or in short form, costs, and the prompt-cache hit
//! rate of a session, a session tree, or one model of either
//! (`session.usage`, `session.tree_usage`).
//!
//! The hit rate is the share of the prompt the provider read from its
//! cache:
//!
//! ```text
//! hit rate = cache-read input tokens ÷ inclusive input tokens
//! ```
//!
//! where inclusive input counts every prompt token: the uncached ones plus
//! the cache reads and the cache writes. Adapters report `input_tokens`
//! inclusive (it is never summed from its parts), so `input_tokens`
//! already contains `cache_read_tokens` and `cache_write_tokens`. The
//! daemon computes the rate (`cache_hit_rate`) over the requests whose
//! provider reported cache reads, so a request that said nothing about the
//! cache does not count as a miss; a client shows that rate as it comes
//! and never recomputes it from the totals, which include such requests.

use cookie_agent_protocol::{ModelUsageRollup, UsageRollup};

/// What a client shows for an unavailable hit rate, where it has room for
/// a dash: no input yet, or no request reported cache use.
pub const NO_HIT_RATE: &str = "—";

/// A rollup's prompt-cache hit rate with the figures behind it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CacheHit {
    /// The share of input read from the cache, from 0 to 1; `None` when
    /// there was no input or no request reported cache use. A reported
    /// zero stays a 0 % rate.
    pub rate: Option<f64>,
    /// Inclusive input tokens: uncached, cache reads, and cache writes.
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl CacheHit {
    /// The hit rate of a session's or a session tree's rollup.
    pub fn of(usage: &UsageRollup) -> Self {
        Self::new(
            usage.cache_hit_rate,
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
        )
    }

    /// The hit rate of one model's row of a rollup.
    pub fn of_model(usage: &ModelUsageRollup) -> Self {
        Self::new(
            usage.cache_hit_rate,
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
        )
    }

    /// `reported` is the daemon's `cache_hit_rate`; without input there is
    /// nothing to rate, whatever it says.
    pub fn new(
        reported: Option<f64>,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) -> Self {
        let rate = reported
            .filter(|rate| input_tokens > 0 && rate.is_finite())
            .map(|rate| rate.clamp(0.0, 1.0));
        Self {
            rate,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
        }
    }

    /// The rate as a percentage with one decimal, as `82.5%`.
    pub fn percent(&self) -> Option<String> {
        self.rate.map(|rate| format!("{:.1}%", rate * 100.0))
    }

    /// The rate, or [`NO_HIT_RATE`].
    pub fn label(&self) -> String {
        self.percent().unwrap_or_else(|| NO_HIT_RATE.to_owned())
    }

    /// The figures behind the rate, in a sentence for a tooltip or a note.
    pub fn detail(&self) -> String {
        let Some(rate) = self.rate else {
            return if self.input_tokens == 0 {
                "No input tokens yet.".to_owned()
            } else {
                "The provider reported no prompt-cache use.".to_owned()
            };
        };
        let mut detail = format!(
            "{} of {} input tokens read from the prompt cache; {} written to it.",
            format_count(self.cache_read_tokens),
            format_count(self.input_tokens),
            format_count(self.cache_write_tokens),
        );
        // The daemon leaves out requests that reported nothing about the
        // cache; the totals keep them, so the two can disagree.
        let totals =
            self.cache_read_tokens.min(self.input_tokens) as f64 / self.input_tokens as f64;
        if (totals - rate).abs() >= 0.0005 {
            detail.push_str(" Requests that did not report cache use are left out of the rate.");
        }
        detail
    }
}

/// `count` with a comma between each group of three digits, as `12,345`.
pub fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let first = digits.len() % 3;
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    output.push_str(&digits[..first]);
    for chunk in digits.as_bytes()[first..].chunks(3) {
        if !output.is_empty() {
            output.push(',');
        }
        output.push_str(std::str::from_utf8(chunk).expect("decimal digits are UTF-8"));
    }
    output
}

/// `tokens` in short form: as it is below a thousand, else in thousands
/// with one decimal, as `12.5K`.
pub fn format_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    }
}

/// A cost in US dollars: to the cent from a cent up, else to four decimals,
/// as `$0.0040`.
pub fn format_cost_usd(cost: f64) -> String {
    if cost >= 0.01 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_is_the_daemons_share_of_inclusive_input_read_from_the_cache() {
        let hit = CacheHit::new(Some(0.8), 10_000, 8_000, 500);
        assert_eq!(hit.percent().as_deref(), Some("80.0%"));
        assert_eq!(hit.label(), "80.0%");
        assert_eq!(
            hit.detail(),
            "8,000 of 10,000 input tokens read from the prompt cache; 500 written to it."
        );
        assert_eq!(CacheHit::new(Some(0.8253), 1, 1, 0).label(), "82.5%");
    }

    #[test]
    fn no_input_or_no_reported_cache_use_is_a_dash_not_zero() {
        let empty = CacheHit::new(Some(0.0), 0, 0, 0);
        assert_eq!(empty.rate, None, "no input: nothing to rate");
        assert_eq!(empty.label(), NO_HIT_RATE);
        assert_eq!(empty.detail(), "No input tokens yet.");

        let unreported = CacheHit::new(None, 1_200, 0, 0);
        assert_eq!(unreported.label(), NO_HIT_RATE);
        assert_eq!(
            unreported.detail(),
            "The provider reported no prompt-cache use."
        );

        // A provider that reported zero cache reads missed: that is 0 %.
        let missed = CacheHit::new(Some(0.0), 1_200, 0, 0);
        assert_eq!(missed.label(), "0.0%");
    }

    #[test]
    fn the_detail_says_when_unreported_requests_make_the_totals_disagree() {
        // One request of 1,000 tokens read 800 from the cache; another of
        // 1,000 said nothing about the cache. The daemon rates the first.
        let mixed = CacheHit::new(Some(0.8), 2_000, 800, 0);
        assert_eq!(mixed.label(), "80.0%");
        assert!(
            mixed
                .detail()
                .ends_with("Requests that did not report cache use are left out of the rate."),
            "{}",
            mixed.detail()
        );
    }

    #[test]
    fn an_out_of_range_rate_is_clamped_and_a_non_finite_one_dropped() {
        assert_eq!(CacheHit::new(Some(1.5), 10, 10, 0).label(), "100.0%");
        assert_eq!(CacheHit::new(Some(-0.1), 10, 0, 0).label(), "0.0%");
        assert_eq!(CacheHit::new(Some(f64::NAN), 10, 0, 0).rate, None);
    }

    #[test]
    fn rollups_and_model_rows_carry_their_own_figures() {
        let model = ModelUsageRollup {
            input_tokens: 4_000,
            cache_read_tokens: 1_000,
            cache_write_tokens: 3_000,
            cache_hit_rate: Some(0.25),
            ..ModelUsageRollup::default()
        };
        let usage = UsageRollup {
            input_tokens: 4_000,
            cache_read_tokens: 1_000,
            cache_write_tokens: 3_000,
            cache_hit_rate: Some(0.25),
            ..UsageRollup::default()
        };
        let expected = CacheHit::new(Some(0.25), 4_000, 1_000, 3_000);
        assert_eq!(CacheHit::of_model(&model), expected);
        assert_eq!(CacheHit::of(&usage), expected);
    }

    #[test]
    fn counts_group_their_digits_by_three() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(12_345_678), "12,345,678");
    }
}

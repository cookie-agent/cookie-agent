//! Numbers, counts, costs, and times as every client writes them: token
//! counts with thousands separators or in short form, costs in US dollars,
//! shares as percentages, elapsed times, and how long ago something
//! happened.
//!
//! Each formatter first works out what it shows (which unit, how many
//! decimals, which bracket of time) and then writes it; the tests check the
//! first step, the arithmetic, not the text.

use std::time::Duration;

use jiff::{Timestamp, civil::Date};

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
    match token_thousands(tokens) {
        None => tokens.to_string(),
        Some(thousands) => format!("{thousands:.1}K"),
    }
}

/// `tokens` in thousands, or `None` below a thousand, where they show
/// whole.
fn token_thousands(tokens: u64) -> Option<f64> {
    (tokens >= 1_000).then(|| tokens as f64 / 1_000.0)
}

/// `count` in at most four characters: as it is below a thousand, else in
/// thousands or millions with one decimal while that fits, as `1.2k`.
pub fn compact_count(count: u64) -> String {
    let short = Shortened::of(count);
    let suffix = match short.unit {
        Magnitude::Ones => "",
        Magnitude::Thousands => "k",
        Magnitude::Millions => "M",
    };
    match short.tenth {
        Some(tenth) => format!("{}.{tenth}{suffix}", short.whole),
        None => format!("{}{suffix}", short.whole),
    }
}

/// The unit a short count is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Magnitude {
    Ones,
    Thousands,
    Millions,
}

/// A count in its unit: whole units and, while it fits, the tenth below
/// them, rounded down; no tenth where it would be `.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shortened {
    whole: u64,
    tenth: Option<u64>,
    unit: Magnitude,
}

impl Shortened {
    fn of(count: u64) -> Self {
        let (unit, divisor, tenths) = match count {
            ..1_000 => (Magnitude::Ones, 1, false),
            1_000..10_000 => (Magnitude::Thousands, 1_000, true),
            10_000..1_000_000 => (Magnitude::Thousands, 1_000, false),
            1_000_000..10_000_000 => (Magnitude::Millions, 1_000_000, true),
            _ => (Magnitude::Millions, 1_000_000, false),
        };
        let tenth = (count % divisor) / (divisor / 10).max(1);
        Self {
            whole: count / divisor,
            tenth: (tenths && tenth != 0).then_some(tenth),
            unit,
        }
    }
}

/// A cost in US dollars: to the cent from a cent up, else to four decimals,
/// as `$0.0040`.
pub fn format_cost_usd(cost: f64) -> String {
    format!("${cost:.decimals$}", decimals = cost_decimals(cost))
}

/// How many decimals [`format_cost_usd`] gives `cost`: two from a cent up,
/// four below, where a cost to the cent would read as nothing.
fn cost_decimals(cost: f64) -> usize {
    if cost >= 0.01 { 2 } else { 4 }
}

/// A cost, or `unpriced` for a model without known prices.
pub fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "unpriced".to_owned(), format_cost_usd)
}

/// A cost in US dollars to the cent, as a phone's bottom bar shows it; a
/// cost that would round to no cents is `<$0.01` rather than nothing.
pub fn compact_cost(cost: f64) -> String {
    if under_half_a_cent(cost) {
        "<$0.01".to_owned()
    } else {
        format!("${cost:.2}")
    }
}

/// Whether `cost` is something that rounds to no cents.
fn under_half_a_cent(cost: f64) -> bool {
    cost > 0.0 && cost < 0.005
}

/// A share from 0 to 1 as a percentage with one decimal, as `82.5%`.
pub fn format_percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

/// An elapsed time as compact text: `42s` under a minute, `1m 23s` under
/// an hour, then `1h 4m`.
#[must_use]
pub fn format_elapsed(duration: Duration) -> String {
    match Elapsed::of(duration.as_secs()) {
        Elapsed::Seconds(seconds) => format!("{seconds}s"),
        Elapsed::Minutes(minutes, seconds) => format!("{minutes}m {seconds}s"),
        Elapsed::Hours(hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

/// An elapsed time as a narrow column shows it, the smaller unit padded to
/// two digits: `42s`, `1m05s`, then `1h04m`.
#[must_use]
pub fn format_clock_elapsed(seconds: u64) -> String {
    match Elapsed::of(seconds) {
        Elapsed::Seconds(seconds) => format!("{seconds}s"),
        Elapsed::Minutes(minutes, seconds) => format!("{minutes}m{seconds:02}s"),
        Elapsed::Hours(hours, minutes) => format!("{hours}h{minutes:02}m"),
    }
}

/// An elapsed time in its two largest units, the smaller one rounded down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Elapsed {
    Seconds(u64),
    Minutes(u64, u64),
    Hours(u64, u64),
}

impl Elapsed {
    fn of(seconds: u64) -> Self {
        match seconds {
            0..60 => Self::Seconds(seconds),
            60..3_600 => Self::Minutes(seconds / 60, seconds % 60),
            _ => Self::Hours(seconds / 3_600, seconds % 3_600 / 60),
        }
    }
}

/// How long before `now` `then` was: "just now", minutes, hours, or days
/// ago, else its date. A time after `now` (another clock) is "just now".
pub fn relative_time(now: Timestamp, then: Timestamp) -> String {
    match Ago::of(now, then) {
        Ago::JustNow => "just now".to_owned(),
        Ago::Minutes(minutes) => format!("{minutes}m ago"),
        Ago::Hours(hours) => format!("{hours}h ago"),
        Ago::Days(days) => format!("{days}d ago"),
        // Formatted by hand: jiff's strftime would add 20 KB to the web
        // page.
        Ago::On(date) => format!("{}-{:02}-{:02}", date.year(), date.month(), date.day()),
    }
}

/// How long ago a time was, in the largest whole unit under a week, else
/// its UTC date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ago {
    JustNow,
    Minutes(i64),
    Hours(i64),
    Days(i64),
    On(Date),
}

impl Ago {
    fn of(now: Timestamp, then: Timestamp) -> Self {
        let seconds = now.as_second().saturating_sub(then.as_second());
        match seconds {
            ..60 => Self::JustNow,
            60..3_600 => Self::Minutes(seconds / 60),
            3_600..86_400 => Self::Hours(seconds / 3_600),
            86_400..604_800 => Self::Days(seconds / 86_400),
            _ => Self::On(jiff::tz::TimeZone::UTC.to_datetime(then).date()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use jiff::{SignedDuration, Timestamp, civil::date};

    use super::*;

    #[test]
    fn counts_group_every_three_digits_from_the_right() {
        for count in [0, 7, 999, 1_000, 12_345, 999_999, 12_345_678, u64::MAX] {
            let text = format_count(count);
            assert_eq!(text.replace(',', ""), count.to_string(), "{text}");
            let groups = text.split(',').collect::<Vec<_>>();
            assert!((1..=3).contains(&groups[0].len()), "{text}");
            assert!(groups[1..].iter().all(|group| group.len() == 3), "{text}");
        }
        assert!(!format_count(999).contains(','));
        assert!(format_count(1_000).contains(','));
    }

    #[test]
    fn token_counts_turn_to_thousands_at_a_thousand() {
        assert_eq!(token_thousands(999), None);
        assert_eq!(token_thousands(1_000), Some(1.0));
        assert_eq!(token_thousands(12_500), Some(12.5));
        assert_eq!(format_token_count(999), 999.to_string());
    }

    #[test]
    fn short_counts_change_unit_and_drop_the_tenth_at_each_boundary() {
        let short = |whole, tenth, unit| Shortened { whole, tenth, unit };
        use Magnitude::*;
        assert_eq!(Shortened::of(999), short(999, None, Ones));
        assert_eq!(Shortened::of(1_000), short(1, None, Thousands));
        assert_eq!(Shortened::of(1_250), short(1, Some(2), Thousands));
        assert_eq!(Shortened::of(9_999), short(9, Some(9), Thousands));
        assert_eq!(Shortened::of(10_000), short(10, None, Thousands));
        assert_eq!(Shortened::of(10_999), short(10, None, Thousands));
        assert_eq!(Shortened::of(999_999), short(999, None, Thousands));
        assert_eq!(Shortened::of(1_000_000), short(1, None, Millions));
        assert_eq!(Shortened::of(1_500_000), short(1, Some(5), Millions));
        assert_eq!(Shortened::of(12_000_000), short(12, None, Millions));
        // Every short form fits in four characters.
        for count in [999, 1_250, 9_999, 999_999, 9_999_999, 99_999_999] {
            assert!(compact_count(count).chars().count() <= 4, "{count}");
        }
    }

    #[test]
    fn costs_keep_four_decimals_below_a_cent() {
        assert_eq!(cost_decimals(0.0099), 4);
        assert_eq!(cost_decimals(0.01), 2);
        assert_eq!(cost_decimals(12.5), 2);
        assert_eq!(cost_decimals(0.0), 4);
    }

    #[test]
    fn a_compact_cost_that_rounds_to_no_cents_is_not_zero() {
        assert!(under_half_a_cent(0.004));
        assert!(!under_half_a_cent(0.005), "rounds up to a cent");
        assert!(!under_half_a_cent(0.0), "nothing is nothing");
        assert_ne!(compact_cost(0.004), compact_cost(0.0));
    }

    #[test]
    fn elapsed_times_turn_over_at_the_minute_and_the_hour() {
        assert_eq!(Elapsed::of(0), Elapsed::Seconds(0));
        assert_eq!(Elapsed::of(59), Elapsed::Seconds(59));
        assert_eq!(Elapsed::of(60), Elapsed::Minutes(1, 0));
        assert_eq!(Elapsed::of(83), Elapsed::Minutes(1, 23));
        assert_eq!(Elapsed::of(3_599), Elapsed::Minutes(59, 59));
        assert_eq!(Elapsed::of(3_600), Elapsed::Hours(1, 0));
        assert_eq!(Elapsed::of(3_600 + 4 * 60 + 59), Elapsed::Hours(1, 4));
        assert_eq!(
            format_elapsed(Duration::from_millis(59_999)),
            format_elapsed(Duration::from_secs(59)),
            "fractions of a second round down"
        );
    }

    #[test]
    fn relative_times_step_from_minutes_to_a_date() {
        let now = Timestamp::from_second(1_800_000_000).expect("timestamp");
        let ago = |seconds: i64| {
            Ago::of(
                now,
                now.checked_sub(SignedDuration::from_secs(seconds))
                    .expect("timestamp"),
            )
        };
        assert_eq!(ago(59), Ago::JustNow);
        assert_eq!(ago(-30), Ago::JustNow, "a later clock is just now");
        assert_eq!(ago(60), Ago::Minutes(1));
        assert_eq!(ago(3_599), Ago::Minutes(59));
        assert_eq!(ago(3_600), Ago::Hours(1));
        assert_eq!(ago(86_399), Ago::Hours(23));
        assert_eq!(ago(86_400), Ago::Days(1));
        assert_eq!(ago(604_799), Ago::Days(6));
        assert_eq!(ago(604_800), Ago::On(date(2027, 1, 8)));
    }
}

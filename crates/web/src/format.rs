//! Short forms the page shows where room is tight: a directory's last
//! component, how long ago something happened, a token count, and a cost.

use jiff::Timestamp;

/// The last component of `path`, `/` or `\` separated, ignoring trailing
/// separators; the path itself when it has no other component (`/`).
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(last) if !last.is_empty() => last,
        _ => path,
    }
}

/// How long before `now` `then` was: "just now", minutes, hours, or days
/// ago, else its date. A time after `now` (another clock) is "just now".
pub fn relative_time(now: Timestamp, then: Timestamp) -> String {
    let seconds = now.as_second().saturating_sub(then.as_second());
    match seconds {
        ..60 => "just now".to_owned(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..604_800 => format!("{}d ago", seconds / 86_400),
        // Formatted by hand: jiff's strftime would add 20 KB to the page.
        _ => {
            let date = jiff::tz::TimeZone::UTC.to_datetime(then).date();
            format!("{}-{:02}-{:02}", date.year(), date.month(), date.day())
        }
    }
}

/// `count` in at most four characters: as it is below a thousand, else in
/// thousands or millions with one decimal while that fits.
pub fn compact_count(count: u64) -> String {
    match count {
        ..1_000 => count.to_string(),
        1_000..10_000 => tenths(count, 1_000, "k"),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        1_000_000..10_000_000 => tenths(count, 1_000_000, "M"),
        _ => format!("{}M", count / 1_000_000),
    }
}

/// `count / unit` with one decimal, rounded down, and no `.0`.
fn tenths(count: u64, unit: u64, suffix: &str) -> String {
    let tenths = count / (unit / 10);
    match tenths % 10 {
        0 => format!("{}{suffix}", tenths / 10),
        decimal => format!("{}.{decimal}{suffix}", tenths / 10),
    }
}

/// A cost in US dollars to the cent, as a phone's bottom bar shows it; a
/// cost under half a cent is "<$0.01" rather than nothing.
pub fn compact_cost(cost: f64) -> String {
    if cost > 0.0 && cost < 0.005 {
        "<$0.01".to_owned()
    } else {
        format!("${cost:.2}")
    }
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};

    use super::{basename, compact_cost, compact_count, relative_time};

    #[test]
    fn costs_show_to_the_cent_and_a_fraction_of_one_is_not_zero() {
        assert_eq!(compact_cost(0.0234), "$0.02");
        assert_eq!(compact_cost(12.5), "$12.50");
        assert_eq!(compact_cost(0.0), "$0.00");
        assert_eq!(compact_cost(0.004), "<$0.01");
        assert_eq!(compact_cost(0.005), "$0.01");
    }

    #[test]
    fn the_basename_is_the_last_component() {
        assert_eq!(basename("/tmp/tmp.x1/greeter"), "greeter");
        assert_eq!(basename("/home/ada/project/"), "project");
        assert_eq!(basename(r"C:\Users\ada\project"), "project");
        assert_eq!(basename("greeter"), "greeter");
        assert_eq!(basename("/"), "/");
        assert_eq!(basename(""), "");
    }

    #[test]
    fn relative_times_step_from_minutes_to_a_date() {
        let now = Timestamp::from_second(1_800_000_000).expect("timestamp");
        let ago = |seconds: i64| {
            relative_time(
                now,
                now.checked_sub(SignedDuration::from_secs(seconds))
                    .expect("timestamp"),
            )
        };
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(-30), "just now");
        assert_eq!(ago(60), "1m ago");
        assert_eq!(ago(3_599), "59m ago");
        assert_eq!(ago(3_600), "1h ago");
        assert_eq!(ago(86_399), "23h ago");
        assert_eq!(ago(86_400), "1d ago");
        assert_eq!(ago(604_799), "6d ago");
        assert_eq!(ago(604_800), "2027-01-08");
    }

    #[test]
    fn counts_shorten_at_each_boundary() {
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_000), "1k");
        assert_eq!(compact_count(1_250), "1.2k");
        assert_eq!(compact_count(9_999), "9.9k");
        assert_eq!(compact_count(10_000), "10k");
        assert_eq!(compact_count(999_999), "999k");
        assert_eq!(compact_count(1_000_000), "1M");
        assert_eq!(compact_count(1_500_000), "1.5M");
        assert_eq!(compact_count(12_000_000), "12M");
    }
}

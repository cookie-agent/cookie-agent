//! How memory reads out ages and sizes (`docs/site/specs/memory.md`
//! section 16.9.4 and 16.9.7): `3 weeks`, `5 days`, `1.4 KB`, `812 bytes`.
//! An age is whole units of the largest unit that fits, from minutes to
//! years; a size under 1 KB is in bytes, otherwise in KB of 1024 bytes
//! with at most one decimal.

use jiff::Timestamp;

/// How long ago `then` was at `now`, such as `3 weeks`; `less than a
/// minute` for anything shorter, a time after `now` included.
pub(crate) fn age(then: Timestamp, now: Timestamp) -> String {
    let seconds = now.as_second().saturating_sub(then.as_second());
    let units: [(i64, &str); 6] = [
        (365 * 86_400, "year"),
        (30 * 86_400, "month"),
        (7 * 86_400, "week"),
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
    ];
    units
        .iter()
        .find(|(length, _)| seconds >= *length)
        .map_or_else(
            || "less than a minute".to_owned(),
            |(length, unit)| {
                let count = seconds / length;
                format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
            },
        )
}

/// `bytes` for a reader, such as `812 bytes` or `1.4 KB`.
pub(crate) fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} {}", if bytes == 1 { "byte" } else { "bytes" });
    }
    let tenths = (bytes * 10 + 512) / 1024;
    if tenths.is_multiple_of(10) {
        format!("{} KB", tenths / 10)
    } else {
        format!("{}.{} KB", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).unwrap()
    }

    #[test]
    fn ages_use_the_largest_whole_unit() {
        let now = at(1_000_000_000);
        for (ago, expected) in [
            (-5, "less than a minute"),
            (59, "less than a minute"),
            (60, "1 minute"),
            (3_599, "59 minutes"),
            (3_600, "1 hour"),
            (2 * 86_400, "2 days"),
            (6 * 86_400 + 86_399, "6 days"),
            (7 * 86_400, "1 week"),
            (21 * 86_400, "3 weeks"),
            (30 * 86_400, "1 month"),
            (364 * 86_400, "12 months"),
            (365 * 86_400, "1 year"),
            (800 * 86_400, "2 years"),
        ] {
            assert_eq!(age(at(1_000_000_000 - ago), now), expected, "{ago}");
        }
    }

    #[test]
    fn sizes_switch_to_kilobytes_at_1024_bytes() {
        for (bytes, expected) in [
            (0, "0 bytes"),
            (1, "1 byte"),
            (812, "812 bytes"),
            (1023, "1023 bytes"),
            (1024, "1 KB"),
            (1434, "1.4 KB"),
            (2048, "2 KB"),
            (2611, "2.5 KB"),
        ] {
            assert_eq!(size(bytes), expected, "{bytes}");
        }
    }
}

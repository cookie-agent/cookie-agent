//! Cron schedules (`docs/site/specs/cron.md` section 3): five-field cron
//! expressions evaluated in a time zone, and one-shot delays.
//!
//! A field is `*`, a number, a range `a-b`, any of these with a step
//! (`*/n`, `a-b/n`, `a/n`, which runs from `a` to the field's end), or a
//! comma-separated list of them. Months and days of the week may also be
//! named by their first three letters (`jan`, `mon`), in any case, and a day
//! of the week of 7 is Sunday. Day of month and day of week combine as in
//! Vixie cron: when both fields are restricted (neither starts with `*`), a
//! day matches when either matches; otherwise both must.
//!
//! Local times that do not exist (a spring-forward gap) fire at the
//! instant the gap jumps to, as `jiff`'s compatible disambiguation maps
//! them; local times that occur twice (a fall-back fold) fire once, at the
//! first occurrence.

use jiff::{
    Span, Timestamp, ToSpan,
    civil::{Date, DateTime, Time},
    tz::TimeZone,
};

/// The longest a one-shot delay may be, in days.
pub(crate) const MAX_DELAY_DAYS: i64 = 365;
/// How many days ahead a next match is searched for: past the longest gap
/// between two February 29ths (eight years, around 2100).
const SEARCH_DAYS: i64 = 366 * 9;

/// A parsed five-field cron expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronExpr {
    minutes: u64,
    hours: u32,
    days_of_month: u32,
    months: u16,
    days_of_week: u8,
    /// Whether day of month and day of week combine with OR (both
    /// restricted).
    either_day: bool,
}

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
    /// The first value `names` stands for.
    names_from: u32,
}

const FIELDS: [Field; 5] = [
    Field {
        name: "minute",
        min: 0,
        max: 59,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "hour",
        min: 0,
        max: 23,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "day of month",
        min: 1,
        max: 31,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "month",
        min: 1,
        max: 12,
        names: &MONTH_NAMES,
        names_from: 1,
    },
    Field {
        name: "day of week",
        min: 0,
        max: 7,
        names: &DAY_NAMES,
        names_from: 0,
    },
];

impl Field {
    fn value(&self, text: &str) -> Result<u32, String> {
        if let Some(index) = self
            .names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(text))
        {
            return Ok(self.names_from + u32::try_from(index).expect("few names"));
        }
        let value = text
            .parse::<u32>()
            .ok()
            .filter(|_| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| format!("`{text}` is not a valid {}", self.name))?;
        if value < self.min || value > self.max {
            return Err(format!(
                "{} {value} is outside {}-{}",
                self.name, self.min, self.max
            ));
        }
        Ok(value)
    }

    /// The bit set of the values `text` names.
    fn parse(&self, text: &str) -> Result<u64, String> {
        let mut bits = 0_u64;
        for item in text.split(',') {
            let (range, step) = match item.split_once('/') {
                Some((range, step)) => {
                    let step = Some(step)
                        .filter(|step| {
                            !step.is_empty() && step.bytes().all(|byte| byte.is_ascii_digit())
                        })
                        .and_then(|step| step.parse::<u32>().ok())
                        .filter(|step| *step >= 1)
                        .ok_or_else(|| {
                            format!("`{item}`: the step must be a whole number from 1")
                        })?;
                    (range, Some(step))
                }
                None => (item, None),
            };
            let (from, to) = if range == "*" {
                (self.min, self.max)
            } else if let Some((from, to)) = range.split_once('-') {
                let (from, to) = (self.value(from)?, self.value(to)?);
                if from > to {
                    return Err(format!("`{item}`: the range runs backwards"));
                }
                (from, to)
            } else {
                let from = self.value(range)?;
                // `a/n` runs from `a` to the field's end.
                (from, if step.is_some() { self.max } else { from })
            };
            let step = step.unwrap_or(1);
            let mut value = from;
            while value <= to {
                bits |= 1 << value;
                value += step;
            }
        }
        Ok(bits)
    }
}

impl CronExpr {
    /// Parses `text`, refusing an expression that never matches.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let parts = text.split_whitespace().collect::<Vec<_>>();
        let [minute, hour, day_of_month, month, day_of_week] = parts[..] else {
            return Err(format!(
                "a cron expression has five fields (minute, hour, day of month, month, day of week); `{text}` has {}",
                parts.len()
            ));
        };
        let mut days_of_week = FIELDS[4].parse(day_of_week)?;
        // 7 is Sunday too.
        if days_of_week & (1 << 7) != 0 {
            days_of_week = (days_of_week | 1) & !(1 << 7);
        }
        let expr = Self {
            minutes: FIELDS[0].parse(minute)?,
            hours: u32::try_from(FIELDS[1].parse(hour)?).expect("24 hour bits"),
            days_of_month: u32::try_from(FIELDS[2].parse(day_of_month)? >> 1).expect("31 day bits"),
            months: u16::try_from(FIELDS[3].parse(month)? >> 1).expect("12 month bits"),
            days_of_week: u8::try_from(days_of_week).expect("7 weekday bits"),
            either_day: !day_of_month.starts_with('*') && !day_of_week.starts_with('*'),
        };
        if !expr.can_match() {
            return Err(format!("`{text}` has no next match"));
        }
        Ok(expr)
    }

    /// Whether some day of some month matches: always when the days of the
    /// week count, else when a day of month exists in a month.
    fn can_match(&self) -> bool {
        const LONGEST: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        if self.either_day || self.days_of_month == (1 << 31) - 1 {
            return true;
        }
        (0..12).any(|month| {
            self.months & (1 << month) != 0
                && self.days_of_month & ((1_u32 << LONGEST[month]) - 1) != 0
        })
    }

    fn day_matches(&self, date: Date) -> bool {
        let month = u8::try_from(date.month()).expect("month") - 1;
        if self.months & (1 << month) == 0 {
            return false;
        }
        let day_of_month = self.days_of_month & (1 << (date.day() - 1)) != 0;
        let day_of_week = self.days_of_week & (1 << date.weekday().to_sunday_zero_offset()) != 0;
        if self.either_day {
            day_of_month || day_of_week
        } else {
            day_of_month && day_of_week
        }
    }

    /// The first instant after `after` that matches, in `zone`.
    pub(crate) fn next_after(&self, after: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        let local = zone.to_datetime(after);
        // The next whole minute after `after`, as a local time.
        let start = local
            .with()
            .second(0)
            .subsec_nanosecond(0)
            .build()
            .ok()?
            .checked_add(1.minute())
            .ok()?;
        let mut date = start.date();
        for day in 0..SEARCH_DAYS {
            if self.day_matches(date) {
                for hour in 0..24_i8 {
                    if self.hours & (1 << hour) == 0 {
                        continue;
                    }
                    for minute in 0..60_i8 {
                        if self.minutes & (1 << minute) == 0 {
                            continue;
                        }
                        let candidate =
                            DateTime::from_parts(date, Time::constant(hour, minute, 0, 0));
                        if day == 0 && candidate < start {
                            continue;
                        }
                        let Ok(zoned) = zone.to_ambiguous_zoned(candidate).compatible() else {
                            continue;
                        };
                        let instant = zoned.timestamp();
                        if instant > after {
                            return Some(instant);
                        }
                    }
                }
            }
            date = date.tomorrow().ok()?;
        }
        None
    }

    /// Every match after `after` up to and including `until`, counted, up
    /// to `limit`.
    pub(crate) fn count_between(
        &self,
        after: Timestamp,
        until: Timestamp,
        zone: &TimeZone,
        limit: u64,
    ) -> u64 {
        let mut count = 0;
        let mut cursor = after;
        while count < limit {
            match self.next_after(cursor, zone) {
                Some(next) if next <= until => {
                    count += 1;
                    cursor = next;
                }
                _ => break,
            }
        }
        count
    }
}

/// Parses a one-shot delay: a whole number with `s`, `m`, `h`, or `d`, from
/// `1s` to `365d`.
pub(crate) fn parse_delay(text: &str) -> Result<Span, String> {
    let refused = || {
        format!(
            "`{text}` is not a delay: write a whole number with s, m, h, or d, from 1s to {MAX_DELAY_DAYS}d"
        )
    };
    let unit = text.chars().last().ok_or_else(refused)?;
    let number = &text[..text.len() - unit.len_utf8()];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refused());
    }
    let value = number.parse::<i64>().map_err(|_| refused())?;
    let seconds = match unit {
        's' => Some(value),
        'm' => value.checked_mul(60),
        'h' => value.checked_mul(3600),
        'd' => value.checked_mul(86_400),
        _ => None,
    }
    .ok_or_else(refused)?;
    if !(1..=MAX_DELAY_DAYS * 86_400).contains(&seconds) {
        return Err(refused());
    }
    Ok(seconds.seconds())
}

/// The instant `delay` after `now`.
pub(crate) fn delay_after(now: Timestamp, delay: Span) -> Result<Timestamp, String> {
    now.checked_add(delay).map_err(|error| error.to_string())
}

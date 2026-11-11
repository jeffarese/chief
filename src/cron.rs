//! A routine's schedule (ARCHITECTURE §13): five-field cron in local time, `@hourly`-style
//! shorthands, or `every 30m`. Pure: the clock is passed in.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, TimeZone};

/// The shortest time between two runs that start a Claude session. Each is a whole session, so
/// a schedule that fires more often is a typo (`* * * * *`), not a wish.
pub const MIN_GAP_MINUTES: u32 = 5;

/// The shortest time between two runs of a command (§13 "A command"): cron's own grain.
pub const MIN_GAP_COMMAND_MINUTES: u32 = 1;

/// How far `next_after` looks for a day that matches: `0 0 29 2 *` comes once in four years, and
/// `0 0 29 2 1` (a Monday) rarely enough that eight years finds one.
const SEARCH_DAYS: i64 = 366 * 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Cron(Cron),
    /// Every so many minutes, counted from the last run.
    Every(u32),
}

/// A parsed cron line: one bit per allowed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Whether day-of-month and day-of-week were restricted: cron matches a day that fits
    /// either when both are.
    days_given: bool,
    weekdays_given: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const WEEKDAY_LABELS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

impl Schedule {
    /// Parses what a person or the coordinator writes. Errors say what to write instead.
    pub fn parse(text: &str) -> Result<Schedule> {
        let text = text.trim();
        let lower = text.to_ascii_lowercase();
        if lower.is_empty() {
            bail!("the schedule is empty; {EXAMPLES}");
        }
        if let Some(rest) = lower.strip_prefix("every") {
            return every(rest.trim()).with_context(|| format!("schedule {text:?}"));
        }
        let line = match lower.as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@weekdays" => "0 9 * * 1-5",
            other if other.starts_with('@') => {
                bail!("schedule {text:?} isn't one chief knows; {EXAMPLES}")
            }
            other => other,
        };
        let cron = Cron::parse(line).with_context(|| format!("schedule {text:?}"))?;
        Ok(Schedule::Cron(cron))
    }

    /// Refuses a schedule that would fire less than `minutes` apart: what a run costs decides
    /// how often it may run, so the routine says which gap applies.
    pub fn check_gap(&self, minutes: u32, why: &str) -> Result<()> {
        let gap = match self {
            Schedule::Every(m) => *m,
            Schedule::Cron(cron) => cron.min_gap(),
        };
        if gap < minutes {
            let unit = if minutes == 1 { "minute" } else { "minutes" };
            bail!(
                "the schedule would run less than {minutes} {unit} apart; {why}, so space \
                 them out (e.g. `*/15 * * * *`, `every 30m`)"
            );
        }
        Ok(())
    }

    /// The first run strictly after `after`. None for a cron line no day ever matches
    /// (`0 0 31 2 *`).
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        match self {
            Schedule::Every(minutes) => Some(after + Duration::minutes(i64::from(*minutes))),
            Schedule::Cron(cron) => cron.next_after(after),
        }
    }

    /// `next_after` in unix milliseconds, the unit the database keeps.
    pub fn next_after_ms(&self, after_ms: i64) -> Option<i64> {
        let after = Local.timestamp_millis_opt(after_ms).single()?;
        self.next_after(after).map(|t| t.timestamp_millis())
    }

    /// For people: "Weekdays at 09:00", "Every 30 minutes"; a line chief has no words for is
    /// shown as written (`cron 0 9 1-7 * 1`).
    pub fn describe(&self) -> String {
        match self {
            Schedule::Every(m) if m.is_multiple_of(24 * 60) => plural(*m / (24 * 60), "day"),
            Schedule::Every(m) if m.is_multiple_of(60) => plural(*m / 60, "hour"),
            Schedule::Every(m) => plural(*m, "minute"),
            Schedule::Cron(cron) => cron.describe(),
        }
    }
}

const EXAMPLES: &str = "write five cron fields in local time (minute hour day-of-month month \
                        day-of-week, e.g. `0 9 * * 1-5` for weekdays at 09:00), `@hourly`, \
                        `@daily`, `@weekly`, `@monthly`, or `every 30m` / `every 2h` / `every 1d`";

fn plural(n: u32, unit: &str) -> String {
    if n == 1 {
        format!("Every {unit}")
    } else {
        format!("Every {n} {unit}s")
    }
}

/// `30m`, `2h`, `1d`, `90 minutes`.
fn every(rest: &str) -> Result<Schedule> {
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let unit = rest[digits.len()..].trim();
    let n: u32 = digits
        .parse()
        .with_context(|| format!("`every` needs a number and a unit; {EXAMPLES}"))?;
    let minutes = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => Some(n),
        "h" | "hr" | "hrs" | "hour" | "hours" => n.checked_mul(60),
        "d" | "day" | "days" => n.checked_mul(24 * 60),
        _ => bail!("`every` takes minutes, hours or days (30m, 2h, 1d), not {unit:?}"),
    };
    let minutes = minutes.context("that interval is too long")?;
    if minutes == 0 {
        bail!("`every` needs at least a minute");
    }
    Ok(Schedule::Every(minutes))
}

impl Cron {
    fn parse(line: &str) -> Result<Cron> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            bail!("has {} fields, not five; {EXAMPLES}", fields.len());
        };
        let minutes = field(minute, 0, 59, &[]).context("minute")?;
        let hours = field(hour, 0, 23, &[]).context("hour")?;
        let days = field(day, 1, 31, &[]).context("day of month")?;
        let months = field(month, 1, 12, &MONTHS).context("month")?;
        // 7 is Sunday too.
        let weekdays = field(weekday, 0, 7, &WEEKDAYS).context("day of week")?;
        let weekdays = (weekdays | (weekdays >> 7)) & 0x7f;
        let cron = Cron {
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            days_given: !day.starts_with('*'),
            weekdays_given: !weekday.starts_with('*'),
        };
        Ok(cron)
    }

    /// The fewest minutes between two runs in one day, or across an hour's edge.
    fn min_gap(&self) -> u32 {
        let minutes: Vec<u32> = (0..60).filter(|m| self.minutes & (1 << m) != 0).collect();
        let within = minutes.windows(2).map(|w| w[1] - w[0]).min().unwrap_or(60);
        let consecutive_hours =
            (0..24).any(|h| self.hours & (1 << h) != 0 && self.hours & (1 << ((h + 1) % 24)) != 0);
        let across = match (minutes.first(), minutes.last()) {
            (Some(first), Some(last)) if consecutive_hours => 60 - last + first,
            _ => 60,
        };
        within.min(across)
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        if self.months & (1 << date.month()) == 0 {
            return false;
        }
        let by_day = self.days & (1 << date.day()) != 0;
        let by_weekday = self.weekdays & (1 << date.weekday().num_days_from_sunday()) != 0;
        match (self.days_given, self.weekdays_given) {
            (true, true) => by_day || by_weekday,
            _ => by_day && by_weekday,
        }
    }

    fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let mut date = after.date_naive();
        for _ in 0..SEARCH_DAYS {
            if self.day_matches(date) {
                for hour in (0..24).filter(|h| self.hours & (1 << h) != 0) {
                    for minute in (0..60).filter(|m| self.minutes & (1 << m) != 0) {
                        // A time the clocks skip (spring forward) doesn't exist that day; one
                        // they repeat runs at its first pass.
                        let at = match Local.with_ymd_and_hms(
                            date.year(),
                            date.month(),
                            date.day(),
                            hour,
                            minute,
                            0,
                        ) {
                            LocalResult::Single(t) => t,
                            LocalResult::Ambiguous(first, _) => first,
                            LocalResult::None => continue,
                        };
                        if at > after {
                            return Some(at);
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }

    fn describe(&self) -> String {
        let minutes: Vec<u32> = (0..60).filter(|m| self.minutes & (1 << m) != 0).collect();
        let hours: Vec<u32> = (0..24).filter(|h| self.hours & (1 << h) != 0).collect();
        let every_day = !self.days_given && !self.weekdays_given;
        let every_month = self.months == 0x1ffe;
        let plain_days = every_month && !self.days_given;

        // Step schedules over every hour of every day: `*/15 * * * *`, `0 * * * *`.
        if every_day && every_month && hours.len() == 24 {
            if let [minute] = minutes.as_slice() {
                return if *minute == 0 {
                    "Every hour".to_string()
                } else {
                    format!("Every hour at :{minute:02}")
                };
            }
            if let Some(step) = step_of(&minutes, 60) {
                return plural(step, "minute");
            }
        }
        let [minute] = minutes.as_slice() else {
            return self.raw();
        };
        let at = match hours.as_slice() {
            [hour] => format!("at {hour:02}:{minute:02}"),
            _ if *minute == 0 && step_of(&hours, 24).is_some() => {
                let step = step_of(&hours, 24).unwrap_or(1);
                return match plain_days {
                    true if every_day => plural(step, "hour"),
                    true => format!("{} on {}", plural(step, "hour"), self.weekday_words()),
                    false => self.raw(),
                };
            }
            _ if hours.len() <= 4 => format!(
                "at {}",
                hours
                    .iter()
                    .map(|h| format!("{h:02}:{minute:02}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => return self.raw(),
        };
        if every_day && every_month {
            return format!("Daily {at}");
        }
        if plain_days {
            return format!("{} {at}", self.weekday_words());
        }
        if every_month && !self.weekdays_given {
            let days: Vec<String> = (1..=31)
                .filter(|d| self.days & (1 << d) != 0)
                .map(|d| d.to_string())
                .collect();
            if days.len() <= 4 {
                return format!("Monthly on day {} {at}", days.join(", "));
            }
        }
        self.raw()
    }

    /// "Weekdays", "Weekends", "Mon, Wed".
    fn weekday_words(&self) -> String {
        match self.weekdays {
            0b0111110 => "Weekdays".to_string(),
            0b1000001 => "Weekends".to_string(),
            days => (0..7)
                // Monday first, Sunday last.
                .map(|i| (i + 1) % 7)
                .filter(|d| days & (1 << d) != 0)
                .map(|d| WEEKDAY_LABELS[d as usize])
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// The line again, from the bits: `cron 0 9 1-7 * 1`.
    fn raw(&self) -> String {
        let weekdays = if self.weekdays_given {
            ranges(u64::from(self.weekdays), 0, 6)
        } else {
            "*".to_string()
        };
        let days = if self.days_given {
            ranges(u64::from(self.days), 1, 31)
        } else {
            "*".to_string()
        };
        format!(
            "cron {} {} {} {} {}",
            ranges(self.minutes, 0, 59),
            ranges(u64::from(self.hours), 0, 23),
            days,
            ranges(u64::from(self.months), 1, 12),
            weekdays,
        )
    }
}

/// The step of an evenly spaced set that starts at 0 and covers `span` (`0,15,30,45` → 15).
fn step_of(values: &[u32], span: u32) -> Option<u32> {
    let step = *values.get(1)?;
    let even = values.first() == Some(&0)
        && span.is_multiple_of(step)
        && values.len() as u32 == span / step
        && values
            .iter()
            .enumerate()
            .all(|(i, v)| *v == i as u32 * step);
    even.then_some(step)
}

/// A bit set as cron writes it: `*`, `1-5`, `0,30`.
fn ranges(bits: u64, min: u32, max: u32) -> String {
    let set: Vec<u32> = (min..=max).filter(|v| bits & (1 << v) != 0).collect();
    if set.len() as u32 == max - min + 1 {
        return "*".to_string();
    }
    let mut parts = Vec::new();
    let mut i = 0;
    while i < set.len() {
        let mut j = i;
        while j + 1 < set.len() && set[j + 1] == set[j] + 1 {
            j += 1;
        }
        parts.push(match j - i {
            0 => set[i].to_string(),
            1 => format!("{},{}", set[i], set[j]),
            _ => format!("{}-{}", set[i], set[j]),
        });
        i = j + 1;
    }
    parts.join(",")
}

/// One cron field as a bit set: `*`, `*/15`, `1-5`, `1-10/2`, `mon,wed`, joined by commas.
fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<u64> {
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .with_context(|| format!("step {step:?} isn't a number"))?;
                if step == 0 {
                    bail!("a step of 0 never advances");
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a, min, max, names)?, value(b, min, max, names)?)
        } else {
            let v = value(range, min, max, names)?;
            // `5/15` means from 5 on, as in cron.
            (v, if part.contains('/') { max } else { v })
        };
        if from > to {
            bail!("{range:?} runs backwards");
        }
        for v in (from..=to).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

fn value(text: &str, min: u32, max: u32, names: &[&str]) -> Result<u32> {
    if let Some(i) = names.iter().position(|n| *n == text) {
        // Months are named from 1, weekdays from 0.
        return Ok(i as u32 + if names.len() == 12 { 1 } else { 0 });
    }
    let v: u32 = text
        .parse()
        .with_context(|| format!("{text:?} isn't a number"))?;
    if !(min..=max).contains(&v) {
        bail!("{v} is outside {min}-{max}");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn next(schedule: &str, after: DateTime<Local>) -> DateTime<Local> {
        Schedule::parse(schedule)
            .unwrap()
            .next_after(after)
            .unwrap()
    }

    #[test]
    fn cron_lines_find_their_next_run() {
        // 2026-11-09 is a Monday.
        let monday = at(2026, 11, 9, 10, 0);
        for (schedule, want) in [
            ("0 9 * * 1-5", at(2026, 11, 10, 9, 0)),
            ("30 10 * * *", at(2026, 11, 9, 10, 30)),
            ("0 10 * * *", at(2026, 11, 10, 10, 0)),
            ("*/15 * * * *", at(2026, 11, 9, 10, 15)),
            ("0 0 1 * *", at(2026, 12, 1, 0, 0)),
            ("0 8 * * sun", at(2026, 11, 15, 8, 0)),
            ("0 8 * * 7", at(2026, 11, 15, 8, 0)),
            ("0 8 * jan mon", at(2027, 1, 4, 8, 0)),
            // Day of month or day of week, as cron does when both are given.
            ("0 8 13 * fri", at(2026, 11, 13, 8, 0)),
            ("0 8 12 * fri", at(2026, 11, 12, 8, 0)),
            ("0 0 29 2 *", at(2028, 2, 29, 0, 0)),
            ("@hourly", at(2026, 11, 9, 11, 0)),
            ("@daily", at(2026, 11, 10, 0, 0)),
            ("@weekly", at(2026, 11, 15, 0, 0)),
            ("@WEEKDAYS", at(2026, 11, 10, 9, 0)),
            ("5/20 12 * * *", at(2026, 11, 9, 12, 5)),
        ] {
            assert_eq!(next(schedule, monday), want, "{schedule}");
        }
    }

    #[test]
    fn the_next_run_is_strictly_later() {
        let nine = at(2026, 11, 9, 9, 0);
        assert_eq!(next("0 9 * * *", nine), at(2026, 11, 10, 9, 0));
    }

    #[test]
    fn intervals_count_from_the_last_run() {
        let from = at(2026, 11, 9, 10, 7);
        assert_eq!(next("every 30m", from), at(2026, 11, 9, 10, 37));
        assert_eq!(next("every 2h", from), at(2026, 11, 9, 12, 7));
        assert_eq!(next("Every 1 day", from), at(2026, 11, 10, 10, 7));
        assert_eq!(next("every 90 minutes", from), at(2026, 11, 9, 11, 37));
    }

    #[test]
    fn a_day_that_never_comes_has_no_next_run() {
        let schedule = Schedule::parse("0 0 31 2 *").unwrap();
        assert_eq!(schedule.next_after(at(2026, 11, 9, 0, 0)), None);
    }

    #[test]
    fn bad_schedules_say_why() {
        for (schedule, why) in [
            ("", "empty"),
            ("0 9 * *", "four fields"),
            ("0 9 * * * *", "six fields"),
            ("60 9 * * *", "minute out of range"),
            ("0 24 * * *", "hour out of range"),
            ("0 9 0 * *", "day 0"),
            ("0 9 * 13 *", "month 13"),
            ("0 9 * * 8", "weekday 8"),
            ("0 9 * * funday", "a name it doesn't know"),
            ("*/0 9 * * *", "a zero step"),
            ("5-1 9 * * *", "a backwards range"),
            ("every 0m", "no time at all"),
            ("every 10 fortnights", "an unknown unit"),
            ("every", "no number"),
            ("@yearlyish", "an unknown shorthand"),
        ] {
            assert!(Schedule::parse(schedule).is_err(), "{why}: {schedule:?}");
        }
    }

    #[test]
    fn a_schedule_keeps_the_gap_its_runs_need() {
        let spaced = |schedule: &str, minutes: u32| {
            Schedule::parse(schedule)
                .unwrap()
                .check_gap(minutes, "each run is a session")
        };
        for (schedule, why) in [
            ("* * * * *", "every minute"),
            ("*/2 * * * *", "every two minutes"),
            ("58,1 * * * *", "three minutes across the hour"),
            ("every 2m", "under the minimum"),
            // Close only across an hour's edge, with hours back to back.
            ("58,1 9,10 * * *", "three minutes across 09:58 and 10:01"),
        ] {
            let err = spaced(schedule, MIN_GAP_MINUTES).unwrap_err().to_string();
            assert!(err.contains("less than 5 minutes apart"), "{why}: {err}");
            assert!(err.contains("each run is a session"), "{err}");
        }
        for schedule in ["58,1 9 * * *", "55,0 9,12 * * *", "*/5 * * * *", "every 5m"] {
            assert!(spaced(schedule, MIN_GAP_MINUTES).is_ok(), "{schedule}");
        }
        // A command may run as often as cron's grain.
        for schedule in ["* * * * *", "every 1m", "*/2 * * * *"] {
            assert!(
                spaced(schedule, MIN_GAP_COMMAND_MINUTES).is_ok(),
                "{schedule}"
            );
        }
        assert_eq!(
            Schedule::parse("* * * * *").unwrap().describe(),
            "Every minute"
        );
    }

    #[test]
    fn schedules_read_as_words() {
        for (schedule, want) in [
            ("0 9 * * 1-5", "Weekdays at 09:00"),
            ("@weekdays", "Weekdays at 09:00"),
            ("30 18 * * *", "Daily at 18:30"),
            ("0 9,17 * * *", "Daily at 09:00, 17:00"),
            ("0 10 * * 6,0", "Weekends at 10:00"),
            ("0 8 * * mon,wed", "Mon, Wed at 08:00"),
            ("0 8 * * sun", "Sun at 08:00"),
            ("@hourly", "Every hour"),
            ("15 * * * *", "Every hour at :15"),
            ("*/15 * * * *", "Every 15 minutes"),
            ("0 */2 * * *", "Every 2 hours"),
            ("0 */6 * * 1-5", "Every 6 hours on Weekdays"),
            ("@monthly", "Monthly on day 1 at 00:00"),
            ("0 7 1,15 * *", "Monthly on day 1, 15 at 07:00"),
            ("every 30m", "Every 30 minutes"),
            ("every 60m", "Every hour"),
            ("every 3h", "Every 3 hours"),
            ("every 2d", "Every 2 days"),
            ("0 9 1-7 * 1", "cron 0 9 1-7 * 1"),
            ("0 9 * 1,6 *", "cron 0 9 * 1,6 *"),
            ("10,40 9 * * *", "cron 10,40 9 * * *"),
        ] {
            assert_eq!(
                Schedule::parse(schedule).unwrap().describe(),
                want,
                "{schedule}"
            );
        }
    }

    #[test]
    fn milliseconds_round_trip() {
        let schedule = Schedule::parse("every 5m").unwrap();
        assert_eq!(schedule.next_after_ms(1_000_000), Some(1_000_000 + 300_000));
    }
}

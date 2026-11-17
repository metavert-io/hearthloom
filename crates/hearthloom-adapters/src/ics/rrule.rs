//! The recurrence rules calendars write (RFC 5545 §3.3.10), expanded in
//! the wall-clock time of the event's DTSTART: a weekly 14:00 meeting
//! stays at 14:00 local across a daylight-saving change, and the caller
//! turns each wall-clock start into an instant with the event's zone.
//!
//! Covered: FREQ DAILY/WEEKLY/MONTHLY/YEARLY with INTERVAL, COUNT, UNTIL,
//! BYMONTH, BYMONTHDAY, BYDAY (with ordinals), BYHOUR, BYMINUTE,
//! BYSECOND, BYSETPOS and WKST — what calendar applications emit. A rule
//! using anything else (sub-daily frequencies, which no calendar
//! application offers, BYYEARDAY, BYWEEKNO) is refused with an error, and
//! the caller keeps the occurrences it can name without it.
//!
//! A start the rule makes at a wall-clock time the event's zone skips (a
//! daylight-saving change) is no occurrence: it doesn't count against
//! COUNT and takes no BYSETPOS position (RFC 5545 §3.3.10).
//!
//! What one rule costs is bounded whatever it says: a value listed twice
//! counts once, a rule naming more than `MAX_TIMES_PER_DAY` times of day
//! is refused, and an expansion looks at no more than `MAX_STARTS`
//! candidate starts.
//!
//! ## Why this expander is hand-written (decided 2026-11-15)
//!
//! Before keeping it, two crates were tested as replacements on the same
//! cases as `ics/matrix_tests.rs` (39 cases from RFC 5545 and from bugs
//! found in review), plus mutation fuzzing, stress inputs, cargo-deny and
//! the effect on stored refs:
//!
//! - `rrule` 0.14.0 (fmeringdal/rust-rrule; MIT/Apache; expands RRULE,
//!   RDATE and EXDATE only, so it would replace this file alone). It
//!   passed 36 of 39: it shifts an occurrence that falls in a
//!   daylight-saving gap and still counts it, doesn't count a DTSTART that
//!   is off the rule (upstream issue #119), reads a mixed BYDAY such as
//!   `1MO,FR` as an intersection (#153), refuses a non-UTC UNTIL next to a
//!   zoned DTSTART, and panics on a non-ASCII BYDAY value. Its last release
//!   was 2025-04, with one owner.
//! - `calcard` 0.3.14 (stalwartlabs; Apache/MIT; a full iCalendar parser
//!   with expansion, so it would replace most of `ics.rs` too). It is built
//!   for CalDAV resources rather than exported files: it drops an event
//!   whose DTSTART falls in a daylight-saving gap or repeated hour, returns
//!   floating times for such occurrences, ignores VTIMEZONE rules, matches
//!   overrides by SEQUENCE and instant rather than UID, never reads STATUS,
//!   and doesn't apply EXDATE to RDATEs (28 of 39, even with 233 lines of
//!   glue). It would also have changed the refs of some stored events,
//!   which imports them twice. Its repository was archived on 2026-10-26;
//!   development continued unpublished inside Stalwart under AGPL-3.0-only
//!   or a proprietary license, which this MIT project can't take code from.
//!
//! This code passed 38 of 39 (the 39th is a sub-daily rule, refused on
//! purpose) and had no panics in about 8.7 million fuzzed inputs. The cases
//! and a short fuzz run (`ics/fuzz_tests.rs`) are in the test suite so it
//! stays that way.
//!
//! Revisit if `rrule` fixes #119, #153 and the non-ASCII panic and releases
//! again; if a maintained, permissively licensed crate passes
//! `ics/matrix_tests.rs` (DST gaps and repeated hours, zones defined only in
//! the file, overrides matched by UID, STATUS, EXDATE on RDATE); or if
//! calendars start sending sub-daily rules. Whatever replaces this must keep
//! the refs `ics.rs` makes (`UID`, `UID#<start as written>`; pinned in the
//! matrix tests), or stored events will be imported twice.

use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};

#[cfg(test)]
thread_local! {
    /// How many expansions this thread has walked: tests check what an
    /// import costs with it.
    pub(super) static WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Upper bound on the periods one expansion walks, whatever the rule: a
/// rule that never matches (`BYMONTH=2;BYMONTHDAY=30`) ends here.
const MAX_PERIODS: i64 = 200_000;

/// The most times of day (BYHOUR × BYMINUTE × BYSECOND) a rule may name:
/// one a minute.
const MAX_TIMES_PER_DAY: usize = 1440;

/// Upper bound on the candidate starts (each day a period picks, at each
/// time of day) one expansion looks at; it ends there.
const MAX_STARTS: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Until {
    /// A DATE: the last day an occurrence may start on.
    Date(NaiveDate),
    /// A DATE-TIME without a zone: compared with the wall-clock start.
    Local(NaiveDateTime),
    /// A UTC DATE-TIME: compared with the occurrence's instant.
    Utc(NaiveDateTime),
}

#[derive(Debug, Clone)]
pub(super) struct Rule {
    freq: Freq,
    interval: i64,
    count: Option<u32>,
    until: Option<Until>,
    by_month: Vec<u32>,
    by_month_day: Vec<i32>,
    /// Sorted by `day_key`, each entry once.
    by_day: Vec<(Option<i32>, Weekday)>,
    /// The weekdays BYDAY names, whatever their ordinals, each once.
    weekdays: Vec<Weekday>,
    by_hour: Vec<u32>,
    by_minute: Vec<u32>,
    by_second: Vec<u32>,
    by_set_pos: Vec<i32>,
    wkst: Weekday,
}

/// What a caller says about a wall-clock start: its UTC instant, or None
/// when the event's zone skips that time.
pub(super) type Instant<'a> = &'a dyn Fn(NaiveDateTime) -> Option<NaiveDateTime>;

fn weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

fn list<T>(value: &str, item: impl Fn(&str) -> Option<T>) -> Result<Vec<T>, String> {
    value
        .split(',')
        .map(|part| item(part.trim()).ok_or_else(|| format!("bad value {part:?}")))
        .collect()
}

/// The order BYDAY's entries are kept in (and looked up by).
fn day_key(nth: Option<i32>, wd: Weekday) -> (Option<i32>, u32) {
    (nth, wd.num_days_from_monday())
}

fn signed_in(part: &str, max: i32) -> Option<i32> {
    let n: i32 = part.trim_start_matches('+').parse().ok()?;
    (n != 0 && n.unsigned_abs() <= max.unsigned_abs()).then_some(n)
}

/// A DATE or DATE-TIME as RRULE's UNTIL writes it.
fn parse_until(value: &str) -> Option<Until> {
    if value.len() == 8 {
        return NaiveDate::parse_from_str(value, "%Y%m%d")
            .ok()
            .map(Until::Date);
    }
    match value.strip_suffix('Z') {
        Some(utc) => NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")
            .ok()
            .map(Until::Utc),
        None => NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
            .ok()
            .map(Until::Local),
    }
}

impl Rule {
    pub(super) fn parse(value: &str) -> Result<Rule, String> {
        let mut freq = None;
        let mut rule = Rule {
            freq: Freq::Daily,
            interval: 1,
            count: None,
            until: None,
            by_month: Vec::new(),
            by_month_day: Vec::new(),
            by_day: Vec::new(),
            weekdays: Vec::new(),
            by_hour: Vec::new(),
            by_minute: Vec::new(),
            by_second: Vec::new(),
            by_set_pos: Vec::new(),
            wkst: Weekday::Mon,
        };
        let below = |limit: u32| move |p: &str| p.parse::<u32>().ok().filter(|n| *n < limit);
        for part in value.trim().split(';').filter(|p| !p.trim().is_empty()) {
            let (key, val) = part
                .split_once('=')
                .ok_or_else(|| format!("bad rule part {part:?}"))?;
            let val = val.trim().to_ascii_uppercase();
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match val.as_str() {
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        other => return Err(format!("FREQ={other} is not expanded")),
                    })
                }
                "INTERVAL" => {
                    rule.interval = val
                        .parse::<i64>()
                        .ok()
                        .filter(|n| (1..=10_000).contains(n))
                        .ok_or_else(|| format!("bad INTERVAL {val:?}"))?
                }
                "COUNT" => {
                    rule.count = Some(val.parse().map_err(|_| format!("bad COUNT {val:?}"))?)
                }
                "UNTIL" => {
                    rule.until =
                        Some(parse_until(&val).ok_or_else(|| format!("bad UNTIL {val:?}"))?)
                }
                "BYMONTH" => {
                    rule.by_month = list(&val, |p| {
                        p.parse::<u32>().ok().filter(|m| (1..=12).contains(m))
                    })?
                }
                "BYMONTHDAY" => rule.by_month_day = list(&val, |p| signed_in(p, 31))?,
                "BYSETPOS" => rule.by_set_pos = list(&val, |p| signed_in(p, 366))?,
                "BYDAY" => {
                    rule.by_day = list(&val, |p| {
                        let split = p.len().checked_sub(2)?;
                        let (ordinal, code) = (p.get(..split)?, p.get(split..)?);
                        let ordinal = if ordinal.is_empty() {
                            None
                        } else {
                            Some(signed_in(ordinal, 53)?)
                        };
                        Some((ordinal, weekday(code)?))
                    })?
                }
                "WKST" => rule.wkst = weekday(&val).ok_or_else(|| format!("bad WKST {val:?}"))?,
                "BYHOUR" => rule.by_hour = list(&val, below(24))?,
                "BYMINUTE" => rule.by_minute = list(&val, below(60))?,
                // 60 is a leap second, which no wall clock shows: allowed
                // in the rule, and never a start.
                "BYSECOND" => rule.by_second = list(&val, below(61))?,
                other @ ("BYYEARDAY" | "BYWEEKNO") => {
                    return Err(format!("{other} is not expanded"))
                }
                // Extension parts (X-…) don't change which days recur.
                _ => {}
            }
        }
        rule.freq = freq.ok_or("rule without FREQ")?;
        // A value listed twice is the same value.
        rule.by_month.sort_unstable();
        rule.by_month.dedup();
        rule.by_month_day.sort_unstable();
        rule.by_month_day.dedup();
        rule.by_day.sort_unstable_by_key(|&(n, wd)| day_key(n, wd));
        rule.by_day.dedup();
        rule.weekdays = rule.by_day.iter().map(|(_, wd)| *wd).collect();
        rule.weekdays
            .sort_unstable_by_key(Weekday::num_days_from_monday);
        rule.weekdays.dedup();
        for times in [&mut rule.by_hour, &mut rule.by_minute, &mut rule.by_second] {
            times.sort_unstable();
            times.dedup();
        }
        rule.by_set_pos.sort_unstable();
        rule.by_set_pos.dedup();
        let per_day = [&rule.by_hour, &rule.by_minute, &rule.by_second]
            .iter()
            .map(|listed| listed.len().max(1))
            .product::<usize>();
        if per_day > MAX_TIMES_PER_DAY {
            return Err(format!(
                "{per_day} times of day (more than {MAX_TIMES_PER_DAY}) are not expanded"
            ));
        }
        Ok(rule)
    }

    /// Wall-clock starts the rule makes after `dtstart`, in order: of
    /// those up to `capped_to`, at most `limit` (the last `limit` when
    /// there are more; the second value counts those left out), and every
    /// one after it. DTSTART itself counts as the first occurrence against
    /// COUNT. Nothing later than `end`, or past UNTIL, is made; `instant`
    /// turns a wall-clock start into its UTC instant (for a UTC UNTIL), or
    /// says the zone skips it — then it is left out and not counted. The
    /// third value holds which of the UTC instants in `wanted` the rule
    /// makes on the way, kept or left out, each with the wall-clock start
    /// it makes there.
    pub(super) fn after_start(
        &self,
        dtstart: NaiveDateTime,
        capped_to: NaiveDateTime,
        end: NaiveDateTime,
        limit: usize,
        instant: Instant,
        wanted: &HashSet<NaiveDateTime>,
    ) -> (
        Vec<NaiveDateTime>,
        usize,
        HashMap<NaiveDateTime, NaiveDateTime>,
    ) {
        let mut kept = std::collections::VecDeque::new();
        let mut left_out = 0usize;
        let mut found = HashMap::new();
        self.walk(dtstart, end, None, instant, |at, utc| {
            if wanted.contains(&utc) {
                found.insert(utc, at);
            }
            kept.push_back(at);
            // Starts come in order: while they are at most `capped_to`,
            // every kept one is.
            if at <= capped_to && kept.len() > limit {
                kept.pop_front();
                left_out += 1;
            }
        });
        (kept.into(), left_out, found)
    }

    /// The latest start the rule makes at or before `at` (DTSTART not
    /// included). Used for a time-zone definition's yearly changes, so
    /// it skips straight to the periods near `at` — or near UNTIL, when
    /// the rule ended before `at` — when no COUNT has to be counted from
    /// the beginning. A rule can go years without a start (February 29),
    /// so when those periods hold none it looks twice as far back, and so
    /// on down to DTSTART.
    pub(super) fn last_at_or_before(
        &self,
        dtstart: NaiveDateTime,
        at: NaiveDateTime,
        instant: Instant,
    ) -> Option<NaiveDateTime> {
        // With COUNT the starts are counted from DTSTART: walk them all.
        let near = self.count.is_none().then(|| {
            // A rule that ended makes its last start at or before UNTIL
            // (a UTC UNTIL's date is within a day of the wall clock's; the
            // margin covers it).
            let until = self.until.map(|until| match until {
                Until::Date(day) => day,
                Until::Local(t) | Until::Utc(t) => t.date(),
            });
            until.map_or(at.date(), |until| until.min(at.date()))
        });
        let mut back = 400i64;
        loop {
            let skip_to = near
                .and_then(|near| near.checked_sub_signed(Duration::try_days(back)?))
                .filter(|day| *day > dtstart.date());
            let mut last = None;
            self.walk(dtstart, at, skip_to, instant, |t, _| last = Some(t));
            // A start in the periods walked is the latest; with none, the
            // latest is before them, if there is one.
            if last.is_some() || skip_to.is_none() {
                return last;
            }
            back = back.saturating_mul(2);
        }
    }

    /// `at` (wall clock) at UTC instant `utc` is past UNTIL.
    fn past_until(&self, at: NaiveDateTime, utc: NaiveDateTime) -> bool {
        match self.until {
            None => false,
            Some(Until::Date(day)) => at.date() > day,
            Some(Until::Local(until)) => at > until,
            Some(Until::Utc(until)) => utc > until,
        }
    }

    fn walk(
        &self,
        dtstart: NaiveDateTime,
        end: NaiveDateTime,
        skip_to: Option<NaiveDate>,
        instant: Instant,
        mut visit: impl FnMut(NaiveDateTime, NaiveDateTime),
    ) {
        #[cfg(test)]
        WALKS.with(|walks| walks.set(walks.get() + 1));
        let first = skip_to
            .map(|day| self.period_index(dtstart.date(), day))
            .unwrap_or(0);
        let times = self.times(dtstart.time());
        // DTSTART is the first occurrence, rule or no rule.
        let mut made = 1u32;
        let mut looked = 0usize;
        for k in first..first + MAX_PERIODS {
            let Some(period) = self.period_start(dtstart.date(), k) else {
                return;
            };
            if period > end.date() {
                return;
            }
            let days = self.candidates(period, dtstart.date());
            looked = looked.saturating_add(days.len().saturating_mul(times.len()));
            if looked > MAX_STARTS {
                return;
            }
            for (at, utc) in self.starts(&days, &times, instant) {
                if at <= dtstart {
                    continue;
                }
                if at > end {
                    return;
                }
                if self.past_until(at, utc) {
                    return;
                }
                if self.count.is_some_and(|count| made >= count) {
                    return;
                }
                made = made.saturating_add(1);
                visit(at, utc);
            }
        }
    }

    /// The first day of period `k` of the series that starts on `start`.
    fn period_start(&self, start: NaiveDate, k: i64) -> Option<NaiveDate> {
        let steps = k.checked_mul(self.interval)?;
        match self.freq {
            Freq::Daily => start.checked_add_signed(Duration::try_days(steps)?),
            Freq::Weekly => week_start(start, self.wkst)
                .checked_add_signed(Duration::try_days(steps.checked_mul(7)?)?),
            Freq::Monthly => {
                let month = i64::from(start.year()) * 12 + i64::from(start.month0()) + steps;
                NaiveDate::from_ymd_opt(
                    i32::try_from(month.div_euclid(12)).ok()?,
                    month.rem_euclid(12) as u32 + 1,
                    1,
                )
            }
            Freq::Yearly => {
                NaiveDate::from_ymd_opt(i32::try_from(i64::from(start.year()) + steps).ok()?, 1, 1)
            }
        }
    }

    /// The index of the last period that begins on or before `day`.
    fn period_index(&self, start: NaiveDate, day: NaiveDate) -> i64 {
        let units = match self.freq {
            Freq::Daily => (day - start).num_days(),
            Freq::Weekly => (day - week_start(start, self.wkst))
                .num_days()
                .div_euclid(7),
            Freq::Monthly => {
                (i64::from(day.year()) * 12 + i64::from(day.month0()))
                    - (i64::from(start.year()) * 12 + i64::from(start.month0()))
            }
            Freq::Yearly => i64::from(day.year() - start.year()),
        };
        units.div_euclid(self.interval).max(0)
    }

    /// The times of day an occurrence starts at: every BYHOUR × BYMINUTE ×
    /// BYSECOND, each DTSTART's own when the rule doesn't list it; sorted.
    /// A leap second has no wall-clock time and makes none.
    fn times(&self, start: NaiveTime) -> Vec<NaiveTime> {
        let or_start = |listed: &[u32], own: u32| {
            if listed.is_empty() {
                vec![own]
            } else {
                listed.to_vec()
            }
        };
        let mut times = Vec::new();
        for hour in or_start(&self.by_hour, start.hour()) {
            for minute in or_start(&self.by_minute, start.minute()) {
                for second in or_start(&self.by_second, start.second()) {
                    times.extend(NaiveTime::from_hms_opt(hour, minute, second));
                }
            }
        }
        times.sort();
        times.dedup();
        times
    }

    /// The starts of one period, sorted, each with its UTC instant: each
    /// of `days` (sorted) at each of `times` (sorted), less the ones the
    /// zone skips — they are no occurrences, so they take no position —
    /// then BYSETPOS over that whole set.
    fn starts(
        &self,
        days: &[NaiveDate],
        times: &[NaiveTime],
        instant: Instant,
    ) -> Vec<(NaiveDateTime, NaiveDateTime)> {
        let starts: Vec<(NaiveDateTime, NaiveDateTime)> = days
            .iter()
            .flat_map(|day| times.iter().map(move |t| day.and_time(*t)))
            .filter_map(|at| Some((at, instant(at)?)))
            .collect();
        if self.by_set_pos.is_empty() {
            return starts;
        }
        let len = i64::try_from(starts.len()).unwrap_or(i64::MAX);
        let mut picked: Vec<(NaiveDateTime, NaiveDateTime)> = self
            .by_set_pos
            .iter()
            .filter_map(|&pos| {
                let pos = i64::from(pos);
                let index = if pos > 0 { pos - 1 } else { len + pos };
                usize::try_from(index)
                    .ok()
                    .and_then(|i| starts.get(i).copied())
            })
            .collect();
        picked.sort();
        picked.dedup();
        picked
    }

    /// The days of one period the rule picks, sorted.
    fn candidates(&self, period: NaiveDate, start: NaiveDate) -> Vec<NaiveDate> {
        let mut days: Vec<NaiveDate> = match self.freq {
            Freq::Daily => vec![period],
            Freq::Weekly => {
                let offsets: Vec<i64> = if self.weekdays.is_empty() {
                    vec![days_from(start.weekday(), self.wkst)]
                } else {
                    self.weekdays
                        .iter()
                        .map(|wd| days_from(*wd, self.wkst))
                        .collect()
                };
                offsets
                    .into_iter()
                    .filter_map(|o| period.checked_add_signed(Duration::days(o)))
                    .collect()
            }
            Freq::Monthly => self.month_days(period.year(), period.month(), start),
            Freq::Yearly => {
                let year = period.year();
                if self.by_month.is_empty()
                    && self.by_month_day.is_empty()
                    && !self.by_day.is_empty()
                {
                    // BYDAY alone in a yearly rule counts within the year.
                    year_span(year)
                        .map(|(first, last)| self.days_named(first, last))
                        .unwrap_or_default()
                } else {
                    let months: Vec<u32> = if !self.by_month.is_empty() {
                        self.by_month.clone()
                    } else if !self.by_month_day.is_empty() || !self.by_day.is_empty() {
                        (1..=12).collect()
                    } else {
                        vec![start.month()]
                    };
                    months
                        .into_iter()
                        .flat_map(|m| self.month_days(year, m, start))
                        .collect()
                }
            }
        };
        // What the frequency didn't expand, it limits.
        if !self.by_month.is_empty() {
            days.retain(|d| self.by_month.contains(&d.month()));
        }
        if self.freq == Freq::Daily {
            if !self.by_month_day.is_empty() {
                days.retain(|d| {
                    let last = days_in_month(d.year(), d.month()) as i32;
                    self.by_month_day
                        .iter()
                        .any(|&md| md == d.day() as i32 || last + 1 + md == d.day() as i32)
                });
            }
            if !self.weekdays.is_empty() {
                days.retain(|d| self.weekdays.contains(&d.weekday()));
            }
        }
        days.sort();
        days.dedup();
        days
    }

    /// The days of one month the rule picks (unsorted).
    fn month_days(&self, year: i32, month: u32, start: NaiveDate) -> Vec<NaiveDate> {
        let Some((first, end)) = month_span(year, month) else {
            return Vec::new();
        };
        if !self.by_month_day.is_empty() {
            let last = end.day() as i32;
            let mut days: Vec<NaiveDate> = self
                .by_month_day
                .iter()
                .filter_map(|&md| {
                    let day = if md > 0 { md } else { last + 1 + md };
                    u32::try_from(day)
                        .ok()
                        .and_then(|d| NaiveDate::from_ymd_opt(year, month, d))
                })
                .collect();
            if !self.by_day.is_empty() {
                // BYDAY limits the days; an ordinal still counts within
                // the month, or the year in a yearly rule without BYMONTH.
                let span = if self.freq == Freq::Yearly && self.by_month.is_empty() {
                    year_span(year)
                } else {
                    Some((first, end))
                };
                days.retain(|d| span.is_some_and(|(from, to)| self.by_day_names(*d, from, to)));
            }
            days
        } else if !self.by_day.is_empty() {
            self.days_named(first, end)
        } else {
            // A day the month doesn't have (the 31st, 29 February) is
            // skipped, never moved.
            NaiveDate::from_ymd_opt(year, month, start.day())
                .into_iter()
                .collect()
        }
    }

    /// The days from `first` to `last` (a month or a year) BYDAY names.
    fn days_named(&self, first: NaiveDate, last: NaiveDate) -> Vec<NaiveDate> {
        first
            .iter_days()
            .take_while(|d| *d <= last)
            .filter(|d| self.by_day_names(*d, first, last))
            .collect()
    }

    /// Whether BYDAY names `day`, a day of the span `first..=last` (a
    /// month or a year): its weekday alone, or with the ordinal `day` has
    /// among that weekday's days in the span, counted from the start (1MO)
    /// or from the end (-1MO).
    fn by_day_names(&self, day: NaiveDate, first: NaiveDate, last: NaiveDate) -> bool {
        let wd = day.weekday();
        let named = |nth: Option<i32>| {
            self.by_day
                .binary_search_by_key(&day_key(nth, wd), |&(n, w)| day_key(n, w))
                .is_ok()
        };
        let from_start = (day - first).num_days() / 7 + 1;
        let from_end = (last - day).num_days() / 7 + 1;
        named(None)
            || [from_start, -from_end]
                .into_iter()
                .any(|n| i32::try_from(n).is_ok_and(|n| named(Some(n))))
    }
}

fn month_span(year: i32, month: u32) -> Option<(NaiveDate, NaiveDate)> {
    Some((
        NaiveDate::from_ymd_opt(year, month, 1)?,
        NaiveDate::from_ymd_opt(year, month, days_in_month(year, month))?,
    ))
}

fn year_span(year: i32) -> Option<(NaiveDate, NaiveDate)> {
    Some((
        NaiveDate::from_ymd_opt(year, 1, 1)?,
        NaiveDate::from_ymd_opt(year, 12, 31)?,
    ))
}

fn days_from(day: Weekday, week_start: Weekday) -> i64 {
    i64::from((day.num_days_from_monday() + 7 - week_start.num_days_from_monday()) % 7)
}

fn week_start(day: NaiveDate, wkst: Weekday) -> NaiveDate {
    day.checked_sub_signed(Duration::days(days_from(day.weekday(), wkst)))
        .unwrap_or(day)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next.and_then(|n| n.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").unwrap()
    }

    fn expand(rule: &str, start: &str, end: &str) -> Vec<String> {
        expand_where(rule, start, end, &|t| Some(t))
    }

    fn expand_where(rule: &str, start: &str, end: &str, instant: Instant) -> Vec<String> {
        Rule::parse(rule)
            .unwrap()
            .after_start(
                at(start),
                at(end),
                at(end),
                10_000,
                instant,
                &HashSet::new(),
            )
            .0
            .iter()
            .map(|t| t.format("%Y%m%dT%H%M%S").to_string())
            .collect()
    }

    /// BYHOUR/BYMINUTE/BYSECOND give each day its start times; BYSETPOS
    /// picks among all of a period's starts.
    #[test]
    fn test_times_of_day() {
        let days = expand(
            "FREQ=DAILY;COUNT=3;BYHOUR=9;BYMINUTE=0;BYSECOND=0",
            "20260301T090000",
            "20300101T000000",
        );
        assert_eq!(days, ["20260302T090000", "20260303T090000"]);
        let days = expand(
            "FREQ=DAILY;COUNT=4;BYHOUR=17,9",
            "20260301T090000",
            "20300101T000000",
        );
        assert_eq!(
            days,
            ["20260301T170000", "20260302T090000", "20260302T170000"]
        );
        let days = expand(
            "FREQ=WEEKLY;BYDAY=TU;BYHOUR=8;BYMINUTE=15,45;COUNT=3",
            "20260303T081500",
            "20300101T000000",
        );
        assert_eq!(days, ["20260303T084500", "20260310T081500"]);
        let days = expand(
            "FREQ=DAILY;BYHOUR=9,12,17;BYSETPOS=-1;COUNT=3",
            "20260301T170000",
            "20300101T000000",
        );
        assert_eq!(days, ["20260302T170000", "20260303T170000"]);
        // A leap second is allowed in the rule and makes no start.
        let days = expand(
            "FREQ=DAILY;BYSECOND=0,60;COUNT=2",
            "20260301T090000",
            "20300101T000000",
        );
        assert_eq!(days, ["20260302T090000"]);
    }

    /// A start the zone skips is left out and doesn't count: a daily
    /// series of three at 02:30 across the night clocks go forward
    /// keeps the 7th (DTSTART), the 9th and the 10th.
    #[test]
    fn test_skipped_times_are_not_occurrences() {
        let gap = at("20260308T023000");
        let days = expand_where(
            "FREQ=DAILY;COUNT=3",
            "20260307T023000",
            "20300101T000000",
            &|t| (t != gap).then_some(t),
        );
        assert_eq!(days, ["20260309T023000", "20260310T023000"]);
    }

    /// BYSETPOS picks among the starts that happen: on the night clocks
    /// go forward, 02:30 doesn't exist, so the day's first start is 03:30.
    #[test]
    fn test_set_position_counts_only_starts_that_happen() {
        let gap = at("20260308T023000");
        let days = expand_where(
            "FREQ=DAILY;BYHOUR=2,3;BYMINUTE=30;BYSETPOS=1;COUNT=3",
            "20260307T023000",
            "20300101T000000",
            &|t| (t != gap).then_some(t),
        );
        assert_eq!(days, ["20260308T033000", "20260309T023000"]);
    }

    /// An ordinal BYDAY that limits BYMONTHDAY keeps its ordinal: 1MO is
    /// a first Monday, so 9 February (the second) is no occurrence.
    #[test]
    fn test_month_days_limited_by_nth_weekday() {
        let days = expand(
            "FREQ=MONTHLY;BYMONTHDAY=2,9;BYDAY=1MO;COUNT=3",
            "20260202T090000",
            "20300101T000000",
        );
        assert_eq!(days, ["20260302T090000", "20261102T090000"]);
        // In a yearly rule without BYMONTH, the ordinal counts in the year:
        // 5 January 2026 is the year's first Monday and 2 February its
        // fifth; the next first Monday on a 2nd or 5th is 5 January 2032.
        let days = expand(
            "FREQ=YEARLY;BYMONTHDAY=2,5;BYDAY=1MO;COUNT=2",
            "20260105T090000",
            "20330101T000000",
        );
        assert_eq!(days, ["20320105T090000"]);
    }

    /// A value listed many times counts once, a rule naming more times of
    /// day than minutes is refused, and an expansion that would make an
    /// unbounded number of starts stops at its budget.
    #[test]
    fn test_rule_cost_is_bounded() {
        let hours = vec!["9"; 10_000].join(",");
        let minutes = vec!["0"; 10_000].join(",");
        let rule = Rule::parse(&format!(
            "FREQ=DAILY;COUNT=2;BYHOUR={hours};BYMINUTE={minutes}"
        ))
        .unwrap();
        assert_eq!(rule.times(at("20260301T090000").time()).len(), 1);
        let none = HashSet::new();
        let (made, _, _) = rule.after_start(
            at("20260301T090000"),
            at("20300101T000000"),
            at("20300101T000000"),
            10,
            &|t| Some(t),
            &none,
        );
        assert_eq!(made, [at("20260302T090000")]);
        let days = vec!["MO"; 10_000].join(",");
        let rule = Rule::parse(&format!("FREQ=WEEKLY;BYDAY={days}")).unwrap();
        assert_eq!(rule.by_day.len(), 1);

        let every = |n: u32| (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        assert!(Rule::parse(&format!(
            "FREQ=DAILY;BYHOUR={};BYMINUTE={};BYSECOND=0,30",
            every(24),
            every(60)
        ))
        .is_err());

        // A start every minute of every day of the year, from 1900.
        let rule = Rule::parse(&format!(
            "FREQ=YEARLY;BYDAY=MO,TU,WE,TH,FR,SA,SU;BYHOUR={};BYMINUTE={}",
            every(24),
            every(60)
        ))
        .unwrap();
        let (kept, left_out, _) = rule.after_start(
            at("19000101T000000"),
            at("20300101T000000"),
            at("20300101T000000"),
            10,
            &|t| Some(t),
            &none,
        );
        assert_eq!(kept.len(), 10);
        assert!(left_out + kept.len() <= MAX_STARTS, "{left_out}");

        // Every ordinal of every weekday, limiting every day of the month:
        // each day is looked up once, not once per BYDAY entry.
        let ordinals: Vec<String> = (1..=53)
            .flat_map(|n| [n, -n])
            .flat_map(|n| {
                ["MO", "TU", "WE", "TH", "FR", "SA", "SU"]
                    .into_iter()
                    .map(move |wd| format!("{n}{wd}"))
            })
            .collect();
        let month_days: Vec<String> = (1..=31)
            .flat_map(|d| [d, -d])
            .map(|d| d.to_string())
            .collect();
        let rule = Rule::parse(&format!(
            "FREQ=YEARLY;BYMONTHDAY={};BYDAY={}",
            month_days.join(","),
            ordinals.join(",")
        ))
        .unwrap();
        assert_eq!(rule.by_day.len(), 742);
        let (kept, left_out, _) = rule.after_start(
            at("19000101T090000"),
            at("20300101T000000"),
            at("20300101T000000"),
            10,
            &|t| Some(t),
            &none,
        );
        assert_eq!(kept.last(), Some(&at("20291231T090000")));
        assert!(left_out > 40_000, "{left_out}");
    }

    #[test]
    fn test_weekly_count_includes_dtstart() {
        let days = expand("FREQ=WEEKLY;COUNT=4", "20260302T140000", "20300101T000000");
        assert_eq!(
            days,
            ["20260309T140000", "20260316T140000", "20260323T140000"]
        );
    }

    #[test]
    fn test_weekly_byday_interval_and_until() {
        // Every other week on Monday and Wednesday, until the 25th.
        let days = expand(
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;UNTIL=20260325T235959Z",
            "20260302T090000",
            "20300101T000000",
        );
        assert_eq!(
            days,
            ["20260304T090000", "20260316T090000", "20260318T090000"]
        );
    }

    #[test]
    fn test_monthly_nth_and_last_weekday() {
        let days = expand(
            "FREQ=MONTHLY;BYDAY=-1FR;COUNT=3",
            "20260130T120000",
            "20300101T000000",
        );
        assert_eq!(days, ["20260227T120000", "20260327T120000"]);
        let days = expand(
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;COUNT=3",
            "20260130T170000",
            "20300101T000000",
        );
        // Last weekday of February and March 2026.
        assert_eq!(days, ["20260227T170000", "20260331T170000"]);
    }

    #[test]
    fn test_monthly_skips_missing_days() {
        let days = expand("FREQ=MONTHLY;COUNT=4", "20260131T080000", "20300101T000000");
        assert_eq!(
            days,
            ["20260331T080000", "20260531T080000", "20260731T080000"]
        );
    }

    #[test]
    fn test_yearly_by_month_and_nth_day() {
        // US Thanksgiving.
        let days = expand(
            "FREQ=YEARLY;BYMONTH=11;BYDAY=4TH",
            "20241128T150000",
            "20271231T000000",
        );
        assert_eq!(
            days,
            ["20251127T150000", "20261126T150000", "20271125T150000"]
        );
    }

    #[test]
    fn test_daily_weekdays_only_and_end_bound() {
        let days = expand(
            "FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR",
            "20260305T093000",
            "20260310T100000",
        );
        assert_eq!(
            days,
            ["20260306T093000", "20260309T093000", "20260310T093000"]
        );
    }

    #[test]
    fn test_a_rule_that_never_matches_ends() {
        assert!(expand(
            "FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30",
            "20260101T000000",
            "29991231T000000"
        )
        .is_empty());
    }

    #[test]
    fn test_unsupported_parts_are_refused() {
        assert!(Rule::parse("FREQ=HOURLY").is_err());
        assert!(Rule::parse("FREQ=DAILY;BYHOUR=24").is_err());
        assert!(Rule::parse("FREQ=DAILY;BYMINUTE=60").is_err());
        assert!(Rule::parse("FREQ=YEARLY;BYWEEKNO=20").is_err());
        assert!(Rule::parse("INTERVAL=2").is_err());
        assert!(Rule::parse("FREQ=WEEKLY;BYDAY=XX").is_err());
    }

    /// The limit keeps the latest starts; the instants asked for are found
    /// whether they are kept or left out, and none past the end.
    #[test]
    fn test_limit_keeps_the_latest() {
        let wanted: HashSet<NaiveDateTime> = [
            at("20260102T080000"),
            at("20260110T080000"),
            at("20260120T080000"),
        ]
        .into_iter()
        .collect();
        let (kept, left_out, found) = Rule::parse("FREQ=DAILY").unwrap().after_start(
            at("20260101T080000"),
            at("20260111T080000"),
            at("20260111T080000"),
            3,
            &|t| Some(t),
            &wanted,
        );
        assert_eq!(left_out, 7);
        assert_eq!(
            kept,
            [
                at("20260109T080000"),
                at("20260110T080000"),
                at("20260111T080000")
            ]
        );
        assert_eq!(
            found,
            [
                (at("20260102T080000"), at("20260102T080000")),
                (at("20260110T080000"), at("20260110T080000")),
            ]
            .into_iter()
            .collect::<HashMap<_, _>>()
        );
    }

    #[test]
    fn test_last_at_or_before_skips_ahead() {
        // A time-zone change rule that began in 1601.
        let rule = Rule::parse("FREQ=YEARLY;BYDAY=2SU;BYMONTH=3").unwrap();
        let instant = |t| Some(t);
        let last = rule.last_at_or_before(at("16010311T020000"), at("20260401T000000"), &instant);
        assert_eq!(last, Some(at("20260308T020000")));
        let last = rule.last_at_or_before(at("16010311T020000"), at("20260301T000000"), &instant);
        assert_eq!(last, Some(at("20250309T020000")));
    }

    /// A rule that makes a start only every few years still has its
    /// latest one found, also when it ended (UNTIL) years before.
    #[test]
    fn test_last_at_or_before_finds_a_sparse_rules_start() {
        let instant = |t| Some(t);
        let leap_day = Rule::parse("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=29").unwrap();
        let last =
            leap_day.last_at_or_before(at("20000229T000000"), at("20260615T000000"), &instant);
        assert_eq!(last, Some(at("20240229T000000")));
        let ended =
            Rule::parse("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=29;UNTIL=20220228T230000Z").unwrap();
        let last = ended.last_at_or_before(at("20000229T000000"), at("20260615T000000"), &instant);
        assert_eq!(last, Some(at("20200229T000000")));
        // None at all after DTSTART.
        let never = Rule::parse("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=30").unwrap();
        let last = never.last_at_or_before(at("20000101T000000"), at("20260615T000000"), &instant);
        assert_eq!(last, None);
    }

    /// The limit applies to the starts up to `capped_to`; every later one
    /// up to the end is kept.
    #[test]
    fn test_limit_applies_up_to_capped_to() {
        let (kept, left_out, _) = Rule::parse("FREQ=DAILY").unwrap().after_start(
            at("20260101T080000"),
            at("20260105T080000"),
            at("20260108T080000"),
            2,
            &|t| Some(t),
            &HashSet::new(),
        );
        assert_eq!(left_out, 2);
        assert_eq!(
            kept,
            [
                at("20260104T080000"),
                at("20260105T080000"),
                at("20260106T080000"),
                at("20260107T080000"),
                at("20260108T080000"),
            ]
        );
    }
}

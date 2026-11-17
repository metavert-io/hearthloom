//! ICS calendar adapter: VEVENTs become episodes; attendee emails drive
//! person entity resolution (attribute-based identity — the same email in
//! a git commit and a calendar invite is the same Person).
//!
//! Hand-rolled parser for the subset we need (unfolding, property params,
//! VEVENT and VTIMEZONE blocks). A property belongs to the component it
//! sits directly in: what an event's nested components carry (an alarm's
//! ATTENDEE and DESCRIPTION) never becomes the event's.
//!
//! Time zones: a TZID resolves as an IANA name (chrono-tz), a Windows zone
//! name (what Outlook and Exchange write), or the file's own VTIMEZONE
//! definition, in that order. Timezone honesty: all-day events get
//! day-level uncertainty; floating times (no TZID, no Z — or a TZID none
//! of the above resolves) get hour-level uncertainty instead of a
//! fabricated offset. A wall-clock time a daylight-saving change skips is
//! read with the offset in force before the change (RFC 5545 §3.3.5); a
//! repeated one as its first occurrence.
//!
//! Recurring events: RRULE and RDATE occurrences are expanded, EXDATEs
//! left out, and an occurrence the file overrides (a VEVENT with the
//! series' UID and a RECURRENCE-ID) comes from the override alone — with
//! RANGE=THISANDFUTURE, every later occurrence too, moved as the override
//! moved its own. An override takes an occurrence from the series only
//! when it can stand in for it: its start can be read, or it cancels the
//! occurrence. The first occurrence keeps the bare UID as its ref; each
//! later one is `<UID>#<start>`, the start written the way the series'
//! DTSTART writes it (as RECURRENCE-ID usually does), so an override, a
//! phone-synced instance and a later import of the same occurrence share
//! one ref. An occurrence only an RDATE adds is written the way that RDATE
//! writes it; one a rule makes as well keeps the rule's ref, also in an
//! import made after the series has left that start out (it keeps its
//! latest `MAX_OCCURRENCES`). An override's ref is the one the series
//! gives the occurrence it names, however its RECURRENCE-ID is written;
//! without the series' own event in the file, or when a RECURRENCE-ID
//! written as a DATE names a day of a series of date-times, it is
//! `<UID>#<RECURRENCE-ID>`. A start the rule makes at a time a
//! daylight-saving change skips is no occurrence (RFC 5545 §3.3.10).
//!
//! The occurrences of a recurring event — its first, the ones its rules
//! and RDATEs add, and the ones its overrides move — are made only once
//! they have begun: one still to come can be moved or cancelled, and the
//! store keeps the text a ref first landed with (calendar refs can't take
//! revisions — a sender chooses the UID), so a pre-made future occurrence
//! would hide the later correction. A later import adds the occurrences
//! that have begun since, with the overrides and EXDATEs its file
//! carries. A series keeps its latest `MAX_OCCURRENCES`. An event that
//! doesn't recur imports whatever its date.
//!
//! A cancelled event or occurrence (STATUS:CANCELLED) becomes no episode.
//! One event that can't be read (no DTSTART, a malformed one) is left out
//! with a warning; the rest of the file still imports.

mod rrule;
mod windows_zones;

#[cfg(test)]
mod fuzz_tests;
#[cfg(test)]
mod matrix_tests;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, Duration, FixedOffset, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use hearthloom_core::{
    one_line, EdgeAssertion, EntityRef, EpisodeDraft, EventTime, TimeUncertainty,
};

/// The most occurrences one series' rule may add (the latest are kept):
/// ten years of a daily event.
const MAX_OCCURRENCES: usize = 3660;

/// Parse an .ics file into episode drafts: one per event, or per
/// occurrence of a recurring one that has begun. `scope: None` lets the
/// store infer scope from the organizer/attendee identities.
pub fn ingest(path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    drafts(
        &raw,
        scope,
        Utc::now().naive_utc(),
        &path.display().to_string(),
    )
}

/// [`ingest`] over a calendar's text already read (`label` names it in
/// warnings and errors) — what `hearthloom repair calendar` reads.
pub fn ingest_text(raw: &str, scope: Option<&str>, label: &str) -> Result<Vec<EpisodeDraft>> {
    drafts(raw, scope, Utc::now().naive_utc(), label)
}

/// Every event of a calendar as drafts, as imported at `imported_at`
/// (UTC): recurring series expanded up to then. An event that fails is
/// left out with a warning; only a file none of whose events can be read
/// is an error.
fn drafts(
    raw: &str,
    scope: Option<&str>,
    imported_at: NaiveDateTime,
    label: &str,
) -> Result<Vec<EpisodeDraft>> {
    let cal = parse_calendar(raw);
    if cal.events.is_empty() {
        return Err(anyhow!("no VEVENTs found in {label}"));
    }
    for tzid in cal.unresolved_zones() {
        tracing::warn!(
            "{label}: time zone {tzid:?} is neither known nor defined in the file; \
             its times are read as floating"
        );
    }
    // An occurrence still to come isn't made (see the module docs).
    let expansion = cal.expansion(imported_at);
    let mut out = Vec::new();
    let mut failed = 0usize;
    let mut last_error = None;
    for event in &cal.events {
        match event_drafts(&cal, event, &expansion, scope) {
            Ok(drafts) => out.extend(drafts),
            Err(err) => {
                failed += 1;
                tracing::warn!("{label}: skipping an event: {err:#}");
                last_error = Some(err);
            }
        }
    }
    if out.is_empty() {
        if let Some(err) = last_error {
            return Err(err.context(format!(
                "none of the {failed} events in {label} could be read"
            )));
        }
    }
    if failed > 0 {
        tracing::warn!("{label}: {failed} of {} events left out", cal.events.len());
    }
    Ok(out)
}

/// One parsed property: value + params.
#[derive(Debug, Clone)]
struct Prop {
    value: String,
    params: HashMap<String, String>,
}

/// A component (VEVENT, STANDARD, …) as a multimap of the properties
/// directly inside it.
#[derive(Debug, Default)]
struct Component {
    props: HashMap<String, Vec<Prop>>,
}

impl Component {
    fn first(&self, name: &str) -> Option<&Prop> {
        self.props.get(name).and_then(|v| v.first())
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.first(name).map(|p| p.value.as_str())
    }
    fn all(&self, name: &str) -> &[Prop] {
        self.props.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// A VTIMEZONE: its STANDARD and DAYLIGHT observances.
#[derive(Debug)]
struct VTimezone {
    observances: Vec<Observance>,
}

/// One STANDARD or DAYLIGHT part: from its onsets on, `to` is the offset.
#[derive(Debug)]
struct Observance {
    /// First onset, in the wall-clock time of the offset before it.
    onset: NaiveDateTime,
    /// Offsets in seconds east of UTC, before and after each onset.
    from: i32,
    to: i32,
    rule: Option<rrule::Rule>,
    rdates: Vec<NaiveDateTime>,
}

#[derive(Debug, Default)]
struct Calendar {
    events: Vec<Component>,
    timezones: HashMap<String, VTimezone>,
}

fn unfold(raw: &str) -> Vec<String> {
    // RFC 5545 line unfolding: a line starting with space/tab continues
    // the previous line.
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && !lines.is_empty() {
            let last = lines.last_mut().unwrap();
            last.push_str(&line[1..]);
        } else {
            lines.push(line.to_string());
        }
    }
    lines
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &line[prefix.len()..])
}

fn parse_calendar(raw: &str) -> Calendar {
    let mut cal = Calendar::default();
    // The open components, outermost first; a property line lands in the
    // innermost one.
    let mut open: Vec<(String, Component)> = Vec::new();
    // Observances read inside the VTIMEZONE being parsed (None = one that
    // can't be used, which makes the whole definition unusable).
    let mut observances: Vec<Option<Observance>> = Vec::new();
    for line in unfold(raw) {
        let line = line.trim_end();
        if let Some(name) = strip_prefix_ci(line, "BEGIN:") {
            let name = name.trim().to_ascii_uppercase();
            if name == "VTIMEZONE" {
                observances.clear();
            }
            open.push((name, Component::default()));
            continue;
        }
        if let Some(name) = strip_prefix_ci(line, "END:") {
            let name = name.trim().to_ascii_uppercase();
            // Close up to the matching BEGIN: a component left without
            // its END closes with its parent.
            let Some(at) = open.iter().rposition(|(n, _)| *n == name) else {
                continue;
            };
            while open.len() > at {
                let (name, component) = open.pop().expect("open component");
                let parent = open.last().map(|(n, _)| n.as_str());
                match name.as_str() {
                    // A VEVENT inside another component isn't an event of
                    // this calendar.
                    "VEVENT" if parent.is_none_or(|p| p == "VCALENDAR") => {
                        cal.events.push(component)
                    }
                    "STANDARD" | "DAYLIGHT" if parent == Some("VTIMEZONE") => {
                        observances.push(observance(&component))
                    }
                    "VTIMEZONE" => {
                        let tzid = component.value("TZID").map(|t| t.trim().to_string());
                        let parts: Option<Vec<Observance>> =
                            std::mem::take(&mut observances).into_iter().collect();
                        if let (Some(tzid), Some(parts)) = (tzid, parts) {
                            if !parts.is_empty() {
                                cal.timezones.insert(tzid, VTimezone { observances: parts });
                            }
                        }
                    }
                    // Everything else (an event's VALARM, a VTODO, …) is
                    // dropped with its properties.
                    _ => {}
                }
            }
            continue;
        }
        if let Some((_, component)) = open.last_mut() {
            if let Some((name, prop)) = parse_prop_line(line) {
                component.props.entry(name).or_default().push(prop);
            }
        }
    }
    cal
}

/// `-0500`, `+0530`, `+013045` → seconds east of UTC.
fn utc_offset(value: &str) -> Option<i32> {
    let value = value.trim();
    let (sign, digits) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => return None,
    };
    if !(digits.len() == 4 || digits.len() == 6) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let field = |range: std::ops::Range<usize>| digits.get(range)?.parse::<i32>().ok();
    let (hours, minutes) = (field(0..2)?, field(2..4)?);
    let seconds = if digits.len() == 6 { field(4..6)? } else { 0 };
    (hours < 24 && minutes < 60 && seconds < 60)
        .then_some(sign * (hours * 3600 + minutes * 60 + seconds))
}

fn observance(c: &Component) -> Option<Observance> {
    let onset = NaiveDateTime::parse_from_str(c.value("DTSTART")?.trim(), "%Y%m%dT%H%M%S")
        .ok()
        .filter(|t| (1..=9999).contains(&t.year()))?;
    let rule = match c.value("RRULE") {
        Some(rule) => Some(rrule::Rule::parse(rule).ok()?),
        None => None,
    };
    Some(Observance {
        onset,
        from: utc_offset(c.value("TZOFFSETFROM")?)?,
        to: utc_offset(c.value("TZOFFSETTO")?)?,
        rule,
        rdates: c
            .all("RDATE")
            .iter()
            .flat_map(|p| p.value.split(','))
            .filter_map(|v| NaiveDateTime::parse_from_str(v.trim(), "%Y%m%dT%H%M%S").ok())
            .collect(),
    })
}

impl Observance {
    /// The latest onset at or before the wall-clock time `at`.
    fn last_onset(&self, at: NaiveDateTime) -> Option<NaiveDateTime> {
        let from = Duration::seconds(i64::from(self.from));
        let instant = move |wall: NaiveDateTime| Some(wall - from);
        [
            (self.onset <= at).then_some(self.onset),
            self.rdates.iter().copied().filter(|d| *d <= at).max(),
            self.rule
                .as_ref()
                .and_then(|rule| rule.last_at_or_before(self.onset, at, &instant)),
        ]
        .into_iter()
        .flatten()
        .max()
    }
}

fn secs(s: i32) -> Duration {
    Duration::seconds(i64::from(s))
}

impl VTimezone {
    /// The observance in force at a wall-clock time, with its latest onset.
    fn latest(&self, at: NaiveDateTime) -> Option<(NaiveDateTime, &Observance)> {
        self.observances
            .iter()
            .filter_map(|o| o.last_onset(at).map(|onset| (onset, o)))
            .max_by_key(|(onset, _)| *onset)
    }

    /// Whether a change skips this wall-clock time.
    fn skips(&self, at: NaiveDateTime) -> bool {
        matches!(self.latest(at), Some((onset, o)) if o.to > o.from && at < onset + secs(o.to - o.from))
    }

    /// The UTC instant of a wall-clock time and the offset in force then.
    fn resolve(&self, at: NaiveDateTime) -> Option<(NaiveDateTime, i32)> {
        Some(match self.latest(at) {
            // In the hour a change skips, the offset before it applies.
            Some((onset, o)) if o.to > o.from && at < onset + secs(o.to - o.from) => {
                (at - secs(o.from), o.to)
            }
            Some((_, o)) => (at - secs(o.to), o.to),
            // Before every onset: the offset the earliest one changes from.
            None => {
                let first = self.observances.iter().min_by_key(|o| o.onset)?;
                (at - secs(first.from), first.from)
            }
        })
    }

    /// The wall-clock time of a UTC instant: the offset of the observance
    /// whose latest onset (an onset is written in the wall clock of the
    /// offset before it) is the latest at or before the instant.
    fn local(&self, utc: NaiveDateTime) -> Option<NaiveDateTime> {
        let offset = self
            .observances
            .iter()
            .filter_map(|o| {
                let onset = o.last_onset(utc + secs(o.from))?;
                Some((onset - secs(o.from), o.to))
            })
            .max_by_key(|(onset, _)| *onset)
            .map(|(_, to)| to)
            .or_else(|| {
                let first = self.observances.iter().min_by_key(|o| o.onset)?;
                Some(first.from)
            })?;
        Some(utc + secs(offset))
    }
}

/// What a TZID resolved to.
#[derive(Debug, Clone, Copy)]
enum Zone<'a> {
    Named(chrono_tz::Tz),
    Defined(&'a VTimezone),
    Floating,
}

/// A TZID chrono-tz knows: an IANA name, a Windows zone name, or an IANA
/// name behind a vendor prefix (`/mozilla.org/20050126_1/America/New_York`).
fn known_zone(tzid: &str) -> Option<chrono_tz::Tz> {
    if let Ok(tz) = tzid.parse() {
        return Some(tz);
    }
    if let Some(tz) = windows_zones::iana(tzid) {
        return Some(tz);
    }
    tzid.match_indices('/')
        .find_map(|(i, _)| tzid[i + 1..].parse().ok())
}

impl Zone<'_> {
    /// Whether two starts' zones are the same zone.
    fn same(&self, other: &Zone) -> bool {
        match (self, other) {
            (Zone::Named(a), Zone::Named(b)) => a == b,
            (Zone::Defined(a), Zone::Defined(b)) => std::ptr::eq(*a, *b),
            (Zone::Floating, Zone::Floating) => true,
            _ => false,
        }
    }

    /// The wall-clock time here of a UTC instant; None for floating time.
    fn local(&self, utc: NaiveDateTime) -> Option<NaiveDateTime> {
        match self {
            Zone::Floating => None,
            Zone::Defined(def) => def.local(utc),
            Zone::Named(tz) => Some(tz.from_utc_datetime(&utc).naive_local()),
        }
    }

    /// Whether this wall-clock time happens here (a daylight-saving change
    /// skips some; floating time has every one).
    fn has(&self, at: NaiveDateTime) -> bool {
        match self {
            Zone::Floating => true,
            Zone::Defined(def) => !def.skips(at),
            Zone::Named(tz) => tz.from_local_datetime(&at).earliest().is_some(),
        }
    }

    /// The UTC instant of a wall-clock time and the offset (seconds east)
    /// in force then; None for floating time.
    fn resolve(&self, at: NaiveDateTime) -> Option<(NaiveDateTime, i32)> {
        match self {
            Zone::Floating => None,
            Zone::Defined(def) => def.resolve(at),
            Zone::Named(tz) => {
                let offset_at = |utc: &NaiveDateTime| tz.offset_from_utc_datetime(utc).fix();
                Some(match tz.from_local_datetime(&at).earliest() {
                    Some(local) => (local.naive_utc(), local.offset().fix().local_minus_utc()),
                    None => {
                        // A skipped wall-clock time: read it with the offset
                        // in force before the change (well before it).
                        let before = offset_at(&(at - Duration::hours(48))).local_minus_utc();
                        let utc = at - Duration::seconds(i64::from(before));
                        (utc, offset_at(&utc).local_minus_utc())
                    }
                })
            }
        }
    }
}

impl Calendar {
    fn zone(&self, tzid: Option<&String>) -> Zone<'_> {
        let Some(tzid) = tzid.map(|t| t.trim()).filter(|t| !t.is_empty()) else {
            return Zone::Floating;
        };
        if let Some(tz) = known_zone(tzid) {
            return Zone::Named(tz);
        }
        match self.timezones.get(tzid) {
            Some(def) => Zone::Defined(def),
            None => Zone::Floating,
        }
    }

    /// TZIDs the events use that resolve to nothing.
    fn unresolved_zones(&self) -> BTreeSet<&str> {
        self.events
            .iter()
            .flat_map(|e| e.props.values().flatten())
            .filter_map(|p| p.params.get("TZID"))
            .filter(|tzid| matches!(self.zone(Some(tzid)), Zone::Floating))
            .map(|tzid| tzid.as_str())
            .filter(|tzid| !tzid.trim().is_empty())
            .collect()
    }

    /// What expanding this file's series up to `horizon` needs to know
    /// about all its events: each series' own event and its overrides.
    fn expansion(&self, horizon: NaiveDateTime) -> Expansion<'_> {
        let mut overrides: HashMap<&str, SeriesOverrides<'_>> = HashMap::new();
        let mut masters = HashMap::new();
        for event in &self.events {
            let Some(uid) = event.value("UID") else {
                continue;
            };
            let Some(rid) = event.first("RECURRENCE-ID") else {
                masters.entry(uid).or_insert(event);
                continue;
            };
            let Some(replaced) = self.start(rid) else {
                continue;
            };
            // An override takes its occurrence from the series only when
            // it stands in for it: it imports itself, or cancels it.
            if !cancelled(event) && self.dtstart(event).is_err() {
                continue;
            }
            let series = overrides.entry(uid).or_default();
            if this_and_future(rid) {
                series.ranges.push(replaced);
            } else {
                series.single.push(replaced);
            }
        }
        let rdate_only = masters
            .iter()
            .filter(|(uid, _)| {
                overrides.get(*uid).is_some_and(|o| {
                    o.single
                        .iter()
                        .chain(&o.ranges)
                        .any(|r| !matches!(r.stamp, Stamp::Date(_)))
                })
            })
            .filter_map(|(uid, master)| {
                let first = self.dtstart(master).ok()?;
                Some((*uid, rdate_only(self, master, first)))
            })
            .collect();
        Expansion {
            overrides,
            masters,
            horizon,
            rdate_only,
        }
    }

    /// An event's DTSTART, read.
    fn dtstart(&self, event: &Component) -> Result<Start<'_>> {
        let prop = event
            .first("DTSTART")
            .ok_or_else(|| anyhow!("VEVENT without DTSTART"))?;
        Ok(Start {
            stamp: Stamp::parse(&prop.value, &prop.params)?,
            zone: self.zone(prop.params.get("TZID")),
        })
    }

    fn start(&self, prop: &Prop) -> Option<Start<'_>> {
        Some(Start {
            stamp: Stamp::parse(&prop.value, &prop.params).ok()?,
            zone: self.zone(prop.params.get("TZID")),
        })
    }

    /// Every date an RDATE/EXDATE-style property lists.
    fn listed(&self, event: &Component, name: &str) -> Vec<Start<'_>> {
        event
            .all(name)
            .iter()
            .flat_map(|p| {
                let zone = self.zone(p.params.get("TZID"));
                p.value.split(',').filter_map(move |v| {
                    // A PERIOD (`start/end`) recurs at its start.
                    let v = v.split('/').next().unwrap_or(v);
                    Some(Start {
                        stamp: Stamp::parse(v, &p.params).ok()?,
                        zone,
                    })
                })
            })
            .collect()
    }
}

/// Parse `NAME;PARAM=VAL;PARAM2="q:v":value`, respecting quoted params.
fn parse_prop_line(line: &str) -> Option<(String, Prop)> {
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.trim().to_uppercase();
    if name.is_empty() {
        return None;
    }
    let mut params = HashMap::new();
    for part in parts {
        if let Some((k, v)) = part.split_once('=') {
            params.insert(
                k.trim().to_uppercase(),
                v.trim().trim_matches('"').to_string(),
            );
        }
    }
    Some((
        name,
        Prop {
            value: value.to_string(),
            params,
        },
    ))
}

/// Unescape RFC 5545 text values (\n, \, \; \\).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A DATE or DATE-TIME as the calendar wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stamp {
    Date(NaiveDate),
    Utc(NaiveDateTime),
    /// A wall-clock time: in its TZID's zone, or floating.
    Wall(NaiveDateTime),
}

impl Stamp {
    fn parse(value: &str, params: &HashMap<String, String>) -> Result<Stamp> {
        let value = value.trim();
        let stamp = Stamp::parse_any_year(value, params)?;
        // Calendars write four-digit years; the arithmetic on a start
        // (offsets, lengths, recurrence) stays inside chrono's range.
        if !(1..=9999).contains(&stamp.wall().year()) {
            return Err(anyhow!("date out of range {value:?}"));
        }
        Ok(stamp)
    }

    fn parse_any_year(value: &str, params: &HashMap<String, String>) -> Result<Stamp> {
        // All-day: VALUE=DATE or a bare 8-digit date.
        let is_date = params
            .get("VALUE")
            .is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
            || (value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()));
        if is_date {
            return NaiveDate::parse_from_str(value, "%Y%m%d")
                .map(Stamp::Date)
                .map_err(|e| anyhow!("bad DATE {value:?}: {e}"));
        }
        if let Some(stripped) = value.strip_suffix('Z') {
            return NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
                .map(Stamp::Utc)
                .map_err(|e| anyhow!("bad UTC datetime {value:?}: {e}"));
        }
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
            .map(Stamp::Wall)
            .map_err(|e| anyhow!("bad datetime {value:?}: {e}"))
    }

    /// The wall-clock time (a date's midnight).
    fn wall(self) -> NaiveDateTime {
        match self {
            Stamp::Date(d) => d.and_hms_opt(0, 0, 0).expect("midnight"),
            Stamp::Utc(t) | Stamp::Wall(t) => t,
        }
    }

    /// The same kind of stamp at another wall-clock time.
    fn at(self, wall: NaiveDateTime) -> Stamp {
        match self {
            Stamp::Date(_) => Stamp::Date(wall.date()),
            Stamp::Utc(_) => Stamp::Utc(wall),
            Stamp::Wall(_) => Stamp::Wall(wall),
        }
    }

    /// The stamp as RECURRENCE-ID writes it.
    fn recurrence_id(self) -> String {
        match self {
            Stamp::Date(d) => d.format("%Y%m%d").to_string(),
            Stamp::Utc(t) => t.format("%Y%m%dT%H%M%SZ").to_string(),
            Stamp::Wall(t) => t.format("%Y%m%dT%H%M%S").to_string(),
        }
    }
}

/// A start (DTSTART, RDATE, EXDATE, RECURRENCE-ID) with its zone.
#[derive(Debug, Clone, Copy)]
struct Start<'a> {
    stamp: Stamp,
    zone: Zone<'a>,
}

fn utc_text(t: NaiveDateTime) -> String {
    format!("{}Z", t.format("%Y-%m-%dT%H:%M:%S"))
}

impl Start<'_> {
    /// The UTC instant; floating time reads its wall clock as UTC, a date
    /// its midnight.
    fn instant(&self) -> NaiveDateTime {
        match self.stamp {
            Stamp::Date(_) | Stamp::Utc(_) => self.stamp.wall(),
            Stamp::Wall(t) => self.zone.resolve(t).map_or(t, |(utc, _)| utc),
        }
    }

    fn event_time(&self) -> EventTime {
        match self.stamp {
            Stamp::Date(d) => EventTime {
                utc: Some(utc_text(Stamp::Date(d).wall())),
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Day,
            },
            Stamp::Utc(t) => EventTime {
                utc: Some(utc_text(t)),
                tz_offset_minutes: Some(0),
                uncertainty: TimeUncertainty::Exact,
            },
            Stamp::Wall(t) => {
                match self.zone.resolve(t).and_then(|(utc, off)| {
                    Some(FixedOffset::east_opt(off)?.from_utc_datetime(&utc))
                }) {
                    Some(local) => EventTime::from_fixed(local, TimeUncertainty::Exact),
                    // Floating time: no timezone at all. Record as if UTC but
                    // mark the hour-level uncertainty rather than fabricating
                    // an offset.
                    None => EventTime {
                        utc: Some(utc_text(t)),
                        tz_offset_minutes: None,
                        uncertainty: TimeUncertainty::Hour,
                    },
                }
            }
        }
    }

    /// Whether `other` (an EXDATE or RECURRENCE-ID) names this occurrence:
    /// the same instant, or for a DATE the same day.
    fn is(&self, other: &Start) -> bool {
        match (self.stamp, other.stamp) {
            (_, Stamp::Date(day)) => self.stamp.wall().date() == day,
            (Stamp::Date(_), _) => false,
            _ => self.instant() == other.instant(),
        }
    }

    /// Whether this occurrence is `other` (a RECURRENCE-ID) or later.
    fn at_or_after(&self, other: &Start) -> bool {
        match other.stamp {
            Stamp::Date(day) => self.stamp.wall().date() >= day,
            _ => self.is(other) || self.instant() >= other.instant(),
        }
    }

    /// Whether its zone has this wall-clock time (DATEs and UTC always).
    fn exists(&self) -> bool {
        match self.stamp {
            Stamp::Wall(t) => self.zone.has(t),
            Stamp::Date(_) | Stamp::Utc(_) => true,
        }
    }

    /// `other` written the way this start is: the same kind of stamp, in
    /// this start's zone. None when there's no such reading — a DATE and a
    /// DATE-TIME, or floating time against a zoned one (floating time
    /// names no instant).
    fn express(&self, other: &Start) -> Option<Stamp> {
        match (self.stamp, other.stamp) {
            (Stamp::Date(_), Stamp::Date(day)) => Some(Stamp::Date(day)),
            (Stamp::Date(_), _) | (_, Stamp::Date(_)) => None,
            (Stamp::Wall(_), Stamp::Wall(t)) if self.zone.same(&other.zone) => Some(Stamp::Wall(t)),
            (_, Stamp::Wall(_)) if matches!(other.zone, Zone::Floating) => None,
            (Stamp::Utc(_), _) => Some(Stamp::Utc(other.instant())),
            (Stamp::Wall(_), _) => self.zone.local(other.instant()).map(Stamp::Wall),
        }
    }

    /// `other`'s wall-clock time read the way this start is written (as
    /// written, when there's no such reading).
    fn wall_of(&self, other: &Start) -> NaiveDateTime {
        self.express(other)
            .map_or_else(|| other.stamp.wall(), Stamp::wall)
    }
}

/// A cancelled event or occurrence (STATUS:CANCELLED).
fn cancelled(event: &Component) -> bool {
    event
        .value("STATUS")
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("CANCELLED"))
}

/// A RECURRENCE-ID that also stands for every later occurrence.
fn this_and_future(rid: &Prop) -> bool {
    rid.params
        .get("RANGE")
        .is_some_and(|r| r.trim().eq_ignore_ascii_case("THISANDFUTURE"))
}

/// The occurrences a file's overrides of one series take from it.
#[derive(Default)]
struct SeriesOverrides<'a> {
    /// Replaced (or cancelled) one at a time.
    single: Vec<Start<'a>>,
    /// RANGE=THISANDFUTURE: from each of these on, the override's (or,
    /// cancelled, none).
    ranges: Vec<Start<'a>>,
}

impl SeriesOverrides<'_> {
    fn take(&self, occurrence: &Start) -> bool {
        self.single.iter().any(|x| occurrence.is(x))
            || self.ranges.iter().any(|r| occurrence.at_or_after(r))
    }

    /// The RANGE=THISANDFUTURE override an occurrence follows: the latest
    /// one at or before it.
    fn range_of(&self, occurrence: &Start) -> Option<NaiveDateTime> {
        self.ranges
            .iter()
            .filter(|r| occurrence.at_or_after(r))
            .map(Start::instant)
            .max()
    }
}

/// What expanding a file's series needs to know about all its events.
struct Expansion<'a> {
    overrides: HashMap<&'a str, SeriesOverrides<'a>>,
    /// Each series' own event (its UID, no RECURRENCE-ID), by UID.
    masters: HashMap<&'a str, &'a Component>,
    /// No occurrence the rules make begins after this (the import).
    horizon: NaiveDateTime,
    /// For each series an override names by a date-time: the occurrences
    /// only an RDATE adds, by instant (`rdate_only`), worked out once.
    rdate_only: HashMap<&'a str, HashMap<NaiveDateTime, Start<'a>>>,
}

/// Extract (email, display name) from an ATTENDEE/ORGANIZER property.
fn parse_person(prop: &Prop) -> Option<(String, String)> {
    let value = prop.value.trim();
    let email = value
        .strip_prefix("mailto:")
        .or_else(|| value.strip_prefix("MAILTO:"))
        .unwrap_or(value)
        .trim()
        .to_lowercase();
    if !email.contains('@') {
        return None;
    }
    let name = prop
        .params
        .get("CN")
        .filter(|cn| !cn.is_empty() && !cn.contains('@'))
        .cloned()
        .unwrap_or_else(|| email.split('@').next().unwrap_or(&email).to_string());
    Some((email, name))
}

use hearthloom_core::machine::is_machine_email as is_machine_address;

/// Whether an event recurs (it has a rule or lists more dates): then its
/// first occurrence, like the others, is made only once it has begun.
fn recurs(event: &Component) -> bool {
    !event.all("RRULE").is_empty() || !event.all("RDATE").is_empty()
}

/// The start a series' rule makes at a wall-clock time: written the way
/// its DTSTART is, in DTSTART's zone.
fn rule_start<'a>(first: Start<'a>, wall: NaiveDateTime) -> Start<'a> {
    Start {
        stamp: first.stamp.at(wall),
        zone: first.zone,
    }
}

/// The instant of that start; None when its zone skips the time (then it
/// is no occurrence).
fn rule_instant(first: Start, wall: NaiveDateTime) -> Option<NaiveDateTime> {
    let start = rule_start(first, wall);
    start.exists().then(|| start.instant())
}

/// Every occurrence of a series' own event up to `horizon`, in order and
/// before any override: DTSTART, then what its rules and RDATEs add (a
/// start its zone skips isn't one), minus its EXDATEs. An event that
/// doesn't recur keeps its DTSTART whatever the horizon.
fn series_starts<'a>(
    cal: &'a Calendar,
    event: &Component,
    uid: &str,
    first: Start<'a>,
    horizon: NaiveDateTime,
) -> Vec<Start<'a>> {
    let mut starts: Vec<Start<'a>> = Vec::new();
    if !recurs(event) || first.instant() <= horizon {
        starts.push(first);
    }
    let listed: Vec<Start<'a>> = cal
        .listed(event, "RDATE")
        .into_iter()
        .filter(|s| s.instant() <= horizon)
        .collect();
    let wanted: HashSet<NaiveDateTime> = listed.iter().map(Start::instant).collect();
    // The RDATEs' instants a rule makes too, with the rule's start there.
    let mut by_rule: HashMap<NaiveDateTime, NaiveDateTime> = HashMap::new();
    let instant = |wall: NaiveDateTime| rule_instant(first, wall);
    for prop in event.all("RRULE") {
        match rrule::Rule::parse(&prop.value) {
            Ok(rule) => {
                // The rule walks wall-clock time: a day's margin, then
                // the horizon itself is applied to each instant.
                let (made, left_out, found) = rule.after_start(
                    first.stamp.wall(),
                    horizon + Duration::days(1),
                    MAX_OCCURRENCES,
                    &instant,
                    &wanted,
                );
                if left_out > 0 {
                    tracing::warn!(
                        "calendar event {uid}: {left_out} earliest occurrences left out \
                         (a series keeps its latest {MAX_OCCURRENCES})"
                    );
                }
                starts.extend(
                    made.into_iter()
                        .map(|wall| rule_start(first, wall))
                        .filter(|s| s.instant() <= horizon),
                );
                for (utc, wall) in found {
                    by_rule.entry(utc).or_insert(wall);
                }
            }
            Err(err) => tracing::warn!(
                "calendar event {uid}: recurrence rule not expanded ({err}); \
                 keeping the occurrences the file lists"
            ),
        }
    }
    // An RDATE at an instant a rule makes is that rule's occurrence, under
    // the rule's ref, also once the series has left the rule's start out
    // (it keeps the latest MAX_OCCURRENCES): the ref doesn't change as
    // later imports move on.
    starts.extend(listed.into_iter().map(|s| {
        by_rule
            .get(&s.instant())
            .map_or(s, |wall| rule_start(first, *wall))
    }));
    let excluded = cal.listed(event, "EXDATE");
    // Stable: DTSTART stays ahead of an RDATE naming the same instant.
    starts.sort_by_cached_key(Start::instant);
    starts.dedup_by_key(|s| s.instant());
    starts.retain(|s| !excluded.iter().any(|x| s.is(x)));
    starts
}

/// The ref a series gives one of its occurrences.
fn occurrence_ref(uid: &str, first: &Start, occurrence: &Start) -> String {
    if occurrence.stamp == first.stamp {
        uid.to_string()
    } else {
        format!("{uid}#{}", occurrence.stamp.recurrence_id())
    }
}

/// The ref of the occurrence an override's RECURRENCE-ID names. With the
/// series' own event in the file, it is the ref the series gives that
/// occurrence — the bare UID for the first one, the RDATE's own for one
/// only an RDATE adds, and the same ref whatever form or zone the
/// RECURRENCE-ID is written in; without it, `<UID>#<RECURRENCE-ID>`. A
/// RECURRENCE-ID written as a DATE on a series of date-times names a day,
/// not one occurrence: its ref is `<UID>#<RECURRENCE-ID>` too.
fn override_ref(cal: &Calendar, expansion: &Expansion, uid: &str, rid: &Prop) -> String {
    let as_written = || format!("{uid}#{}", rid.value);
    let Some(replaced) = cal.start(rid) else {
        return as_written();
    };
    let Some(first) = expansion
        .masters
        .get(uid)
        .and_then(|master| cal.dtstart(master).ok())
    else {
        return as_written();
    };
    if replaced.is(&first) {
        return uid.to_string();
    }
    if !matches!(replaced.stamp, Stamp::Date(_)) {
        if let Some(listed) = expansion
            .rdate_only
            .get(uid)
            .and_then(|only| only.get(&replaced.instant()))
            .filter(|listed| listed.is(&replaced))
        {
            return occurrence_ref(uid, &first, listed);
        }
    }
    match first.express(&replaced) {
        Some(stamp) => occurrence_ref(
            uid,
            &first,
            &Start {
                stamp,
                zone: first.zone,
            },
        ),
        None => as_written(),
    }
}

/// The occurrences only an RDATE adds to a series, by instant: at each
/// instant an RDATE lists that neither DTSTART nor a rule makes, the first
/// RDATE listed there. `series_starts` keeps that start, under the RDATE's
/// own ref, at every import that reaches it (RDATEs still to come
/// included), so an override naming it can take the same ref.
fn rdate_only<'a>(
    cal: &'a Calendar,
    event: &Component,
    first: Start<'a>,
) -> HashMap<NaiveDateTime, Start<'a>> {
    let listed: Vec<Start<'a>> = cal
        .listed(event, "RDATE")
        .into_iter()
        .filter(|s| s.instant() != first.instant())
        .collect();
    let wanted: HashSet<NaiveDateTime> = listed.iter().map(Start::instant).collect();
    let Some(last) = wanted.iter().max().copied() else {
        return HashMap::new();
    };
    let instant = |wall: NaiveDateTime| rule_instant(first, wall);
    let mut by_rule = HashSet::new();
    for rule in event
        .all("RRULE")
        .iter()
        .filter_map(|prop| rrule::Rule::parse(&prop.value).ok())
    {
        // One walk per rule, up to a day past the last RDATE (the rule
        // walks wall-clock time; a day reaches it in any zone). Only the
        // instants it finds are needed: it keeps no start.
        let (_, _, found) = rule.after_start(
            first.stamp.wall(),
            last + Duration::days(1),
            0,
            &instant,
            &wanted,
        );
        by_rule.extend(found.into_keys());
    }
    let mut only = HashMap::new();
    for start in listed {
        if !by_rule.contains(&start.instant()) {
            only.entry(start.instant()).or_insert(start);
        }
    }
    only
}

/// The occurrences a series' own event imports, in order, each with its
/// ref: those no override took.
fn occurrences<'a>(
    cal: &'a Calendar,
    event: &Component,
    expansion: &Expansion<'a>,
    uid: &str,
    first: Start<'a>,
) -> Vec<(Start<'a>, String)> {
    let overrides = event.value("UID").and_then(|u| expansion.overrides.get(u));
    series_starts(cal, event, uid, first, expansion.horizon)
        .into_iter()
        .filter(|s| overrides.is_none_or(|o| !o.take(s)))
        .map(|s| (s, occurrence_ref(uid, &first, &s)))
        .collect()
}

/// The later occurrences a RANGE=THISANDFUTURE override carries: the
/// series' occurrences after `replaced` that follow this override (not
/// a later one, not a single override), each moved as the override moved
/// its own occurrence — by the same wall-clock difference, both read in
/// the override's zone — and under the ref the series gives it.
fn carried_on<'a>(
    cal: &'a Calendar,
    expansion: &Expansion<'a>,
    uid: &str,
    replaced: Start<'a>,
    start: Start<'a>,
) -> Vec<(Start<'a>, String)> {
    let (Some(master), Some(overrides)) =
        (expansion.masters.get(uid), expansion.overrides.get(uid))
    else {
        return Vec::new();
    };
    let Ok(first) = cal.dtstart(master) else {
        return Vec::new();
    };
    // The series' starts, the RECURRENCE-ID and the override's DTSTART
    // may each be written in another zone: the move is measured, and
    // made, in the wall clock of the override's.
    let shift = start.stamp.wall() - start.wall_of(&replaced);
    // An occurrence moved earlier may come from past the horizon.
    let reach = expansion.horizon - shift.min(Duration::zero());
    series_starts(cal, master, uid, first, reach)
        .into_iter()
        .filter(|s| s.at_or_after(&replaced) && !s.is(&replaced))
        .filter(|s| overrides.range_of(s) == Some(replaced.instant()))
        .filter(|s| !overrides.single.iter().any(|x| s.is(x)))
        .filter_map(|s| {
            let moved = Start {
                stamp: start.stamp.at(start.wall_of(&s) + shift),
                zone: start.zone,
            };
            (moved.exists() && moved.instant() <= expansion.horizon)
                .then(|| (moved, occurrence_ref(uid, &first, &s)))
        })
        .collect()
}

fn event_drafts<'a>(
    cal: &'a Calendar,
    event: &Component,
    expansion: &Expansion<'a>,
    scope: Option<&str>,
) -> Result<Vec<EpisodeDraft>> {
    // SUMMARY and LOCATION are one-line fields whoever sent the invite
    // wrote: an escaped `\n` must not start a line of its own (a chapter
    // lists titles one per line; a venue names a visit).
    let summary = event
        .value("SUMMARY")
        .map(|s| one_line(&unescape(s)))
        .unwrap_or_else(|| "(untitled event)".into());
    // What was cancelled didn't happen. (A cancelled override still takes
    // its occurrences from the series: see `Calendar::expansion`.)
    if cancelled(event) {
        return Ok(Vec::new());
    }
    let start = cal
        .dtstart(event)
        .with_context(|| format!("event {summary:?}"))?;
    // DTEND, as a length every occurrence shares.
    let length = event
        .first("DTEND")
        .and_then(|p| cal.start(p))
        .map(|end| end.instant() - start.instant());

    // Source-native identity: UID (+ RECURRENCE-ID for instances), with
    // DTSTART as a fallback for UID-less exports.
    let uid = event
        .value("UID")
        .or_else(|| event.value("DTSTART"))
        .unwrap_or_default()
        .to_string();
    let occurrences = match event.first("RECURRENCE-ID") {
        Some(rid) => {
            // Like the series' own, an override's occurrence is made once
            // it has begun; until then it still takes its occurrence from
            // the series (`Calendar::expansion`).
            let mut own = Vec::new();
            if start.instant() <= expansion.horizon {
                own.push((start, override_ref(cal, expansion, &uid, rid)));
            }
            if let Some(replaced) = cal.start(rid).filter(|_| this_and_future(rid)) {
                own.extend(carried_on(cal, expansion, &uid, replaced, start));
            }
            own
        }
        None => occurrences(cal, event, expansion, &uid, start),
    };

    let mut head = format!("event: {summary}\n");
    if let Some(location) = event.value("LOCATION").map(|l| one_line(&unescape(l))) {
        if !location.is_empty() {
            head.push_str(&format!("location: {location}\n"));
        }
    }

    // Entity 0: the Event itself, identity-anchored on the ICS UID.
    let mut entities = vec![EntityRef {
        entity_type: "Event".into(),
        name: summary.clone(),
        attributes: vec![("ics-uid".into(), uid)],
        role: "subject".into(),
    }];
    let mut edges = Vec::new();
    let mut tail = String::new();

    if let Some((email, name)) = event.first("ORGANIZER").and_then(parse_person) {
        tail.push_str(&format!("organizer: {name} <{email}>\n"));
        if !is_machine_address(&email) {
            entities.push(EntityRef {
                entity_type: "Person".into(),
                name,
                attributes: vec![("email".into(), email)],
                role: "organizer".into(),
            });
            edges.push(EdgeAssertion {
                src: entities.len() - 1,
                relation: "organized".into(),
                dst: 0,
            });
        }
    }
    let attendees: Vec<(String, String)> = event
        .all("ATTENDEE")
        .iter()
        .filter_map(parse_person)
        .collect();
    if !attendees.is_empty() {
        tail.push_str("attendees:\n");
    }
    for (email, name) in attendees {
        tail.push_str(&format!("  {name} <{email}>\n"));
        // Machine attendees stay in the body but never become entities.
        if is_machine_address(&email) {
            continue;
        }
        // The organizer often appears as an attendee too; the store's
        // attribute resolution collapses them into one Person.
        entities.push(EntityRef {
            entity_type: "Person".into(),
            name,
            attributes: vec![("email".into(), email)],
            role: "attendee".into(),
        });
        edges.push(EdgeAssertion {
            src: entities.len() - 1,
            relation: "attended".into(),
            dst: 0,
        });
    }
    if let Some(description) = event.value("DESCRIPTION").map(unescape) {
        if !description.is_empty() {
            tail.push_str(&format!("\n{description}\n"));
        }
    }

    Ok(occurrences
        .into_iter()
        .map(|(occurrence, source_ref)| {
            let event_time = occurrence.event_time();
            let mut body = head.clone();
            if let Some(utc) = &event_time.utc {
                body.push_str(&format!("start: {utc}\n"));
            }
            if let Some(length) = length {
                body.push_str(&format!(
                    "end: {}\n",
                    utc_text(occurrence.instant() + length)
                ));
            }
            body.push_str(&tail);
            EpisodeDraft {
                source: "ics".into(),
                source_ref,
                kind: "calendar_event".into(),
                title: summary.clone(),
                body,
                event_time,
                trust: "imported".into(),
                scope: scope.map(String::from),
                entities: entities.clone(),
                edges: edges.clone(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:evt-123@example.com\r\n\
SUMMARY:Roadmap review with the \r\n\
\x20platform team\r\n\
DTSTART;TZID=America/New_York:20260315T140000\r\n\
DTEND;TZID=America/New_York:20260315T150000\r\n\
LOCATION:Zoom\r\n\
DESCRIPTION:Quarterly roadmap\\nBring the deck\r\n\
ORGANIZER;CN=Jon Hale:mailto:jon@harbor.example\r\n\
ATTENDEE;CN=Jane Doe;PARTSTAT=ACCEPTED:mailto:jane@example.com\r\n\
ATTENDEE:mailto:sam@example.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:allday-1\r\n\
SUMMARY:Company holiday\r\n\
DTSTART;VALUE=DATE:20260704\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    /// An import time far enough out that no test's series is cut by it
    /// unless it means to be.
    fn horizon() -> NaiveDateTime {
        at("20300101T000000")
    }

    fn at(wall: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(wall, "%Y%m%dT%H%M%S").unwrap()
    }

    fn all(raw: &str) -> Vec<EpisodeDraft> {
        drafts(raw, None, horizon(), "test.ics").unwrap()
    }

    /// The drafts of the calendar's `index`th event.
    fn event_to_draft(raw: &str, index: usize, scope: Option<&str>) -> Vec<EpisodeDraft> {
        let cal = parse_calendar(raw);
        let expansion = cal.expansion(horizon());
        event_drafts(&cal, &cal.events[index], &expansion, scope).unwrap()
    }

    /// Each draft's ref with its start (UTC) and title, by ref.
    fn by_ref(drafts: &[EpisodeDraft]) -> std::collections::BTreeMap<&str, (&str, &str)> {
        drafts
            .iter()
            .map(|d| {
                (
                    d.source_ref.as_str(),
                    (d.event_time.utc.as_deref().unwrap_or(""), d.title.as_str()),
                )
            })
            .collect()
    }

    fn one(raw: &str, index: usize, scope: Option<&str>) -> EpisodeDraft {
        let mut drafts = event_to_draft(raw, index, scope);
        assert_eq!(drafts.len(), 1);
        drafts.remove(0)
    }

    fn time_of(prop_line: &str) -> EventTime {
        let cal = parse_calendar("");
        let (_, prop) = parse_prop_line(prop_line).unwrap();
        cal.start(&prop).unwrap().event_time()
    }

    #[test]
    fn test_parse_sample() {
        let events = parse_calendar(SAMPLE).events;
        assert_eq!(events.len(), 2);

        let draft = one(SAMPLE, 0, Some("work"));
        // Folded SUMMARY line reassembled.
        assert_eq!(draft.title, "Roadmap review with the platform team");
        assert_eq!(draft.source_ref, "evt-123@example.com");
        assert_eq!(draft.kind, "calendar_event");
        assert_eq!(draft.trust, "imported");
        // 14:00 EDT (-0400) = 18:00 UTC.
        assert_eq!(
            draft.event_time.utc.as_deref(),
            Some("2026-03-15T18:00:00Z")
        );
        assert_eq!(draft.event_time.tz_offset_minutes, Some(-240));
        assert_eq!(draft.event_time.uncertainty, TimeUncertainty::Exact);
        assert!(draft.body.contains("start: 2026-03-15T18:00:00Z\n"));
        assert!(draft.body.contains("end: 2026-03-15T19:00:00Z\n"));
        // Unescaped description in the body.
        assert!(draft.body.contains("Quarterly roadmap\nBring the deck"));

        // Event + organizer + 2 attendees.
        assert_eq!(draft.entities.len(), 4);
        assert_eq!(draft.entities[0].entity_type, "Event");
        assert_eq!(
            draft.entities[0].attributes[0],
            ("ics-uid".to_string(), "evt-123@example.com".to_string())
        );
        let jane = &draft.entities[2];
        assert_eq!(jane.name, "Jane Doe");
        assert_eq!(
            jane.attributes[0],
            ("email".to_string(), "jane@example.com".to_string())
        );
        // Attendee without CN falls back to the email local part.
        assert_eq!(draft.entities[3].name, "sam");
        assert_eq!(draft.edges.len(), 3);
    }

    #[test]
    fn test_machine_addresses_never_become_entities() {
        // base32 token@google.com, a group calendar, and a resource — all
        // machine; jane@example.com is a real person.
        const M: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:m-1\r\n\
SUMMARY:Main Street Dental\r\n\
DTSTART;VALUE=DATE:20260210\r\n\
ORGANIZER:mailto:q3ffz8m2tu9xk4wq7npl1a0bcd@google.com\r\n\
ATTENDEE;CN=Jane Doe:mailto:jane@example.com\r\n\
ATTENDEE:mailto:abcdefghij1234567890xy@group.calendar.google.com\r\n\
ATTENDEE:mailto:room-7@resource.calendar.google.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let draft = one(M, 0, Some("personal"));
        // Event + Jane only — the three machine addresses are filtered.
        assert_eq!(draft.entities.len(), 2);
        assert_eq!(draft.entities[0].entity_type, "Event");
        assert_eq!(draft.entities[1].name, "Jane Doe");
        // But they DO stay in the body (raw content preserved).
        assert!(draft.body.contains("q3ffz8m2tu9xk4wq7npl1a0bcd@google.com"));
        assert!(draft.body.contains("room-7@resource.calendar.google.com"));
        // Only Jane's "attended" edge remains.
        assert_eq!(draft.edges.len(), 1);

        // Unit-level: real Googlers are kept.
        assert!(is_machine_address("q3ffz8m2tu9xk4wq7npl1a0bcd@google.com"));
        assert!(is_machine_address("x@group.calendar.google.com"));
        assert!(!is_machine_address("jane.doe@google.com"));
        assert!(!is_machine_address("jsmith@google.com"));
        assert!(!is_machine_address("jane@example.com"));
    }

    /// SUMMARY and LOCATION stay on one line even when the sender put an
    /// escaped line break in them; DESCRIPTION keeps its lines.
    #[test]
    fn test_summary_and_location_cannot_add_lines() {
        const FORGED: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:f-1\r\n\
SUMMARY:Lunch\\n- Wire the money\\N\r\n\
LOCATION:Cafe\\n[12:00] Elsewhere (40.71,-74.00)\r\n\
DTSTART:20260315T120000Z\r\n\
DESCRIPTION:line one\\nline two\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let draft = one(FORGED, 0, None);
        assert_eq!(draft.title, "Lunch - Wire the money");
        assert_eq!(draft.entities[0].name, "Lunch - Wire the money");
        assert!(
            draft.body.contains("event: Lunch - Wire the money\n"),
            "{}",
            draft.body
        );
        assert!(
            draft
                .body
                .contains("location: Cafe [12:00] Elsewhere (40.71,-74.00)\n"),
            "{}",
            draft.body
        );
        assert!(
            !draft
                .body
                .lines()
                .any(|l| l.starts_with('[') || l.starts_with("- ")),
            "{}",
            draft.body
        );
        assert!(draft.body.contains("line one\nline two"));
    }

    #[test]
    fn test_all_day_event_day_uncertainty() {
        let draft = one(SAMPLE, 1, Some("personal"));
        assert_eq!(draft.event_time.uncertainty, TimeUncertainty::Day);
        assert_eq!(
            draft.event_time.utc.as_deref(),
            Some("2026-07-04T00:00:00Z")
        );
        assert_eq!(draft.event_time.tz_offset_minutes, None);
    }

    #[test]
    fn test_quoted_param_with_colon() {
        let (name, prop) =
            parse_prop_line("ORGANIZER;CN=\"Doe: Jane\":mailto:jane@x.example").unwrap();
        assert_eq!(name, "ORGANIZER");
        assert_eq!(prop.params.get("CN").unwrap(), "Doe: Jane");
        assert_eq!(prop.value, "mailto:jane@x.example");
    }

    #[test]
    fn test_floating_time_marked_uncertain() {
        let t = time_of("DTSTART:20260315T140000");
        assert_eq!(t.uncertainty, TimeUncertainty::Hour);
        assert_eq!(t.tz_offset_minutes, None);
    }

    /// An alarm's ATTENDEE and DESCRIPTION belong to the alarm: the event
    /// gains no attendee and no description from it.
    #[test]
    fn test_nested_alarm_properties_stay_out_of_the_event() {
        const ALARM: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:a-1\r\n\
SUMMARY:Dentist\r\n\
DTSTART:20260315T120000Z\r\n\
BEGIN:VALARM\r\n\
ACTION:EMAIL\r\n\
TRIGGER:-PT30M\r\n\
SUMMARY:Alarm notification\r\n\
DESCRIPTION:This is an event reminder\r\n\
ATTENDEE:mailto:owner@example.com\r\n\
END:VALARM\r\n\
ATTENDEE;CN=Jane Doe:mailto:jane@example.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let draft = one(ALARM, 0, None);
        assert_eq!(draft.title, "Dentist");
        assert!(!draft.body.contains("event reminder"), "{}", draft.body);
        assert!(!draft.body.contains("owner@example.com"), "{}", draft.body);
        // Event + Jane, and only Jane's edge.
        let names: Vec<&str> = draft.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Dentist", "Jane Doe"]);
        assert_eq!(draft.edges.len(), 1);
    }

    /// A Windows zone name, a zone only the file defines, and a time a
    /// daylight-saving change skips all resolve; an unknown zone reads as
    /// floating; an event that can't be read is left out and the rest of
    /// the file imports.
    #[test]
    fn test_unusual_events_resolve_or_are_left_out_alone() {
        const MIXED: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Customized Time Zone\r\n\
BEGIN:STANDARD\r\n\
DTSTART:16011104T020000\r\n\
RRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11\r\n\
TZOFFSETFROM:-0400\r\n\
TZOFFSETTO:-0500\r\n\
END:STANDARD\r\n\
BEGIN:DAYLIGHT\r\n\
DTSTART:16010311T020000\r\n\
RRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=3\r\n\
TZOFFSETFROM:-0500\r\n\
TZOFFSETTO:-0400\r\n\
END:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:win-1\r\n\
SUMMARY:Outlook meeting\r\n\
DTSTART;TZID=\"Eastern Standard Time\":20260315T140000\r\n\
DTEND;TZID=\"Eastern Standard Time\":20260315T150000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:gap-1\r\n\
SUMMARY:Early flight\r\n\
DTSTART;TZID=America/New_York:20260308T023000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:custom-summer\r\n\
SUMMARY:Custom zone, summer\r\n\
DTSTART;TZID=Customized Time Zone:20260701T090000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:custom-winter\r\n\
SUMMARY:Custom zone, winter\r\n\
DTSTART;TZID=Customized Time Zone:20260115T090000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:custom-gap\r\n\
SUMMARY:Custom zone, skipped hour\r\n\
DTSTART;TZID=Customized Time Zone:20260308T023000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:nowhere-1\r\n\
SUMMARY:Unknown zone\r\n\
DTSTART;TZID=Atlantis Standard Time:20260315T140000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:broken-1\r\n\
SUMMARY:Broken start\r\n\
DTSTART:2026-03-15 14:00\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:nostart-1\r\n\
SUMMARY:No start\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(MIXED);
        let by_ref = |r: &str| {
            drafts
                .iter()
                .find(|d| d.source_ref == r)
                .unwrap_or_else(|| panic!("no draft {r}"))
        };
        assert_eq!(drafts.len(), 6, "the two unreadable events are left out");

        let win = by_ref("win-1");
        assert_eq!(win.event_time.utc.as_deref(), Some("2026-03-15T18:00:00Z"));
        assert_eq!(win.event_time.tz_offset_minutes, Some(-240));
        assert_eq!(win.event_time.uncertainty, TimeUncertainty::Exact);
        assert!(win.body.contains("end: 2026-03-15T19:00:00Z\n"));

        // 02:30 doesn't exist on 8 March 2026 in New York: read with the
        // offset before the change (-05:00), which lands at 03:30 EDT.
        let gap = by_ref("gap-1");
        assert_eq!(gap.event_time.utc.as_deref(), Some("2026-03-08T07:30:00Z"));
        assert_eq!(gap.event_time.tz_offset_minutes, Some(-240));

        let summer = by_ref("custom-summer");
        assert_eq!(
            summer.event_time.utc.as_deref(),
            Some("2026-07-01T13:00:00Z")
        );
        assert_eq!(summer.event_time.tz_offset_minutes, Some(-240));
        assert_eq!(summer.event_time.uncertainty, TimeUncertainty::Exact);
        let winter = by_ref("custom-winter");
        assert_eq!(
            winter.event_time.utc.as_deref(),
            Some("2026-01-15T14:00:00Z")
        );
        assert_eq!(winter.event_time.tz_offset_minutes, Some(-300));
        let custom_gap = by_ref("custom-gap");
        assert_eq!(
            custom_gap.event_time.utc.as_deref(),
            Some("2026-03-08T07:30:00Z")
        );

        let nowhere = by_ref("nowhere-1");
        assert_eq!(nowhere.event_time.uncertainty, TimeUncertainty::Hour);
        assert_eq!(nowhere.event_time.tz_offset_minutes, None);
        assert_eq!(
            nowhere.event_time.utc.as_deref(),
            Some("2026-03-15T14:00:00Z")
        );

        // A file none of whose events can be read is still an error.
        const NONE: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:broken-1\r\n\
DTSTART:yesterday\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        assert!(drafts_err(NONE).contains("none of the 1 events"));
    }

    fn drafts_err(raw: &str) -> String {
        format!(
            "{:#}",
            drafts(raw, None, horizon(), "test.ics").unwrap_err()
        )
    }

    /// The repeated hour when clocks go back reads as its first
    /// occurrence (daylight time).
    #[test]
    fn test_repeated_hour_reads_as_first_occurrence() {
        let t = time_of("DTSTART;TZID=America/New_York:20261101T013000");
        assert_eq!(t.utc.as_deref(), Some("2026-11-01T05:30:00Z"));
        assert_eq!(t.tz_offset_minutes, Some(-240));
    }

    /// A weekly series across the March daylight-saving change: every
    /// occurrence stays at 14:00 New York time, an EXDATE removes one, an
    /// override replaces one, and refs follow RECURRENCE-ID's form.
    #[test]
    fn test_recurring_series_expands_across_dst() {
        const SERIES: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:weekly-1\r\n\
SUMMARY:Team sync\r\n\
DTSTART;TZID=America/New_York:20260223T140000\r\n\
DTEND;TZID=America/New_York:20260223T150000\r\n\
RRULE:FREQ=WEEKLY;COUNT=6\r\n\
EXDATE;TZID=America/New_York:20260316T140000\r\n\
ATTENDEE;CN=Jane Doe:mailto:jane@example.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:weekly-1\r\n\
RECURRENCE-ID;TZID=America/New_York:20260309T140000\r\n\
SUMMARY:Team sync (moved)\r\n\
DTSTART;TZID=America/New_York:20260310T100000\r\n\
DTEND;TZID=America/New_York:20260310T110000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(SERIES);
        let refs: Vec<(&str, &str)> = drafts
            .iter()
            .map(|d| {
                (
                    d.source_ref.as_str(),
                    d.event_time.utc.as_deref().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            refs,
            [
                ("weekly-1", "2026-02-23T19:00:00Z"),
                ("weekly-1#20260302T140000", "2026-03-02T19:00:00Z"),
                // 9 March comes from the override; 16 March is excluded.
                ("weekly-1#20260323T140000", "2026-03-23T18:00:00Z"),
                ("weekly-1#20260330T140000", "2026-03-30T18:00:00Z"),
                ("weekly-1#20260309T140000", "2026-03-10T14:00:00Z"),
            ]
        );
        let after_change = &drafts[2];
        assert_eq!(after_change.event_time.tz_offset_minutes, Some(-240));
        assert!(after_change.body.contains("end: 2026-03-23T19:00:00Z\n"));
        assert_eq!(after_change.title, "Team sync");
        // Every occurrence carries the series' Event and attendee.
        assert_eq!(after_change.entities[0].attributes[0].1, "weekly-1");
        assert_eq!(after_change.entities[1].name, "Jane Doe");
        assert_eq!(drafts[4].title, "Team sync (moved)");
    }

    /// A series without an end stops at the horizon; an all-day yearly
    /// series honours a DATE UNTIL; RDATE adds a day; an unexpandable
    /// rule keeps the first occurrence.
    #[test]
    fn test_recurrence_bounds() {
        const OPEN: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:open-1\r\n\
SUMMARY:Standup\r\n\
DTSTART:20291220T150000Z\r\n\
RRULE:FREQ=DAILY\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:bday-1\r\n\
SUMMARY:Birthday\r\n\
DTSTART;VALUE=DATE:20240229\r\n\
RRULE:FREQ=YEARLY;UNTIL=20290301\r\n\
RDATE;VALUE=DATE:20250301\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:hourly-1\r\n\
SUMMARY:Hourly\r\n\
DTSTART:20260101T080000Z\r\n\
RRULE:FREQ=HOURLY;COUNT=5\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(OPEN);
        let refs = |prefix: &str| -> Vec<String> {
            drafts
                .iter()
                .filter(|d| d.source_ref.starts_with(prefix))
                .map(|d| d.source_ref.clone())
                .collect()
        };
        // 20 Dec 2029 through 31 Dec 2029: the horizon is 1 Jan 2030.
        let open = refs("open-1");
        assert_eq!(open.len(), 12, "{open:?}");
        assert_eq!(open[0], "open-1");
        assert_eq!(open[1], "open-1#20291221T150000Z");
        assert_eq!(open[11], "open-1#20291231T150000Z");
        // 29 February only recurs in leap years; RDATE adds 1 March 2025.
        assert_eq!(
            refs("bday-1"),
            ["bday-1", "bday-1#20250301", "bday-1#20280229"]
        );
        let leap = drafts
            .iter()
            .find(|d| d.source_ref == "bday-1#20280229")
            .unwrap();
        assert_eq!(leap.event_time.uncertainty, TimeUncertainty::Day);
        assert_eq!(leap.event_time.utc.as_deref(), Some("2028-02-29T00:00:00Z"));
        assert_eq!(refs("hourly-1"), ["hourly-1"]);
    }

    /// Imported twice into a store: the first import makes only the
    /// occurrences that had begun — not an override's that is still to
    /// come, nor a series' first that is — so the second one's moves and
    /// cancellation of later occurrences are what the record keeps.
    #[test]
    fn test_a_later_import_corrects_occurrences_still_to_come() {
        const FIRST: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:s-1\r\n\
SUMMARY:Review\r\n\
DTSTART:20260302T090000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=6\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:s-1\r\n\
RECURRENCE-ID:20260316T090000Z\r\n\
SUMMARY:Review (moved)\r\n\
DTSTART:20260317T100000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:f-1\r\n\
SUMMARY:Kickoff\r\n\
DTSTART:20260320T090000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=2\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        const SECOND: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:s-1\r\n\
SUMMARY:Review\r\n\
DTSTART:20260302T090000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=6\r\n\
EXDATE:20260323T090000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:s-1\r\n\
RECURRENCE-ID:20260316T090000Z\r\n\
SUMMARY:Review (moved again)\r\n\
DTSTART:20260318T100000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:f-1\r\n\
SUMMARY:Kickoff\r\n\
DTSTART:20260321T090000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=2\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path());
        let store = hearthloom_core::Store::open_in_memory().unwrap();
        let import = |raw: &str, imported_at: &str| {
            let episodes = drafts(raw, None, at(imported_at), "test.ics").unwrap();
            store
                .apply_batch(
                    &home,
                    &hearthloom_core::IngestBatch {
                        actor: "adapter:ics".into(),
                        session_id: None,
                        episodes,
                    },
                )
                .unwrap();
        };
        // Between the second and third occurrences, before f-1 began.
        assert_eq!(
            drafts(FIRST, None, at("20260310T000000"), "test.ics")
                .unwrap()
                .len(),
            2
        );
        import(FIRST, "20260310T000000");
        // After the series ended.
        import(SECOND, "20260501T000000");

        let stored = |source_ref: &str| {
            store
                .episode(&hearthloom_core::ids::episode_id("ics", source_ref))
                .unwrap()
                .map(|e| (e.title, e.event_time.unwrap_or_default()))
        };
        let review = |t: &str| Some(("Review".to_string(), t.to_string()));
        assert_eq!(stored("s-1"), review("2026-03-02T09:00:00Z"));
        assert_eq!(
            stored("s-1#20260309T090000Z"),
            review("2026-03-09T09:00:00Z")
        );
        assert_eq!(
            stored("s-1#20260316T090000Z"),
            Some((
                "Review (moved again)".to_string(),
                "2026-03-18T10:00:00Z".to_string()
            ))
        );
        let kickoff = |t: &str| Some(("Kickoff".to_string(), t.to_string()));
        assert_eq!(stored("f-1"), kickoff("2026-03-21T09:00:00Z"));
        assert_eq!(
            stored("f-1#20260328T090000Z"),
            kickoff("2026-03-28T09:00:00Z")
        );
        assert_eq!(stored("s-1#20260323T090000Z"), None);
        assert_eq!(
            stored("s-1#20260330T090000Z"),
            review("2026-03-30T09:00:00Z")
        );
        assert_eq!(
            stored("s-1#20260406T090000Z"),
            review("2026-04-06T09:00:00Z")
        );

        // A file read now: a series running from two weeks ago into the
        // next two makes the occurrences up to now and none after.
        let begun = (Utc::now() - Duration::days(14)).format("%Y%m%dT%H%M%SZ");
        let file = tmp.path().join("daily.ics");
        std::fs::write(
            &file,
            format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:d-1\r\nSUMMARY:Walk\r\n\
                 DTSTART:{begun}\r\nRRULE:FREQ=DAILY;COUNT=28\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            ),
        )
        .unwrap();
        let now = utc_text(Utc::now().naive_utc());
        let made = ingest(&file, None).unwrap();
        assert_eq!(made.len(), 15, "today's walk and the 14 before it");
        assert!(made.iter().all(|d| d
            .event_time
            .utc
            .as_deref()
            .is_some_and(|t| t <= now.as_str())));
    }

    /// A cancelled event, series or occurrence becomes no episode; a
    /// cancelled override still takes its occurrence from the series.
    #[test]
    fn test_cancelled_events_become_no_episode() {
        const CANCELLED: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:c-1\r\n\
SUMMARY:Standup\r\n\
DTSTART:20260302T090000Z\r\n\
RRULE:FREQ=DAILY;COUNT=3\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:c-1\r\n\
RECURRENCE-ID:20260303T090000Z\r\n\
SUMMARY:Standup\r\n\
DTSTART:20260303T090000Z\r\n\
STATUS:CANCELLED\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:c-2\r\n\
SUMMARY:Offsite\r\n\
DTSTART:20260310T090000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=3\r\n\
STATUS:CANCELLED\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:c-3\r\n\
SUMMARY:Lunch\r\n\
DTSTART:20260311T120000Z\r\n\
STATUS:Cancelled\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:c-4\r\n\
SUMMARY:Retro\r\n\
DTSTART:20260312T150000Z\r\n\
STATUS:CONFIRMED\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(CANCELLED);
        let refs: Vec<&str> = drafts.iter().map(|d| d.source_ref.as_str()).collect();
        assert_eq!(refs, ["c-1", "c-1#20260304T090000Z", "c-4"]);
    }

    /// RANGE=THISANDFUTURE: the override moves its own occurrence and
    /// every later one the same way (in local time, across the DST
    /// change), until a single override takes one.
    #[test]
    fn test_this_and_future_override_carries_on() {
        const RANGE: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:r-1\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
DTEND;TZID=America/New_York:20260302T093000\r\n\
RRULE:FREQ=WEEKLY;COUNT=5\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:r-1\r\n\
RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=America/New_York:20260309T090000\r\n\
SUMMARY:Sync (later)\r\n\
DTSTART;TZID=America/New_York:20260309T100000\r\n\
DTEND;TZID=America/New_York:20260309T110000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:r-1\r\n\
RECURRENCE-ID;TZID=America/New_York:20260323T090000\r\n\
SUMMARY:Sync (moved)\r\n\
DTSTART;TZID=America/New_York:20260324T120000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(RANGE);
        assert_eq!(
            by_ref(&drafts),
            [
                ("r-1", ("2026-03-02T14:00:00Z", "Sync")),
                (
                    "r-1#20260309T090000",
                    ("2026-03-09T14:00:00Z", "Sync (later)")
                ),
                (
                    "r-1#20260316T090000",
                    ("2026-03-16T14:00:00Z", "Sync (later)")
                ),
                (
                    "r-1#20260323T090000",
                    ("2026-03-24T16:00:00Z", "Sync (moved)")
                ),
                (
                    "r-1#20260330T090000",
                    ("2026-03-30T14:00:00Z", "Sync (later)")
                ),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );
        assert_eq!(drafts.len(), 5);
        // The carried occurrences keep the override's length.
        let carried = drafts
            .iter()
            .find(|d| d.source_ref == "r-1#20260316T090000")
            .unwrap();
        assert!(
            carried.body.contains("end: 2026-03-16T15:00:00Z\n"),
            "{}",
            carried.body
        );

        // Cancelled from an occurrence on: the series ends there.
        const ENDED: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:r-2\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
RRULE:FREQ=WEEKLY;COUNT=5\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:r-2\r\n\
RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=America/New_York:20260316T090000\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260316T090000\r\n\
STATUS:CANCELLED\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let refs: Vec<String> = all(ENDED).into_iter().map(|d| d.source_ref).collect();
        assert_eq!(refs, ["r-2", "r-2#20260309T090000"]);
    }

    /// An override's ref is the one the series gives the occurrence it
    /// names: the bare UID for the first, and the same ref when its
    /// RECURRENCE-ID is written in another zone than DTSTART. Without the
    /// series' own event in the file, it is the RECURRENCE-ID as written.
    #[test]
    fn test_override_refs_follow_the_series() {
        const SERIES: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:o-1\r\n\
SUMMARY:Standup\r\n\
DTSTART:20260302T090000Z\r\n\
RRULE:FREQ=DAILY;COUNT=3\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:o-1\r\n\
RECURRENCE-ID:20260302T090000Z\r\n\
SUMMARY:Standup (first, later)\r\n\
DTSTART:20260302T100000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:o-1\r\n\
RECURRENCE-ID;TZID=Europe/London:20260303T090000\r\n\
SUMMARY:Standup (second, later)\r\n\
DTSTART:20260303T110000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(SERIES);
        assert_eq!(drafts.len(), 3);
        assert_eq!(
            by_ref(&drafts),
            [
                ("o-1", ("2026-03-02T10:00:00Z", "Standup (first, later)")),
                (
                    "o-1#20260303T090000Z",
                    ("2026-03-03T11:00:00Z", "Standup (second, later)")
                ),
                ("o-1#20260304T090000Z", ("2026-03-04T09:00:00Z", "Standup")),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );

        const ALONE: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:o-2\r\n\
RECURRENCE-ID:20260302T090000Z\r\n\
SUMMARY:Standup (moved)\r\n\
DTSTART:20260302T100000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let refs: Vec<String> = all(ALONE).into_iter().map(|d| d.source_ref).collect();
        assert_eq!(refs, ["o-2#20260302T090000Z"]);
    }

    /// An occurrence an RDATE adds to a zoned series, written in UTC or in
    /// another zone, keeps its ref when an override names it, however the
    /// RECURRENCE-ID is written: the ref an import before the override
    /// stored it under. One the rule makes as well keeps the rule's ref.
    #[test]
    fn test_override_of_an_rdate_keeps_its_ref() {
        const SERIES: &str = "BEGIN:VEVENT\r\n\
UID:rd-1\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
RRULE:FREQ=WEEKLY;COUNT=3\r\n\
RDATE:20260304T200000Z\r\n\
RDATE;TZID=Europe/London:20260305T160000\r\n\
RDATE:20260309T130000Z\r\n\
END:VEVENT\r\n";
        const OVERRIDES: &str = "BEGIN:VEVENT\r\n\
UID:rd-1\r\n\
RECURRENCE-ID;TZID=America/New_York:20260304T150000\r\n\
SUMMARY:Sync (moved)\r\n\
DTSTART;TZID=America/New_York:20260304T170000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:rd-1\r\n\
RECURRENCE-ID:20260305T160000Z\r\n\
SUMMARY:Sync (moved)\r\n\
DTSTART:20260305T180000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:rd-1\r\n\
RECURRENCE-ID:20260309T130000Z\r\n\
SUMMARY:Sync (moved)\r\n\
DTSTART:20260309T150000Z\r\n\
END:VEVENT\r\n";
        let calendar = |events: &str| format!("BEGIN:VCALENDAR\r\n{events}END:VCALENDAR\r\n");
        let before = all(&calendar(SERIES));
        assert_eq!(
            by_ref(&before),
            [
                ("rd-1", ("2026-03-02T14:00:00Z", "Sync")),
                ("rd-1#20260304T200000Z", ("2026-03-04T20:00:00Z", "Sync")),
                ("rd-1#20260305T160000", ("2026-03-05T16:00:00Z", "Sync")),
                ("rd-1#20260309T090000", ("2026-03-09T13:00:00Z", "Sync")),
                ("rd-1#20260316T090000", ("2026-03-16T13:00:00Z", "Sync")),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );
        let after = all(&calendar(&format!("{SERIES}{OVERRIDES}")));
        assert_eq!(
            by_ref(&after),
            [
                ("rd-1", ("2026-03-02T14:00:00Z", "Sync")),
                (
                    "rd-1#20260304T200000Z",
                    ("2026-03-04T22:00:00Z", "Sync (moved)")
                ),
                (
                    "rd-1#20260305T160000",
                    ("2026-03-05T18:00:00Z", "Sync (moved)")
                ),
                (
                    "rd-1#20260309T090000",
                    ("2026-03-09T15:00:00Z", "Sync (moved)")
                ),
                ("rd-1#20260316T090000", ("2026-03-16T13:00:00Z", "Sync")),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );
        assert_eq!(after.len(), 5);
    }

    /// A RECURRENCE-ID written as a DATE names a day of a series of
    /// date-times, and takes every occurrence that day: it keeps the ref
    /// it is written with, also when an RDATE adds an occurrence that day.
    #[test]
    fn test_a_date_recurrence_id_keeps_its_ref_next_to_an_rdate() {
        const CAL: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:dd-1\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
RRULE:FREQ=DAILY;COUNT=5\r\n\
RDATE:20260304T200000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:dd-1\r\n\
RECURRENCE-ID;VALUE=DATE:20260304\r\n\
SUMMARY:Sync (moved)\r\n\
DTSTART;TZID=America/New_York:20260304T110000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        assert_eq!(
            by_ref(&all(CAL)),
            [
                ("dd-1", ("2026-03-02T14:00:00Z", "Sync")),
                ("dd-1#20260303T090000", ("2026-03-03T14:00:00Z", "Sync")),
                ("dd-1#20260304", ("2026-03-04T16:00:00Z", "Sync (moved)")),
                ("dd-1#20260305T090000", ("2026-03-05T14:00:00Z", "Sync")),
                ("dd-1#20260306T090000", ("2026-03-06T14:00:00Z", "Sync")),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );
    }

    /// An RDATE at an instant the rule makes is the rule's occurrence. It
    /// keeps the rule's ref in an import made after the series has left
    /// that start out (a series keeps its latest `MAX_OCCURRENCES`), and an
    /// override naming it takes the same ref.
    #[test]
    fn test_an_rdate_the_rule_makes_keeps_its_ref_as_the_series_moves_on() {
        const SERIES: &str = "BEGIN:VEVENT\r\n\
UID:long-1\r\n\
SUMMARY:Daily\r\n\
DTSTART;TZID=America/New_York:20100104T090000\r\n\
RRULE:FREQ=DAILY\r\n\
RDATE:20120601T130000Z\r\n\
END:VEVENT\r\n";
        const OVERRIDE: &str = "BEGIN:VEVENT\r\n\
UID:long-1\r\n\
RECURRENCE-ID:20120601T130000Z\r\n\
SUMMARY:Daily (moved)\r\n\
DTSTART:20120601T150000Z\r\n\
END:VEVENT\r\n";
        // The drafts of 1 June 2012, as (ref, title).
        let on_the_day = |events: &str, imported: &str| -> Vec<(String, String)> {
            let raw = format!("BEGIN:VCALENDAR\r\n{events}END:VCALENDAR\r\n");
            drafts(&raw, None, at(imported), "test.ics")
                .unwrap()
                .into_iter()
                .filter(|d| {
                    d.event_time
                        .utc
                        .as_deref()
                        .is_some_and(|t| t.starts_with("2012-06-01"))
                })
                .map(|d| (d.source_ref, d.title))
                .collect()
        };
        let series = vec![("long-1#20120601T090000".to_string(), "Daily".to_string())];
        // The next year the series still keeps the rule's start there; in
        // 2026 it keeps those from 2016 on.
        assert_eq!(on_the_day(SERIES, "20130101T000000"), series);
        assert_eq!(on_the_day(SERIES, "20260101T000000"), series);
        assert_eq!(
            on_the_day(&format!("{SERIES}{OVERRIDE}"), "20260101T000000"),
            [(
                "long-1#20120601T090000".to_string(),
                "Daily (moved)".to_string()
            )]
        );
    }

    /// However many overrides name an occurrence an RDATE adds, an import
    /// walks the series' rule twice: once for the series, once for the
    /// occurrences only its RDATEs add.
    #[test]
    fn test_overrides_of_an_rdate_add_no_rule_walks() {
        let mut ics = String::from(
            "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:many-1\r\n\
SUMMARY:Daily\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
RRULE:FREQ=DAILY;COUNT=10\r\n\
RDATE:20260304T200000Z\r\n\
END:VEVENT\r\n",
        );
        for i in 0..50 {
            ics.push_str(&format!(
                "BEGIN:VEVENT\r\n\
UID:many-1\r\n\
RECURRENCE-ID:20260304T200000Z\r\n\
SUMMARY:Moved {i}\r\n\
DTSTART:20260304T210000Z\r\n\
END:VEVENT\r\n"
            ));
        }
        ics.push_str("END:VCALENDAR\r\n");
        let before = rrule::WALKS.with(std::cell::Cell::get);
        let drafts = all(&ics);
        assert_eq!(rrule::WALKS.with(std::cell::Cell::get) - before, 2);
        let moved: Vec<&EpisodeDraft> = drafts
            .iter()
            .filter(|d| d.title.starts_with("Moved"))
            .collect();
        assert_eq!(moved.len(), 50);
        assert!(moved
            .iter()
            .all(|d| d.source_ref == "many-1#20260304T200000Z"));
    }

    /// A RANGE=THISANDFUTURE override naming an occurrence only an RDATE
    /// adds takes that RDATE's ref, and the later occurrences it carries
    /// keep the refs the series gives them.
    #[test]
    fn test_range_override_of_an_rdate_keeps_the_refs() {
        const CAL: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:rr-1\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
RRULE:FREQ=WEEKLY;COUNT=3\r\n\
RDATE:20260304T200000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:rr-1\r\n\
RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=America/New_York:20260304T150000\r\n\
SUMMARY:Sync (later)\r\n\
DTSTART;TZID=America/New_York:20260304T160000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        assert_eq!(
            by_ref(&all(CAL)),
            [
                ("rr-1", ("2026-03-02T14:00:00Z", "Sync")),
                (
                    "rr-1#20260304T200000Z",
                    ("2026-03-04T21:00:00Z", "Sync (later)")
                ),
                (
                    "rr-1#20260309T090000",
                    ("2026-03-09T14:00:00Z", "Sync (later)")
                ),
                (
                    "rr-1#20260316T090000",
                    ("2026-03-16T14:00:00Z", "Sync (later)")
                ),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );
    }

    /// On a series written in UTC, or in floating time, an RDATE written
    /// in a zone keeps its ref when an override names it.
    #[test]
    fn test_override_of_a_zoned_rdate_on_a_utc_or_floating_series() {
        const SERIES: &str = "BEGIN:VEVENT\r\n\
UID:u-1\r\n\
SUMMARY:Utc\r\n\
DTSTART:20260302T140000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=2\r\n\
RDATE;TZID=America/New_York:20260304T150000\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:f-1\r\n\
SUMMARY:Floating\r\n\
DTSTART:20260302T090000\r\n\
RRULE:FREQ=WEEKLY;COUNT=2\r\n\
RDATE;TZID=America/New_York:20260304T150000\r\n\
END:VEVENT\r\n";
        const OVERRIDES: &str = "BEGIN:VEVENT\r\n\
UID:u-1\r\n\
RECURRENCE-ID:20260304T200000Z\r\n\
SUMMARY:Utc (moved)\r\n\
DTSTART:20260304T210000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:f-1\r\n\
RECURRENCE-ID;TZID=America/New_York:20260304T150000\r\n\
SUMMARY:Floating (moved)\r\n\
DTSTART;TZID=America/New_York:20260304T160000\r\n\
END:VEVENT\r\n";
        let calendar = |events: &str| format!("BEGIN:VCALENDAR\r\n{events}END:VCALENDAR\r\n");
        let refs = |events: &str| -> Vec<String> {
            let mut refs: Vec<String> = all(&calendar(events))
                .into_iter()
                .map(|d| d.source_ref)
                .collect();
            refs.sort();
            refs
        };
        let expected = [
            "f-1",
            "f-1#20260304T150000",
            "f-1#20260309T090000",
            "u-1",
            "u-1#20260304T150000",
            "u-1#20260309T140000Z",
        ];
        assert_eq!(refs(SERIES), expected);
        assert_eq!(refs(&format!("{SERIES}{OVERRIDES}")), expected);
        let after = all(&calendar(&format!("{SERIES}{OVERRIDES}")));
        let after = by_ref(&after);
        assert_eq!(
            after["u-1#20260304T150000"],
            ("2026-03-04T21:00:00Z", "Utc (moved)")
        );
        assert_eq!(
            after["f-1#20260304T150000"],
            ("2026-03-04T21:00:00Z", "Floating (moved)")
        );
    }

    /// Two RDATEs naming one instant are one occurrence, under the ref of
    /// the one listed first; an override naming it, in either form, takes
    /// that ref.
    #[test]
    fn test_two_rdates_at_one_instant_keep_the_first_ref() {
        for (rdates, kept) in [
            (
                "RDATE:20260304T200000Z\r\nRDATE;TZID=America/New_York:20260304T150000\r\n",
                "two-1#20260304T200000Z",
            ),
            (
                "RDATE;TZID=America/New_York:20260304T150000\r\nRDATE:20260304T200000Z\r\n",
                "two-1#20260304T150000",
            ),
        ] {
            let series = format!(
                "BEGIN:VEVENT\r\n\
UID:two-1\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
RRULE:FREQ=WEEKLY;COUNT=2\r\n\
{rdates}END:VEVENT\r\n"
            );
            let alone = all(&format!("BEGIN:VCALENDAR\r\n{series}END:VCALENDAR\r\n"));
            assert_eq!(
                by_ref(&alone).keys().copied().collect::<Vec<_>>(),
                ["two-1", kept, "two-1#20260309T090000"],
                "{rdates}"
            );
            for rid in [
                "RECURRENCE-ID:20260304T200000Z",
                "RECURRENCE-ID;TZID=America/New_York:20260304T150000",
            ] {
                let raw = format!(
                    "BEGIN:VCALENDAR\r\n{series}BEGIN:VEVENT\r\n\
UID:two-1\r\n\
{rid}\r\n\
SUMMARY:Sync (moved)\r\n\
DTSTART:20260304T210000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n"
                );
                let drafts = all(&raw);
                let drafts = by_ref(&drafts);
                assert_eq!(drafts.len(), 3, "{rdates} {rid}");
                assert_eq!(
                    drafts[kept],
                    ("2026-03-04T21:00:00Z", "Sync (moved)"),
                    "{rdates} {rid}"
                );
            }
        }
    }

    /// A RANGE=THISANDFUTURE override whose RECURRENCE-ID is in UTC and
    /// whose DTSTART is in the series' zone moves the later occurrences by
    /// the move it made, both read in one zone: 09:00 to 10:00 New York
    /// time, every week.
    #[test]
    fn test_range_override_written_in_two_zones() {
        const RANGE: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:z-1\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260302T090000\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:z-1\r\n\
RECURRENCE-ID;RANGE=THISANDFUTURE:20260309T130000Z\r\n\
SUMMARY:Sync (later)\r\n\
DTSTART;TZID=America/New_York:20260309T100000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(RANGE);
        assert_eq!(drafts.len(), 4);
        assert_eq!(
            by_ref(&drafts),
            [
                ("z-1", ("2026-03-02T14:00:00Z", "Sync")),
                (
                    "z-1#20260309T090000",
                    ("2026-03-09T14:00:00Z", "Sync (later)")
                ),
                (
                    "z-1#20260316T090000",
                    ("2026-03-16T14:00:00Z", "Sync (later)")
                ),
                (
                    "z-1#20260323T090000",
                    ("2026-03-23T14:00:00Z", "Sync (later)")
                ),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );
    }

    /// A zone the file defines reads a UTC instant back as its wall clock,
    /// on either side of a change and in the repeated hour.
    #[test]
    fn test_defined_zone_reads_instants_as_wall_clock() {
        const ZONE: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Customized Time Zone\r\n\
BEGIN:STANDARD\r\n\
DTSTART:16011104T020000\r\n\
RRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11\r\n\
TZOFFSETFROM:-0400\r\n\
TZOFFSETTO:-0500\r\n\
END:STANDARD\r\n\
BEGIN:DAYLIGHT\r\n\
DTSTART:16010311T020000\r\n\
RRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=3\r\n\
TZOFFSETFROM:-0500\r\n\
TZOFFSETTO:-0400\r\n\
END:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
END:VCALENDAR\r\n";
        let cal = parse_calendar(ZONE);
        let def = &cal.timezones["Customized Time Zone"];
        for (utc, wall) in [
            ("20260115T140000", "20260115T090000"),
            ("20260308T065959", "20260308T015959"),
            ("20260308T070000", "20260308T030000"),
            ("20260701T130000", "20260701T090000"),
            ("20261101T053000", "20261101T013000"),
            ("20261101T063000", "20261101T013000"),
        ] {
            assert_eq!(def.local(at(utc)), Some(at(wall)), "{utc}");
        }
    }

    /// A daily series at a set time of day (BYHOUR/BYMINUTE/BYSECOND)
    /// expands; a series across the night clocks go forward leaves out
    /// the start that doesn't exist, without counting it.
    #[test]
    fn test_times_of_day_and_skipped_hour() {
        const SERIES: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Customized Time Zone\r\n\
BEGIN:STANDARD\r\n\
DTSTART:16011104T020000\r\n\
RRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=11\r\n\
TZOFFSETFROM:-0400\r\n\
TZOFFSETTO:-0500\r\n\
END:STANDARD\r\n\
BEGIN:DAYLIGHT\r\n\
DTSTART:16010311T020000\r\n\
RRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=3\r\n\
TZOFFSETFROM:-0500\r\n\
TZOFFSETTO:-0400\r\n\
END:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:b-1\r\n\
SUMMARY:Pills\r\n\
DTSTART:20260301T090000Z\r\n\
RRULE:FREQ=DAILY;COUNT=3;BYHOUR=9;BYMINUTE=0;BYSECOND=0\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:g-1\r\n\
SUMMARY:Night shift check\r\n\
DTSTART;TZID=America/New_York:20260307T023000\r\n\
RRULE:FREQ=DAILY;COUNT=3\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:g-2\r\n\
SUMMARY:Night shift check\r\n\
DTSTART;TZID=Customized Time Zone:20260307T023000\r\n\
RRULE:FREQ=DAILY;COUNT=3\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(SERIES);
        let times = by_ref(&drafts);
        let starts: Vec<(&str, &str)> = times.iter().map(|(r, (t, _))| (*r, *t)).collect();
        assert_eq!(
            starts,
            [
                ("b-1", "2026-03-01T09:00:00Z"),
                ("b-1#20260302T090000Z", "2026-03-02T09:00:00Z"),
                ("b-1#20260303T090000Z", "2026-03-03T09:00:00Z"),
                ("g-1", "2026-03-07T07:30:00Z"),
                ("g-1#20260309T023000", "2026-03-09T06:30:00Z"),
                ("g-1#20260310T023000", "2026-03-10T06:30:00Z"),
                ("g-2", "2026-03-07T07:30:00Z"),
                ("g-2#20260309T023000", "2026-03-09T06:30:00Z"),
                ("g-2#20260310T023000", "2026-03-10T06:30:00Z"),
            ]
        );
    }

    /// An override whose own start can't be read is left out, and the
    /// occurrence it would have replaced stays the series'.
    #[test]
    fn test_unreadable_override_leaves_the_occurrence() {
        const BROKEN: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:m-1\r\n\
SUMMARY:Standup\r\n\
DTSTART:20260301T090000Z\r\n\
RRULE:FREQ=DAILY;COUNT=3\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:m-1\r\n\
RECURRENCE-ID:20260302T090000Z\r\n\
SUMMARY:Standup (moved)\r\n\
DTSTART:garbage\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let drafts = all(BROKEN);
        assert_eq!(
            by_ref(&drafts),
            [
                ("m-1", ("2026-03-01T09:00:00Z", "Standup")),
                ("m-1#20260302T090000Z", ("2026-03-02T09:00:00Z", "Standup")),
                ("m-1#20260303T090000Z", ("2026-03-03T09:00:00Z", "Standup")),
            ]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        );
    }
}

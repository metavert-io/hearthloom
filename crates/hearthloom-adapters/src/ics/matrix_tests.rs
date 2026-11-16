//! The recurrence matrix: 39 calendars, each run through the importer
//! (`drafts`, imported on 1 January 2030) and checked against the
//! occurrences RFC 5545 or a stated Hearthloom policy gives. The cases come
//! from the evaluation that decided to keep the hand-written expander (see
//! the decision note at the top of `ics/rrule.rs`); a replacement must pass
//! them all.
//!
//! An occurrence is written `<start> | <title>`, the start as
//! `2026-03-02T14:00:00Z` (an instant, shown in UTC), `2026-03-10T09:00:00
//! floating` (a wall-clock time with no zone) or `2026-03-10 date` (an
//! all-day date). America/New_York in 2026: daylight time starts Sunday 8
//! March at 02:00 (UTC-5 to UTC-4) and ends Sunday 1 November at 02:00.
//!
//! The refs are pinned too: the cases' (`test_case_refs_are_stable`) and
//! nine inputs about how a UID is read (`test_ref_inputs_are_stable`). A
//! change that alters a ref makes every stored event under the old one
//! import a second time.

use chrono::NaiveDateTime;
use hearthloom_core::{EpisodeDraft, TimeUncertainty};

/// One calendar of the matrix.
pub(super) struct Case {
    pub(super) id: &'static str,
    /// What the case is about, in one line.
    pub(super) what: &'static str,
    events: Vec<String>,
    /// VTIMEZONE blocks, written ahead of the events.
    zones: Vec<String>,
    /// The occurrences it must import, in any order.
    expected: &'static [&'static str],
    /// Today's refs, as `<ref> -> <start>`, in any order.
    refs: &'static [&'static str],
}

impl Case {
    pub(super) fn ics(&self) -> String {
        let mut s = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//eval//EN\r\n");
        for zone in &self.zones {
            s.push_str(zone);
        }
        for event in &self.events {
            s.push_str(event);
        }
        s.push_str("END:VCALENDAR\r\n");
        s
    }
}

/// Lines, each ended with CRLF.
fn block(lines: &[&str]) -> String {
    lines.iter().map(|l| format!("{l}\r\n")).collect()
}

/// A VEVENT with these property lines.
fn ev(lines: &[&str]) -> String {
    format!("BEGIN:VEVENT\r\n{}END:VEVENT\r\n", block(lines))
}

/// A 09:00 New York series starting Monday 2 March 2026, 30 minutes long.
fn weekly_ny(uid: &str, rule: &str, extra: &[&str]) -> String {
    let mut lines = vec![
        format!("UID:{uid}"),
        "DTSTAMP:20260101T000000Z".into(),
        "SUMMARY:Weekly".into(),
        "DTSTART;TZID=America/New_York:20260302T090000".into(),
        "DTEND;TZID=America/New_York:20260302T093000".into(),
        format!("RRULE:{rule}"),
    ];
    lines.extend(extra.iter().map(|l| l.to_string()));
    ev(&lines.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Eastern time as Outlook and Exchange define it, under `tzid`, with
/// `extra` lines after the TZID.
fn eastern_zone(tzid: &str, extra: &[&str]) -> String {
    let tzid = format!("TZID:{tzid}");
    let mut lines = vec!["BEGIN:VTIMEZONE", tzid.as_str()];
    lines.extend(extra);
    lines.extend([
        "BEGIN:STANDARD",
        "DTSTART:16010101T020000",
        "TZOFFSETFROM:-0400",
        "TZOFFSETTO:-0500",
        "RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=1SU;BYMONTH=11",
        "END:STANDARD",
        "BEGIN:DAYLIGHT",
        "DTSTART:16010101T020000",
        "TZOFFSETFROM:-0500",
        "TZOFFSETTO:-0400",
        "RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=2SU;BYMONTH=3",
        "END:DAYLIGHT",
        "END:VTIMEZONE",
    ]);
    block(&lines)
}

pub(super) fn cases() -> Vec<Case> {
    vec![
        // Basis: RFC 5545 §3.3.10 and §3.8.5.3 — a rule repeats DTSTART's
        // wall-clock time in its zone, so 09:00 stays 09:00 once daylight
        // time starts.
        Case {
            id: "C01",
            what: "Weekly in a named zone across the March DST change",
            events: vec![weekly_ny("c01", "FREQ=WEEKLY;COUNT=3", &[])],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T13:00:00Z | Weekly",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c01 -> 2026-03-02T14:00:00Z",
                "c01#20260309T090000 -> 2026-03-09T13:00:00Z",
                "c01#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.4 and §3.8.2.4 — a DATE is a day, with no
        // time or zone.
        Case {
            id: "C02",
            what: "All-day (VALUE=DATE) daily, COUNT",
            events: vec![ev(&[
                "UID:c02",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Holiday",
                "DTSTART;VALUE=DATE:20260310",
                "DTEND;VALUE=DATE:20260311",
                "RRULE:FREQ=DAILY;COUNT=3",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-10 date | Holiday",
                "2026-03-11 date | Holiday",
                "2026-03-12 date | Holiday",
            ],
            refs: &[
                "c02 -> 2026-03-10 date",
                "c02#20260311 -> 2026-03-11 date",
                "c02#20260312 -> 2026-03-12 date",
            ],
        },
        // Basis: RFC 5545 §3.3.10 — UNTIL is inclusive and has DTSTART's
        // value type (a DATE here).
        Case {
            id: "C03",
            what: "All-day weekly with UNTIL as a DATE",
            events: vec![ev(&[
                "UID:c03",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Bins",
                "DTSTART;VALUE=DATE:20260301",
                "RRULE:FREQ=WEEKLY;UNTIL=20260315",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-01 date | Bins",
                "2026-03-08 date | Bins",
                "2026-03-15 date | Bins",
            ],
            refs: &[
                "c03 -> 2026-03-01 date",
                "c03#20260308 -> 2026-03-08 date",
                "c03#20260315 -> 2026-03-15 date",
            ],
        },
        // Basis: RFC 5545 §3.3.5 (form 1) — floating time is the same wall
        // clock in any zone and names no instant.
        Case {
            id: "C04",
            what: "Floating time (no TZID, no Z)",
            events: vec![ev(&[
                "UID:c04",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Standup",
                "DTSTART:20260310T090000",
                "RRULE:FREQ=DAILY;COUNT=2",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-10T09:00:00 floating | Standup",
                "2026-03-11T09:00:00 floating | Standup",
            ],
            refs: &[
                "c04 -> 2026-03-10T09:00:00 floating",
                "c04#20260311T090000 -> 2026-03-11T09:00:00 floating",
            ],
        },
        // Basis: RFC 5545 §3.3.10 — next to a zoned DTSTART, UNTIL must be
        // UTC, and it is inclusive (16 March 09:00 EDT is 13:00Z).
        Case {
            id: "C05",
            what: "UNTIL in UTC with a zoned DTSTART (inclusive bound)",
            events: vec![weekly_ny("c05", "FREQ=WEEKLY;UNTIL=20260316T130000Z", &[])],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T13:00:00Z | Weekly",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c05 -> 2026-03-02T14:00:00Z",
                "c05#20260309T090000 -> 2026-03-09T13:00:00Z",
                "c05#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: Hearthloom policy, a lenient reading — RFC 5545 §3.3.10
        // requires a UTC UNTIL here; a local one is read in DTSTART's zone,
        // as libical does.
        Case {
            id: "C06",
            what: "UNTIL written as local time next to a zoned DTSTART (non-conformant, common)",
            events: vec![weekly_ny("c06", "FREQ=WEEKLY;UNTIL=20260316T090000", &[])],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T13:00:00Z | Weekly",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c06 -> 2026-03-02T14:00:00Z",
                "c06#20260309T090000 -> 2026-03-09T13:00:00Z",
                "c06#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.5.1 — EXDATE names an instant: 13:00 London
        // (GMT) on 9 March is 09:00 New York (EDT).
        Case {
            id: "C07",
            what: "EXDATE written in another zone",
            events: vec![weekly_ny(
                "c07",
                "FREQ=WEEKLY;COUNT=3",
                &["EXDATE;TZID=Europe/London:20260309T130000"],
            )],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c07 -> 2026-03-02T14:00:00Z",
                "c07#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.5.1.
        Case {
            id: "C08",
            what: "EXDATE as a DATE on an all-day series",
            events: vec![ev(&[
                "UID:c08",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Holiday",
                "DTSTART;VALUE=DATE:20260310",
                "RRULE:FREQ=DAILY;COUNT=3",
                "EXDATE;VALUE=DATE:20260311",
            ])],
            zones: vec![],
            expected: &["2026-03-10 date | Holiday", "2026-03-12 date | Holiday"],
            refs: &["c08 -> 2026-03-10 date", "c08#20260312 -> 2026-03-12 date"],
        },
        // Basis: RFC 5545 §3.8.5.2 — RDATE adds occurrences; a PERIOD
        // starts at its first part.
        Case {
            id: "C09",
            what: "RDATE date-time in a zone plus RDATE;VALUE=PERIOD",
            events: vec![weekly_ny(
                "c09",
                "FREQ=WEEKLY;COUNT=2",
                &[
                    "RDATE;TZID=America/New_York:20260304T150000",
                    "RDATE;VALUE=PERIOD:20260305T170000Z/20260305T180000Z",
                ],
            )],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-04T20:00:00Z | Weekly",
                "2026-03-05T17:00:00Z | Weekly",
                "2026-03-09T13:00:00Z | Weekly",
            ],
            refs: &[
                "c09 -> 2026-03-02T14:00:00Z",
                "c09#20260304T150000 -> 2026-03-04T20:00:00Z",
                "c09#20260305T170000Z -> 2026-03-05T17:00:00Z",
                "c09#20260309T090000 -> 2026-03-09T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.5.2, and §3.8.2.4 (DTSTART is the first
        // occurrence).
        Case {
            id: "C10",
            what: "RDATE without an RRULE",
            events: vec![ev(&[
                "UID:c10",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Classes",
                "DTSTART:20260302T140000Z",
                "RDATE:20260303T140000Z,20260304T140000Z",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Classes",
                "2026-03-03T14:00:00Z | Classes",
                "2026-03-04T14:00:00Z | Classes",
            ],
            refs: &[
                "c10 -> 2026-03-02T14:00:00Z",
                "c10#20260303T140000Z -> 2026-03-03T14:00:00Z",
                "c10#20260304T140000Z -> 2026-03-04T14:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.4.4 — an override replaces the occurrence
        // its RECURRENCE-ID names.
        Case {
            id: "C11",
            what: "Override (RECURRENCE-ID) moves one occurrence",
            events: vec![
                weekly_ny("c11", "FREQ=WEEKLY;COUNT=3", &[]),
                ev(&[
                    "UID:c11",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Moved",
                    "RECURRENCE-ID;TZID=America/New_York:20260309T090000",
                    "DTSTART;TZID=America/New_York:20260309T110000",
                    "DTEND;TZID=America/New_York:20260309T113000",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T15:00:00Z | Moved",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c11 -> 2026-03-02T14:00:00Z",
                "c11#20260309T090000 -> 2026-03-09T15:00:00Z",
                "c11#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.4.4. The override of the first occurrence
        // takes the series' bare UID as its ref.
        Case {
            id: "C12",
            what: "Override of the first occurrence",
            events: vec![
                weekly_ny("c12", "FREQ=WEEKLY;COUNT=3", &[]),
                ev(&[
                    "UID:c12",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Moved first",
                    "RECURRENCE-ID;TZID=America/New_York:20260302T090000",
                    "DTSTART;TZID=America/New_York:20260302T100000",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T15:00:00Z | Moved first",
                "2026-03-09T13:00:00Z | Weekly",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c12 -> 2026-03-02T15:00:00Z",
                "c12#20260309T090000 -> 2026-03-09T13:00:00Z",
                "c12#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.4.4 — RECURRENCE-ID 13:00Z is the 09:00 EDT
        // occurrence, whatever form it is written in.
        Case {
            id: "C13",
            what: "Override whose RECURRENCE-ID and DTSTART are in other zones",
            events: vec![
                weekly_ny("c13", "FREQ=WEEKLY;COUNT=3", &[]),
                ev(&[
                    "UID:c13",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Moved",
                    "RECURRENCE-ID:20260309T130000Z",
                    "DTSTART;TZID=Europe/London:20260309T150000",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T15:00:00Z | Moved",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c13 -> 2026-03-02T14:00:00Z",
                "c13#20260309T090000 -> 2026-03-09T15:00:00Z",
                "c13#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.4.4 and §3.2.13 — THISANDFUTURE applies the
        // override, its move included, to every later occurrence.
        Case {
            id: "C14",
            what: "RANGE=THISANDFUTURE moves this and later occurrences",
            events: vec![
                weekly_ny("c14", "FREQ=WEEKLY;COUNT=4", &[]),
                ev(&[
                    "UID:c14",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Later",
                    "RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=America/New_York:20260309T090000",
                    "DTSTART;TZID=America/New_York:20260309T100000",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T14:00:00Z | Later",
                "2026-03-16T14:00:00Z | Later",
                "2026-03-23T14:00:00Z | Later",
            ],
            refs: &[
                "c14 -> 2026-03-02T14:00:00Z",
                "c14#20260309T090000 -> 2026-03-09T14:00:00Z",
                "c14#20260316T090000 -> 2026-03-16T14:00:00Z",
                "c14#20260323T090000 -> 2026-03-23T14:00:00Z",
            ],
        },
        // Basis: as C14 — the move is one hour however the two times are
        // written.
        Case {
            id: "C15",
            what: "RANGE=THISANDFUTURE with RECURRENCE-ID in UTC and DTSTART in the zone",
            events: vec![
                weekly_ny("c15", "FREQ=WEEKLY;COUNT=4", &[]),
                ev(&[
                    "UID:c15",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Later",
                    "RECURRENCE-ID;RANGE=THISANDFUTURE:20260309T130000Z",
                    "DTSTART;TZID=America/New_York:20260309T100000",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T14:00:00Z | Later",
                "2026-03-16T14:00:00Z | Later",
                "2026-03-23T14:00:00Z | Later",
            ],
            refs: &[
                "c15 -> 2026-03-02T14:00:00Z",
                "c15#20260309T090000 -> 2026-03-09T14:00:00Z",
                "c15#20260316T090000 -> 2026-03-16T14:00:00Z",
                "c15#20260323T090000 -> 2026-03-23T14:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.1.11 (STATUS:CANCELLED) and §3.8.4.4;
        // Hearthloom policy: a cancelled occurrence is no episode.
        Case {
            id: "C16",
            what: "Cancelled override (STATUS:CANCELLED on a RECURRENCE-ID)",
            events: vec![
                weekly_ny("c16", "FREQ=WEEKLY;COUNT=3", &[]),
                ev(&[
                    "UID:c16",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Weekly",
                    "RECURRENCE-ID;TZID=America/New_York:20260309T090000",
                    "DTSTART;TZID=America/New_York:20260309T090000",
                    "STATUS:CANCELLED",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c16 -> 2026-03-02T14:00:00Z",
                "c16#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.1.11; Hearthloom policy: a cancelled event
        // is no episode.
        Case {
            id: "C17",
            what: "Cancelled master (STATUS:CANCELLED on the series)",
            events: vec![weekly_ny(
                "c17",
                "FREQ=WEEKLY;COUNT=3",
                &["STATUS:CANCELLED"],
            )],
            zones: vec![],
            expected: &[],
            refs: &[],
        },
        // Basis: Hearthloom policy (no RFC answer for malformed input): an
        // override that can't be read doesn't take the series' occurrence.
        Case {
            id: "C18",
            what: "Override with an unreadable DTSTART",
            events: vec![
                ev(&[
                    "UID:c18",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Daily",
                    "DTSTART;TZID=America/New_York:20260310T090000",
                    "RRULE:FREQ=DAILY;COUNT=3",
                ]),
                ev(&[
                    "UID:c18",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:Broken",
                    "RECURRENCE-ID;TZID=America/New_York:20260311T090000",
                    "DTSTART:garbage",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-10T13:00:00Z | Daily",
                "2026-03-11T13:00:00Z | Daily",
                "2026-03-12T13:00:00Z | Daily",
            ],
            refs: &[
                "c18 -> 2026-03-10T13:00:00Z",
                "c18#20260311T090000 -> 2026-03-11T13:00:00Z",
                "c18#20260312T090000 -> 2026-03-12T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.10 — BYHOUR and BYMINUTE expand a DAILY
        // rule.
        Case {
            id: "C19",
            what: "BYHOUR/BYMINUTE/BYSECOND on a daily rule",
            events: vec![ev(&[
                "UID:c19",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Pills",
                "DTSTART;TZID=America/New_York:20260310T090000",
                "RRULE:FREQ=DAILY;COUNT=4;BYHOUR=9,17;BYMINUTE=0;BYSECOND=0",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-10T13:00:00Z | Pills",
                "2026-03-10T21:00:00Z | Pills",
                "2026-03-11T13:00:00Z | Pills",
                "2026-03-11T21:00:00Z | Pills",
            ],
            refs: &[
                "c19 -> 2026-03-10T13:00:00Z",
                "c19#20260310T170000 -> 2026-03-10T21:00:00Z",
                "c19#20260311T090000 -> 2026-03-11T13:00:00Z",
                "c19#20260311T170000 -> 2026-03-11T21:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.5 — a wall-clock time that doesn't exist is
        // read with the offset before the change (02:30 EST is 07:30Z).
        Case {
            id: "C20",
            what: "DTSTART in the DST gap (single event)",
            events: vec![ev(&[
                "UID:c20",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Gap",
                "DTSTART;TZID=America/New_York:20260308T023000",
            ])],
            zones: vec![],
            expected: &["2026-03-08T07:30:00Z | Gap"],
            refs: &["c20 -> 2026-03-08T07:30:00Z"],
        },
        // Basis: RFC 5545 §3.3.10 — an occurrence at a wall-clock time that
        // doesn't exist is ignored and not counted.
        Case {
            id: "C21",
            what: "Generated occurrence in the DST gap, with COUNT",
            events: vec![ev(&[
                "UID:c21",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Early",
                "DTSTART;TZID=America/New_York:20260307T023000",
                "RRULE:FREQ=DAILY;COUNT=3",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-07T07:30:00Z | Early",
                "2026-03-09T06:30:00Z | Early",
                "2026-03-10T06:30:00Z | Early",
            ],
            refs: &[
                "c21 -> 2026-03-07T07:30:00Z",
                "c21#20260309T023000 -> 2026-03-09T06:30:00Z",
                "c21#20260310T023000 -> 2026-03-10T06:30:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.5 — a wall-clock time that happens twice is
        // the first (01:30 EDT is 05:30Z).
        Case {
            id: "C22",
            what: "DTSTART in the repeated hour (single event)",
            events: vec![ev(&[
                "UID:c22",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Twice",
                "DTSTART;TZID=America/New_York:20261101T013000",
            ])],
            zones: vec![],
            expected: &["2026-11-01T05:30:00Z | Twice"],
            refs: &["c22 -> 2026-11-01T05:30:00Z"],
        },
        // Basis: RFC 5545 §3.3.5 (the first of the two) for each
        // occurrence.
        Case {
            id: "C23",
            what: "Generated occurrence in the repeated hour",
            events: vec![ev(&[
                "UID:c23",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Night",
                "DTSTART;TZID=America/New_York:20261031T013000",
                "RRULE:FREQ=DAILY;COUNT=3",
            ])],
            zones: vec![],
            expected: &[
                "2026-10-31T05:30:00Z | Night",
                "2026-11-01T05:30:00Z | Night",
                "2026-11-02T06:30:00Z | Night",
            ],
            refs: &[
                "c23 -> 2026-10-31T05:30:00Z",
                "c23#20261101T013000 -> 2026-11-01T05:30:00Z",
                "c23#20261102T013000 -> 2026-11-02T06:30:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.10, read strictly — a start that doesn't
        // exist is no part of the set, so BYSETPOS=1 on 8 March picks
        // 03:30 EDT. (The RFC doesn't spell out the order.)
        Case {
            id: "C24",
            what: "BYSETPOS after removing a nonexistent time",
            events: vec![ev(&[
                "UID:c24",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Pick",
                "DTSTART;TZID=America/New_York:20260307T023000",
                "RRULE:FREQ=DAILY;COUNT=3;BYHOUR=2,3;BYMINUTE=30;BYSECOND=0;BYSETPOS=1",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-07T07:30:00Z | Pick",
                "2026-03-08T07:30:00Z | Pick",
                "2026-03-09T06:30:00Z | Pick",
            ],
            refs: &[
                "c24 -> 2026-03-07T07:30:00Z",
                "c24#20260308T033000 -> 2026-03-08T07:30:00Z",
                "c24#20260309T023000 -> 2026-03-09T06:30:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.10 (its own "last work day of the month"
        // example).
        Case {
            id: "C25",
            what: "BYSETPOS=-1: last weekday of the month",
            events: vec![ev(&[
                "UID:c25",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Payday",
                "DTSTART;TZID=America/New_York:20260130T090000",
                "RRULE:FREQ=MONTHLY;COUNT=3;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
            ])],
            zones: vec![],
            expected: &[
                "2026-01-30T14:00:00Z | Payday",
                "2026-02-27T14:00:00Z | Payday",
                "2026-03-31T13:00:00Z | Payday",
            ],
            refs: &[
                "c25 -> 2026-01-30T14:00:00Z",
                "c25#20260227T090000 -> 2026-02-27T14:00:00Z",
                "c25#20260331T090000 -> 2026-03-31T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.10 (the BYxxx table, note 1) — BYDAY limits
        // the BYMONTHDAY days and keeps its ordinal: only days that are the
        // 2nd or 9th and a first Monday (9 March is the second Monday).
        Case {
            id: "C26",
            what: "BYMONTHDAY with an ordinal BYDAY (1MO)",
            events: vec![ev(&[
                "UID:c26",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Club",
                "DTSTART:20260202T090000",
                "RRULE:FREQ=MONTHLY;BYMONTHDAY=2,9;BYDAY=1MO;COUNT=3",
            ])],
            zones: vec![],
            expected: &[
                "2026-02-02T09:00:00 floating | Club",
                "2026-03-02T09:00:00 floating | Club",
                "2026-11-02T09:00:00 floating | Club",
            ],
            refs: &[
                "c26 -> 2026-02-02T09:00:00 floating",
                "c26#20260302T090000 -> 2026-03-02T09:00:00 floating",
                "c26#20261102T090000 -> 2026-11-02T09:00:00 floating",
            ],
        },
        // Basis: RFC 5545 §3.3.10 — "The DTSTART property value always
        // counts as the first occurrence." (§3.8.5.3 calls a DTSTART off
        // the rule undefined, so another answer is defensible.)
        Case {
            id: "C27",
            what: "DTSTART that the rule doesn't match, with COUNT",
            events: vec![ev(&[
                "UID:c27",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Off-rule",
                "DTSTART;TZID=America/New_York:20260302T090000",
                "RRULE:FREQ=WEEKLY;BYDAY=TU;COUNT=2",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Off-rule",
                "2026-03-03T14:00:00Z | Off-rule",
            ],
            refs: &[
                "c27 -> 2026-03-02T14:00:00Z",
                "c27#20260303T090000 -> 2026-03-03T14:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.2.19 and §3.6.5 — the TZID names the file's
        // VTIMEZONE (here also the Windows name of America/New_York).
        Case {
            id: "C28",
            what: "Windows zone name as TZID ('Eastern Standard Time', with Outlook's VTIMEZONE)",
            events: vec![ev(&[
                "UID:c28",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Outlook",
                "DTSTART;TZID=Eastern Standard Time:20260302T090000",
                "RRULE:FREQ=WEEKLY;COUNT=2",
            ])],
            zones: vec![eastern_zone("Eastern Standard Time", &[])],
            expected: &[
                "2026-03-02T14:00:00Z | Outlook",
                "2026-03-09T13:00:00Z | Outlook",
            ],
            refs: &[
                "c28 -> 2026-03-02T14:00:00Z",
                "c28#20260309T090000 -> 2026-03-09T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.6.5 — the VTIMEZONE's own rules (CDO id 10 is
        // Eastern too).
        Case {
            id: "C29",
            what: "Custom TZID with X-MICROSOFT-CDO-TZID",
            events: vec![ev(&[
                "UID:c29",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Exchange",
                "DTSTART;TZID=\"Customized Time Zone\":20260302T090000",
                "RRULE:FREQ=WEEKLY;COUNT=2",
            ])],
            zones: vec![eastern_zone(
                "Customized Time Zone",
                &["X-MICROSOFT-CDO-TZID:10"],
            )],
            expected: &[
                "2026-03-02T14:00:00Z | Exchange",
                "2026-03-09T13:00:00Z | Exchange",
            ],
            refs: &[
                "c29 -> 2026-03-02T14:00:00Z",
                "c29#20260309T090000 -> 2026-03-09T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.6.5 — the file's VTIMEZONE defines the
        // offsets.
        Case {
            id: "C30",
            what: "VTIMEZONE defined only in the file (no known name, no X- hints)",
            events: vec![ev(&[
                "UID:c30",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Office",
                "DTSTART;TZID=Head Office:20260302T090000",
                "RRULE:FREQ=WEEKLY;COUNT=2",
            ])],
            zones: vec![block(&[
                "BEGIN:VTIMEZONE",
                "TZID:Head Office",
                "BEGIN:STANDARD",
                "DTSTART:19701101T020000",
                "TZOFFSETFROM:-0400",
                "TZOFFSETTO:-0500",
                "RRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=1SU",
                "END:STANDARD",
                "BEGIN:DAYLIGHT",
                "DTSTART:19700308T020000",
                "TZOFFSETFROM:-0500",
                "TZOFFSETTO:-0400",
                "RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=2SU",
                "END:DAYLIGHT",
                "END:VTIMEZONE",
            ])],
            expected: &[
                "2026-03-02T14:00:00Z | Office",
                "2026-03-09T13:00:00Z | Office",
            ],
            refs: &[
                "c30 -> 2026-03-02T14:00:00Z",
                "c30#20260309T090000 -> 2026-03-09T13:00:00Z",
            ],
        },
        // Basis: Hearthloom policy — invalid per RFC 5545 §3.2.19 (a TZID
        // needs a VTIMEZONE); read as floating, with hour-level
        // uncertainty.
        Case {
            id: "C31",
            what: "Unknown TZID with no VTIMEZONE",
            events: vec![ev(&[
                "UID:c31",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Mystery",
                "DTSTART;TZID=Mars/Olympus_Mons:20260302T090000",
            ])],
            zones: vec![],
            expected: &["2026-03-02T09:00:00 floating | Mystery"],
            refs: &["c31 -> 2026-03-02T09:00:00 floating"],
        },
        // Basis: RFC 5545 §3.6.6 — VALARM is a nested component; its
        // properties are its own.
        Case {
            id: "C32",
            what: "VALARM inside an event (its SUMMARY/DESCRIPTION/ATTENDEE must not leak)",
            events: vec![ev(&[
                "UID:c32",
                "DTSTAMP:20260101T000000Z",
                "DTSTART:20260310T140000Z",
                "BEGIN:VALARM",
                "ACTION:EMAIL",
                "TRIGGER:-PT15M",
                "SUMMARY:Alarm summary",
                "DESCRIPTION:Reminder text",
                "ATTENDEE:mailto:alarm@example.com",
                "END:VALARM",
                "SUMMARY:Dentist",
            ])],
            zones: vec![],
            expected: &["2026-03-10T14:00:00Z | Dentist"],
            refs: &["c32 -> 2026-03-10T14:00:00Z"],
        },
        // Basis: RFC 5545 §3.8.4.4 — an override belongs to the series
        // with its UID.
        Case {
            id: "C33",
            what: "Two series in one file; an override of one must not touch the other",
            events: vec![
                weekly_ny("c33-a", "FREQ=WEEKLY;COUNT=3", &[])
                    .replace("SUMMARY:Weekly", "SUMMARY:A"),
                weekly_ny("c33-b", "FREQ=WEEKLY;COUNT=3", &[])
                    .replace("SUMMARY:Weekly", "SUMMARY:B"),
                ev(&[
                    "UID:c33-b",
                    "DTSTAMP:20260101T000000Z",
                    "SUMMARY:B moved",
                    "RECURRENCE-ID;TZID=America/New_York:20260309T090000",
                    "DTSTART;TZID=America/New_York:20260309T110000",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | A",
                "2026-03-09T13:00:00Z | A",
                "2026-03-16T13:00:00Z | A",
                "2026-03-02T14:00:00Z | B",
                "2026-03-09T15:00:00Z | B moved",
                "2026-03-16T13:00:00Z | B",
            ],
            refs: &[
                "c33-a -> 2026-03-02T14:00:00Z",
                "c33-a#20260309T090000 -> 2026-03-09T13:00:00Z",
                "c33-a#20260316T090000 -> 2026-03-16T13:00:00Z",
                "c33-b -> 2026-03-02T14:00:00Z",
                "c33-b#20260309T090000 -> 2026-03-09T15:00:00Z",
                "c33-b#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.4.4 — overrides match by UID and
        // RECURRENCE-ID; SEQUENCE is a revision counter (§3.8.7.4) and
        // often differs between a series and its overrides.
        Case {
            id: "C34",
            what: "Override with a higher SEQUENCE than its master",
            events: vec![
                weekly_ny("c34", "FREQ=WEEKLY;COUNT=3", &["SEQUENCE:0"]),
                ev(&[
                    "UID:c34",
                    "DTSTAMP:20260101T000000Z",
                    "SEQUENCE:1",
                    "SUMMARY:Moved",
                    "RECURRENCE-ID;TZID=America/New_York:20260309T090000",
                    "DTSTART;TZID=America/New_York:20260309T110000",
                ]),
            ],
            zones: vec![],
            expected: &[
                "2026-03-02T14:00:00Z | Weekly",
                "2026-03-09T15:00:00Z | Moved",
                "2026-03-16T13:00:00Z | Weekly",
            ],
            refs: &[
                "c34 -> 2026-03-02T14:00:00Z",
                "c34#20260309T090000 -> 2026-03-09T15:00:00Z",
                "c34#20260316T090000 -> 2026-03-16T13:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.3.10 — invalid dates (31 February, 31 April)
        // are ignored and not counted.
        Case {
            id: "C35",
            what: "Monthly on the 31st skips short months (not counted)",
            events: vec![ev(&[
                "UID:c35",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Month end",
                "DTSTART:20260131T090000",
                "RRULE:FREQ=MONTHLY;COUNT=3",
            ])],
            zones: vec![],
            expected: &[
                "2026-01-31T09:00:00 floating | Month end",
                "2026-03-31T09:00:00 floating | Month end",
                "2026-05-31T09:00:00 floating | Month end",
            ],
            refs: &[
                "c35 -> 2026-01-31T09:00:00 floating",
                "c35#20260331T090000 -> 2026-03-31T09:00:00 floating",
                "c35#20260531T090000 -> 2026-05-31T09:00:00 floating",
            ],
        },
        // Basis: RFC 5545 §3.8.5.2 — an occurrence RRULE and RDATE both
        // make counts once.
        Case {
            id: "C36",
            what: "RDATE equal to an RRULE occurrence appears once",
            events: vec![ev(&[
                "UID:c36",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Dup",
                "DTSTART:20260302T140000Z",
                "RRULE:FREQ=WEEKLY;COUNT=2",
                "RDATE:20260309T140000Z",
            ])],
            zones: vec![],
            expected: &["2026-03-02T14:00:00Z | Dup", "2026-03-09T14:00:00Z | Dup"],
            refs: &[
                "c36 -> 2026-03-02T14:00:00Z",
                "c36#20260309T140000Z -> 2026-03-09T14:00:00Z",
            ],
        },
        // Basis: RFC 5545 §3.8.5.1 — EXDATE wins over RDATE and RRULE.
        Case {
            id: "C37",
            what: "EXDATE removes an RDATE",
            events: vec![ev(&[
                "UID:c37",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Gone",
                "DTSTART:20260302T140000Z",
                "RDATE:20260303T140000Z",
                "EXDATE:20260303T140000Z",
            ])],
            zones: vec![],
            expected: &["2026-03-02T14:00:00Z | Gone"],
            refs: &["c37 -> 2026-03-02T14:00:00Z"],
        },
        // Basis: RFC 5545 §3.3.10 — BYDAY's entries are a union: the first
        // Monday, and every Friday.
        Case {
            id: "C38",
            what: "BYDAY mixing a numbered and a plain weekday (1MO,FR)",
            events: vec![ev(&[
                "UID:c38",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Mixed",
                "DTSTART:20260302T090000",
                "RRULE:FREQ=MONTHLY;BYDAY=1MO,FR;COUNT=4",
            ])],
            zones: vec![],
            expected: &[
                "2026-03-02T09:00:00 floating | Mixed",
                "2026-03-06T09:00:00 floating | Mixed",
                "2026-03-13T09:00:00 floating | Mixed",
                "2026-03-20T09:00:00 floating | Mixed",
            ],
            refs: &[
                "c38 -> 2026-03-02T09:00:00 floating",
                "c38#20260306T090000 -> 2026-03-06T09:00:00 floating",
                "c38#20260313T090000 -> 2026-03-13T09:00:00 floating",
                "c38#20260320T090000 -> 2026-03-20T09:00:00 floating",
            ],
        },
        // Basis: Hearthloom policy — sub-daily rules (HOURLY, MINUTELY,
        // SECONDLY; no calendar application offers them) are refused on
        // purpose, and the event keeps the occurrences the file names
        // without its rule: DTSTART alone. RFC 5545 §3.3.10's answer is
        // four occurrences, BYHOUR limiting the HOURLY rule: 09:00Z and
        // 10:00Z on 10 and 11 March.
        Case {
            id: "C39",
            what: "HOURLY rule limited by BYHOUR",
            events: vec![ev(&[
                "UID:c39",
                "DTSTAMP:20260101T000000Z",
                "SUMMARY:Hourly",
                "DTSTART:20260310T090000Z",
                "RRULE:FREQ=HOURLY;BYHOUR=9,10;COUNT=4",
            ])],
            zones: vec![],
            expected: &["2026-03-10T09:00:00Z | Hourly"],
            refs: &["c39 -> 2026-03-10T09:00:00Z"],
        },
    ]
}

/// Inputs about how a ref is read from a UID (or made without one). Each
/// pins today's refs exactly: a change that alters one makes the events
/// already stored under it import a second time.
pub(super) fn ref_inputs() -> Vec<RefInput> {
    let cal = |body: &str| format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n");
    vec![
        RefInput {
            id: "R1",
            what: "UID with an escaped comma and semicolon (kept as written)",
            ics: cal("BEGIN:VEVENT\r\nUID:a\\,b\\;c@x\r\nDTSTART:20260302T140000Z\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            refs: &["a\\,b\\;c@x -> 2026-03-02T14:00:00Z"],
        },
        RefInput {
            id: "R2",
            what: "UID with trailing spaces (dropped)",
            ics: cal("BEGIN:VEVENT\r\nUID:spaced@x  \r\nDTSTART:20260302T140000Z\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            refs: &["spaced@x -> 2026-03-02T14:00:00Z"],
        },
        RefInput {
            id: "R3",
            what: "UID folded over two lines (unfolded)",
            ics: cal("BEGIN:VEVENT\r\nUID:0123456789012345678901234567890123456789012345678901234567890123456789\r\n abcdef@example.com\r\nDTSTART:20260302T140000Z\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            refs: &["0123456789012345678901234567890123456789012345678901234567890123456789abcdef@example.com -> 2026-03-02T14:00:00Z"],
        },
        RefInput {
            id: "R4",
            what: "Lower-case property names, quoted TZID",
            ics: cal("begin:vevent\r\nuid:lower@x\r\ndtstart;tzid=\"America/New_York\":20260302T090000\r\nrrule:FREQ=DAILY;COUNT=2\r\nsummary:x\r\nend:vevent\r\n"),
            refs: &[
                "lower@x -> 2026-03-02T14:00:00Z",
                "lower@x#20260303T090000 -> 2026-03-03T14:00:00Z",
            ],
        },
        RefInput {
            id: "R5",
            what: "No UID (the ref falls back to DTSTART's text)",
            ics: cal("BEGIN:VEVENT\r\nDTSTART:20260302T140000Z\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            refs: &["20260302T140000Z -> 2026-03-02T14:00:00Z"],
        },
        RefInput {
            id: "R6",
            what: "UID with a backslash-n (kept as written)",
            ics: cal("BEGIN:VEVENT\r\nUID:line\\nbreak@x\r\nDTSTART:20260302T140000Z\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            refs: &["line\\nbreak@x -> 2026-03-02T14:00:00Z"],
        },
        RefInput {
            id: "R7",
            what: "Override alone (no series in the file): RECURRENCE-ID as written",
            ics: cal("BEGIN:VEVENT\r\nUID:lonely@x\r\nRECURRENCE-ID;TZID=America/New_York:20260309T090000\r\nDTSTART;TZID=America/New_York:20260309T110000\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            refs: &["lonely@x#20260309T090000 -> 2026-03-09T15:00:00Z"],
        },
        RefInput {
            id: "R8",
            what: "UID with non-ASCII characters",
            ics: cal("BEGIN:VEVENT\r\nUID:caf\u{e9}-\u{1F600}@x\r\nDTSTART:20260302T140000Z\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            refs: &["caf\u{e9}-\u{1F600}@x -> 2026-03-02T14:00:00Z"],
        },
        RefInput {
            id: "R9",
            what: "SUMMARY with escapes (the title is unescaped, on one line; the ref is the UID)",
            ics: cal("BEGIN:VEVENT\r\nUID:esc@x\r\nDTSTART:20260302T140000Z\r\nSUMMARY:a\\, b\\; c \\\\ d\\Ne\r\nEND:VEVENT\r\n"),
            refs: &["esc@x -> 2026-03-02T14:00:00Z"],
        },
    ]
}

/// One input of `ref_inputs`.
pub(super) struct RefInput {
    id: &'static str,
    what: &'static str,
    pub(super) ics: String,
    /// Today's refs, as `<ref> -> <start>`, in any order.
    refs: &'static [&'static str],
}

/// The import time of every case: late enough that no case's series is cut
/// short by it.
fn imported_at() -> NaiveDateTime {
    NaiveDateTime::parse_from_str("20300101T000000", "%Y%m%dT%H%M%S").expect("import time")
}

/// An occurrence's start in the matrix's notation.
fn start(draft: &EpisodeDraft) -> String {
    let utc = draft.event_time.utc.clone().unwrap_or_default();
    match (
        draft.event_time.uncertainty,
        draft.event_time.tz_offset_minutes,
    ) {
        (TimeUncertainty::Day, _) => format!("{} date", utc.get(..10).unwrap_or(&utc)),
        (TimeUncertainty::Hour, None) => format!("{} floating", utc.trim_end_matches('Z')),
        _ => utc,
    }
}

/// What the importer makes of a calendar: its occurrences
/// (`<start> | <title>`) and refs (`<ref> -> <start>`), each sorted. A
/// file it refuses is an error, never an import of nothing: a case that
/// expects no occurrence (C17) passes only when the file was read.
fn import(ics: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let drafts =
        super::drafts(ics, None, imported_at(), "matrix.ics").map_err(|e| format!("{e:#}"))?;
    let mut occurrences: Vec<String> = drafts
        .iter()
        .map(|d| format!("{} | {}", start(d), d.title))
        .collect();
    let mut refs: Vec<String> = drafts
        .iter()
        .map(|d| format!("{} -> {}", d.source_ref, start(d)))
        .collect();
    occurrences.sort();
    refs.sort();
    Ok((occurrences, refs))
}

/// A failure line for a calendar the importer refused.
fn refused(id: &str, what: &str, error: &str) -> String {
    format!("{id} {what}\n  refused: {error}")
}

fn sorted(items: &[&str]) -> Vec<String> {
    let mut items: Vec<String> = items.iter().map(|s| s.to_string()).collect();
    items.sort();
    items
}

#[test]
fn test_every_case_imports_its_expected_occurrences() {
    let cases = cases();
    assert_eq!(cases.len(), 39);
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|case| {
            let (got, _) = match import(&case.ics()) {
                Ok(imported) => imported,
                Err(error) => return Some(refused(case.id, case.what, &error)),
            };
            let expected = sorted(case.expected);
            (got != expected).then(|| {
                format!(
                    "{} {}\n  expected: {expected:#?}\n  got: {got:#?}",
                    case.id, case.what
                )
            })
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A change that alters one of these refs makes every event stored under
/// the old one import a second time.
#[test]
fn test_case_refs_are_stable() {
    let failures: Vec<String> = cases()
        .iter()
        .filter_map(|case| {
            let (_, got) = match import(&case.ics()) {
                Ok(imported) => imported,
                Err(error) => return Some(refused(case.id, case.what, &error)),
            };
            let expected = sorted(case.refs);
            (got != expected).then(|| {
                format!(
                    "{} {}\n  expected: {expected:#?}\n  got: {got:#?}",
                    case.id, case.what
                )
            })
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A change that alters one of these refs makes every event stored under
/// the old one import a second time.
#[test]
fn test_ref_inputs_are_stable() {
    let inputs = ref_inputs();
    assert_eq!(inputs.len(), 9);
    let failures: Vec<String> = inputs
        .iter()
        .filter_map(|input| {
            let (_, got) = match import(&input.ics) {
                Ok(imported) => imported,
                Err(error) => return Some(refused(input.id, input.what, &error)),
            };
            let expected = sorted(input.refs);
            (got != expected).then(|| {
                format!(
                    "{} {}\n  expected: {expected:#?}\n  got: {got:#?}",
                    input.id, input.what
                )
            })
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    // R9's title: unescaped, and its escaped line break read as a space.
    let r9 = inputs.iter().find(|i| i.id == "R9").expect("R9");
    let (occurrences, _) = import(&r9.ics).expect("R9 imports");
    assert_eq!(occurrences, ["2026-03-02T14:00:00Z | a, b; c \\ d e"]);
}

/// A calendar the importer refuses fails a case that expects nothing,
/// rather than passing as an import of nothing.
#[test]
fn test_a_refused_calendar_is_no_empty_import() {
    let refusal = import("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n");
    assert!(refusal.is_err(), "{refusal:?}");
    let c17 = cases().into_iter().find(|c| c.id == "C17").expect("C17");
    assert!(c17.expected.is_empty());
    assert_eq!(import(&c17.ics()), Ok((Vec::new(), Vec::new())));
}

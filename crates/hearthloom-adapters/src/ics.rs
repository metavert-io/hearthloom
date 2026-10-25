//! ICS calendar adapter: VEVENTs become episodes; attendee emails drive
//! person entity resolution (attribute-based identity — the same email in
//! a git commit and a calendar invite is the same Person).
//!
//! Hand-rolled parser for the subset we need (unfolding, property params,
//! VEVENT blocks) with chrono-tz for TZID resolution. Timezone honesty:
//! all-day events get day-level uncertainty; floating times (no TZID, no
//! Z) get hour-level uncertainty instead of a fabricated offset.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use hearthloom_core::{one_line, EdgeAssertion, EntityRef, EpisodeDraft, EventTime, TimeUncertainty};

/// Parse an .ics file into episode drafts (one per VEVENT). `scope: None`
/// lets the store infer scope from the organizer/attendee identities.
pub fn ingest(path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let events = parse_events(&raw);
    if events.is_empty() {
        return Err(anyhow!("no VEVENTs found in {}", path.display()));
    }
    events.iter().map(|e| event_to_draft(e, scope)).collect()
}

/// One parsed property: value + params.
#[derive(Debug, Clone)]
struct Prop {
    value: String,
    params: HashMap<String, String>,
}

/// A VEVENT as a multimap of properties.
#[derive(Debug, Default)]
struct VEvent {
    props: HashMap<String, Vec<Prop>>,
}

impl VEvent {
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

fn parse_events(raw: &str) -> Vec<VEvent> {
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

    let mut events = Vec::new();
    let mut current: Option<VEvent> = None;
    for line in &lines {
        let line = line.trim_end();
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            current = Some(VEvent::default());
            continue;
        }
        if line.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(event) = current.take() {
                events.push(event);
            }
            continue;
        }
        if let Some(event) = current.as_mut() {
            if let Some((name, prop)) = parse_prop_line(line) {
                event.props.entry(name).or_default().push(prop);
            }
        }
    }
    events
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

/// Turn a DTSTART/DTEND property into an EventTime.
fn parse_ics_time(prop: &Prop) -> Result<EventTime> {
    let value = prop.value.trim();

    // All-day: VALUE=DATE or a bare 8-digit date.
    let is_date = prop.params.get("VALUE").map(|v| v == "DATE").unwrap_or(false)
        || (value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()));
    if is_date {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d")
            .map_err(|e| anyhow!("bad DATE {value:?}: {e}"))?;
        let midnight = date.and_hms_opt(0, 0, 0).unwrap();
        return Ok(EventTime {
            utc: Some(format!("{}Z", midnight.format("%Y-%m-%dT%H:%M:%S"))),
            tz_offset_minutes: None,
            uncertainty: TimeUncertainty::Day,
        });
    }

    // UTC: ...Z suffix.
    if let Some(stripped) = value.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S")
            .map_err(|e| anyhow!("bad UTC datetime {value:?}: {e}"))?;
        return Ok(EventTime {
            utc: Some(format!("{}Z", naive.format("%Y-%m-%dT%H:%M:%S"))),
            tz_offset_minutes: Some(0),
            uncertainty: TimeUncertainty::Exact,
        });
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .map_err(|e| anyhow!("bad datetime {value:?}: {e}"))?;

    // TZID param → resolve the offset for that instant via chrono-tz.
    if let Some(tzid) = prop.params.get("TZID") {
        let tz: chrono_tz::Tz = tzid
            .parse()
            .map_err(|_| anyhow!("unknown TZID {tzid:?}"))?;
        let local = tz
            .from_local_datetime(&naive)
            .earliest()
            .ok_or_else(|| anyhow!("nonexistent local time {value:?} in {tzid}"))?;
        return Ok(EventTime::from_fixed(
            local.with_timezone(&local.offset().fix()),
            TimeUncertainty::Exact,
        ));
    }

    // Floating time: no timezone at all. Record as if UTC but mark the
    // hour-level uncertainty rather than fabricating an offset.
    Ok(EventTime {
        utc: Some(
            Utc.from_utc_datetime(&naive)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        ),
        tz_offset_minutes: None,
        uncertainty: TimeUncertainty::Hour,
    })
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

fn event_to_draft(event: &VEvent, scope: Option<&str>) -> Result<EpisodeDraft> {
    // SUMMARY and LOCATION are one-line fields whoever sent the invite
    // wrote: an escaped `\n` must not start a line of its own (a chapter
    // lists titles one per line; a venue names a visit).
    let summary = event
        .value("SUMMARY")
        .map(|s| one_line(&unescape(s)))
        .unwrap_or_else(|| "(untitled event)".into());
    let dtstart = event
        .first("DTSTART")
        .ok_or_else(|| anyhow!("VEVENT without DTSTART: {summary}"))?;
    let event_time = parse_ics_time(dtstart)?;

    // Source-native identity: UID (+ RECURRENCE-ID for instances), with
    // DTSTART as a fallback for UID-less exports.
    let uid = event.value("UID").unwrap_or(&dtstart.value).to_string();
    let source_ref = match event.value("RECURRENCE-ID") {
        Some(rid) => format!("{uid}#{rid}"),
        None => uid.clone(),
    };

    let mut body = String::new();
    body.push_str(&format!("event: {summary}\n"));
    if let Some(location) = event.value("LOCATION").map(|l| one_line(&unescape(l))) {
        if !location.is_empty() {
            body.push_str(&format!("location: {location}\n"));
        }
    }
    if let Some(utc) = &event_time.utc {
        body.push_str(&format!("start: {utc}\n"));
    }
    if let Some(end) = event.first("DTEND").and_then(|p| parse_ics_time(p).ok()) {
        if let Some(utc) = end.utc {
            body.push_str(&format!("end: {utc}\n"));
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

    if let Some((email, name)) = event.first("ORGANIZER").and_then(parse_person) {
        body.push_str(&format!("organizer: {name} <{email}>\n"));
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
        body.push_str("attendees:\n");
    }
    for (email, name) in attendees {
        body.push_str(&format!("  {name} <{email}>\n"));
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
            body.push_str(&format!("\n{description}\n"));
        }
    }

    Ok(EpisodeDraft {
        source: "ics".into(),
        source_ref,
        kind: "calendar_event".into(),
        title: summary,
        body,
        event_time,
        trust: "imported".into(),
        scope: scope.map(String::from),
        entities,
        edges,
    })
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

    #[test]
    fn test_parse_sample() {
        let events = parse_events(SAMPLE);
        assert_eq!(events.len(), 2);

        let draft = event_to_draft(&events[0], Some("work")).unwrap();
        // Folded SUMMARY line reassembled.
        assert_eq!(draft.title, "Roadmap review with the platform team");
        assert_eq!(draft.source_ref, "evt-123@example.com");
        assert_eq!(draft.kind, "calendar_event");
        assert_eq!(draft.trust, "imported");
        // 14:00 EDT (-0400) = 18:00 UTC.
        assert_eq!(draft.event_time.utc.as_deref(), Some("2026-03-15T18:00:00Z"));
        assert_eq!(draft.event_time.tz_offset_minutes, Some(-240));
        assert_eq!(draft.event_time.uncertainty, TimeUncertainty::Exact);
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
        let events = parse_events(M);
        let draft = event_to_draft(&events[0], Some("personal")).unwrap();
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
        let events = parse_events(FORGED);
        let draft = event_to_draft(&events[0], None).unwrap();
        assert_eq!(draft.title, "Lunch - Wire the money");
        assert_eq!(draft.entities[0].name, "Lunch - Wire the money");
        assert!(draft.body.contains("event: Lunch - Wire the money\n"), "{}", draft.body);
        assert!(draft.body.contains("location: Cafe [12:00] Elsewhere (40.71,-74.00)\n"), "{}", draft.body);
        assert!(!draft.body.lines().any(|l| l.starts_with('[') || l.starts_with("- ")), "{}", draft.body);
        assert!(draft.body.contains("line one\nline two"));
    }

    #[test]
    fn test_all_day_event_day_uncertainty() {
        let events = parse_events(SAMPLE);
        let draft = event_to_draft(&events[1], Some("personal")).unwrap();
        assert_eq!(draft.event_time.uncertainty, TimeUncertainty::Day);
        assert_eq!(draft.event_time.utc.as_deref(), Some("2026-07-04T00:00:00Z"));
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
        let prop = Prop {
            value: "20260315T140000".into(),
            params: HashMap::new(),
        };
        let t = parse_ics_time(&prop).unwrap();
        assert_eq!(t.uncertainty, TimeUncertainty::Hour);
        assert_eq!(t.tz_offset_minutes, None);
    }
}

//! Draft builders + apply steps for the iOS device-feed wave (v51):
//! calendar events, contact cards, motion/trip segments, media plays,
//! and vision scans. Each mirrors the proven live_health/live_photos
//! pattern — the HTTP handler deserializes the phone's batch, these
//! builders shape idempotent EpisodeDrafts (UNIQUE(source, source_ref)
//! makes re-syncs no-ops), and apply_batch's attribute-first entity
//! resolution does the identity work.

use hearthloom_core::store::{Actor, PendingContact};
use hearthloom_core::time::{EventTime, TimeUncertainty};
use hearthloom_core::{one_line, EdgeAssertion, EntityRef, EpisodeDraft, IngestBatch};

use crate::AppState;

/* ————— calendar (POST /v1/ingest/calendar) ————— */

/// One live calendar event from EventKit. Converges with the ics import
/// on source="ics" + the event UID, so a phone-synced event and a later
/// .ics import of the same calendar dedupe via UNIQUE(source, source_ref).
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct CalendarEventIn {
    pub uid: String,
    pub title: String,
    /// RFC3339 start/end.
    pub start: String,
    #[serde(default)]
    pub end: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub attendees: Vec<AttendeeIn>,
    #[serde(default)]
    pub organizer: Option<AttendeeIn>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Set for a recurring-event instance (mirrors ICS RECURRENCE-ID).
    #[serde(default)]
    pub recurrence_id: Option<String>,
    #[serde(default)]
    pub all_day: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct AttendeeIn {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

impl AttendeeIn {
    fn person(&self) -> Option<(String, Vec<(String, String)>)> {
        let email = self
            .email
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty());
        let name = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from)
            .or_else(|| email.map(|e| e.split('@').next().unwrap_or(e).to_string()))?;
        let attrs = email
            .map(|e| vec![("email".to_string(), e.to_lowercase())])
            .unwrap_or_default();
        Some((name, attrs))
    }
}

/// Mirror of the ics adapter's draft shape (same body preamble, same
/// entity/edge structure) so live and imported events read identically.
pub(crate) fn calendar_in_to_episode(ev: &CalendarEventIn) -> anyhow::Result<EpisodeDraft> {
    let uncertainty = if ev.all_day {
        TimeUncertainty::Day
    } else {
        TimeUncertainty::Minute
    };
    let event_time = EventTime::parse_rfc3339(&ev.start, uncertainty)?;
    let source_ref = match ev.recurrence_id.as_deref().filter(|r| !r.is_empty()) {
        Some(rid) => format!("{}#{rid}", ev.uid),
        None => ev.uid.clone(),
    };
    // Title and venue are one-line fields an invite's sender wrote (the
    // ics adapter's rule): a line break in them must not start a line.
    let summary = one_line(&ev.title);
    let mut body = format!("event: {summary}\n");
    if let Some(location) = ev
        .location
        .as_deref()
        .map(one_line)
        .filter(|l| !l.is_empty())
    {
        body.push_str(&format!("location: {location}\n"));
    }
    if let Some(utc) = &event_time.utc {
        body.push_str(&format!("start: {utc}\n"));
    }
    if let Some(end) = ev.end.as_deref() {
        if let Ok(t) = EventTime::parse_rfc3339(end, TimeUncertainty::Minute) {
            if let Some(utc) = t.utc {
                body.push_str(&format!("end: {utc}\n"));
            }
        }
    }
    let mut entities = vec![EntityRef {
        entity_type: "Event".into(),
        name: summary.clone(),
        attributes: vec![("ics-uid".into(), ev.uid.clone())],
        role: "subject".into(),
    }];
    let mut edges = Vec::new();
    if let Some((name, attrs)) = ev.organizer.as_ref().and_then(|o| o.person()) {
        body.push_str(&format!("organizer: {name}\n"));
        entities.push(EntityRef {
            entity_type: "Person".into(),
            name,
            attributes: attrs,
            role: "organizer".into(),
        });
        edges.push(EdgeAssertion {
            src: entities.len() - 1,
            relation: "organized".into(),
            dst: 0,
        });
    }
    let attendees: Vec<(String, Vec<(String, String)>)> =
        ev.attendees.iter().filter_map(|a| a.person()).collect();
    if !attendees.is_empty() {
        body.push_str("attendees:\n");
    }
    for (name, attrs) in attendees {
        body.push_str(&format!("  {name}\n"));
        entities.push(EntityRef {
            entity_type: "Person".into(),
            name,
            attributes: attrs,
            role: "attendee".into(),
        });
        edges.push(EdgeAssertion {
            src: entities.len() - 1,
            relation: "attended".into(),
            dst: 0,
        });
    }
    if let Some(notes) = ev.notes.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        body.push_str(&format!("\n{notes}\n"));
    }
    Ok(EpisodeDraft {
        source: "ics".into(),
        source_ref,
        kind: "calendar_event".into(),
        title: summary,
        body,
        event_time,
        trust: "self".into(),
        scope: Some("personal".into()),
        entities,
        edges,
    })
}

/* ————— contacts (POST /v1/ingest/contacts) ————— */

/// One stored card → its contact_card episode + Person/Org entities.
/// Attribute-first resolution (email/phone owns the Person) means a
/// contact whose email already appears in mail history resolves to THAT
/// Person and enriches it — the identity backbone at work. A changed
/// card re-mints under `<key>#r<n>` (the supersession pattern).
pub(crate) fn contact_to_episode(c: &PendingContact) -> EpisodeDraft {
    let name = c.name.trim().to_string();
    let mut body = format!("contact: {name}\n");
    let mut attrs: Vec<(String, String)> = Vec::new();
    for e in &c.emails {
        let e = e.trim().to_lowercase();
        if !e.is_empty() {
            body.push_str(&format!("email: {e}\n"));
            attrs.push(("email".into(), e));
        }
    }
    for p in &c.phones {
        let digits: String = p
            .chars()
            .filter(|ch| ch.is_ascii_digit() || *ch == '+')
            .collect();
        if !digits.is_empty() {
            body.push_str(&format!("phone: {p}\n"));
            attrs.push(("phone".into(), digits));
        }
    }
    if let Some(org) = c.org.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        body.push_str(&format!("org: {org}\n"));
    }
    if let Some(bd) = c.birthday.as_deref().filter(|b| !b.is_empty()) {
        body.push_str(&format!("birthday: {bd}\n"));
    }
    for addr in &c.postal {
        if !addr.trim().is_empty() {
            body.push_str(&format!("address: {}\n", addr.trim()));
        }
    }
    let mut entities = vec![EntityRef {
        entity_type: "Person".into(),
        name,
        attributes: attrs,
        role: "subject".into(),
    }];
    let mut edges = Vec::new();
    if let Some(org) = c.org.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        entities.push(EntityRef {
            entity_type: "Org".into(),
            name: org.to_string(),
            attributes: Vec::new(),
            role: "mentioned".into(),
        });
        edges.push(EdgeAssertion {
            src: 0,
            relation: "affiliated_with".into(),
            dst: 1,
        });
    }
    EpisodeDraft {
        source: "contacts".into(),
        source_ref: if c.revision <= 1 {
            c.key.clone()
        } else {
            format!("{}#r{}", c.key, c.revision)
        },
        kind: "contact_card".into(),
        title: format!("Contact \u{2014} {}", c.name.trim()),
        body,
        // an address book has no event time — the honest unknown
        event_time: EventTime {
            utc: None,
            tz_offset_minutes: None,
            uncertainty: TimeUncertainty::Unknown,
        },
        trust: "self".into(),
        scope: Some("personal".into()),
        entities,
        edges,
    }
}

/// Drain pending (new or changed) cards into episodes/entities, in
/// small chunks. Pure store+CPU — spawn_blocking'd by callers. Runs
/// from BOTH the ingest handler (immediate) and the consolidation pass
/// (drains peer-synced cards). Returns cards applied.
pub(crate) fn apply_pending_contacts(state: &AppState, cap: usize) -> anyhow::Result<usize> {
    let actor = Actor::new("adapter:contacts");
    let mut applied = 0usize;
    while applied < cap {
        let pending = state.store.contacts_pending_apply(100.min(cap - applied))?;
        if pending.is_empty() {
            break;
        }
        for chunk in pending.chunks(50) {
            let drafts: Vec<EpisodeDraft> = chunk.iter().map(contact_to_episode).collect();
            state.store.apply_batch(
                &state.home,
                &IngestBatch {
                    actor: actor.actor.clone(),
                    session_id: None,
                    episodes: drafts,
                },
            )?;
            for c in chunk {
                state.store.contacts_mark_applied(&c.key, c.revision)?;
            }
            applied += chunk.len();
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    Ok(applied)
}

/* ————— motion / trips (POST /v1/ingest/motion) ————— */

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct MotionIn {
    pub key: String,
    /// walking | running | cycling | automotive | stationary
    pub mode: String,
    pub start: String,
    pub end: String,
    #[serde(default)]
    pub distance_m: Option<f64>,
    #[serde(default)]
    pub steps: Option<i64>,
    #[serde(default)]
    pub tz_offset_minutes: i32,
}

pub(crate) fn motion_in_to_episode(m: &MotionIn, scope: &str) -> anyhow::Result<EpisodeDraft> {
    let mut event_time = EventTime::parse_rfc3339(&m.start, TimeUncertainty::Minute)?;
    if event_time.tz_offset_minutes.unwrap_or(0) == 0 && m.tz_offset_minutes != 0 {
        event_time.tz_offset_minutes = Some(m.tz_offset_minutes);
    }
    let minutes = chrono::DateTime::parse_from_rfc3339(&m.start)
        .ok()
        .zip(chrono::DateTime::parse_from_rfc3339(&m.end).ok())
        .map(|(s, e)| (e - s).num_seconds().max(0) / 60);
    let mode_label = match m.mode.as_str() {
        "automotive" => "Drive",
        "cycling" => "Ride",
        "running" => "Run",
        "walking" => "Walk",
        other => other,
    };
    let km = m.distance_m.map(|d| d / 1000.0);
    let title = match (km, minutes) {
        (Some(km), _) if km >= 0.1 => format!("{mode_label} \u{2014} {km:.1} km"),
        (_, Some(min)) => format!("{mode_label} \u{2014} {min} min"),
        _ => mode_label.to_string(),
    };
    let mut body = format!("mode: {}\nstart: {}\nend: {}\n", m.mode, m.start, m.end);
    if let Some(min) = minutes {
        body.push_str(&format!("duration: {min} min\n"));
    }
    if let Some(km) = km {
        body.push_str(&format!("distance: {km:.2} km\n"));
    }
    if let Some(steps) = m.steps {
        body.push_str(&format!("steps: {steps}\n"));
    }
    Ok(EpisodeDraft {
        source: "ios-motion".into(),
        source_ref: m.key.clone(),
        kind: "trip".into(),
        title,
        body,
        event_time,
        trust: "self".into(),
        scope: Some(scope.to_string()),
        entities: Vec::new(),
        edges: Vec::new(),
    })
}

/* ————— media plays (POST /v1/ingest/media) ————— */

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct MediaIn {
    pub key: String,
    /// song | podcast
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub show: Option<String>,
    pub played_at: String,
    #[serde(default)]
    pub duration_s: Option<i64>,
    #[serde(default)]
    pub album: Option<String>,
}

pub(crate) fn media_in_to_episode(m: &MediaIn) -> anyhow::Result<EpisodeDraft> {
    let event_time = EventTime::parse_rfc3339(&m.played_at, TimeUncertainty::Minute)?;
    let title = m.title.trim().to_string();
    let creator = m
        .artist
        .as_deref()
        .or(m.show.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut body = format!(
        "{}: {title}\n",
        if m.kind == "podcast" {
            "episode"
        } else {
            "track"
        }
    );
    let mut entities = Vec::new();
    let mut edges = Vec::new();
    // the played work itself
    let work_type = if m.kind == "podcast" { "Show" } else { "Track" };
    if m.kind == "podcast" {
        if let Some(show) = m.show.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            body.push_str(&format!("show: {show}\n"));
            entities.push(EntityRef {
                entity_type: work_type.into(),
                name: show.to_string(),
                attributes: Vec::new(),
                role: "subject".into(),
            });
        }
    } else {
        entities.push(EntityRef {
            entity_type: work_type.into(),
            name: title.clone(),
            attributes: Vec::new(),
            role: "subject".into(),
        });
        if let Some(artist) = m.artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            body.push_str(&format!("artist: {artist}\n"));
            entities.push(EntityRef {
                entity_type: "Artist".into(),
                name: artist.to_string(),
                attributes: Vec::new(),
                role: "mentioned".into(),
            });
            edges.push(EdgeAssertion {
                src: 1,
                relation: "by".into(),
                dst: 0,
            });
        }
        if let Some(album) = m.album.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            body.push_str(&format!("album: {album}\n"));
        }
    }
    body.push_str(&format!("played: {}\n", m.played_at));
    if let Some(d) = m.duration_s {
        body.push_str(&format!("duration: {}:{:02}\n", d / 60, d % 60));
    }
    let title_line = match creator {
        Some(c) => format!("\u{266a} {title} \u{2014} {c}"),
        None => format!("\u{266a} {title}"),
    };
    Ok(EpisodeDraft {
        source: "ios-media".into(),
        source_ref: m.key.clone(),
        kind: "media_play".into(),
        title: title_line,
        body,
        event_time,
        trust: "self".into(),
        scope: Some("personal".into()),
        entities,
        edges,
    })
}

/* ————— vision scans (POST /v1/ingest/scan) ————— */

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ScanIn {
    pub key: String,
    /// receipt | card | document
    pub kind: String,
    pub ocr_text: String,
    pub captured_at: String,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub tz_offset_minutes: i32,
}

/// Best-effort receipt total: the largest `$X.XX`-shaped amount on a
/// line mentioning total/amount/balance, else the largest overall.
pub(crate) fn parse_receipt_total(text: &str) -> Option<f64> {
    let amount_in = |line: &str| -> Vec<f64> {
        let mut out = Vec::new();
        let mut chars = line.char_indices().peekable();
        while let Some((i, ch)) = chars.next() {
            if ch == '$' || ch.is_ascii_digit() {
                let start = if ch == '$' { i + 1 } else { i };
                let rest: String = line[start..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
                    .collect();
                if rest.contains('.') {
                    if let Ok(v) = rest.replace(',', "").parse::<f64>() {
                        if (0.01..100_000.0).contains(&v) {
                            out.push(v);
                        }
                    }
                }
                while let Some((_, c)) = chars.peek() {
                    if c.is_ascii_digit() || *c == '.' || *c == ',' {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        }
        out
    };
    let mut keyed: Vec<f64> = Vec::new();
    let mut all: Vec<f64> = Vec::new();
    for line in text.lines() {
        let lower = line.to_lowercase();
        let vals = amount_in(line);
        if lower.contains("total") || lower.contains("amount") || lower.contains("balance") {
            keyed.extend(&vals);
        }
        all.extend(vals);
    }
    keyed
        .into_iter()
        .fold(None::<f64>, |m, v| Some(m.map_or(v, |m| m.max(v))))
        .or_else(|| {
            all.into_iter()
                .fold(None, |m, v| Some(m.map_or(v, |m: f64| m.max(v))))
        })
}

pub(crate) fn scan_in_to_episode(s: &ScanIn) -> anyhow::Result<EpisodeDraft> {
    let mut event_time = EventTime::parse_rfc3339(&s.captured_at, TimeUncertainty::Minute)?;
    if event_time.tz_offset_minutes.unwrap_or(0) == 0 && s.tz_offset_minutes != 0 {
        event_time.tz_offset_minutes = Some(s.tz_offset_minutes);
    }
    let kind = match s.kind.as_str() {
        "receipt" => "receipt",
        "card" => "business_card",
        _ => "document",
    };
    let first_line = s
        .ocr_text
        .lines()
        .map(str::trim)
        .find(|l| l.len() >= 3)
        .unwrap_or("scan");
    let label: String = first_line.chars().take(60).collect();
    let title = match kind {
        "receipt" => format!("Receipt \u{2014} {label}"),
        "business_card" => format!("Business card \u{2014} {label}"),
        _ => format!("Document \u{2014} {label}"),
    };
    let mut body = format!("scan: {kind}\ncaptured: {}\n", s.captured_at);
    if kind == "receipt" {
        if let Some(total) = parse_receipt_total(&s.ocr_text) {
            body.push_str(&format!("amount: {total:.2}\n"));
        }
    }
    if let (Some(lat), Some(lon)) = (s.lat, s.lon) {
        body.push_str(&format!("captured at: ({lat:.5},{lon:.5})\n"));
    }
    body.push('\n');
    body.push_str(&s.ocr_text);
    // Entity mining happens in the LLM extraction pass (ios-scan is
    // allowlisted): merchants → Org, card owners → Person. Structural
    // minting here would guess; extraction reads.
    Ok(EpisodeDraft {
        source: "ios-scan".into(),
        source_ref: s.key.clone(),
        kind: kind.into(),
        title,
        body,
        event_time,
        trust: "self".into(),
        scope: Some("personal".into()),
        entities: Vec::new(),
        edges: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calendar_converges_on_ics_refs() {
        let ev = CalendarEventIn {
            uid: "evt-123@example.com".into(),
            title: "Board meeting".into(),
            start: "2026-08-20T14:00:00-04:00".into(),
            end: Some("2026-08-20T15:00:00-04:00".into()),
            location: Some("HQ".into()),
            lat: None,
            lon: None,
            attendees: vec![AttendeeIn {
                name: Some("Ada Lovelace".into()),
                email: Some("Ada@Example.com".into()),
            }],
            organizer: None,
            notes: None,
            recurrence_id: None,
            all_day: false,
        };
        let d = calendar_in_to_episode(&ev).unwrap();
        assert_eq!(
            d.source, "ics",
            "live phone events dedupe against ics imports"
        );
        assert_eq!(d.source_ref, "evt-123@example.com");
        assert_eq!(d.kind, "calendar_event");
        assert_eq!(d.trust, "self");
        assert!(d.body.contains("location: HQ"));
        // attendee resolves attribute-first on the lowercased email
        assert_eq!(
            d.entities[1].attributes[0],
            ("email".into(), "ada@example.com".into())
        );
        // recurrence instances get their own ref, like ICS RECURRENCE-ID
        let inst = CalendarEventIn {
            recurrence_id: Some("20260827T140000".into()),
            ..ev
        };
        assert_eq!(
            calendar_in_to_episode(&inst).unwrap().source_ref,
            "evt-123@example.com#20260827T140000"
        );
    }

    /// A line break in a live event's title or venue never starts a line
    /// of the body (or of the chapter that later lists the title).
    #[test]
    fn test_calendar_title_and_location_stay_one_line() {
        let ev = CalendarEventIn {
            uid: "evt-9@example.com".into(),
            title: " Lunch\n- Wire the money\u{2028}".into(),
            start: "2026-08-20T12:00:00-04:00".into(),
            end: Some("2026-08-20T13:00:00-04:00".into()),
            location: Some("Cafe\r\n[12:00] Elsewhere (40.71,-74.00)".into()),
            lat: Some(42.36),
            lon: Some(-71.06),
            attendees: vec![],
            organizer: None,
            notes: Some("notes keep\ntheir lines".into()),
            recurrence_id: None,
            all_day: false,
        };
        let d = calendar_in_to_episode(&ev).unwrap();
        assert_eq!(d.title, "Lunch - Wire the money");
        assert_eq!(d.entities[0].name, "Lunch - Wire the money");
        assert!(
            d.body.starts_with(
                "event: Lunch - Wire the money\nlocation: Cafe [12:00] Elsewhere (40.71,-74.00)\n"
            ),
            "{}",
            d.body
        );
        assert!(
            !d.body
                .lines()
                .any(|l| l.starts_with('[') || l.starts_with("- ")),
            "{}",
            d.body
        );
        assert!(d.body.contains("notes keep\ntheir lines"));
    }

    #[test]
    fn test_contact_episode_revisions_and_attributes() {
        let c = PendingContact {
            key: "ABC-DEF".into(),
            name: "Grace Hopper".into(),
            emails: vec!["grace@example.org".into()],
            phones: vec!["+1 (617) 555-0100".into()],
            org: Some("US Navy".into()),
            birthday: Some("1906-12-09".into()),
            postal: vec![],
            revision: 1,
        };
        let d = contact_to_episode(&c);
        assert_eq!(d.source_ref, "ABC-DEF");
        assert_eq!(d.kind, "contact_card");
        assert_eq!(
            d.entities[0].attributes[0],
            ("email".into(), "grace@example.org".into())
        );
        assert_eq!(
            d.entities[0].attributes[1],
            ("phone".into(), "+16175550100".into())
        );
        assert_eq!(d.entities[1].entity_type, "Org");
        assert_eq!(d.edges[0].relation, "affiliated_with");
        let r2 = PendingContact { revision: 2, ..c };
        assert_eq!(
            contact_to_episode(&r2).source_ref,
            "ABC-DEF#r2",
            "changed card supersedes"
        );
    }

    #[test]
    fn test_motion_and_media_and_scan_shapes() {
        let m = MotionIn {
            key: "seg1".into(),
            mode: "cycling".into(),
            start: "2026-08-15T09:00:00Z".into(),
            end: "2026-08-15T09:40:00Z".into(),
            distance_m: Some(12_400.0),
            steps: None,
            tz_offset_minutes: -240,
        };
        let d = motion_in_to_episode(&m, "personal").unwrap();
        assert_eq!(d.kind, "trip");
        assert_eq!(d.title, "Ride \u{2014} 12.4 km");
        assert!(d.body.contains("duration: 40 min"));

        let song = MediaIn {
            key: "play1".into(),
            kind: "song".into(),
            title: "Blue in Green".into(),
            artist: Some("Miles Davis".into()),
            show: None,
            played_at: "2026-08-15T21:00:00Z".into(),
            duration_s: Some(337),
            album: Some("Kind of Blue".into()),
        };
        let d = media_in_to_episode(&song).unwrap();
        assert_eq!(d.kind, "media_play");
        assert!(d.title.contains("Miles Davis"));
        assert_eq!(d.entities[0].entity_type, "Track");
        assert_eq!(d.entities[1].entity_type, "Artist");

        let scan = ScanIn {
            key: "sc1".into(),
            kind: "receipt".into(),
            ocr_text: "BLUE BOTTLE COFFEE\nLatte 6.50\nTOTAL $14.25\n".into(),
            captured_at: "2026-08-15T10:00:00Z".into(),
            lat: Some(42.36),
            lon: Some(-71.06),
            tz_offset_minutes: 0,
        };
        let d = scan_in_to_episode(&scan).unwrap();
        assert_eq!(d.kind, "receipt");
        assert!(d.body.contains("amount: 14.25"), "{}", d.body);
        assert!(d.title.contains("BLUE BOTTLE"));
    }
}

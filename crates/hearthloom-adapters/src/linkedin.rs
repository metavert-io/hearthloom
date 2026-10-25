//! LinkedIn data-export adapter (the "Basic" archive of CSVs + article
//! HTML; built to absorb the later FULL export — messages, shares —
//! through the same idempotent source_refs). Structural only, like
//! facebook/mbox: no LLM extraction.
//!
//! What materializes (source `linkedin`, trust `imported`):
//!   li_position    — one per career position, dated at its start
//!   li_education   — schools, dated
//!   li_honor       — honors/awards
//!   li_publication — books/papers
//!   li_article     — articles you WROTE (HTML → text)
//!   li_event       — events you joined
//!   li_profile     — your headline/summary + emails as identity attrs
//!
//! The ~30k connections do NOT materialize: `connections()` feeds the
//! social_connections registry — with company/position/email per row,
//! the richest disambiguation any network gives us.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};

/// Minimal RFC-4180 CSV: quoted fields, embedded commas/newlines/"".
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_quotes = false,
                _ => field.push(c),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    if row.iter().any(|f| !f.is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    } else {
                        row.clear();
                    }
                }
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        if row.iter().any(|f| !f.is_empty()) {
            rows.push(row);
        }
    }
    rows
}

/// Header-mapped CSV read; returns (headers lowercased, data rows).
/// LinkedIn prefixes some files with a "Notes:" preamble — skip until a
/// row that looks like a header for `want` columns.
fn read_csv(path: &Path, want: &[&str]) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let rows = parse_csv(&text);
    let header_at = rows.iter().position(|r| {
        let lower: Vec<String> = r.iter().map(|c| c.trim().to_lowercase()).collect();
        want.iter().all(|w| lower.contains(&w.to_string()))
    });
    let Some(at) = header_at else {
        return Err(anyhow!("{}: no header row with {:?}", path.display(), want));
    };
    let headers: Vec<String> = rows[at].iter().map(|c| c.trim().to_lowercase()).collect();
    Ok((headers, rows[at + 1..].to_vec()))
}

fn col<'a>(headers: &[String], row: &'a [String], name: &str) -> &'a str {
    headers
        .iter()
        .position(|h| h == name)
        .and_then(|i| row.get(i))
        .map(|s| s.trim())
        .unwrap_or("")
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// LinkedIn dates come in several shapes: "10 Jul 2026", "Apr 12, 2011",
/// "Apr 2020", "2001". Returns (iso-ish date, uncertainty).
fn parse_date(s: &str) -> Option<(String, TimeUncertainty)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s
        .split([' ', ','])
        .filter(|p| !p.is_empty())
        .collect();
    let month_of = |t: &str| MONTHS.iter().position(|m| t.to_lowercase().starts_with(m));
    match parts.as_slice() {
        // 10 Jul 2026
        [d, m, y] if d.parse::<u32>().is_ok() && month_of(m).is_some() => Some((
            format!("{y}-{:02}-{:02}", month_of(m).unwrap() + 1, d.parse::<u32>().unwrap()),
            TimeUncertainty::Day,
        )),
        // Apr 12, 2011
        [m, d, y] if d.parse::<u32>().is_ok() && month_of(m).is_some() => Some((
            format!("{y}-{:02}-{:02}", month_of(m).unwrap() + 1, d.parse::<u32>().unwrap()),
            TimeUncertainty::Day,
        )),
        // Apr 2020
        [m, y] if month_of(m).is_some() && y.len() == 4 => Some((
            format!("{y}-{:02}-01", month_of(m).unwrap() + 1),
            TimeUncertainty::Month,
        )),
        // 2001
        [y] if y.len() == 4 && y.parse::<u32>().is_ok() => {
            Some((format!("{y}-01-01"), TimeUncertainty::Year))
        }
        _ => None,
    }
}

fn event_time(date: Option<(String, TimeUncertainty)>) -> EventTime {
    match date {
        Some((day, uncertainty)) => EventTime {
            utc: Some(format!("{day}T12:00:00Z")),
            tz_offset_minutes: None,
            uncertainty,
        },
        None => EventTime {
            utc: None,
            tz_offset_minutes: None,
            uncertainty: TimeUncertainty::Unknown,
        },
    }
}

/// The connections list → registry rows (name, dates, company/position/
/// email — never entities; see connections.rs).
pub struct LiConnection {
    pub name: String,
    pub connected_at: Option<String>,
    pub company: Option<String>,
    pub position: Option<String>,
    pub handle: Option<String>,
}

pub fn connections(root: &Path) -> Result<Vec<LiConnection>> {
    let path = root.join("Connections.csv");
    if !path.is_file() {
        return Ok(vec![]);
    }
    let (headers, rows) = read_csv(&path, &["first name", "last name", "connected on"])?;
    let mut out = Vec::new();
    for row in &rows {
        let first = col(&headers, row, "first name");
        let last = col(&headers, row, "last name");
        let name = format!("{first} {last}").trim().to_string();
        if name.is_empty() {
            continue;
        }
        let email = col(&headers, row, "email address");
        let url = col(&headers, row, "url");
        let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
        out.push(LiConnection {
            name,
            connected_at: parse_date(col(&headers, row, "connected on")).map(|(d, _)| d),
            company: non_empty(col(&headers, row, "company")),
            position: non_empty(col(&headers, row, "position")),
            handle: non_empty(email).or_else(|| non_empty(url)),
        });
    }
    Ok(out)
}

/// Crude HTML → text: drop tags/scripts, unescape the basics.
fn html_to_text(html: &str) -> (String, String) {
    let title = html
        .split("<title>")
        .nth(1)
        .and_then(|r| r.split("</title>").next())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut text = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut skip_until: Option<&str> = None;
    let mut rest = html;
    while !rest.is_empty() {
        if let Some(until) = skip_until {
            match rest.find(until) {
                Some(at) => {
                    rest = &rest[at + until.len()..];
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        let c = rest.chars().next().unwrap();
        if !in_tag && rest.starts_with("<script") {
            skip_until = Some("</script>");
            continue;
        }
        if !in_tag && rest.starts_with("<style") {
            skip_until = Some("</style>");
            continue;
        }
        match c {
            '<' => {
                in_tag = true;
                // block-level closers become newlines
                if rest.starts_with("</p>") || rest.starts_with("<br") || rest.starts_with("</h") {
                    text.push('\n');
                }
            }
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
        rest = &rest[c.len_utf8()..];
    }
    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let cleaned: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (title, cleaned)
}

pub fn ingest(root: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    if !root.join("Connections.csv").is_file() && !root.join("Profile.csv").is_file() {
        return Err(anyhow!(
            "{} does not look like an extracted LinkedIn export",
            root.display()
        ));
    }
    let mut drafts = Vec::new();
    let mk = |source_ref: String, kind: &str, title: String, body: String,
              time: EventTime, entities: Vec<EntityRef>| EpisodeDraft {
        source: "linkedin".into(),
        source_ref,
        kind: kind.into(),
        title,
        body,
        event_time: time,
        trust: "imported".into(),
        scope: scope.map(String::from),
        entities,
        edges: vec![],
    };

    // — positions (the career spine, dated at start) —
    if let Ok((h, rows)) = read_csv(&root.join("Positions.csv"), &["company name", "title"]) {
        for row in &rows {
            let company = col(&h, row, "company name").to_string();
            let title = col(&h, row, "title").to_string();
            if company.is_empty() && title.is_empty() {
                continue;
            }
            let started = col(&h, row, "started on");
            let finished = col(&h, row, "finished on");
            let description = col(&h, row, "description");
            let location = col(&h, row, "location");
            let mut body = format!("position: {title}\ncompany: {company}\n");
            if !location.is_empty() {
                body.push_str(&format!("location: {location}\n"));
            }
            let span_end = if finished.is_empty() { "present".to_string() } else { finished.to_string() };
            body.push_str(&format!("span: {started} → {span_end}\n"));
            if !description.is_empty() {
                body.push_str(&format!("\n{description}\n"));
            }
            let entities = if company.is_empty() {
                vec![]
            } else {
                vec![EntityRef {
                    entity_type: "Org".into(),
                    name: company.clone(),
                    attributes: vec![],
                    role: "employer".into(),
                }]
            };
            drafts.push(mk(
                format!("position:{company}:{started}"),
                "li_position",
                format!("{title} at {company}"),
                body,
                event_time(parse_date(started)),
                entities,
            ));
        }
    }

    // — education / honors / publications (light, dated where possible) —
    if let Ok((h, rows)) = read_csv(&root.join("Education.csv"), &["school name"]) {
        for row in &rows {
            let school = col(&h, row, "school name").to_string();
            if school.is_empty() {
                continue;
            }
            let start = col(&h, row, "start date");
            let notes = col(&h, row, "notes");
            let degree = col(&h, row, "degree name");
            let mut body = format!("school: {school}\n");
            if !degree.is_empty() {
                body.push_str(&format!("degree: {degree}\n"));
            }
            if !notes.is_empty() {
                body.push_str(&format!("\n{notes}\n"));
            }
            drafts.push(mk(
                format!("education:{school}:{start}"),
                "li_education",
                format!("Studied at {school}"),
                body,
                event_time(parse_date(start)),
                vec![EntityRef {
                    entity_type: "Org".into(),
                    name: school,
                    attributes: vec![],
                    role: "school".into(),
                }],
            ));
        }
    }
    if let Ok((h, rows)) = read_csv(&root.join("Honors.csv"), &["title"]) {
        for row in &rows {
            let title = col(&h, row, "title").to_string();
            if title.is_empty() {
                continue;
            }
            let issued = col(&h, row, "issued on");
            drafts.push(mk(
                format!("honor:{title}:{issued}"),
                "li_honor",
                format!("Honor — {title}"),
                format!("{title}\n\n{}\n", col(&h, row, "description")),
                event_time(parse_date(issued)),
                vec![],
            ));
        }
    }
    if let Ok((h, rows)) = read_csv(&root.join("Publications.csv"), &["name"]) {
        for row in &rows {
            let name = col(&h, row, "name").to_string();
            if name.is_empty() {
                continue;
            }
            let published = col(&h, row, "published on");
            let publisher = col(&h, row, "publisher");
            drafts.push(mk(
                format!("publication:{name}:{published}"),
                "li_publication",
                format!("Published — {name}"),
                format!("{name}\npublisher: {publisher}\n\n{}\n", col(&h, row, "description")),
                event_time(parse_date(published)),
                vec![],
            ));
        }
    }

    // — events —
    if let Ok((h, rows)) = read_csv(&root.join("Events.csv"), &["event name", "event time"]) {
        for row in &rows {
            let name = col(&h, row, "event name").to_string();
            if name.is_empty() {
                continue;
            }
            // "Aug 13, 2020 01:00 PM - …" → date words lead
            let time_raw = col(&h, row, "event time");
            let date = parse_date(&time_raw.split_whitespace().take(3).collect::<Vec<_>>().join(" "));
            drafts.push(mk(
                format!("event:{name}:{time_raw}"),
                "li_event",
                format!("Event — {name}"),
                format!("event: {name}\nwhen: {time_raw}\nstatus: {}\n", col(&h, row, "status")),
                event_time(date),
                vec![],
            ));
        }
    }

    // — articles you wrote (HTML → text; date from filename when present) —
    let articles_dir = root.join("Articles/Articles");
    if articles_dir.is_dir() {
        for entry in std::fs::read_dir(&articles_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let html = std::fs::read_to_string(&path)?;
            let (title, mut text) = html_to_text(&html);
            text.truncate(48 * 1024);
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("article");
            let date = stem
                .get(0..10)
                .filter(|d| d.as_bytes().get(4) == Some(&b'-'))
                .map(|d| (d.to_string(), TimeUncertainty::Day));
            let display = if title.is_empty() { stem.replace('-', " ") } else { title };
            drafts.push(mk(
                format!("article:{stem}"),
                "li_article",
                format!("Article — {display}"),
                text,
                event_time(date),
                vec![],
            ));
        }
    }

    // — profile: headline/summary + emails as identity attributes —
    if let Ok((h, rows)) = read_csv(&root.join("Profile.csv"), &["first name", "last name"]) {
        if let Some(row) = rows.first() {
            let name = format!(
                "{} {}",
                col(&h, row, "first name"),
                col(&h, row, "last name")
            )
            .trim()
            .to_string();
            let mut attrs = Vec::new();
            if let Ok((eh, erows)) = read_csv(&root.join("Email Addresses.csv"), &["email address"]) {
                for erow in &erows {
                    let email = col(&eh, erow, "email address");
                    if !email.is_empty() {
                        attrs.push(("email".to_string(), email.to_lowercase()));
                    }
                }
            }
            let headline = col(&h, row, "headline");
            let summary = col(&h, row, "summary");
            drafts.push(mk(
                "profile".into(),
                "li_profile",
                format!("LinkedIn profile — {name}"),
                format!("headline: {headline}\n\n{summary}\n"),
                event_time(None),
                vec![EntityRef {
                    entity_type: "Person".into(),
                    name,
                    attributes: attrs,
                    role: "subject".into(),
                }],
            ));
        }
    }

    Ok(drafts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_csv_quotes_and_dates() {
        let rows = parse_csv("a,\"b, with comma\",\"quoted \"\"x\"\"\"\nc,d,e\n");
        assert_eq!(rows[0], vec!["a", "b, with comma", "quoted \"x\""]);
        assert_eq!(rows.len(), 2);
        assert_eq!(parse_date("10 Jul 2026").unwrap().0, "2026-07-10");
        assert_eq!(parse_date("Apr 12, 2011").unwrap().0, "2011-04-12");
        assert_eq!(parse_date("Apr 2020").unwrap().0, "2020-04-01");
        assert_eq!(parse_date("2001").unwrap().0, "2001-01-01");
        assert!(parse_date("").is_none());
    }

    #[test]
    fn test_fixture_export() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("Connections.csv"),
            "Notes:\n\"Some preamble about emails\"\n\nFirst Name,Last Name,URL,Email Address,Company,Position,Connected On\nPriya,Nair,https://linkedin.com/in/priyanair,priya.nair@ledger.example,Ledger & Co,Accountant,10 Jul 2026\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Positions.csv"),
            "Company Name,Title,Description,Location,Started On,Finished On\nHarbor Labs LLC,CPO,\"Tools for small teams.\",Portland,Apr 2020,\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Profile.csv"),
            "First Name,Last Name,Maiden Name,Address,Birth Date,Headline,Summary\nJon,Hale,,,Jan 2,Example headline,Example summary\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Email Addresses.csv"),
            "Email Address,Confirmed,Primary,Updated On\njon+li@example.com,Yes,No,\"1/2/21, 3:04 AM\"\n",
        )
        .unwrap();

        let conns = connections(root).unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].name, "Priya Nair");
        assert_eq!(conns[0].company.as_deref(), Some("Ledger & Co"));
        assert_eq!(conns[0].position.as_deref(), Some("Accountant"));
        assert_eq!(conns[0].handle.as_deref(), Some("priya.nair@ledger.example"));
        assert_eq!(conns[0].connected_at.as_deref(), Some("2026-07-10"));

        let drafts = ingest(root, Some("work")).unwrap();
        let position = drafts.iter().find(|d| d.kind == "li_position").unwrap();
        assert_eq!(position.title, "CPO at Harbor Labs LLC");
        assert_eq!(position.event_time.utc.as_deref(), Some("2020-04-01T12:00:00Z"));
        assert!(position.entities.iter().any(|e| e.name == "Harbor Labs LLC" && e.entity_type == "Org"));
        let profile = drafts.iter().find(|d| d.kind == "li_profile").unwrap();
        assert!(profile.entities[0].attributes.iter().any(|(k, v)| k == "email" && v == "jon+li@example.com"));
    }
}

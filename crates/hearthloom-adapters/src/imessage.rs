//! iMessage adapter: macOS `~/Library/Messages/chat.db` (local SQLite,
//! read-only). Messages are bundled into one episode per chat per day —
//! the granularity day-view recall wants — with participants as Person
//! entities keyed by phone/email handle.
//!
//! Reading someone else's SQLite file does not violate the sole-writer
//! law: that law is about *our* substrate. chat.db is opened read-only.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use rusqlite::{Connection, OpenFlags};

const BODY_CAP: usize = 64 * 1024;
/// Apple epoch (2001-01-01T00:00:00Z) as Unix seconds.
const APPLE_EPOCH: i64 = 978_307_200;

pub fn ingest(db_path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "opening {} read-only (on macOS this needs Full Disk Access for your terminal)",
            db_path.display()
        )
    })?;

    let mut stmt = conn.prepare(
        "SELECT m.date, m.text, m.is_from_me,
                COALESCE(h.id, '') AS handle,
                COALESCE(c.chat_identifier, COALESCE(h.id, 'unknown')) AS chat,
                COALESCE(c.display_name, '') AS chat_name
         FROM message m
         LEFT JOIN handle h ON h.ROWID = m.handle_id
         LEFT JOIN chat_message_join cmj ON cmj.message_id = m.ROWID
         LEFT JOIN chat c ON c.ROWID = cmj.chat_id
         WHERE m.text IS NOT NULL AND m.text != ''
         ORDER BY m.date",
    )?;

    struct Row {
        at_unix: i64,
        text: String,
        from_me: bool,
        handle: String,
        chat: String,
        chat_name: String,
    }
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok(Row {
                at_unix: apple_to_unix(row.get::<_, i64>(0)?),
                text: row.get(1)?,
                from_me: row.get::<_, i64>(2)? != 0,
                handle: row.get(3)?,
                chat: row.get(4)?,
                chat_name: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        return Err(anyhow!("no text messages found in {}", db_path.display()));
    }

    // Bundle: (chat, UTC date) → messages. chat.db stores UTC; the local
    // offset is unknown, so day bundling is by UTC date with hour-level
    // uncertainty on the episode rather than a fabricated offset.
    let mut buckets: BTreeMap<(String, String), Vec<Row>> = BTreeMap::new();
    for row in rows {
        let Some(dt) = DateTime::from_timestamp(row.at_unix, 0) else {
            continue;
        };
        let date = dt.format("%Y-%m-%d").to_string();
        buckets
            .entry((row.chat.clone(), date))
            .or_default()
            .push(row);
    }

    let mut drafts = Vec::new();
    for ((chat, date), messages) in buckets {
        let first = &messages[0];
        let chat_label = if first.chat_name.is_empty() {
            chat.clone()
        } else {
            first.chat_name.clone()
        };
        let title = format!("Messages with {chat_label}");

        let mut body = format!("chat: {chat_label}\ndate: {date}\n\n");
        let mut participants: BTreeMap<String, ()> = BTreeMap::new();
        for message in &messages {
            let Some(dt) = DateTime::from_timestamp(message.at_unix, 0) else {
                continue;
            };
            let sender = if message.from_me {
                "me".to_string()
            } else {
                participants.insert(message.handle.clone(), ());
                message.handle.clone()
            };
            let line = format!(
                "[{}] {sender}: {}\n",
                dt.format("%H:%M"),
                message.text.trim()
            );
            if body.len() + line.len() > BODY_CAP {
                body.push_str("[truncated]\n");
                break;
            }
            body.push_str(&line);
        }

        let entities = participants
            .keys()
            .filter(|handle| !handle.is_empty())
            .map(|handle| {
                let kind = if handle.contains('@') { "email" } else { "phone" };
                EntityRef {
                    entity_type: "Person".into(),
                    name: handle.clone(),
                    attributes: vec![(kind.into(), handle.to_lowercase())],
                    role: "participant".into(),
                }
            })
            .collect();

        let event_time = EventTime {
            utc: DateTime::from_timestamp(first.at_unix, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            tz_offset_minutes: None,
            uncertainty: TimeUncertainty::Hour,
        };

        drafts.push(EpisodeDraft {
            source: "imessage".into(),
            source_ref: format!("{chat}#{date}"),
            kind: "messages".into(),
            title,
            body,
            event_time,
            trust: "self".into(),
            scope: scope.map(String::from),
            entities,
            edges: vec![],
        });
    }
    Ok(drafts)
}

/// chat.db dates are nanoseconds (modern) or seconds (ancient) since the
/// Apple epoch.
fn apple_to_unix(apple: i64) -> i64 {
    let seconds = if apple > 10_000_000_000 {
        apple / 1_000_000_000
    } else {
        apple
    };
    seconds + APPLE_EPOCH
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, date INTEGER, text TEXT,
                 is_from_me INTEGER, handle_id INTEGER);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, display_name TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             INSERT INTO handle VALUES (1, '+15551234567');
             INSERT INTO chat VALUES (1, '+15551234567', '');
             -- 2026-03-15 ~14:00 UTC in Apple nanoseconds:
             INSERT INTO message VALUES (1, 795362400000000000, 'lunch?', 0, 1);
             INSERT INTO message VALUES (2, 795362460000000000, 'yes! noon', 1, 1);
             -- Next day, same chat: separate episode.
             INSERT INTO message VALUES (3, 795448800000000000, 'that was fun', 0, 1);
             INSERT INTO chat_message_join VALUES (1, 1), (1, 2), (1, 3);",
        )
        .unwrap();
    }

    #[test]
    fn test_day_bundling() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("chat.db");
        fixture_db(&db);

        let drafts = ingest(&db, None).unwrap();
        assert_eq!(drafts.len(), 2, "two days → two episodes");
        let first = &drafts[0];
        assert_eq!(first.kind, "messages");
        assert!(first.source_ref.starts_with("+15551234567#"));
        assert!(first.body.contains("+15551234567: lunch?"));
        assert!(first.body.contains("me: yes! noon"));
        assert_eq!(first.entities.len(), 1);
        assert_eq!(first.entities[0].attributes[0].0, "phone");
        // UTC bundling is honest about the unknown local offset.
        assert_eq!(first.event_time.tz_offset_minutes, None);
        assert_eq!(first.event_time.uncertainty, TimeUncertainty::Hour);
    }

    #[test]
    fn test_apple_epoch_conversion() {
        // 0 Apple seconds = 2001-01-01.
        assert_eq!(apple_to_unix(0), APPLE_EPOCH);
        // Nanosecond form collapses to the same instant.
        assert_eq!(apple_to_unix(5_000_000_000_000), APPLE_EPOCH + 5000);
    }
}

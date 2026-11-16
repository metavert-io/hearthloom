//! iMessage adapter: macOS `~/Library/Messages/chat.db` (local SQLite,
//! read-only). Messages are bundled into one episode per chat per day —
//! the granularity day-view recall wants — with participants as Person
//! entities keyed by phone/email handle.
//!
//! Reading someone else's SQLite file does not violate the sole-writer
//! law: that law is about *our* substrate. chat.db is opened read-only.
//!
//! Messages are imported only when the owner asks (nothing reads them in
//! the background), so every import reads every day, today's included. A
//! later import that finds more messages for a stored day lands as a
//! fuller copy (the store's growing-source revisions): each day's text
//! opens with how many messages it holds (`messages: <n>`), and a day too
//! long to keep whole keeps its first and its latest messages.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use rusqlite::{Connection, OpenFlags};

const BODY_CAP: usize = 64 * 1024;
/// Apple epoch (2001-01-01T00:00:00Z) as Unix seconds.
const APPLE_EPOCH: i64 = 978_307_200;

/// The text of a message's `attributedBody`: an NSAttributedString
/// archived as a typedstream. Messages keep their text only there when
/// `text` is empty. Its string follows the NSString class name, after the
/// `+` type tag: a length (one byte below 0x80, or 0x81 + u16 LE, or
/// 0x82 + u32 LE), then that many UTF-8 bytes.
fn attributed_text(blob: &[u8]) -> Option<String> {
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let class = find(blob, b"NSString")? + b"NSString".len();
    let after = blob.get(class..)?;
    // class version and object header, then the `+` tag
    let tag = find(after.get(..after.len().min(16))?, &[0x84, 0x01, b'+'])? + 3;
    let rest = after.get(tag..)?;
    let (len, start): (usize, usize) = match *rest.first()? {
        0x81 => (
            u16::from_le_bytes(rest.get(1..3)?.try_into().ok()?) as usize,
            3,
        ),
        0x82 => (
            u32::from_le_bytes(rest.get(1..5)?.try_into().ok()?) as usize,
            5,
        ),
        n if n < 0x80 => (n as usize, 1),
        _ => return None,
    };
    let bytes = rest.get(start..start.checked_add(len)?)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Every chat-day of a Messages database (the live one, or a copy).
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

    // Many messages keep their text only in `attributedBody` (with `text`
    // empty); a database without that column still reads its `text`.
    let has_body: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('message') WHERE name = 'attributedBody')",
        [],
        |r| r.get(0),
    )?;
    let (body_col, body_filter) = if has_body {
        ("m.attributedBody", " OR m.attributedBody IS NOT NULL")
    } else {
        ("NULL", "")
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT m.date, m.text, m.is_from_me,
                COALESCE(h.id, '') AS handle,
                COALESCE(c.chat_identifier, COALESCE(h.id, 'unknown')) AS chat,
                COALESCE(c.display_name, '') AS chat_name,
                {body_col}
         FROM message m
         LEFT JOIN handle h ON h.ROWID = m.handle_id
         LEFT JOIN chat_message_join cmj ON cmj.message_id = m.ROWID
         LEFT JOIN chat c ON c.ROWID = cmj.chat_id
         WHERE (m.text IS NOT NULL AND m.text != ''){body_filter}
         ORDER BY m.date"
    ))?;

    struct Row {
        at_unix: i64,
        text: String,
        from_me: bool,
        handle: String,
        chat: String,
        chat_name: String,
    }
    let mut unreadable = 0usize;
    let mut rows: Vec<Row> = Vec::new();
    let mut found = stmt.query([])?;
    while let Some(row) = found.next()? {
        let text = match row.get::<_, Option<String>>(1)?.filter(|t| !t.is_empty()) {
            Some(text) => text,
            None => match row
                .get::<_, Option<Vec<u8>>>(6)?
                .and_then(|blob| attributed_text(&blob))
            {
                Some(text) if !text.trim().is_empty() => text,
                Some(_) => continue, // an empty body: nothing was said
                None => {
                    unreadable += 1;
                    continue;
                }
            },
        };
        rows.push(Row {
            at_unix: apple_to_unix(row.get::<_, i64>(0)?),
            text,
            from_me: row.get::<_, i64>(2)? != 0,
            handle: row.get(3)?,
            chat: row.get(4)?,
            chat_name: row.get(5)?,
        });
    }
    if unreadable > 0 {
        tracing::info!(
            "imessage: {unreadable} messages in {} had a body this reader couldn't decode; left out",
            db_path.display()
        );
    }
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

        let mut participants: BTreeMap<String, ()> = BTreeMap::new();
        let mut lines = Vec::with_capacity(messages.len());
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
            lines.push(format!(
                "[{}] {sender}: {}\n",
                dt.format("%H:%M"),
                message.text.trim()
            ));
        }
        // The count comes first, ahead of the chat's name (anyone in a
        // group can set that): a later import of the day is fuller when it
        // holds more, even where the text below can't show every message.
        let body = crate::fit_entries(
            format!(
                "messages: {}\nchat: {chat_label}\ndate: {date}\n\n",
                lines.len()
            ),
            &lines,
            BODY_CAP,
            |n| format!("[{n} messages not shown]\n"),
        );

        let entities = participants
            .keys()
            .filter(|handle| !handle.is_empty())
            .map(|handle| {
                let kind = if handle.contains('@') {
                    "email"
                } else {
                    "phone"
                };
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

    /// An NSAttributedString archived the way chat.db stores it, holding
    /// `text`; `reference` is the class-reference byte after NSString.
    fn archived(text: &str, reference: u8) -> Vec<u8> {
        let mut blob = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01".to_vec();
        blob.extend([reference, 0x84, 0x01, b'+']);
        let bytes = text.as_bytes();
        if bytes.len() < 0x80 {
            blob.push(bytes.len() as u8);
        } else {
            blob.push(0x81);
            blob.extend((bytes.len() as u16).to_le_bytes());
        }
        blob.extend(bytes);
        blob.extend(b"\x86\x84\x02iI\x01\x05\x92\x84\x84\x84\x0cNSDictionary\x00\x94\x84\x01i\x01\x92\x84\x96\x96\x1d__kIMMessagePartAttributeName\x86\x86");
        blob
    }

    #[test]
    fn test_attributed_body_text() {
        assert_eq!(
            attributed_text(&archived("lunch?", 0x94)).unwrap(),
            "lunch?"
        );
        assert_eq!(
            attributed_text(&archived("café ☕", 0x95)).unwrap(),
            "café ☕"
        );
        let long = "a long message ".repeat(20);
        assert_eq!(attributed_text(&archived(&long, 0x94)).unwrap(), long);
        // cut short, or no string in it at all: nothing
        let blob = archived(&long, 0x94);
        assert_eq!(attributed_text(&blob[..blob.len() - 300]), None);
        assert_eq!(attributed_text(b"\x04\x0bstreamtyped\x84\x01@"), None);
    }

    /// Messages whose text is only in `attributedBody` are read; a body
    /// that can't be decoded is left out without failing the import.
    #[test]
    fn test_messages_with_text_only_in_the_attributed_body() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("chat.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, date INTEGER, text TEXT,
                 is_from_me INTEGER, handle_id INTEGER, attributedBody BLOB);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, display_name TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             INSERT INTO handle VALUES (1, '+15551234567');
             INSERT INTO chat VALUES (1, '+15551234567', '');
             INSERT INTO chat_message_join VALUES (1, 1), (1, 2), (1, 3), (1, 4);",
        )
        .unwrap();
        let add = |id: i64, at: i64, text: Option<&str>, body: Option<Vec<u8>>| {
            conn.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, 0, 1, ?4)",
                rusqlite::params![id, at, text, body],
            )
            .unwrap();
        };
        // 2026-03-15 ~14:00 UTC in Apple nanoseconds
        add(1, 795362400000000000, Some("plain text"), None);
        add(
            2,
            795362460000000000,
            None,
            Some(archived("only in the body", 0x94)),
        );
        add(
            3,
            795362520000000000,
            Some(""),
            Some(archived("empty text too", 0x94)),
        );
        add(
            4,
            795362580000000000,
            None,
            Some(b"not an archive".to_vec()),
        );

        let drafts = ingest(&db, None).unwrap();
        assert_eq!(drafts.len(), 1);
        let body = &drafts[0].body;
        for said in ["plain text", "only in the body", "empty text too"] {
            assert!(body.contains(&format!("+15551234567: {said}")), "{body}");
        }
        assert_eq!(body.lines().filter(|l| l.starts_with('[')).count(), 3);
    }

    /// An import reads every day, today's included, and each day's text
    /// says how many messages it holds.
    #[test]
    fn test_every_day_is_read_with_its_count() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("chat.db");
        fixture_db(&db);
        let now_apple = (chrono::Utc::now().timestamp() - APPLE_EPOCH) * 1_000_000_000;
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "INSERT INTO message VALUES (4, {now_apple}, 'sent just now', 1, 1);
                 INSERT INTO chat_message_join VALUES (1, 4);"
            ))
            .unwrap();
        let drafts = ingest(&db, None).unwrap();
        assert_eq!(drafts.len(), 3);
        assert!(drafts.iter().any(|d| d.body.contains("sent just now")));
        let counts: Vec<u64> = drafts
            .iter()
            .map(|d| hearthloom_core::store::growing_items("imessage", &d.body).unwrap())
            .collect();
        assert_eq!(counts, [2, 1, 1]);
        assert!(drafts[0].body.starts_with("messages: 2\nchat: "));
    }

    /// A day too long to keep whole keeps its first and its latest
    /// messages, and still counts every one — so a message recovered early
    /// in the day, or one added at its end, makes a fuller copy.
    #[test]
    fn test_a_long_day_keeps_its_latest_messages_and_counts_them_all() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("chat.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, date INTEGER, text TEXT,
                 is_from_me INTEGER, handle_id INTEGER);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, display_name TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             INSERT INTO handle VALUES (1, '+15551234567');
             INSERT INTO chat VALUES (1, '+15551234567', '');",
        )
        .unwrap();
        let add = |id: i64, text: &str| {
            // one second apart from 2026-03-15 ~14:00 UTC
            conn.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, 0, 1)",
                rusqlite::params![id, 795362400000000000 + id * 1_000_000_000, text],
            )
            .unwrap();
            conn.execute("INSERT INTO chat_message_join VALUES (1, ?1)", [id])
                .unwrap();
        };
        let words = "x".repeat(400);
        for id in 1..=300 {
            add(id, &format!("message {id} {words}"));
        }
        let count = |d: &EpisodeDraft| hearthloom_core::store::growing_items("imessage", &d.body);
        let before = ingest(&db, None).unwrap();
        assert!(before[0].body.len() <= BODY_CAP);
        assert_eq!(count(&before[0]), Some(300));
        assert!(before[0].body.contains("message 1 "));
        assert!(before[0].body.contains("message 300 "));
        assert!(before[0].body.contains("messages not shown]"));

        add(301, "the latest one");
        let after = ingest(&db, None).unwrap();
        assert_eq!(count(&after[0]), Some(301));
        assert!(after[0].body.contains("the latest one"));
    }

    #[test]
    fn test_apple_epoch_conversion() {
        // 0 Apple seconds = 2001-01-01.
        assert_eq!(apple_to_unix(0), APPLE_EPOCH);
        // Nanosecond form collapses to the same instant.
        assert_eq!(apple_to_unix(5_000_000_000_000), APPLE_EPOCH + 5000);
    }
}

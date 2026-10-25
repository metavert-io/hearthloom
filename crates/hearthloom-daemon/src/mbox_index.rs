//! Sidecar index for mbox virtual sources: makes a Takeout-scale mail
//! archive searchable at need without materializing it (the Tier-2 half
//! of the materialize/virtualize split — the substrate holds only what
//! was worth keeping; the rest stays at origin, query-at-need).
//!
//! The index is a *separate* SQLite file next to the archive
//! (`<archive>.vidx.sqlite`) — design law #4 (the daemon is the sole
//! writer of hearthloom.db) is untouched: this file is derived, holds
//! no substrate state, and can be rebuilt from the archive at any time.
//! Header fields only (from/subject/labels/date) go into FTS — bodies
//! stay in the archive and are read by byte offset on fetch.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;

/// Only the head of each message is scanned for headers; a header block
/// bigger than this (giant recipient lists) is indexed truncated.
const HEADER_SCAN_CAP: usize = 32 * 1024;
/// Insert batch per transaction while building.
const BUILD_TX_BATCH: usize = 5000;

pub fn index_path(mbox: &Path) -> PathBuf {
    let mut name = mbox
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
    name.push_str(".vidx.sqlite");
    mbox.with_file_name(name)
}

/// True when there is no index, or the archive changed since it was
/// built (size+mtime fingerprint).
pub fn is_stale(mbox: &Path) -> bool {
    let idx = index_path(mbox);
    let Ok(conn) = Connection::open_with_flags(
        &idx,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return true;
    };
    let stored: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='fingerprint'", [], |r| {
            r.get(0)
        })
        .ok();
    stored.as_deref() != fingerprint(mbox).ok().as_deref()
}

fn fingerprint(mbox: &Path) -> Result<String> {
    let meta = std::fs::metadata(mbox)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(format!("{}:{}", meta.len(), mtime))
}

struct HeaderFields {
    message_id: Option<String>,
    date: String,
    from: String,
    to: String,
    subject: String,
    labels: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Slot {
    None,
    Date,
    From,
    To,
    Subject,
    Labels,
}

fn slot_of(fields: &mut HeaderFields, slot: Slot) -> Option<&mut String> {
    match slot {
        Slot::Date => Some(&mut fields.date),
        Slot::From => Some(&mut fields.from),
        Slot::To => Some(&mut fields.to),
        Slot::Subject => Some(&mut fields.subject),
        Slot::Labels => Some(&mut fields.labels),
        Slot::None => None,
    }
}

/// Minimal header scan over a message's head bytes (RFC 5322 folding).
/// The FIRST occurrence of each header wins, as in the mbox adapter's
/// readers: Takeout and the live pull both write their `X-Gmail-Labels`
/// at the top, so a sender's own copy lower down (or a second From or
/// Message-ID) never overrides what vsearch shows.
fn scan_headers(head: &str) -> HeaderFields {
    let mut fields = HeaderFields {
        message_id: None,
        date: String::new(),
        from: String::new(),
        to: String::new(),
        subject: String::new(),
        labels: String::new(),
    };
    let mut seen: Vec<Slot> = Vec::new();
    let mut last = Slot::None;
    for line in head.lines() {
        if line.trim().is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(s) = slot_of(&mut fields, last) {
                s.push(' ');
                s.push_str(line.trim());
            }
            continue;
        }
        last = Slot::None;
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let slot = match name.to_ascii_lowercase().as_str() {
            "date" => Slot::Date,
            "from" => Slot::From,
            "to" => Slot::To,
            "subject" => Slot::Subject,
            "x-gmail-labels" => Slot::Labels,
            "message-id" => {
                if fields.message_id.is_none() {
                    fields.message_id =
                        Some(value.trim_matches(|c| c == '<' || c == '>').to_string());
                }
                continue;
            }
            _ => continue,
        };
        // A repeat is skipped whole — its folded lines too (last stays None).
        if seen.contains(&slot) {
            continue;
        }
        seen.push(slot);
        last = slot;
        if let Some(s) = slot_of(&mut fields, slot) {
            *s = value.to_string();
        }
    }
    fields
}

/// Build (or rebuild) the sidecar index by streaming the archive once.
/// Writes to a temp file and renames over the old index, so a crashed
/// or concurrent build never leaves a half-written index in place.
/// Non-fingerprint meta rows (the live pull's IMAP cursor) survive a
/// rebuild — losing them only costs a re-anchored backfill, but there's
/// no reason to pay that on every rebuild.
pub fn build(mbox: &Path) -> Result<usize> {
    let final_path = index_path(mbox);
    let tmp_path = final_path.with_extension("sqlite.building");
    let _ = std::fs::remove_file(&tmp_path);
    let conn = Connection::open(&tmp_path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
         CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE messages(
             id TEXT PRIMARY KEY,
             offset INTEGER NOT NULL,
             length INTEGER NOT NULL,
             date TEXT, from_addr TEXT, subject TEXT, labels TEXT
         );
         CREATE VIRTUAL TABLE messages_fts USING fts5(
             id UNINDEXED, subject, from_addr, to_addr, labels, date
         );",
    )?;
    if final_path.exists() {
        let _ = carry_meta(&final_path, &conn);
    }

    let file = std::fs::File::open(mbox).with_context(|| format!("opening {}", mbox.display()))?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let count = index_stream(&conn, &mut reader, 0, mbox)?;

    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('fingerprint', ?1)",
        [fingerprint(mbox)?],
    )?;
    drop(conn);
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("installing index at {}", final_path.display()))?;
    Ok(count)
}

/// Best-effort copy of non-fingerprint meta from a previous index.
fn carry_meta(old_index: &Path, conn: &Connection) -> Result<()> {
    let old = Connection::open_with_flags(
        old_index,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut stmt = old.prepare("SELECT key, value FROM meta WHERE key != 'fingerprint'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows.flatten() {
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            rusqlite::params![row.0, row.1],
        )?;
    }
    Ok(())
}

/// Index every message found in `reader`, whose first byte sits at
/// `base_offset` of the archive file. A caller starting mid-file must
/// start at a message boundary (the live pull appends whole records
/// after a trailing blank line, so its old-length offset is one).
fn index_stream(
    conn: &Connection,
    reader: &mut impl BufRead,
    base_offset: u64,
    mbox: &Path,
) -> Result<usize> {
    let mut line = Vec::new();
    let mut offset: u64 = base_offset; // byte offset of the current line
    let mut prev_blank = true;
    // (start offset, header head) of the message being scanned.
    let mut current: Option<(u64, String)> = None;
    let mut in_headers = false;
    let mut count = 0usize;
    let mut pending: Vec<(String, u64, u64, HeaderFields)> = Vec::new();

    let flush = |conn: &Connection, pending: &mut Vec<(String, u64, u64, HeaderFields)>| -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        conn.execute_batch("BEGIN")?;
        for (id, start, len, f) in pending.drain(..) {
            // Message-IDs are not actually unique across a real archive
            // (self-copies, list blasts): the first occurrence keeps the
            // id, later duplicates fall back to their byte offset.
            let changed = conn.execute(
                "INSERT OR IGNORE INTO messages(id, offset, length, date, from_addr, subject, labels)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, start as i64, len as i64, f.date, f.from, f.subject, f.labels],
            )?;
            let id = if changed == 0 {
                let fallback = format!("at:{start}");
                conn.execute(
                    "INSERT INTO messages(id, offset, length, date, from_addr, subject, labels)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![fallback, start as i64, len as i64, f.date, f.from, f.subject, f.labels],
                )?;
                fallback
            } else {
                id
            };
            conn.execute(
                "INSERT INTO messages_fts(id, subject, from_addr, to_addr, labels, date)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, f.subject, f.from, f.to, f.labels, f.date],
            )?;
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    };

    let finish_message =
        |current: &mut Option<(u64, String)>, end: u64, pending: &mut Vec<_>, count: &mut usize| {
            if let Some((start, head)) = current.take() {
                let fields = scan_headers(&head);
                // Message-ID when unique enough; a duplicate or missing id
                // falls back to the (stable, unique) byte offset.
                let id = match fields.message_id.clone() {
                    Some(id) if !id.is_empty() => id,
                    _ => format!("at:{start}"),
                };
                pending.push((id, start, end - start, fields));
                *count += 1;
            }
        };

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        let is_sep = prev_blank && line.starts_with(b"From ");
        if is_sep {
            finish_message(&mut current, offset, &mut pending, &mut count);
            if pending.len() >= BUILD_TX_BATCH {
                flush(conn, &mut pending)?;
                if count % 50_000 < BUILD_TX_BATCH {
                    tracing::info!("mbox index {}: {count} messages indexed", mbox.display());
                }
            }
            current = Some((offset + read as u64, String::new()));
            in_headers = true;
        } else if in_headers {
            if let Some((_, head)) = current.as_mut() {
                if line.iter().all(|b| b.is_ascii_whitespace()) || head.len() > HEADER_SCAN_CAP {
                    in_headers = false;
                } else {
                    head.push_str(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']));
                    head.push('\n');
                }
            }
        }
        prev_blank = line.iter().all(|b| b.is_ascii_whitespace());
        offset += read as u64;
    }
    finish_message(&mut current, offset, &mut pending, &mut count);
    flush(conn, &mut pending)?;
    Ok(count)
}

/// Incrementally index messages appended at/after `from_offset` and
/// refresh the fingerprint — so a live-pull append never forces a
/// Takeout-scale rebuild. The caller guarantees the file is append-only
/// (bytes before `from_offset` unchanged) and that `from_offset` sits
/// on a record boundary. Missing index → full build. A crash between
/// the row inserts and the fingerprint stamp leaves the index stale,
/// which the next register/build heals — never a wrong answer.
pub fn append_index(mbox: &Path, from_offset: u64) -> Result<usize> {
    let idx = index_path(mbox);
    if !idx.exists() {
        return build(mbox);
    }
    let conn = Connection::open(&idx)?;
    let file = std::fs::File::open(mbox).with_context(|| format!("opening {}", mbox.display()))?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    reader.seek(SeekFrom::Start(from_offset))?;
    let count = index_stream(&conn, &mut reader, from_offset, mbox)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('fingerprint', ?1)",
        [fingerprint(mbox)?],
    )?;
    Ok(count)
}

/// Read a non-fingerprint meta value (the live pull's IMAP cursor).
pub fn meta_get(mbox: &Path, key: &str) -> Result<Option<String>> {
    let idx = index_path(mbox);
    if !idx.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&idx, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .ok())
}

pub fn meta_set(mbox: &Path, key: &str, value: &str) -> Result<()> {
    let conn = Connection::open(index_path(mbox))?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// One search hit: the message id (usable as a vfetch/promote path) and
/// a `date | from | subject | labels` snippet line.
pub fn search(mbox: &Path, query: &str, limit: usize) -> Result<Vec<(String, String)>> {
    if is_stale(mbox) {
        return Err(anyhow!(
            "the index for {} is missing or stale — re-register the source to rebuild it",
            mbox.display()
        ));
    }
    let conn = Connection::open_with_flags(
        index_path(mbox),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    // Quote each term so FTS5 treats them as plain tokens (implicit AND).
    let fts_query = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT f.id, m.date, m.from_addr, m.subject, m.labels
         FROM messages_fts f JOIN messages m ON m.id = f.id
         WHERE messages_fts MATCH ?1
         ORDER BY rank LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
        let id: String = row.get(0)?;
        let date: String = row.get(1)?;
        let from: String = row.get(2)?;
        let subject: String = row.get(3)?;
        let labels: String = row.get(4)?;
        Ok((id, format!("{date} | {from} | {subject} | {labels}")))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Read one message's raw text (headers + body) by its indexed offset.
pub fn fetch(mbox: &Path, id: &str, cap_chars: usize) -> Result<String> {
    if is_stale(mbox) {
        return Err(anyhow!(
            "the index for {} is missing or stale — re-register the source to rebuild it",
            mbox.display()
        ));
    }
    let conn = Connection::open_with_flags(
        index_path(mbox),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (offset, length): (i64, i64) = conn
        .query_row(
            "SELECT offset, length FROM messages WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| anyhow!("no message {id:?} in the index"))?;
    let mut file = std::fs::File::open(mbox)?;
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).chars().take(cap_chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "From a@x Mon Jan 05 10:00:00 2026\n\
From: Ann Lee <ann@x.example>\n\
To: Jon <jon@y.example>\n\
Subject: dinner plans in Paris\n\
Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
Message-ID: <paris1@x>\n\
X-Gmail-Labels: Inbox,Category Personal\n\
\n\
Let's meet at the bistro.\n\
\n\
From b@x Mon Jan 05 11:00:00 2026\n\
From: Bob <bob@z.example>\n\
Subject: quarterly report\n\
Date: Mon, 05 Jan 2026 11:00:00 -0400\n\
X-Gmail-Labels: Archived,Category Updates,\n\
 Unread\n\
\n\
Numbers attached.\n";

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mail.mbox");
        std::fs::write(&path, SAMPLE).unwrap();
        (tmp, path)
    }

    #[test]
    fn test_build_search_fetch_roundtrip() {
        let (_tmp, mbox) = fixture();
        assert!(is_stale(&mbox), "no index yet");
        let count = build(&mbox).unwrap();
        assert_eq!(count, 2);
        assert!(!is_stale(&mbox));

        let hits = search(&mbox, "paris", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "paris1@x");
        assert!(hits[0].1.contains("dinner plans in Paris"));
        assert!(hits[0].1.contains("Category Personal"));

        // Folded labels are searchable; missing Message-ID falls back to
        // a stable offset id.
        let hits = search(&mbox, "updates quarterly", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].0.starts_with("at:"));

        let raw = fetch(&mbox, "paris1@x", 100_000).unwrap();
        assert!(raw.starts_with("From: Ann Lee"));
        assert!(raw.contains("Let's meet at the bistro."));
        // The next message's separator is not included.
        assert!(!raw.contains("quarterly"));
    }

    #[test]
    fn test_duplicate_message_ids_fall_back_to_offset() {
        // Real archives repeat Message-IDs (self-copies, list blasts) —
        // the 2026-07 Takeout had 11. First keeps the id; dup gets at:<offset>.
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("mail.mbox");
        let msg = "From: Ann <ann@x.example>\n\
                   Subject: twice-sent\n\
                   Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
                   Message-ID: <dup@x>\n\
                   \n\
                   same id twice\n";
        std::fs::write(
            &mbox,
            format!("From a@x Mon Jan 05 10:00:00 2026\n{msg}\nFrom a@x Mon Jan 05 10:01:00 2026\n{msg}"),
        )
        .unwrap();
        let count = build(&mbox).unwrap();
        assert_eq!(count, 2);
        let hits = search(&mbox, "twice-sent", 10).unwrap();
        assert_eq!(hits.len(), 2);
        let ids: Vec<&str> = hits.iter().map(|h| h.0.as_str()).collect();
        assert!(ids.contains(&"dup@x"));
        assert!(ids.iter().any(|i| i.starts_with("at:")), "got: {ids:?}");
    }

    #[test]
    fn test_first_header_wins_over_a_senders_repeat() {
        // Gmail's header sits on top; a sender repeats labels, From and
        // Message-ID lower down (one folded) to dress up the vsearch hit.
        let head = "X-Gmail-Labels: Category Promotions\n\
                    From: Shop <deals@shop.example>\n\
                    Subject: SALE\n\
                    Message-ID: <real@shop.example>\n\
                    X-Gmail-Labels: Starred,\n Sent\n\
                    From: Your Bank <security@bank.example>\n\
                    Message-ID: <paris1@x>\n";
        let fields = scan_headers(head);
        assert_eq!(fields.labels, "Category Promotions");
        assert_eq!(fields.from, "Shop <deals@shop.example>");
        assert_eq!(fields.message_id.as_deref(), Some("real@shop.example"));
        // Folded continuations still extend the FIRST occurrence.
        let fields = scan_headers("X-Gmail-Labels: Inbox,\n Unread\nX-Gmail-Labels: Sent\n");
        assert_eq!(fields.labels, "Inbox, Unread");
        // An empty first header (the live pull's, for unlabeled mail) holds.
        assert_eq!(scan_headers("X-Gmail-Labels:\nX-Gmail-Labels: Starred\n").labels, "");

        // End to end: the hit shows the first labels and keeps the real id.
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("mail.mbox");
        std::fs::write(&mbox, format!("From x@y Mon Jan 05 10:00:00 2026\n{head}\npromo body\n")).unwrap();
        build(&mbox).unwrap();
        let hits = search(&mbox, "sale", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "real@shop.example");
        assert!(hits[0].1.contains("Category Promotions"), "got: {}", hits[0].1);
        assert!(!hits[0].1.contains("Starred") && !hits[0].1.contains("bank"), "got: {}", hits[0].1);
    }

    #[test]
    fn test_stale_after_archive_change() {
        let (_tmp, mbox) = fixture();
        build(&mbox).unwrap();
        assert!(!is_stale(&mbox));
        let mut grown = std::fs::read_to_string(&mbox).unwrap();
        grown.push_str("\nFrom c@x Mon Jan 05 12:00:00 2026\nSubject: new\nDate: x\n\nhi\n");
        std::fs::write(&mbox, grown).unwrap();
        assert!(is_stale(&mbox), "size change must invalidate");
        let err = search(&mbox, "paris", 5).unwrap_err().to_string();
        assert!(err.contains("stale"), "got: {err}");
    }
}

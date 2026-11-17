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
//! stay in the archive and are read by byte offset on fetch. Two things
//! besides the archive ride along through a rebuild: the meta rows (the
//! live pull's cursor, and its list of messages still to observe) and
//! `label_updates`, the labels the live pull read again from Gmail after a
//! message was archived. A rebuild that can't read them leaves the old
//! file in place (`carry_over`). The labels come from Gmail, and the pull
//! re-reads every message's labels in time, so a file deleted or damaged
//! past reading loses no label for good — only those messages on the list
//! whose labels didn't change (their observation failed on arrival).

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OptionalExtension};

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
    let Ok(conn) = Connection::open_with_flags(&idx, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
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
/// Message-ID) never overrides what vsearch shows. The Message-ID is read
/// by the mail import's own rule (`mbox::message_id_ref`, folded lines
/// and all), so a message is indexed under the key its memory has and the
/// one the live pull reads from Gmail when it re-reads labels.
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
    fields.message_id = hearthloom_adapters::mbox::message_id_ref(head);
    fields
}

/// The meta row naming how an index keys its messages: `IDS_RULE`, the
/// mail import's key (`scan_headers`). An index built before it has none:
/// it read a folded Message-ID as missing and keyed that message by its
/// offset, where nothing that knows the message's Message-ID finds it.
const IDS_KEY: &str = "ids";
const IDS_RULE: &str = "import-key";

/// Whether this index keys its messages by the mail import's rule. One
/// built before doesn't (`IDS_KEY`); rebuilding it does.
pub fn ids_current(mbox: &Path) -> bool {
    meta_get(mbox, IDS_KEY).ok().flatten().as_deref() == Some(IDS_RULE)
}

/// One writer of an archive's index at a time. A rebuild copies the meta
/// rows and label updates of the index it replaces, then takes its place:
/// a write to that index while the rebuild ran would be lost with it. So a
/// rebuild holds this for its whole run, and so does every write it
/// carries over (meta rows, label updates) or reads the archive for
/// (appended records). Readers don't take it, and neither does the text
/// pass (a rebuilt index starts its texts over).
static WRITERS: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>> = Mutex::new(BTreeMap::new());

fn writer(mbox: &Path) -> Arc<Mutex<()>> {
    // one lock per file, however its path was written
    let key = std::fs::canonicalize(mbox).unwrap_or_else(|_| mbox.to_path_buf());
    WRITERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(key)
        .or_default()
        .clone()
}

fn hold(lock: &Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    lock.lock().unwrap_or_else(|p| p.into_inner())
}

/// Labels the live pull re-read from Gmail after a message was archived
/// (`set_labels`): the archive keeps each message's labels as received,
/// so these are kept beside them and win over them, across rebuilds too.
const LABEL_UPDATES: &str =
    "CREATE TABLE IF NOT EXISTS label_updates(id TEXT PRIMARY KEY, labels TEXT NOT NULL)";

/// Build (or rebuild) the sidecar index by streaming the archive once.
/// Writes to a temp file and renames over the old index, so a crashed
/// build never leaves a half-written index in place; a build waits for
/// the index's other writes, and they for it (`writer`).
/// Non-fingerprint meta rows (the live pull's IMAP cursor and its list of
/// messages still to observe) survive a rebuild, and so do label updates;
/// an old index that can't be read now fails the build and stays in place
/// (`carry_over`).
pub fn build(mbox: &Path) -> Result<usize> {
    let lock = writer(mbox);
    let _one_writer = hold(&lock);
    build_held(mbox)
}

/// `build`, with the index's writer lock held.
fn build_held(mbox: &Path) -> Result<usize> {
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
    conn.execute(LABEL_UPDATES, [])?;
    let mut updates = std::collections::HashMap::new();
    if final_path.exists() {
        updates = carry_over(&final_path, &conn)?;
    }

    let file = std::fs::File::open(mbox).with_context(|| format!("opening {}", mbox.display()))?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let count = index_stream(&conn, &mut reader, 0, mbox, &updates)?;

    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('fingerprint', ?1)",
        [fingerprint(mbox)?],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        [IDS_KEY, IDS_RULE],
    )?;
    drop(conn);
    #[cfg(test)]
    before_install(mbox);
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("installing index at {}", final_path.display()))?;
    Ok(count)
}

/// A test's hook, run once by the next build of this archive just before
/// it puts its new index in place.
#[cfg(test)]
type InstallHook = (PathBuf, Box<dyn FnOnce() + Send>);
#[cfg(test)]
static BEFORE_INSTALL: Mutex<Vec<InstallHook>> = Mutex::new(Vec::new());

#[cfg(test)]
fn before_install(mbox: &Path) {
    let hook = {
        let mut hooks = BEFORE_INSTALL.lock().unwrap_or_else(|p| p.into_inner());
        hooks
            .iter()
            .position(|(path, _)| path == mbox)
            .map(|at| hooks.remove(at).1)
    };
    if let Some(hook) = hook {
        hook();
    }
}

/// What a rebuild keeps of the index it replaces: its meta rows (all but
/// the fingerprint and the text watermark) and its label updates, copied
/// into the new one; the label updates are returned (id → labels) for the
/// build to index under. The meta rows hold work not yet done (the pull's
/// list of messages still to observe, its cursor), so an old index that
/// can't be read now — busy, locked, out of reach — fails the build and
/// stays in place for the next one. Only a file that is no database at
/// all, or a damaged one, is replaced without what can't be read: nothing
/// could ever read it. Meta rows that can't be read take the label updates
/// with them (labels whose list is gone would never be observed — without
/// both, the pull reads the labels from Gmail again and finds the changes
/// anew); label updates that can't be read go alone — the meta rows read
/// are kept, and the pull finds those labels again.
fn carry_over(
    old_index: &Path,
    conn: &Connection,
) -> Result<std::collections::HashMap<String, String>> {
    let (meta, updates) = match read_carried(old_index) {
        Ok((meta, Some(updates))) => (meta, updates),
        Ok((meta, None)) => {
            tracing::warn!(
                "mbox index {}: the label updates of the index being replaced are damaged: rebuilt with its meta rows, without them",
                old_index.display()
            );
            (meta, Vec::new())
        }
        Err(err) if unreadable_for_good(&err) => {
            tracing::warn!(
                "mbox index {}: the index being replaced is unreadable ({err:#}): rebuilt without its meta rows and label updates",
                old_index.display()
            );
            return Ok(std::collections::HashMap::new());
        }
        Err(err) => {
            return Err(err.context(format!(
                "reading {} before rebuilding it (it stays as it is)",
                old_index.display()
            )))
        }
    };
    for (key, value) in &meta {
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
    }
    let mut out = std::collections::HashMap::new();
    for (id, labels) in updates {
        conn.execute(
            "INSERT OR REPLACE INTO label_updates(id, labels) VALUES (?1, ?2)",
            rusqlite::params![id, labels],
        )?;
        out.insert(id, labels);
    }
    Ok(out)
}

type CarriedMeta = Vec<(rusqlite::types::Value, rusqlite::types::Value)>;
/// Label updates carried: (message id, labels).
type CarriedLabels = Vec<(String, String)>;

/// How long a rebuild waits for the index it replaces to be free.
const CARRY_BUSY: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(200)
} else {
    std::time::Duration::from_secs(10)
};

/// The old index's meta rows and label updates, read in one transaction;
/// a table it doesn't have holds nothing to carry. Label updates that are
/// damaged (`unreadable_for_good`) come back None — the meta rows read
/// stand. Opened for writing (never created): a write that stopped
/// half-way left a journal only such a connection rolls back, and a
/// read-only one fails on every try.
fn read_carried(old_index: &Path) -> Result<(CarriedMeta, Option<CarriedLabels>)> {
    let old = Connection::open_with_flags(old_index, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    old.busy_timeout(CARRY_BUSY)?;
    let tx = old.unchecked_transaction()?;
    let has = |table: &str| -> rusqlite::Result<bool> {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |r| r.get(0),
        )
    };
    let mut meta = Vec::new();
    if has("meta")? {
        // the text watermark belongs to the old file's texts: a rebuild
        // starts its text pass from the beginning
        let mut stmt = tx.prepare(
            "SELECT key, value FROM meta WHERE key != 'fingerprint' AND key != 'text_rowid'",
        )?;
        meta = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
    }
    let updates = || -> Result<CarriedLabels> {
        if !has("label_updates")? {
            return Ok(Vec::new());
        }
        let mut stmt = tx.prepare("SELECT id, labels FROM label_updates")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    };
    match updates() {
        Ok(updates) => Ok((meta, Some(updates))),
        Err(err) if unreadable_for_good(&err) => Ok((meta, None)),
        Err(err) => Err(err),
    }
}

/// An error saying the file is no SQLite database, or a damaged one: no
/// later try reads it either.
fn unreadable_for_good(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
                )
        )
    })
}

/// Index every message found in `reader`, whose first byte sits at
/// `base_offset` of the archive file. A caller starting mid-file must
/// start at a message boundary (the live pull appends whole records
/// after a trailing blank line, so its old-length offset is one). A
/// message indexed under its Message-ID shows the labels `updates` holds
/// for that id, if any, instead of its archived ones.
fn index_stream(
    conn: &Connection,
    reader: &mut impl BufRead,
    base_offset: u64,
    mbox: &Path,
    updates: &std::collections::HashMap<String, String>,
) -> Result<usize> {
    let mut line = Vec::new();
    let mut offset: u64 = base_offset; // byte offset of the current line
    let mut prev_blank = true;
    // (start offset, header head) of the message being scanned.
    let mut current: Option<(u64, String)> = None;
    let mut in_headers = false;
    let mut count = 0usize;
    let mut pending: Vec<(String, u64, u64, HeaderFields)> = Vec::new();

    let flush = |conn: &Connection,
                 pending: &mut Vec<(String, u64, u64, HeaderFields)>|
     -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        conn.execute_batch("BEGIN")?;
        for (id, start, len, mut f) in pending.drain(..) {
            // Message-IDs are not actually unique across a real archive
            // (self-copies, list blasts): the first occurrence keeps the
            // id, later duplicates fall back to their byte offset.
            let archived = f.labels.clone();
            if let Some(now) = updates.get(&id) {
                f.labels = now.clone();
            }
            let changed = conn.execute(
                "INSERT OR IGNORE INTO messages(id, offset, length, date, from_addr, subject, labels)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, start as i64, len as i64, f.date, f.from, f.subject, f.labels],
            )?;
            if changed == 0 {
                f.labels = archived; // a later copy: its own labels
            }
            let id = if changed == 0 {
                let fallback = format!("at:{start}");
                conn.execute(
                    "INSERT INTO messages(id, offset, length, date, from_addr, subject, labels)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        fallback,
                        start as i64,
                        len as i64,
                        f.date,
                        f.from,
                        f.subject,
                        f.labels
                    ],
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
    let lock = writer(mbox);
    let _one_writer = hold(&lock);
    let idx = index_path(mbox);
    if !idx.exists() {
        return build_held(mbox);
    }
    let conn = Connection::open(&idx)?;
    let file = std::fs::File::open(mbox).with_context(|| format!("opening {}", mbox.display()))?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    reader.seek(SeekFrom::Start(from_offset))?;
    // appended mail carries the labels Gmail gave it just now
    let count = index_stream(
        &conn,
        &mut reader,
        from_offset,
        mbox,
        &std::collections::HashMap::new(),
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('fingerprint', ?1)",
        [fingerprint(mbox)?],
    )?;
    Ok(count)
}

/// Read a non-fingerprint meta value (the live pull's IMAP cursor). None:
/// no index, or no such key; an index that can't be read is an error.
pub fn meta_get(mbox: &Path, key: &str) -> Result<Option<String>> {
    let idx = index_path(mbox);
    if !idx.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&idx, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .map_err(Into::into)
}

pub fn meta_set(mbox: &Path, key: &str, value: &str) -> Result<()> {
    let lock = writer(mbox);
    let _one_writer = hold(&lock);
    let conn = Connection::open(index_path(mbox))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// The labels the index shows for the message with this id (its
/// `X-Gmail-Labels` as archived, or as last updated by `set_labels`).
/// None: no such message; an index that can't be read is an error.
pub fn labels_of(mbox: &Path, id: &str) -> Result<Option<String>> {
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(conn
        .query_row("SELECT labels FROM messages WHERE id = ?1", [id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .map(Option::unwrap_or_default))
}

/// Show these labels (id → labels) in search: the live pull's label
/// re-read (Gmail's labels change after a message is archived). The
/// archive's own bytes stay as received; the update is kept in the index
/// (`label_updates`) and a rebuild indexes under it again. These `meta`
/// rows (key, value) are set in the same transaction: all are in, or none.
pub fn set_labels(mbox: &Path, updates: &[(String, String)], meta: &[(&str, &str)]) -> Result<()> {
    if updates.is_empty() && meta.is_empty() {
        return Ok(());
    }
    let lock = writer(mbox);
    let _one_writer = hold(&lock);
    let mut conn = Connection::open(index_path(mbox))?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute(LABEL_UPDATES, [])?;
    let tx = conn.transaction()?;
    for (key, value) in meta {
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
    }
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS relabel(id TEXT PRIMARY KEY, labels TEXT NOT NULL);
         DELETE FROM temp.relabel;",
    )?;
    for (id, labels) in updates {
        tx.execute(
            "INSERT OR REPLACE INTO temp.relabel(id, labels) VALUES (?1, ?2)",
            rusqlite::params![id, labels],
        )?;
    }
    // one pass over the text index (its id column has no index of its own)
    tx.execute_batch(
        "UPDATE messages SET labels = (SELECT labels FROM temp.relabel r WHERE r.id = messages.id)
             WHERE id IN (SELECT id FROM temp.relabel);
         UPDATE messages_fts SET labels = (SELECT labels FROM temp.relabel r WHERE r.id = messages_fts.id)
             WHERE id IN (SELECT id FROM temp.relabel);
         INSERT OR REPLACE INTO label_updates(id, labels) SELECT id, labels FROM temp.relabel;
         DELETE FROM temp.relabel;",
    )?;
    tx.commit()?;
    Ok(())
}

/// One archive record's place: its index row, its id (the Message-ID, or
/// `at:<offset>` for a later copy or a message without one) and its `From `
/// separator line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Separator {
    pub row: i64,
    pub id: String,
    pub line: String,
}

/// The separator of each record after index row `after_row` (rows run in
/// archive order), at most `limit`. The live pull writes each message's UID
/// on its separator line (`gmail_imap::record_uid`).
pub fn separators_after(mbox: &Path, after_row: i64, limit: usize) -> Result<Vec<Separator>> {
    if is_stale(mbox) {
        return Err(anyhow!(
            "the index for {} is missing or stale — re-register the source to rebuild it",
            mbox.display()
        ));
    }
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT rowid, id, offset FROM messages WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![after_row, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut file = std::fs::File::open(mbox)?;
    let mut out = Vec::with_capacity(rows.len());
    for (row, id, offset) in rows {
        // the record's text starts at `offset`, right after its separator
        let end = offset.max(0) as u64;
        let start = end.saturating_sub(256);
        let mut buf = vec![0u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
        let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
        let line = match line.iter().rposition(|b| *b == b'\n') {
            Some(at) => &line[at + 1..],
            None => line,
        };
        out.push(Separator {
            row,
            id,
            line: String::from_utf8_lossy(line).into_owned(),
        });
    }
    Ok(out)
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
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
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
    let mut hits = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    // the message text and its links (0.5.87, filled in the background):
    // after the header hits, with the words that matched
    let has_text: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'texts_fts')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if has_text && hits.len() < limit {
        let mut stmt = conn.prepare(
            "SELECT t.id, m.date, m.from_addr, m.subject, m.labels,
                    snippet(texts_fts, -1, '«', '»', '…', 14)
             FROM texts_fts t JOIN messages m ON m.id = t.id
             WHERE texts_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
            let id: String = row.get(0)?;
            let date: String = row.get(1)?;
            let from: String = row.get(2)?;
            let subject: String = row.get(3)?;
            let labels: String = row.get(4)?;
            let snippet: String = row.get(5)?;
            Ok((
                id,
                format!("{date} | {from} | {subject} | {labels} | text: {snippet}"),
            ))
        })?;
        for row in rows {
            let (id, line) = row?;
            if hits.len() >= limit {
                break;
            }
            if !hits.iter().any(|(seen, _)| seen == &id) {
                hits.push((id, line));
            }
        }
    }
    Ok(hits)
}

/// What an archive holds, for Ask's instructions: how many messages and
/// the months they span. Parsing every date is too slow for the start of a
/// question (665k in the Takeout), so it is worked out in the background
/// and kept until the index changes; until then (or if the index can't be
/// read) this says nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub messages: usize,
    /// YYYY-MM of the oldest and newest dated message
    pub first: Option<String>,
    pub last: Option<String>,
}

type Summaries = std::collections::HashMap<PathBuf, (String, Option<Summary>)>;
static SUMMARIES: std::sync::Mutex<Option<Summaries>> = std::sync::Mutex::new(None);

/// The summary changes only when the archive does (the text pass writes
/// to the index all the while; keying on the index would recount
/// hundreds of thousands of dates after every batch).
fn index_fingerprint(mbox: &Path) -> Option<String> {
    let _ = std::fs::metadata(index_path(mbox)).ok()?; // no index, no summary
    let meta = std::fs::metadata(mbox).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("{}:{mtime}", meta.len()))
}

pub fn summary(mbox: &Path) -> Option<Summary> {
    let fingerprint = index_fingerprint(mbox)?;
    let mut guard = SUMMARIES.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(Default::default);
    let known = map.get(mbox).cloned();
    if known.as_ref().is_some_and(|(f, _)| f == &fingerprint) {
        return known.and_then(|(_, s)| s);
    }
    // mark it in progress (keeping the last summary meanwhile), then count
    // off the runtime: a thread of its own, so a slow disk stalls nothing
    let last = known.and_then(|(_, s)| s);
    map.insert(mbox.to_path_buf(), (fingerprint.clone(), last.clone()));
    let path = mbox.to_path_buf();
    std::thread::spawn(move || {
        let computed = compute_summary(&path).ok();
        let mut guard = SUMMARIES.lock().unwrap_or_else(|p| p.into_inner());
        let map = guard.get_or_insert_with(Default::default);
        map.insert(path, (fingerprint, computed));
    });
    last
}

pub fn compute_summary(mbox: &Path) -> Result<Summary> {
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT date FROM messages")?;
    let mut rows = stmt.query([])?;
    let (mut messages, mut first, mut last) = (0usize, None::<String>, None::<String>);
    while let Some(row) = rows.next()? {
        messages += 1;
        let date: String = row.get(0)?;
        let Ok(when) = chrono::DateTime::parse_from_rfc2822(date.trim()) else {
            continue;
        };
        let month = when.format("%Y-%m").to_string();
        // a bad clock or a spammer's date shouldn't stretch the span
        if month.as_str() < "1990" || month.as_str() > "2100" {
            continue;
        }
        if first.as_ref().is_none_or(|f| &month < f) {
            first = Some(month.clone());
        }
        if last.as_ref().is_none_or(|l| &month > l) {
            last = Some(month);
        }
    }
    Ok(Summary {
        messages,
        first,
        last,
    })
}

/// Messages dated since `since`, oldest first: (id, labels) — the labels
/// are the pull's own header (a sender's forged one is stripped).
pub fn since(mbox: &Path, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<(String, String)>> {
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT id, date, labels FROM messages")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out: Vec<(chrono::DateTime<chrono::Utc>, String, String)> = Vec::new();
    for row in rows {
        let (id, date, labels) = row?;
        let Some(when) = date
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_rfc2822(d.trim()).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
        else {
            continue;
        };
        if when >= since {
            out.push((when, id, labels.unwrap_or_default()));
        }
    }
    out.sort_by_key(|(when, _, _)| *when);
    Ok(out
        .into_iter()
        .map(|(_, id, labels)| (id, labels))
        .collect())
}

/// Read one message's raw text (headers + body) by its indexed offset.
pub fn fetch(mbox: &Path, id: &str, cap_chars: usize) -> Result<String> {
    Ok(String::from_utf8_lossy(&fetch_bytes(mbox, id)?)
        .chars()
        .take(cap_chars)
        .collect())
}

/// One message's bytes as archived (8-bit text in its own charset).
pub fn fetch_bytes(mbox: &Path, id: &str) -> Result<Vec<u8>> {
    if is_stale(mbox) {
        return Err(anyhow!(
            "the index for {} is missing or stale — re-register the source to rebuild it",
            mbox.display()
        ));
    }
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
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
    Ok(buf)
}

/// The head of the message the index keeps under `id` (at most
/// `HEAD_READ` bytes: its headers), or None when the index has no such id.
/// An index that is missing, out of date or can't be read is an error,
/// never "not there".
pub fn head_of(mbox: &Path, id: &str) -> Result<Option<Vec<u8>>> {
    if is_stale(mbox) {
        return Err(anyhow!(
            "the index for {} is missing or stale",
            mbox.display()
        ));
    }
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT offset, length FROM messages WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((offset, length)) = row else {
        return Ok(None);
    };
    let mut file = std::fs::File::open(mbox)?;
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut buf = vec![0u8; (length.max(0) as usize).min(HEAD_READ)];
    file.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Bytes read of a message's head to find its Message-ID.
const HEAD_READ: usize = 64 * 1024;

/// The Message-IDs of the messages an index keeps under their offset
/// (`at:<offset>`): a later copy of a Message-ID it gives to the first copy
/// (`index_stream`), read from each one's head by the import's rule. Maps
/// a Message-ID to those rows' ids. Read once per archive; such rows are
/// few (duplicates, and messages without a Message-ID).
pub fn offset_keyed_ids(mbox: &Path) -> Result<std::collections::HashMap<String, Vec<String>>> {
    if is_stale(mbox) {
        return Err(anyhow!(
            "the index for {} is missing or stale",
            mbox.display()
        ));
    }
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT id, offset, length FROM messages WHERE id >= 'at:' AND id < 'at;' ORDER BY offset",
    )?;
    let rows: Vec<(String, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut file = std::fs::File::open(mbox)?;
    let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
    for (id, offset, length) in rows {
        let mut head = vec![0u8; (length.max(0) as usize).min(HEAD_READ)];
        file.seek(SeekFrom::Start(offset as u64))?;
        file.read_exact(&mut head)?;
        let head = String::from_utf8_lossy(&head);
        let head = head
            .split("\n\n")
            .next()
            .unwrap_or_default()
            .split("\r\n\r\n")
            .next()
            .unwrap_or_default();
        if let Some(message_id) = hearthloom_adapters::mbox::message_id_ref(head) {
            out.entry(message_id).or_default().push(id);
        }
    }
    Ok(out)
}

/// One archived copy of a message.
#[derive(Debug, Clone)]
pub struct ArchivedCopy {
    /// Its labels as the index holds them now (label updates applied).
    pub labels: String,
    pub bytes: Vec<u8>,
}

/// Every copy of the message `message_id` an archive holds: the one its
/// index keeps under that id and the later ones it keeps under their
/// offset (`offset_keyed`, from [`offset_keyed_ids`]). Empty: none here.
/// An index that can't be read is an error, never "none here".
pub fn copies_of(
    mbox: &Path,
    message_id: &str,
    offset_keyed: &std::collections::HashMap<String, Vec<String>>,
) -> Result<Vec<ArchivedCopy>> {
    if is_stale(mbox) {
        return Err(anyhow!(
            "the index for {} is missing or stale",
            mbox.display()
        ));
    }
    let conn =
        Connection::open_with_flags(index_path(mbox), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT offset, length, labels FROM messages WHERE id = ?1")?;
    let ids = std::iter::once(message_id.to_string())
        .chain(offset_keyed.get(message_id).into_iter().flatten().cloned());
    let mut file: Option<std::fs::File> = None;
    let mut out = Vec::new();
    for id in ids {
        let Some((offset, length, labels)) = stmt
            .query_row([&id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .optional()?
        else {
            continue;
        };
        let file = match &mut file {
            Some(f) => f,
            None => file.insert(std::fs::File::open(mbox)?),
        };
        file.seek(SeekFrom::Start(offset as u64))?;
        let mut bytes = vec![0u8; length.max(0) as usize];
        file.read_exact(&mut bytes)?;
        out.push(ArchivedCopy {
            labels: labels.unwrap_or_default(),
            bytes,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recent_messages_come_oldest_first_with_the_pulls_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("live.mbox");
        let now = chrono::Utc::now();
        let msg = |id: &str, days: i64, labels: &str| {
            format!(
                "From gmail-imap uid=1\nX-Gmail-Labels: {labels}\nFrom: A <a@x>\nSubject: s\nDate: {}\nMessage-ID: <{id}>\n\nbody\n\n",
                (now - chrono::Duration::days(days)).to_rfc2822()
            )
        };
        std::fs::write(
            &mbox,
            [
                msg("old@x", 30, "Inbox"),
                msg("b@x", 2, "Inbox, Category Personal"),
                msg("a@x", 5, "Sent"),
            ]
            .concat(),
        )
        .unwrap();
        build(&mbox).unwrap();
        let got = since(&mbox, now - chrono::Duration::days(14)).unwrap();
        assert_eq!(
            got,
            vec![
                ("a@x".to_string(), "Sent".to_string()),
                ("b@x".to_string(), "Inbox, Category Personal".to_string())
            ]
        );
    }

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
        assert_eq!(
            scan_headers("X-Gmail-Labels:\nX-Gmail-Labels: Starred\n").labels,
            ""
        );

        // End to end: the hit shows the first labels and keeps the real id.
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("mail.mbox");
        std::fs::write(
            &mbox,
            format!("From x@y Mon Jan 05 10:00:00 2026\n{head}\npromo body\n"),
        )
        .unwrap();
        build(&mbox).unwrap();
        let hits = search(&mbox, "sale", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "real@shop.example");
        assert!(
            hits[0].1.contains("Category Promotions"),
            "got: {}",
            hits[0].1
        );
        assert!(
            !hits[0].1.contains("Starred") && !hits[0].1.contains("bank"),
            "got: {}",
            hits[0].1
        );
    }

    /// Labels re-read after archiving show in search and stay through a
    /// rebuild; a later copy of the message keeps its own labels, and so
    /// does mail appended afterwards.
    #[test]
    fn test_label_updates_survive_a_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("live.mbox");
        let rec = |uid: u32, id: &str, labels: &str, subject: &str| {
            format!(
                "From gmail-imap uid={uid}\nX-Gmail-Labels: {labels}\nFrom: A <a@x>\n\
                 Subject: {subject}\nDate: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\nbody\n\n"
            )
        };
        std::fs::write(
            &mbox,
            [
                rec(1, "promo@x", "Category Promotions", "sale"),
                rec(2, "other@x", "Inbox", "hello"),
                rec(3, "promo@x", "Category Promotions", "sale again"),
            ]
            .concat(),
        )
        .unwrap();
        build(&mbox).unwrap();
        set_labels(
            &mbox,
            &[(
                "promo@x".to_string(),
                "Category Promotions, Starred".to_string(),
            )],
            &[],
        )
        .unwrap();
        let starred = |mbox: &Path| {
            search(mbox, "starred", 10)
                .unwrap()
                .into_iter()
                .map(|h| h.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(starred(&mbox), vec!["promo@x"]);

        // the archive changes (more mail) and the index is rebuilt whole
        let mut grown = std::fs::read_to_string(&mbox).unwrap();
        grown.push_str(&rec(4, "new@x", "Inbox", "fresh"));
        std::fs::write(&mbox, grown).unwrap();
        assert!(is_stale(&mbox));
        assert_eq!(build(&mbox).unwrap(), 4);
        assert_eq!(starred(&mbox), vec!["promo@x"]);
        assert_eq!(
            labels_of(&mbox, "promo@x").unwrap().as_deref(),
            Some("Category Promotions, Starred")
        );
        assert_eq!(labels_of(&mbox, "new@x").unwrap().as_deref(), Some("Inbox"));
        // the second copy is indexed by its offset, with its own labels
        let copy = search(&mbox, "again", 10).unwrap();
        assert_eq!(copy.len(), 1);
        assert!(copy[0].0.starts_with("at:"), "{copy:?}");
        assert!(!copy[0].1.contains("Starred"), "{copy:?}");

        // and a second rebuild still has them
        build(&mbox).unwrap();
        assert_eq!(starred(&mbox), vec!["promo@x"]);

        // an index from before label updates were kept takes one
        let conn = Connection::open(index_path(&mbox)).unwrap();
        conn.execute_batch("DROP TABLE label_updates").unwrap();
        drop(conn);
        set_labels(
            &mbox,
            &[("other@x".to_string(), "Inbox, Starred".to_string())],
            &[],
        )
        .unwrap();
        build(&mbox).unwrap();
        assert_eq!(starred(&mbox), vec!["other@x"]);
    }

    /// A label update and a meta row written while a rebuild runs are in
    /// the index the rebuild leaves: the writes wait for it.
    #[test]
    fn test_writes_during_a_rebuild_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("live.mbox");
        std::fs::write(
            &mbox,
            "From gmail-imap uid=1\nX-Gmail-Labels: Category Promotions\nFrom: A <a@x>\n\
             Subject: sale\nDate: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <promo@x>\n\nbody\n\n",
        )
        .unwrap();
        build(&mbox).unwrap();
        meta_set(&mbox, "gmail_cursor", "1:1").unwrap();

        // the rebuild stops just before it puts its index in place
        let (paused, at_install) = std::sync::mpsc::channel::<()>();
        let (resume, go) = std::sync::mpsc::channel::<()>();
        BEFORE_INSTALL.lock().unwrap().push((
            mbox.clone(),
            Box::new(move || {
                paused.send(()).unwrap();
                go.recv().unwrap();
            }),
        ));
        let rebuild = {
            let mbox = mbox.clone();
            std::thread::spawn(move || build(&mbox).unwrap())
        };
        at_install.recv().unwrap();
        let write = {
            let mbox = mbox.clone();
            std::thread::spawn(move || {
                set_labels(
                    &mbox,
                    &[(
                        "promo@x".to_string(),
                        "Category Promotions, Starred".to_string(),
                    )],
                    &[("gmail_cursor", "1:2")],
                )
                .unwrap();
            })
        };
        // time for the write to land, were it not waiting
        std::thread::sleep(std::time::Duration::from_millis(300));
        resume.send(()).unwrap();
        rebuild.join().unwrap();
        write.join().unwrap();

        assert_eq!(
            labels_of(&mbox, "promo@x").unwrap().as_deref(),
            Some("Category Promotions, Starred")
        );
        assert_eq!(
            meta_get(&mbox, "gmail_cursor").unwrap().as_deref(),
            Some("1:2")
        );
        // and the next rebuild carries them on
        build(&mbox).unwrap();
        assert_eq!(
            labels_of(&mbox, "promo@x").unwrap().as_deref(),
            Some("Category Promotions, Starred")
        );
    }

    /// A rebuild that can't read the index it would replace fails and
    /// leaves it in place — its meta rows hold the pull's list of messages
    /// still to observe; once it reads again, the rebuild carries the list
    /// and the label updates on. A file that is no database at all is
    /// rebuilt over, without either.
    #[test]
    fn test_a_rebuild_leaves_an_index_it_cannot_read_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("live.mbox");
        std::fs::write(
            &mbox,
            "From gmail-imap uid=1\nX-Gmail-Labels: Category Promotions\nFrom: A <a@x>\n\
             Subject: sale\nDate: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <promo@x>\n\nbody\n\n",
        )
        .unwrap();
        build(&mbox).unwrap();
        let list = "gmail_observe_pending";
        set_labels(
            &mbox,
            &[(
                "promo@x".to_string(),
                "Category Promotions, Starred".to_string(),
            )],
            &[(list, r#"["promo@x"]"#)],
        )
        .unwrap();
        let index = index_path(&mbox);
        let mode = |m: u32| {
            std::fs::set_permissions(&index, std::fs::Permissions::from_mode(m)).unwrap();
        };

        // out of reach for a while (a reader permissions don't stop skips
        // this step)
        mode(0o000);
        if std::fs::File::open(&index).is_err() {
            let err = build(&mbox).unwrap_err();
            assert!(format!("{err:#}").contains("stays as it is"), "{err:#}");
        }
        mode(0o644);
        assert_eq!(
            meta_get(&mbox, list).unwrap().as_deref(),
            Some(r#"["promo@x"]"#)
        );

        // readable again: rebuilt, with both
        build(&mbox).unwrap();
        assert_eq!(
            meta_get(&mbox, list).unwrap().as_deref(),
            Some(r#"["promo@x"]"#)
        );
        assert_eq!(
            labels_of(&mbox, "promo@x").unwrap().as_deref(),
            Some("Category Promotions, Starred")
        );

        // no database at all: nothing in it can be read, ever
        std::fs::write(&index, vec![b'x'; 4096]).unwrap();
        build(&mbox).unwrap();
        assert_eq!(meta_get(&mbox, list).unwrap(), None);
        assert_eq!(
            labels_of(&mbox, "promo@x").unwrap().as_deref(),
            Some("Category Promotions")
        );
    }

    /// An archive with a pending list, a cursor and a label update, indexed.
    fn live_archive(tmp: &Path) -> PathBuf {
        let mbox = tmp.join("live.mbox");
        std::fs::write(
            &mbox,
            "From gmail-imap uid=1\nX-Gmail-Labels: Category Promotions\nFrom: A <a@x>\n\
             Subject: sale\nDate: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <promo@x>\n\nbody\n\n",
        )
        .unwrap();
        build(&mbox).unwrap();
        set_labels(
            &mbox,
            &[(
                "promo@x".to_string(),
                "Category Promotions, Starred".to_string(),
            )],
            &[
                ("gmail_observe_pending", r#"["promo@x"]"#),
                ("gmail_cursor", "1:1"),
            ],
        )
        .unwrap();
        mbox
    }

    /// A rebuild while another connection holds the index it would replace
    /// (busy, locked) fails and leaves it in place, list and all; once the
    /// index is free, the rebuild carries the list and the label updates on.
    #[test]
    fn test_a_rebuild_leaves_a_busy_index_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = live_archive(tmp.path());
        let list = "gmail_observe_pending";

        let holder = Connection::open(index_path(&mbox)).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let err = build(&mbox).unwrap_err();
        assert!(format!("{err:#}").contains("stays as it is"), "{err:#}");
        holder.execute_batch("COMMIT").unwrap();
        drop(holder);
        assert_eq!(
            meta_get(&mbox, list).unwrap().as_deref(),
            Some(r#"["promo@x"]"#)
        );

        build(&mbox).unwrap();
        assert_eq!(
            meta_get(&mbox, list).unwrap().as_deref(),
            Some(r#"["promo@x"]"#)
        );
        assert_eq!(
            labels_of(&mbox, "promo@x").unwrap().as_deref(),
            Some("Category Promotions, Starred")
        );
    }

    /// Label updates that are damaged are rebuilt over alone: the meta rows
    /// read — the pull's cursor and its list of messages still to observe —
    /// are kept, and the message shows its archived labels until the pull
    /// reads them from Gmail again.
    #[test]
    fn test_damaged_label_updates_leave_the_meta_rows() {
        use std::io::{Seek, Write};
        let tmp = tempfile::tempdir().unwrap();
        let mbox = live_archive(tmp.path());
        let index = index_path(&mbox);
        let (root, page): (i64, i64) = {
            let conn = Connection::open(&index).unwrap();
            let root = conn
                .query_row(
                    "SELECT rootpage FROM sqlite_master WHERE name = 'label_updates'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let page = conn
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .unwrap();
            (root, page)
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&index)
            .unwrap();
        file.seek(std::io::SeekFrom::Start(((root - 1) * page) as u64))
            .unwrap();
        file.write_all(&vec![0xFF; page as usize]).unwrap();
        drop(file);
        let damaged = Connection::open(&index)
            .unwrap()
            .prepare("SELECT id, labels FROM label_updates")
            .and_then(|mut stmt| {
                stmt.query_map([], |r| r.get::<_, String>(1))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .is_err();
        assert!(damaged, "the table is damaged");

        build(&mbox).unwrap();
        assert_eq!(
            meta_get(&mbox, "gmail_observe_pending").unwrap().as_deref(),
            Some(r#"["promo@x"]"#)
        );
        assert_eq!(
            meta_get(&mbox, "gmail_cursor").unwrap().as_deref(),
            Some("1:1")
        );
        assert_eq!(
            labels_of(&mbox, "promo@x").unwrap().as_deref(),
            Some("Category Promotions")
        );
    }

    /// A Message-ID header folded onto its next line, or written with a
    /// comment beside the id, indexes the message under the key the mail
    /// import gives it — not as a message without one.
    #[test]
    fn test_message_ids_are_indexed_under_the_imports_key() {
        for header in [
            "Message-ID:\n <folded@x>",
            "Message-ID: (desk) <hush@x>",
            "message-id: <plain@x>",
        ] {
            let head = format!("From: A <a@x>\n{header}\nSubject: s\n");
            assert_eq!(
                scan_headers(&head).message_id,
                hearthloom_adapters::mbox::message_id_ref(&head),
                "{header}"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("mail.mbox");
        std::fs::write(
            &mbox,
            "From x@y Mon Jan 05 10:00:00 2026\nFrom: A <a@x>\nSubject: folded\n\
             Message-ID:\n <folded@x>\n\nbody\n",
        )
        .unwrap();
        build(&mbox).unwrap();
        assert!(ids_current(&mbox));
        let hits = search(&mbox, "folded", 10).unwrap();
        assert_eq!(hits.len(), 1);
        // the import's key: the header unfolded, the brackets at its ends
        // trimmed
        assert_eq!(hits[0].0, " <folded@x");
        assert!(fetch(&mbox, &hits[0].0, 10_000).unwrap().contains("body"));
    }

    /// Each record's separator line, in archive order, from after a row.
    #[test]
    fn test_separator_lines_in_archive_order() {
        let (_tmp, mbox) = fixture();
        build(&mbox).unwrap();
        let all = separators_after(&mbox, 0, 10).unwrap();
        assert_eq!(
            all.iter().map(|s| s.line.as_str()).collect::<Vec<_>>(),
            vec![
                "From a@x Mon Jan 05 10:00:00 2026",
                "From b@x Mon Jan 05 11:00:00 2026"
            ]
        );
        // each with its id: the Message-ID, or its offset when it has none
        assert_eq!(all[0].id, "paris1@x");
        assert!(all[1].id.starts_with("at:"), "{:?}", all[1]);
        let rest = separators_after(&mbox, all[0].row, 10).unwrap();
        assert_eq!(rest, vec![all[1].clone()]);
        assert!(separators_after(&mbox, all[1].row, 10).unwrap().is_empty());
        assert_eq!(separators_after(&mbox, 0, 1).unwrap(), vec![all[0].clone()]);
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

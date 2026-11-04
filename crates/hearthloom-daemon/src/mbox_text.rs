//! The text of archived mail, searchable (0.5.87).
//!
//! A mail archive's index (`mbox_index.rs`) covers the headers: sender,
//! recipients, subject, labels, date. What people look for in old mail —
//! a link, an account, a portal — sits in the text, so this adds a second
//! table to the same sidecar, `texts_fts(id, body, links)`: each message's
//! readable text (the plain part, else its HTML turned into text; decoded
//! from base64 / quoted-printable and its charset), capped, and the web
//! addresses it links to (from the text and from HTML links).
//!
//! It fills in the background, in small resumable batches, behind a
//! `text_rowid` watermark over `messages.rowid` (a full rebuild starts a
//! new file with no watermark; `carry_meta` leaves it behind). Search works
//! on headers throughout and gains the text as it lands; new mail from the
//! live watch is picked up the same way. Progress is kept in memory for
//! Ask's instructions, so reading it touches no disk.
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use rusqlite::Connection;

use crate::AppState;

/// Text kept per message: enough for a letter, an invitation or a receipt.
const TEXT_CAP: usize = 16 * 1024;
/// Bytes of a message read to find its text (attachments follow the text
/// parts; parsing a 25 MB message to find one paragraph is waste).
const READ_CAP: u64 = 2 * 1024 * 1024;
const LINKS_CAP: usize = 20;
/// Longer addresses are almost always tracking redirects.
const LINK_MAX: usize = 120;
/// Archives bigger than this wait for the owner to turn text on (the
/// 665k-message Takeout would take several GB): `hearthloom source text`.
pub const AUTO_LIMIT: usize = 100_000;
const BATCH: usize = 200;

pub const WATERMARK: &str = "text_rowid";

static PROGRESS: Mutex<Option<HashMap<PathBuf, (usize, usize)>>> = Mutex::new(None);
static OFF: Mutex<Option<std::collections::HashSet<PathBuf>>> = Mutex::new(None);

/// The owner's choice for an archive on this node (`mail.text.<name>`),
/// if they made one.
pub fn setting(state: &AppState, source: &str) -> Option<bool> {
    state
        .store
        .consolidation_state(&format!("mail.text.{source}"))
        .ok()
        .flatten()
        .map(|v| v == "on")
}

pub fn set_setting(state: &AppState, source: &str, on: bool) -> Result<()> {
    state.store.set_consolidation_state(
        &format!("mail.text.{source}"),
        if on { "on" } else { "off" },
    )
}

/// Whether text indexing is off for this archive (the last pass skipped it).
pub fn is_off(mbox: &Path) -> bool {
    OFF.lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|s| s.contains(mbox))
}

fn mark_off(mbox: &Path, off: bool) {
    let mut guard = OFF.lock().unwrap_or_else(|p| p.into_inner());
    let set = guard.get_or_insert_with(Default::default);
    if off {
        set.insert(mbox.to_path_buf());
    } else {
        set.remove(mbox);
    }
}

/// Text search turned off: drop the text and give the space back.
pub fn clear(mbox: &Path) -> Result<()> {
    let conn = Connection::open(crate::mbox_index::index_path(mbox))?;
    conn.busy_timeout(Duration::from_secs(30))?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS texts_fts; DELETE FROM meta WHERE key = 'text_rowid'; VACUUM;",
    )?;
    set_progress(mbox, 0, 0);
    Ok(())
}

/// Room left on the archive's disk, in bytes (statvfs; None if unknown).
fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let dir = path.parent().unwrap_or(path);
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: a valid C path and a zeroed out-struct
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Bytes the text index needs per message, measured on a real archive
/// (19,560 messages of live Gmail: +169 MB, 12 s on a Mac mini), rounded up.
pub const BYTES_PER_MESSAGE: u64 = 9_000;

/// (messages with text, messages) for an archive, as of the last pass —
/// None before the first pass has looked at it.
pub fn progress(mbox: &Path) -> Option<(usize, usize)> {
    PROGRESS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()?
        .get(mbox)
        .copied()
}

fn set_progress(mbox: &Path, done: usize, total: usize) {
    PROGRESS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(mbox.to_path_buf(), (done, total));
}

/// The searchable text of one raw message, and the web addresses it
/// links to (host and path; query strings are mostly tracking).
pub fn message_text(raw: &[u8]) -> (String, String) {
    let Some(message) = mail_parser::MessageParser::default().parse(raw) else {
        return (String::new(), String::new());
    };
    let text: String = message
        .body_text(0)
        .map(|t| t.into_owned())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TEXT_CAP)
        .collect();
    let mut links: Vec<String> = Vec::new();
    let mut scan = |s: &str| {
        for link in find_links(s) {
            if links.len() < LINKS_CAP && !links.contains(&link) {
                links.push(link);
            }
        }
    };
    scan(&text);
    if let Some(html) = message.body_html(0) {
        scan(&html);
    }
    (text, links.join(" "))
}

/// `https://portal.example.com/client/login?x=1` → `portal.example.com/client/login`.
fn find_links(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = s.to_ascii_lowercase();
    let mut at = 0;
    while let Some(found) = lower[at..]
        .find("https://")
        .or_else(|| lower[at..].find("http://"))
    {
        let start = at + found;
        let after = start
            + if lower[start..].starts_with("https://") {
                8
            } else {
                7
            };
        let end = s[after..]
            .find(|c: char| c.is_whitespace() || "\"'<>()[]{}|\\^`".contains(c))
            .map_or(s.len(), |e| after + e);
        let link = s[after..end]
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_end_matches(['.', ',', ';', ':', '!', '/'])
            .to_string();
        if link.contains('.') && link.len() <= LINK_MAX {
            out.push(link);
        }
        at = end.max(after);
    }
    out
}

/// Index the text of the next batch of messages without it. Returns
/// (indexed now, messages with text, messages).
pub fn index_batch(mbox: &Path, batch: usize) -> Result<(usize, usize, usize)> {
    let conn = Connection::open(crate::mbox_index::index_path(mbox))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS texts_fts USING fts5(id UNINDEXED, body, links);",
    )?;
    let mark: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [WATERMARK], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let rows: Vec<(i64, String, i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT rowid, id, offset, length FROM messages WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![mark, batch as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    // read and parse outside the write transaction: the live watch appends
    // to this file too
    let mut file = std::fs::File::open(mbox)?;
    let mut texts = Vec::with_capacity(rows.len());
    for (rowid, id, offset, length) in &rows {
        let mut buf = vec![0u8; (*length as u64).min(READ_CAP) as usize];
        file.seek(SeekFrom::Start(*offset as u64))?;
        let read = file.read(&mut buf).unwrap_or(0);
        buf.truncate(read);
        let (body, links) = message_text(&buf);
        texts.push((*rowid, id.clone(), body, links));
    }
    if let Some(last) = texts.last().map(|t| t.0) {
        let tx = conn.unchecked_transaction()?;
        for (_, id, body, links) in &texts {
            tx.execute(
                "INSERT INTO texts_fts(id, body, links) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, body, links],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            rusqlite::params![WATERMARK, last.to_string()],
        )?;
        tx.commit()?;
    }
    let mark = texts.last().map_or(mark, |t| t.0);
    let done: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE rowid <= ?1",
        [mark],
        |r| r.get(0),
    )?;
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    Ok((texts.len(), done as usize, total as usize))
}

/// Index this archive's text? The owner's switch wins; without one, only
/// archives up to AUTO_LIMIT (and only once their size is known). Never
/// without room for three times the projected index and 2 GB besides.
fn should_index(setting: Option<bool>, messages: Option<usize>, free: Option<u64>) -> bool {
    let wanted = match setting {
        Some(on) => on,
        None => messages.is_some_and(|n| n <= AUTO_LIMIT),
    };
    let room = free.is_none_or(|free| {
        let need = messages.unwrap_or(0) as u64 * BYTES_PER_MESSAGE;
        free > need.saturating_mul(3).max(2 << 30)
    });
    wanted && room
}

/// The background pass: every mail archive this node holds gets its text
/// indexed, a batch at a time, then the pass rests and looks again.
pub async fn run_loop(state: AppState) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    loop {
        let mut busy = false;
        let sources = state.store.list_virtual_sources().unwrap_or_default();
        for source in sources.iter().filter(|s| s.adapter == "mbox") {
            let Some(mbox) = crate::virtual_source::mbox_path_if_online(source) else {
                continue;
            };
            if crate::mbox_index::is_stale(&mbox) {
                continue; // a rebuild is due; its new file starts from zero
            }
            // big archives wait for the owner; and never fill the disk
            let total = crate::mbox_index::summary(&mbox).map(|s| s.messages);
            if !should_index(setting(&state, &source.name), total, free_bytes(&mbox)) {
                mark_off(&mbox, true);
                continue;
            }
            mark_off(&mbox, false);
            let path = mbox.clone();
            let outcome = tokio::task::spawn_blocking(move || index_batch(&path, BATCH)).await;
            match outcome {
                Ok(Ok((now, done, total))) => {
                    set_progress(&mbox, done, total);
                    if now > 0 {
                        busy = true;
                        // a backlog's milestones, not every new message
                        if (done == total && now > 1) || (done % 10_000 < BATCH && now == BATCH) {
                            tracing::info!("mail text {}: {done} of {total} messages", source.name);
                        }
                    }
                }
                Ok(Err(err)) => {
                    tracing::warn!("mail text {}: {err:#}", source.name);
                }
                Err(err) => tracing::warn!("mail text {}: {err}", source.name),
            }
        }
        // keep going while there's a backlog, gently; then look once a minute
        let rest = if busy {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(60)
        };
        tokio::time::sleep(rest).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_and_links_come_out_of_real_shaped_mail() {
        // an HTML-only, quoted-printable invitation: the shape that hid
        // a portal's address from the header index
        let invite = b"From: Portal <noreply@portal.example.com>\r\n\
Subject: You've been invited\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
<p>Susan shared a folder with you.</p><a href=3D\"https://portal.example.com/=\r\n\
client/login?token=3Dabc\">Open the portal</a>\r\n";
        let (text, links) = message_text(invite);
        assert!(text.contains("Susan shared a folder"), "{text}");
        assert!(!text.contains("<p>"), "HTML becomes text: {text}");
        assert_eq!(links, "portal.example.com/client/login");

        let plain = b"From: Ann <ann@x.example>\r\nSubject: hi\r\n\
Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n\
VGhlIGZvcm0gaXMgYXQgaHR0cHM6Ly9mb3Jtcy5leGFtcGxlLm9yZy9hcHBseS4=\r\n";
        let (text, links) = message_text(plain);
        assert_eq!(text, "The form is at https://forms.example.org/apply.");
        assert_eq!(links, "forms.example.org/apply");
    }

    /// The whole path: headers searchable at once; the text joins in
    /// batches; new mail is picked up; a rebuild starts the text again.
    #[test]
    fn test_archive_text_is_indexed_in_batches_and_searched() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        let message = |id: &str, subject: &str, body: &str| {
            format!(
                "From x@y Mon Jan 05 10:00:00 2026\nFrom: Notices <noreply@files.example.com>\n\
                 Subject: {subject}\nDate: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\
                 Content-Type: text/plain\n\n{body}\n\n"
            )
        };
        std::fs::write(
            &mbox,
            [
                message(
                    "m1@x",
                    "Welcome",
                    "Your client portal is ready at https://clients.example.com/login",
                ),
                message("m2@x", "Receipt", "Thanks for your order."),
                message("m3@x", "Weekly digest", "Nothing to see."),
            ]
            .concat(),
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        // before the text pass: headers only
        assert!(crate::mbox_index::search(&mbox, "portal", 10)
            .unwrap()
            .is_empty());

        assert_eq!(index_batch(&mbox, 2).unwrap(), (2, 2, 3));
        assert_eq!(index_batch(&mbox, 2).unwrap(), (1, 3, 3));
        assert_eq!(index_batch(&mbox, 2).unwrap(), (0, 3, 3));
        let hits = crate::mbox_index::search(&mbox, "portal", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "m1@x");
        assert!(
            hits[0].1.contains("text:") && hits[0].1.contains("«portal»"),
            "{}",
            hits[0].1
        );
        // a link's host is searchable on its own
        assert_eq!(
            crate::mbox_index::search(&mbox, "clients.example.com", 10)
                .unwrap()
                .len(),
            1
        );
        // a header hit comes first, and once
        let hits = crate::mbox_index::search(&mbox, "welcome", 10).unwrap();
        assert_eq!(hits.iter().filter(|h| h.0 == "m1@x").count(), 1);

        // new mail appended by the live watch is picked up
        let before = std::fs::metadata(&mbox).unwrap().len();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&mbox)
            .unwrap();
        std::io::Write::write_all(
            &mut file,
            message(
                "m4@x",
                "Shared with you",
                "Open https://vault.example.net/f/1",
            )
            .as_bytes(),
        )
        .unwrap();
        drop(file);
        crate::mbox_index::append_index(&mbox, before).unwrap();
        assert_eq!(index_batch(&mbox, 10).unwrap(), (1, 4, 4));
        assert_eq!(
            crate::mbox_index::search(&mbox, "vault.example.net", 10).unwrap()[0].0,
            "m4@x"
        );

        // a rebuild is a new file: the text starts again from the beginning
        crate::mbox_index::build(&mbox).unwrap();
        assert!(crate::mbox_index::search(&mbox, "portal", 10)
            .unwrap()
            .is_empty());
        assert_eq!(index_batch(&mbox, 10).unwrap(), (4, 4, 4));
    }

    #[test]
    fn test_big_archives_wait_for_the_owner_and_the_disk_is_never_filled() {
        let gb = 1u64 << 30;
        assert!(
            should_index(None, Some(19_544), Some(280 * gb)),
            "the live archive: on"
        );
        assert!(
            !should_index(None, Some(664_921), Some(280 * gb)),
            "the Takeout waits"
        );
        assert!(
            should_index(Some(true), Some(664_921), Some(280 * gb)),
            "…until turned on"
        );
        assert!(
            !should_index(Some(true), Some(664_921), Some(10 * gb)),
            "not without room"
        );
        assert!(
            !should_index(Some(false), Some(10), Some(280 * gb)),
            "off is off"
        );
        assert!(
            !should_index(None, None, Some(280 * gb)),
            "size not known yet"
        );
    }

    #[test]
    fn test_links_are_trimmed_to_host_and_path() {
        assert_eq!(
            find_links("see https://a.example.com/x?y=1, or (http://b.example.org/)."),
            vec!["a.example.com/x".to_string(), "b.example.org".to_string()]
        );
        let tracking = format!("https://click.example.com/{}", "x".repeat(200));
        assert!(
            find_links(&tracking).is_empty(),
            "long tracking redirects are left out"
        );
        assert!(find_links("http://localhost").is_empty());
    }
}

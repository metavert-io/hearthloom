//! Gmail live federation (Tier 3 of the materialize/virtualize split):
//! incremental IMAP pull of new mail into a rolling live mbox, so the
//! record stays current past the last Takeout. Transport is app-password
//! IMAP with Gmail's extensions (X-GM-LABELS for label fidelity,
//! X-GM-RAW for category membership) per the design log, 2026-07-08;
//! Gmail-API OAuth only if IMAP fidelity ever disappoints.
//!
//! The adapter owns the protocol + pure record shaping; the daemon owns
//! all file/store IO (design law #4). Pull orchestration is written
//! against the `MailFeed` trait so the whole loop is testable without a
//! network; `ImapFeed` is the real Gmail-backed implementation.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use imap::extensions::idle::SetReadTimeout;

const GMAIL_HOST: &str = "imap.gmail.com";
const GMAIL_PORT: u16 = 993;
/// How long looking up the server's address may take.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long opening the TCP connection may take (per address tried).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one read or write on an open connection may wait: the TLS
/// handshake, the greeting, LOGIN, each command's reply, each stretch of a
/// FETCH. A server silent this long fails the command; the caller drops
/// the session, and whatever it held (a mailbox's pull) is released.
pub const IO_TIMEOUT: Duration = Duration::from_secs(120);

/// Where a pull resumes. UIDs are only meaningful within one
/// UIDVALIDITY generation — a generation change re-anchors via the
/// backfill window (Message-ID dedupe absorbs any overlap). A `last_uid`
/// of 0 records no position: it re-anchors like a first run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmailCursor {
    pub uidvalidity: u32,
    pub last_uid: u32,
}

/// What selecting All Mail reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selected {
    pub uidvalidity: u32,
    /// The UID the next message will get (UIDNEXT): every message already
    /// in the mailbox has a smaller one.
    pub uid_next: Option<u32>,
}

/// Gmail's labels for one message as they are now (labels only, no body),
/// with the key its memory has (`mbox::message_id_ref`; None: no
/// Message-ID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentLabels {
    pub uid: u32,
    pub labels: Vec<String>,
    pub message_id: Option<String>,
}

/// One fetched message: stable UID, normalized Gmail labels (system
/// labels stripped of the backslash prefix, `Category Personal` merged
/// from the X-GM-RAW category search), and the raw RFC822 bytes.
#[derive(Debug, Clone)]
pub struct PulledMessage {
    pub uid: u32,
    pub labels: Vec<String>,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct PullOptions {
    /// First-run (or re-anchor) window: pull mail from the last N days
    /// instead of the whole mailbox.
    pub backfill_days: u32,
    /// Max messages per pull; the rest waits for the next cycle.
    pub cap: usize,
    /// Messages per FETCH round trip (bounds peak memory — attachments
    /// ride along in BODY[]).
    pub batch: usize,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            backfill_days: 90,
            cap: 2000,
            batch: 25,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PullOutcome {
    pub uidvalidity: u32,
    pub last_uid: u32,
    pub fetched: usize,
    /// More messages were available beyond `cap` — the next pull
    /// continues from `last_uid`.
    pub more: bool,
}

/// The mailbox surface `pull_new` needs — implemented by `ImapFeed`
/// against real Gmail and by test fakes.
pub trait MailFeed {
    /// Select `[Gmail]/All Mail`; returns its UIDVALIDITY and UIDNEXT.
    fn select_all_mail(&mut self) -> Result<Selected>;
    /// UIDs strictly greater than `last_uid`.
    fn uids_after(&mut self, last_uid: u32) -> Result<Vec<u32>>;
    /// UIDs of mail received in the last `days` days (first-run anchor).
    fn uids_since_days(&mut self, days: u32) -> Result<Vec<u32>>;
    /// Which of the UIDs in `lo..=hi` sit in a Gmail category
    /// (X-GM-RAW `category:<name>` — categories are NOT in X-GM-LABELS).
    fn uids_in_category(&mut self, lo: u32, hi: u32, category: &str) -> Result<HashSet<u32>>;
    fn fetch_batch(&mut self, uids: &[u32]) -> Result<Vec<PulledMessage>>;
    /// Gmail's labels and the Message-ID of each of `uids`, without bodies.
    fn labels_of(&mut self, uids: &[u32]) -> Result<Vec<CurrentLabels>>;
    /// UIDs of the messages with these Message-IDs (each one passing
    /// [`is_message_id`]). It is a search and may find other messages too:
    /// callers key what they read by Message-ID.
    fn uids_of(&mut self, message_ids: &[String]) -> Result<Vec<u32>>;
}

/// `Category Personal` comes from a category search (X-GM-LABELS doesn't
/// carry categories): add it to a message's labels when the search found it.
fn merge_personal(labels: &mut Vec<String>, uid: u32, personal: &HashSet<u32>) {
    if personal.contains(&uid)
        && !labels
            .iter()
            .any(|l| l.eq_ignore_ascii_case("Category Personal"))
    {
        labels.push("Category Personal".to_string());
    }
}

/// Incremental pull: resolve the cursor against UIDVALIDITY, list new
/// UIDs (capped), annotate `Category Personal` membership, and deliver
/// messages to `sink` in fetch-batch-sized groups. The returned cursor
/// is only durable once the CALLER persists it — persist after the
/// downstream writes succeed, so a failed run replays idempotently.
pub fn pull_new(
    feed: &mut dyn MailFeed,
    cursor: Option<GmailCursor>,
    opts: &PullOptions,
    mut sink: impl FnMut(Vec<PulledMessage>) -> Result<()>,
) -> Result<PullOutcome> {
    let selected = feed.select_all_mail()?;
    let uidvalidity = selected.uidvalidity;
    let anchored = match cursor {
        Some(c) if c.uidvalidity == uidvalidity && c.last_uid > 0 => Some(c.last_uid),
        Some(c) if c.uidvalidity != uidvalidity => {
            tracing::warn!(
                "gmail UIDVALIDITY changed {} -> {uidvalidity}; re-anchoring via the {}-day backfill window",
                c.uidvalidity,
                opts.backfill_days
            );
            None
        }
        // no cursor, or one that records no position
        _ => None,
    };

    let mut uids = match anchored {
        Some(last) => feed.uids_after(last)?,
        None => feed.uids_since_days(opts.backfill_days)?,
    };
    if let Some(last) = anchored {
        // `UID n:*` includes the highest existing UID even when it is
        // < n (RFC 3501 quirk) — drop anything not actually new.
        uids.retain(|u| *u > last);
    }
    uids.sort_unstable();
    let more = uids.len() > opts.cap;
    uids.truncate(opts.cap);

    let mut last_uid = anchored.unwrap_or(0);
    if uids.is_empty() {
        if anchored.is_none() {
            // Nothing in the window: start at the mailbox's end, so the mail
            // older than the window stays behind on the next pull too.
            last_uid = selected
                .uid_next
                .ok_or_else(|| anyhow!("[Gmail]/All Mail reported no UIDNEXT"))?
                .saturating_sub(1);
        }
        return Ok(PullOutcome {
            uidvalidity,
            last_uid,
            fetched: 0,
            more: false,
        });
    }

    let (lo, hi) = (uids[0], *uids.last().unwrap());
    let personal = feed.uids_in_category(lo, hi, "personal")?;

    let mut fetched = 0usize;
    for chunk in uids.chunks(opts.batch.max(1)) {
        let mut msgs = feed.fetch_batch(chunk)?;
        for m in &mut msgs {
            merge_personal(&mut m.labels, m.uid, &personal);
            last_uid = last_uid.max(m.uid);
        }
        fetched += msgs.len();
        sink(msgs)?;
    }
    // Advance past UIDs that fetched nothing (expunged mid-pull) so
    // they aren't re-asked forever.
    last_uid = last_uid.max(hi);

    Ok(PullOutcome {
        uidvalidity,
        last_uid,
        fetched,
        more,
    })
}

/// How far back a re-read of labels reaches for the newest mail
/// (`current_labels`).
pub const RELABEL_DAYS: u32 = 30;
/// At most this many of the newest messages are re-read per pass.
pub const RELABEL_CAP: usize = 500;
/// Messages per labels-only FETCH.
const RELABEL_BATCH: usize = 100;
/// Message-IDs per search when finding messages by Message-ID.
const LOOKUP_BATCH: usize = 20;

/// One archived message the caller wants re-read: the UID its archive
/// record carries (None: the record has none) and the Message-ID it was
/// archived under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    pub uid: Option<u32>,
    pub message_id: String,
}

/// Gmail's labels as they are now for the mail a pull already holds: the
/// messages of the last `days` days at or below the cursor (the newest
/// `cap` of them), and the archived messages in `also` (the caller's pass
/// over older mail), each with `Category Personal` merged as `pull_new`
/// does. Labels change after a message is pulled (starred later, moved
/// between categories); the pull itself only ever reads new UIDs. Empty
/// when the cursor belongs to another UIDVALIDITY or records no position.
///
/// An archived UID counts only when Gmail still has the message it was
/// archived as there: a UID can name another message after Gmail renumbers
/// the mailbox (a new UIDVALIDITY), and its record keeps the old number. A
/// message `also` names that its UID didn't find is looked up by its
/// Message-ID. Every UID read, however found, is at or below the cursor.
pub fn current_labels(
    feed: &mut dyn MailFeed,
    cursor: GmailCursor,
    days: u32,
    cap: usize,
    also: &[Archived],
) -> Result<Vec<CurrentLabels>> {
    let selected = feed.select_all_mail()?;
    if selected.uidvalidity != cursor.uidvalidity || cursor.last_uid == 0 {
        return Ok(vec![]);
    }
    let pulled = |uids: Vec<u32>| -> Vec<u32> {
        let mut uids: Vec<u32> = uids
            .into_iter()
            .filter(|u| *u > 0 && *u <= cursor.last_uid)
            .collect();
        uids.sort_unstable();
        uids.dedup();
        uids
    };
    let recent = pulled(feed.uids_since_days(days)?);
    let recent = recent[recent.len().saturating_sub(cap)..].to_vec();
    let older: Vec<u32> = pulled(also.iter().filter_map(|a| a.uid).collect())
        .into_iter()
        .filter(|u| recent.binary_search(u).is_err())
        .collect();
    let mut out = labels_now(feed, &recent)?;
    out.extend(labels_now(feed, &older)?);

    // the archived messages none of those UIDs holds now: by Message-ID
    let found: HashSet<&str> = out.iter().filter_map(|c| c.message_id.as_deref()).collect();
    let mut missing: Vec<String> = also
        .iter()
        .map(|a| a.message_id.as_str())
        .filter(|id| !found.contains(id) && is_message_id(id))
        .map(str::to_string)
        .collect();
    missing.sort_unstable();
    missing.dedup();
    if missing.is_empty() {
        return Ok(out);
    }
    let read: HashSet<u32> = out.iter().map(|c| c.uid).collect();
    let mut found_by_id = Vec::new();
    for chunk in missing.chunks(LOOKUP_BATCH) {
        found_by_id.extend(feed.uids_of(chunk)?);
    }
    let uids: Vec<u32> = pulled(found_by_id)
        .into_iter()
        .filter(|u| !read.contains(u))
        .collect();
    let wanted: HashSet<&str> = missing.iter().map(String::as_str).collect();
    out.extend(labels_now(feed, &uids)?.into_iter().filter(|c| {
        c.message_id
            .as_deref()
            .is_some_and(|id| wanted.contains(id))
    }));
    Ok(out)
}

/// The labels of these UIDs (sorted), `Category Personal` merged in.
fn labels_now(feed: &mut dyn MailFeed, uids: &[u32]) -> Result<Vec<CurrentLabels>> {
    let (Some(&lo), Some(&hi)) = (uids.first(), uids.last()) else {
        return Ok(vec![]);
    };
    let personal = feed.uids_in_category(lo, hi, "personal")?;
    let mut out = Vec::with_capacity(uids.len());
    for chunk in uids.chunks(RELABEL_BATCH) {
        for mut now in feed.labels_of(chunk)? {
            merge_personal(&mut now.labels, now.uid, &personal);
            out.push(now);
        }
    }
    Ok(out)
}

/// The UID the live pull wrote on a message's mbox separator line
/// (`mbox_record`: `From gmail-imap uid=<n>`); None for any other line.
pub fn record_uid(separator: &str) -> Option<u32> {
    separator
        .trim_end()
        .strip_prefix(RECORD_PREFIX)?
        .parse()
        .ok()
}

/// How `mbox_record` opens a record: this, then the message's UID.
const RECORD_PREFIX: &str = "From gmail-imap uid=";

/// Whether two label lists name the same labels (order and case aside).
pub fn same_labels(a: &[String], b: &[String]) -> bool {
    let set = |list: &[String]| -> HashSet<String> {
        list.iter()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect()
    };
    set(a) == set(b)
}

/// Gmail system labels arrive as `\Sent`, `\Starred`, … — strip the
/// backslash so they match the Takeout archive's `X-Gmail-Labels`
/// vocabulary (the LabelFilter compares whole labels, case-insensitive).
pub fn normalize_label(raw: &str) -> String {
    raw.trim().trim_start_matches('\\').to_string()
}

/// Headers only Gmail may set: its label header and its `X-GM-*`
/// extension names. A sender can write any of them into a message, so
/// the stored copy keeps none of theirs — only the one we inject.
fn is_gmail_owned_header(name: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case("X-Gmail-Labels")
        || name
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("X-GM-"))
}

/// The message as it goes into the live mbox and the draft parser: CRLF
/// normalized to LF, an `X-Gmail-Labels` header injected first (the same
/// header Takeout writes, so LabelFilter/scan_headers see live and
/// archived mail identically), and `From `-at-line-start escaped so the
/// mbox separator scan can never split inside a message. Every other byte
/// is the message's own: 8-bit text in any charset is kept as it came, so
/// the archive can still be decoded by its declared charset.
///
/// The injected header is ALWAYS written, empty when Gmail returned no
/// labels, and any `X-Gmail-Labels`/`X-GM-*` header the sender put in
/// the message is dropped (folded lines too). Every reader takes the
/// first label header, so without this a sender's own
/// `X-Gmail-Labels: Starred` passed `LabelFilter::core_strata` and the
/// message materialized as Core mail.
pub fn message_bytes(msg: &PulledMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.raw.len() + 64);
    out.extend_from_slice(b"X-Gmail-Labels:");
    if !msg.labels.is_empty() {
        out.push(b' ');
        // One header line, whatever a label name holds.
        out.extend_from_slice(msg.labels.join(", ").replace(['\r', '\n'], " ").as_bytes());
    }
    out.push(b'\n');
    let mut in_headers = true;
    // A folded line at the very top has no header of its own — kept, it
    // would continue OURS. Drop it like a stripped header's folds.
    let mut dropping = true;
    for piece in msg.raw.split_inclusive(|b| *b == b'\n') {
        let line = match piece.strip_suffix(b"\r\n") {
            Some(rest) => [rest, b"\n".as_slice()].concat(),
            None => piece.to_vec(),
        };
        if in_headers {
            if String::from_utf8_lossy(&line).trim().is_empty() {
                in_headers = false; // the blank line ends the header block
            } else if line.starts_with(b" ") || line.starts_with(b"\t") {
                if dropping {
                    continue;
                }
            } else {
                dropping = line.iter().position(|b| *b == b':').is_some_and(|colon| {
                    is_gmail_owned_header(&String::from_utf8_lossy(&line[..colon]))
                });
                if dropping {
                    continue;
                }
            }
        }
        if line.starts_with(b"From ") {
            out.push(b'>');
        }
        out.extend_from_slice(&line);
    }
    if out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out
}

/// [`message_bytes`] read as text (invalid UTF-8 replaced) — for readers
/// that want a string; the archive and the draft parser take the bytes.
pub fn message_text(msg: &PulledMessage) -> String {
    String::from_utf8_lossy(&message_bytes(msg)).into_owned()
}

/// One appendable mbox record: separator line + message + the blank
/// line the next separator scan requires. Pure — the daemon does the IO.
pub fn mbox_record(msg: &PulledMessage) -> Vec<u8> {
    let mut rec = format!("{RECORD_PREFIX}{}\n", msg.uid).into_bytes();
    rec.extend_from_slice(&message_bytes(msg));
    rec.extend_from_slice(b"\n");
    rec
}

/// Gmail refused the sign-in itself — a wrong or revoked app password, or
/// an account that won't take one — as opposed to a network error or a
/// busy server. Only this stops a mailbox ([`is_auth_failure`]).
#[derive(Debug)]
pub struct SignInRefused(pub String);

impl std::fmt::Display for SignInRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gmail refused the sign-in: {}", self.0)
    }
}

impl std::error::Error for SignInRefused {}

/// Whether the text of a `NO` reply to LOGIN says the credentials
/// themselves were refused. Other `NO` replies (`[UNAVAILABLE] Temporary
/// System Problem`, too many simultaneous connections) pass with time.
fn refuses_credentials(reply: &str) -> bool {
    let reply = reply.to_ascii_lowercase();
    [
        "authenticationfailed",
        "invalid credentials",
        "web login required",
        "password required",
    ]
    .iter()
    .any(|w| reply.contains(w))
}

/// A failed LOGIN as an error: [`SignInRefused`] only for a `NO` reply
/// that refuses the credentials; anything else (a dropped connection, a
/// timeout, a temporary refusal) stays an ordinary error.
fn login_error(err: imap::Error) -> anyhow::Error {
    match &err {
        imap::Error::No(no) if refuses_credentials(&no.information) => {
            SignInRefused(no.information.clone()).into()
        }
        _ => anyhow::Error::new(err).context("gmail sign-in did not complete"),
    }
}

/// Whether an error is Gmail refusing the sign-in (a wrong or revoked app
/// password) rather than a network hiccup: retrying it fast only risks
/// Google locking the account. Decided by the error's type — the reply to
/// LOGIN ([`SignInRefused`]) — never by the words of a message.
pub fn is_auth_failure(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<SignInRefused>())
}

/// An IMAP stream whose reads always have a deadline. The imap crate
/// clears the read timeout when an IDLE ends (`set_read_timeout(None)`),
/// which would leave the reply to IDLE's DONE and every later read
/// unbounded; here "none" means the connection's ordinary `io` limit.
struct Bounded<S> {
    inner: S,
    io: Duration,
}

impl<S: Read> Read for Bounded<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Bounded<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<S: SetReadTimeout> SetReadTimeout for Bounded<S> {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> imap::Result<()> {
        self.inner
            .set_read_timeout(Some(timeout.unwrap_or(self.io)))
    }
}

/// Lookups that outlived their deadline and still wait on the system
/// resolver. One can't be interrupted: its thread stays until the resolver
/// answers. Past [`MAX_STALLED_LOOKUPS`] of them a connection fails at once
/// rather than leave another thread behind.
static STALLED_LOOKUPS: AtomicUsize = AtomicUsize::new(0);
const MAX_STALLED_LOOKUPS: usize = 4;

/// The addresses of `host`, looked up within `deadline`. The system
/// resolver has no deadline of its own, so the lookup runs on a thread of
/// its own and the caller stops waiting at the deadline.
fn resolve(host: &str, port: u16, deadline: Duration) -> Result<Vec<SocketAddr>> {
    resolve_with(host, port, deadline, &STALLED_LOOKUPS, |host, port| {
        (host, port).to_socket_addrs().map(Iterator::collect)
    })
}

/// [`resolve`] with the lookup and the count of stalled lookups given.
fn resolve_with(
    host: &str,
    port: u16,
    deadline: Duration,
    stalled: &'static AtomicUsize,
    lookup: impl FnOnce(&str, u16) -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
) -> Result<Vec<SocketAddr>> {
    if stalled.load(Ordering::SeqCst) >= MAX_STALLED_LOOKUPS {
        bail!("looking up {host}: earlier lookups are still waiting on the system resolver");
    }
    let (tx, rx) = mpsc::sync_channel(1);
    // true once the caller stopped waiting: the answer then has no reader,
    // and the thread counts itself out of `stalled` when it ends
    let gave_up = Arc::new(Mutex::new(false));
    let theirs = gave_up.clone();
    let name = host.to_string();
    std::thread::Builder::new()
        .name("gmail-lookup".into())
        .spawn(move || {
            let found = lookup(&name, port);
            let gave_up = theirs.lock().unwrap_or_else(|p| p.into_inner());
            if *gave_up {
                stalled.fetch_sub(1, Ordering::SeqCst);
            } else {
                let _ = tx.send(found);
            }
        })
        .context("starting the address lookup")?;
    let found = match rx.recv_timeout(deadline) {
        Ok(found) => found,
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("looking up {host} stopped"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let mut gave_up = gave_up.lock().unwrap_or_else(|p| p.into_inner());
            match rx.try_recv() {
                Ok(found) => found, // answered just now
                Err(_) => {
                    *gave_up = true;
                    stalled.fetch_add(1, Ordering::SeqCst);
                    bail!("looking up {host} took longer than {deadline:?}");
                }
            }
        }
    };
    found.with_context(|| format!("looking up {host}"))
}

/// A TCP connection with a deadline on looking up the host, on connecting
/// (each address in turn) and on every read and write after.
fn open_tcp(host: &str, port: u16, connect: Duration, io: Duration) -> Result<TcpStream> {
    let mut last = None;
    for addr in resolve(host, port, RESOLVE_TIMEOUT)? {
        match TcpStream::connect_timeout(&addr, connect) {
            Ok(tcp) => {
                tcp.set_read_timeout(Some(io))?;
                tcp.set_write_timeout(Some(io))?;
                return Ok(tcp);
            }
            Err(err) => last = Some(err),
        }
    }
    Err(match last {
        Some(err) => anyhow!("connecting to {host}:{port}: {err}"),
        None => anyhow!("{host} has no address"),
    })
}

/// Read the server's greeting and sign in.
fn sign_in<T: Read + Write>(
    mut client: imap::Client<T>,
    user: &str,
    password: &str,
) -> Result<imap::Session<T>> {
    client
        .read_greeting()
        .map_err(|e| anyhow::Error::new(e).context("gmail greeting"))?;
    client
        .login(user, password)
        .map_err(|(e, _)| login_error(e))
}

/// A signed-in Gmail session over TLS in which no step can wait forever:
/// connecting ([`CONNECT_TIMEOUT`]), the TLS handshake, and every read and
/// write after it ([`IO_TIMEOUT`]). Whoever holds it while it stalls gets
/// an error, not a hang.
fn open_gmail(user: &str, password: &str) -> Result<imap::Session<imap::Connection>> {
    let tcp = open_tcp(GMAIL_HOST, GMAIL_PORT, CONNECT_TIMEOUT, IO_TIMEOUT)?;
    let tls = native_tls::TlsConnector::new()
        .context("setting up TLS")?
        .connect(GMAIL_HOST, tcp)
        .map_err(|e| anyhow!("TLS handshake with {GMAIL_HOST} failed: {e}"))?;
    let stream: imap::Connection = Box::new(Bounded {
        inner: tls,
        io: IO_TIMEOUT,
    });
    sign_in(imap::Client::new(stream), user, password)
}

/// Real Gmail transport. Blocking (the pull pass runs in
/// spawn_blocking) over TLS to imap.gmail.com:993 with an app password.
pub struct ImapFeed {
    session: imap::Session<imap::Connection>,
    /// All Mail is selected for IDLE (`wait_for_change`).
    idling: bool,
}

impl ImapFeed {
    pub fn connect(user: &str, password: &str) -> Result<Self> {
        Ok(Self {
            session: open_gmail(user, password)?,
            idling: false,
        })
    }

    pub fn logout(mut self) {
        let _ = self.session.logout();
    }

    /// Wait on All Mail with IMAP IDLE (RFC 2177) until the mailbox
    /// changes — new mail, a flag, a deletion — or `limit` passes. True
    /// when it changed. The first call selects the mailbox; later calls
    /// re-issue IDLE on the same session (no new login). Keep `limit`
    /// well under 29 minutes, where servers may drop an idle client.
    pub fn wait_for_change(&mut self, limit: std::time::Duration) -> Result<bool> {
        if !self.idling {
            self.session.select("[Gmail]/All Mail")?;
            self.idling = true;
        }
        let mut idle = self.session.idle();
        idle.timeout(limit).keepalive(false);
        let outcome = idle.wait_while(imap::extensions::idle::stop_on_any)?;
        Ok(matches!(
            outcome,
            imap::extensions::idle::WaitOutcome::MailboxChanged
        ))
    }
}

/// One message Gmail's own search found: its UID, labels, raw header,
/// and the start of its text (raw MIME, enough to decode a few KB).
#[derive(Debug, Clone)]
pub struct FoundMessage {
    pub uid: u32,
    pub labels: Vec<String>,
    pub header: Vec<u8>,
    /// At most `SEARCH_TEXT_HEAD` bytes: a text this long may go on.
    pub text_head: Vec<u8>,
}

/// Bytes of a found message's text (`BODY[TEXT]`) a search reads.
pub const SEARCH_TEXT_HEAD: usize = 65_536;

/// One message read by `GmailSearch::fetch`: its UID, its labels, and its
/// raw bytes (cut at the cap asked for).
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub uid: u32,
    pub labels: Vec<String>,
    pub raw: Vec<u8>,
}

/// Read-only search of the whole mailbox with Gmail's own search (0.5.89):
/// the same syntax as the Gmail search box, over All Mail, server-side.
/// The mailbox is opened with EXAMINE and messages read with BODY.PEEK, so
/// nothing is ever marked read or changed. A session of its own: the
/// watch's IDLE session is never disturbed.
pub struct GmailSearch {
    session: imap::Session<imap::Connection>,
}

/// Whether `id` can be a Message-ID as fetched by (without its `<>`): one
/// token of at most 998 characters with no whitespace, control character,
/// quote, backslash, angle bracket or parenthesis. A real Message-ID has
/// none of them (RFC 5322 `msg-id`), and they are what would let the id
/// read as more than one term inside a Gmail search.
pub fn is_message_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= 998
        && !id.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '"' | '\\' | '<' | '>' | '(' | ')')
        })
}

/// An IMAP quoted string (RFC 3501): backslash and double quote escaped;
/// line breaks can't appear in one, so they become spaces.
fn quoted(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

impl GmailSearch {
    pub fn connect(user: &str, password: &str) -> Result<Self> {
        let mut session = open_gmail(user, password)?;
        // lets a search carry non-ASCII words (Gmail supports RFC 6855);
        // without it such a search is refused, never mangled
        let _ = session.run_command_and_check_ok("ENABLE UTF8=ACCEPT");
        session.examine("[Gmail]/All Mail")?;
        Ok(Self { session })
    }

    /// Messages matching a Gmail search, newest first, at most `limit`.
    pub fn search(&mut self, query: &str, limit: usize) -> Result<Vec<FoundMessage>> {
        let mut uids: Vec<u32> = self
            .session
            .uid_search(format!("X-GM-RAW {}", quoted(query)))?
            .into_iter()
            .collect();
        uids.sort_unstable_by(|a, b| b.cmp(a));
        uids.truncate(limit);
        self.found(&uids)
    }

    fn found(&mut self, uids: &[u32]) -> Result<Vec<FoundMessage>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        let set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetches = self.session.uid_fetch(
            set,
            format!("(UID X-GM-LABELS BODY.PEEK[HEADER] BODY.PEEK[TEXT]<0.{SEARCH_TEXT_HEAD}>)"),
        )?;
        let mut out: Vec<FoundMessage> = fetches
            .iter()
            .filter_map(|f| {
                Some(FoundMessage {
                    uid: f.uid?,
                    labels: f
                        .gmail_labels()
                        .map(|it| it.map(normalize_label).filter(|l| !l.is_empty()).collect())
                        .unwrap_or_default(),
                    header: f.header().unwrap_or_default().to_vec(),
                    text_head: f.text().unwrap_or_default().to_vec(),
                })
            })
            .collect();
        // newest first, as asked
        out.sort_unstable_by_key(|m| std::cmp::Reverse(m.uid));
        Ok(out)
    }

    /// The raw message with this Message-ID (Gmail's `rfc822msgid:`), or
    /// with this UID (`uid:<n>`), cut at `cap` bytes. None: not found — or
    /// not a Message-ID (`is_message_id`): the id goes into a Gmail search,
    /// so anything that could read as more search terms finds nothing.
    /// Gmail's match is a search, so a caller that needs this exact message
    /// checks the Message-ID of what comes back.
    pub fn fetch(&mut self, id: &str, cap: usize) -> Result<Option<FetchedMessage>> {
        let uid = match id.strip_prefix("uid:").and_then(|u| u.parse::<u32>().ok()) {
            Some(uid) => Some(uid),
            None if !is_message_id(id) => return Ok(None),
            None => self
                .session
                .uid_search(format!("X-GM-RAW {}", quoted(&format!("rfc822msgid:{id}"))))?
                .into_iter()
                .max(),
        };
        let Some(uid) = uid else {
            return Ok(None);
        };
        let fetches = self.session.uid_fetch(
            uid.to_string(),
            format!("(UID X-GM-LABELS BODY.PEEK[]<0.{cap}>)"),
        )?;
        Ok(fetches.iter().find_map(|f| {
            Some(FetchedMessage {
                uid: f.uid.unwrap_or(uid),
                labels: f
                    .gmail_labels()
                    .map(|it| it.map(normalize_label).filter(|l| !l.is_empty()).collect())
                    .unwrap_or_default(),
                raw: f.body()?.to_vec(),
            })
        }))
    }

    pub fn logout(mut self) {
        let _ = self.session.logout();
    }
}

/// Save a draft in the mailbox's Drafts folder (0.5.94) — the folder
/// marked `\Drafts` (RFC 6154; Gmail names it by the account's language),
/// `[Gmail]/Drafts` when none is marked. It is APPENDed with the `\Draft`
/// and `\Seen` flags in a session of its own and nothing else is touched:
/// this never sends, and never changes or deletes another message.
/// Returns the folder it went to.
pub fn save_draft(user: &str, password: &str, raw: &[u8]) -> Result<String> {
    if !raw.is_ascii() {
        bail!("a draft must be 7-bit (the composer encodes everything else)");
    }
    let mut session = open_gmail(user, password)?;
    let out = (|| -> Result<String> {
        let names = session.list(None, Some("*"))?;
        let folder = names
            .iter()
            .find(|n| {
                n.attributes()
                    .iter()
                    .any(|a| matches!(a, imap_proto::NameAttribute::Drafts))
            })
            .map(|n| n.name().to_string())
            .unwrap_or_else(|| "[Gmail]/Drafts".to_string());
        if folder.contains('"') || folder.contains('\\') {
            bail!("the Drafts folder's name can't be used ({folder:?})");
        }
        session
            .append(&folder, raw)
            .flag(imap::types::Flag::Draft)
            .flag(imap::types::Flag::Seen)
            .finish()?;
        Ok(folder)
    })();
    let _ = session.logout();
    out
}

impl MailFeed for ImapFeed {
    fn select_all_mail(&mut self) -> Result<Selected> {
        let mailbox = self.session.select("[Gmail]/All Mail")?;
        Ok(Selected {
            uidvalidity: mailbox
                .uid_validity
                .ok_or_else(|| anyhow!("[Gmail]/All Mail reported no UIDVALIDITY"))?,
            uid_next: mailbox.uid_next,
        })
    }

    fn uids_after(&mut self, last_uid: u32) -> Result<Vec<u32>> {
        let found = self
            .session
            .uid_search(format!("UID {}:*", last_uid.saturating_add(1)))?;
        Ok(found.into_iter().collect())
    }

    fn uids_since_days(&mut self, days: u32) -> Result<Vec<u32>> {
        let date = (chrono::Utc::now() - chrono::Duration::days(days as i64)).format("%d-%b-%Y");
        let found = self.session.uid_search(format!("SINCE {date}"))?;
        Ok(found.into_iter().collect())
    }

    fn uids_in_category(&mut self, lo: u32, hi: u32, category: &str) -> Result<HashSet<u32>> {
        Ok(self
            .session
            .uid_search(format!("UID {lo}:{hi} X-GM-RAW \"category:{category}\""))?)
    }

    fn fetch_batch(&mut self, uids: &[u32]) -> Result<Vec<PulledMessage>> {
        let set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetches = self
            .session
            .uid_fetch(set, "(UID X-GM-LABELS BODY.PEEK[])")?;
        let mut out = Vec::new();
        for f in fetches.iter() {
            let Some(uid) = f.uid else { continue };
            let Some(body) = f.body() else { continue };
            let labels = f
                .gmail_labels()
                .map(|it| {
                    it.map(normalize_label)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            out.push(PulledMessage {
                uid,
                labels,
                raw: body.to_vec(),
            });
        }
        Ok(out)
    }

    fn labels_of(&mut self, uids: &[u32]) -> Result<Vec<CurrentLabels>> {
        if uids.is_empty() {
            return Ok(vec![]);
        }
        let set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fetches = self.session.uid_fetch(
            set,
            "(UID X-GM-LABELS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])",
        )?;
        Ok(fetches
            .iter()
            .filter_map(|f| {
                Some(CurrentLabels {
                    uid: f.uid?,
                    labels: f
                        .gmail_labels()
                        .map(|it| it.map(normalize_label).filter(|l| !l.is_empty()).collect())
                        .unwrap_or_default(),
                    message_id: f
                        .header()
                        .and_then(|h| crate::mbox::message_id_ref(&String::from_utf8_lossy(h))),
                })
            })
            .collect())
    }

    fn uids_of(&mut self, message_ids: &[String]) -> Result<Vec<u32>> {
        // one Gmail search per call; an id that could read as more than one
        // search term is left out (`is_message_id`)
        let terms: Vec<String> = message_ids
            .iter()
            .filter(|id| is_message_id(id))
            .map(|id| format!("rfc822msgid:{id}"))
            .collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let query = terms.join(" OR ");
        Ok(self
            .session
            .uid_search(format!("X-GM-RAW {}", quoted(&query)))?
            .into_iter()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_a_single_message_id_is_looked_up() {
        for id in [
            "CAF=abc123+x@mail.example.com",
            "p1@neutral.example",
            "a.b-c_d$e%f!g@[192.0.2.1]",
        ] {
            assert!(is_message_id(id), "{id}");
        }
        for id in [
            "",
            "a@x OR from:bank.example",
            "a@x\tfrom:bank.example",
            "a@x\"",
            "a@x\\",
            "<a@x>",
            "a@x (from:bank.example)",
            "a@x\r\nfrom:bank.example",
        ] {
            assert!(!is_message_id(id), "{id:?}");
        }
        assert!(!is_message_id(&"a".repeat(999)));
    }

    #[test]
    fn test_search_text_is_a_safe_quoted_string() {
        assert_eq!(quoted("from:susan portal"), "\"from:susan portal\"");
        assert_eq!(
            quoted("say \"hi\" \\ there\r\nnow"),
            "\"say \\\"hi\\\" \\\\ there  now\""
        );
    }

    struct FakeFeed {
        uidvalidity: u32,
        messages: Vec<PulledMessage>,
        personal: HashSet<u32>,
        since_window: Vec<u32>,
    }

    impl MailFeed for FakeFeed {
        fn select_all_mail(&mut self) -> Result<Selected> {
            Ok(Selected {
                uidvalidity: self.uidvalidity,
                uid_next: Some(self.messages.iter().map(|m| m.uid).max().unwrap_or(0) + 1),
            })
        }
        fn uids_after(&mut self, last_uid: u32) -> Result<Vec<u32>> {
            let mut v: Vec<u32> = self
                .messages
                .iter()
                .map(|m| m.uid)
                .filter(|u| *u > last_uid)
                .collect();
            // Model the RFC 3501 `n:*` quirk: the highest UID always
            // matches, even when it is below n.
            if v.is_empty() {
                if let Some(max) = self.messages.iter().map(|m| m.uid).max() {
                    v.push(max);
                }
            }
            Ok(v)
        }
        fn uids_since_days(&mut self, _days: u32) -> Result<Vec<u32>> {
            Ok(self.since_window.clone())
        }
        fn uids_in_category(&mut self, lo: u32, hi: u32, category: &str) -> Result<HashSet<u32>> {
            assert_eq!(category, "personal");
            Ok(self
                .personal
                .iter()
                .copied()
                .filter(|u| *u >= lo && *u <= hi)
                .collect())
        }
        fn fetch_batch(&mut self, uids: &[u32]) -> Result<Vec<PulledMessage>> {
            Ok(self
                .messages
                .iter()
                .filter(|m| uids.contains(&m.uid))
                .cloned()
                .collect())
        }
        fn labels_of(&mut self, uids: &[u32]) -> Result<Vec<CurrentLabels>> {
            Ok(self
                .messages
                .iter()
                .filter(|m| uids.contains(&m.uid))
                .map(|m| CurrentLabels {
                    uid: m.uid,
                    labels: m.labels.clone(),
                    message_id: crate::mbox::message_id_ref(&String::from_utf8_lossy(&m.raw)),
                })
                .collect())
        }
        fn uids_of(&mut self, message_ids: &[String]) -> Result<Vec<u32>> {
            Ok(self
                .messages
                .iter()
                .filter(|m| {
                    crate::mbox::message_id_ref(&String::from_utf8_lossy(&m.raw))
                        .is_some_and(|id| message_ids.contains(&id))
                })
                .map(|m| m.uid)
                .collect())
        }
    }

    fn msg(uid: u32, labels: &[&str]) -> PulledMessage {
        PulledMessage {
            uid,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            raw: format!(
                "From: A <a@x.example>\r\nSubject: m{uid}\r\nDate: Mon, 05 Jan 2026 10:00:00 -0400\r\nMessage-ID: <m{uid}@x>\r\n\r\nbody {uid}\r\n"
            )
            .into_bytes(),
        }
    }

    #[test]
    fn test_first_run_uses_backfill_window_and_annotates_category() {
        let mut feed = FakeFeed {
            uidvalidity: 7,
            messages: vec![msg(10, &["Sent"]), msg(11, &[]), msg(12, &[])],
            personal: HashSet::from([11]),
            since_window: vec![10, 11, 12],
        };
        let mut got = Vec::new();
        let out = pull_new(&mut feed, None, &PullOptions::default(), |b| {
            got.extend(b);
            Ok(())
        })
        .unwrap();
        assert_eq!(out.uidvalidity, 7);
        assert_eq!(out.last_uid, 12);
        assert_eq!(out.fetched, 3);
        assert!(!out.more);
        let m11 = got.iter().find(|m| m.uid == 11).unwrap();
        assert_eq!(m11.labels, vec!["Category Personal"]);
        let m12 = got.iter().find(|m| m.uid == 12).unwrap();
        assert!(m12.labels.is_empty());
    }

    #[test]
    fn test_cursor_resume_and_uid_star_quirk() {
        let mut feed = FakeFeed {
            uidvalidity: 7,
            messages: vec![msg(10, &[]), msg(11, &[])],
            personal: HashSet::new(),
            since_window: vec![],
        };
        // Cursor at the tip: the n:* quirk returns uid 11 anyway; the
        // retain guard must yield an empty pull, cursor unchanged.
        let out = pull_new(
            &mut feed,
            Some(GmailCursor {
                uidvalidity: 7,
                last_uid: 11,
            }),
            &PullOptions::default(),
            |_| panic!("nothing should be fetched"),
        )
        .unwrap();
        assert_eq!(out.fetched, 0);
        assert_eq!(out.last_uid, 11);

        // Cursor behind: only the newer message arrives.
        let mut got = Vec::new();
        let out = pull_new(
            &mut feed,
            Some(GmailCursor {
                uidvalidity: 7,
                last_uid: 10,
            }),
            &PullOptions::default(),
            |b| {
                got.extend(b);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out.fetched, 1);
        assert_eq!(got[0].uid, 11);
        assert_eq!(out.last_uid, 11);
    }

    #[test]
    fn test_uidvalidity_change_reanchors() {
        let mut feed = FakeFeed {
            uidvalidity: 8, // generation changed
            messages: vec![msg(3, &[])],
            personal: HashSet::new(),
            since_window: vec![3],
        };
        let mut got = Vec::new();
        let out = pull_new(
            &mut feed,
            Some(GmailCursor {
                uidvalidity: 7,
                last_uid: 9999,
            }),
            &PullOptions::default(),
            |b| {
                got.extend(b);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out.uidvalidity, 8);
        assert_eq!(out.fetched, 1, "re-anchor must use the backfill window");
        assert_eq!(out.last_uid, 3);
    }

    /// A mailbox whose mail is all older than the backfill window: the first
    /// pull starts at the mailbox's end, and the next pull brings only what
    /// arrived since — never the mailbox's whole history.
    #[test]
    fn test_an_empty_first_window_starts_at_the_mailbox_end() {
        let mut feed = FakeFeed {
            uidvalidity: 7,
            messages: (1..=5).map(|u| msg(u, &["Sent"])).collect(),
            personal: HashSet::new(),
            since_window: vec![],
        };
        let out = pull_new(&mut feed, None, &PullOptions::default(), |_| {
            panic!("nothing in the window is fetched")
        })
        .unwrap();
        assert_eq!(out.fetched, 0);
        assert_eq!(out.last_uid, 5, "anchored at UIDNEXT - 1");

        let cursor = Some(GmailCursor {
            uidvalidity: out.uidvalidity,
            last_uid: out.last_uid,
        });
        let out = pull_new(&mut feed, cursor, &PullOptions::default(), |_| {
            panic!("old mail stays behind")
        })
        .unwrap();
        assert_eq!(out.fetched, 0);
        assert_eq!(out.last_uid, 5);

        // new mail after the anchor is pulled
        feed.messages.push(msg(6, &[]));
        let mut got = Vec::new();
        let out = pull_new(
            &mut feed,
            Some(GmailCursor {
                uidvalidity: 7,
                last_uid: 5,
            }),
            &PullOptions::default(),
            |b| {
                got.extend(b);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(got.iter().map(|m| m.uid).collect::<Vec<_>>(), vec![6]);
        assert_eq!(out.last_uid, 6);
    }

    /// A cursor at 0 records no position (what an empty first window was
    /// stored as before): it re-anchors through the window, not `UID 1:*`.
    #[test]
    fn test_a_cursor_at_zero_reanchors_through_the_window() {
        let mut feed = FakeFeed {
            uidvalidity: 7,
            messages: (1..=5).map(|u| msg(u, &[])).collect(),
            personal: HashSet::new(),
            since_window: vec![],
        };
        let out = pull_new(
            &mut feed,
            Some(GmailCursor {
                uidvalidity: 7,
                last_uid: 0,
            }),
            &PullOptions::default(),
            |_| panic!("the window is empty"),
        )
        .unwrap();
        assert_eq!(out.fetched, 0);
        assert_eq!(out.last_uid, 5);

        // an empty mailbox anchors at 0, and its first mail is pulled
        let mut empty = FakeFeed {
            uidvalidity: 7,
            messages: vec![],
            personal: HashSet::new(),
            since_window: vec![],
        };
        let out = pull_new(&mut empty, None, &PullOptions::default(), |_| Ok(())).unwrap();
        assert_eq!(out.last_uid, 0);
        empty.messages.push(msg(1, &[]));
        empty.since_window = vec![1];
        let cursor = Some(GmailCursor {
            uidvalidity: 7,
            last_uid: 0,
        });
        let out = pull_new(&mut empty, cursor, &PullOptions::default(), |_| Ok(())).unwrap();
        assert_eq!(out.fetched, 1);
        assert_eq!(out.last_uid, 1);
    }

    /// The labels re-read covers mail at or below the cursor in the window,
    /// newest first up to the cap, with the category merged in.
    #[test]
    fn test_current_labels_rereads_pulled_mail() {
        let mut feed = FakeFeed {
            uidvalidity: 7,
            messages: vec![msg(1, &[]), msg(2, &["Starred"]), msg(3, &[]), msg(4, &[])],
            personal: HashSet::from([3]),
            since_window: vec![2, 3, 4],
        };
        let cursor = GmailCursor {
            uidvalidity: 7,
            last_uid: 3,
        };
        let now = current_labels(&mut feed, cursor, 30, 10, &[]).unwrap();
        assert_eq!(
            now,
            vec![
                CurrentLabels {
                    uid: 2,
                    labels: vec!["Starred".into()],
                    message_id: Some("m2@x".into()),
                },
                CurrentLabels {
                    uid: 3,
                    labels: vec!["Category Personal".into()],
                    message_id: Some("m3@x".into()),
                },
            ],
            "uid 1 is outside the window, uid 4 not pulled yet"
        );
        let newest = current_labels(&mut feed, cursor, 30, 1, &[]).unwrap();
        assert_eq!(newest.iter().map(|c| c.uid).collect::<Vec<_>>(), vec![3]);
        // older mail named by the caller is read too, once, and never past
        // the cursor
        let archived = |uid: u32| Archived {
            uid: Some(uid),
            message_id: format!("m{uid}@x"),
        };
        let with_older = current_labels(
            &mut feed,
            cursor,
            30,
            1,
            &[archived(1), archived(3), archived(4), archived(0)],
        )
        .unwrap();
        assert_eq!(
            with_older.iter().map(|c| c.uid).collect::<Vec<_>>(),
            vec![3, 1]
        );
        // another generation, or no position: nothing to re-read
        let other = GmailCursor {
            uidvalidity: 8,
            last_uid: 3,
        };
        assert!(current_labels(&mut feed, other, 30, 10, &[archived(1)])
            .unwrap()
            .is_empty());
        let none = GmailCursor {
            uidvalidity: 7,
            last_uid: 0,
        };
        assert!(current_labels(&mut feed, none, 30, 10, &[archived(1)])
            .unwrap()
            .is_empty());

        assert!(same_labels(
            &["Starred".into(), "Sent".into()],
            &["sent".into(), " Starred".into()]
        ));
        assert!(!same_labels(
            &["Sent".into()],
            &["Sent".into(), "Starred".into()]
        ));
    }

    /// After Gmail renumbers the mailbox (a new UIDVALIDITY), an archived
    /// record's UID names another message or none: the message it was
    /// archived as is found by its Message-ID, and the one now at that UID
    /// is reported under its own.
    #[test]
    fn test_archived_mail_is_found_again_after_renumbering() {
        let renumbered = |uid: u32, id: u32, labels: &[&str]| {
            let mut m = msg(id, labels);
            m.uid = uid;
            m
        };
        let mut feed = FakeFeed {
            uidvalidity: 9,
            // archived as m100 at uid 100 and m200 at uid 200; now m100 is
            // uid 2000 and uid 100 holds m7, and m200 is gone
            messages: vec![renumbered(100, 7, &[]), renumbered(2000, 100, &["Starred"])],
            personal: HashSet::new(),
            since_window: vec![],
        };
        let cursor = GmailCursor {
            uidvalidity: 9,
            last_uid: 2500,
        };
        let also = [
            Archived {
                uid: Some(100),
                message_id: "m100@x".into(),
            },
            Archived {
                uid: Some(200),
                message_id: "m200@x".into(),
            },
            // a record without a UID, or with an id no search may carry
            Archived {
                uid: None,
                message_id: "m100@x".into(),
            },
            Archived {
                uid: None,
                message_id: "x@y OR from:z".into(),
            },
        ];
        let now = current_labels(&mut feed, cursor, 30, 10, &also).unwrap();
        assert_eq!(
            now,
            vec![
                CurrentLabels {
                    uid: 100,
                    labels: vec![],
                    message_id: Some("m7@x".into()),
                },
                CurrentLabels {
                    uid: 2000,
                    labels: vec!["Starred".into()],
                    message_id: Some("m100@x".into()),
                },
            ]
        );
    }

    #[test]
    fn test_cap_limits_and_reports_more() {
        let mut feed = FakeFeed {
            uidvalidity: 7,
            messages: (1..=5).map(|u| msg(u, &[])).collect(),
            personal: HashSet::new(),
            since_window: (1..=5).collect(),
        };
        let mut got = Vec::new();
        let out = pull_new(
            &mut feed,
            None,
            &PullOptions {
                backfill_days: 90,
                cap: 3,
                batch: 2,
            },
            |b| {
                got.extend(b);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out.fetched, 3);
        assert_eq!(out.last_uid, 3, "cursor stops at the cap boundary");
        assert!(out.more);
    }

    #[test]
    fn test_normalize_label() {
        assert_eq!(normalize_label("\\Sent"), "Sent");
        assert_eq!(normalize_label("\\Starred"), "Starred");
        assert_eq!(normalize_label(" My Label "), "My Label");
    }

    #[test]
    fn test_message_text_injects_labels_normalizes_and_escapes() {
        let m = PulledMessage {
            uid: 5,
            labels: vec!["Sent".into(), "Category Personal".into()],
            raw: b"Subject: hi\r\n\r\nFrom the top\r\nbody\r\n".to_vec(),
        };
        let text = message_text(&m);
        assert!(text.starts_with("X-Gmail-Labels: Sent, Category Personal\n"));
        assert!(!text.contains('\r'));
        assert!(text.contains(">From the top"), "got: {text}");

        let rec = String::from_utf8(mbox_record(&m)).unwrap();
        assert!(rec.starts_with("From gmail-imap uid=5\n"));
        assert!(rec.ends_with("\n\n"), "record must end with a blank line");
        assert_eq!(record_uid(rec.lines().next().unwrap()), Some(5));
        assert_eq!(record_uid("From gmail-imap uid=12\r\n"), Some(12));
        for other in [
            "From a@x Mon Jan 05 10:00:00 2026",
            "From gmail-imap uid=",
            "From gmail-imap uid=5x",
        ] {
            assert_eq!(record_uid(other), None, "{other}");
        }
    }

    /// A lookup the system resolver doesn't answer fails at the deadline;
    /// once several are stuck, the next fails at once without starting
    /// another; each counts itself out when the resolver finally answers.
    #[test]
    fn test_a_stalled_address_lookup_fails_within_its_deadline() {
        static STALLED: AtomicUsize = AtomicUsize::new(0);
        let deadline = Duration::from_millis(100);
        let mut release = Vec::new();
        for _ in 0..MAX_STALLED_LOOKUPS {
            let (tx, rx) = mpsc::channel::<()>();
            release.push(tx);
            let started = std::time::Instant::now();
            let err = resolve_with("imap.example", 993, deadline, &STALLED, move |_, _| {
                let _ = rx.recv();
                Err(std::io::Error::other("no answer"))
            })
            .unwrap_err();
            assert!(started.elapsed() < Duration::from_secs(2), "{err:#}");
            assert!(format!("{err:#}").contains("took longer"), "{err:#}");
        }
        assert_eq!(STALLED.load(Ordering::SeqCst), MAX_STALLED_LOOKUPS);
        let err = resolve_with("imap.example", 993, deadline, &STALLED, |_, _| {
            panic!("no new lookup starts while earlier ones are stuck")
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("still waiting"), "{err:#}");

        drop(release); // the resolver answers at last
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while STALLED.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(STALLED.load(Ordering::SeqCst), 0);

        // an answer in time is used
        let addr: SocketAddr = "192.0.2.7:993".parse().unwrap();
        let found = resolve_with("imap.example", 993, deadline, &STALLED, move |_, _| {
            Ok(vec![addr])
        })
        .unwrap();
        assert_eq!(found, vec![addr]);
        let err = resolve_with("imap.example", 993, deadline, &STALLED, |_, _| {
            Err(std::io::Error::other("no such host"))
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("no such host"), "{err:#}");
        assert_eq!(STALLED.load(Ordering::SeqCst), 0);
    }

    /// 8-bit mail in a legacy charset reaches the archive byte for byte.
    #[test]
    fn test_eight_bit_mail_is_archived_as_received() {
        let m = PulledMessage {
            uid: 6,
            labels: vec!["Inbox".into()],
            raw: b"Subject: caf\xe9\r\nContent-Type: text/plain; charset=windows-1252\r\n\
                   Content-Transfer-Encoding: 8bit\r\n\r\nUn caf\xe9 cr\xe8me\r\nFrom here\r\n"
                .to_vec(),
        };
        let rec = mbox_record(&m);
        let body = b"Un caf\xe9 cr\xe8me\n>From here\n";
        assert!(
            rec.windows(body.len()).any(|w| w == body),
            "got: {:?}",
            String::from_utf8_lossy(&rec)
        );
        assert!(rec.windows(9).any(|w| w == b"caf\xe9\nCont"));
        assert!(
            !rec.windows(3).any(|w| w == "\u{fffd}".as_bytes()),
            "nothing replaced"
        );
        assert!(!rec.contains(&b'\r'));
        // the text form reads the same message
        assert_eq!(
            message_text(&m),
            String::from_utf8_lossy(&message_bytes(&m))
        );
    }

    /// How a loopback test server answers the client's first command.
    enum Answer {
        Reply(&'static str),
        Close,
        Silent,
    }

    /// One-connection IMAP server on loopback: greets (unless `greet` is
    /// false), reads the first command and answers it as told.
    fn test_server(greet: bool, answer: Answer) -> u16 {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            if !greet {
                std::thread::sleep(Duration::from_secs(3));
                return;
            }
            sock.write_all(b"* OK Gimap ready\r\n").unwrap();
            let mut line = Vec::new();
            let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
            let _ = reader.read_until(b'\n', &mut line);
            let tag = String::from_utf8_lossy(&line)
                .split(' ')
                .next()
                .unwrap_or("a1")
                .to_string();
            match answer {
                Answer::Reply(reply) => {
                    let _ = sock.write_all(format!("{tag} {reply}\r\n").as_bytes());
                    std::thread::sleep(Duration::from_millis(200));
                }
                Answer::Close => {}
                Answer::Silent => std::thread::sleep(Duration::from_secs(3)),
            }
        });
        port
    }

    fn sign_in_to(port: u16, io: Duration) -> anyhow::Error {
        let tcp = open_tcp("127.0.0.1", port, Duration::from_secs(5), io).unwrap();
        match sign_in(
            imap::Client::new(Bounded { inner: tcp, io }),
            "a@x.example",
            "pw",
        ) {
            Ok(_) => panic!("the test server never signs anyone in"),
            Err(err) => err,
        }
    }

    /// Only a `NO` reply refusing the credentials stops a mailbox; a dropped
    /// connection, a temporary refusal or a connection limit don't.
    #[test]
    fn test_only_a_refused_password_is_an_auth_failure() {
        let io = Duration::from_secs(5);
        for refused in [
            "NO [AUTHENTICATIONFAILED] Invalid credentials (Failure)",
            "NO [ALERT] Application-specific password required: https://support.google.com/accounts/answer/185833 (Failure)",
            "NO [WEBALERT https://accounts.google.com/x] Web login required.",
        ] {
            let err = sign_in_to(test_server(true, Answer::Reply(refused)), io);
            assert!(is_auth_failure(&err), "{refused}: {err:#}");
            assert!(is_auth_failure(&err.context("pulling mail")), "{refused}");
        }
        for passing in [
            "NO [UNAVAILABLE] Temporary System Problem. Try again later (Failure)",
            "NO [ALERT] Too many simultaneous connections. (Failure)",
            "BAD [CLIENTBUG] Invalid command",
        ] {
            let err = sign_in_to(test_server(true, Answer::Reply(passing)), io);
            assert!(!is_auth_failure(&err), "{passing}: {err:#}");
        }
        let err = sign_in_to(test_server(true, Answer::Close), io);
        assert!(!is_auth_failure(&err), "dropped: {err:#}");
        // the words alone never count
        assert!(!is_auth_failure(&anyhow!(
            "gmail login failed: No Response: [AUTHENTICATIONFAILED] Invalid credentials"
        )));
        assert!(!is_auth_failure(&anyhow!(
            "gmail login failed: Connection Lost"
        )));
    }

    /// A server that stops answering — before its greeting, or after LOGIN —
    /// fails the sign-in within the read deadline instead of holding it.
    #[test]
    fn test_a_silent_server_fails_within_the_deadline() {
        let io = Duration::from_millis(300);
        for port in [
            test_server(false, Answer::Silent),
            test_server(true, Answer::Silent),
        ] {
            let started = std::time::Instant::now();
            let err = sign_in_to(port, io);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "took {:?}",
                started.elapsed()
            );
            assert!(!is_auth_failure(&err), "{err:#}");
        }
    }

    /// The imap crate clears the read timeout after IDLE; the stream keeps
    /// its ordinary deadline instead.
    #[test]
    fn test_clearing_the_read_timeout_keeps_the_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let probe = tcp.try_clone().unwrap();
        let mut stream = Bounded {
            inner: tcp,
            io: Duration::from_secs(7),
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(probe.read_timeout().unwrap(), Some(Duration::from_secs(2)));
        stream.set_read_timeout(None).unwrap();
        assert_eq!(probe.read_timeout().unwrap(), Some(Duration::from_secs(7)));
    }

    fn core_draft(text: &str) -> Option<hearthloom_core::EpisodeDraft> {
        crate::mbox::draft_from_message_filtered(
            text,
            &crate::mbox::LabelFilter::core_strata(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_sender_cannot_forge_gmail_labels() {
        // Gmail returned no labels; the sender wrote their own label
        // header, and X-GM-* lookalikes (odd casing, one folded).
        let forged = PulledMessage {
            uid: 9,
            labels: vec![],
            raw: b"From: Mallory <m@evil.example>\r\n\
                   X-Gmail-Labels: Starred,Sent\r\n\
                   x-gm-thrid: 42\r\n\
                   X-GM-Labels: \\Starred\r\n \\Sent\r\n\
                   Subject: totally personal\r\n\
                   Date: Mon, 05 Jan 2026 10:00:00 -0400\r\n\
                   Message-ID: <forged1@evil.example>\r\n\
                   \r\n\
                   X-Gmail-Labels: in the body this is just text\r\n"
                .to_vec(),
        };
        let text = message_text(&forged);
        // Ours comes first, empty; theirs is gone, folded lines and all.
        assert!(
            text.starts_with("X-Gmail-Labels:\nFrom: Mallory"),
            "got: {text}"
        );
        let (head, body) = text.split_once("\n\n").unwrap();
        assert_eq!(head.matches("X-Gmail-Labels").count(), 1, "got: {head}");
        assert!(!head.to_ascii_lowercase().contains("x-gm-"), "got: {head}");
        assert!(
            !head.contains("Sent"),
            "a stripped header's folded line goes too: {head}"
        );
        assert!(head.contains("Subject: totally personal"));
        assert!(
            body.contains("X-Gmail-Labels: in the body this is just text"),
            "body untouched"
        );
        assert!(
            core_draft(&text).is_none(),
            "a forged Starred/Sent must not make Core mail"
        );

        // Gmail's own labels are the only ones that count.
        let promo = PulledMessage {
            labels: vec!["Category Promotions".into()],
            ..forged.clone()
        };
        let text = message_text(&promo);
        assert!(text.starts_with("X-Gmail-Labels: Category Promotions\nFrom: Mallory"));
        assert!(core_draft(&text).is_none());
        let sent = PulledMessage {
            labels: vec!["Sent".into()],
            ..forged
        };
        let draft = core_draft(&message_text(&sent)).expect("really Sent: Core mail");
        assert!(draft.body.contains("labels: Sent\n"), "got: {}", draft.body);
        assert!(!draft.body.contains("Starred"));
    }

    #[test]
    fn test_folded_line_at_top_cannot_extend_our_header() {
        // A message that opens with a folded line would otherwise continue
        // the header we inject: "X-Gmail-Labels: Starred" by the sender.
        let m = PulledMessage {
            uid: 10,
            labels: vec![],
            raw: b" Starred\r\n\tSent\r\nFrom: M <m@evil.example>\r\nSubject: s\r\n\
                   Date: Mon, 05 Jan 2026 10:00:00 -0400\r\n\r\nbody\r\n"
                .to_vec(),
        };
        let text = message_text(&m);
        assert!(text.starts_with("X-Gmail-Labels:\nFrom: M"), "got: {text}");
        assert!(core_draft(&text).is_none());
    }
}

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

use anyhow::{anyhow, Result};

/// Where a pull resumes. UIDs are only meaningful within one
/// UIDVALIDITY generation — a generation change re-anchors via the
/// backfill window (Message-ID dedupe absorbs any overlap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmailCursor {
    pub uidvalidity: u32,
    pub last_uid: u32,
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
    /// Select `[Gmail]/All Mail`; returns its UIDVALIDITY.
    fn select_all_mail(&mut self) -> Result<u32>;
    /// UIDs strictly greater than `last_uid`.
    fn uids_after(&mut self, last_uid: u32) -> Result<Vec<u32>>;
    /// UIDs of mail received in the last `days` days (first-run anchor).
    fn uids_since_days(&mut self, days: u32) -> Result<Vec<u32>>;
    /// Which of the UIDs in `lo..=hi` sit in a Gmail category
    /// (X-GM-RAW `category:<name>` — categories are NOT in X-GM-LABELS).
    fn uids_in_category(&mut self, lo: u32, hi: u32, category: &str) -> Result<HashSet<u32>>;
    fn fetch_batch(&mut self, uids: &[u32]) -> Result<Vec<PulledMessage>>;
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
    let uidvalidity = feed.select_all_mail()?;
    let anchored = match cursor {
        Some(c) if c.uidvalidity == uidvalidity => Some(c.last_uid),
        Some(c) => {
            tracing::warn!(
                "gmail UIDVALIDITY changed {} -> {uidvalidity}; re-anchoring via the {}-day backfill window",
                c.uidvalidity,
                opts.backfill_days
            );
            None
        }
        None => None,
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
            if personal.contains(&m.uid)
                && !m
                    .labels
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case("Category Personal"))
            {
                m.labels.push("Category Personal".to_string());
            }
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

/// The message text as it goes into the live mbox and the draft parser:
/// CRLF normalized to LF, an `X-Gmail-Labels` header injected first (the
/// same header Takeout writes, so LabelFilter/scan_headers see live and
/// archived mail identically), and `From `-at-line-start escaped so the
/// mbox separator scan can never split inside a message.
///
/// The injected header is ALWAYS written, empty when Gmail returned no
/// labels, and any `X-Gmail-Labels`/`X-GM-*` header the sender put in
/// the message is dropped (folded lines too). Every reader takes the
/// first label header, so without this a sender's own
/// `X-Gmail-Labels: Starred` passed `LabelFilter::core_strata` and the
/// message materialized as Core mail.
pub fn message_text(msg: &PulledMessage) -> String {
    let raw = String::from_utf8_lossy(&msg.raw).replace("\r\n", "\n");
    let mut out = String::with_capacity(raw.len() + 64);
    out.push_str("X-Gmail-Labels:");
    if !msg.labels.is_empty() {
        out.push(' ');
        // One header line, whatever a label name holds.
        out.push_str(&msg.labels.join(", ").replace(['\r', '\n'], " "));
    }
    out.push('\n');
    let mut in_headers = true;
    // A folded line at the very top has no header of its own — kept, it
    // would continue OURS. Drop it like a stripped header's folds.
    let mut dropping = true;
    for line in raw.split_inclusive('\n') {
        if in_headers {
            if line.trim().is_empty() {
                in_headers = false; // the blank line ends the header block
            } else if line.starts_with(' ') || line.starts_with('\t') {
                if dropping {
                    continue;
                }
            } else {
                dropping = line
                    .split_once(':')
                    .is_some_and(|(name, _)| is_gmail_owned_header(name));
                if dropping {
                    continue;
                }
            }
        }
        if line.starts_with("From ") {
            out.push('>');
        }
        out.push_str(line);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// One appendable mbox record: separator line + message + the blank
/// line the next separator scan requires. Pure — the daemon does the IO.
pub fn mbox_record(msg: &PulledMessage) -> Vec<u8> {
    let mut rec = format!("From gmail-imap uid={}\n", msg.uid).into_bytes();
    rec.extend_from_slice(message_text(msg).as_bytes());
    rec.extend_from_slice(b"\n");
    rec
}

/// Real Gmail transport. Blocking (the pull pass runs in
/// spawn_blocking) over TLS to imap.gmail.com:993 with an app password.
pub struct ImapFeed {
    session: imap::Session<imap::Connection>,
}

impl ImapFeed {
    pub fn connect(user: &str, password: &str) -> Result<Self> {
        let client = imap::ClientBuilder::new("imap.gmail.com", 993).connect()?;
        let session = client
            .login(user, password)
            .map_err(|(e, _)| anyhow!("gmail login failed: {e}"))?;
        Ok(Self { session })
    }

    pub fn logout(mut self) {
        let _ = self.session.logout();
    }
}

impl MailFeed for ImapFeed {
    fn select_all_mail(&mut self) -> Result<u32> {
        let mailbox = self.session.select("[Gmail]/All Mail")?;
        mailbox
            .uid_validity
            .ok_or_else(|| anyhow!("[Gmail]/All Mail reported no UIDVALIDITY"))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeFeed {
        uidvalidity: u32,
        messages: Vec<PulledMessage>,
        personal: HashSet<u32>,
        since_window: Vec<u32>,
    }

    impl MailFeed for FakeFeed {
        fn select_all_mail(&mut self) -> Result<u32> {
            Ok(self.uidvalidity)
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

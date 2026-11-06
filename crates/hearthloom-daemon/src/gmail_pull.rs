//! Gmail live federation (Tier 3): the consolidation pass that pulls
//! new mail over IMAP into a rolling live mbox next to the Takeout
//! archive, incrementally extends its sidecar index (searchable at
//! need, like the archive), and materializes the same Core strata
//! (Sent ∪ Category Personal ∪ Starred) as episodes.
//!
//! Dedupe against the archive is structural: live episodes use
//! `source: "mbox"` + Message-ID `source_ref` — the exact identity the
//! Takeout Tier-1 ingest minted — so `UNIQUE(source, source_ref)` +
//! `INSERT OR IGNORE` makes an overlap a silent no-op, and "mbox" stays
//! outside the LLM extraction allowlist (mail volume never burns
//! budget; entities are minted structurally by the adapter).
//!
//! Ordering is data-loss-safe: append → index → materialize → THEN
//! advance the cursor. A failure anywhere replays the same UIDs next
//! run; a replay can duplicate raw bytes in the live mbox (harmless —
//! dup index ids fall back to `at:<offset>`, same as the archive's
//! self-copies) but can never lose or double an episode.
//!
//! Each mailbox (mailboxes.rs, 0.5.98) has its own live mbox, sidecar and
//! cursor: one puller per mailbox, and every pull serialized (`PULL`). The
//! same message in two mailboxes is one memory (same Message-ID). A
//! refused sign-in stops that mailbox's pulls until it's saved again.

use std::io::Write;

use anyhow::{Context, Result};
use hearthloom_adapters::gmail_imap::{self, is_auth_failure, GmailCursor, ImapFeed, PullOptions};
use hearthloom_adapters::mbox::LabelFilter;
use hearthloom_core::store::Actor;
use hearthloom_core::IngestBatch;

use crate::mailboxes::{self, Mailbox};
use crate::AppState;

const CURSOR_KEY: &str = "gmail_cursor";
/// Episodes per store-lock hold while materializing (the
/// refresh_local_sources pattern — THE PASS CONTRACT).
const APPLY_CHUNK: usize = 200;

fn pull_options() -> PullOptions {
    let mut opts = PullOptions::default();
    if let Some(days) = std::env::var("GMAIL_IMAP_BACKFILL_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        opts.backfill_days = days;
    }
    opts
}

fn read_cursor(mbox: &std::path::Path) -> Option<GmailCursor> {
    let raw = crate::mbox_index::meta_get(mbox, CURSOR_KEY)
        .ok()
        .flatten()?;
    let (v, u) = raw.split_once(':')?;
    Some(GmailCursor {
        uidvalidity: v.parse().ok()?,
        last_uid: u.parse().ok()?,
    })
}

/// One pull at a time per node: the consolidation pass and the live
/// watches (gmail_watch.rs) all call `pull_once`, and two at once on one
/// mailbox would race on its cursor and append the same messages twice.
static PULL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One incremental pull of one mailbox. Blocking (network + file + store
/// IO) — the caller runs it whole in `spawn_blocking` (THE PASS
/// CONTRACT). Returns (messages_fetched, episodes_new, more_pending). A
/// mailbox Google refused isn't tried; a refusal now stops it.
pub(crate) fn pull_once(state: &AppState, mb: &Mailbox) -> Result<(usize, usize, bool)> {
    mailboxes::check_refused(mb)?;
    let _one_at_a_time = PULL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pull_once_unserialized(state, mb).inspect_err(|err| {
        if is_auth_failure(err) {
            mailboxes::refuse(Some(state), mb, err);
        }
    })
}

/// Every mailbox here, one after the other (the six-hour pass and "pull
/// now"); one Google refused is skipped. The totals, or the first error
/// when every mailbox failed.
pub(crate) fn pull_all(state: &AppState) -> Result<(usize, usize, bool)> {
    let (mut fetched, mut new, mut more) = (0, 0, false);
    let (mut ok, mut first_err) = (false, None);
    for mb in mailboxes::all() {
        if mailboxes::refused(&mb.id) {
            continue;
        }
        match pull_once(state, &mb) {
            Ok((f, n, m)) => {
                ok = true;
                fetched += f;
                new += n;
                more |= m;
            }
            Err(err) => {
                tracing::warn!("gmail pull: {}: {err:#}", mb.address);
                first_err.get_or_insert(err);
            }
        }
    }
    match first_err {
        Some(err) if !ok => Err(err),
        _ => Ok((fetched, new, more)),
    }
}

fn pull_once_unserialized(state: &AppState, mb: &Mailbox) -> Result<(usize, usize, bool)> {
    let (user, password) = (mb.address.as_str(), mb.password());
    // whose mail is the owner's own: every mailbox and alias (mailboxes.rs)
    let mine = mailboxes::owner_addresses(state);

    let mbox_path = mb.live_mbox_path(state);
    if let Some(dir) = mbox_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let created = !mbox_path.exists();
    if created {
        std::fs::File::create(&mbox_path)
            .with_context(|| format!("creating {}", mbox_path.display()))?;
    }
    // A fresh or externally-touched file needs a (cheap, from-empty or
    // small) full index before incremental appends can extend it.
    if crate::mbox_index::is_stale(&mbox_path) {
        crate::mbox_index::build(&mbox_path)?;
    }
    let cursor = read_cursor(&mbox_path);

    let mut feed = ImapFeed::connect(user, password)?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&mbox_path)?;
    let appended_from = file.metadata()?.len();

    let filter = LabelFilter::core_strata();
    let mut drafts = Vec::new();
    // what each message means for its thread (mail_triage.rs, 0.5.92)
    let mut seen = Vec::new();
    let outcome = gmail_imap::pull_new(&mut feed, cursor, &pull_options(), |msgs| {
        for m in &msgs {
            file.write_all(&gmail_imap::mbox_record(m))?;
            if let Some(s) = crate::mail_triage::seen(state, user, &mine, &m.labels, &m.raw) {
                seen.push(s);
            }
            match hearthloom_adapters::mbox::draft_from_message_filtered(
                &gmail_imap::message_text(m),
                &filter,
                None, // scope stays inferred (identity rules > personal)
            ) {
                Ok(Some(draft)) => drafts.push(draft),
                Ok(None) => {}
                Err(err) => tracing::warn!("gmail pull: skipping uid {}: {err}", m.uid),
            }
        }
        Ok(())
    })?;
    feed.logout();
    file.flush()?;
    drop(file);

    if outcome.fetched > 0 {
        crate::mbox_index::append_index(&mbox_path, appended_from)?;
    }

    let mut episodes_new = 0usize;
    for chunk in drafts.chunks(APPLY_CHUNK) {
        let report = state.store.apply_batch(
            &state.home,
            &IngestBatch {
                actor: "adapter:gmail-imap".into(),
                session_id: None,
                episodes: chunk.to_vec(),
            },
        )?;
        episodes_new += report.episodes_new;
        // Let parked HTTP handlers through between holds.
        std::thread::sleep(std::time::Duration::from_millis(60));
    }

    // threads after the memories (a replay observes nothing twice)
    crate::mail_triage::observe(state, &seen);

    // The cursor advances LAST: everything before this line is
    // idempotent under replay.
    crate::mbox_index::meta_set(
        &mbox_path,
        CURSOR_KEY,
        &format!("{}:{}", outcome.uidvalidity, outcome.last_uid),
    )?;

    if created {
        let name = mb.source_name();
        state.store.register_virtual_source(
            &Actor::new("agent:gmail-pull"),
            &name,
            "mbox",
            &serde_json::json!({
                "mount": mbox_path.to_string_lossy(),
                "trust": "imported",
                "account": mb.account(),
            }),
            &["search".into(), "fetch".into(), "promote".into()],
        )?;
        tracing::info!(
            "gmail pull: registered virtual source {name} ({})",
            mb.address
        );
    }

    Ok((outcome.fetched, episodes_new, outcome.more))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_adapters::gmail_imap::{MailFeed, PulledMessage};
    use std::collections::HashSet;

    struct FakeFeed {
        messages: Vec<PulledMessage>,
    }

    impl MailFeed for FakeFeed {
        fn select_all_mail(&mut self) -> Result<u32> {
            Ok(1)
        }
        fn uids_after(&mut self, last_uid: u32) -> Result<Vec<u32>> {
            Ok(self
                .messages
                .iter()
                .map(|m| m.uid)
                .filter(|u| *u > last_uid)
                .collect())
        }
        fn uids_since_days(&mut self, _days: u32) -> Result<Vec<u32>> {
            Ok(self.messages.iter().map(|m| m.uid).collect())
        }
        fn uids_in_category(&mut self, _lo: u32, _hi: u32, _c: &str) -> Result<HashSet<u32>> {
            Ok(HashSet::new())
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

    fn message(uid: u32, msgid: &str, labels: &[&str]) -> PulledMessage {
        PulledMessage {
            uid,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            raw: format!(
                "From: Ann <ann@x.example>\r\nTo: Jon <jon@y.example>\r\nSubject: live {uid}\r\n\
                 Date: Mon, 05 Jan 2026 10:0{uid}:00 -0400\r\nMessage-ID: <{msgid}>\r\n\r\n\
                 live body {uid}\r\n"
            )
            .into_bytes(),
        }
    }

    /// One pull as `pull_once` runs it, minus the network and the store:
    /// through `feed` into the live mbox + sidecar, cursor stamped last.
    /// Returns the Core drafts it would materialize.
    fn pull_into(
        mbox: &std::path::Path,
        feed: &mut FakeFeed,
        cursor: Option<GmailCursor>,
    ) -> (Vec<hearthloom_core::EpisodeDraft>, gmail_imap::PullOutcome) {
        let mut file = std::fs::OpenOptions::new().append(true).open(mbox).unwrap();
        let from = file.metadata().unwrap().len();
        let filter = LabelFilter::core_strata();
        let mut drafts = Vec::new();
        let outcome = gmail_imap::pull_new(feed, cursor, &PullOptions::default(), |msgs| {
            for m in &msgs {
                file.write_all(&gmail_imap::mbox_record(m)).unwrap();
                if let Ok(Some(d)) = hearthloom_adapters::mbox::draft_from_message_filtered(
                    &gmail_imap::message_text(m),
                    &filter,
                    None,
                ) {
                    drafts.push(d);
                }
            }
            Ok(())
        })
        .unwrap();
        drop(file);
        if outcome.fetched > 0 {
            crate::mbox_index::append_index(mbox, from).unwrap();
        }
        crate::mbox_index::meta_set(
            mbox,
            CURSOR_KEY,
            &format!("{}:{}", outcome.uidvalidity, outcome.last_uid),
        )
        .unwrap();
        (drafts, outcome)
    }

    /// The full loop minus the network: pull through a fake feed into a
    /// real live mbox + sidecar, materialize Core strata, verify the
    /// cursor, incremental re-index, dedupe, and vsearch reachability.
    #[test]
    fn test_pull_append_index_materialize_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let run = |feed: &mut FakeFeed, cursor| pull_into(&mbox, feed, cursor);

        // Round 1: a Sent message (kept) and a promo (virtual-only).
        let mut feed = FakeFeed {
            messages: vec![
                message(1, "live1@x", &["Sent"]),
                message(2, "live2@x", &["Category Promotions"]),
            ],
        };
        let (drafts, outcome) = run(&mut feed, None);
        assert_eq!(outcome.fetched, 2);
        assert_eq!(drafts.len(), 1, "only the Sent message materializes");
        assert_eq!(drafts[0].source, "mbox", "archive-convergent identity");
        assert_eq!(drafts[0].source_ref, "live1@x");
        assert_eq!(
            read_cursor(&mbox),
            Some(GmailCursor {
                uidvalidity: 1,
                last_uid: 2
            })
        );

        // BOTH messages are in the virtual tier.
        assert!(
            !crate::mbox_index::is_stale(&mbox),
            "append_index refreshed the fingerprint"
        );
        let hits = crate::mbox_index::search(&mbox, "live", 10).unwrap();
        assert_eq!(hits.len(), 2);
        let raw = crate::mbox_index::fetch(&mbox, "live2@x", 100_000).unwrap();
        assert!(raw.contains("live body 2"));

        // Round 2: resume from the cursor — only uid 3 arrives, and the
        // incremental index keeps rounds 1's messages.
        feed.messages.push(message(3, "live3@x", &["Starred"]));
        let cursor = read_cursor(&mbox);
        let (drafts, outcome) = run(&mut feed, cursor);
        assert_eq!(outcome.fetched, 1);
        assert_eq!(drafts.len(), 1);
        assert_eq!(read_cursor(&mbox).unwrap().last_uid, 3);
        assert_eq!(
            crate::mbox_index::search(&mbox, "live", 10).unwrap().len(),
            3
        );

        // A full rebuild (e.g. after an external touch) preserves the
        // cursor and still indexes every appended record.
        let n = crate::mbox_index::build(&mbox).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            read_cursor(&mbox).unwrap().last_uid,
            3,
            "cursor survives rebuild"
        );
    }

    /// A sender's own `X-Gmail-Labels: Starred,Sent` on mail Gmail filed
    /// nowhere: it must not materialize, and vsearch must show Gmail's
    /// (empty) labels, not theirs — while the pull itself still works.
    #[test]
    fn test_forged_label_header_neither_materializes_nor_indexes() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut forged = message(1, "forged1@x", &[]);
        forged.raw = [
            b"X-Gmail-Labels: Starred,Sent\r\nX-GM-THRID: 7\r\n".as_slice(),
            forged.raw.as_slice(),
        ]
        .concat();
        let mut feed = FakeFeed {
            messages: vec![forged],
        };

        let (drafts, outcome) = pull_into(&mbox, &mut feed, None);
        assert_eq!(outcome.fetched, 1);
        assert!(
            drafts.is_empty(),
            "a forged Starred/Sent must not make Core mail"
        );
        assert_eq!(
            read_cursor(&mbox).unwrap().last_uid,
            1,
            "the cursor still advances"
        );

        // Still in the virtual tier — under Gmail's labels only.
        let hits = crate::mbox_index::search(&mbox, "live", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "forged1@x");
        assert!(
            !hits[0].1.contains("Starred") && !hits[0].1.contains("Sent"),
            "got: {}",
            hits[0].1
        );
        let stored = crate::mbox_index::fetch(&mbox, "forged1@x", 100_000).unwrap();
        assert!(
            stored.starts_with("X-Gmail-Labels:\nFrom: Ann"),
            "got: {stored}"
        );
        assert!(
            !stored.contains("Starred") && !stored.contains("X-GM-THRID"),
            "got: {stored}"
        );
    }

    /// Two mailboxes pull into two archives with two cursors: one's
    /// progress never moves the other's, and a message both received is
    /// in both archives.
    #[test]
    fn test_each_mailbox_keeps_its_own_archive_and_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, work) = (
            tmp.path().join("gmail-live.mbox"),
            tmp.path().join("gmail-live-1a2b3c4d.mbox"),
        );
        for p in [&home, &work] {
            std::fs::File::create(p).unwrap();
            crate::mbox_index::build(p).unwrap();
        }
        let mut home_feed = FakeFeed {
            messages: vec![
                message(1, "a@x", &["Sent"]),
                message(2, "both@x", &["Starred"]),
            ],
        };
        let mut work_feed = FakeFeed {
            messages: vec![message(7, "both@x", &[]), message(9, "w@x", &[])],
        };
        pull_into(&home, &mut home_feed, None);
        pull_into(&work, &mut work_feed, None);
        assert_eq!(read_cursor(&home).unwrap().last_uid, 2);
        assert_eq!(read_cursor(&work).unwrap().last_uid, 9);
        // more mail in one mailbox moves only its cursor
        home_feed.messages.push(message(3, "c@x", &[]));
        let (_, outcome) = pull_into(&home, &mut home_feed, read_cursor(&home));
        assert_eq!(outcome.fetched, 1);
        assert_eq!(read_cursor(&home).unwrap().last_uid, 3);
        assert_eq!(read_cursor(&work).unwrap().last_uid, 9);
        for p in [&home, &work] {
            assert!(crate::mbox_index::fetch(p, "both@x", 100_000).is_ok());
        }
        assert!(crate::mbox_index::fetch(&work, "a@x", 100_000).is_err());
    }
}

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

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use hearthloom_adapters::gmail_imap::{self, GmailCursor, ImapFeed, PullOptions};
use hearthloom_adapters::mbox::LabelFilter;
use hearthloom_core::store::Actor;
use hearthloom_core::IngestBatch;

use crate::AppState;

pub(crate) const SOURCE_NAME: &str = "gmail-live";
const CURSOR_KEY: &str = "gmail_cursor";
/// Episodes per store-lock hold while materializing (the
/// refresh_local_sources pattern — THE PASS CONTRACT).
const APPLY_CHUNK: usize = 200;

pub(crate) fn live_mbox_path(state: &AppState) -> PathBuf {
    state.home.root().join("archives").join("gmail-live.mbox")
}

/// Feature-detect: the pass is a silent no-op until both are set (in
/// `~/.hearthloom/.env`, sealed or plain — the launchd daemon has no
/// shell env).
fn credentials() -> Option<(String, String)> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((get("GMAIL_IMAP_USER")?, get("GMAIL_IMAP_PASSWORD")?))
}

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
    let raw = crate::mbox_index::meta_get(mbox, CURSOR_KEY).ok().flatten()?;
    let (v, u) = raw.split_once(':')?;
    Some(GmailCursor {
        uidvalidity: v.parse().ok()?,
        last_uid: u.parse().ok()?,
    })
}

/// One incremental pull. Blocking (network + file + store IO) — the
/// caller runs it whole in `spawn_blocking` (THE PASS CONTRACT).
/// Returns (messages_fetched, episodes_new, more_pending).
pub(crate) fn pull_once(state: &AppState) -> Result<(usize, usize, bool)> {
    let Some((user, password)) = credentials() else {
        tracing::debug!("gmail pull: GMAIL_IMAP_USER/GMAIL_IMAP_PASSWORD not set — skipping");
        return Ok((0, 0, false));
    };

    let mbox_path = live_mbox_path(state);
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

    let mut feed = ImapFeed::connect(&user, &password)?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&mbox_path)?;
    let appended_from = file.metadata()?.len();

    let filter = LabelFilter::core_strata();
    let mut drafts = Vec::new();
    let outcome = gmail_imap::pull_new(&mut feed, cursor, &pull_options(), |msgs| {
        for m in &msgs {
            file.write_all(&gmail_imap::mbox_record(m))?;
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

    // The cursor advances LAST: everything before this line is
    // idempotent under replay.
    crate::mbox_index::meta_set(
        &mbox_path,
        CURSOR_KEY,
        &format!("{}:{}", outcome.uidvalidity, outcome.last_uid),
    )?;

    if created {
        state.store.register_virtual_source(
            &Actor::new("agent:gmail-pull"),
            SOURCE_NAME,
            "mbox",
            &serde_json::json!({
                "mount": mbox_path.to_string_lossy(),
                "trust": "imported",
            }),
            &["search".into(), "fetch".into(), "promote".into()],
        )?;
        tracing::info!("gmail pull: registered virtual source {SOURCE_NAME}");
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
        assert_eq!(read_cursor(&mbox), Some(GmailCursor { uidvalidity: 1, last_uid: 2 }));

        // BOTH messages are in the virtual tier.
        assert!(!crate::mbox_index::is_stale(&mbox), "append_index refreshed the fingerprint");
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
        assert_eq!(crate::mbox_index::search(&mbox, "live", 10).unwrap().len(), 3);

        // A full rebuild (e.g. after an external touch) preserves the
        // cursor and still indexes every appended record.
        let n = crate::mbox_index::build(&mbox).unwrap();
        assert_eq!(n, 3);
        assert_eq!(read_cursor(&mbox).unwrap().last_uid, 3, "cursor survives rebuild");
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
        forged.raw = [b"X-Gmail-Labels: Starred,Sent\r\nX-GM-THRID: 7\r\n".as_slice(), forged.raw.as_slice()].concat();
        let mut feed = FakeFeed { messages: vec![forged] };

        let (drafts, outcome) = pull_into(&mbox, &mut feed, None);
        assert_eq!(outcome.fetched, 1);
        assert!(drafts.is_empty(), "a forged Starred/Sent must not make Core mail");
        assert_eq!(read_cursor(&mbox).unwrap().last_uid, 1, "the cursor still advances");

        // Still in the virtual tier — under Gmail's labels only.
        let hits = crate::mbox_index::search(&mbox, "live", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "forged1@x");
        assert!(!hits[0].1.contains("Starred") && !hits[0].1.contains("Sent"), "got: {}", hits[0].1);
        let stored = crate::mbox_index::fetch(&mbox, "forged1@x", 100_000).unwrap();
        assert!(stored.starts_with("X-Gmail-Labels:\nFrom: Ann"), "got: {stored}");
        assert!(!stored.contains("Starred") && !stored.contains("X-GM-THRID"), "got: {stored}");
    }

    #[test]
    fn test_credentials_feature_detect() {
        // (env-var poking is process-global; these keys are only read
        // here and in pull_once, and tests run with them unset.)
        assert!(credentials().is_none() || std::env::var("GMAIL_IMAP_USER").is_ok());
    }
}

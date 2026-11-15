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
//! cursor: one puller per mailbox, and each mailbox's pulls serialized
//! (`pull_lock`) — mailboxes never wait on each other. The same message in
//! two mailboxes is one memory (same Message-ID). A refused sign-in stops
//! that mailbox's pulls until it's saved again.
//!
//! Labels change after a message is pulled (starred later, moved to
//! Personal). A pull also re-reads the labels of the mail it already holds
//! (`relabel`, at most every `RELABEL_EVERY_SECS`): the newest mail every
//! time, and the next stretch of the archive in turn, so every pulled
//! message is looked at again. A message that now belongs to the Core
//! strata becomes a memory; one that gained a label is seen again by mail
//! triage and the embers' watches, as if it had arrived so; and search
//! shows the labels Gmail has now. A memory already made keeps the labels
//! it was made under (episodes are append-only).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, TryLockError};

use anyhow::{Context, Result};
use hearthloom_adapters::gmail_imap::{
    self, is_auth_failure, GmailCursor, ImapFeed, MailFeed, PullOptions, PulledMessage,
};
use hearthloom_adapters::mbox::LabelFilter;
use hearthloom_core::store::Actor;
use hearthloom_core::{EpisodeDraft, IngestBatch};

use crate::mailboxes::{self, Mailbox};
use crate::AppState;

const CURSOR_KEY: &str = "gmail_cursor";
/// When the labels of pulled mail were last re-read (unix seconds).
const RELABEL_KEY: &str = "gmail_relabel_at";
/// The archive row the next re-read's stretch starts after.
const SWEEP_KEY: &str = "gmail_relabel_row";
/// The messages triage and the embers' watches still have to see (a JSON
/// list of archive index ids): the ones whose new labels a re-read
/// recorded, and any whose thread or match didn't record. Written with the
/// labels, emptied as each is observed — so a pull that stops in between,
/// or an observation that fails, is finished by the next re-read.
const PENDING_KEY: &str = "gmail_observe_pending";
/// How often a mailbox's pulled mail has its labels re-read.
const RELABEL_EVERY_SECS: i64 = 10 * 60;

/// How much pulled mail one re-read of labels looks at.
#[derive(Debug, Clone, Copy)]
struct RelabelLimits {
    /// The newest mail: received in the last `days` days, at most `recent`
    /// messages of it.
    days: u32,
    recent: usize,
    /// Then the next `sweep` messages of the archive, in archive order,
    /// starting again from its beginning after its end.
    sweep: usize,
}

const RELABEL: RelabelLimits = RelabelLimits {
    days: gmail_imap::RELABEL_DAYS,
    recent: gmail_imap::RELABEL_CAP,
    sweep: 500,
};
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

/// One pull at a time per mailbox: the consolidation pass and the
/// mailbox's live watch (gmail_watch.rs) both call into it, and two at once
/// on one mailbox would race on its cursor and append the same messages
/// twice. Each mailbox has its own lock, so a mailbox whose server stalls
/// holds up only itself — and only until the connection's deadline
/// (`gmail_imap::IO_TIMEOUT`).
static PULLS: Mutex<BTreeMap<String, Arc<Mutex<()>>>> = Mutex::new(BTreeMap::new());

fn pull_lock(id: &str) -> Arc<Mutex<()>> {
    PULLS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(id.to_string())
        .or_default()
        .clone()
}

/// One incremental pull of one mailbox, waiting for one already running.
/// Blocking (network + file + store IO) — the caller runs it whole in
/// `spawn_blocking` (THE PASS CONTRACT). Returns (messages_fetched,
/// episodes_new, more_pending). A mailbox Google refused isn't tried; a
/// refusal now stops it.
pub(crate) fn pull_once(state: &AppState, mb: &Mailbox) -> Result<(usize, usize, bool)> {
    mailboxes::check_refused(mb)?;
    let lock = pull_lock(&mb.id);
    let _one_at_a_time = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pull_held(state, mb)
}

/// A pull, with this mailbox's lock held.
fn pull_held(state: &AppState, mb: &Mailbox) -> Result<(usize, usize, bool)> {
    pull_once_unserialized(state, mb).inspect_err(|err| {
        if is_auth_failure(err) {
            mailboxes::refuse(Some(state), mb, err);
        }
    })
}

/// Every mailbox here, one after the other (the six-hour pass and "pull
/// now"); one Google refused is skipped, and so is one already being
/// pulled (its watch is doing this work — the pass never waits on it). The
/// totals, or the first error when every mailbox failed.
pub(crate) fn pull_all(state: &AppState) -> Result<(usize, usize, bool)> {
    let (mut fetched, mut new, mut more) = (0, 0, false);
    let (mut ok, mut first_err) = (false, None);
    for mb in mailboxes::all() {
        if mailboxes::refused(&mb.id) {
            continue;
        }
        let lock = pull_lock(&mb.id);
        let _held = match lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                ok = true;
                continue;
            }
        };
        match pull_held(state, &mb) {
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
    // small) full index before incremental appends can extend it; so does
    // one that keys messages by an older rule than the label re-read looks
    // them up by (`ids_current`) — once.
    if crate::mbox_index::is_stale(&mbox_path) || !crate::mbox_index::ids_current(&mbox_path) {
        crate::mbox_index::build(&mbox_path)?;
    }
    let cursor = read_cursor(&mbox_path);

    let mut feed = ImapFeed::connect(user, password)?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&mbox_path)?;
    let appended_from = file.metadata()?.len();

    let filter = LabelFilter::core_strata();
    let mut drafts = Vec::new();
    // what each message means for its thread (mail_triage.rs, 0.5.92);
    // the record's mail privacy read once for the pull (`observe` reads it
    // again after the memories are made)
    let mut seen = Vec::new();
    let privacy = crate::virtual_source::MailPrivacy::new(state);
    let outcome = gmail_imap::pull_new(&mut feed, cursor, &pull_options(), |msgs| {
        for m in &msgs {
            file.write_all(&gmail_imap::mbox_record(m))?;
            if let Some(s) =
                crate::mail_triage::seen_with(state, &privacy, user, &mine, &m.labels, &m.raw)
            {
                seen.push(s);
            }
            match hearthloom_adapters::mbox::draft_from_bytes_filtered(
                &gmail_imap::message_bytes(m),
                &filter,
                None, // scope stays inferred (identity rules > personal)
            ) {
                Ok(Some(draft)) => drafts.push(draft),
                Ok(None) => {}
                Err(err) => tracing::warn!("gmail pull: skipping uid {}: {err}", m.uid),
            }
        }
        Ok(())
    })?; // a failed session is dropped, not logged out: it may be the one that stalled
    file.flush()?;
    drop(file);

    if outcome.fetched > 0 {
        crate::mbox_index::append_index(&mbox_path, appended_from)?;
    }

    let mut episodes_new = apply_drafts(state, &drafts)?;

    // threads after the memories (a replay observes nothing twice); one
    // not wholly recorded is finished by the next label re-read
    let failed = crate::mail_triage::observe(state, &seen);
    let unfinished: Vec<String> = failed
        .iter()
        .filter_map(|i| seen[*i].memory_refs.first().cloned())
        .collect();
    if !unfinished.is_empty() {
        let mut pending = pending_observations(&mbox_path)?;
        merge_pending(&mut pending, unfinished);
        crate::mbox_index::meta_set(&mbox_path, PENDING_KEY, &serde_json::to_string(&pending)?)?;
    }

    // The cursor advances LAST: everything before this line is
    // idempotent under replay.
    crate::mbox_index::meta_set(
        &mbox_path,
        CURSOR_KEY,
        &format!("{}:{}", outcome.uidvalidity, outcome.last_uid),
    )?;

    // Labels of the mail pulled before this run, once the backlog is in.
    // A failure here doesn't fail the pull: the new mail and the cursor are
    // saved, and the next re-read tries again.
    if cursor.is_some() && !outcome.more && relabel_due(&mbox_path) {
        let pulled = GmailCursor {
            uidvalidity: outcome.uidvalidity,
            last_uid: outcome.last_uid,
        };
        let relabeled = relabel(state, user, &mine, &mut feed, &mbox_path, pulled, RELABEL);
        let _ = crate::mbox_index::meta_set(
            &mbox_path,
            RELABEL_KEY,
            &chrono::Utc::now().timestamp().to_string(),
        );
        match relabeled {
            Ok((changed, new)) => {
                if changed > 0 {
                    tracing::info!(
                        "gmail pull: {} — labels changed on {changed} message(s), {new} new episode(s)",
                        mb.address
                    );
                }
                episodes_new += new;
            }
            Err(err) => {
                tracing::warn!("gmail pull: {}: label re-read: {err:#}", mb.address);
                drop(feed); // not logged out: the session may be the one that stalled
                return finish(state, mb, &mbox_path, created, outcome, episodes_new);
            }
        }
    }
    feed.logout();
    finish(state, mb, &mbox_path, created, outcome, episodes_new)
}

/// The last step of a pull: register a new live archive as a source.
fn finish(
    state: &AppState,
    mb: &Mailbox,
    mbox_path: &Path,
    created: bool,
    outcome: gmail_imap::PullOutcome,
    episodes_new: usize,
) -> Result<(usize, usize, bool)> {
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

/// Materialize drafts in small store-lock holds; the number new.
fn apply_drafts(state: &AppState, drafts: &[EpisodeDraft]) -> Result<usize> {
    let mut new = 0usize;
    for chunk in drafts.chunks(APPLY_CHUNK) {
        let report = state.store.apply_batch(
            &state.home,
            &IngestBatch {
                actor: "adapter:gmail-imap".into(),
                session_id: None,
                episodes: chunk.to_vec(),
            },
        )?;
        new += report.episodes_new;
        // Let parked HTTP handlers through between holds.
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
    Ok(new)
}

fn relabel_due(mbox: &Path) -> bool {
    crate::mbox_index::meta_get(mbox, RELABEL_KEY)
        .ok()
        .flatten()
        .and_then(|at| at.parse::<i64>().ok())
        .is_none_or(|at| chrono::Utc::now().timestamp() - at >= RELABEL_EVERY_SECS)
}

/// What a re-read of labels found.
#[derive(Debug, Default)]
struct Relabel {
    /// (message id, Gmail's labels now) for each message whose labels
    /// differ from what the archive's index shows.
    changed: Vec<(String, Vec<String>)>,
    /// Memories for the messages that newly belong to the Core strata.
    drafts: Vec<EpisodeDraft>,
    /// Where the next re-read's stretch of the archive starts (an index
    /// row; 0: from the beginning).
    sweep_next: i64,
}

/// One re-read of the labels of mail already pulled, and what follows
/// from it: memories for mail that now belongs to the Core strata, the
/// labels recorded in the archive's index, then mail that gained a label
/// seen again by triage and the embers' watches (as the pull does on
/// arrival — observing a message twice changes nothing). Returns (messages
/// whose labels changed, memories made).
fn relabel(
    state: &AppState,
    account: &str,
    mine: &BTreeSet<String>,
    feed: &mut dyn MailFeed,
    mbox: &Path,
    pulled: GmailCursor,
    limits: RelabelLimits,
) -> Result<(usize, usize)> {
    let mut gained = Vec::new();
    let mut gained_keys = Vec::new();
    let plan = relabel_plan(feed, mbox, pulled, limits, |id, _, raw| {
        gained.push(id.to_string());
        gained_keys.extend(crate::mail_triage::message_keys(raw));
    })?;
    // a memory that didn't land leaves the change unrecorded: the next
    // re-read finds it again
    let new = apply_drafts(state, &plan.drafts)?;
    // A memory that arrived private makes its message's thread private at
    // once, from the record alone: the index reads below can fail, and the
    // thread (sorted already, say — triage won't look at it again) mustn't
    // stay in view until a later re-read gets through.
    crate::mail_triage::close_private_threads(state, &gained_keys)?;
    // Recorded before the threads: triage, woken by them, reads a thread's
    // labels from the index (`mailboxes::labels_now`). The messages that
    // gained a label are recorded as still to be observed in the same
    // transaction, so a recorded label is never one nobody looks at again.
    let mut pending = pending_observations(mbox)?;
    merge_pending(&mut pending, gained);
    record_relabel(mbox, &plan, &pending)?;
    #[cfg(test)]
    if STOP_AFTER_RECORD.with(|stop| stop.get()) {
        anyhow::bail!("stopped after the labels were recorded");
    }
    // threads after the memories, as on arrival — read after them, so a
    // memory that arrived private keeps its message from the watches
    observe_pending(state, account, mine, mbox, pending)?;
    Ok((plan.changed.len(), new))
}

#[cfg(test)]
thread_local! {
    /// A test's stop between recording a re-read's labels and observing
    /// the mail that gained one (as a pull ended there would).
    static STOP_AFTER_RECORD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A test's busy index: the list of messages still to observe can't be
    /// read.
    static PENDING_UNREADABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The messages still to be observed (`PENDING_KEY`). An index that can't
/// be read is an error, never an empty list: the list written back after
/// would drop the messages it holds. A value that isn't a list (none this
/// code writes) can't be read on a later try either: a new list starts.
fn pending_observations(mbox: &Path) -> Result<Vec<String>> {
    #[cfg(test)]
    if PENDING_UNREADABLE.with(|busy| busy.get()) {
        anyhow::bail!("database is locked");
    }
    let Some(list) = crate::mbox_index::meta_get(mbox, PENDING_KEY)? else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_str(&list).unwrap_or_else(|err| {
        tracing::warn!(
            "gmail pull: the messages still to observe aren't a list ({err}): starting a new one"
        );
        Vec::new()
    }))
}

fn merge_pending(pending: &mut Vec<String>, more: Vec<String>) {
    for id in more {
        if !pending.contains(&id) {
            pending.push(id);
        }
    }
}

/// Observe each pending message as the archive's index shows it now — its
/// archived copy under the labels last recorded for it — and keep pending
/// only those that couldn't be read or recorded. A message gone from the
/// index, or one triage doesn't read (`mail_triage::seen` finds no
/// Message-ID), is done. While mail is held nothing could be recorded: the
/// list is left as it is.
fn observe_pending(
    state: &AppState,
    account: &str,
    mine: &BTreeSet<String>,
    mbox: &Path,
    pending: Vec<String>,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let privacy = crate::virtual_source::MailPrivacy::new(state);
    if privacy.hold()?.is_some() {
        return Ok(());
    }
    let mut left = Vec::new();
    let mut seen = Vec::new();
    for id in pending {
        let labels = match crate::mbox_index::labels_of(mbox, &id) {
            Ok(Some(labels)) => labels,
            Ok(None) => continue,
            Err(err) => {
                tracing::warn!("gmail pull: {id}: labels unreadable: {err:#}");
                left.push(id);
                continue;
            }
        };
        let archived = match crate::mbox_index::fetch_bytes(mbox, &id) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!("gmail pull: {id}: archived copy unreadable: {err:#}");
                left.push(id);
                continue;
            }
        };
        let labels: Vec<String> = labels
            .split(',')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        // the archived copy under its labels now, as the re-read hands it on
        let raw = gmail_imap::message_bytes(&PulledMessage {
            uid: 0,
            labels: labels.clone(),
            raw: archived,
        });
        if let Some(s) =
            crate::mail_triage::seen_with(state, &privacy, account, mine, &labels, &raw)
        {
            seen.push((id, s));
        }
    }
    // one batch: the record's mail privacy is read once for all of them
    let batch: Vec<_> = seen.iter().map(|(_, s)| s.clone()).collect();
    for i in crate::mail_triage::observe(state, &batch) {
        left.push(seen[i].0.clone());
    }
    crate::mbox_index::meta_set(mbox, PENDING_KEY, &serde_json::to_string(&left)?)
}

/// Re-read Gmail's labels (`gmail_imap::current_labels`) for the newest
/// mail pulled and for the archive's next stretch (`limits.sweep` records,
/// from where the last re-read stopped — by the UID each record carries,
/// or by its Message-ID where that UID no longer holds it), and compare
/// them with the archive's index. A message whose labels now pass the Core strata, and
/// didn't, is drafted from its archived copy under those labels — the same
/// memory the pull would have made had they been there at first (keyed by
/// its Message-ID, so one that exists already is left as it is). Each
/// message that gained a label is handed to `gained` with its index id,
/// its labels now and its archived bytes under them. A message without a
/// Message-ID is left out: nothing finds its archived copy.
fn relabel_plan(
    feed: &mut dyn MailFeed,
    mbox: &Path,
    pulled: GmailCursor,
    limits: RelabelLimits,
    mut gained: impl FnMut(&str, &[String], &[u8]),
) -> Result<Relabel> {
    let core = LabelFilter::core_strata();
    let mut plan = Relabel::default();
    let after = crate::mbox_index::meta_get(mbox, SWEEP_KEY)
        .ok()
        .flatten()
        .and_then(|row| row.parse::<i64>().ok())
        .unwrap_or(0);
    let stretch = crate::mbox_index::separators_after(mbox, after, limits.sweep)?;
    plan.sweep_next = match stretch.last() {
        Some(last) if stretch.len() >= limits.sweep => last.row,
        _ => 0, // the archive's end: the next stretch starts over
    };
    // each record's UID with the Message-ID it was archived under, so a UID
    // Gmail has since given another message (a new UIDVALIDITY) is caught
    // and the message found by its Message-ID; a later copy (`at:`) is
    // read where its first copy is
    let older: Vec<gmail_imap::Archived> = stretch
        .iter()
        .filter(|s| !s.id.starts_with("at:"))
        .map(|s| gmail_imap::Archived {
            uid: gmail_imap::record_uid(&s.line),
            message_id: s.id.clone(),
        })
        .collect();
    let mut seen_ids = BTreeSet::new();
    for now in gmail_imap::current_labels(feed, pulled, limits.days, limits.recent, &older)? {
        let Some(id) = now.message_id else {
            continue;
        };
        if !seen_ids.insert(id.clone()) {
            continue;
        }
        let Some(before) = crate::mbox_index::labels_of(mbox, &id)? else {
            continue;
        };
        let before: Vec<String> = before
            .split(',')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if gmail_imap::same_labels(&before, &now.labels) {
            continue;
        }
        let newly_core = !core.keeps(&before) && core.keeps(&now.labels);
        let added = now
            .labels
            .iter()
            .any(|l| !before.iter().any(|b| b.eq_ignore_ascii_case(l.trim())));
        if newly_core || added {
            let archived = match crate::mbox_index::fetch_bytes(mbox, &id) {
                Ok(raw) => raw,
                Err(err) => {
                    // left unrecorded: the next re-read tries it again
                    tracing::warn!("gmail pull: {id}: archived copy unreadable: {err:#}");
                    continue;
                }
            };
            // the archived copy under the labels Gmail has now (the label
            // header it was archived with is replaced)
            let raw = gmail_imap::message_bytes(&PulledMessage {
                uid: now.uid,
                labels: now.labels.clone(),
                raw: archived,
            });
            if newly_core {
                match hearthloom_adapters::mbox::draft_from_bytes_filtered(&raw, &core, None) {
                    Ok(Some(draft)) => plan.drafts.push(draft),
                    Ok(None) => {}
                    Err(err) => tracing::warn!("gmail pull: skipping {id}: {err}"),
                }
            }
            if added {
                gained(&id, &now.labels, &raw);
            }
        }
        plan.changed.push((id, now.labels));
    }
    Ok(plan)
}

/// Show the labels a re-read found in the archive's search, move its
/// stretch on, and record which messages are still to be observed — one
/// transaction. Called after its memories are in: a message whose new
/// labels are recorded is never looked at again for them, so the messages
/// that gained one are recorded as `pending` with them.
fn record_relabel(mbox: &Path, plan: &Relabel, pending: &[String]) -> Result<()> {
    let updates: Vec<(String, String)> = plan
        .changed
        .iter()
        .map(|(id, labels)| (id.clone(), labels.join(", ")))
        .collect();
    let sweep = plan.sweep_next.to_string();
    let pending = serde_json::to_string(pending)?;
    crate::mbox_index::set_labels(
        mbox,
        &updates,
        &[(SWEEP_KEY, &sweep), (PENDING_KEY, &pending)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_adapters::gmail_imap::{CurrentLabels, Selected};
    use std::collections::HashSet;

    #[derive(Default)]
    struct FakeFeed {
        messages: Vec<PulledMessage>,
        /// What `uids_since_days` finds (every message when None).
        window: Option<Vec<u32>>,
        /// The mailbox's UIDVALIDITY (1 when 0).
        generation: u32,
    }

    impl MailFeed for FakeFeed {
        fn select_all_mail(&mut self) -> Result<Selected> {
            Ok(Selected {
                uidvalidity: self.generation.max(1),
                uid_next: Some(self.messages.iter().map(|m| m.uid).max().unwrap_or(0) + 1),
            })
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
            Ok(match &self.window {
                Some(window) => window.clone(),
                None => self.messages.iter().map(|m| m.uid).collect(),
            })
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
        fn labels_of(&mut self, uids: &[u32]) -> Result<Vec<CurrentLabels>> {
            Ok(self
                .messages
                .iter()
                .filter(|m| uids.contains(&m.uid))
                .map(|m| CurrentLabels {
                    uid: m.uid,
                    labels: m.labels.clone(),
                    message_id: hearthloom_adapters::mbox::message_id_ref(
                        &String::from_utf8_lossy(&m.raw),
                    ),
                })
                .collect())
        }
        fn uids_of(&mut self, message_ids: &[String]) -> Result<Vec<u32>> {
            Ok(self
                .messages
                .iter()
                .filter(|m| {
                    hearthloom_adapters::mbox::message_id_ref(&String::from_utf8_lossy(&m.raw))
                        .is_some_and(|id| message_ids.contains(&id))
                })
                .map(|m| m.uid)
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
                if let Ok(Some(d)) = hearthloom_adapters::mbox::draft_from_bytes_filtered(
                    &gmail_imap::message_bytes(m),
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
            ..Default::default()
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
            ..Default::default()
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

    /// A pull holds only its own mailbox's lock: another mailbox's pull
    /// goes ahead, a second pull of the same mailbox waits (or, from the
    /// pass, is skipped).
    #[test]
    fn test_each_mailbox_pulls_under_its_own_lock() {
        let a = pull_lock("lock-test-a");
        let _held = a.lock().unwrap();
        assert!(pull_lock("lock-test-b").try_lock().is_ok());
        assert!(matches!(
            pull_lock("lock-test-a").try_lock(),
            Err(TryLockError::WouldBlock)
        ));
    }

    /// A re-read, as `relabel` runs it, minus the store: the plan, and the
    /// labels of each message handed on for having gained one.
    fn plan_for(
        feed: &mut FakeFeed,
        mbox: &Path,
        pulled: GmailCursor,
        limits: RelabelLimits,
    ) -> (Relabel, Vec<Vec<String>>) {
        let mut gained = Vec::new();
        let plan = relabel_plan(feed, mbox, pulled, limits, |_, labels, raw| {
            assert!(
                String::from_utf8_lossy(raw)
                    .starts_with(&format!("X-Gmail-Labels: {}\n", labels.join(", "))),
                "handed on under the labels it has now"
            );
            gained.push(labels.to_vec());
        })
        .unwrap();
        (plan, gained)
    }

    /// A message pulled as Promotions and starred afterwards becomes Core
    /// mail on the next label re-read, and search shows Gmail's labels as
    /// they are now; a later change that leaves the Core strata makes no
    /// memory.
    #[test]
    fn test_mail_starred_after_its_pull_becomes_core_mail() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                message(1, "promo1@x", &["Category Promotions"]),
                message(2, "sent2@x", &["Sent"]),
            ],
            ..Default::default()
        };
        let (drafts, _) = pull_into(&mbox, &mut feed, None);
        assert_eq!(drafts.len(), 1, "only the Sent message at first");
        let pulled = read_cursor(&mbox).unwrap();
        let relabel_plan = |feed: &mut FakeFeed, mbox: &Path, pulled| {
            let (plan, gained) = plan_for(feed, mbox, pulled, RELABEL);
            (plan, gained.len())
        };

        // nothing changed: nothing to do
        let (plan, gained) = relabel_plan(&mut feed, &mbox, pulled);
        assert!(plan.changed.is_empty() && plan.drafts.is_empty() && gained == 0);

        feed.messages[0].labels.push("Starred".into());
        let (plan, gained) = relabel_plan(&mut feed, &mbox, pulled);
        assert_eq!(gained, 1);
        assert_eq!(plan.drafts.len(), 1);
        assert_eq!(plan.drafts[0].source, "mbox");
        assert_eq!(plan.drafts[0].source_ref, "promo1@x");
        assert!(
            plan.drafts[0]
                .body
                .contains("labels: Category Promotions, Starred\n"),
            "{}",
            plan.drafts[0].body
        );
        assert_eq!(
            plan.changed,
            vec![(
                "promo1@x".to_string(),
                vec!["Category Promotions".to_string(), "Starred".to_string()]
            )]
        );
        record_relabel(&mbox, &plan, &[]).unwrap();
        let hits = crate::mbox_index::search(&mbox, "Starred", 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.0.as_str()).collect::<Vec<_>>(),
            vec!["promo1@x"]
        );

        // recorded: the next re-read finds nothing new
        let (plan, gained) = relabel_plan(&mut feed, &mbox, pulled);
        assert!(plan.changed.is_empty() && plan.drafts.is_empty() && gained == 0);

        // unstarred again: the labels follow, no memory is made, and losing
        // a label is nothing for triage or the watches to see
        feed.messages[0].labels.pop();
        let (plan, gained) = relabel_plan(&mut feed, &mbox, pulled);
        assert_eq!(plan.changed.len(), 1);
        assert!(plan.drafts.is_empty());
        assert_eq!(gained, 0);
        record_relabel(&mbox, &plan, &[]).unwrap();
        assert!(crate::mbox_index::search(&mbox, "Starred", 10)
            .unwrap()
            .is_empty());
    }

    /// Every pulled message has its labels read again in time, not only the
    /// newest: a message older than the recent window, and one inside it
    /// but past its cap, are both found by the stretch that moves through
    /// the archive — and what was found stays after the index is rebuilt.
    #[test]
    fn test_every_pulled_message_has_its_labels_read_again() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: (1..=7)
                .map(|u| message(u, &format!("m{u}@x"), &["Category Promotions"]))
                .collect(),
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        let pulled = read_cursor(&mbox).unwrap();
        // the recent window holds uids 4–7, and only its newest two are
        // read every time
        feed.window = Some(vec![4, 5, 6, 7]);
        let limits = RelabelLimits {
            days: 30,
            recent: 2,
            sweep: 3,
        };
        // starred later: uid 1 (older than the window), uid 4 (in it, past
        // the cap)
        for uid in [1, 4] {
            feed.messages[uid - 1].labels.push("Starred".into());
        }

        let mut found = Vec::new();
        let mut passes = 0;
        while found.len() < 2 && passes < 5 {
            passes += 1;
            let (plan, _) = plan_for(&mut feed, &mbox, pulled, limits);
            found.extend(plan.drafts.iter().map(|d| d.source_ref.clone()));
            record_relabel(&mbox, &plan, &[]).unwrap();
        }
        found.sort();
        assert_eq!(found, vec!["m1@x", "m4@x"], "within {passes} passes");
        assert!(passes <= 3, "one trip through the archive: {passes}");

        // a rebuild keeps what the re-read found: the old labels don't come
        // back, and nothing reads as changed again
        crate::mbox_index::build(&mbox).unwrap();
        let mut starred: Vec<String> = crate::mbox_index::search(&mbox, "Starred", 10)
            .unwrap()
            .into_iter()
            .map(|h| h.0)
            .collect();
        starred.sort();
        assert_eq!(starred, vec!["m1@x", "m4@x"]);
        for _ in 0..4 {
            let (plan, gained) = plan_for(&mut feed, &mbox, pulled, limits);
            assert!(plan.changed.is_empty(), "{:?}", plan.changed);
            assert!(gained.is_empty());
            record_relabel(&mbox, &plan, &[]).unwrap();
        }
    }

    /// Mail starred after its pull reaches what mail arriving starred
    /// reaches: a memory, a thread on the Desk, an ember watching for
    /// Starred mail — once, however often it is read again.
    fn state_at(dir: &Path) -> AppState {
        let home = hearthloom_core::paths::Home::at(dir);
        home.init().unwrap();
        AppState {
            store: Arc::new(hearthloom_core::store::Store::open(&home.db_path()).unwrap()),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        }
    }

    /// A message from Ann to the owner; `parent` makes it a reply.
    fn mail(
        uid: u32,
        msgid: &str,
        parent: Option<&str>,
        labels: &[&str],
        body: &str,
    ) -> PulledMessage {
        let threading = parent
            .map(|p| format!("In-Reply-To: <{p}>\r\nReferences: <{p}>\r\n"))
            .unwrap_or_default();
        PulledMessage {
            uid,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            raw: format!(
                "From: Ann <ann@x.example>\r\nTo: Jon <jon@y.example>\r\nSubject: plans\r\n\
                 Date: Mon, 05 Jan 2026 10:{:02}:00 -0400\r\nMessage-ID: <{msgid}>\r\n{threading}\r\n\
                 {body}\r\n",
                uid % 60
            )
            .into_bytes(),
        }
    }

    /// What the pull does with each message on arrival: its thread, and the
    /// embers' watches.
    fn observe_as_pulled(state: &AppState, account: &str, msgs: &[PulledMessage]) {
        let seen: Vec<_> = msgs
            .iter()
            .filter_map(|m| {
                crate::mail_triage::seen(
                    state,
                    account,
                    &BTreeSet::new(),
                    &m.labels,
                    &gmail_imap::message_bytes(m),
                )
            })
            .collect();
        crate::mail_triage::observe(state, &seen);
    }

    fn watch(state: &AppState, ember: &str, labels: &[&str], classes: &[&str]) {
        use hearthloom_core::store::mail_watch::WatchFilter;
        state
            .store
            .mail_watch_set(
                &Actor::new(format!("plugin:{ember}")),
                ember,
                "watched mail",
                &WatchFilter {
                    labels: labels.iter().map(|s| s.to_string()).collect(),
                    classes: classes.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    /// A reply in a thread a privacy rule made private never reaches an
    /// ember — on arrival, nor when it gains a label later.
    #[tokio::test]
    async fn test_a_later_label_keeps_a_private_threads_mail_from_the_watches() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        watch(&state, "stars", &["Starred"], &[]);
        state
            .store
            .add_privacy_rule(&Actor::new("human:test"), "term", "test results")
            .unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                mail(
                    1,
                    "a@x",
                    None,
                    &["Category Personal"],
                    "about the test results",
                ),
                mail(
                    2,
                    "b@x",
                    Some("a@x"),
                    &["Category Personal"],
                    "see you then",
                ),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        assert!(state.store.mail_thread_of_message("b@x").unwrap().is_some());

        feed.messages[1].labels.push("Starred".into());
        let pulled = read_cursor(&mbox).unwrap();
        let (changed, _) = relabel(
            &state,
            "jon@y.example",
            &BTreeSet::new(),
            &mut feed,
            &mbox,
            pulled,
            RELABEL,
        )
        .unwrap();
        assert_eq!(changed, 1);
        assert!(
            state
                .store
                .mail_match_for("stars", "b@x")
                .unwrap()
                .is_none(),
            "the reply's thread is private"
        );
        assert!(state
            .store
            .mail_matches_after("stars", 0, 50)
            .unwrap()
            .is_empty());
    }

    /// A watch that wants a label AND a triage class sees mail that gained
    /// the label after its pull: in a thread already sorted, at once; in a
    /// thread the label opened, once triage sorts it — under the labels
    /// Gmail has now, not the ones it was archived with.
    #[tokio::test]
    async fn test_mail_starred_later_reaches_watches_that_wait_for_a_class() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        watch(&state, "replies", &["Starred"], &["needs_reply"]);
        let mb = crate::mailboxes::from_vars([
            ("GMAIL_IMAP_USER".to_string(), "jon@y.example".to_string()),
            ("GMAIL_IMAP_PASSWORD".to_string(), "unused".to_string()),
        ])
        .remove(0);
        let mbox = mb.live_mbox_path(&state);
        std::fs::create_dir_all(mbox.parent().unwrap()).unwrap();
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                mail(1, "p@x", None, &["Category Promotions"], "a sale"),
                mail(2, "q@x", None, &["Category Personal"], "can you call me?"),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, &mb.address, &feed.messages);
        let classify = |message_id: &str| {
            let id = state
                .store
                .mail_thread_of_message(message_id)
                .unwrap()
                .expect("a thread");
            assert!(state
                .store
                .mail_classify(
                    &Actor::new("system:mail"),
                    &id,
                    "needs_reply",
                    "asks",
                    0.9,
                    None
                )
                .unwrap());
            let thread = state.store.mail_thread(&id).unwrap().unwrap();
            crate::mail_embers::on_classified_in(&state, &thread, std::slice::from_ref(&mb))
        };
        // q's thread is sorted before anything is starred: nothing yet
        assert_eq!(classify("q@x"), 0);
        assert!(state.store.mail_thread_of_message("p@x").unwrap().is_none());

        for m in &mut feed.messages {
            m.labels.push("Starred".into());
        }
        let pulled = read_cursor(&mbox).unwrap();
        let (changed, new) = relabel(
            &state,
            &mb.address,
            &BTreeSet::new(),
            &mut feed,
            &mbox,
            pulled,
            RELABEL,
        )
        .unwrap();
        assert_eq!((changed, new), (2, 1), "p is Core mail now");
        // q's thread had its class: matched as soon as it was starred
        let q = state.store.mail_match_for("replies", "q@x").unwrap();
        assert!(
            q.as_ref()
                .is_some_and(|m| m.class.as_deref() == Some("needs_reply")
                    && m.labels.iter().any(|l| l == "Starred")),
            "{q:?}"
        );
        // p opened a thread waiting for triage; sorted, it matches
        assert!(state
            .store
            .mail_match_for("replies", "p@x")
            .unwrap()
            .is_none());
        assert_eq!(classify("p@x"), 1);
        let p = state.store.mail_match_for("replies", "p@x").unwrap();
        assert!(
            p.as_ref()
                .is_some_and(|m| m.labels.iter().any(|l| l == "Starred")),
            "{p:?}"
        );
    }

    /// After Gmail renumbers the mailbox (a new UIDVALIDITY), an old
    /// message's archive record still carries its old UID; the re-read finds
    /// the message by its Message-ID and sees the label it gained.
    #[test]
    fn test_labels_are_read_again_after_gmail_renumbers_the_mailbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let promo = &["Category Promotions"];
        let mut feed = FakeFeed {
            messages: vec![
                mail(100, "old@x", None, promo, "a sale"),
                mail(101, "other@x", None, promo, "another sale"),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        // renumbered: old@x is uid 2000 now, and starred; uid 100 is
        // another message, one this archive doesn't hold
        feed.generation = 2;
        feed.messages = vec![
            mail(100, "stranger@x", None, &["Starred"], "hello"),
            mail(
                2000,
                "old@x",
                None,
                &["Category Promotions", "Starred"],
                "a sale",
            ),
            mail(2001, "other@x", None, promo, "another sale"),
        ];
        feed.window = Some(vec![]);
        let pulled = GmailCursor {
            uidvalidity: 2,
            last_uid: 2500,
        };
        let (plan, gained) = plan_for(&mut feed, &mbox, pulled, RELABEL);
        assert_eq!(
            plan.drafts
                .iter()
                .map(|d| d.source_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["old@x"]
        );
        assert_eq!(gained.len(), 1);
        assert_eq!(
            plan.changed,
            vec![(
                "old@x".to_string(),
                vec!["Category Promotions".to_string(), "Starred".to_string()]
            )]
        );
    }

    #[tokio::test]
    async fn test_mail_starred_later_reaches_the_desk_and_the_watches() {
        use hearthloom_core::store::mail_watch::WatchFilter;
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        state
            .store
            .mail_watch_set(
                &Actor::new("plugin:stars"),
                "stars",
                "starred mail",
                &WatchFilter {
                    labels: vec!["Starred".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                message(1, "promo1@x", &["Category Promotions"]),
                message(2, "sent2@x", &["Sent"]),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        let pulled = read_cursor(&mbox).unwrap();
        let mine = BTreeSet::new();
        let run = |feed: &mut FakeFeed| {
            relabel(&state, "jon@y.example", &mine, feed, &mbox, pulled, RELABEL).unwrap()
        };

        assert_eq!(run(&mut feed), (0, 0));
        assert!(!state.store.episode_exists("mbox", "promo1@x").unwrap());

        feed.messages[0].labels.push("Starred".into());
        assert_eq!(run(&mut feed), (1, 1));
        assert!(state.store.episode_exists("mbox", "promo1@x").unwrap());
        let threads = state.store.mail_threads(&[], 10).unwrap();
        assert_eq!(threads.len(), 1, "{threads:?}");
        assert_eq!(threads[0].last_message_id.as_deref(), Some("promo1@x"));
        assert_eq!(threads[0].state, "new");
        let matched = state.store.mail_match_for("stars", "promo1@x").unwrap();
        assert!(
            matched.is_some_and(|m| m.labels.iter().any(|l| l == "Starred")),
            "the ember's watch saw it"
        );

        // recorded: nothing new the next time
        assert_eq!(run(&mut feed), (0, 0));
        assert_eq!(state.store.mail_threads(&[], 10).unwrap().len(), 1);
        assert_eq!(
            state
                .store
                .mail_matches_after("stars", 0, 50)
                .unwrap()
                .len(),
            1
        );
    }

    /// A message from Ann to the owner whose Message-ID header is written
    /// out whole (`header`): folded, or with a comment beside the id.
    fn mail_with_id_header(uid: u32, header: &str, labels: &[&str], body: &str) -> PulledMessage {
        PulledMessage {
            uid,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            raw: format!(
                "From: Ann <ann@x.example>\r\nTo: Jon <jon@y.example>\r\nSubject: plans\r\n\
                 Date: Mon, 05 Jan 2026 10:{:02}:00 -0400\r\n{header}\r\n\r\n{body}\r\n",
                uid % 60
            )
            .into_bytes(),
        }
    }

    /// The mail import's key for a pulled message.
    fn import_key(m: &PulledMessage) -> String {
        hearthloom_adapters::mbox::message_id_ref(&String::from_utf8_lossy(&m.raw)).unwrap()
    }

    /// A pulled message's memory, labelled private, landing without the
    /// pull (as a copy from another node would).
    fn private_memory(state: &AppState, m: &PulledMessage) {
        let draft = hearthloom_adapters::mbox::draft_from_bytes_filtered(
            &gmail_imap::message_bytes(m),
            &LabelFilter::default(),
            Some("private"),
        )
        .unwrap()
        .unwrap();
        state
            .store
            .apply_batch(
                &state.home,
                &IngestBatch {
                    actor: "adapter:mbox".into(),
                    session_id: None,
                    episodes: vec![draft],
                },
            )
            .unwrap();
    }

    /// A message whose memory is private under the key the mail import gave
    /// it — which a MIME parser's reading of the Message-ID differs from,
    /// for a header with a comment beside the id — opens no thread a model
    /// sorts and reaches no ember when it is starred later; and a match
    /// queued for it before is held back when the ember reads its queue.
    #[tokio::test]
    async fn test_a_private_memory_under_the_imports_key_holds_its_mail_back() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        watch(&state, "stars", &["Starred"], &[]);
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let promo = &["Category Promotions"];
        let mut feed = FakeFeed {
            messages: vec![
                mail_with_id_header(1, "Message-ID: (desk) <hush@x>", promo, "the lease terms"),
                mail(2, "open@x", None, promo, "a sale"),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        // the first message's memory, labelled private
        let private = hearthloom_adapters::mbox::draft_from_bytes_filtered(
            &gmail_imap::message_bytes(&feed.messages[0]),
            &LabelFilter::default(),
            Some("private"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(private.source_ref, import_key(&feed.messages[0]));
        assert_ne!(private.source_ref, "hush@x");
        state
            .store
            .apply_batch(
                &state.home,
                &IngestBatch {
                    actor: "adapter:mbox".into(),
                    session_id: None,
                    episodes: vec![private],
                },
            )
            .unwrap();

        for m in &mut feed.messages {
            m.labels.push("Starred".into());
        }
        let pulled = read_cursor(&mbox).unwrap();
        relabel(
            &state,
            "jon@y.example",
            &BTreeSet::new(),
            &mut feed,
            &mbox,
            pulled,
            RELABEL,
        )
        .unwrap();
        let to_sort: Vec<Option<String>> = state
            .store
            .mail_threads_to_triage(10)
            .unwrap()
            .into_iter()
            .map(|t| t.last_message_id)
            .collect();
        assert_eq!(to_sort, vec![Some("open@x".to_string())]);
        assert!(state
            .store
            .mail_match_for("stars", "hush@x")
            .unwrap()
            .is_none());
        assert!(state
            .store
            .mail_match_for("stars", "open@x")
            .unwrap()
            .is_some());

        // a match queued while its memory wasn't private yet
        let earlier = hearthloom_core::store::mail_watch::MailMatch {
            plugin: "stars".into(),
            watch: "mw_earlier".into(),
            watch_name: "watched mail".into(),
            message_id: "hush@x".into(),
            sender: "Ann <ann@x.example>".into(),
            subject: "plans".into(),
            text: "the lease terms".into(),
            ..Default::default()
        };
        assert!(state.store.mail_match_add(&earlier).unwrap());
        assert!(crate::mail_embers::held_back(&state, &earlier));
        let got = crate::mail_embers::deliver(&state, "stars", 0, 50).unwrap();
        let delivered: Vec<&str> = got["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["message_id"].as_str())
            .collect();
        assert_eq!(delivered, vec!["open@x"]);
    }

    /// Mail from someone whose identity the owner made private: its memory
    /// arrives private when the pull makes it, after the message was read —
    /// on arrival and when it is starred later. Its thread is private
    /// (never sorted by a model) and no ember sees it.
    #[tokio::test]
    async fn test_mail_whose_memory_arrives_private_stays_private() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        watch(&state, "stars", &["Starred"], &[]);
        state
            .store
            .claim_identity(
                &Actor::new("human:test"),
                "Ann",
                "email",
                "ann@x.example",
                Some("private"),
            )
            .unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                mail(
                    1,
                    "later@x",
                    None,
                    &["Category Promotions"],
                    "dinner friday?",
                ),
                mail(2, "now@x", None, &["Starred"], "lunch monday?"),
            ],
            ..Default::default()
        };
        // on arrival, as the pull runs it: read, then the memory, then the
        // thread
        let (drafts, _) = pull_into(&mbox, &mut feed, None);
        let seen: Vec<_> = feed
            .messages
            .iter()
            .filter_map(|m| {
                crate::mail_triage::seen(
                    &state,
                    "jon@y.example",
                    &BTreeSet::new(),
                    &m.labels,
                    &m.raw,
                )
            })
            .collect();
        assert!(seen.iter().all(|s| !s.private), "no memory yet");
        assert_eq!(apply_drafts(&state, &drafts).unwrap(), 1);
        assert!(!state
            .store
            .private_source_refs("mbox", &["now@x".to_string()])
            .unwrap()
            .is_empty());
        assert!(crate::mail_triage::observe(&state, &seen).is_empty());
        assert!(state.store.mail_threads_to_triage(10).unwrap().is_empty());
        assert!(state
            .store
            .mail_match_for("stars", "now@x")
            .unwrap()
            .is_none());

        // starred later: its memory is made, private, by the re-read
        feed.messages[0].labels.push("Starred".into());
        let pulled = read_cursor(&mbox).unwrap();
        let (changed, new) = relabel(
            &state,
            "jon@y.example",
            &BTreeSet::new(),
            &mut feed,
            &mbox,
            pulled,
            RELABEL,
        )
        .unwrap();
        assert_eq!((changed, new), (1, 1));
        assert!(!state
            .store
            .private_source_refs("mbox", &["later@x".to_string()])
            .unwrap()
            .is_empty());
        assert!(state.store.mail_threads_to_triage(10).unwrap().is_empty());
        assert!(state
            .store
            .mail_match_for("stars", "later@x")
            .unwrap()
            .is_none());
    }

    /// Mail an owner rule made a thread of before it had a memory: starred
    /// later, its memory arrives private (its sender's identity is), and the
    /// thread it already joined becomes private — triage no longer takes it.
    #[tokio::test]
    async fn test_a_thread_turns_private_when_its_mail_is_found_private_later() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let owner = Actor::new("human:test");
        state
            .store
            .mail_rule_add(&owner, "from", "ann@x.example", "triage", None)
            .unwrap();
        state
            .store
            .claim_identity(&owner, "Ann", "email", "ann@x.example", Some("private"))
            .unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![mail(
                1,
                "ruled@x",
                None,
                &["Category Promotions"],
                "dinner friday?",
            )],
            ..Default::default()
        };
        let (drafts, _) = pull_into(&mbox, &mut feed, None);
        assert!(drafts.is_empty(), "not Core mail: no memory yet");
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        let thread = state
            .store
            .mail_thread_of_message("ruled@x")
            .unwrap()
            .expect("the rule made a thread");
        let to_sort = || {
            state
                .store
                .mail_threads_to_triage_for(10, &[], Some(&[]))
                .unwrap()
        };
        assert_eq!(to_sort().len(), 1);

        feed.messages[0].labels.push("Starred".into());
        let pulled = read_cursor(&mbox).unwrap();
        let (changed, new) = relabel(
            &state,
            "jon@y.example",
            &BTreeSet::new(),
            &mut feed,
            &mbox,
            pulled,
            RELABEL,
        )
        .unwrap();
        assert_eq!((changed, new), (1, 1));
        assert!(!state
            .store
            .private_source_refs("mbox", &["ruled@x".to_string()])
            .unwrap()
            .is_empty());
        assert!(to_sort().is_empty(), "a private thread waits for no model");
        assert!(state.store.mail_thread(&thread).unwrap().is_none());
        state.store.vault_unlock(5);
        assert_eq!(
            state.store.mail_thread(&thread).unwrap().unwrap().scope,
            "private"
        );
    }

    /// A message's memory that turns private after its thread formed, by a
    /// path that doesn't see the message again (a copy arriving private
    /// from another node, the owner's label), leaves the thread as it was.
    /// Triage looks the thread's memories up again before a model reads it:
    /// the thread is made private and left out, and the others are sorted.
    #[tokio::test]
    async fn test_triage_leaves_out_a_thread_whose_mail_turned_private() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let owner = Actor::new("human:test");
        state
            .store
            .mail_rule_add(&owner, "from", "ann@x.example", "triage", None)
            .unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![mail(
                1,
                "ruled@x",
                None,
                &["Category Promotions"],
                "dinner friday?",
            )],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        let thread = state
            .store
            .mail_thread_of_message("ruled@x")
            .unwrap()
            .expect("the rule made a thread");
        // another thread, from someone else
        let open = state
            .store
            .mail_observe(
                &owner,
                &hearthloom_core::store::mail::MailSeen {
                    account: "jon@y.example".into(),
                    message_id: "open@x".into(),
                    from: "Susan <susan@z.example>".into(),
                    from_addr: "susan@z.example".into(),
                    subject: "Your portal".into(),
                    primary: true,
                    snippet: "Please upload the statements by Friday.".into(),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();

        // its memory, private, from elsewhere
        private_memory(&state, &feed.messages[0]);
        assert!(!state
            .store
            .private_source_refs("mbox", &["ruled@x".to_string()])
            .unwrap()
            .is_empty());
        let waiting = || {
            state
                .store
                .mail_threads_to_triage_for(10, &[], Some(&[]))
                .unwrap()
                .len()
        };
        assert_eq!(waiting(), 2, "the thread is as it was");

        let asked = crate::mail_triage::stand_in_claude(
            &state,
            tmp.path(),
            serde_json::json!([{
                "id": open, "class": "needs_reply", "confidence": 0.9,
                "span": "upload the statements by Friday", "why": "Susan asks."
            }]),
        );
        let sorted = crate::mail_triage::triage_for(&state, &crate::mail_triage::every_mailbox())
            .await
            .unwrap();
        assert_eq!(sorted, 1);
        let asked = std::fs::read_to_string(&asked).unwrap();
        assert!(asked.contains("Your portal"), "{asked}");
        assert!(
            !asked.contains(&thread) && !asked.contains("dinner friday"),
            "private mail never goes to the model: {asked}"
        );
        assert_eq!(waiting(), 0);
        state.store.vault_unlock(5);
        assert_eq!(
            state.store.mail_thread(&thread).unwrap().unwrap().scope,
            "private"
        );
    }

    /// A thread sorted already — triage won't look at it again — whose
    /// message's memory arrives private when a label re-read makes it (its
    /// sender's identity is private) turns private at once, from the record
    /// alone, even when the archive's index can't be read right after and
    /// the re-read fails before it records anything.
    #[tokio::test]
    async fn test_a_sorted_thread_turns_private_when_the_index_cannot_be_read_after() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let owner = Actor::new("human:test");
        state
            .store
            .mail_rule_add(&owner, "from", "ann@x.example", "triage", None)
            .unwrap();
        state
            .store
            .claim_identity(&owner, "Ann", "email", "ann@x.example", Some("private"))
            .unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![mail(
                1,
                "ruled@x",
                None,
                &["Category Promotions"],
                "dinner friday?",
            )],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        let thread = state
            .store
            .mail_thread_of_message("ruled@x")
            .unwrap()
            .expect("the rule made a thread");
        assert!(state
            .store
            .mail_classify(&owner, &thread, "fyi", "a note", 0.9, None)
            .unwrap());

        feed.messages[0].labels.push("Starred".into());
        let pulled = read_cursor(&mbox).unwrap();
        // the index busy once the memory is made: the list of messages still
        // to observe can't be read
        PENDING_UNREADABLE.with(|busy| busy.set(true));
        let failed = relabel(
            &state,
            "jon@y.example",
            &BTreeSet::new(),
            &mut feed,
            &mbox,
            pulled,
            RELABEL,
        );
        PENDING_UNREADABLE.with(|busy| busy.set(false));
        assert!(failed.is_err());
        assert!(!state
            .store
            .private_source_refs("mbox", &["ruled@x".to_string()])
            .unwrap()
            .is_empty());
        assert!(
            state.store.mail_thread(&thread).unwrap().is_none(),
            "out of view with the vault locked"
        );
        state.store.vault_unlock(5);
        let t = state.store.mail_thread(&thread).unwrap().unwrap();
        assert_eq!((t.scope.as_str(), t.state.as_str()), ("private", "fyi"));
    }

    /// A message with two Message-ID headers: the mail import keys its
    /// memory by the first, a MIME parser reads the last. A memory under the
    /// first that turns private after its thread formed is found by triage
    /// through the keys the thread index kept: the thread is made private
    /// and never reaches the model.
    #[tokio::test]
    async fn test_triage_finds_a_private_memory_under_the_imports_key() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                mail_with_id_header(
                    1,
                    "Message-ID: <first@x>\r\nMessage-ID: <second@x>",
                    &["Category Personal"],
                    "dinner friday?",
                ),
                mail(2, "open@x", None, &["Category Personal"], "lunch monday?"),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        assert_eq!(import_key(&feed.messages[0]), "first@x");
        let thread = state
            .store
            .mail_thread_of_message("second@x")
            .unwrap()
            .expect("the thread index keys it by the parser's reading");
        let open = state
            .store
            .mail_thread_of_message("open@x")
            .unwrap()
            .unwrap();

        private_memory(&state, &feed.messages[0]);
        let asked = crate::mail_triage::stand_in_claude(
            &state,
            tmp.path(),
            serde_json::json!([{
                "id": open, "class": "fyi", "confidence": 0.9,
                "span": "lunch monday", "why": "A lunch."
            }]),
        );
        let sorted = crate::mail_triage::triage_for(&state, &crate::mail_triage::every_mailbox())
            .await
            .unwrap();
        assert_eq!(sorted, 1);
        let asked = std::fs::read_to_string(&asked).unwrap();
        assert!(asked.contains("lunch monday"), "{asked}");
        assert!(
            !asked.contains(&thread) && !asked.contains("dinner friday"),
            "private mail never goes to the model: {asked}"
        );
        state.store.vault_unlock(5);
        assert_eq!(
            state.store.mail_thread(&thread).unwrap().unwrap().scope,
            "private"
        );
    }

    /// The same, for threads recorded before the thread index kept each
    /// message's memory keys (v70: `memory_refs` NULL) and not observed
    /// since. Triage reads the keys from the live archive: a message the
    /// archive holds under the id the index knows gets them back (and
    /// keeps them); the two-Message-ID message, which the archive holds
    /// under the import's key, can't be read that way — its thread can't be
    /// checked, so it waits, never reaching the model, its label as it was
    /// (a private memory of some message without a Message-ID says nothing
    /// of it). Once the pull observes the message again (the label re-read
    /// reaches every message in time), its keys are kept and its thread is
    /// made private.
    #[tokio::test]
    async fn test_triage_checks_threads_recorded_before_their_keys_were_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let archives = tmp.path().join("archives");
        std::fs::create_dir_all(&archives).unwrap();
        let mbox = archives.join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                mail_with_id_header(
                    1,
                    "Message-ID: <first@x>\r\nMessage-ID: <second@x>",
                    &["Category Personal"],
                    "dinner friday?",
                ),
                mail(2, "open@x", None, &["Category Personal"], "lunch monday?"),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        let thread = state
            .store
            .mail_thread_of_message("second@x")
            .unwrap()
            .unwrap();
        let open = state
            .store
            .mail_thread_of_message("open@x")
            .unwrap()
            .unwrap();
        // as an earlier version recorded them
        rusqlite::Connection::open(state.home.db_path())
            .unwrap()
            .execute("UPDATE mail_thread_messages SET memory_refs = NULL", [])
            .unwrap();
        assert_eq!(
            state.store.mail_thread_message_keys(&open).unwrap(),
            vec![("open@x".to_string(), None)]
        );

        private_memory(&state, &feed.messages[0]);
        // another message's memory, private, keyed by its text
        private_memory(
            &state,
            &mail_with_id_header(3, "X-Note: no id", &["Category Personal"], "a receipt"),
        );
        assert!(crate::virtual_source::MailPrivacy::new(&state)
            .key_found(&crate::virtual_source::ItemKey::Unkeyed)
            .unwrap());
        let asked = crate::mail_triage::stand_in_claude(
            &state,
            tmp.path(),
            serde_json::json!([{
                "id": open, "class": "fyi", "confidence": 0.9,
                "span": "lunch monday", "why": "A lunch."
            }]),
        );
        let sorted = crate::mail_triage::triage_for(&state, &crate::mail_triage::every_mailbox())
            .await
            .unwrap();
        assert_eq!(sorted, 1);
        let asked = std::fs::read_to_string(&asked).unwrap();
        assert!(asked.contains("lunch monday"), "{asked}");
        assert!(
            !asked.contains(&thread) && !asked.contains("dinner friday"),
            "private mail never goes to the model: {asked}"
        );
        assert_eq!(
            state.store.mail_thread_message_keys(&open).unwrap(),
            vec![("open@x".to_string(), Some(vec!["open@x".to_string()]))],
            "keys read from the archive are kept"
        );
        let waiting = |id: &str| {
            state
                .store
                .mail_threads_to_triage_for(10, &[], Some(&[]))
                .unwrap()
                .iter()
                .any(|t| t.id == id)
        };
        assert!(waiting(&thread), "it waits to be checked");
        assert_eq!(
            state.store.mail_thread(&thread).unwrap().unwrap().scope,
            "personal"
        );

        // observed again: its keys are kept, and its private memory found
        observe_as_pulled(&state, "jon@y.example", &feed.messages[..1]);
        assert!(!waiting(&thread));
        state.store.vault_unlock(5);
        assert_eq!(
            state.store.mail_thread(&thread).unwrap().unwrap().scope,
            "private"
        );
    }

    /// The same, for threads another node recorded: threads mirror, the
    /// thread index doesn't, so this node's index holds none of their
    /// messages — only the thread's latest message id is known. Its keys
    /// are read from the live archive here: the message it holds under that
    /// id is checked and sorted; the two-Message-ID message, held under the
    /// import's key, can't be checked — it waits, never sent to the model
    /// (not even the thread's synced snippet), its label left to the node
    /// that recorded it.
    #[tokio::test]
    async fn test_triage_checks_threads_another_node_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let archives = tmp.path().join("archives");
        std::fs::create_dir_all(&archives).unwrap();
        let mbox = archives.join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![
                mail_with_id_header(
                    1,
                    "Message-ID: <first@x>\r\nMessage-ID: <second@x>",
                    &["Category Personal"],
                    "dinner friday?",
                ),
                mail(2, "open@x", None, &["Category Personal"], "lunch monday?"),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        let thread = state
            .store
            .mail_thread_of_message("second@x")
            .unwrap()
            .unwrap();
        let open = state
            .store
            .mail_thread_of_message("open@x")
            .unwrap()
            .unwrap();
        // as the threads reach a node that didn't record their mail
        rusqlite::Connection::open(state.home.db_path())
            .unwrap()
            .execute("DELETE FROM mail_thread_messages", [])
            .unwrap();
        assert!(state
            .store
            .mail_thread_message_keys(&thread)
            .unwrap()
            .is_empty());
        assert_eq!(
            state
                .store
                .mail_thread(&thread)
                .unwrap()
                .unwrap()
                .last_message_id
                .as_deref(),
            Some("second@x")
        );

        private_memory(&state, &feed.messages[0]);
        let asked = crate::mail_triage::stand_in_claude(
            &state,
            tmp.path(),
            serde_json::json!([{
                "id": open, "class": "fyi", "confidence": 0.9,
                "span": "lunch monday", "why": "A lunch."
            }]),
        );
        let sorted = crate::mail_triage::triage_for(&state, &crate::mail_triage::every_mailbox())
            .await
            .unwrap();
        assert_eq!(sorted, 1);
        let asked = std::fs::read_to_string(&asked).unwrap();
        assert!(asked.contains("lunch monday"), "{asked}");
        assert!(
            !asked.contains(&thread) && !asked.contains("dinner friday"),
            "private mail never goes to the model: {asked}"
        );
        let t = state.store.mail_thread(&thread).unwrap().unwrap();
        assert_eq!((t.scope.as_str(), t.state.as_str()), ("personal", "new"));
    }

    /// A node that takes over another node's mailbox while that node is away
    /// (mailboxes::duty) holds the threads it recorded — threads mirror —
    /// but none of their mail: no thread index rows, no live archive. Those
    /// threads can't be checked here, so they wait: none goes to the model,
    /// none is labelled (a private label is for good, and would sync back),
    /// and more of them than a round's batch, older than this node's own
    /// mail, don't hold that mail up. Once a live archive here holds their
    /// mail, the next round checks them and sorts them.
    #[tokio::test]
    async fn test_threads_this_node_cannot_check_yet_wait_and_are_sorted_later() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        // node a watches the work mailbox and is away; this node watches its own
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        let away = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let duty = crate::mailboxes::duty_from(
            "node-b",
            &["jon@y.example".to_string()],
            &[serde_json::json!({
                "node_id": "node-a",
                "last_seen": away,
                "roles": {"mail": true, "mailboxes": [
                    {"address": "work@y.example", "watching": true}
                ]}
            })],
            &cutoff,
        );
        assert!(duty.handles("work@y.example") && duty.handles("jon@y.example"));

        // the work mailbox's threads, as node a recorded them: older mail
        let work = tmp.path().join("node-a.mbox");
        std::fs::File::create(&work).unwrap();
        crate::mbox_index::build(&work).unwrap();
        let count = crate::mail_triage::BATCH as u32 + 2;
        let mut work_feed = FakeFeed {
            messages: (1..=count)
                .map(|n| {
                    mail(
                        n,
                        &format!("work{n}@x"),
                        None,
                        &["Category Personal"],
                        "the quarterly figures",
                    )
                })
                .collect(),
            ..Default::default()
        };
        pull_into(&work, &mut work_feed, None);
        observe_as_pulled(&state, "work@y.example", &work_feed.messages);
        let work_threads: Vec<String> = (1..=count)
            .map(|n| {
                state
                    .store
                    .mail_thread_of_message(&format!("work{n}@x"))
                    .unwrap()
                    .unwrap()
            })
            .collect();
        // as they reach this node: the thread index is node a's own
        rusqlite::Connection::open(state.home.db_path())
            .unwrap()
            .execute("DELETE FROM mail_thread_messages", [])
            .unwrap();

        // this node's own mail, newer, in its live archive
        let archives = tmp.path().join("archives");
        std::fs::create_dir_all(&archives).unwrap();
        let mbox = archives.join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut own = FakeFeed {
            messages: vec![mail(
                50,
                "own@x",
                None,
                &["Category Personal"],
                "lunch monday?",
            )],
            ..Default::default()
        };
        pull_into(&mbox, &mut own, None);
        observe_as_pulled(&state, "jon@y.example", &own.messages);
        let own_thread = state
            .store
            .mail_thread_of_message("own@x")
            .unwrap()
            .unwrap();

        let mut items = vec![serde_json::json!({
            "id": own_thread, "class": "fyi", "confidence": 0.9,
            "span": "lunch monday", "why": "A lunch."
        })];
        items.extend(work_threads.iter().map(|id| {
            serde_json::json!({
                "id": id, "class": "fyi", "confidence": 0.9,
                "span": "the quarterly figures", "why": "Figures."
            })
        }));
        let asked = crate::mail_triage::stand_in_claude(
            &state,
            tmp.path(),
            serde_json::Value::Array(items),
        );
        let round = || crate::mail_triage::triage_for(&state, &duty);
        assert_eq!(round().await.unwrap(), 1);
        let prompt = std::fs::read_to_string(&asked).unwrap();
        assert!(prompt.contains("lunch monday"), "{prompt}");
        assert!(
            !prompt.contains("quarterly figures"),
            "mail that can't be checked never goes to the model: {prompt}"
        );
        for id in &work_threads {
            let t = state.store.mail_thread(id).unwrap().unwrap();
            assert_eq!((t.scope.as_str(), t.state.as_str()), ("personal", "new"));
        }
        // nothing else can be sorted: the round ends
        std::fs::remove_file(&asked).unwrap();
        assert_eq!(round().await.unwrap(), 0);
        assert!(!asked.exists());

        // this node pulls the work mailbox: its live archive holds the mail
        let work_here = archives.join("gmail-live-work.mbox");
        std::fs::File::create(&work_here).unwrap();
        crate::mbox_index::build(&work_here).unwrap();
        pull_into(&work_here, &mut work_feed, None);
        let mut sorted = 0;
        loop {
            match round().await.unwrap() {
                0 => break,
                n => sorted += n,
            }
        }
        assert_eq!(sorted, work_threads.len());
        let prompt = std::fs::read_to_string(&asked).unwrap();
        assert!(prompt.contains("quarterly figures"), "{prompt}");
        for id in &work_threads {
            let t = state.store.mail_thread(id).unwrap().unwrap();
            assert_eq!((t.scope.as_str(), t.state.as_str()), ("personal", "fyi"));
        }
    }

    /// The thread index read failing before a model is asked fails the
    /// round: nothing goes to the model. Once it reads again, the round runs.
    #[tokio::test]
    async fn test_triage_fails_the_round_when_a_threads_mail_cannot_be_looked_up() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![mail(
                1,
                "open@x",
                None,
                &["Category Personal"],
                "lunch monday?",
            )],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        let open = state
            .store
            .mail_thread_of_message("open@x")
            .unwrap()
            .unwrap();
        let asked = crate::mail_triage::stand_in_claude(
            &state,
            tmp.path(),
            serde_json::json!([{
                "id": open, "class": "fyi", "confidence": 0.9,
                "span": "lunch monday", "why": "A lunch."
            }]),
        );
        let index = |from: &str, to: &str| {
            rusqlite::Connection::open(state.home.db_path())
                .unwrap()
                .execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
                .unwrap();
        };
        let duty = crate::mail_triage::every_mailbox();
        let round = || crate::mail_triage::triage_for(&state, &duty);

        index("mail_thread_messages", "mail_thread_messages_aside");
        assert!(round().await.is_err());
        assert!(!asked.exists(), "nothing went to the model");
        index("mail_thread_messages_aside", "mail_thread_messages");
        assert_eq!(round().await.unwrap(), 1);
        assert!(asked.exists());
    }

    /// While mail is held — a private Gmail promotion an earlier version
    /// kept under a `uid:` path can't be told apart from any message —
    /// nothing is recorded about a message's privacy on the hold's strength:
    /// new mail forms no thread and stays to be observed, triage asks no
    /// model and makes no thread private, an ember's queue waits with its
    /// cursor where it was. Only an owner whose vault is open is told which
    /// memory holds mail. Once it leaves the vault, the waiting mail is
    /// recorded as it is — not private — sorted and delivered.
    #[tokio::test]
    async fn test_a_legacy_hold_records_nothing_and_mail_flows_once_it_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let owner = Actor::new("human:test");
        watch(&state, "stars", &["Starred"], &[]);
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        // a thread waiting for triage from before the hold
        let mut feed = FakeFeed {
            messages: vec![mail(
                1,
                "before@x",
                None,
                &["Category Personal"],
                "can you call me?",
            )],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        observe_as_pulled(&state, "jon@y.example", &feed.messages);
        let waiting = state
            .store
            .mail_thread_of_message("before@x")
            .unwrap()
            .unwrap();
        let to_sort = || {
            state
                .store
                .mail_threads_to_triage_for(10, &[], Some(&[]))
                .unwrap()
                .len()
        };
        assert_eq!(to_sort(), 1);
        // a match an ember hasn't read yet
        let earlier = hearthloom_core::store::mail_watch::MailMatch {
            plugin: "stars".into(),
            watch: "mw_earlier".into(),
            watch_name: "watched mail".into(),
            message_id: "before@x".into(),
            sender: "Ann <ann@x.example>".into(),
            subject: "plans".into(),
            text: "can you call me?".into(),
            ..Default::default()
        };
        assert!(state.store.mail_match_add(&earlier).unwrap());

        // what an earlier version kept: a Gmail message promoted private
        // under its uid, from the dashboard
        state
            .store
            .apply_batch(
                &state.home,
                &IngestBatch {
                    actor: "human:dashboard".into(),
                    session_id: None,
                    episodes: vec![hearthloom_core::EpisodeDraft {
                        source: crate::gmail_search::SOURCE.into(),
                        source_ref: "uid:42".into(),
                        kind: "email".into(),
                        title: "promoted".into(),
                        body: "From: Ann <ann@x.example>\nSubject: plans\n\nSee you then.".into(),
                        event_time: hearthloom_core::EventTime::unknown(),
                        trust: "imported".into(),
                        scope: Some("private".into()),
                        entities: vec![],
                        edges: vec![],
                    }],
                },
            )
            .unwrap();

        // new mail, starred, arrives: read as private, recorded as nothing
        let arriving = [mail(
            2,
            "during@x",
            None,
            &["Category Personal", "Starred"],
            "lunch monday?",
        )];
        let seen: Vec<_> = arriving
            .iter()
            .filter_map(|m| {
                crate::mail_triage::seen(
                    &state,
                    "jon@y.example",
                    &BTreeSet::new(),
                    &m.labels,
                    &gmail_imap::message_bytes(m),
                )
            })
            .collect();
        assert!(seen[0].held && seen[0].private);
        assert_eq!(crate::mail_triage::observe(&state, &seen), vec![0]);
        assert!(state
            .store
            .mail_thread_of_message("during@x")
            .unwrap()
            .is_none());
        assert!(state
            .store
            .mail_match_for("stars", "during@x")
            .unwrap()
            .is_none());

        // the ember's queue waits, its cursor where it was
        let got = crate::mail_embers::deliver(&state, "stars", 0, 50).unwrap();
        assert_eq!(got["matches"].as_array().unwrap().len(), 0);
        assert_eq!(got["next"], 0);

        // triage asks no model and makes no thread private
        let asked = crate::mail_triage::stand_in_claude(
            &state,
            tmp.path(),
            serde_json::json!([{
                "id": waiting, "class": "needs_reply", "confidence": 0.9,
                "span": "can you call me", "why": "Ann asks."
            }]),
        );
        let duty = crate::mail_triage::every_mailbox();
        let round = || crate::mail_triage::triage_for(&state, &duty);
        assert_eq!(round().await.unwrap(), 0);
        assert!(!asked.exists(), "nothing went to the model");
        assert_eq!(to_sort(), 1, "still waiting, and not private");

        // which memory holds mail: told only to a viewer whose vault is open
        assert!(crate::mail_triage::status(&state)["held"].is_null());
        state.store.vault_unlock(5);
        let held = crate::mail_triage::status(&state)["held"].clone();
        assert_eq!(
            (held["source"].as_str(), held["source_ref"].as_str()),
            (Some(crate::gmail_search::SOURCE), Some("uid:42"))
        );

        // the owner takes it out of the vault: mail flows
        state
            .store
            .set_episode_scopes(
                &owner,
                &[hearthloom_core::ids::episode_id(
                    crate::gmail_search::SOURCE,
                    "uid:42",
                )],
                "personal",
            )
            .unwrap();
        assert!(crate::mail_triage::status(&state)["held"].is_null());
        state.store.vault_lock();
        assert!(crate::mail_triage::observe(&state, &seen).is_empty());
        let during = state
            .store
            .mail_thread_of_message("during@x")
            .unwrap()
            .expect("recorded once the hold ended");
        assert_eq!(
            state.store.mail_thread(&during).unwrap().unwrap().scope,
            "personal"
        );
        assert!(state
            .store
            .mail_match_for("stars", "during@x")
            .unwrap()
            .is_some());
        assert!(round().await.unwrap() >= 1);
        assert_eq!(
            state.store.mail_thread(&waiting).unwrap().unwrap().state,
            "needs_reply"
        );
        let got = crate::mail_embers::deliver(&state, "stars", 0, 50).unwrap();
        let mut delivered: Vec<&str> = got["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["message_id"].as_str())
            .collect();
        delivered.sort();
        assert_eq!(delivered, vec!["before@x", "during@x"]);
    }

    /// The record's private set is scanned once for a batch of messages —
    /// once for what the pull reads, once when their threads are recorded —
    /// not once per message.
    #[test]
    fn test_mail_privacy_is_read_once_per_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let messages: Vec<PulledMessage> = (1..=6)
            .map(|uid| {
                mail(
                    uid,
                    &format!("m{uid}@x"),
                    None,
                    &["Category Personal"],
                    "hello",
                )
            })
            .collect();
        let reads = || crate::virtual_source::NOT_BARE_READS.with(|n| n.get());
        let before = reads();
        let privacy = crate::virtual_source::MailPrivacy::new(&state);
        let seen: Vec<_> = messages
            .iter()
            .filter_map(|m| {
                crate::mail_triage::seen_with(
                    &state,
                    &privacy,
                    "jon@y.example",
                    &BTreeSet::new(),
                    &m.labels,
                    &gmail_imap::message_bytes(m),
                )
            })
            .collect();
        assert_eq!(seen.len(), 6);
        assert_eq!(reads() - before, 1);
        assert!(crate::mail_triage::observe(&state, &seen).is_empty());
        assert_eq!(reads() - before, 2);
    }

    /// A message whose Message-ID header is folded onto its next line is
    /// archived under the key the mail import and Gmail's label re-read give
    /// it; starred later, it is found by the stretch through the archive,
    /// becomes a memory, and keeps its labels through a rebuild.
    #[test]
    fn test_a_folded_message_id_is_read_again_and_kept_through_a_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let promo = &["Category Promotions"];
        let mut feed = FakeFeed {
            messages: vec![
                mail_with_id_header(1, "Message-ID:\r\n <folded@x>", promo, "a sale"),
                mail(2, "plain@x", None, promo, "another sale"),
            ],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        let key = import_key(&feed.messages[0]);
        assert_eq!(
            crate::mbox_index::labels_of(&mbox, &key)
                .unwrap()
                .as_deref(),
            Some("Category Promotions")
        );

        // starred later; the newest-mail window doesn't hold it
        feed.window = Some(vec![]);
        feed.messages[0].labels.push("Starred".into());
        let pulled = read_cursor(&mbox).unwrap();
        let (plan, gained) = plan_for(&mut feed, &mbox, pulled, RELABEL);
        assert_eq!(
            plan.drafts
                .iter()
                .map(|d| d.source_ref.as_str())
                .collect::<Vec<_>>(),
            vec![key.as_str()]
        );
        assert_eq!(gained.len(), 1);
        record_relabel(&mbox, &plan, &[]).unwrap();

        crate::mbox_index::build(&mbox).unwrap();
        assert_eq!(
            crate::mbox_index::labels_of(&mbox, &key)
                .unwrap()
                .as_deref(),
            Some("Category Promotions, Starred")
        );
        let (plan, gained) = plan_for(&mut feed, &mbox, pulled, RELABEL);
        assert!(plan.changed.is_empty() && gained.is_empty(), "{plan:?}");
    }

    /// A pull that stops after a re-read recorded a message's new label,
    /// before its thread and the embers' matches were made: the next
    /// re-read, finding the labels unchanged, makes them.
    #[tokio::test]
    async fn test_a_recorded_label_is_observed_after_a_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        watch(&state, "stars", &["Starred"], &[]);
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![message(1, "promo1@x", &["Category Promotions"])],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        let pulled = read_cursor(&mbox).unwrap();
        let mine = BTreeSet::new();
        let run = |feed: &mut FakeFeed| {
            relabel(&state, "jon@y.example", &mine, feed, &mbox, pulled, RELABEL)
        };

        feed.messages[0].labels.push("Starred".into());
        STOP_AFTER_RECORD.with(|stop| stop.set(true));
        let stopped = run(&mut feed);
        STOP_AFTER_RECORD.with(|stop| stop.set(false));
        assert!(stopped.is_err());
        assert!(state.store.episode_exists("mbox", "promo1@x").unwrap());
        assert_eq!(
            crate::mbox_index::labels_of(&mbox, "promo1@x")
                .unwrap()
                .as_deref(),
            Some("Category Promotions, Starred")
        );
        assert!(state
            .store
            .mail_thread_of_message("promo1@x")
            .unwrap()
            .is_none());

        assert_eq!(run(&mut feed).unwrap(), (0, 0), "no label changed");
        assert!(state
            .store
            .mail_thread_of_message("promo1@x")
            .unwrap()
            .is_some());
        assert!(state
            .store
            .mail_match_for("stars", "promo1@x")
            .unwrap()
            .is_some());
        assert!(pending_observations(&mbox).unwrap().is_empty());
    }

    /// A message still to be observed stays on the list while the archive's
    /// index can't be read — the list itself, or the message's labels — and
    /// is observed once the index reads again.
    #[tokio::test]
    async fn test_a_pending_observation_is_kept_while_the_index_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_at(tmp.path());
        let mbox = tmp.path().join("gmail-live.mbox");
        std::fs::File::create(&mbox).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let mut feed = FakeFeed {
            messages: vec![mail(
                1,
                "kept@x",
                None,
                &["Category Personal"],
                "can you call me?",
            )],
            ..Default::default()
        };
        pull_into(&mbox, &mut feed, None);
        crate::mbox_index::meta_set(&mbox, PENDING_KEY, r#"["kept@x"]"#).unwrap();
        // a table of the index out of reach for a while, then back
        let index = crate::mbox_index::index_path(&mbox);
        let unreadable = |table: &str, yes: bool| {
            let aside = format!("{table}_aside");
            let (from, to) = if yes {
                (table, &*aside)
            } else {
                (&*aside, table)
            };
            rusqlite::Connection::open(&index)
                .unwrap()
                .execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
                .unwrap();
        };
        let observe = || {
            let pending = pending_observations(&mbox).unwrap();
            observe_pending(&state, "jon@y.example", &BTreeSet::new(), &mbox, pending).unwrap();
        };

        // the list unreadable: an error, not an empty list
        unreadable("meta", true);
        assert!(crate::mbox_index::meta_get(&mbox, PENDING_KEY).is_err());
        assert!(pending_observations(&mbox).is_err());
        unreadable("meta", false);

        // the message's labels unreadable: it stays on the list
        unreadable("messages", true);
        assert!(crate::mbox_index::labels_of(&mbox, "kept@x").is_err());
        observe();
        assert_eq!(pending_observations(&mbox).unwrap(), vec!["kept@x"]);
        assert!(state
            .store
            .mail_thread_of_message("kept@x")
            .unwrap()
            .is_none());

        // readable again: observed, and off the list
        unreadable("messages", false);
        observe();
        assert!(state
            .store
            .mail_thread_of_message("kept@x")
            .unwrap()
            .is_some());
        assert!(pending_observations(&mbox).unwrap().is_empty());
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
            ..Default::default()
        };
        let mut work_feed = FakeFeed {
            messages: vec![message(7, "both@x", &[]), message(9, "w@x", &[])],
            ..Default::default()
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

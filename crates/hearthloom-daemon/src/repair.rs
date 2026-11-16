//! Owner repairs, the daemon side (the mechanism and its rules are in
//! hearthloom-core store/repair.rs): find the memories an earlier version
//! stored wrongly, read their originals again with the current importer,
//! and — only when asked to apply — land the corrected copies in their
//! place. A dry run lists what would change by episode id and a short
//! reason, never a message's or an event's words.
//!
//!   mail     — memories still encoded (an encoded subject or name, a body
//!              still base64 or quoted-printable, raw MIME parts): each
//!              message's original is read from a local archive (the
//!              Takeout's, the live mailboxes'), decoded by the current
//!              reader with the labels its memory was made under, and
//!              replaced when the text differs. A Message-ID is the
//!              sender's choice and an archive can hold several messages
//!              under one, so every archived copy under it is read: the
//!              original is one outside Spam and Trash with the memory's
//!              date and sender, and when such copies read differently the
//!              memory is left (`ambiguous`).
//!   calendar — the given .ics files read by the current importer: an event
//!              the .ics importer stored with other text (an alarm's
//!              attendee or description taken in, a time read another way)
//!              is replaced — one a live calendar feed stored, or whose
//!              writer isn't recorded here, is left; an
//!              occurrence stored under no ref is imported. An event an
//!              earlier importer stored under another ref (an override's
//!              RECURRENCE-ID written another way) is found by its UID and
//!              start and replaced, never imported a second time.
//!   chatgpt  — a ChatGPT export read again (both import routes, `openai`
//!              and `chatlog-chatgpt`): a stored conversation whose
//!              transcript holds turns off the branch it ended on
//!              (abandoned branches mixed in) is replaced — only when the
//!              export holds every turn it shows (a later export's
//!              transcript, or a title alone that differs, is left); one
//!              flagged "do not remember" ends private, put into the vault
//!              before its corrected copy lands. A stored conversation the
//!              export doesn't hold is listed.
//!   health   — an Apple Health export read again: a stored day summary or
//!              workout whose text differs (units averaged together, metres
//!              read as kilometres, sleep added across trackers) is
//!              replaced; days and workouts the export doesn't cover, the
//!              day it was made on (only part of it is in it), and a day
//!              showing a tracker or a measure the export lacks, are left.
//!   copies   — growing sources' days and sessions stored under an
//!              unversioned base re-minted as `<base>#r<n>` from their
//!              stored text, so the growing rule keeps the fullest copy in
//!              view on every node (each node re-mints its own when the run
//!              reaches it — core store/repair.rs).
//!   photodays — photo days whose count is inflated by duplicate points (a
//!              photo shown twice, or a count minted here above the photos
//!              stored) re-minted under their next revision when the photos
//!              stored here cover every one the day shows and every place
//!              it names; the minted ledger set with it. A day that reads
//!              otherwise for another reason is only counted.
//!
//! Only the owner, with the vault open (http.rs `repair_allowed`), one run
//! at a time on a node.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use hearthloom_core::store::{
    replacement_ref, Actor, PhotoLedger, RepairApplied, RepairItem, RepairTarget, RepairWork,
    SameEvent,
};
use hearthloom_core::EpisodeDraft;
use serde::Serialize;
use serde_json::{json, Value};

use crate::AppState;

/// The nodes (by data home) where a scan or run is going on: one at a
/// time on a node (a second would find the same work; the store would land
/// it once, but twice the reading). Keyed by home, not one lock for the
/// process, so two nodes in one process (the tests) never wait on each
/// other — a daemon has one home, so it is the same lock there.
static RUNNING: Mutex<std::collections::BTreeSet<PathBuf>> =
    Mutex::new(std::collections::BTreeSet::new());

/// Findings listed per report (the counts cover them all).
const LISTED: usize = 2000;

/// One memory a repair looked at closely: its id, what happens to it, why.
/// Never its words.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub episode: String,
    pub status: &'static str,
    pub reason: String,
}

/// What a repair would do.
#[derive(Debug, Default)]
pub struct Plan {
    pub kind: &'static str,
    /// Memories (mail, conversations, health records, days and sessions,
    /// photo days) or events (calendar) read.
    pub checked: usize,
    pub counts: BTreeMap<&'static str, usize>,
    pub findings: Vec<Finding>,
    pub items: Vec<RepairItem>,
    pub additions: Vec<EpisodeDraft>,
    /// Who the additions are imported as.
    pub addition_actor: &'static str,
    /// Copies re-minted under their source's own revision rule (growing
    /// sources' days and sessions, photo days).
    pub revisions: Vec<EpisodeDraft>,
    /// Photo-day ledgers set with the run.
    pub photo_ledgers: Vec<PhotoLedger>,
    /// Records (source, ref) the owner flagged "do not remember" that go
    /// into the vault with the run.
    pub private: Vec<(String, String)>,
    /// The images corrected ChatGPT transcripts name, copied from the
    /// export (`export_dir`) when the run is applied.
    pub images: Vec<ImageCopy>,
    pub export_dir: Option<PathBuf>,
    /// An export unpacked for the run: held, never read — the folder goes
    /// when the plan does.
    pub _unpacked: Option<tempfile::TempDir>,
    /// New episodes the extraction pass would read (their source switched
    /// on for it, not private, not retired) — each costs model tokens.
    pub extraction_again: usize,
    /// Notes about the run as a whole (an archive that couldn't be read).
    pub notes: Vec<String>,
}

/// The images one conversation's corrected transcript names.
#[derive(Debug, Clone)]
pub struct ImageCopy {
    pub conversation: String,
    pub pointers: Vec<String>,
}

impl Plan {
    fn new(kind: &'static str) -> Plan {
        Plan {
            kind,
            ..Default::default()
        }
    }

    fn note(&mut self, episode: &str, status: &'static str, reason: impl Into<String>) {
        *self.counts.entry(status).or_default() += 1;
        self.findings.push(Finding {
            episode: episode.to_string(),
            status,
            reason: reason.into(),
        });
    }

    fn count(&mut self, status: &'static str) {
        *self.counts.entry(status).or_default() += 1;
    }

    fn replace(&mut self, state: &AppState, target: &RepairTarget, draft: EpisodeDraft, why: &str) {
        self.replace_as(state, target, draft, why, false)
    }

    /// `replace`; `ends_private`: the record goes into the vault before the
    /// run (`apply`), so the copy lands private and the extraction pass
    /// never reads it.
    fn replace_as(
        &mut self,
        state: &AppState,
        target: &RepairTarget,
        draft: EpisodeDraft,
        why: &str,
        ends_private: bool,
    ) {
        if !target.retired
            && !ends_private
            && target.scope != "private"
            && crate::consolidate::extraction_tier(&state.store, &target.source).is_some()
        {
            self.extraction_again += 1;
        }
        let why = if target.retired {
            format!("{why}; retired, stays retired")
        } else {
            why.to_string()
        };
        self.note(&target.current_id, "replace", why);
        self.items.push(RepairItem {
            base_ref: target.base_ref.clone(),
            draft,
        });
    }

    fn add(&mut self, state: &AppState, draft: EpisodeDraft, why: &str) {
        if draft.scope.as_deref() != Some("private")
            && crate::consolidate::extraction_tier(&state.store, &draft.source).is_some()
        {
            self.extraction_again += 1;
        }
        let id = hearthloom_core::ids::episode_id(&draft.source, &draft.source_ref);
        self.note(&id, "add", why);
        self.additions.push(draft);
    }

    /// The report a dry run (applied None) or a run returns.
    pub fn report(&self, applied: Option<&RepairApplied>) -> Value {
        let listed: Vec<&Finding> = self.findings.iter().take(LISTED).collect();
        let mut out = json!({
            "kind": self.kind,
            "dry_run": applied.is_none(),
            "checked": self.checked,
            "counts": self.counts,
            "replace": self.items.len(),
            "add": self.additions.len(),
            "remint": self.revisions.len(),
            "ledger": self.photo_ledgers.len(),
            "private": self.private.len(),
            "extraction_again": self.extraction_again,
            "findings": listed,
            "findings_total": self.findings.len(),
            "notes": self.notes,
        });
        if let Some(a) = applied {
            out["run"] = json!(a.run);
            out["replaced"] = json!(a.replaced);
            out["added"] = json!(a.added);
            out["reminted"] = json!(a.revised);
            out["ledgers"] = json!(a.ledgers);
            out["made_private"] = json!(a.made_private);
            out["existing"] = json!(a.existing);
        }
        out
    }
}

/// What a node publishes in the node list (`roles.repairs`) once it
/// settles owner repairs (v69). A node without it stores a corrected copy
/// as one more memory beside the original.
pub const REPAIRS_ROLE: &str = "repairs";

/// A node not seen for this long doesn't hold up a repair.
const NODE_RECENT_DAYS: i64 = 30;

/// The other nodes seen since `cutoff` that don't publish
/// [`REPAIRS_ROLE`], each as `name (version)`.
pub(crate) fn older_nodes_from(me: &str, nodes: &[Value], cutoff: &str) -> Vec<String> {
    nodes
        .iter()
        .filter(|n| n["node_id"].as_str().is_some_and(|id| id != me))
        .filter(|n| n["last_seen"].as_str().is_some_and(|t| t > cutoff))
        .filter(|n| n["roles"][REPAIRS_ROLE] != json!(true))
        .map(|n| {
            format!(
                "{} ({})",
                n["name"].as_str().unwrap_or("a node"),
                n["version"].as_str().unwrap_or("an older version")
            )
        })
        .collect()
}

/// This person's other nodes, seen in the last 30 days, that don't know
/// owner repairs yet.
pub fn older_nodes(state: &AppState) -> Vec<String> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(NODE_RECENT_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    older_nodes_from(
        &crate::agents::node_id(state),
        &state.store.nodes().unwrap_or_default(),
        &cutoff,
    )
}

/// What a run means on those nodes, and what to do about it.
pub fn older_nodes_note(older: &[String]) -> String {
    format!(
        "{} {} a version that doesn't know owner repairs: there each corrected copy lands as a \
         second memory beside the original, in view even where the original is retired or \
         private on that node, and through an older hub an undo doesn't reach your other \
         nodes. Upgrade {} first — applying is refused until then, unless you pass \
         --allow-older-nodes",
        older.join(", "),
        if older.len() == 1 { "runs" } else { "run" },
        if older.len() == 1 { "it" } else { "them" },
    )
}

/// A scan or run going on at a node; the node is free again when dropped.
pub struct Running(PathBuf);

impl Drop for Running {
    fn drop(&mut self) {
        running_homes().remove(&self.0);
    }
}

fn running_homes() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<PathBuf>> {
    RUNNING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Is a repair running at this node? Taken by a scan or run for its whole
/// length; None while one is.
pub fn try_start(home: &hearthloom_core::Home) -> Option<Running> {
    let key = home.root().to_path_buf();
    // the set's lock ends with this statement: a `Running` made (or
    // dropped) while it is held would lock it again
    let started = running_homes().insert(key.clone());
    started.then(|| Running(key))
}

/// Land a plan (blocking: call from the blocking pool). The records an
/// export flags "do not remember" go into the vault first, so their
/// corrected copies land private (a copy takes its record's label) — never
/// in view of the extraction pass or a peer outside the vault, not even
/// for a moment, and still in the vault if the run fails. The images the
/// corrected ChatGPT transcripts name go in place next.
pub fn apply(state: &AppState, actor: &str, plan: &Plan) -> Result<RepairApplied> {
    let owner = Actor::new(actor);
    let made_private = if plan.private.is_empty() {
        0
    } else {
        state
            .store
            .label_private_on_reimport(&owner, &[], &plan.private)?
    };
    if let Some(dir) = &plan.export_dir {
        let media = state.home.raw_dir().join("openai/media");
        for copy in &plan.images {
            hearthloom_adapters::openai::copy_images(
                dir,
                &media,
                &copy.conversation,
                &copy.pointers,
            );
        }
    }
    let mut applied = state.store.repair_apply_work(
        &state.home,
        &owner,
        plan.kind,
        RepairWork {
            items: &plan.items,
            revisions: &plan.revisions,
            additions: &plan.additions,
            addition_actor: plan.addition_actor,
            photo_ledgers: &plan.photo_ledgers,
        },
    )?;
    applied.made_private = made_private;
    Ok(applied)
}

/* ————— mail ————— */

/// Does a text hold an RFC 2047 encoded word (`=?charset?B|Q?…?=`)?
fn has_encoded_word(s: &str) -> bool {
    let mut rest = s;
    while let Some(i) = rest.find("=?") {
        let after = &rest[i + 2..];
        if let Some(q) = after.find('?') {
            let charset = &after[..q];
            let tail = &after.as_bytes()[q + 1..];
            if !charset.is_empty()
                && charset.len() <= 40
                && !charset.contains(char::is_whitespace)
                && tail.len() >= 2
                && matches!(tail[0], b'B' | b'b' | b'Q' | b'q')
                && tail[1] == b'?'
                && after[q + 3..].contains("?=")
            {
                return true;
            }
        }
        rest = &rest[i + 2..];
    }
    false
}

/// A line of base64 text.
fn base64_line(line: &str) -> bool {
    let t = line.trim_end();
    let data = t.trim_end_matches('=');
    let alphabet = data
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    // a run of letters is a word (a capital first, maybe); base64 mixes in
    // digits, or capitals inside lower case
    let mixed = data
        .bytes()
        .any(|b| b.is_ascii_digit() || b == b'+' || b == b'/')
        || (data.bytes().skip(1).any(|b| b.is_ascii_uppercase())
            && data.bytes().any(|b| b.is_ascii_lowercase()));
    t.len() >= 16 && t.len() - data.len() <= 2 && alphabet && mixed
}

/// A text that is base64 as a whole — every filled line in its alphabet,
/// padding only at the end — and decodes to readable text, UTF-8 or a
/// single-byte charset: a short one too (`SGVsbG8=`, `Q2Fm6Q==`), or one
/// whose last line is short, which a line-by-line look misses. A word or a
/// hash that happens to fit the alphabet decodes to bytes that aren't text.
fn base64_text(lines: &[&str]) -> bool {
    use base64::Engine;
    let joined: String = lines.iter().map(|l| l.trim()).collect();
    let data = joined.trim_end_matches('=');
    if joined.len() < 4
        || !joined.len().is_multiple_of(4)
        || joined.len() - data.len() > 2
        || !data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&joined) else {
        return false;
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            // a single-byte charset (Latin-1 `Café`): no control bytes, and
            // more ASCII letters than bytes beyond ASCII — a word decoded by
            // chance gives mostly high or control bytes
            let bytes = e.into_bytes();
            let control = bytes
                .iter()
                .any(|&c| (c < 0x20 && !c.is_ascii_whitespace()) || c == 0x7F);
            let letters = bytes.iter().filter(|c| c.is_ascii_alphabetic()).count();
            let high = bytes.iter().filter(|&&c| c >= 0x80).count();
            return !control && letters > high;
        }
    };
    text.chars().any(char::is_alphabetic)
        && !text.chars().any(|c| c.is_control() && !c.is_whitespace())
}

/// Quoted-printable escapes that only an encoder writes, however few: a run
/// that decodes to UTF-8 beyond ASCII (`=C3=A9` is é), an escaped `=`
/// (`=3D`), a Latin-1 letter inside a word (`f=FCr`) or a lower-case one
/// ending it (`Caf=E9`), or an escaped space before a word (`Hello=20world`).
fn qp_escapes(text: &str) -> bool {
    let b = text.as_bytes();
    let hex = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let escape_at = |i: usize| -> Option<u8> {
        (b.get(i) == Some(&b'='))
            .then(|| Some(hex(*b.get(i + 1)?)? * 16 + hex(*b.get(i + 2)?)?))
            .flatten()
    };
    let mut i = 0;
    while i < b.len() {
        let Some(first) = escape_at(i) else {
            i += 1;
            continue;
        };
        let start = i;
        let mut run = vec![first];
        i += 3;
        while let Some(next) = escape_at(i) {
            run.push(next);
            i += 3;
        }
        if run.contains(&b'=') {
            return true;
        }
        if run.iter().any(|&c| c >= 0x80) && std::str::from_utf8(&run).is_ok() {
            return true;
        }
        let before = start.checked_sub(1).and_then(|s| b.get(s));
        let after = b.get(i);
        let lower = |at: Option<&u8>| at.is_some_and(u8::is_ascii_lowercase);
        let latin_letters = run.iter().all(|&c| c >= 0xC0 && c != 0xD7 && c != 0xF7);
        if latin_letters && lower(before) && lower(after) {
            return true;
        }
        // a lower-case Latin-1 letter ending a word (`Caf=E9`); a capital
        // there (`lang=DE`) is a value, not a letter
        let latin_lower = run.iter().all(|&c| c >= 0xDF && c != 0xF7);
        if latin_lower && lower(before) && !after.is_some_and(u8::is_ascii_alphanumeric) {
            return true;
        }
        // an escaped space or tab between a word and the next (`Hello=20world`)
        let blanks = run.iter().all(|&c| c == b' ' || c == b'\t');
        let graphic = before.is_some_and(|&c| c.is_ascii_graphic() && c != b'=');
        if blanks && graphic && after.is_some_and(u8::is_ascii_alphabetic) {
            return true;
        }
    }
    false
}

/// What makes a stored mail memory look like an earlier version stored it
/// undecoded. Empty: it reads as decoded text. A memory flagged here is
/// only read again: its original decoded by the current reader decides
/// whether anything changes.
pub(crate) fn still_encoded(title: &str, body: &str) -> Vec<&'static str> {
    let mut why = Vec::new();
    let (head, text) = body.split_once("\n\n").unwrap_or((body, ""));
    if has_encoded_word(title) || head.lines().any(has_encoded_word) {
        why.push("encoded subject or name");
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.iter().any(|l| {
        let l = l.trim_start().to_ascii_lowercase();
        l.starts_with("content-type:") || l.starts_with("content-transfer-encoding:")
    }) {
        why.push("raw MIME parts");
    }
    let filled = lines.iter().filter(|l| !l.trim().is_empty()).count();
    let base64 = lines.iter().filter(|l| base64_line(l)).count();
    if base64 >= 3 || (base64 >= 1 && base64 == filled) || base64_text(&lines) {
        why.push("base64 text");
    }
    let soft_breaks = lines
        .iter()
        .filter(|l| l.len() >= 20 && l.ends_with('=') && !base64_line(l))
        .count();
    let hex = |b: u8| b.is_ascii_digit() || (b'A'..=b'F').contains(&b);
    let escapes = text
        .as_bytes()
        .windows(3)
        .filter(|w| w[0] == b'=' && hex(w[1]) && hex(w[2]))
        .count();
    if soft_breaks >= 2 || escapes >= 3 || qp_escapes(text) {
        why.push("quoted-printable text");
    }
    why
}

/// The labels a mail memory was made under (its `labels:` line).
fn stored_labels(body: &str) -> Vec<String> {
    body.split("\n\n")
        .next()
        .unwrap_or("")
        .lines()
        .find_map(|l| l.strip_prefix("labels: "))
        .map(|l| l.split(", ").map(String::from).collect())
        .unwrap_or_default()
}

/// The sender's address on a mail memory's `from:` line (lowercase), the
/// part every version of the reader wrote the same way.
fn sender_of(body: &str) -> Option<String> {
    hearthloom_core::store::mail_sender(body)
}

/// Labels a message the import never keeps sits under.
fn dropped_labels(labels: &str) -> bool {
    labels.split(',').any(|l| {
        let l = l.trim();
        l.eq_ignore_ascii_case("spam") || l.eq_ignore_ascii_case("trash")
    })
}

/// The mail archives this node holds: every enabled registered mail
/// archive and each mailbox's live archive. Those whose index can be read
/// are returned; each one that can't be read now — its mount doesn't
/// answer, its index is out of date or can't be opened — is named in
/// `unavailable` with why. It may hold the original of any message, so
/// the caller repairs none while one is listed.
fn mail_archives(state: &AppState, unavailable: &mut Vec<String>) -> Vec<PathBuf> {
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    for def in state.store.list_virtual_sources().unwrap_or_default() {
        if def.adapter != "mbox" || !crate::virtual_source::is_enabled(&def) {
            continue;
        }
        match crate::virtual_source::mbox_path_if_online(&def) {
            Some(path) => found.push((path, def.name.clone())),
            None => unavailable.push(format!("{} doesn't answer", def.name)),
        }
    }
    for mb in crate::mailboxes::all() {
        let path = mb.live_mbox_path(state);
        if path.exists() {
            found.push((path, mb.source_name()));
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for (path, name) in found {
        if !seen.insert(path.clone()) {
            continue;
        }
        if crate::mbox_index::is_stale(&path) {
            unavailable.push(format!(
                "the index of {name} is out of date or can't be opened"
            ));
        } else {
            out.push(path);
        }
    }
    out
}

/// The mail repair's plan (blocking).
pub fn plan_mail(state: &AppState) -> Result<Plan> {
    let mut plan = Plan::new("mail");
    // the privacy rules over each original read whole: the corrected copy
    // is cut as the import cuts it
    let rules = state.store.list_privacy_rules()?;
    // An archive that can't be read now may hold the original — or another
    // message under the same Message-ID that would make it ambiguous — so
    // while one is listed no message is repaired.
    let mut unavailable = Vec::new();
    let archives = mail_archives(state, &mut unavailable);
    // each archive with the later copies its index keeps by offset
    let mut readable = Vec::new();
    for archive in &archives {
        match crate::mbox_index::offset_keyed_ids(archive) {
            Ok(keyed) => readable.push((archive, keyed)),
            Err(err) => unavailable.push(format!("{} can't be read ({err:#})", archive.display())),
        }
    }
    for why in &unavailable {
        plan.notes.push(format!(
            "{why} — no message is repaired until every archive can be read"
        ));
    }
    if archives.is_empty() && unavailable.is_empty() {
        plan.notes
            .push("no mail archive with a readable index on this node".into());
    }
    for target in state.store.repair_targets("mbox")? {
        plan.checked += 1;
        let Some(body) = target.body(&state.home) else {
            plan.note(
                &target.current_id,
                "unreadable",
                "its stored text can't be read",
            );
            continue;
        };
        let why = still_encoded(&target.title, &body);
        if why.is_empty() {
            plan.count("fine");
            continue;
        }
        let why = why.join(", ");
        if target
            .base_ref
            .starts_with(hearthloom_adapters::mbox::UNKEYED_REF_PREFIX)
        {
            plan.note(
                &target.current_id,
                "no_message_id",
                format!("{why}; no Message-ID to find it by in an archive"),
            );
            continue;
        }
        if !unavailable.is_empty() {
            plan.note(
                &target.current_id,
                "archive_error",
                format!("{why}; an archive that may hold it can't be read"),
            );
            continue;
        }
        // Every archived copy under its Message-ID (ids are the sender's
        // choice, and an archive can hold several messages under one):
        // the original is a copy the import could have kept — not in
        // Spam or Trash — with the memory's date and sender, and all such
        // copies must read the same.
        let mut copies = Vec::new();
        let mut failed = None;
        for (archive, keyed) in &readable {
            match crate::mbox_index::copies_of(archive, &target.base_ref, keyed) {
                Ok(found) => copies.extend(found),
                Err(err) => failed = Some(format!("{err:#}")),
            }
        }
        if let Some(err) = failed {
            plan.note(
                &target.current_id,
                "archive_error",
                format!("{why}; an archive couldn't be read ({err})"),
            );
            continue;
        }
        if copies.is_empty() {
            plan.note(
                &target.current_id,
                "not_in_archive",
                format!("{why}; its original isn't in a local archive"),
            );
            continue;
        }
        let labels = stored_labels(&body);
        let sender = sender_of(&body);
        let mut readings: Vec<EpisodeDraft> = Vec::new();
        let mut rule_matches = false;
        // the reader drafts a message private itself (more link hosts than
        // their room holds: a domain rule made later couldn't find them)
        let mut drafted_private = false;
        for copy in &copies {
            if dropped_labels(&copy.labels) {
                continue;
            }
            let Ok(draft) =
                hearthloom_adapters::mbox::draft_with_labels(&copy.bytes, &labels, None)
            else {
                continue;
            };
            if draft.source_ref != target.base_ref
                || draft.event_time.utc != target.event_time
                || sender_of(&draft.body) != sender
            {
                continue;
            }
            rule_matches =
                rule_matches || crate::mbox_text::rules_match_message(&rules, &copy.bytes);
            drafted_private = drafted_private || draft.scope.as_deref() == Some("private");
            if !readings
                .iter()
                .any(|r| r.title == draft.title && r.body == draft.body)
            {
                readings.push(draft);
            }
        }
        if readings.len() > 1 {
            plan.note(
                &target.current_id,
                "ambiguous",
                format!(
                    "{why}; {} archived messages carry its Message-ID, date and sender with different text",
                    readings.len()
                ),
            );
            continue;
        }
        let Some(draft) = readings.pop() else {
            plan.note(
                &target.current_id,
                "other_message",
                format!(
                    "{why}; no archived message under its Message-ID has its date and sender outside Spam and Trash"
                ),
            );
            continue;
        };
        if draft.title == target.title && draft.body == body {
            plan.note(
                &target.current_id,
                "unchanged",
                format!("{why}; but it reads the same now"),
            );
            continue;
        }
        let fixed = replacement_ref(&target.base_ref, &draft.title, &draft.body);
        if target.replacements.iter().any(|(r, _)| *r == fixed) {
            plan.note(
                &target.current_id,
                "undone",
                "this correction was made and undone; left as it is",
            );
            continue;
        }
        // a rule matching the original read whole, or the reader drafting it
        // private, puts the record into the vault before the run (`apply`),
        // so the corrected copy lands private
        let to_vault = (rule_matches || drafted_private) && target.scope != "private";
        if to_vault {
            plan.private
                .push((target.source.clone(), target.base_ref.clone()));
            let why = if rule_matches {
                format!("{why}; a privacy rule matches it, it ends private")
            } else {
                format!(
                    "{why}; it links to more hosts than its stored text has room for, it ends private"
                )
            };
            plan.replace_as(state, &target, draft, &why, true);
        } else {
            plan.replace(state, &target, draft, &why);
        }
    }
    Ok(plan)
}

/* ————— calendar ————— */

/// A calendar file the owner gave: its name (for messages) and its text.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CalendarFile {
    pub name: String,
    pub text: String,
}

/// The parts of a stored event's text, by what they say: title, location,
/// time, organizer, attendees, description.
fn event_parts(body: &str) -> BTreeMap<&'static str, Vec<&str>> {
    let mut parts: BTreeMap<&'static str, Vec<&str>> = BTreeMap::new();
    let (mut attendees, mut description) = (false, false);
    for line in body.lines() {
        if description {
            parts.entry("description").or_default().push(line);
            continue;
        }
        if line.is_empty() {
            description = true;
            continue;
        }
        if attendees && line.starts_with("  ") {
            parts.entry("attendees").or_default().push(line);
            continue;
        }
        attendees = false;
        let part = match line.split_once(':').map(|(k, _)| k) {
            Some("event") => "title",
            Some("location") => "location",
            Some("start") | Some("end") => "time",
            Some("organizer") => "organizer",
            Some("attendees") => {
                attendees = true;
                "attendees"
            }
            _ => "other",
        };
        parts.entry(part).or_default().push(line);
    }
    // trailing blank lines say nothing
    if let Some(d) = parts.get_mut("description") {
        while d.last().is_some_and(|l| l.trim().is_empty()) {
            d.pop();
        }
    }
    parts
}

/// Which parts of an event read otherwise now.
fn differing_parts(old_title: &str, old: &str, new_title: &str, new: &str) -> String {
    let (a, b) = (event_parts(old), event_parts(new));
    let mut keys: Vec<&'static str> = a.keys().chain(b.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let mut differ: Vec<&str> = keys.into_iter().filter(|k| a.get(k) != b.get(k)).collect();
    if old_title != new_title && !differ.contains(&"title") {
        differ.insert(0, "title");
    }
    if differ.is_empty() {
        "text differs".into()
    } else {
        format!("{} differ", differ.join(", "))
    }
}

/// An event's calendar UID, as its draft names it.
fn uid_of(draft: &EpisodeDraft) -> Option<&str> {
    draft
        .entities
        .iter()
        .find(|e| e.entity_type == "Event")
        .and_then(|e| e.attributes.iter().find(|(k, _)| k == "ics-uid"))
        .map(|(_, v)| v.as_str())
}

/// The writer every .ics import is recorded under.
const ICS_IMPORTER: &str = "adapter:ics";

/// The calendar repair's plan (blocking).
pub fn plan_calendar(state: &AppState, files: &[CalendarFile]) -> Result<Plan> {
    let mut plan = Plan::new("calendar");
    plan.addition_actor = ICS_IMPORTER;
    // one correction per record and one import per ref in a run
    let mut planned: HashSet<String> = HashSet::new();
    for file in files {
        let drafts = match hearthloom_adapters::ics::ingest_text(&file.text, None, &file.name) {
            Ok(d) => d,
            // never the importer's error: it can quote an event's title
            Err(_) => {
                plan.notes.push(format!(
                    "{} can't be read: it holds no event the importer can read",
                    file.name
                ));
                continue;
            }
        };
        for draft in drafts {
            plan.checked += 1;
            let id = hearthloom_core::ids::episode_id(&draft.source, &draft.source_ref);
            let found = match state.store.repair_target("ics", &draft.source_ref)? {
                Some(t) => Some((t, false)),
                None => {
                    let (Some(start), Some(uid)) =
                        (draft.event_time.utc.as_deref(), uid_of(&draft))
                    else {
                        plan.note(
                            &id,
                            "unmatched",
                            "no start time to tell it from an event stored under another ref; not imported",
                        );
                        continue;
                    };
                    match state.store.repair_same_event("ics", uid, start)? {
                        SameEvent::Stored(base) => {
                            state.store.repair_target("ics", &base)?.map(|t| (t, true))
                        }
                        SameEvent::Unsure(stored) => {
                            plan.note(
                                &id,
                                "unmatched",
                                format!(
                                    "{stored}, stored at its start, may be this event or another \
                                     whose UID begins the same way, and can't tell; not imported"
                                ),
                            );
                            continue;
                        }
                        SameEvent::Absent => None,
                    }
                }
            };
            let Some((target, elsewhere)) = found else {
                if planned.insert(format!("add:{}", draft.source_ref)) {
                    plan.add(state, draft, "an occurrence not stored yet");
                }
                continue;
            };
            if !planned.insert(target.base_ref.clone()) {
                plan.count("same_event");
                continue;
            }
            // Only what the .ics importer stored is read again from a file:
            // the phone's calendar feed stores events under the same source
            // and UIDs, in its own shape and with its own times.
            if target.base_writer.as_deref() != Some(ICS_IMPORTER) {
                plan.note(
                    &target.current_id,
                    "other_writer",
                    "not stored by the .ics importer (a live calendar feed, or a writer not recorded here); left as it is",
                );
                continue;
            }
            let Some(body) = target.body(&state.home) else {
                plan.note(
                    &target.current_id,
                    "unreadable",
                    "its stored text can't be read",
                );
                continue;
            };
            if draft.title == target.title && draft.body == body {
                plan.count("fine");
                continue;
            }
            let fixed = replacement_ref(&target.base_ref, &draft.title, &draft.body);
            if target.replacements.iter().any(|(r, _)| *r == fixed) {
                plan.note(
                    &target.current_id,
                    "undone",
                    "this correction was made and undone; left as it is",
                );
                continue;
            }
            let mut why = differing_parts(&target.title, &body, &draft.title, &draft.body);
            if elsewhere {
                why.push_str("; stored under an earlier ref");
            }
            plan.replace(state, &target, draft, &why);
        }
    }
    Ok(plan)
}

/* ————— exports given by path ————— */

/// The folder under `root` (itself, or one at most three levels down)
/// that `holds` says is the export. Links aren't followed.
fn find_folder(root: &Path, holds: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut level = vec![root.to_path_buf()];
    for _ in 0..4 {
        if let Some(found) = level.iter().find(|d| holds(d)) {
            return Some(found.clone());
        }
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut subdirs: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n != "__MACOSX"))
                .collect();
            subdirs.sort();
            next.extend(subdirs);
        }
        level = next;
    }
    None
}

/// An export the owner named by its path: a folder (or a folder holding
/// it), or a zip — unpacked into a temporary folder that lives as long as
/// the returned handle, within what the disks can spare
/// (`inbox::unpack_budget`).
fn export_folder(
    state: &AppState,
    path: &Path,
    holds: &dyn Fn(&Path) -> bool,
    what: &str,
) -> Result<(PathBuf, Option<tempfile::TempDir>)> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.is_dir() {
        let dir =
            find_folder(path, holds).ok_or_else(|| anyhow!("no {what} in {}", path.display()))?;
        return Ok((dir, None));
    }
    let is_zip = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if !is_zip {
        let parent = path.parent().filter(|p| holds(p));
        return match parent {
            Some(dir) => Ok((dir.to_path_buf(), None)),
            None => Err(anyhow!("{} is neither a folder nor a zip", path.display())),
        };
    }
    let unpacked = crate::inbox::extract_archive(path, crate::inbox::unpack_budget(&state.home))?;
    let dir = find_folder(unpacked.path(), holds)
        .ok_or_else(|| anyhow!("no {what} in {}", path.display()))?;
    Ok((dir, Some(unpacked)))
}

/* ————— chatgpt ————— */

/// A folder holding a ChatGPT export's conversations.
fn holds_conversations(dir: &Path) -> bool {
    dir.join("conversations.json").is_file()
        || std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.filter_map(|e| e.ok()).any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("conversations-") && name.ends_with(".json")
            })
        })
}

/// The turns of a transcript (`<header>\n\n` then `<turn>\n\n`…, maybe cut
/// with `[transcript truncated]`), read against `known` — the turns of the
/// export's tree as the route writes them: at each turn, the longest known
/// turn its text starts with that ends where a turn ends; else, up to the
/// next paragraph that opens with a speaker (a turn the export doesn't
/// hold). Returns the turns and how many of them aren't known.
fn transcript_turns<'a>(body: &'a str, known: &[String]) -> (Vec<&'a str>, usize) {
    let Some((_, rest)) = body.split_once("\n\n") else {
        return (vec![], 0);
    };
    let rest = rest
        .strip_suffix("[transcript truncated]\n")
        .unwrap_or(rest)
        .trim_end_matches('\n');
    let next_turn = |s: &str| {
        ["\n\nyou: ", "\n\nchatgpt: "]
            .iter()
            .filter_map(|m| s.find(m))
            .min()
    };
    let (mut turns, mut unknown, mut at) = (Vec::new(), 0, 0);
    while at < rest.len() {
        let here = &rest[at..];
        let ends = |len: usize| len == here.len() || here[len..].starts_with("\n\n");
        let len = match known
            .iter()
            .filter(|k| here.starts_with(k.as_str()) && ends(k.len()))
            .map(String::len)
            .max()
        {
            Some(len) => len,
            None => {
                unknown += 1;
                next_turn(here).filter(|&i| i > 0).unwrap_or(here.len())
            }
        };
        turns.push(&here[..len]);
        at += len;
        if rest[at..].starts_with("\n\n") {
            at += 2;
        }
    }
    (turns, unknown)
}

/// How often each turn occurs.
fn turn_counts<'a>(turns: impl IntoIterator<Item = &'a str>) -> HashMap<&'a str, usize> {
    let mut counts = HashMap::new();
    for t in turns {
        *counts.entry(t).or_default() += 1;
    }
    counts
}

/// Does `part` hold no turn more often than `whole`?
fn within(part: &HashMap<&str, usize>, whole: &HashMap<&str, usize>) -> bool {
    part.iter()
        .all(|(t, n)| whole.get(t).is_some_and(|m| m >= n))
}

/// What the ChatGPT repair makes of a stored transcript, given the
/// corrected one (the branch the conversation ended on) and every turn of
/// the export's tree.
#[derive(Debug, PartialEq)]
enum TranscriptVerdict {
    /// The defect, and nothing the export lacks: every stored turn is one
    /// of the export's, and some are off the branch (or in another order).
    Replace(String),
    /// It holds turns the export doesn't (a later export's, or images
    /// named otherwise): replacing it would lose them.
    Holds(usize),
    /// Its turns are the branch's own: only the title or header differ.
    SameTurns,
    /// Every turn it holds is on the branch, which holds more (a later
    /// export's): nothing to correct.
    Fewer(usize),
}

fn transcript_verdict(stored: &str, corrected: &str, every_turn: &[String]) -> TranscriptVerdict {
    let (old, unknown) = transcript_turns(stored, every_turn);
    let (new, _) = transcript_turns(corrected, every_turn);
    let all = turn_counts(every_turn.iter().map(String::as_str));
    let (old_counts, new_counts) = (
        turn_counts(old.iter().copied()),
        turn_counts(new.iter().copied()),
    );
    if unknown > 0 || !within(&old_counts, &all) {
        let beyond = old_counts
            .iter()
            .map(|(t, n)| n.saturating_sub(all.get(t).copied().unwrap_or(0)))
            .sum::<usize>()
            .max(unknown);
        return TranscriptVerdict::Holds(beyond);
    }
    if old == new {
        return TranscriptVerdict::SameTurns;
    }
    if within(&old_counts, &new_counts) && old.len() < new.len() {
        return TranscriptVerdict::Fewer(new.len() - old.len());
    }
    TranscriptVerdict::Replace(if old.len() != new.len() {
        format!(
            "{} turns stored, {} on the branch it ended on",
            old.len(),
            new.len()
        )
    } else {
        "its turns stored in another order".to_string()
    })
}

/// The ChatGPT repair's plan (blocking). `path`: the export — a folder, or
/// a zip, holding `conversations.json` or `conversations-NNN.json`.
pub fn plan_chatgpt(state: &AppState, path: &Path) -> Result<Plan> {
    let mut plan = Plan::new("chatgpt");
    let (dir, unpacked) = export_folder(state, path, &holds_conversations, "ChatGPT export")?;
    let media = state.home.raw_dir().join("openai/media");
    let conversations = hearthloom_adapters::openai::read_routes(&dir, Some(&media))?;
    const SOURCES: [&str; 2] = ["openai", "chatlog-chatgpt"];
    let mut targets: HashMap<(&str, String), RepairTarget> = HashMap::new();
    for source in SOURCES {
        for t in state.store.repair_targets(source)? {
            targets.insert((source, t.base_ref.clone()), t);
        }
    }
    let mut seen: HashSet<(&str, String)> = HashSet::new();
    for conv in &conversations {
        let mut stored = false;
        for source in SOURCES {
            let key = (source, conv.id.clone());
            let Some(target) = targets.get(&key) else {
                continue;
            };
            seen.insert(key);
            stored = true;
            plan.checked += 1;
            let to_vault = conv.do_not_remember && target.scope != "private";
            if to_vault {
                plan.private.push((source.to_string(), conv.id.clone()));
            }
            let flagged = if to_vault {
                "; flagged \"do not remember\", it ends private"
            } else {
                ""
            };
            let corrected = if source == "openai" {
                conv.export.clone()
            } else {
                conv.single_file.clone()
            };
            let Some(draft) = corrected else {
                if to_vault {
                    plan.note(
                        &target.current_id,
                        "private",
                        "flagged \"do not remember\"; no turns on the branch it ended on, so its text stays — it goes into the vault",
                    );
                } else {
                    plan.note(
                        &target.current_id,
                        "no_turns",
                        "no turns on the branch it ended on; left as it is",
                    );
                }
                continue;
            };
            let Some(body) = target.body(&state.home) else {
                plan.note(
                    &target.current_id,
                    "unreadable",
                    format!("its stored text can't be read{flagged}"),
                );
                continue;
            };
            if draft.title == target.title && draft.body == body {
                if to_vault {
                    plan.note(
                        &target.current_id,
                        "private",
                        "reads the same; flagged \"do not remember\", it goes into the vault",
                    );
                } else {
                    plan.count("fine");
                }
                continue;
            }
            // Only the defect is corrected — turns of abandoned branches
            // mixed in — and only from an export that holds every turn the
            // stored text shows: an older export, one whose images are
            // missing or named otherwise, or a renamed title must not
            // replace a transcript stored from a later one.
            let every_turn = if source == "openai" {
                &conv.every_turn_export
            } else {
                &conv.every_turn_single_file
            };
            let why = match transcript_verdict(&body, &draft.body, every_turn) {
                TranscriptVerdict::Replace(why) => why,
                TranscriptVerdict::Holds(n) => {
                    plan.note(
                        &target.current_id,
                        "other_export",
                        format!(
                            "holds {n} turn{} this export doesn't (a later export's, or images named otherwise); left as it is{flagged}",
                            if n == 1 { "" } else { "s" }
                        ),
                    );
                    continue;
                }
                TranscriptVerdict::SameTurns => {
                    plan.note(
                        &target.current_id,
                        "other_export",
                        format!(
                            "its turns are the branch it ended on; only its title or header reads otherwise (another export's); left as it is{flagged}"
                        ),
                    );
                    continue;
                }
                TranscriptVerdict::Fewer(n) => {
                    plan.note(
                        &target.current_id,
                        "other_export",
                        format!(
                            "every turn it holds is on the branch it ended on, and this export holds {n} more (a later export's); left as it is{flagged}"
                        ),
                    );
                    continue;
                }
            };
            let why = if draft.title != target.title {
                format!("{why}; title differs")
            } else {
                why
            };
            let fixed = replacement_ref(&target.base_ref, &draft.title, &draft.body);
            if target.replacements.iter().any(|(r, _)| *r == fixed) {
                plan.note(
                    &target.current_id,
                    "undone",
                    format!("this correction was made and undone; left as it is{flagged}"),
                );
                continue;
            }
            if source == "openai" && !conv.images.is_empty() {
                plan.images.push(ImageCopy {
                    conversation: conv.id.clone(),
                    pointers: conv.images.clone(),
                });
            }
            plan.replace_as(state, target, draft, &format!("{why}{flagged}"), to_vault);
        }
        if !stored {
            plan.count("not_stored");
        }
    }
    let mut missing: Vec<&RepairTarget> = targets
        .iter()
        .filter(|(key, _)| !seen.contains(*key))
        .map(|(_, t)| t)
        .collect();
    missing.sort_by(|a, b| a.current_id.cmp(&b.current_id));
    for target in missing {
        plan.checked += 1;
        plan.note(
            &target.current_id,
            "not_in_export",
            "not in this export; left as it is",
        );
    }
    plan.export_dir = Some(dir);
    plan._unpacked = unpacked;
    Ok(plan)
}

/* ————— health ————— */

/// Where the export's `export.xml` is: the file itself, a folder holding
/// it (or `apple_health_export/export.xml`), or an `export.zip` (read in
/// place, never unpacked).
enum HealthExport {
    Xml(PathBuf),
    Zip(PathBuf, String),
}

fn health_export(path: &Path) -> Result<HealthExport> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.is_dir() {
        let holds = |d: &Path| d.join("export.xml").is_file();
        let dir = find_folder(path, &holds)
            .ok_or_else(|| anyhow!("no export.xml in {}", path.display()))?;
        return Ok(HealthExport::Xml(dir.join("export.xml")));
    }
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
    {
        return Ok(HealthExport::Xml(path.to_path_buf()));
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let archive = zip::ZipArchive::new(file).context("reading the zip")?;
    let mut names: Vec<String> = archive
        .file_names()
        .filter_map(|n| n.ok().map(|n| n.into_owned()))
        .filter(|n| !n.starts_with("__MACOSX") && (n == "export.xml" || n.ends_with("/export.xml")))
        .collect();
    // the shallowest one: `apple_health_export/export.xml`
    names.sort_by_key(|n| (n.matches('/').count(), n.clone()));
    let name = names
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no export.xml in {}", path.display()))?;
    Ok(HealthExport::Zip(path.to_path_buf(), name))
}

/// What the current reader makes of an Apple Health export.
struct HealthReading {
    /// Its day summaries and workouts.
    drafts: Vec<EpisodeDraft>,
    /// The day it was made on.
    export_day: Option<String>,
    /// The sleep stages it holds for each day summary (`day:<date>` →
    /// `sleep/deep`, `sleep/rem`), from any tracker: a summary shows one
    /// tracker's night, so a stage it leaves out can be another's.
    sleep_stages: HashMap<String, HashSet<String>>,
}

/// Read an Apple Health export with the current reader.
fn read_health(path: &Path) -> Result<HealthReading> {
    use hearthloom_adapters::apple_health as ah;
    let mut table = ah::DayTable::default();
    let mut workouts: Vec<ah::WorkoutRec> = Vec::new();
    let sink = |item: ah::Item| {
        match item {
            ah::Item::Sample(s) => table.add(&s),
            ah::Item::Workout(w) => workouts.push(w),
        }
        Ok(())
    };
    let summary = match health_export(path)? {
        HealthExport::Xml(xml) => {
            let file =
                std::fs::File::open(&xml).with_context(|| format!("opening {}", xml.display()))?;
            ah::scan_reader(std::io::BufReader::with_capacity(1 << 20, file), sink)?
        }
        HealthExport::Zip(zip_path, name) => {
            let file = std::fs::File::open(&zip_path)?;
            let mut archive = zip::ZipArchive::new(file).context("reading the zip")?;
            let entry = archive.by_name(&name).context("reading the zip")?;
            ah::scan_reader(std::io::BufReader::with_capacity(1 << 20, entry), sink)?
        }
    };
    let mut sleep_stages: HashMap<String, HashSet<String>> = HashMap::new();
    for (day, metric, _) in table.map.keys() {
        let measure = match metric.as_str() {
            "Sleep/AsleepDeep" => "sleep/deep",
            "Sleep/AsleepREM" => "sleep/rem",
            _ => continue,
        };
        sleep_stages
            .entry(format!("day:{day}"))
            .or_default()
            .insert(measure.to_string());
    }
    let mut drafts = table.into_episodes(None);
    drafts.extend(workouts.iter().map(|w| ah::workout_to_episode(w, None)));
    let export_day = summary
        .export_date
        .as_deref()
        .and_then(|d| d.get(..10))
        .filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok())
        .map(String::from);
    Ok(HealthReading {
        drafts,
        export_day,
        sleep_stages,
    })
}

/// A workout ref the export reader writes (`workout:<its start>`), not the
/// phone's (`workout:<uuid>`).
fn export_workout_day(base_ref: &str) -> Option<&str> {
    let start = base_ref.strip_prefix("workout:")?;
    let day = start.get(..10)?;
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    (start.len() > 10 && start.as_bytes()[10] == b' ').then_some(day)
}

/// Which lines of a day summary or workout read otherwise now, by their
/// labels (`activity`, `sleep`, `totals`…) — never their values.
fn differing_lines(old_title: &str, old: &str, new_title: &str, new: &str) -> String {
    let parts = |body: &str| -> BTreeMap<String, Vec<String>> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let label = line
                .split_once(':')
                .map(|(k, _)| k.trim())
                .filter(|k| !k.is_empty() && k.len() <= 20 && !k.contains(' '))
                .unwrap_or("text");
            map.entry(label.to_string())
                .or_default()
                .push(line.to_string());
        }
        map
    };
    let (a, b) = (parts(old), parts(new));
    let mut labels: Vec<&String> = a.keys().chain(b.keys()).collect();
    labels.sort();
    labels.dedup();
    let mut differ: Vec<&str> = labels
        .into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(String::as_str)
        .collect();
    if old_title != new_title {
        differ.insert(0, "title");
    }
    if differ.is_empty() {
        "text differs".into()
    } else {
        format!("{} differ", differ.join(", "))
    }
}

/// The units a day summary writes an amount in (a weight, a distance): a
/// measure written with one of them alone is that line's amount, whatever
/// the unit — the unit is what a repair may correct.
const AMOUNT_UNITS: &[&str] = &[
    "kg", "lb", "lbs", "g", "oz", "st", "km", "mi", "m", "ft", "yd", "cm",
];

/// The measures a day summary shows, each as `<line>/<measure>`: the
/// measures of its `activity`, `heart`, `sleep` and `body` lines one by one
/// (`activity/steps`, `heart/hrv`, `sleep/deep`, `body/body fat`, a weight
/// or distance as `…/amount`), any other line as a whole (`glucose`). Values,
/// units of an amount and trackers are left out.
fn day_measures(body: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in body.lines() {
        let Some((label, rest)) = line.split_once(": ") else {
            continue;
        };
        let label = label.trim();
        if label.is_empty() || label.len() > 20 || label.contains(' ') || label == "sources" {
            continue;
        }
        let parts: Vec<&str> = match label {
            "activity" | "heart" | "body" => rest.split(" · ").collect(),
            "sleep" => rest.split(", ").collect(),
            _ => {
                out.insert(label.to_string());
                continue;
            }
        };
        for part in parts {
            // the tracker it was read from: `[apple watch]`
            let part = match part.rfind(" [") {
                Some(at) if part.ends_with(']') => &part[..at],
                _ => part,
            };
            let measure = if label == "heart" {
                // `rhr 55`, `hr 50–120 avg 70`, `spo2 97%`: its name first
                part.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            } else {
                let words: Vec<&str> = part
                    .split_whitespace()
                    .filter(|w| !w.bytes().any(|b| b.is_ascii_digit()))
                    .collect();
                match words.as_slice() {
                    [] => "amount".to_string(),
                    [unit] if AMOUNT_UNITS.contains(&unit.to_ascii_lowercase().as_str()) => {
                        "amount".to_string()
                    }
                    _ => words.join(" "),
                }
            };
            out.insert(format!("{label}/{measure}"));
        }
    }
    out
}

/// Does a day summary read from an export keep what the stored one shows:
/// each measure it shows (`activity/steps`, `body/body fat`, `heart/hrv`…,
/// [`day_measures`]) and each tracker on its `sources:` line? An export
/// made before the stored day was read can lack a measure or a tracker that
/// synced later; the values a reading differs in (units, sleep) are what
/// the repair corrects, so they aren't compared. A sleep stage the
/// corrected summary leaves out counts as kept when the export holds it
/// for the day (`sleep_stages`): it is another tracker's, and the night
/// shown is one tracker's.
fn day_keeps(
    stored: &str,
    corrected: &EpisodeDraft,
    sleep_stages: Option<&HashSet<String>>,
) -> bool {
    let mut measures = day_measures(&corrected.body);
    measures.extend(sleep_stages.into_iter().flatten().cloned());
    if !day_measures(stored).is_subset(&measures) {
        return false;
    }
    let trackers: Vec<String> = corrected
        .entities
        .iter()
        .filter(|e| e.entity_type == "Device")
        .map(|e| e.name.to_lowercase())
        .collect();
    stored
        .lines()
        .find_map(|l| l.strip_prefix("sources: "))
        .is_none_or(|named| joined_from(named, &trackers))
}

/// The Apple Health repair's plan (blocking). `path`: `export.zip`, its
/// `export.xml`, or the folder holding it.
pub fn plan_health(state: &AppState, path: &Path) -> Result<Plan> {
    let mut plan = Plan::new("health");
    let HealthReading {
        drafts,
        export_day,
        sleep_stages,
    } = read_health(path)?;
    let mut corrected: HashMap<String, EpisodeDraft> = drafts
        .into_iter()
        .map(|d| (d.source_ref.clone(), d))
        .collect();
    if export_day.is_none() {
        plan.notes.push(
            "the export doesn't say when it was made, so its last day can't be told from a whole one"
                .into(),
        );
    }
    for target in state.store.repair_targets("apple-health")? {
        let day = if let Some(day) = target.base_ref.strip_prefix("day:") {
            day
        } else if target.base_ref.starts_with("workout:") {
            match export_workout_day(&target.base_ref) {
                Some(day) => day,
                None => {
                    // the phone's, keyed by its own id; not in an export
                    plan.count("from_phone");
                    continue;
                }
            }
        } else {
            continue;
        };
        plan.checked += 1;
        let what = if target.base_ref.starts_with("day:") {
            "day"
        } else {
            "workout"
        };
        let Some(draft) = corrected.remove(&target.base_ref) else {
            plan.note(
                &target.current_id,
                "not_in_export",
                format!("the export doesn't cover this {what}; left as it is"),
            );
            continue;
        };
        if export_day.as_deref().is_some_and(|e| day >= e) {
            plan.note(
                &target.current_id,
                "partial",
                "the export was made that day, so it holds only part of it; left as it is",
            );
            continue;
        }
        let Some(body) = target.body(&state.home) else {
            plan.note(
                &target.current_id,
                "unreadable",
                "its stored text can't be read",
            );
            continue;
        };
        if draft.title == target.title && draft.body == body {
            plan.count("fine");
            continue;
        }
        // a day read from an export that lacks a tracker or a measure the
        // stored day shows (an older export's reading) is left: replacing
        // it would lose them
        if what == "day" && !day_keeps(&body, &draft, sleep_stages.get(&target.base_ref)) {
            plan.note(
                &target.current_id,
                "other_export",
                "it shows trackers or measures this export doesn't hold for the day (a later export's?); left as it is",
            );
            continue;
        }
        let fixed = replacement_ref(&target.base_ref, &draft.title, &draft.body);
        if target.replacements.iter().any(|(r, _)| *r == fixed) {
            plan.note(
                &target.current_id,
                "undone",
                "this correction was made and undone; left as it is",
            );
            continue;
        }
        let why = format!(
            "{what}: {}",
            differing_lines(&target.title, &body, &draft.title, &draft.body)
        );
        plan.replace(state, &target, draft, &why);
    }
    // what the export holds that isn't stored is for an import, not a repair
    let not_stored = corrected
        .keys()
        .filter(|r| r.starts_with("day:") || r.starts_with("workout:"))
        .count();
    if not_stored > 0 {
        *plan.counts.entry("not_stored").or_default() += not_stored;
    }
    Ok(plan)
}

/* ————— copies ————— */

/// The copies repair's plan (blocking): this node's days and sessions
/// stored under an unversioned base, re-minted from their stored text
/// (core `Store::repair_copies_scan`).
pub fn plan_copies(state: &AppState) -> Result<Plan> {
    let mut plan = Plan::new(hearthloom_core::store::COPIES_KIND);
    for finding in state.store.repair_copies_scan(&state.home)? {
        plan.checked += 1;
        match finding.draft {
            Some(draft) => {
                if !finding.private
                    && !finding.retired
                    && crate::consolidate::extraction_tier(&state.store, &finding.source).is_some()
                {
                    plan.extraction_again += 1;
                }
                plan.note(&finding.episode, "remint", finding.reason);
                plan.revisions.push(draft);
            }
            // held by a fuller copy: nothing to say per day
            None if finding.status == "held" => plan.count("held"),
            None => plan.note(&finding.episode, finding.status, finding.reason),
        }
    }
    Ok(plan)
}

/* ————— photo days ————— */

/// Is `named` (a day's `places:` line) a `, `-joined list of places from
/// `places`? A place's own name can hold `, `, so the line is matched by
/// whole names, not split.
fn joined_from(named: &str, places: &[String]) -> bool {
    let mut reach = vec![false; named.len() + 1];
    reach[0] = true;
    for at in 0..named.len() {
        if !reach[at] {
            continue;
        }
        for place in places.iter().filter(|p| !p.is_empty()) {
            if !named[at..].starts_with(place.as_str()) {
                continue;
            }
            let end = at + place.len();
            if end == named.len() {
                return true;
            }
            if named[end..].starts_with(", ") {
                reach[end + 2] = true;
            }
        }
    }
    named.is_empty()
}

/// The photo-days repair's plan (blocking).
pub fn plan_photo_days(state: &AppState) -> Result<Plan> {
    use hearthloom_adapters::photos as photos_adapter;
    let mut plan = Plan::new(hearthloom_core::store::PHOTO_DAYS_KIND);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    for (i, day) in state
        .store
        .photo_days_for_repair(&state.home)?
        .into_iter()
        .enumerate()
    {
        if i > 0 && i % 100 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        if day.day >= today {
            // a day still open is minted once it closes
            plan.count("open");
            continue;
        }
        plan.checked += 1;
        let Some(body) = day.body.as_deref() else {
            plan.note(&day.id, "unreadable", "its stored text can't be read");
            continue;
        };
        let points = state.store.photo_points_for_day(&day.day)?;
        if points.is_empty() {
            plan.note(
                &day.id,
                "no_points",
                "none of its photos is stored on this node; left as it is",
            );
            continue;
        }
        let Some(shown) = hearthloom_core::store::photo_day_lines(body) else {
            plan.note(
                &day.id,
                "cut",
                "its list of photos is cut short, so what it holds can't be checked; left as it is",
            );
            continue;
        };
        // each line one of the photos stored here, by a name it has had (its
        // own, or one of a key it took the place of) — each photo answering
        // for one line per name, so one photo never stands for two photos
        // shown under the same name
        let held = state.store.photo_day_held(&day.day)?;
        let missing = hearthloom_core::store::photos_missing(&held, &shown);
        if missing > 0 {
            plan.note(
                &day.id,
                "not_covered",
                format!(
                    "it shows {missing} photo{} this node doesn't hold; left as it is",
                    if missing == 1 { "" } else { "s" }
                ),
            );
            continue;
        }
        let mut keys: Vec<String> = points.iter().map(|p| p.key.clone()).collect();
        let keys_hash = hearthloom_core::store::photo_keys_hash(&mut keys);
        let count = points.len() as i64;
        let ledger_revision = day.ledger.as_ref().map_or(0, |l| l.3);
        let adapter_points: Vec<photos_adapter::PhotoPoint> = points
            .iter()
            .map(crate::live_photos::stored_to_adapter)
            .collect();
        let next = day.max_revision.max(ledger_revision) + 1;
        let mut draft = photos_adapter::day_draft(&day.day, &adapter_points, None, next as u32);
        if draft.title == day.title && draft.body == body {
            let settled = day.ledger.as_ref().is_some_and(|(c, h, _, r)| {
                *c == count && *h == keys_hash && *r >= day.max_revision
            });
            if settled {
                plan.count("fine");
            } else {
                plan.note(
                    &day.id,
                    "ledger",
                    "reads right; the count it was minted with here is put right",
                );
                plan.photo_ledgers.push(PhotoLedger {
                    day: day.day.clone(),
                    count,
                    keys_hash,
                    revision: day.max_revision.max(ledger_revision),
                });
            }
            continue;
        }
        // Only an inflated count is corrected: the day shows a photo more
        // than once (every line is one of the photos here, and there are
        // more lines than photos), or the count it was minted with here is
        // higher than the photos stored, which keeps the finalize pass from
        // adding the ones it lacks. A day that reads otherwise for another
        // reason (a place or a position read since, another order) is left.
        let minted = day.ledger.as_ref().map(|l| l.0);
        let mut why = if shown.len() > points.len() {
            format!("shows {} photos, {} once each", shown.len(), points.len())
        } else if shown.len() < points.len() && minted.is_some_and(|c| c > count) {
            format!(
                "shows {} photos of the {} stored here; the count it was minted with here keeps the rest out",
                shown.len(),
                points.len()
            )
        } else {
            plan.count("reads_otherwise");
            continue;
        };
        let draft_places: Vec<String> = {
            let mut places: Vec<String> = Vec::new();
            for p in &points {
                let place = p
                    .place
                    .as_deref()
                    .map(hearthloom_core::one_line)
                    .filter(|p| !p.is_empty());
                if let Some(place) = place.filter(|p| !places.contains(p)) {
                    places.push(place);
                }
            }
            places
        };
        let named = body.lines().find_map(|l| l.strip_prefix("places: "));
        if named.is_some_and(|named| !joined_from(named, &draft_places)) {
            plan.note(
                &day.id,
                "places",
                "its photos here don't name every place it does; left as it is",
            );
            continue;
        }
        let hash = hearthloom_core::ids::content_hash(draft.body.as_bytes());
        if day
            .undone
            .iter()
            .any(|(title, h)| *title == draft.title && *h == hash)
        {
            plan.note(
                &day.id,
                "undone",
                "this correction was made and undone; left as it is",
            );
            continue;
        }
        if day.retired {
            why.push_str("; retired, the new revision lands retired");
        }
        // the day's label goes with it (the finalize pass mints unlabelled)
        draft.scope = Some(day.scope.clone());
        plan.note(&day.id, "remint", why);
        plan.revisions.push(draft);
        plan.photo_ledgers.push(PhotoLedger {
            day: day.day.clone(),
            count,
            keys_hash,
            revision: next,
        });
    }
    Ok(plan)
}

/* ————— runs made elsewhere ————— */

/// Settle here every run this node hasn't (made or undone on another node,
/// or interrupted). Waits while a repair runs here. Returns how many.
pub async fn catch_up(state: &AppState) -> usize {
    let store = state.store.clone();
    let home = state.home.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<usize> {
        // a run being applied here is settled by its own landing: wait
        // for it (the next cycle catches up)
        let Some(_one) = try_start(&home) else {
            return Ok(0);
        };
        // each run on its own: one whose settling keeps failing is tried
        // again next cycle, and never holds up the runs after it
        let mut settled = 0;
        for run in store.repairs_pending_here()? {
            match store.repair_settle_run(&home, &run) {
                Ok(_) => settled += 1,
                Err(err) => tracing::warn!("repair {run}: settling here failed: {err:#}"),
            }
        }
        Ok(settled)
    })
    .await;
    match result {
        Ok(Ok(0)) => 0,
        Ok(Ok(n)) => {
            tracing::info!("repairs: caught up with {n}");
            crate::bus::touch(state, "operational.changed");
            n
        }
        Ok(Err(err)) => {
            tracing::warn!("repair catch-up failed: {err:#}");
            0
        }
        Err(err) => {
            tracing::warn!("repair catch-up panicked: {err}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::store::Store;
    use hearthloom_core::{EntityRef, EventTime, IngestBatch, TimeUncertainty};

    fn state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(Store::open(&home.db_path()).unwrap()),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        (state, tmp)
    }

    fn land(state: &AppState, actor: &str, drafts: Vec<EpisodeDraft>) {
        state
            .store
            .apply_batch(
                &state.home,
                &IngestBatch {
                    actor: actor.into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap();
    }

    fn live_of(state: &AppState, source: &str) -> Vec<(String, String)> {
        let (_, rows) = state
            .store
            .read_query(
                &format!(
                    "SELECT source_ref, content_ref FROM episodes
                     WHERE source = '{source}' AND superseded_by IS NULL ORDER BY source_ref"
                ),
                1000,
            )
            .unwrap();
        rows.into_iter()
            .map(|r| {
                (
                    r[0].as_str().unwrap().to_string(),
                    r[1].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn text_of(state: &AppState, content_ref: &str) -> String {
        std::fs::read_to_string(state.home.root().join(content_ref)).unwrap()
    }

    /// A memory as an earlier mail import stored it: the subject and the
    /// sender's name left encoded, the body still base64.
    fn stored_encoded(source_ref: &str) -> EpisodeDraft {
        let subject = "=?UTF-8?Q?Caf=C3=A9_plans?=";
        EpisodeDraft {
            source: "mbox".into(),
            source_ref: source_ref.into(),
            kind: "email".into(),
            title: subject.into(),
            body: format!(
                "subject: {subject}\nfrom: jose <jose@x.example>\nto: bob <bob@y.example>\n\
                 labels: Sent\n\nTWVldCBhdCB0aGUgY2Fmw6kgb24gVHVlc2RheS4K"
            ),
            event_time: EventTime::parse_rfc3339(
                "2026-01-05T10:00:00-04:00",
                TimeUncertainty::Exact,
            )
            .unwrap(),
            trust: "imported".into(),
            scope: None,
            entities: vec![EntityRef {
                entity_type: "Person".into(),
                name: "jose".into(),
                attributes: vec![("email".into(), "jose@x.example".into())],
                role: "author".into(),
            }],
            edges: vec![],
        }
    }

    const ARCHIVED: &str = "From jose@x.example Mon Jan  5 10:00:00 2026\n\
X-Gmail-Labels: Sent,Inbox\n\
From: =?UTF-8?B?Sm9zw6kgR2FyY8OtYQ==?= <jose@x.example>\n\
To: bob@y.example\n\
Subject: =?UTF-8?Q?Caf=C3=A9_plans?=\n\
Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
Message-ID: <dec1@x>\n\
Content-Type: text/plain; charset=utf-8\n\
Content-Transfer-Encoding: base64\n\
\n\
TWVldCBhdCB0aGUgY2Fmw6kgb24gVHVlc2RheS4K\n\
\n\
From ann@x.example Mon Jan  5 11:00:00 2026\n\
From: Ann <ann@x.example>\n\
Subject: plain\n\
Date: Mon, 05 Jan 2026 11:00:00 -0400\n\
Message-ID: <plain1@x>\n\
\n\
Nothing encoded here.\n";

    /// The mail repair finds the memory stored encoded, reads its original
    /// from the archive, and lands the decoded copy in its place — keeping
    /// the labels it was made under; one whose original no archive holds is
    /// reported and left; the report names no words; a second run finds
    /// nothing to do.
    #[test]
    fn test_the_mail_repair_decodes_a_memory_from_its_archive() {
        let (state, tmp) = state();
        let archive = tmp.path().join("takeout.mbox");
        std::fs::write(&archive, ARCHIVED).unwrap();
        crate::mbox_index::build(&archive).unwrap();
        state
            .store
            .register_virtual_source(
                &Actor::new("human:test"),
                "gmail-archive",
                "mbox",
                &json!({"mount": archive.display().to_string()}),
                &[],
            )
            .unwrap();
        let mut plain = stored_encoded("plain1@x");
        plain.title = "plain".into();
        plain.body = "subject: plain\nfrom: Ann <ann@x.example>\n\nNothing encoded here.".into();
        land(
            &state,
            "adapter:mbox",
            vec![stored_encoded("dec1@x"), stored_encoded("gone@x"), plain],
        );

        let plan = plan_mail(&state).unwrap();
        assert_eq!(plan.checked, 3);
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].base_ref, "dec1@x");
        let report = plan.report(None);
        assert_eq!(report["counts"]["replace"], 1);
        assert_eq!(report["counts"]["not_in_archive"], 1);
        assert_eq!(report["counts"]["fine"], 1);
        assert_eq!(report["extraction_again"], 0, "mail extraction is off");
        let finding = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["status"] == "replace")
            .unwrap()
            .clone();
        assert_eq!(
            finding["episode"],
            hearthloom_core::ids::episode_id("mbox", "dec1@x")
        );
        let why = finding["reason"].as_str().unwrap();
        assert!(
            why.contains("encoded subject or name") && why.contains("base64"),
            "{why}"
        );
        let shown = report.to_string();
        for words in ["Café", "Caf=C3", "Meet", "TWVl", "José", "jose"] {
            assert!(!shown.contains(words), "the report shows {words:?}");
        }

        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!(applied.replaced, 1);
        let live = live_of(&state, "mbox");
        let fixed = live
            .iter()
            .find(|(r, _)| r.starts_with("dec1@x#repair:"))
            .expect("the corrected copy is in view");
        assert!(!live.iter().any(|(r, _)| r == "dec1@x"));
        let text = text_of(&state, &fixed.1);
        assert!(text.starts_with("subject: Café plans\n"), "{text}");
        assert!(
            text.contains("from: José García <jose@x.example>"),
            "{text}"
        );
        assert!(text.contains("\nlabels: Sent\n"), "{text}");
        assert!(text.contains("Meet at the café on Tuesday."), "{text}");

        // nothing left to do, and doing it again changes nothing
        let again = plan_mail(&state).unwrap();
        assert!(again.items.is_empty());
        assert!(apply(&state, "human:owner", &again).unwrap().run.is_none());
    }

    /// A privacy rule naming a domain only the part of a link the corrected
    /// copy leaves out names (the link is longer than the room for link
    /// targets; the copy keeps its host) still decides: the record goes
    /// into the vault before the run, and the corrected copy lands private.
    #[test]
    fn test_a_rule_matching_a_link_the_corrected_copy_leaves_out_ends_it_private() {
        let (state, tmp) = state();
        let archive = tmp.path().join("takeout.mbox");
        std::fs::write(
            &archive,
            format!(
                "From jose@x.example Mon Jan  5 10:00:00 2026\n\
                 X-Gmail-Labels: Sent\n\
                 From: Someone <jose@x.example>\n\
                 To: bob@y.example\n\
                 Subject: =?UTF-8?Q?Caf=C3=A9_plans?=\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
                 Message-ID: <lnk1@x>\n\
                 Content-Type: text/html; charset=utf-8\n\
                 \n\
                 <p>Your results are ready: \
                 <a href=\"https://go.example/r?to=portal.clinic-one.example&d={}\">view them</a></p>\n",
                "a".repeat(70_000)
            ),
        )
        .unwrap();
        crate::mbox_index::build(&archive).unwrap();
        state
            .store
            .register_virtual_source(
                &Actor::new("human:test"),
                "gmail-archive",
                "mbox",
                &json!({"mount": archive.display().to_string()}),
                &[],
            )
            .unwrap();
        land(&state, "adapter:mbox", vec![stored_encoded("lnk1@x")]);
        state
            .store
            .add_privacy_rule(&Actor::new("human:test"), "domain", "clinic-one.example")
            .unwrap();
        let refs = |more: &[String]| {
            let mut refs = vec!["lnk1@x".to_string()];
            refs.extend_from_slice(more);
            state.store.private_source_refs("mbox", &refs).unwrap()
        };
        assert!(refs(&[]).is_empty());

        let plan = plan_mail(&state).unwrap();
        assert_eq!(plan.items.len(), 1);
        let draft = &plan.items[0].draft;
        assert!(!draft.body.contains("clinic-one"), "left out");
        assert_eq!(plan.private, [("mbox".to_string(), "lnk1@x".to_string())]);
        let why = plan.findings[0].reason.as_str();
        assert!(why.ends_with("it ends private"), "{why}");

        let fixed = replacement_ref("lnk1@x", &draft.title, &draft.body);
        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!((applied.replaced, applied.made_private), (1, 1));
        assert_eq!(
            refs(std::slice::from_ref(&fixed)),
            HashSet::from(["lnk1@x".to_string(), fixed])
        );
    }

    /// An original whose links name more hosts than their room in the
    /// stored text holds, which the mail reader drafts private, ends
    /// private with no privacy rule: the record goes into the vault before
    /// the run, and the plan says why without quoting the message.
    #[test]
    fn test_an_original_the_reader_drafts_private_ends_private() {
        let (state, tmp) = state();
        let prefix = "https://track.example/p?d=";
        let filler = format!("{prefix}{}", "a".repeat(65_535 - prefix.len()));
        let mut html = format!("<p><img src=\"{filler}\">Your results are ready:");
        for i in 0..3_000 {
            html.push_str(&format!(" <a href=\"https://h{i:04}.example/x\">h</a>"));
        }
        html.push_str("</p>");
        let archive = tmp.path().join("takeout.mbox");
        std::fs::write(
            &archive,
            format!(
                "From jose@x.example Mon Jan  5 10:00:00 2026\n\
                 X-Gmail-Labels: Sent\n\
                 From: Someone <jose@x.example>\n\
                 To: bob@y.example\n\
                 Subject: =?UTF-8?Q?Caf=C3=A9_plans?=\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
                 Message-ID: <hosts1@x>\n\
                 Content-Type: text/html; charset=utf-8\n\
                 \n\
                 {html}\n"
            ),
        )
        .unwrap();
        crate::mbox_index::build(&archive).unwrap();
        state
            .store
            .register_virtual_source(
                &Actor::new("human:test"),
                "gmail-archive",
                "mbox",
                &json!({"mount": archive.display().to_string()}),
                &[],
            )
            .unwrap();
        land(&state, "adapter:mbox", vec![stored_encoded("hosts1@x")]);
        assert!(state.store.list_privacy_rules().unwrap().is_empty());
        let refs = |more: &[String]| {
            let mut refs = vec!["hosts1@x".to_string()];
            refs.extend_from_slice(more);
            state.store.private_source_refs("mbox", &refs).unwrap()
        };
        assert!(refs(&[]).is_empty());

        let plan = plan_mail(&state).unwrap();
        assert_eq!(plan.items.len(), 1);
        let draft = &plan.items[0].draft;
        assert_eq!(draft.scope.as_deref(), Some("private"));
        assert_eq!(plan.private, [("mbox".to_string(), "hosts1@x".to_string())]);
        let report = plan.report(None);
        assert_eq!(report["private"], 1);
        let why = plan.findings[0].reason.as_str();
        assert!(why.ends_with("it ends private"), "{why}");
        for words in ["results", "track.example", "h0001", "Café"] {
            assert!(!why.contains(words), "the note shows {words:?}: {why}");
        }

        let fixed = replacement_ref("hosts1@x", &draft.title, &draft.body);
        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!((applied.replaced, applied.made_private), (1, 1));
        assert_eq!(
            refs(std::slice::from_ref(&fixed)),
            HashSet::from(["hosts1@x".to_string(), fixed])
        );
    }

    /// One archived message from `from` under `id`, dated as the stored
    /// memories are.
    fn archived(id: &str, labels: &str, from: &str, body: &str) -> String {
        let encoding = if body.starts_with("TWVl") {
            "Content-Transfer-Encoding: base64\n"
        } else {
            ""
        };
        format!(
            "From {from} Mon Jan  5 10:00:00 2026\n\
X-Gmail-Labels: {labels}\n\
From: Someone <{from}>\n\
To: bob@y.example\n\
Subject: =?UTF-8?Q?Caf=C3=A9_plans?=\n\
Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
Message-ID: <{id}>\n\
Content-Type: text/plain; charset=utf-8\n\
{encoding}\n\
{body}\n\n"
        )
    }

    /// A Message-ID can name several archived messages: the original of a
    /// memory is the copy outside Spam and Trash with its date and sender,
    /// wherever the index puts it; copies that pass and read differently
    /// leave the memory as it is; an archive that can't be read leaves every
    /// memory unrepaired, reported as such.
    #[test]
    fn test_the_mail_repair_reads_every_copy_under_a_message_id() {
        let (state, tmp) = state();
        const REAL: &str = "TWVldCBhdCB0aGUgY2Fmw6kgb24gVHVlc2RheS4K";
        let takeout = tmp.path().join("takeout.mbox");
        std::fs::write(
            &takeout,
            [
                // a Spam copy first, then the original
                archived("dup1@x", "Spam", "jose@x.example", "Claim your prize."),
                archived("dup1@x", "Sent", "jose@x.example", REAL),
                // another sender's message first, then the original
                archived("dup2@x", "Inbox", "mallory@z.example", "Wire the money."),
                archived("dup2@x", "Sent", "jose@x.example", REAL),
                archived("amb1@x", "Sent", "jose@x.example", REAL),
            ]
            .concat(),
        )
        .unwrap();
        let live = tmp.path().join("live.mbox");
        std::fs::write(
            &live,
            archived("amb1@x", "Inbox", "jose@x.example", "Wire the money."),
        )
        .unwrap();
        for (name, path) in [("gmail-archive", &takeout), ("gmail-later", &live)] {
            crate::mbox_index::build(path).unwrap();
            state
                .store
                .register_virtual_source(
                    &Actor::new("human:test"),
                    name,
                    "mbox",
                    &json!({"mount": path.display().to_string()}),
                    &[],
                )
                .unwrap();
        }
        land(
            &state,
            "adapter:mbox",
            vec![
                stored_encoded("dup1@x"),
                stored_encoded("dup2@x"),
                stored_encoded("amb1@x"),
            ],
        );
        let id = |r: &str| hearthloom_core::ids::episode_id("mbox", r);

        let plan = plan_mail(&state).unwrap();
        let report = plan.report(None);
        let status = |r: &str| -> Vec<String> {
            findings_for(&report, &id(r))
                .into_iter()
                .map(|f| f.split(':').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(status("dup1@x"), ["replace"], "{report}");
        assert_eq!(status("dup2@x"), ["replace"], "{report}");
        assert_eq!(status("amb1@x"), ["ambiguous"], "{report}");
        for item in &plan.items {
            assert!(
                item.draft.body.contains("Meet at the café on Tuesday.")
                    && item.draft.body.contains("<jose@x.example>"),
                "{}: {}",
                item.base_ref,
                item.draft.body
            );
        }
        assert_eq!(plan.items.len(), 2);

        // an index that can't be read: nothing is repaired, and it says so
        let index = crate::mbox_index::index_path(&live);
        rusqlite::Connection::open(&index)
            .unwrap()
            .execute_batch("DROP TABLE messages")
            .unwrap();
        let plan = plan_mail(&state).unwrap();
        assert!(plan.items.is_empty());
        let report = plan.report(None);
        assert_eq!(report["counts"]["archive_error"], 3, "{report}");
        assert!(report["counts"].get("not_in_archive").is_none(), "{report}");
    }

    /// A memory whose subject reads fine but whose short body was stored
    /// still encoded — base64 under a line long, quoted-printable with two
    /// escapes — is read from its archive and replaced like any other.
    #[test]
    fn test_a_short_encoded_body_is_repaired() {
        let (state, tmp) = state();
        let message = |id: &str, encoding: &str, body: &str| {
            format!(
                "From jose@x.example Mon Jan  5 10:00:00 2026\n\
X-Gmail-Labels: Sent\n\
From: Jose <jose@x.example>\n\
To: bob@y.example\n\
Subject: Lunch\n\
Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
Message-ID: <{id}>\n\
Content-Type: text/plain; charset=utf-8\n\
Content-Transfer-Encoding: {encoding}\n\
\n\
{body}\n\n"
            )
        };
        let archive = tmp.path().join("takeout.mbox");
        std::fs::write(
            &archive,
            message("b64@x", "base64", "SGVsbG8=")
                + &message("qp@x", "quoted-printable", "Caf=C3=A9"),
        )
        .unwrap();
        crate::mbox_index::build(&archive).unwrap();
        state
            .store
            .register_virtual_source(
                &Actor::new("human:test"),
                "gmail-archive",
                "mbox",
                &json!({"mount": archive.display().to_string()}),
                &[],
            )
            .unwrap();
        let stored = |id: &str, body: &str| {
            let mut d = stored_encoded(id);
            d.title = "Lunch".into();
            d.body = format!(
                "subject: Lunch\nfrom: Jose <jose@x.example>\nto: bob <bob@y.example>\n\
                 labels: Sent\n\n{body}"
            );
            d
        };
        land(
            &state,
            "adapter:mbox",
            vec![stored("b64@x", "SGVsbG8="), stored("qp@x", "Caf=C3=A9")],
        );
        let plan = plan_mail(&state).unwrap();
        let report = plan.report(None);
        let id = |r: &str| hearthloom_core::ids::episode_id("mbox", r);
        assert_eq!(
            findings_for(&report, &id("b64@x")),
            ["replace: base64 text"],
            "{report}"
        );
        assert_eq!(
            findings_for(&report, &id("qp@x")),
            ["replace: quoted-printable text"],
            "{report}"
        );
        let text = |r: &str| {
            plan.items
                .iter()
                .find(|i| i.base_ref == r)
                .map(|i| i.draft.body.clone())
                .unwrap()
        };
        assert!(text("b64@x").contains("\n\nHello"), "{}", text("b64@x"));
        assert!(text("qp@x").contains("\n\nCafé"), "{}", text("qp@x"));
    }

    /// An archive that can't be read now — its index out of date or its
    /// mount not answering — may hold the original, or another message
    /// under its Message-ID that makes it ambiguous: no message is
    /// repaired from the archives that can be read, and the report says
    /// why.
    #[test]
    fn test_an_archive_that_cannot_be_read_now_holds_every_repair_back() {
        let (state, tmp) = state();
        const REAL: &str = "TWVldCBhdCB0aGUgY2Fmw6kgb24gVHVlc2RheS4K";
        let takeout = tmp.path().join("takeout.mbox");
        std::fs::write(&takeout, archived("dup3@x", "Sent", "jose@x.example", REAL)).unwrap();
        let other = tmp.path().join("other.mbox");
        std::fs::write(
            &other,
            archived("dup3@x", "Inbox", "jose@x.example", "Wire the money."),
        )
        .unwrap();
        let register = |name: &str, path: &Path| {
            state
                .store
                .register_virtual_source(
                    &Actor::new("human:test"),
                    name,
                    "mbox",
                    &json!({"mount": path.display().to_string()}),
                    &[],
                )
                .unwrap();
        };
        for (name, path) in [("gmail-archive", &takeout), ("gmail-other", &other)] {
            crate::mbox_index::build(path).unwrap();
            register(name, path);
        }
        land(&state, "adapter:mbox", vec![stored_encoded("dup3@x")]);
        let id = hearthloom_core::ids::episode_id("mbox", "dup3@x");
        let status = |plan: &Plan| -> Vec<String> {
            findings_for(&plan.report(None), &id)
                .into_iter()
                .map(|f| f.split(':').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(status(&plan_mail(&state).unwrap()), ["ambiguous"]);

        // the original's archive grew after its index was built
        let mut grown = std::fs::read_to_string(&takeout).unwrap();
        grown.push_str(&archived("new1@x", "Inbox", "ann@x.example", "Later."));
        std::fs::write(&takeout, grown).unwrap();
        let plan = plan_mail(&state).unwrap();
        assert!(plan.items.is_empty(), "{:?}", plan.findings);
        assert_eq!(status(&plan), ["archive_error"]);
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("gmail-archive is out of date")),
            "{:?}",
            plan.notes
        );

        // rebuilt; then an archive whose mount doesn't answer
        crate::mbox_index::build(&takeout).unwrap();
        assert_eq!(status(&plan_mail(&state).unwrap()), ["ambiguous"]);
        register("gmail-unplugged", &tmp.path().join("unplugged/all.mbox"));
        let plan = plan_mail(&state).unwrap();
        assert!(plan.items.is_empty());
        assert_eq!(status(&plan), ["archive_error"]);
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("gmail-unplugged doesn't answer")),
            "{:?}",
            plan.notes
        );
    }

    /// The nodes a repair waits for: the others, seen lately, that don't
    /// say they settle owner repairs.
    /// One repair at a time per node, and one node's run never holds
    /// another node's back (two nodes in one process: the tests).
    #[test]
    fn test_one_repair_runs_at_a_time_per_node() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (
            hearthloom_core::Home::at(a.path()),
            hearthloom_core::Home::at(b.path()),
        );
        let first = try_start(&a).unwrap();
        assert!(try_start(&a).is_none(), "a second run at the same node");
        assert!(try_start(&b).is_some(), "another node's run");
        drop(first);
        assert!(try_start(&a).is_some(), "free again once the run ends");
    }

    #[test]
    fn test_older_nodes_are_the_recent_ones_without_the_rule() {
        let node = |id: &str, seen: &str, roles: Value| json!({"node_id": id, "name": id, "version": "0.5.99", "last_seen": seen, "roles": roles});
        let nodes = [
            node("me", "2026-11-10T00:00:00Z", json!({})),
            node("old", "2026-11-10T00:00:00Z", json!({"mail": true})),
            node("new", "2026-11-10T00:00:00Z", json!({"repairs": true})),
            node("gone", "2026-01-01T00:00:00Z", json!({})),
            node("odd", "2026-11-10T00:00:00Z", json!({"repairs": "yes"})),
        ];
        assert_eq!(
            older_nodes_from("me", &nodes, "2026-10-15T00:00:00Z"),
            ["old (0.5.99)", "odd (0.5.99)"]
        );
    }

    #[test]
    fn test_what_reads_as_still_encoded() {
        assert!(still_encoded("Plans", "subject: Plans\n\nSee you at 10.").is_empty());
        assert_eq!(
            still_encoded("=?utf-8?b?SGk=?=", "subject: =?utf-8?b?SGk=?=\n\nhi"),
            ["encoded subject or name"]
        );
        assert_eq!(
            still_encoded(
                "x",
                "subject: x\n\nThis is a long line that was cut by the encod=\ning and continues on =\nthe next line =E2=80=94 ok"
            ),
            ["quoted-printable text"]
        );
        assert_eq!(
            still_encoded(
                "x",
                "subject: x\n\n--b1\nContent-Type: text/plain\n\nhello\n--b1--"
            ),
            ["raw MIME parts"]
        );
        // a word alone, or a link, isn't base64
        assert!(still_encoded("x", "subject: x\n\nSupercalifragilistic").is_empty());
        assert!(still_encoded("x", "subject: x\n\nhttps://example.com/a/b").is_empty());
        // short encoded bodies: base64 under a line, a few escapes
        for (body, why) in [
            ("SGVsbG8=", "base64 text"),
            (
                "TWVldCBhdCB0aGUgY2Fmw6kgb24gVHVlc2RheSBhdCBub29uLCB0aGVuIHRoZSBtdXNldW0u\nCg==",
                "base64 text",
            ),
            ("Caf=C3=A9", "quoted-printable text"),
            (
                "See you at the caf=C3=A9 =E2=80=94 noon",
                "quoted-printable text",
            ),
            ("a=3Db", "quoted-printable text"),
            ("Gr=FC=DFe, bis Montag", "quoted-printable text"),
            // an escaped space, a Latin-1 letter ending a word, Latin-1 base64
            ("Hello=20world", "quoted-printable text"),
            ("Caf=E9", "quoted-printable text"),
            ("See you at the caf=E9.", "quoted-printable text"),
            ("Q2Fm6Q==", "base64 text"),
        ] {
            assert_eq!(
                still_encoded("x", &format!("subject: x\n\n{body}")),
                [why],
                "{body}"
            );
        }
        // ...and text that only looks like it
        for body in [
            "Test",
            "Yeah",
            "McDonald",
            "Thanks!",
            "Fix",
            "https://example.com/?country=CA&lang=DE",
            "x = 3",
            "n=20",
            "n=20 people",
            "Sure",
            "Okay",
        ] {
            assert!(
                still_encoded("x", &format!("subject: x\n\n{body}")).is_empty(),
                "{body}"
            );
        }
        assert_eq!(
            stored_labels("subject: s\nlabels: Sent, Starred\n\nx"),
            ["Sent", "Starred"]
        );
        assert!(stored_labels("subject: s\n\nlabels: Sent").is_empty());
    }

    /// An event an earlier importer stored with its alarm's attendee and
    /// description is replaced; an occurrence nothing stored is imported;
    /// an override an earlier importer stored under its RECURRENCE-ID as
    /// written is found by its UID and start and replaced, never imported a
    /// second time; and a second run finds nothing to do.
    #[test]
    fn test_the_calendar_repair_drops_alarm_text_and_adds_what_is_missing() {
        let (state, _tmp) = state();
        let event = |source_ref: &str, title: &str, at: &str, body: &str| EpisodeDraft {
            source: "ics".into(),
            source_ref: source_ref.into(),
            kind: "calendar_event".into(),
            title: title.into(),
            body: body.into(),
            event_time: EventTime::parse_rfc3339(at, TimeUncertainty::Exact).unwrap(),
            trust: "imported".into(),
            scope: None,
            entities: vec![EntityRef {
                entity_type: "Event".into(),
                name: title.into(),
                attributes: vec![(
                    "ics-uid".into(),
                    source_ref.split('#').next().unwrap().into(),
                )],
                role: "subject".into(),
            }],
            edges: vec![],
        };
        land(
            &state,
            "adapter:ics",
            vec![
                // the alarm's attendee and description taken in
                event(
                    "a-1",
                    "Dentist",
                    "2026-03-15T12:00:00Z",
                    "event: Dentist\nstart: 2026-03-15T12:00:00Z\nattendees:\n  owner <owner@example.com>\n  \
                     Jane Doe <jane@example.com>\n\nThis is an event reminder\n",
                ),
                // an override stored under its RECURRENCE-ID as written
                event(
                    "r-1#20260316T140000Z",
                    "Standup (moved)",
                    "2026-03-16T14:30:00Z",
                    "event: Standup (moved)\nstart: 2026-03-16T14:30:00Z\n\nThis is an event reminder\n",
                ),
            ],
        );
        const ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:a-1\r\n\
SUMMARY:Dentist\r\n\
DTSTART:20260315T120000Z\r\n\
BEGIN:VALARM\r\n\
ACTION:EMAIL\r\n\
TRIGGER:-PT30M\r\n\
DESCRIPTION:This is an event reminder\r\n\
ATTENDEE:mailto:owner@example.com\r\n\
END:VALARM\r\n\
ATTENDEE;CN=Jane Doe:mailto:jane@example.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:b-1\r\n\
SUMMARY:Lunch\r\n\
DTSTART:20260320T170000Z\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:r-1\r\n\
SUMMARY:Standup\r\n\
DTSTART;TZID=America/New_York:20260309T100000\r\n\
RRULE:FREQ=WEEKLY;COUNT=2\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:r-1\r\n\
RECURRENCE-ID:20260316T140000Z\r\n\
SUMMARY:Standup (moved)\r\n\
DTSTART;TZID=America/New_York:20260316T103000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let files = [CalendarFile {
            name: "work.ics".into(),
            text: ICS.into(),
        }];
        let plan = plan_calendar(&state, &files).unwrap();
        let report = plan.report(None);
        let status = |id: &str| -> Vec<String> {
            report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|f| f["episode"] == id)
                .map(|f| {
                    format!(
                        "{}: {}",
                        f["status"].as_str().unwrap(),
                        f["reason"].as_str().unwrap()
                    )
                })
                .collect()
        };
        let id = |r: &str| hearthloom_core::ids::episode_id("ics", r);
        assert_eq!(
            status(&id("a-1")),
            ["replace: attendees, description differ"]
        );
        assert_eq!(status(&id("b-1")), ["add: an occurrence not stored yet"]);
        assert_eq!(status(&id("r-1")), ["add: an occurrence not stored yet"]);
        // the importer gives an override the ref earlier versions stored it
        // under, so it is found under its own ref
        let moved = status(&id("r-1#20260316T140000Z"));
        assert_eq!(moved, ["replace: description differ"], "{report}");
        assert!(!report.to_string().contains("reminder"));

        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!((applied.replaced, applied.added), (2, 2));
        let live: Vec<String> = live_of(&state, "ics").into_iter().map(|(r, _)| r).collect();
        assert_eq!(live.len(), 4, "{live:?}");
        for (base, n) in [("a-1#repair:", 1), ("r-1#20260316T140000Z#repair:", 1)] {
            assert_eq!(
                live.iter().filter(|r| r.starts_with(base)).count(),
                n,
                "{live:?}"
            );
        }
        assert!(live.contains(&"b-1".to_string()) && live.contains(&"r-1".to_string()));
        // the moved occurrence is one memory, under the ref it was stored under
        let (_, at_moved) = state
            .store
            .read_query(
                "SELECT COUNT(*) FROM episodes WHERE source = 'ics' AND superseded_by IS NULL
                 AND event_time = '2026-03-16T14:30:00Z'",
                1,
            )
            .unwrap();
        assert_eq!(at_moved[0][0], 1);
        let (_, dentist) = live_of(&state, "ics")
            .into_iter()
            .find(|(r, _)| r.starts_with("a-1#"))
            .unwrap();
        let text = text_of(&state, &dentist);
        assert!(
            !text.contains("reminder") && !text.contains("owner@"),
            "{text}"
        );
        assert!(text.contains("Jane Doe <jane@example.com>"), "{text}");

        let again = plan_calendar(&state, &files).unwrap();
        assert!(
            again.items.is_empty() && again.additions.is_empty(),
            "{:?}",
            again.findings
        );
        assert!(apply(&state, "human:owner", &again).unwrap().run.is_none());
    }

    /// An event stored at the same start under a ref that begins with the
    /// file's UID and `#` but is another event's own UID (`meeting#other`)
    /// is left as it is, and the file's event is imported under its ref.
    #[test]
    fn test_the_calendar_repair_leaves_an_event_whose_uid_only_begins_alike() {
        let (state, _tmp) = state();
        land(
            &state,
            "adapter:ics",
            vec![EpisodeDraft {
                source: "ics".into(),
                source_ref: "meeting#other".into(),
                kind: "calendar_event".into(),
                title: "Budget review".into(),
                body: "event: Budget review\nstart: 2026-03-20T17:00:00Z\n".into(),
                event_time: EventTime::parse_rfc3339(
                    "2026-03-20T17:00:00Z",
                    TimeUncertainty::Exact,
                )
                .unwrap(),
                trust: "imported".into(),
                scope: None,
                entities: vec![EntityRef {
                    entity_type: "Event".into(),
                    name: "Budget review".into(),
                    attributes: vec![("ics-uid".into(), "meeting#other".into())],
                    role: "subject".into(),
                }],
                edges: vec![],
            }],
        );
        const ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:meeting\r\n\
SUMMARY:Lunch\r\n\
DTSTART:20260320T170000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let plan = plan_calendar(
            &state,
            &[CalendarFile {
                name: "work.ics".into(),
                text: ICS.into(),
            }],
        )
        .unwrap();
        let report = plan.report(None);
        let id = |r: &str| hearthloom_core::ids::episode_id("ics", r);
        assert!(
            findings_for(&report, &id("meeting#other")).is_empty(),
            "{report}"
        );
        assert_eq!(
            findings_for(&report, &id("meeting")),
            ["add: an occurrence not stored yet"],
            "{report}"
        );
        assert!(plan.items.is_empty());
    }

    /// A file none of whose events can be read is named in the report, and
    /// nothing an event in it says is.
    #[test]
    fn test_an_unreadable_calendar_file_is_reported_without_its_words() {
        let (state, _tmp) = state();
        const BROKEN: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:x-1\r\n\
SUMMARY:Biopsy results with Dr Quinn\r\n\
LOCATION:Oncology wing\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let plan = plan_calendar(
            &state,
            &[CalendarFile {
                name: "health.ics".into(),
                text: BROKEN.into(),
            }],
        )
        .unwrap();
        let report = plan.report(None);
        assert_eq!(
            report["notes"],
            json!(["health.ics can't be read: it holds no event the importer can read"]),
            "{report}"
        );
        let shown = report.to_string();
        for words in ["Biopsy", "Quinn", "Oncology"] {
            assert!(!shown.contains(words), "the report shows {words:?}");
        }
    }

    /// An event the phone's calendar feed stored (same source, same UID, its
    /// own shape and times) is left as it is when an .ics file holds it.
    #[test]
    fn test_the_calendar_repair_leaves_what_the_live_feed_stored() {
        let (state, _tmp) = state();
        let ev: crate::live_ingest::CalendarEventIn = serde_json::from_value(json!({
            "uid": "p-1",
            "title": "Board meeting",
            "start": "2026-08-20T00:00:00-04:00",
            "all_day": true,
            "organizer": {"name": "Ada Lovelace", "email": "ada@example.com"},
            "attendees": [{"name": "Bob", "email": "bob@example.com"}],
        }))
        .unwrap();
        let fed = crate::live_ingest::calendar_in_to_episode(&ev).unwrap();
        land(&state, "human:device:phone", vec![fed]);
        const ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:p-1\r\n\
SUMMARY:Board meeting\r\n\
DTSTART;VALUE=DATE:20260820\r\n\
ORGANIZER;CN=Ada Lovelace:mailto:ada@example.com\r\n\
ATTENDEE;CN=Bob:mailto:bob@example.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let plan = plan_calendar(
            &state,
            &[CalendarFile {
                name: "work.ics".into(),
                text: ICS.into(),
            }],
        )
        .unwrap();
        let report = plan.report(None);
        assert_eq!(
            findings_for(&report, &hearthloom_core::ids::episode_id("ics", "p-1")),
            ["other_writer: not stored by the .ics importer (a live calendar feed, or a writer not recorded here); left as it is"],
            "{report}"
        );
        assert!(plan.items.is_empty() && plan.additions.is_empty());
    }

    /// (status, reason) of each finding about `episode`.
    fn findings_for(report: &Value, episode: &str) -> Vec<String> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["episode"] == episode)
            .map(|f| {
                format!(
                    "{}: {}",
                    f["status"].as_str().unwrap(),
                    f["reason"].as_str().unwrap()
                )
            })
            .collect()
    }

    /// A zip of `entries` (name, contents).
    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, contents) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn conversation(source: &str, id: &str, title: &str, turns: &[(&str, &str)]) -> EpisodeDraft {
        let mut body = format!("conversation: {title}\nprovider: chatgpt\n\n");
        for (speaker, text) in turns {
            body.push_str(&format!("{speaker}: {text}\n\n"));
        }
        EpisodeDraft {
            source: source.into(),
            source_ref: id.into(),
            kind: "conversation".into(),
            title: title.into(),
            body,
            event_time: EventTime::parse_rfc3339("2024-07-14T21:32:57Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: "self".into(),
            scope: None,
            entities: vec![],
            edges: vec![],
        }
    }

    const CHATGPT_EXPORT: &str = r#"[
      {"conversation_id":"c1","title":"Trip","create_time":1720992777.0,
       "current_node":"a2b",
       "mapping":{
         "root":{"parent":null,"children":["u1"],"message":null},
         "u1":{"parent":"root","children":["a1"],
               "message":{"author":{"role":"user"},"create_time":1720992778.0,
                          "content":{"parts":["Plan a trip"]}}},
         "a1":{"parent":"u1","children":["u2","u2e"],
               "message":{"author":{"role":"assistant"},"create_time":1720992780.0,
                          "content":{"parts":["Where to?"]}}},
         "u2":{"parent":"a1","children":["a2"],
               "message":{"author":{"role":"user"},"create_time":1720992790.0,
                          "content":{"parts":["The first wording"]}}},
         "a2":{"parent":"u2","children":[],
               "message":{"author":{"role":"assistant"},"create_time":1720992791.0,
                          "content":{"parts":["An answer to the first wording"]}}},
         "u2e":{"parent":"a1","children":["a2a","a2b"],
                "message":{"author":{"role":"user"},"create_time":1720992800.0,
                           "content":{"parts":["Lisbon in May"]}}},
         "a2a":{"parent":"u2e","children":[],
                "message":{"author":{"role":"assistant"},"create_time":1720992801.0,
                           "content":{"parts":["A regenerated-away answer"]}}},
         "a2b":{"parent":"u2e","children":[],
                "message":{"author":{"role":"assistant"},"create_time":1720992802.0,
                           "content":{"parts":["Lisbon it is"]}}}
       }},
      {"conversation_id":"c2","title":"Clinic","create_time":1720999999.0,
       "is_do_not_remember":true,
       "mapping":{"m":{"message":{"author":{"role":"user"},"create_time":1721000000.0,
                  "content":{"parts":["about the results"]}}}}},
      {"conversation_id":"c4","title":"Garden","create_time":1721000100.0,
       "mapping":{"g":{"message":{"author":{"role":"user"},"create_time":1721000101.0,
                  "content":{"parts":["what to plant"]}}}}}
    ]"#;

    /// A conversation an earlier version stored with every branch sorted
    /// by time (an edited prompt, a regenerated answer mixed in) is
    /// replaced by the branch it ended on; one the single-file route stored
    /// without its "do not remember" flag ends private; one the export
    /// doesn't hold is listed and left; the report names no words; and a
    /// second run — from the export zipped — finds nothing to do.
    #[test]
    fn test_the_chatgpt_repair_keeps_the_kept_branch_and_vaults_do_not_remember() {
        let (state, tmp) = state();
        land(
            &state,
            "adapter:openai",
            vec![
                // every node of the tree, sorted by time
                conversation(
                    "openai",
                    "c1",
                    "Trip",
                    &[
                        ("you", "Plan a trip"),
                        ("chatgpt", "Where to?"),
                        ("you", "The first wording"),
                        ("chatgpt", "An answer to the first wording"),
                        ("you", "Lisbon in May"),
                        ("chatgpt", "A regenerated-away answer"),
                        ("chatgpt", "Lisbon it is"),
                    ],
                ),
                conversation("openai", "c3", "Elsewhere", &[("you", "not in it")]),
                conversation("openai", "c4", "Garden", &[("you", "what to plant")]),
            ],
        );
        land(
            &state,
            "adapter:chatlog",
            vec![conversation(
                "chatlog-chatgpt",
                "c2",
                "Clinic",
                &[("you", "about the results")],
            )],
        );
        let export = tmp.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("conversations-000.json"), CHATGPT_EXPORT).unwrap();

        let plan = plan_chatgpt(&state, &export).unwrap();
        let report = plan.report(None);
        let id = |source: &str, r: &str| hearthloom_core::ids::episode_id(source, r);
        assert_eq!(
            findings_for(&report, &id("openai", "c1")),
            ["replace: 7 turns stored, 4 on the branch it ended on"]
        );
        assert_eq!(
            findings_for(&report, &id("chatlog-chatgpt", "c2")),
            ["private: reads the same; flagged \"do not remember\", it goes into the vault"]
        );
        assert_eq!(
            findings_for(&report, &id("openai", "c3")),
            ["not_in_export: not in this export; left as it is"]
        );
        assert_eq!(report["counts"]["fine"], 1);
        assert_eq!(report["checked"], 4);
        assert_eq!(
            report["extraction_again"], 1,
            "the corrected c1 is read again"
        );
        let shown = report.to_string();
        for words in ["Trip", "Lisbon", "wording", "Clinic", "results", "Garden"] {
            assert!(!shown.contains(words), "the report shows {words:?}");
        }

        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!((applied.replaced, applied.made_private), (1, 1));
        let live = live_of(&state, "openai");
        let refs: Vec<&str> = live.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(refs.len(), 3, "{refs:?}");
        assert!(refs.contains(&"c3") && refs.contains(&"c4"));
        let (_, fixed) = live
            .iter()
            .find(|(r, _)| r.starts_with("c1#repair:"))
            .expect("the corrected c1 in view");
        let text = text_of(&state, fixed);
        assert!(
            text.contains("you: Plan a trip\n\nchatgpt: Where to?\n\nyou: Lisbon in May\n\nchatgpt: Lisbon it is"),
            "{text}"
        );
        assert!(
            !text.contains("wording") && !text.contains("regenerated"),
            "{text}"
        );
        for source in ["chatlog-chatgpt", "openai"] {
            let private = state
                .store
                .private_source_refs(source, &["c2".to_string(), "c1".to_string()])
                .unwrap();
            let expected: HashSet<String> = if source == "chatlog-chatgpt" {
                HashSet::from(["c2".to_string()])
            } else {
                HashSet::new()
            };
            assert_eq!(private, expected, "{source}");
        }

        // the same export as a zip: nothing left to do
        let zipped = tmp.path().join("chatgpt-export.zip");
        write_zip(
            &zipped,
            &[("chatgpt-export/conversations-000.json", CHATGPT_EXPORT)],
        );
        let again = plan_chatgpt(&state, &zipped).unwrap();
        assert!(
            again.items.is_empty() && again.private.is_empty(),
            "{:?}",
            again.findings
        );
        assert!(apply(&state, "human:owner", &again).unwrap().run.is_none());
    }

    /// A stored transcript the export can't account for is left: one that
    /// holds turns an older export lacks (the conversation continued since),
    /// one whose turns are the branch's own under another title (renamed
    /// since), and one that holds fewer turns, all on the branch (stored
    /// from an earlier export). Nothing is replaced.
    #[test]
    fn test_the_chatgpt_repair_leaves_what_an_older_export_lacks() {
        let (state, tmp) = state();
        let kept = [
            ("you", "Plan a trip"),
            ("chatgpt", "Where to?"),
            ("you", "Lisbon in May"),
            ("chatgpt", "Lisbon it is"),
        ];
        let continued: Vec<(&str, &str)> = kept
            .iter()
            .copied()
            .chain([("you", "And in June?"), ("chatgpt", "June works too")])
            .collect();
        land(
            &state,
            "adapter:openai",
            vec![
                conversation("openai", "c1", "Trip", &continued),
                conversation("openai", "c4", "Garden plans", &[("you", "what to plant")]),
            ],
        );
        land(
            &state,
            "adapter:chatlog",
            vec![conversation("chatlog-chatgpt", "c1", "Trip", &kept[..2])],
        );
        let export = tmp.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("conversations.json"), CHATGPT_EXPORT).unwrap();

        let plan = plan_chatgpt(&state, &export).unwrap();
        let report = plan.report(None);
        let id = |source: &str, r: &str| hearthloom_core::ids::episode_id(source, r);
        assert_eq!(
            findings_for(&report, &id("openai", "c1")),
            ["other_export: holds 2 turns this export doesn't (a later export's, or images named otherwise); left as it is"]
        );
        assert_eq!(
            findings_for(&report, &id("openai", "c4")),
            ["other_export: its turns are the branch it ended on; only its title or header reads otherwise (another export's); left as it is"]
        );
        assert_eq!(
            findings_for(&report, &id("chatlog-chatgpt", "c1")),
            ["other_export: every turn it holds is on the branch it ended on, and this export holds 2 more (a later export's); left as it is"]
        );
        assert!(plan.items.is_empty(), "{:?}", plan.findings);
        assert!(apply(&state, "human:owner", &plan).unwrap().run.is_none());
    }

    const FLAGGED_EXPORT: &str = r#"[
      {"conversation_id":"c5","title":"Clinic","create_time":1721000000.0,
       "is_do_not_remember":true,"current_node":"a1b",
       "mapping":{
         "root":{"parent":null,"children":["u1"],"message":null},
         "u1":{"parent":"root","children":["a1a","a1b"],
               "message":{"author":{"role":"user"},"create_time":1721000001.0,
                          "content":{"parts":["about the results"]}}},
         "a1a":{"parent":"u1","children":[],
                "message":{"author":{"role":"assistant"},"create_time":1721000002.0,
                           "content":{"parts":["a first answer"]}}},
         "a1b":{"parent":"u1","children":[],
                "message":{"author":{"role":"assistant"},"create_time":1721000003.0,
                           "content":{"parts":["the kept answer"]}}}
       }}
    ]"#;

    /// A conversation flagged "do not remember" whose stored transcript
    /// also mixes in an abandoned answer: its record goes into the vault
    /// before the corrected copy lands, so the copy lands private — never
    /// outside the vault — and the undo leaves it in the vault.
    #[test]
    fn test_a_flagged_conversation_is_vaulted_before_its_correction_lands() {
        let (state, tmp) = state();
        land(
            &state,
            "adapter:openai",
            vec![conversation(
                "openai",
                "c5",
                "Clinic",
                &[
                    ("you", "about the results"),
                    ("chatgpt", "a first answer"),
                    ("chatgpt", "the kept answer"),
                ],
            )],
        );
        let export = tmp.path().join("export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("conversations-000.json"), FLAGGED_EXPORT).unwrap();

        let plan = plan_chatgpt(&state, &export).unwrap();
        let report = plan.report(None);
        assert_eq!(
            findings_for(&report, &hearthloom_core::ids::episode_id("openai", "c5")),
            ["replace: 3 turns stored, 2 on the branch it ended on; flagged \"do not remember\", it ends private"]
        );
        assert_eq!(report["extraction_again"], 0);
        let item = &plan.items[0];
        let fixed = replacement_ref("c5", &item.draft.title, &item.draft.body);
        let fixed_id = hearthloom_core::ids::episode_id("openai", &fixed);

        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!((applied.replaced, applied.made_private), (1, 1));
        let private = state
            .store
            .private_source_refs("openai", &["c5".to_string(), fixed.clone()])
            .unwrap();
        assert_eq!(private, HashSet::from(["c5".to_string(), fixed.clone()]));
        // the vault label came first: the copy landed private, and no label
        // reached it afterwards
        let feed = state.store.mutations_since(0, 1000).unwrap();
        let names_copy =
            |m: &&hearthloom_core::store::MutationRow| m.payload.to_string().contains(&fixed_id);
        let kinds: Vec<&str> = feed
            .iter()
            .filter(|m| {
                m.kind == "scope_private_on_reimport"
                    || (names_copy(m)
                        && (m.kind == "episode_ingested" || m.kind == "scope_private_copies"))
            })
            .map(|m| m.kind.as_str())
            .collect();
        assert_eq!(kinds, ["scope_private_on_reimport", "episode_ingested"]);

        // the undo brings the stored transcript back, still in the vault
        state
            .store
            .repair_undo(
                &state.home,
                &Actor::new("human:owner"),
                &applied.run.unwrap(),
            )
            .unwrap();
        let private = state
            .store
            .private_source_refs("openai", &["c5".to_string(), fixed])
            .unwrap();
        assert_eq!(private.len(), 2);
        let again = plan_chatgpt(&state, &export).unwrap();
        assert_eq!(
            findings_for(
                &again.report(None),
                &hearthloom_core::ids::episode_id("openai", "c5")
            ),
            ["undone: this correction was made and undone; left as it is"]
        );
    }

    fn health_draft(source_ref: &str, title: &str, body: &str, at: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "apple-health".into(),
            source_ref: source_ref.into(),
            kind: if source_ref.starts_with("day:") {
                "health_day".into()
            } else {
                "workout".into()
            },
            title: title.into(),
            body: body.into(),
            event_time: EventTime::parse_rfc3339(at, TimeUncertainty::Day).unwrap(),
            trust: "imported".into(),
            scope: Some("health".into()),
            entities: vec![],
            edges: vec![],
        }
    }

    const HEALTH_EXPORT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<HealthData locale="en_US">
 <ExportDate value="2026-03-12 09:00:00 -0500"/>
 <Record type="HKQuantityTypeIdentifierBodyMass" sourceName="Withings" unit="kg" startDate="2026-03-10 08:00:00 -0500" endDate="2026-03-10 08:00:00 -0500" value="70"/>
 <Record type="HKQuantityTypeIdentifierBodyMass" sourceName="Withings" unit="lb" startDate="2026-03-10 20:00:00 -0500" endDate="2026-03-10 20:00:00 -0500" value="154.324"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="Ada’s Apple Watch" unit="count" startDate="2026-03-11 09:00:00 -0500" endDate="2026-03-11 09:10:00 -0500" value="4200"/>
 <Record type="HKQuantityTypeIdentifierStepCount" sourceName="Ada’s Apple Watch" unit="count" startDate="2026-03-12 08:00:00 -0500" endDate="2026-03-12 08:10:00 -0500" value="300"/>
 <Workout workoutActivityType="HKWorkoutActivityTypeRunning" duration="30" durationUnit="min" totalDistance="5000" totalDistanceUnit="m" totalEnergyBurned="300" totalEnergyBurnedUnit="kcal" sourceName="Ada’s Apple Watch" startDate="2026-03-10 07:00:00 -0500" endDate="2026-03-10 07:30:00 -0500">
  <MetadataEntry key="HKIndoorWorkout" value="0"/>
 </Workout>
</HealthData>
"#;

    /// A day whose weights in kg and lb an earlier version averaged as one
    /// number, and a workout whose distance in metres it read as
    /// kilometres, are replaced by what the current reader makes of the
    /// export; a day the export doesn't cover, the day the export was made
    /// on (only part of it is in it) and the phone's own workout are left;
    /// a day that reads the same is fine. The export read from its zip
    /// finds nothing left to do.
    #[test]
    fn test_the_health_repair_corrects_mixed_units_and_metres() {
        let (state, tmp) = state();
        let export = tmp.path().join("apple_health_export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("export.xml"), HEALTH_EXPORT).unwrap();
        // the day the export reads the same, as the current reader makes it
        let reading = read_health(&export).unwrap();
        assert_eq!(reading.export_day.as_deref(), Some("2026-03-12"));
        let fine = reading
            .drafts
            .iter()
            .find(|d| d.source_ref == "day:2026-03-11")
            .unwrap()
            .clone();
        let workout_ref = "workout:2026-03-10 07:00:00 -0500";
        land(
            &state,
            "adapter:apple-health",
            vec![
                // 70 kg and 154.3 lb averaged as one number
                health_draft(
                    "day:2026-03-10",
                    "Health: 2026-03-10",
                    "body: 112.2 kg [withings]\nsources: withings",
                    "2026-03-10T13:00:00Z",
                ),
                // 5000 m read as 5000 km
                health_draft(
                    workout_ref,
                    "Running — 30 min · 5000.0 km · 300 kcal (apple watch)",
                    "workout: Running\nstart: 2026-03-10 07:00:00 -0500\nend: 2026-03-10 07:30:00 -0500\ntotals: 30 min · 5000.0 km · 300 kcal\n",
                    "2026-03-10T12:00:00Z",
                ),
                fine,
                health_draft(
                    "day:2026-03-09",
                    "Health: 2026-03-09",
                    "activity: 100 steps [iphone]\nsources: iphone",
                    "2026-03-09T13:00:00Z",
                ),
                health_draft(
                    "day:2026-03-12",
                    "Health: 2026-03-12 — 9000 steps",
                    "activity: 9000 steps [apple watch]\nsources: apple watch",
                    "2026-03-12T13:00:00Z",
                ),
                health_draft(
                    "workout:3F2504E0-4F89-11D3-9A0C-0305E82C3301",
                    "Walking (apple watch)",
                    "workout: Walking\n",
                    "2026-03-11T12:00:00Z",
                ),
            ],
        );

        let plan = plan_health(&state, &export).unwrap();
        let report = plan.report(None);
        let id = |r: &str| hearthloom_core::ids::episode_id("apple-health", r);
        assert_eq!(
            findings_for(&report, &id("day:2026-03-10")),
            ["replace: day: body differ"]
        );
        assert_eq!(
            findings_for(&report, &id(workout_ref)),
            ["replace: workout: title, totals differ"]
        );
        assert_eq!(
            findings_for(&report, &id("day:2026-03-09")),
            ["not_in_export: the export doesn't cover this day; left as it is"]
        );
        assert_eq!(
            findings_for(&report, &id("day:2026-03-12")),
            ["partial: the export was made that day, so it holds only part of it; left as it is"]
        );
        assert_eq!(report["counts"]["fine"], 1);
        assert_eq!(report["counts"]["from_phone"], 1);
        assert_eq!(report["extraction_again"], 0, "health extraction is off");
        let shown = report.to_string();
        for words in ["112.2", "70.0", "5000", "kg"] {
            assert!(!shown.contains(words), "the report shows {words:?}");
        }

        assert_eq!(apply(&state, "human:owner", &plan).unwrap().replaced, 2);
        let live = live_of(&state, "apple-health");
        let text = |base: &str| {
            let (_, content) = live
                .iter()
                .find(|(r, _)| r.starts_with(&format!("{base}#repair:")))
                .unwrap_or_else(|| panic!("{base} corrected: {live:?}"));
            text_of(&state, content)
        };
        assert_eq!(
            text("day:2026-03-10"),
            "body: 70.0 kg [withings]\nsources: withings"
        );
        let workout = text(workout_ref);
        assert!(
            workout.contains("totals: 30 min · 5.0 km · 300 kcal"),
            "{workout}"
        );
        for gone in ["day:2026-03-10", workout_ref] {
            assert!(!live.iter().any(|(r, _)| r == gone), "{gone} still in view");
        }

        // the export as Apple's zip, read in place: nothing left to do
        let zipped = tmp.path().join("export.zip");
        write_zip(
            &zipped,
            &[("apple_health_export/export.xml", HEALTH_EXPORT)],
        );
        let again = plan_health(&state, &zipped).unwrap();
        assert!(again.items.is_empty(), "{:?}", again.findings);
        assert!(apply(&state, "human:owner", &again).unwrap().run.is_none());
    }

    /// A day summary stored from a reading that holds a tracker or a measure
    /// the given export lacks for that day (an export made before they
    /// synced) is left; the undo of a health run brings the stored day back.
    #[test]
    fn test_the_health_repair_leaves_a_day_the_export_holds_less_of() {
        let (state, tmp) = state();
        let export = tmp.path().join("apple_health_export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("export.xml"), HEALTH_EXPORT).unwrap();
        land(
            &state,
            "adapter:apple-health",
            vec![
                // a later reading: the ring's night synced after this export
                health_draft(
                    "day:2026-03-11",
                    "Health: 2026-03-11 — 4200 steps · 7h10m sleep",
                    "activity: 4200 steps [apple watch]\nsleep: 7h10m asleep [oura]\nsources: apple watch, oura",
                    "2026-03-11T14:00:00Z",
                ),
                // averaged across units
                health_draft(
                    "day:2026-03-10",
                    "Health: 2026-03-10",
                    "body: 112.2 kg [withings]\nsources: withings",
                    "2026-03-10T13:00:00Z",
                ),
            ],
        );
        let plan = plan_health(&state, &export).unwrap();
        let report = plan.report(None);
        let id = |r: &str| hearthloom_core::ids::episode_id("apple-health", r);
        assert_eq!(
            findings_for(&report, &id("day:2026-03-11")),
            ["other_export: it shows trackers or measures this export doesn't hold for the day (a later export's?); left as it is"]
        );
        assert_eq!(
            findings_for(&report, &id("day:2026-03-10")),
            ["replace: day: body differ"]
        );
        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!(applied.replaced, 1);
        state
            .store
            .repair_undo(
                &state.home,
                &Actor::new("human:owner"),
                &applied.run.unwrap(),
            )
            .unwrap();
        let live: Vec<String> = live_of(&state, "apple-health")
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(live, ["day:2026-03-10", "day:2026-03-11"]);
        let again = plan_health(&state, &export).unwrap();
        assert_eq!(
            findings_for(&again.report(None), &id("day:2026-03-10")),
            ["undone: this correction was made and undone; left as it is"]
        );
    }

    /// A photo point: `name` at `hour` of `day_start` (unix seconds of
    /// the day's midnight, UTC), carrying the key an earlier Mac scan gave it.
    fn photo(
        day_start: i64,
        name: &str,
        hour: i64,
        earlier: Option<&str>,
    ) -> hearthloom_core::store::PhotoPointIn {
        let at = day_start + hour * 3600;
        hearthloom_core::store::PhotoPointIn {
            key: format!("{at}|{name}"),
            filename: name.into(),
            taken_at: chrono::DateTime::from_timestamp(at, 0)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
            tz_offset_minutes: 0,
            lat: Some(42.0),
            lon: Some(-71.0),
            place: Some("Cambridge".into()),
            earlier_key: earlier.map(|e| format!("{at}|{e}")),
        }
    }

    /// A photo day minted while each photo was stored twice — the earlier
    /// Mac scan's key next to the phone's — shows every photo twice. Once
    /// the duplicates are one point each, it is re-minted under its next
    /// revision holding each once, because the photos stored here cover
    /// every one it shows; a day minted elsewhere that shows a photo this
    /// node doesn't hold is left. The ledger goes with the re-mint, the
    /// finalize pass then has nothing to do, a second run finds nothing,
    /// and the undo puts the day and its ledger back.
    #[test]
    fn test_the_photo_day_repair_re_mints_only_a_day_its_photos_cover() {
        use hearthloom_adapters::photos as photos_adapter;
        let (state, _tmp) = state();
        let store = &state.store;
        // 2026-07-20T00:00:00Z
        const JULY_20: i64 = 1_784_505_600;
        const JULY_21: i64 = JULY_20 + 86_400;
        const JULY_22: i64 = JULY_21 + 86_400;
        // July 20: an earlier scan's points next to the phone's
        for hour in 0..3 {
            store
                .photos_add_points(
                    "mac",
                    &[photo(JULY_20, &format!("UUID-{hour}.heic"), hour, None)],
                )
                .unwrap();
            store
                .photos_add_points(
                    "iPhone",
                    &[photo(JULY_20, &format!("IMG_{hour}.HEIC"), hour, None)],
                )
                .unwrap();
        }
        // July 21: one photo here; the day another node minted shows two
        store
            .photos_add_points("iPhone", &[photo(JULY_21, "IMG_A.HEIC", 10, None)])
            .unwrap();
        let elsewhere: Vec<photos_adapter::PhotoPoint> = [("IMG_A.HEIC", 10), ("IMG_C.HEIC", 12)]
            .iter()
            .map(|(name, hour)| photos_adapter::PhotoPoint {
                at_unix: JULY_21 + hour * 3600,
                tz_offset_minutes: Some(0),
                latitude: Some(42.0),
                longitude: Some(-71.0),
                filename: name.to_string(),
                place: Some("Cambridge".into()),
                stored_name: None,
            })
            .collect();
        land(
            &state,
            "peer:cloud",
            vec![photos_adapter::day_draft("2026-07-21", &elsewhere, None, 1)],
        );
        // July 22: minted from exactly what is here
        store
            .photos_add_points("iPhone", &[photo(JULY_22, "IMG_X.HEIC", 9, None)])
            .unwrap();
        crate::live_photos::finalize_photo_days(&state).unwrap();
        let shown = |day: &str| {
            store
                .current_photo_episode(&state.home, day)
                .unwrap()
                .unwrap()
        };
        assert_eq!(shown("2026-07-20").2, 6);
        // this version's scan: each library name is the phone's photo
        let scan: Vec<_> = (0..3)
            .map(|hour| {
                photo(
                    JULY_20,
                    &format!("IMG_{hour}.HEIC"),
                    hour,
                    Some(&format!("UUID-{hour}.heic")),
                )
            })
            .collect();
        store.photos_add_points("mac", &scan).unwrap();
        assert_eq!(store.photo_points_for_day("2026-07-20").unwrap().len(), 3);
        assert_eq!(
            crate::live_photos::finalize_photo_days(&state).unwrap(),
            (0, 0),
            "the current rule only re-mints a day that grows"
        );
        let before = store.photo_day_state("2026-07-20").unwrap().unwrap();

        let plan = plan_photo_days(&state).unwrap();
        let report = plan.report(None);
        let id = |day: &str| shown(day).0;
        assert_eq!(
            findings_for(&report, &id("2026-07-20")),
            ["remint: shows 6 photos, 3 once each"]
        );
        assert_eq!(
            findings_for(&report, &id("2026-07-21")),
            ["not_covered: it shows 1 photo this node doesn't hold; left as it is"]
        );
        assert_eq!(report["counts"]["fine"], 1);
        assert!(!report.to_string().contains("IMG_"), "no file names");

        let applied = apply(&state, "human:owner", &plan).unwrap();
        assert_eq!((applied.revised, applied.ledgers), (1, 1));
        let (_, now_ref, count) = shown("2026-07-20");
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r2", 3));
        assert_eq!(shown("2026-07-21").1, "photos#2026-07-21", "left as it was");
        let (ledger_count, _, _, revision) = store.photo_day_state("2026-07-20").unwrap().unwrap();
        assert_eq!((ledger_count, revision), (3, 2));
        assert_eq!(
            crate::live_photos::finalize_photo_days(&state).unwrap(),
            (0, 0)
        );
        let again = plan_photo_days(&state).unwrap();
        assert!(
            again.revisions.is_empty() && again.photo_ledgers.is_empty(),
            "{:?}",
            again.findings
        );
        assert!(apply(&state, "human:owner", &again).unwrap().run.is_none());

        // the undo: the day as it was, and its ledger — at the run's
        // revision, which stays stored behind it
        store
            .repair_undo(
                &state.home,
                &Actor::new("human:owner"),
                &applied.run.unwrap(),
            )
            .unwrap();
        assert_eq!(shown("2026-07-20").1, "photos#2026-07-20");
        let (count, hash, episode, revision) =
            store.photo_day_state("2026-07-20").unwrap().unwrap();
        assert_eq!((count, hash, episode), (before.0, before.1, before.2));
        assert_eq!(revision, 2);
        // asking again doesn't make the undone correction again
        let after_undo = plan_photo_days(&state).unwrap();
        assert_eq!(
            findings_for(&after_undo.report(None), &id("2026-07-20")),
            ["undone: this correction was made and undone; left as it is"]
        );
        assert!(after_undo.revisions.is_empty());
        // the day gains a photo: the finalize pass mints it past the run's
        // revision
        store
            .photos_add_points("iPhone", &[photo(JULY_20, "IMG_3.HEIC", 3, None)])
            .unwrap();
        assert_eq!(
            crate::live_photos::finalize_photo_days(&state).unwrap(),
            (0, 1)
        );
        let (_, now_ref, count) = shown("2026-07-20");
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r3", 4));
    }

    /// Only an inflated count is corrected: a day that shows each of its
    /// photos once but reads otherwise now (a place read since) is left;
    /// a day that shows fewer photos than are stored here is re-minted only
    /// while the count it was minted with here keeps the finalize pass from
    /// adding them.
    #[test]
    fn test_the_photo_day_repair_corrects_only_an_inflated_count() {
        use hearthloom_adapters::photos as photos_adapter;
        let (state, _tmp) = state();
        let store = &state.store;
        // 2026-07-20T00:00:00Z
        const JULY_20: i64 = 1_784_505_600;
        const JULY_21: i64 = JULY_20 + 86_400;
        let point =
            |day_start: i64, name: &str, hour: i64, place: &str| photos_adapter::PhotoPoint {
                at_unix: day_start + hour * 3600,
                tz_offset_minutes: Some(0),
                latitude: Some(42.0),
                longitude: Some(-71.0),
                filename: name.to_string(),
                place: Some(place.into()),
                stored_name: None,
            };
        // July 20: one photo, minted elsewhere when its place read otherwise
        store
            .photos_add_points("iPhone", &[photo(JULY_20, "IMG_A.HEIC", 9, None)])
            .unwrap();
        // July 21: three photos shown, five stored here
        for (name, hour) in [("IMG_1.HEIC", 9), ("IMG_2.HEIC", 10), ("IMG_3.HEIC", 11)] {
            store
                .photos_add_points("iPhone", &[photo(JULY_21, name, hour, None)])
                .unwrap();
        }
        land(
            &state,
            "peer:cloud",
            vec![
                photos_adapter::day_draft(
                    "2026-07-20",
                    &[point(JULY_20, "IMG_A.HEIC", 9, "Boston")],
                    None,
                    1,
                ),
                photos_adapter::day_draft(
                    "2026-07-21",
                    &[("IMG_1.HEIC", 9), ("IMG_2.HEIC", 10), ("IMG_3.HEIC", 11)]
                        .map(|(name, hour)| point(JULY_21, name, hour, "Cambridge")),
                    None,
                    1,
                ),
            ],
        );
        crate::live_photos::finalize_photo_days(&state).unwrap();
        for (name, hour) in [("IMG_4.HEIC", 12), ("IMG_5.HEIC", 13)] {
            store
                .photos_add_points("iPhone", &[photo(JULY_21, name, hour, None)])
                .unwrap();
        }
        let id = |day: &str| {
            store
                .current_photo_episode(&state.home, day)
                .unwrap()
                .unwrap()
                .0
        };
        // minted with as many photos as are stored: the finalize pass adds them
        let plan = plan_photo_days(&state).unwrap();
        assert!(plan.revisions.is_empty(), "{:?}", plan.findings);
        assert_eq!(plan.report(None)["counts"]["reads_otherwise"], 2);

        // the count it was minted with here is higher than the photos stored
        // (a day minted with duplicates, read again since): re-minted
        let (_, hash, _, revision) = store.photo_day_state("2026-07-21").unwrap().unwrap();
        store
            .set_photo_day_state("2026-07-21", 6, &hash, None, revision)
            .unwrap();
        let plan = plan_photo_days(&state).unwrap();
        let report = plan.report(None);
        assert_eq!(
            findings_for(&report, &id("2026-07-21")),
            ["remint: shows 3 photos of the 5 stored here; the count it was minted with here keeps the rest out"]
        );
        assert!(findings_for(&report, &id("2026-07-20")).is_empty());
        assert_eq!(plan.revisions.len(), 1);
        apply(&state, "human:owner", &plan).unwrap();
        let (_, now_ref, count) = store
            .current_photo_episode(&state.home, "2026-07-21")
            .unwrap()
            .unwrap();
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-21#r2", 5));
    }

    /// Two photos of the same file name in the same minute (two devices'
    /// IMG_1 a few seconds apart) are two photos: a node holding only one
    /// of them doesn't cover the day, and the day isn't re-minted as if
    /// the other were a duplicate.
    #[test]
    fn test_one_photo_never_covers_two_shown_under_its_name() {
        use hearthloom_adapters::photos as photos_adapter;
        let (state, _tmp) = state();
        // 2026-07-20T10:00:00Z
        const TEN: i64 = 1_784_505_600 + 10 * 3600;
        let at = |secs: i64| photos_adapter::PhotoPoint {
            at_unix: TEN + secs,
            tz_offset_minutes: Some(0),
            latitude: Some(42.0),
            longitude: Some(-71.0),
            filename: "IMG_1.HEIC".into(),
            place: Some("Cambridge".into()),
            stored_name: None,
        };
        land(
            &state,
            "peer:cloud",
            vec![photos_adapter::day_draft(
                "2026-07-20",
                &[at(5), at(45)],
                None,
                1,
            )],
        );
        // only the first is stored here
        state
            .store
            .photos_add_points(
                "iPhone",
                &[hearthloom_core::store::PhotoPointIn {
                    key: format!("{}|IMG_1.HEIC", TEN + 5),
                    filename: "IMG_1.HEIC".into(),
                    taken_at: "2026-07-20T10:00:05Z".into(),
                    tz_offset_minutes: 0,
                    lat: Some(42.0),
                    lon: Some(-71.0),
                    place: Some("Cambridge".into()),
                    earlier_key: None,
                }],
            )
            .unwrap();
        let plan = plan_photo_days(&state).unwrap();
        let id = hearthloom_core::ids::episode_id("photos", "photos#2026-07-20");
        assert_eq!(
            findings_for(&plan.report(None), &id),
            ["not_covered: it shows 1 photo this node doesn't hold; left as it is"]
        );
        assert!(plan.revisions.is_empty());
    }

    /// A node that received another node's correction of a photo day (its
    /// revision past the one this node's ledger names) mints the day again
    /// when it grows, past that revision — never again under a ref already
    /// taken, which would never land.
    #[test]
    fn test_a_node_that_received_a_photo_day_correction_mints_past_it() {
        let (a, _tmp_a) = state();
        let (b, _tmp_b) = state();
        // 2026-07-20T00:00:00Z
        const JULY_20: i64 = 1_784_505_600;
        for hour in 0..3 {
            a.store
                .photos_add_points(
                    "mac",
                    &[photo(JULY_20, &format!("UUID-{hour}.heic"), hour, None)],
                )
                .unwrap();
            a.store
                .photos_add_points(
                    "iPhone",
                    &[photo(JULY_20, &format!("IMG_{hour}.HEIC"), hour, None)],
                )
                .unwrap();
        }
        crate::live_photos::finalize_photo_days(&a).unwrap();
        let scan: Vec<_> = (0..3)
            .map(|hour| {
                photo(
                    JULY_20,
                    &format!("IMG_{hour}.HEIC"),
                    hour,
                    Some(&format!("UUID-{hour}.heic")),
                )
            })
            .collect();
        a.store.photos_add_points("mac", &scan).unwrap();
        // b holds the same three photos, and minted the day from them
        b.store
            .photo_points_apply(&a.store.photo_points_export(0, 500).unwrap())
            .unwrap();
        crate::live_photos::finalize_photo_days(&b).unwrap();
        assert_eq!(b.store.photo_day_state("2026-07-20").unwrap().unwrap().3, 1);

        // a corrects its day; the correction reaches b and stands there
        let plan = plan_photo_days(&a).unwrap();
        assert_eq!(plan.revisions.len(), 1, "{:?}", plan.findings);
        apply(&a, "human:owner", &plan).unwrap();
        let page = a.store.sync_export_for(&a.home, 0, 100, Some("b")).unwrap();
        b.store
            .apply_batch_from(
                &b.home,
                &IngestBatch {
                    actor: "peer:a".into(),
                    session_id: None,
                    episodes: page.episodes,
                },
                &page.origins,
            )
            .unwrap();
        let shown = |s: &AppState| {
            s.store
                .current_photo_episode(&s.home, "2026-07-20")
                .unwrap()
                .unwrap()
        };
        assert_eq!(shown(&b).1, "photos#2026-07-20#r2");

        // b's day grows: minted past the correction, and only once
        b.store
            .photos_add_points("iPhone", &[photo(JULY_20, "IMG_3.HEIC", 3, None)])
            .unwrap();
        assert_eq!(crate::live_photos::finalize_photo_days(&b).unwrap(), (0, 1));
        let (_, now_ref, count) = shown(&b);
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r3", 4));
        assert_eq!(crate::live_photos::finalize_photo_days(&b).unwrap(), (0, 0));
    }

    /// A node that minted a day from fewer photos than another node's
    /// correction shows keeps the correction in view when its own photos
    /// grow but still don't hold every one the correction shows: minting
    /// past it would take its place with less.
    #[test]
    fn test_a_correction_showing_more_photos_stays_when_this_node_gains_fewer() {
        let (a, _tmp_a) = state();
        let (b, _tmp_b) = state();
        // 2026-07-20T00:00:00Z
        const JULY_20: i64 = 1_784_505_600;
        for hour in 0..5 {
            a.store
                .photos_add_points(
                    "mac",
                    &[photo(JULY_20, &format!("UUID-{hour}.heic"), hour, None)],
                )
                .unwrap();
            a.store
                .photos_add_points(
                    "iPhone",
                    &[photo(JULY_20, &format!("IMG_{hour}.HEIC"), hour, None)],
                )
                .unwrap();
        }
        crate::live_photos::finalize_photo_days(&a).unwrap();
        let scan: Vec<_> = (0..5)
            .map(|hour| {
                photo(
                    JULY_20,
                    &format!("IMG_{hour}.HEIC"),
                    hour,
                    Some(&format!("UUID-{hour}.heic")),
                )
            })
            .collect();
        a.store.photos_add_points("mac", &scan).unwrap();
        // b holds two of the photos, and minted the day from them
        b.store
            .photos_add_points(
                "iPhone",
                &[
                    photo(JULY_20, "IMG_0.HEIC", 0, None),
                    photo(JULY_20, "IMG_1.HEIC", 1, None),
                ],
            )
            .unwrap();
        crate::live_photos::finalize_photo_days(&b).unwrap();
        assert_eq!(b.store.photo_day_state("2026-07-20").unwrap().unwrap().0, 2);

        // a's five-photo correction reaches b and stands there
        let plan = plan_photo_days(&a).unwrap();
        assert_eq!(plan.revisions.len(), 1, "{:?}", plan.findings);
        apply(&a, "human:owner", &plan).unwrap();
        let page = a.store.sync_export_for(&a.home, 0, 100, Some("b")).unwrap();
        b.store
            .apply_batch_from(
                &b.home,
                &IngestBatch {
                    actor: "peer:a".into(),
                    session_id: None,
                    episodes: page.episodes,
                },
                &page.origins,
            )
            .unwrap();
        let shown = |s: &AppState| {
            s.store
                .current_photo_episode(&s.home, "2026-07-20")
                .unwrap()
                .unwrap()
        };
        let (_, now_ref, count) = shown(&b);
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r2", 5));

        // b gains a third photo: three don't hold the five shown
        b.store
            .photos_add_points("iPhone", &[photo(JULY_20, "IMG_2.HEIC", 2, None)])
            .unwrap();
        assert_eq!(crate::live_photos::finalize_photo_days(&b).unwrap(), (0, 0));
        let (_, now_ref, count) = shown(&b);
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r2", 5));
    }

    /// Node `a`'s three photos of 2026-07-20, the day minted while one of
    /// them was also stored under the key an earlier Mac scan gave it (so
    /// it shows four), and corrected by a run here; `a` is returned with
    /// that midnight.
    fn photo_day_corrected_with_one_duplicate() -> (AppState, tempfile::TempDir, i64) {
        let (a, tmp) = state();
        // 2026-07-20T00:00:00Z
        const JULY_20: i64 = 1_784_505_600;
        for hour in 0..3 {
            a.store
                .photos_add_points(
                    "iPhone",
                    &[photo(JULY_20, &format!("IMG_{hour}.HEIC"), hour, None)],
                )
                .unwrap();
        }
        a.store
            .photos_add_points("mac", &[photo(JULY_20, "UUID-0.heic", 0, None)])
            .unwrap();
        crate::live_photos::finalize_photo_days(&a).unwrap();
        a.store
            .photos_add_points(
                "mac",
                &[photo(JULY_20, "IMG_0.HEIC", 0, Some("UUID-0.heic"))],
            )
            .unwrap();
        let plan = plan_photo_days(&a).unwrap();
        assert_eq!(plan.revisions.len(), 1, "{:?}", plan.findings);
        apply(&a, "human:owner", &plan).unwrap();
        (a, tmp, JULY_20)
    }

    /// `from`'s episodes applied on `to` as its peer's pull.
    fn pull_episodes(from: &AppState, to: &AppState) {
        let page = from
            .store
            .sync_export_for(&from.home, 0, 100, Some("to"))
            .unwrap();
        to.store
            .apply_batch_from(
                &to.home,
                &IngestBatch {
                    actor: "peer:from".into(),
                    session_id: None,
                    episodes: page.episodes,
                },
                &page.origins,
            )
            .unwrap();
    }

    /// A node with no ledger for a photo day, where another node's
    /// correction waits behind the day's copy in view (this node can't yet
    /// tell the correction holds it), doesn't mint past both while its
    /// photos outnumber the copy in view without holding every photo it
    /// shows; once they hold it, the correction stands and the day grows
    /// past it.
    #[test]
    fn test_a_node_without_a_ledger_keeps_a_day_its_photos_dont_hold() {
        let (a, _tmp_a, july_20) = photo_day_corrected_with_one_duplicate();
        let (b, _tmp_b) = state();
        // the phone's three photos and two more; nothing minted here
        for (name, hour) in [
            ("IMG_0.HEIC", 0),
            ("IMG_1.HEIC", 1),
            ("IMG_2.HEIC", 2),
            ("IMG_4.HEIC", 4),
            ("IMG_5.HEIC", 5),
        ] {
            b.store
                .photos_add_points("iPhone", &[photo(july_20, name, hour, None)])
                .unwrap();
        }
        pull_episodes(&a, &b);
        let shown = |s: &AppState| {
            s.store
                .current_photo_episode(&s.home, "2026-07-20")
                .unwrap()
                .unwrap()
        };
        assert_eq!(shown(&b).1, "photos#2026-07-20");
        assert!(b.store.photo_day_state("2026-07-20").unwrap().is_none());

        // five photos here, but not the one shown under the earlier key
        assert_eq!(crate::live_photos::finalize_photo_days(&b).unwrap(), (0, 0));
        assert_eq!(shown(&b).1, "photos#2026-07-20");

        // a's photos arrive: the correction stands, and the day grows past it
        b.store
            .photo_points_apply_and_settle(&b.home, &a.store.photo_points_export(0, 500).unwrap())
            .unwrap();
        assert_eq!(shown(&b).1, "photos#2026-07-20#r2");
        assert_eq!(crate::live_photos::finalize_photo_days(&b).unwrap(), (0, 1));
        let (_, now_ref, count) = shown(&b);
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r3", 5));
    }

    /// A node with no ledger for a photo day, where another node's
    /// correction stands (this node holds the copy it replaced: a photo by
    /// both its names, the others by their lines), doesn't mint past it
    /// while its photos outnumber the correction's without holding every
    /// photo it shows; once they hold it, the day grows past it.
    #[test]
    fn test_a_node_without_a_ledger_keeps_a_standing_correction_its_photos_dont_hold() {
        let (a, _tmp_a, july_20) = photo_day_corrected_with_one_duplicate();
        let (b, _tmp_b) = state();
        // one of the correction's photos, by both its names; three others
        b.store
            .photos_add_points(
                "mac",
                &[photo(july_20, "IMG_0.HEIC", 0, Some("UUID-0.heic"))],
            )
            .unwrap();
        for (name, hour) in [("IMG_4.HEIC", 4), ("IMG_5.HEIC", 5), ("IMG_6.HEIC", 6)] {
            b.store
                .photos_add_points("iPhone", &[photo(july_20, name, hour, None)])
                .unwrap();
        }
        pull_episodes(&a, &b);
        let shown = |s: &AppState| {
            s.store
                .current_photo_episode(&s.home, "2026-07-20")
                .unwrap()
                .unwrap()
        };
        let (_, now_ref, count) = shown(&b);
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r2", 3));
        assert!(b.store.photo_day_state("2026-07-20").unwrap().is_none());

        // four photos here, but two the correction shows aren't among them
        assert_eq!(crate::live_photos::finalize_photo_days(&b).unwrap(), (0, 0));
        assert_eq!(shown(&b).1, "photos#2026-07-20#r2");

        // a's photos arrive: the day grows past the correction, holding all
        b.store
            .photo_points_apply_and_settle(&b.home, &a.store.photo_points_export(0, 500).unwrap())
            .unwrap();
        assert_eq!(crate::live_photos::finalize_photo_days(&b).unwrap(), (0, 1));
        let (_, now_ref, count) = shown(&b);
        assert_eq!((now_ref.as_str(), count), ("photos#2026-07-20#r3", 6));
    }

    /// A correction waiting behind a day's copy on a node that couldn't yet
    /// tell it holds that copy stands once this node's own Mac scan records
    /// the key its photo took the place of — after the run settled here.
    #[test]
    fn test_a_photo_day_correction_stands_when_this_nodes_scan_shows_it_holds_the_day() {
        use hearthloom_adapters::photos as photos_adapter;
        let (a, _tmp_a, july_20) = photo_day_corrected_with_one_duplicate();
        let (b, _tmp_b) = state();
        // the phone's photos only
        for hour in 0..3 {
            b.store
                .photos_add_points(
                    "iPhone",
                    &[photo(july_20, &format!("IMG_{hour}.HEIC"), hour, None)],
                )
                .unwrap();
        }
        pull_episodes(&a, &b);
        let runs = a.store.repairs_sync_export("").unwrap();
        b.store.repairs_sync_apply(&runs).unwrap();
        for run in b.store.repairs_pending_here().unwrap() {
            b.store.repair_settle_run(&b.home, &run).unwrap();
        }
        assert!(b.store.repairs_pending_here().unwrap().is_empty());
        let shown = |s: &AppState| {
            s.store
                .current_photo_episode(&s.home, "2026-07-20")
                .unwrap()
                .unwrap()
                .1
        };
        assert_eq!(shown(&b), "photos#2026-07-20");

        // this node's scan: the library's name is the phone's photo
        let scan = [photos_adapter::PhotoPoint {
            at_unix: july_20,
            tz_offset_minutes: Some(0),
            latitude: Some(42.0),
            longitude: Some(-71.0),
            filename: "IMG_0.HEIC".into(),
            place: Some("Cambridge".into()),
            stored_name: Some("UUID-0.heic".into()),
        }];
        crate::live_photos::store_scan_points(&b, "mac", &scan).unwrap();
        assert_eq!(shown(&b), "photos#2026-07-20#r2");
    }

    /// The same correction stands when this node's scan stopped after
    /// recording the key its photo took the place of and before settling the
    /// day: the next read of the same scan, which changes nothing, settles it.
    #[test]
    fn test_a_photo_day_correction_stands_when_a_stopped_scan_is_read_again() {
        use hearthloom_adapters::photos as photos_adapter;
        let (a, _tmp_a, july_20) = photo_day_corrected_with_one_duplicate();
        let (b, _tmp_b) = state();
        for hour in 0..3 {
            b.store
                .photos_add_points(
                    "iPhone",
                    &[photo(july_20, &format!("IMG_{hour}.HEIC"), hour, None)],
                )
                .unwrap();
        }
        pull_episodes(&a, &b);
        let runs = a.store.repairs_sync_export("").unwrap();
        b.store.repairs_sync_apply(&runs).unwrap();
        for run in b.store.repairs_pending_here().unwrap() {
            b.store.repair_settle_run(&b.home, &run).unwrap();
        }
        let shown = |s: &AppState| {
            s.store
                .current_photo_episode(&s.home, "2026-07-20")
                .unwrap()
                .unwrap()
                .1
        };
        assert_eq!(shown(&b), "photos#2026-07-20");

        let scan = [photos_adapter::PhotoPoint {
            at_unix: july_20,
            tz_offset_minutes: Some(0),
            latitude: Some(42.0),
            longitude: Some(-71.0),
            filename: "IMG_0.HEIC".into(),
            place: Some("Cambridge".into()),
            stored_name: Some("UUID-0.heic".into()),
        }];
        // the scan's write lands; it stops before settling the day
        b.store
            .photos_add_points("mac", &crate::live_photos::scan_rows(&scan))
            .unwrap();
        assert_eq!(shown(&b), "photos#2026-07-20");
        // read again: nothing new is stored, and the day is settled
        assert_eq!(
            crate::live_photos::store_scan_points(&b, "mac", &scan).unwrap(),
            (0, 1)
        );
        assert_eq!(shown(&b), "photos#2026-07-20#r2");
    }

    /// A day summary that shows a measure of a line the export holds only
    /// part of — body fat beside a weight, exercise minutes beside steps —
    /// is left: replacing it would lose that measure.
    #[test]
    fn test_the_health_repair_leaves_a_day_showing_a_measure_the_export_lacks() {
        let (state, tmp) = state();
        let export = tmp.path().join("apple_health_export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("export.xml"), HEALTH_EXPORT).unwrap();
        land(
            &state,
            "adapter:apple-health",
            vec![
                // weights averaged across units, and a body-fat reading the
                // export lacks
                health_draft(
                    "day:2026-03-10",
                    "Health: 2026-03-10",
                    "body: 112.2 kg [withings] · 18.2% body fat [withings]\nsources: withings",
                    "2026-03-10T13:00:00Z",
                ),
                health_draft(
                    "day:2026-03-11",
                    "Health: 2026-03-11 — 4200 steps",
                    "activity: 4200 steps [apple watch] · 31 min exercise [apple watch]\nsources: apple watch",
                    "2026-03-11T14:00:00Z",
                ),
            ],
        );
        let plan = plan_health(&state, &export).unwrap();
        let report = plan.report(None);
        let id = |r: &str| hearthloom_core::ids::episode_id("apple-health", r);
        for day in ["day:2026-03-10", "day:2026-03-11"] {
            assert_eq!(
                findings_for(&report, &id(day)),
                ["other_export: it shows trackers or measures this export doesn't hold for the day (a later export's?); left as it is"],
                "{day}"
            );
        }
        assert!(plan.items.is_empty());
    }

    /// A night the watch recorded without stages and another tracker
    /// recorded in stages.
    const SLEEP_EXPORT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<HealthData locale="en_US">
 <ExportDate value="2026-03-12 09:00:00 -0500"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Ada’s Apple Watch" startDate="2026-03-09 23:00:00 -0500" endDate="2026-03-10 06:00:00 -0500" value="HKCategoryValueSleepAnalysisAsleepUnspecified"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="Ada’s Apple Watch" startDate="2026-03-10 23:00:00 -0500" endDate="2026-03-11 06:00:00 -0500" value="HKCategoryValueSleepAnalysisAsleepUnspecified"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="WHOOP" startDate="2026-03-10 23:30:00 -0500" endDate="2026-03-11 01:00:00 -0500" value="HKCategoryValueSleepAnalysisAsleepDeep"/>
 <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="WHOOP" startDate="2026-03-11 03:00:00 -0500" endDate="2026-03-11 05:00:00 -0500" value="HKCategoryValueSleepAnalysisAsleepREM"/>
</HealthData>
"#;

    /// A day whose sleep an earlier version added up from two trackers —
    /// the watch's night and another tracker's deep and REM stages — is
    /// replaced by the watch's night alone, its stages left out: the export
    /// holds them, from the tracker whose night isn't shown. A day showing
    /// a stage the export holds from no tracker is left.
    #[test]
    fn test_the_health_repair_corrects_sleep_added_up_from_two_trackers() {
        let (state, tmp) = state();
        let export = tmp.path().join("apple_health_export");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("export.xml"), SLEEP_EXPORT).unwrap();
        land(
            &state,
            "adapter:apple-health",
            vec![
                // the watch's seven hours, the other tracker's stages added
                health_draft(
                    "day:2026-03-11",
                    "Health: 2026-03-11 — 10h30m sleep",
                    "sleep: 10h30m asleep, 1h30m deep, 2h00m rem\nsources: apple watch, whoop",
                    "2026-03-11T04:00:00Z",
                ),
                // a deep stage no tracker in this export recorded
                health_draft(
                    "day:2026-03-10",
                    "Health: 2026-03-10 — 7h00m sleep",
                    "sleep: 7h00m asleep, 1h30m deep\nsources: apple watch",
                    "2026-03-10T04:00:00Z",
                ),
            ],
        );
        let plan = plan_health(&state, &export).unwrap();
        let report = plan.report(None);
        let id = |r: &str| hearthloom_core::ids::episode_id("apple-health", r);
        assert_eq!(
            findings_for(&report, &id("day:2026-03-11")),
            ["replace: day: title, sleep differ"]
        );
        assert_eq!(
            findings_for(&report, &id("day:2026-03-10")),
            ["other_export: it shows trackers or measures this export doesn't hold for the day (a later export's?); left as it is"]
        );
        assert_eq!(apply(&state, "human:owner", &plan).unwrap().replaced, 1);
        let live = live_of(&state, "apple-health");
        let (_, content) = live
            .iter()
            .find(|(r, _)| r.starts_with("day:2026-03-11#repair:"))
            .unwrap_or_else(|| panic!("the day corrected: {live:?}"));
        assert_eq!(
            text_of(&state, content),
            "sleep: 7h00m asleep [apple watch]\nsources: apple watch, whoop"
        );
    }

    #[test]
    fn test_a_day_summary_is_read_measure_by_measure() {
        let measures = |body: &str| {
            let mut m: Vec<String> = day_measures(body).into_iter().collect();
            m.sort();
            m
        };
        assert_eq!(
            measures(
                "activity: 4200 steps [apple watch] · 5.0 km [apple watch] · 300 kcal active [apple watch] · 30 min exercise [apple watch]\n\
                 heart: rhr 55 [oura] · hr 50–120 avg 70 [apple watch] · spo2 97% [apple watch]\n\
                 sleep: 7h10m asleep [oura], 1h30m deep, 2h00m rem, 8h00m in bed [oura]\n\
                 body: 154.3 lb [withings] · 18.2% body fat [withings]\n\
                 glucose: avg 100 mg/dL (80–120) [dexcom]\n\
                 sources: apple watch, oura"
            ),
            [
                "activity/amount",
                "activity/kcal active",
                "activity/min exercise",
                "activity/steps",
                "body/amount",
                "body/body fat",
                "glucose",
                "heart/hr",
                "heart/rhr",
                "heart/spo2",
                "sleep/asleep",
                "sleep/deep",
                "sleep/in bed",
                "sleep/rem",
            ]
        );
        // a weight in another unit is the same measure
        assert_eq!(
            measures("body: 112.2 kg [withings]"),
            measures("body: 70.0 lb [withings]")
        );
    }

    #[test]
    fn test_a_places_line_is_read_by_whole_names() {
        let places = ["Cambridge, MA".to_string(), "Boston".to_string()];
        assert!(joined_from("Cambridge, MA, Boston", &places));
        assert!(joined_from("Boston", &places));
        assert!(joined_from("", &places));
        assert!(!joined_from("Cambridge", &places));
        assert!(!joined_from("Boston, Salem", &places));
    }

    /// A run whose settling here keeps failing is tried again later and
    /// never holds up the runs after it.
    #[tokio::test]
    async fn test_a_run_that_fails_to_settle_holds_up_no_other() {
        let (state, _tmp) = state();
        {
            let conn = rusqlite::Connection::open(state.home.db_path()).unwrap();
            for (id, at) in [
                ("rp_stuck", "2026-01-01T00:00:00Z"),
                ("rp_later", "2026-01-02T00:00:00Z"),
            ] {
                conn.execute(
                    "INSERT INTO repairs (id, kind, replaced, added, actor, node, created_at,
                         undone_at, updated_at)
                     VALUES (?1, 'mail', 0, 0, 'human:owner', 'n-peer', ?2, ?2, ?2)",
                    [id, at],
                )
                .unwrap();
            }
            // the older run's settling can never be recorded here
            conn.execute_batch(
                "CREATE TRIGGER stuck BEFORE INSERT ON consolidation_state
                 WHEN NEW.key = 'repair.state.rp_stuck'
                 BEGIN SELECT RAISE(ABORT, 'disk trouble'); END;",
            )
            .unwrap();
        }
        assert_eq!(
            state.store.repairs_pending_here().unwrap(),
            ["rp_stuck", "rp_later"]
        );
        assert_eq!(catch_up(&state).await, 1);
        assert_eq!(state.store.repairs_pending_here().unwrap(), ["rp_stuck"]);
    }
}

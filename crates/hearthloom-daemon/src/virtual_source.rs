//! Virtualized sources: data that stays at origin and is queried at need,
//! never becoming substrate the system depends on (design law #1 governs
//! only *materialized* sources — the 9 adapters that clone into the episode
//! log). The first virtual source is HearthFS, a local-first semantic
//! filesystem exposing magic paths (`@search/query`, `file@fit/budget`) on
//! a mount.
//!
//! Two READ handles surface to the RLM (`vsearch` / `vfetch`) so the
//! deep-query loop stays read-only (design law #3). A separate, explicit
//! *promotion* step materializes a fetched result into an episode with
//! provenance recorded structurally (source = the virtual source,
//! source_ref = origin path, ingest_time = fetch time); untrusted origins
//! are quarantined first (the human gate — MINJA-class poisoning is
//! segregated, not sanitized). Virtual sources are best-effort/online: an
//! offline mount degrades to a clear error, never a panic or a stall.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use hearthloom_core::store::{Actor, VirtualSourceDef};
use hearthloom_core::{ids, EpisodeDraft, EventTime, IngestBatch};
use serde::Serialize;
use serde_json::json;

use crate::AppState;

/// Cap fetched content — a virtual read is a preview into the substrate,
/// not a bulk copy (bulk copy is what *promotion* is for).
const MAX_FETCH_CHARS: usize = 256 * 1024;
/// Bound a plain-filesystem search so a huge mount can't wedge a round.
const MAX_SCAN_FILES: usize = 5000;
const SNIPPET_CHARS: usize = 140;

/// One search hit: an origin-relative path plus a short snippet.
#[derive(Debug, Clone, Serialize)]
pub struct VirtualHit {
    pub path: String,
    pub snippet: String,
    /// What the privacy rules read for the item, when the search already
    /// has it (Gmail search) — never sent anywhere.
    #[serde(skip)]
    pub privacy: Option<Privacy>,
    /// What the item's memory is keyed by, when the search already knows
    /// (Gmail search) — for the privacy check, never sent anywhere.
    #[serde(skip)]
    pub key: ItemKey,
}

/// What the privacy rules read for an item. A message is judged on the
/// whole of it, decoded (`mbox_text::privacy_text`) — never on the part a
/// reader shows, which is cut and may still be encoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Privacy {
    /// A message read whole: its bytes, and the labels it carries besides
    /// the ones in its header (Gmail's, the index's) — decoded only when a
    /// rule has to be checked.
    Message { raw: Vec<u8>, labels: String },
    /// A message only the start of which was read: it is read whole from
    /// this path of the same source before it can be judged (a Gmail
    /// message, by its uid). If it can't be, it counts as private.
    ReadAt(String),
    /// Plain text, as it is (a file, a search snippet).
    Text(String),
}

/// An item as `fetch` read it: what the caller is shown, the key its memory
/// would carry, and what the privacy rules read.
#[derive(Debug, Clone)]
pub(crate) struct Fetched {
    pub shown: String,
    pub key: ItemKey,
    pub privacy: Privacy,
}

/// What the memory made from an item would be keyed by, besides the path
/// that reached it — the vault check looks the memory up by these.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ItemKey {
    /// Only its path (an item that isn't a message).
    #[default]
    Path,
    /// Its path and these: the ref an import gives the message
    /// (`mbox::message_id_ref`), and any other spelling of its Message-ID
    /// a reader parsed.
    Refs(Vec<String>),
    /// A message whose memory is keyed by a hash of the exact copy an
    /// import read (`mbox::UNKEYED_REF_PREFIX`) — it has no Message-ID, or
    /// its headers couldn't be read. No later read rebuilds that key.
    Unkeyed,
}

/// A message's key, read from its raw text by the import's own rule, and
/// beside it the Message-ID as a MIME parser reads it — `parsed`, or read
/// here when not given. A header holding more than `<id>` (a comment beside
/// it) gives the two different spellings; the vault check matches a
/// message's memory by its identity through either (`private_mail_ids`).
pub fn mail_key(raw: &str, parsed: Option<&str>) -> ItemKey {
    match hearthloom_adapters::mbox::message_id_ref(raw) {
        Some(id) => {
            let parsed = match parsed {
                Some(parsed) => Some(parsed.to_string()),
                None => mail_parser::MessageParser::default()
                    .parse_headers(raw.as_bytes())
                    .and_then(|m| m.message_id().map(String::from)),
            };
            let other = parsed.filter(|p| *p != id);
            ItemKey::Refs(std::iter::once(id).chain(other).collect())
        }
        None => ItemKey::Unkeyed,
    }
}

/// The result of a promotion: exactly one of `episode_id` (materialized)
/// or `quarantined` (untrusted origin, awaiting the human gate) is set.
#[derive(Debug, Clone, Serialize)]
pub struct PromoteOutcome {
    pub episode_id: Option<String>,
    pub quarantined: Option<String>,
    /// True when the episode already existed (idempotent re-promotion).
    pub existing: bool,
}

/// Is this source switched on? Absent flag = enabled (every row predates
/// the Integrations page's disable toggle).
pub fn is_enabled(def: &VirtualSourceDef) -> bool {
    def.config
        .get("enabled")
        .and_then(|e| e.as_bool())
        .unwrap_or(true)
}

/// Resolve a registered virtual source, or a clear error naming it.
pub fn resolve(state: &AppState, name: &str) -> Result<VirtualSourceDef> {
    let def = state
        .store
        .virtual_source(name)?
        .ok_or_else(|| anyhow!("no virtual source named {name:?} is registered"))?;
    if !is_enabled(&def) {
        bail!("virtual source {name:?} is disabled (Integrations page re-enables it)");
    }
    Ok(def)
}

/// Resolved mount path, for callers outside this module (register-time
/// liveness/index checks).
pub fn mount_path(def: &VirtualSourceDef) -> Result<PathBuf> {
    mount_of(def)
}

/// The on-disk root this source is mounted at (`config.mount`).
fn mount_of(def: &VirtualSourceDef) -> Result<PathBuf> {
    let mount = def
        .config
        .get("mount")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("virtual source {:?} has no `mount` in its config", def.name))?;
    Ok(PathBuf::from(expand_tilde(mount)))
}

/// A mail archive's file, when the source is on and its mount answers
/// (the background text pass reads only those).
pub fn mbox_path_if_online(def: &VirtualSourceDef) -> Option<PathBuf> {
    if def.adapter != "mbox" || !is_enabled(def) {
        return None;
    }
    let mount = mount_of(def).ok()?;
    mount_alive(&mount).then_some(mount)
}

/// Tilde-expanded PathBuf for callers outside this module (the register
/// handler's recursion probe).
pub fn expand_tilde_path(p: &str) -> PathBuf {
    PathBuf::from(expand_tilde(p))
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }
    p.to_string()
}

/// Does this source speak HearthFS magic paths? Default: yes for the
/// `hearthfs` adapter, overridable via `config.magic`. A `magic: false`
/// source is just a directory queried by filename/content match.
fn uses_magic(def: &VirtualSourceDef) -> bool {
    def.config
        .get("magic")
        .and_then(|m| m.as_bool())
        .unwrap_or(def.adapter == "hearthfs")
}

/// Trust tier promoted episodes inherit. Default `imported` (your own
/// files at origin); a federated/untrusted source sets `untrusted` so
/// promotions quarantine-first.
fn trust_of(def: &VirtualSourceDef) -> String {
    def.config
        .get("trust")
        .and_then(|t| t.as_str())
        .unwrap_or("imported")
        .to_string()
}

/// Join an origin-relative path onto the mount, rejecting anything that
/// escapes it (`..`, absolute or prefix components). Also the GPX pass's
/// join (consolidate.rs `workout_route_start`).
pub(crate) fn safe_join(mount: &Path, rel: &str) -> Result<PathBuf> {
    let rel = rel.trim_start_matches('/');
    for part in Path::new(rel).components() {
        match part {
            Component::Normal(_) => {}
            _ => bail!("path {rel:?} escapes the mount"),
        }
    }
    Ok(mount.join(rel))
}

/// Does this path answer within two seconds? A WEDGED network/userspace
/// mount (a stale NFS export after a reboot, a dead FUSE daemon) does not
/// fail — it blocks the calling thread in the kernel, uninterruptibly and
/// forever. So nothing may touch a mount on its own thread before this
/// says it is alive: the probe runs on a throwaway thread with a
/// deadline (the Integrations status card's pattern), and a failure is
/// remembered for 30s so repeated asks don't each leak another stuck
/// thread. Found live 2026-10-07: one question that searched a dead
/// HearthFS mount hung for ten minutes and parked a runtime worker.
pub(crate) fn mount_alive(path: &Path) -> bool {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static DEAD: OnceLock<Mutex<std::collections::HashMap<std::path::PathBuf, Instant>>> =
        OnceLock::new();
    let dead = DEAD.get_or_init(Default::default);
    if let Some(at) = dead.lock().unwrap().get(path) {
        if at.elapsed() < Duration::from_secs(30) {
            return false;
        }
    }
    // Probe in a CHILD PROCESS, never on a thread of this one. A thread
    // blocked on a dead mount cannot be cancelled, and — learned the hard
    // way on 2026-10-07 — a process with such a thread cannot EXIT: the
    // daemon hung in exit after a config-save restart, kept its listening
    // socket, and launchd could not start a replacement until a reboot.
    // A stuck child is just a stuck child; we stop waiting and move on.
    let alive = probe_in_child(path).unwrap_or_else(|| probe_on_thread(path));
    let mut dead = dead.lock().unwrap();
    if alive {
        dead.remove(path);
    } else {
        dead.insert(path.to_path_buf(), Instant::now());
    }
    alive
}

/// `ls -d <path>/.` in a child with a 2s deadline. None = couldn't even
/// spawn `ls` (a minimal container) — the caller falls back to a thread.
fn probe_in_child(path: &Path) -> Option<bool> {
    use std::time::{Duration, Instant};
    // The trailing "/." makes a directory resolve THROUGH the mount (a
    // bare stat of the mountpoint can be answered from cache); a plain
    // file fails that and is retried as itself.
    let try_one = |target: std::ffi::OsString| -> Option<bool> {
        let mut child = std::process::Command::new("/bin/ls")
            .arg("-d")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status.success()),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                _ => {
                    // Not answering. Kill what can be killed and do NOT
                    // wait on it — a child wedged in the kernel never reaps.
                    let _ = child.kill();
                    return Some(false);
                }
            }
        }
    };
    let mut through = path.as_os_str().to_os_string();
    through.push("/.");
    match try_one(through)? {
        true => Some(true),
        false => try_one(path.as_os_str().to_os_string()),
    }
}

/// Last resort where no `ls` exists: a throwaway thread with a deadline.
fn probe_on_thread(path: &Path) -> bool {
    let probe = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::metadata(&probe).is_ok());
    });
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or(false)
}

fn require_online(def: &VirtualSourceDef, mount: &Path) -> Result<()> {
    // FIRST, and with a deadline: every check below touches the mount.
    if !mount_alive(mount) {
        bail!(
            "virtual source {:?} is offline (mount {} is not answering)",
            def.name,
            mount.display()
        )
    }
    if !mount.exists() {
        bail!(
            "virtual source {:?} is offline (mount {} not present)",
            def.name,
            mount.display()
        )
    }
    // A dead FUSE mount leaves its mountpoint as a plain empty dir — for
    // magic-path sources the @search entry IS the liveness probe (its
    // absence used to surface as a 500 mid-read instead of offline).
    if uses_magic(def) && !mount.join("@search").exists() {
        bail!(
            "virtual source {:?} is offline (mount {} present but not serving @search)",
            def.name,
            mount.display()
        )
    }
    Ok(())
}

/// Search a virtual source for files matching `query`. HearthFS mounts use
/// the `@search` magic path; plain mounts fall back to a bounded recursive
/// filename+content match (AND over whitespace-split terms).
pub fn search(def: &VirtualSourceDef, query: &str, limit: usize) -> Result<Vec<VirtualHit>> {
    if def.adapter == crate::gmail_search::ADAPTER {
        // the live mailbox: Gmail's own search (0.5.89)
        return crate::gmail_search::search(query, limit);
    }
    let mount = mount_of(def)?;
    require_online(def, &mount)?;
    if def.adapter == "apple-health" {
        bail!(
            "{} is a numeric source — use vquery (read-only SQL over its sidecar)",
            def.name
        );
    }
    if def.adapter == "mbox" {
        // Mail archive: header FTS over the sidecar index; hit paths are
        // message ids usable with vfetch/promote.
        return Ok(crate::mbox_index::search(&mount, query, limit)?
            .into_iter()
            .map(|(id, snippet)| VirtualHit {
                path: id,
                snippet,
                privacy: None, // read (and keyed) by the vault check when needed
                key: ItemKey::default(),
            })
            .collect());
    }
    if uses_magic(def) {
        magic_search(&mount, query, limit)
    } else {
        fs_search(&mount, query, limit)
    }
}

/// Whether a message's memory is labelled private — the check
/// `item_private` makes of a message read in place, and the one triage
/// makes of mail the pull reads. Looked up by its key (`mail_key`) under
/// every source the record holds, registered or not: the Gmail pull and
/// the Takeout ingest keep messages under `mbox`, a promotion under the
/// name of the source it came from (which may since have been
/// unregistered, or never registered on this node) — matched by identity,
/// the same way as `private_mail_ids`. Also found: a private promotion an
/// earlier version kept under a path (`legacy_promotions`). A message whose
/// memory key can't be rebuilt counts as private while any memory keyed
/// that way is; every message does while a legacy promotion's message
/// can't be identified.
pub(crate) fn mail_memory_private(state: &AppState, key: &ItemKey) -> Result<bool> {
    Ok(match key {
        ItemKey::Path => false,
        ItemKey::Refs(refs) => !private_mail_ids(state, refs)?.is_empty(),
        ItemKey::Unkeyed => {
            let legacy = legacy_promotions(state)?;
            legacy.unknown
                || legacy.unkeyed
                || state
                    .store
                    .private_ref_prefixed_anywhere(hearthloom_adapters::mbox::UNKEYED_REF_PREFIX)?
        }
    })
}

/// Which of these keys of a message (Message-IDs as a MIME parser reads
/// them, or as the mail import keys them) has a memory labelled private,
/// under any source and any key a mail import can have given it: the key
/// itself, or a header's text around the id (`id_in_key`); a private
/// promotion an earlier version kept under a path counts by the Message-ID
/// its text names — and while one's message can't be identified, every key
/// counts.
pub(crate) fn private_mail_ids(
    state: &AppState,
    ids: &[String],
) -> Result<std::collections::HashSet<String>> {
    let mut out = std::collections::HashSet::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let legacy = legacy_promotions(state)?;
    if legacy.unknown {
        out.extend(ids.iter().cloned());
        return Ok(out);
    }
    out.extend(state.store.private_refs_anywhere(ids)?);
    for key in state
        .store
        .private_refs_not_bare()?
        .iter()
        .chain(legacy.keys.iter())
    {
        out.extend(ids.iter().filter(|id| id_in_key(key, id)).cloned());
    }
    Ok(out)
}

/// The paths a mail promotion was kept under before promotions kept the
/// message's own key (0.5.86): an archive copy's `at:<offset>`, a Gmail
/// `uid:` path. No reader gives a Message-ID either form, so such a
/// promotion is known by the text it stored, if at all.
const LEGACY_PROMOTION_PATHS: &[&str] = &["at:", "uid:"];

/// What the private promotions kept under a legacy path
/// (`LEGACY_PROMOTION_PATHS`) are memories of, under any source.
#[derive(Debug, Default)]
struct LegacyPromotions {
    /// The Message-IDs their stored text names, as the mail import keys
    /// them (`mbox::message_id_ref`) — an archive promotion stored the
    /// message as it is, headers first.
    keys: std::collections::HashSet<String>,
    /// Some stored a message that names none: counts as a private memory
    /// of a message the import keys by its text.
    unkeyed: bool,
    /// Some can't be identified: a Gmail promotion kept under its `uid:`
    /// path — Gmail's fetch took a uid for any message, with or without a
    /// Message-ID, and the promotion stored only the text shown, which
    /// names none — or one whose text can't be read. No message can be told
    /// apart from it, so every message counts as private.
    unknown: bool,
}

/// Read from the record alone — the stored text of each private promotion
/// — so it needs no archive, no mailbox and no registered source, and it
/// covers every such promotion. A memory's text never changes, so each is
/// read once per process (by its content hash).
fn legacy_promotions(state: &AppState) -> Result<LegacyPromotions> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static KEY_OF_TEXT: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    let known = KEY_OF_TEXT.get_or_init(Default::default);
    let unknown = || LegacyPromotions {
        unknown: true,
        ..Default::default()
    };
    let mut out = LegacyPromotions::default();
    for prefix in LEGACY_PROMOTION_PATHS {
        for memory in state.store.private_prefixed_anywhere(prefix)? {
            if crate::gmail_search::parse_uid_path(&memory.source_ref).is_some()
                && reads_gmail(state, &memory.source)?
            {
                return Ok(unknown());
            }
            let seen = known
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&memory.content_hash)
                .cloned();
            let key = match seen {
                Some(key) => key,
                None => match state
                    .store
                    .private_memory_text(&memory.source, &memory.source_ref)?
                {
                    Some(text) => {
                        let key = hearthloom_adapters::mbox::message_id_ref(&text);
                        known
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(memory.content_hash, key.clone());
                        key
                    }
                    // no text to read (relabelled since, or none stored):
                    // its message can't be identified (not remembered)
                    None => return Ok(unknown()),
                },
            };
            match key {
                Some(key) => {
                    out.keys.insert(key);
                }
                None => out.unkeyed = true,
            }
        }
    }
    Ok(out)
}

/// Whether a source reads Gmail: `gmail`, or another registered here with
/// Gmail's adapter.
fn reads_gmail(state: &AppState, source: &str) -> Result<bool> {
    Ok(source == crate::gmail_search::SOURCE
        || state
            .store
            .virtual_source(source)?
            .is_some_and(|def| def.adapter == crate::gmail_search::ADAPTER))
}

/// Whether `key` — what a mail import made of a Message-ID header
/// (`mbox::message_id_ref`: its value with the angle brackets at its ends
/// trimmed) — is the key of the message whose Message-ID a MIME parser read
/// as `id`. A header holding more than `<id>` (a comment before or after
/// it, folding whitespace) keeps the bracket on that side of the id.
fn id_in_key(key: &str, id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() {
        return false;
    }
    let key = key.trim();
    key == id
        || key.ends_with(&format!("<{id}"))
        || key.starts_with(&format!("{id}>"))
        || key.contains(&format!("<{id}>"))
}

/// Which of these paths became a memory labelled private under this
/// source's own name (a promotion kept under the path asked for). A
/// message's other keys are `mail_memory_private`'s.
fn marked_private(
    state: &AppState,
    def: &VirtualSourceDef,
    paths: &[String],
) -> Result<std::collections::HashSet<String>> {
    state.store.private_source_refs(&def.name, paths)
}

/// Is this item private for the vault (0.5.86)? The tests a memory meets:
/// the memory made from it is labelled private (by a rule or by hand), or
/// a privacy rule matches its text. Archives are read in place and never
/// pass through ingest, so until 0.5.86 neither test ran on them: mail a
/// rule makes private was searchable and readable through the archive
/// with the vault locked.
/// The memory is looked up by every key the item is known by: the path
/// asked for, under this source's name; and for a message the key an
/// import gives it (`key` — its Message-ID as the mail import reads it,
/// which a `uid:` path or an archive's `at:<offset>` copy doesn't carry),
/// under every source the record holds (`mail_memory_private`). A message
/// whose memory key can't be rebuilt (`ItemKey::Unkeyed`) counts as
/// private while any memory keyed that way is: the check can't tell it
/// isn't that one.
/// The rules read the whole item (`privacy`): for a message, every header
/// the import reads and all of its text, decoded. A message that can't be
/// read whole counts as private while any rule exists.
fn item_private(
    state: &AppState,
    def: &VirtualSourceDef,
    path: &str,
    key: &ItemKey,
    privacy: &Privacy,
) -> Result<bool> {
    if !marked_private(state, def, &[path.to_string()])?.is_empty()
        || mail_memory_private(state, key)?
    {
        return Ok(true);
    }
    if state.store.list_privacy_rules()?.is_empty() {
        return Ok(false); // nothing to read the text for
    }
    match privacy_text(def, privacy) {
        Some(text) => state.store.privacy_rules_match(&text),
        None => Ok(true),
    }
}

/// The whole text the rules read, or None when it can't be read whole.
fn privacy_text(def: &VirtualSourceDef, privacy: &Privacy) -> Option<String> {
    match privacy {
        Privacy::Text(text) => Some(text.clone()),
        Privacy::Message { raw, labels } => {
            Some(format!("{}{labels}", crate::mbox_text::privacy_text(raw)))
        }
        Privacy::ReadAt(at) if def.adapter == crate::gmail_search::ADAPTER => {
            crate::gmail_search::read_whole(at)
        }
        Privacy::ReadAt(_) => None,
    }
}

/// The key a promotion of a message is stored under: the message's own
/// Message-ID as the mail import reads it, whichever path reached it (a
/// `uid:` path, an archive copy's `at:<offset>`), so the label it is given
/// holds for every path to the message; a message without one is keyed
/// the way the import keys it (`nomsgid:` and a hash of what was read).
/// Anything else keeps its path.
fn promotion_ref(path: &str, key: &ItemKey, content: &str) -> String {
    match key {
        ItemKey::Path => path.to_string(),
        ItemKey::Refs(refs) => refs.first().cloned().unwrap_or_else(|| path.to_string()),
        ItemKey::Unkeyed => format!(
            "{}{}",
            hearthloom_adapters::mbox::UNKEYED_REF_PREFIX,
            ids::content_hash(content.as_bytes())
        ),
    }
}

/// `search`, leaving out what the vault hides from this caller (the
/// thread's viewer: Ask's, the dashboard's). Nothing says how much was
/// left out. A mail hit is judged on the whole message, as at ingest.
pub fn search_for_viewer(
    state: &AppState,
    def: &VirtualSourceDef,
    query: &str,
    limit: usize,
) -> Result<Vec<VirtualHit>> {
    search_with(state, def, query, limit, state.store.vault_status().0)
}

/// `search_for_viewer` with the vault state given — for a lookup another
/// of the owner's nodes asked for (lookups.rs), which carries the asker's.
pub fn search_with(
    state: &AppState,
    def: &VirtualSourceDef,
    query: &str,
    limit: usize,
    locked: bool,
) -> Result<Vec<VirtualHit>> {
    if !locked {
        return search(def, query, limit);
    }
    // ask for more than needed: some may be held back
    let hits = search(def, query, limit.saturating_mul(2).max(limit))?;
    let refs: Vec<String> = hits.iter().map(|h| h.path.clone()).collect();
    let marked = marked_private(state, def, &refs)?;
    let mut kept = Vec::new();
    for hit in hits {
        if kept.len() >= limit {
            break;
        }
        if marked.contains(&hit.path) {
            continue;
        }
        let (privacy, key) = if let Some(privacy) = &hit.privacy {
            (privacy.clone(), hit.key.clone())
        } else if def.adapter == "mbox" {
            match fetch(def, &hit.path, None) {
                Ok(read) => (read.privacy, read.key),
                // a message that can't be read can't be checked: held back
                Err(_) => continue,
            }
        } else {
            (
                Privacy::Text(format!("{}\n{}", hit.path, hit.snippet)),
                ItemKey::Path,
            )
        };
        // the path is already checked above; this adds the item's own key
        if item_private(state, def, &hit.path, &key, &privacy)? {
            continue;
        }
        kept.push(hit);
    }
    Ok(kept)
}

/// `fetch`, refusing what the vault hides from this caller — with the
/// same words as an item that isn't there.
pub fn fetch_for_viewer(
    state: &AppState,
    def: &VirtualSourceDef,
    path: &str,
    budget: Option<u32>,
) -> Result<String> {
    fetch_with(state, def, path, budget, state.store.vault_status().0)
}

/// `fetch_for_viewer` with the vault state given (lookups.rs).
pub fn fetch_with(
    state: &AppState,
    def: &VirtualSourceDef,
    path: &str,
    budget: Option<u32>,
    locked: bool,
) -> Result<String> {
    Ok(fetch_checked(state, def, path, budget, locked)?.shown)
}

/// `fetch_with`, keeping the item's key (promotion stores it).
fn fetch_checked(
    state: &AppState,
    def: &VirtualSourceDef,
    path: &str,
    budget: Option<u32>,
    locked: bool,
) -> Result<Fetched> {
    let fetched = fetch(def, path, budget)?;
    shown_to(state, def, path, fetched, locked)
}

/// What `fetch_with` gives the caller for an item as fetched: the item,
/// or — while locked, for a private item — the same words as an item that
/// isn't there.
fn shown_to(
    state: &AppState,
    def: &VirtualSourceDef,
    path: &str,
    fetched: Fetched,
    locked: bool,
) -> Result<Fetched> {
    if locked && item_private(state, def, path, &fetched.key, &fetched.privacy)? {
        return Err(if def.adapter == "mbox" {
            anyhow!("no message {path:?} in the index")
        } else if def.adapter == crate::gmail_search::ADAPTER {
            anyhow!("no message {path:?} in Gmail")
        } else {
            anyhow!("nothing at {path:?}")
        });
    }
    Ok(fetched)
}

fn magic_search(mount: &Path, query: &str, limit: usize) -> Result<Vec<VirtualHit>> {
    // HearthFS exposes matches as ONE virtual markdown file at
    // `@search/<query>` (verified against the live mount 2026-07-11 —
    // not a directory): hits are `- **<path>** (score: …, ~N tokens)`
    // headers, each followed by snippet lines up to a `next:` read hint.
    // A `?n=` suffix is not supported upstream; the limit stays here.
    let file = mount.join("@search").join(query);
    let content =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let mut hits: Vec<VirtualHit> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // A hit header is `- **<path>** (score: …)` — the score suffix
        // matters: snippet text can contain plain `- **bold**` bullets.
        let header = trimmed.strip_prefix("- **").and_then(|rest| {
            let end = rest.find("**")?;
            rest[end + 2..]
                .trim_start()
                .starts_with("(score:")
                .then(|| rest[..end].to_string())
        });
        if let Some(path) = header {
            if hits.len() >= limit {
                break;
            }
            hits.push(VirtualHit {
                path,
                snippet: String::new(),
                privacy: None,
                key: ItemKey::Path,
            });
        } else if let Some(hit) = hits.last_mut() {
            if trimmed.is_empty() || trimmed.starts_with("next:") {
                continue;
            }
            if hit.snippet.chars().count() < SNIPPET_CHARS {
                if !hit.snippet.is_empty() {
                    hit.snippet.push(' ');
                }
                hit.snippet.push_str(trimmed);
            }
        }
    }
    for hit in &mut hits {
        hit.snippet = snippet_of(&hit.snippet);
    }
    Ok(hits)
}

fn fs_search(mount: &Path, query: &str, limit: usize) -> Result<Vec<VirtualHit>> {
    let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    let mut stack = vec![mount.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip dotfiles and magic-path directories.
            if name.starts_with('.') || name.starts_with('@') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            scanned += 1;
            if scanned > MAX_SCAN_FILES {
                return Ok(hits);
            }
            let rel = path
                .strip_prefix(mount)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let hay = format!("{}\n{}", rel.to_lowercase(), content.to_lowercase());
            if terms.is_empty() || terms.iter().all(|t| hay.contains(t)) {
                hits.push(VirtualHit {
                    path: rel,
                    snippet: snippet_of(&content),
                    privacy: None,
                    key: ItemKey::Path,
                });
                if hits.len() >= limit {
                    return Ok(hits);
                }
            }
        }
    }
    Ok(hits)
}

/// Read a file's content from a virtual source. On a HearthFS mount a
/// `budget` requests a token-fit view (`file@fit/<budget>`); on a plain
/// mount `budget` is ignored (the whole file, capped, is returned).
/// Returned with the key the item's memory would carry: for a message, its
/// Message-ID as the mail import reads it, which the path that reached it
/// need not be (a `uid:` path, an archive's `at:<offset>` duplicate copy,
/// another parser's spelling). Callers read through `fetch_with`, which
/// checks the vault on both — a message on the whole of it, decoded, not on
/// the raw (and cut) text shown.
fn fetch(def: &VirtualSourceDef, path: &str, budget: Option<u32>) -> Result<Fetched> {
    if def.adapter == crate::gmail_search::ADAPTER {
        return crate::gmail_search::fetch(path);
    }
    let mount = mount_of(def)?;
    require_online(def, &mount)?;
    if def.adapter == "apple-health" {
        bail!(
            "{} is a numeric source — use vquery (read-only SQL over its sidecar)",
            def.name
        );
    }
    if def.adapter == "mbox" {
        // `path` is a message id from a prior vsearch; the sidecar index
        // maps it to a byte range in the archive.
        let raw = crate::mbox_index::fetch_bytes(&mount, path)?;
        let text = String::from_utf8_lossy(&raw);
        let key = mail_key(&text, None);
        let shown = text.chars().take(MAX_FETCH_CHARS).collect();
        // the labels the live pull read again since (the archive keeps
        // the ones it arrived with)
        let labels = crate::mbox_index::labels_of(&mount, path)
            .ok()
            .flatten()
            .unwrap_or_default();
        return Ok(Fetched {
            shown,
            key,
            privacy: Privacy::Message { raw, labels },
        });
    }
    let target = match (uses_magic(def), budget) {
        (true, Some(budget)) => {
            let resolved = safe_join(&mount, path)?;
            let parent = resolved
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| mount.clone());
            let file = resolved
                .file_name()
                .and_then(|f| f.to_str())
                .ok_or_else(|| anyhow!("path {path:?} has no file component"))?;
            parent.join(format!("{file}@fit")).join(budget.to_string())
        }
        _ => safe_join(&mount, path)?,
    };
    let bytes = std::fs::read(&target).with_context(|| format!("fetching {}", target.display()))?;
    let text: String = String::from_utf8_lossy(&bytes)
        .chars()
        .take(MAX_FETCH_CHARS)
        .collect();
    Ok(Fetched {
        privacy: Privacy::Text(text.clone()),
        shown: text,
        key: ItemKey::Path,
    })
}

/// `vquery`: read-only SQL over a source's numeric sidecar. The sidecar
/// rows carry no per-row scope, so the gate is coarse and zero-leakage:
/// if ANY episode of this source is marked private, the whole handle
/// locks with the vault.
pub fn query(
    state: &AppState,
    name: &str,
    sql: &str,
    max_rows: usize,
) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
    let def = resolve(state, name)?;
    let sidecar = sidecar_of(&def)?;
    let (vault_locked, _) = state.store.vault_status();
    if vault_locked && state.store.source_has_private(&def.name)? {
        bail!(
            "{}'s numbers are vault-locked (the source contains private material)",
            def.name
        );
    }
    crate::health_index::query(&sidecar, sql, max_rows)
}

/// The sidecar backing vquery: explicit `config.sidecar`, else the
/// apple-health default (`<mount>/health.vidx.sqlite`).
fn sidecar_of(def: &VirtualSourceDef) -> Result<PathBuf> {
    if let Some(path) = def.config.get("sidecar").and_then(|s| s.as_str()) {
        return Ok(PathBuf::from(expand_tilde(path)));
    }
    Ok(mount_of(def)?.join("health.vidx.sqlite"))
}

/// Promote a virtual result into the substrate: fetch it, then materialize
/// (or quarantine, for untrusted origins) with provenance = origin +
/// fetch time. Idempotent on `(source, item)` — re-promoting is a no-op. A
/// message is stored under its own Message-ID (`promotion_ref`), whichever
/// path reached it, so its label covers every path to it.
pub fn promote(
    state: &AppState,
    actor: &Actor,
    name: &str,
    path: &str,
    kind: Option<&str>,
    scope: Option<&str>,
    budget: Option<u32>,
) -> Result<PromoteOutcome> {
    let def = resolve(state, name)?;
    // a message the vault hides from this caller can't be promoted by it
    let fetched = fetch_checked(state, &def, path, budget, state.store.vault_status().0)?;
    let source_ref = promotion_ref(path, &fetched.key, &fetched.shown);
    let content = fetched.shown;
    let trust = trust_of(&def);
    let is_mail = def.adapter == "mbox";
    let kind = kind
        .unwrap_or(if is_mail { "email" } else { "file" })
        .to_string();
    let (title, event_time) = if is_mail {
        email_meta(&content, path)
    } else {
        (title_of(path, &content), EventTime::unknown())
    };

    if trust == "untrusted" {
        // Quarantine-first. Carry the full draft so the human gate can
        // promote it *with* provenance, not as a generic "memory" episode
        // (see resolve_quarantine in http.rs, which honors these fields).
        let entry = json!({
            "kind": kind,
            "text": content,
            "title": title,
            "source": def.name,
            "source_ref": source_ref,
            "trust": "untrusted",
            "promoted_from": {"virtual_source": def.name, "origin": path},
        });
        let ids = state.store.propose_memories(actor, &[entry])?;
        return Ok(PromoteOutcome {
            episode_id: None,
            quarantined: ids.into_iter().next(),
            existing: false,
        });
    }

    let batch = IngestBatch {
        actor: actor.actor.clone(),
        session_id: actor.session_id.clone(),
        episodes: vec![EpisodeDraft {
            source: def.name.clone(),
            source_ref: source_ref.clone(),
            kind,
            title,
            body: content,
            event_time,
            trust,
            scope: scope.map(String::from),
            entities: vec![],
            edges: vec![],
        }],
    };
    let report = state.store.apply_batch(&state.home, &batch)?;
    Ok(PromoteOutcome {
        episode_id: Some(ids::episode_id(&def.name, &source_ref)),
        quarantined: None,
        existing: report.episodes_new == 0 && report.episodes_existing > 0,
    })
}

/// The RLM system-prompt fragment describing available virtual sources and
/// their read handles — None when none are registered (base experience
/// unchanged; the system stays degraded-but-useful without them).
/// The mailboxes a `gmail` source searches: `accounts` (0.5.98), else the
/// one `account` older nodes name.
fn gmail_accounts(config: &serde_json::Value) -> Vec<String> {
    match config["accounts"].as_array() {
        Some(list) => list
            .iter()
            .filter_map(|a| a.as_str().map(String::from))
            .collect(),
        None => config["account"]
            .as_str()
            .map(|a| vec![a.to_string()])
            .unwrap_or_default(),
    }
}

/// The live mailbox's line in Ask's instructions (0.5.89; several
/// mailboxes 0.5.98).
fn gmail_line(accounts: &[String], node: Option<&str>, reachable: bool) -> String {
    format!(
        "- gmail [gmail]{} — the WHOLE Gmail mailbox{}, searched live with Gmail's own search: \
         every message the account holds, old and new{}\n",
        node.map(|n| format!(" on node \"{n}\""))
            .unwrap_or_default(),
        match accounts {
            [] => String::new(),
            [one] => format!(" of {one}"),
            several => format!(
                "es {} (all searched at once; each hit ends `in <mailbox>`)",
                several.join(", ")
            ),
        },
        if reachable {
            ""
        } else {
            " — UNAVAILABLE right now: don't call it; say so if it mattered"
        }
    )
}

/// How Ask should look for things in mail (0.5.86; message text 0.5.87).
const MAIL_GUIDANCE: &str = "\
Finding things in mail: when a `gmail` source is listed, search it FIRST — it is the whole \
mailbox (every one the user connected), current to the minute. Its vsearch takes Gmail search syntax: words anywhere in the \
message, from:, to:, subject:, after:YYYY/MM/DD, before:, has:attachment, label:, \
category:, OR, quotes for a phrase; results come newest first (up to 25), so narrow with \
operators rather than paging; each hit ends with the start of its text, and vfetch reads one \
message's text and links. The archives are the fallback when Gmail can't be reached. The \
user's Sent, Primary and Starred mail is ALSO in the record as \
email episodes, and `search` finds words in their text. Everything else (Updates, Social, \
Promotions, Forums) is only in Gmail and the mail archives above. vsearch over an archive matches \
sender, recipients, subject, labels and date, and also the message text and the web \
addresses it links to where the archive's line says its text is searchable (such hits end \
`text: …` with the words that matched). Where the text isn't searchable yet, a link, an \
account or a portal is found by vfetching the most likely messages and reading them. \
Invitations and notifications come from the SERVICE (noreply@…, a company's name), not from \
the person who set it up: search for words like invited, invitation, shared, secure, portal, \
access, welcome, account, link, login — as well as the person's name. Run independent \
vsearch calls together in one turn; vfetch only the best candidates.\n";

/// "; its message text and links are searchable" — or how far the
/// background pass has got (in tens of percent, so the instructions don't
/// change with every batch).
fn text_status_line(progress: Option<(usize, usize)>) -> String {
    match progress {
        Some((done, total)) if total > 0 && done * 100 >= total * 99 => {
            "; its message text and links are searchable".into()
        }
        Some((done, total)) if total > 0 && done * 10 >= total => format!(
            "; message text searchable for about {}% so far (headers for all)",
            done * 10 / total * 10
        ),
        _ => "; headers only for now (its text is being indexed)".into(),
    }
}

/// " — about 19,500 messages, 2026-08 → 2026-11" (rounded so the
/// instructions don't change with every new message).
fn archive_summary_line(s: &crate::mbox_index::Summary) -> String {
    let rounded = |n: usize| -> String {
        let n = if n >= 1000 { (n + 50) / 100 * 100 } else { n };
        let digits = n.to_string();
        let mut out = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out
    };
    let span = match (&s.first, &s.last) {
        (Some(a), Some(b)) => format!(", {a} → {b}"),
        _ => String::new(),
    };
    format!(" — about {} messages{span}", rounded(s.messages))
}

pub fn rlm_prompt_section(state: &AppState) -> Option<String> {
    let mut sources = state.store.list_virtual_sources().ok()?;
    sources.retain(is_enabled); // a disabled integration shouldn't nag the prompt
    let elsewhere = crate::lookups::archives_elsewhere(state);
    if sources.is_empty() && elsewhere.is_empty() {
        return None;
    }
    let mut out = String::from(
        "\n\nVirtual sources (data kept at origin, queried at need — NOT in the substrate \
         until promoted; their results have no episode id, so never put a vfetch/vsearch \
         path in \"citations\"):\n",
    );
    for source in &sources {
        if source.adapter == crate::gmail_search::ADAPTER {
            out.push_str(&gmail_line(
                &gmail_accounts(&source.config),
                None,
                !crate::gmail_search::accounts().is_empty(),
            ));
            continue;
        }
        let mount = source
            .config
            .get("mount")
            .and_then(|m| m.as_str())
            .unwrap_or("?");
        let online = mount_of(source).map(|m| mount_alive(&m)).unwrap_or(false);
        // what a mail archive holds, so Ask knows which to search (0.5.86);
        // counts are rounded so the instructions stay the same between asks
        let holds = if online && source.adapter == "mbox" {
            mount_of(source)
                .ok()
                .map(|m| {
                    let summary = crate::mbox_index::summary(&m)
                        .map(|s| archive_summary_line(&s))
                        .unwrap_or_default();
                    format!(
                        "{summary}{}",
                        if crate::mbox_text::is_off(&m) {
                            "; headers only (its text isn't indexed on this node)".to_string()
                        } else {
                            text_status_line(crate::mbox_text::progress(&m))
                        }
                    )
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        out.push_str(&format!(
            "- {} [{}] at {}{}{}\n",
            source.name,
            source.adapter,
            mount,
            holds,
            if online {
                ""
            } else {
                " — OFFLINE right now (mount not present): do NOT call vsearch/vfetch \
                 on it; if it would have been relevant, say it is unavailable"
            }
        ));
    }
    // archives your other nodes hold (0.5.88): reached through the hub
    if !elsewhere.is_empty() {
        out.push_str(
            "Mail archives on your OTHER nodes — add \"node\": \"<node>\" to vsearch/vfetch \
             (each lookup goes through your hub to that node and takes a few seconds):\n",
        );
        for (_, node, seen, archive) in &elsewhere {
            if archive["adapter"] == crate::gmail_search::ADAPTER {
                out.push_str(&gmail_line(
                    &gmail_accounts(archive),
                    Some(node),
                    crate::lookups::recently_seen(seen),
                ));
                continue;
            }
            let summary = crate::mbox_index::Summary {
                messages: archive["messages"].as_u64().unwrap_or(0) as usize,
                first: archive["first"].as_str().map(String::from),
                last: archive["last"].as_str().map(String::from),
            };
            let text = match archive["text"].as_str() {
                Some("on") => "; its message text and links are searchable".to_string(),
                Some("off") | None => "; headers only (its text isn't indexed there)".to_string(),
                Some(p) => format!("; message text searchable for {p} so far"),
            };
            out.push_str(&format!(
                "- {} [{}] on node \"{node}\"{}{text}{}\n",
                archive["name"].as_str().unwrap_or("?"),
                archive["adapter"].as_str().unwrap_or("mbox"),
                if summary.messages > 0 {
                    archive_summary_line(&summary)
                } else {
                    String::new()
                },
                if crate::lookups::recently_seen(seen) {
                    ""
                } else {
                    " — that node hasn't been seen lately: don't call it; say it's unavailable if it mattered"
                }
            ));
        }
    }
    if sources.iter().any(|s| s.adapter == "mbox") || !elsewhere.is_empty() {
        out.push_str(MAIL_GUIDANCE);
    }
    let has_numeric = sources.iter().any(|s| s.adapter == "apple-health");
    out.push_str(
        "Extra READ handles for these:\n\
         - vsearch {source, query, limit?, node?}   search a virtual source; each hit's path is \
         fetchable (for mbox mail archives, hits are message ids; see below for what matches)\n\
         - vfetch {source, path, budget?, node?}    read one result's content (budget = token-fit view)\n\
         (node: only for an archive listed on another node)\n",
    );
    if has_numeric {
        out.push_str(
            "- vquery {source, sql}              read-only SELECT/WITH over a numeric sidecar. \
             apple-health tables: daily(day,metric,source,n,sum,min,max,avg,unit) — one row per \
             day/metric/source, every row of a metric in the same unit, PREFER this for \
             trends/comparisons; \
             samples(day,metric,source,value,unit,start) full resolution. Metrics include \
             StepCount, ActiveEnergyBurned, AppleExerciseTime, HeartRate, RestingHeartRate, \
             HeartRateVariabilitySDNN, RespiratoryRate, OxygenSaturation, VO2Max, BodyMass, \
             BloodGlucose, MindfulSession (hours), Sleep/AsleepDeep|AsleepREM|AsleepCore|InBed \
             (hours). Sources overlap (Apple Watch, WHOOP, Withings, Oura, Signos…) — filter or \
             group by source, never sum across them. Gaps in coverage are themselves signal.\n",
        );
    }
    out.push_str(
        "\
         If a virtual result is worth keeping, tell the user it can be promoted (materialized \
         into an episode) — you cannot promote from within this loop.",
    );
    Some(out)
}

fn snippet_of(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(SNIPPET_CHARS)
        .collect()
}

/// Subject/Date-derived metadata for a promoted email; falls back to a
/// path-based title and unknown time when the headers are absent. The
/// first Subject/Date wins, as in mbox_index — a sender's repeat lower
/// in the header block doesn't retitle or redate what gets promoted.
fn email_meta(content: &str, path: &str) -> (String, EventTime) {
    let mut subject = None;
    let mut date = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "subject" if subject.is_none() => subject = Some(value.trim().to_string()),
                "date" if date.is_none() => date = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    let time = date
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(&d).ok())
        .map(|dt| EventTime::from_fixed(dt, hearthloom_core::TimeUncertainty::Exact))
        .unwrap_or_else(EventTime::unknown);
    (subject.unwrap_or_else(|| format!("mail {path}")), time)
}

fn title_of(path: &str, content: &str) -> String {
    let base = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    if !base.is_empty() {
        return base.to_string();
    }
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(untitled)")
        .chars()
        .take(80)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::{Home, Store};
    use std::sync::Arc;

    /// The exfil-guard invariant (encryption regime I.3): no RLM read
    /// handle may reach outside its mount — every fetch path goes
    /// through safe_join, which must reject traversal, absolute paths
    /// and prefix components. If this test breaks, an Ask query can be
    /// tricked into reading arbitrary files (e.g. a sealed-secrets
    /// master key file, an SSH key) — never weaken it.
    #[test]
    fn test_safe_join_contains_every_escape() {
        let mount = Path::new("/tmp/mount");
        assert!(safe_join(mount, "notes/a.md").is_ok());
        assert!(safe_join(mount, "/leading/slash.md").is_ok()); // stripped, stays inside
        for escape in [
            "../outside.txt",
            "a/../../outside.txt",
            "..",
            "a/./../../b",
            "/../etc/passwd",
        ] {
            assert!(
                safe_join(mount, escape).is_err(),
                "{escape:?} must be refused"
            );
        }
        // Absolute-path smuggling: joining must never yield a path
        // outside the mount even when the input starts absolute.
        let joined = safe_join(mount, "/etc/passwd");
        if let Ok(p) = joined {
            assert!(p.starts_with(mount), "{p:?} escaped the mount");
        }
    }

    fn state_with(mount: &Path, trust: &str) -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        store
            .register_virtual_source(
                &Actor::new("human:test"),
                "hearthfs",
                "hearthfs",
                &json!({"mount": mount.to_string_lossy(), "magic": false, "trust": trust}),
                &["search".into(), "fetch".into(), "promote".into()],
            )
            .unwrap();
        let state = AppState {
            store: Arc::new(store),
            home,
            embedder: None,
            llm: None,
            auth_locked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        (state, tmp)
    }

    /// A plain directory mount we can drive without a live FSKit mount.
    fn fixture_mount() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.md"),
            "# Auth flows\nWe use OAuth device code grant for the CLI.",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("notes.txt"),
            "Grocery list: milk, eggs, coffee beans.",
        )
        .unwrap();
        dir
    }

    #[test]
    fn test_search_matches_filename_and_content() {
        let mount = fixture_mount();
        let (state, _tmp) = state_with(mount.path(), "imported");
        let def = resolve(&state, "hearthfs").unwrap();

        let hits = search(&def, "oauth device", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "auth.md");
        assert!(hits[0].snippet.contains("OAuth"));

        // No match → empty, not an error.
        assert!(search(&def, "kubernetes", 10).unwrap().is_empty());
    }

    #[test]
    fn test_magic_search_parses_results_file() {
        // The `@search/<query>` virtual FILE format the live mount emits.
        let mount = tempfile::tempdir().unwrap();
        std::fs::create_dir(mount.path().join("@search")).unwrap();
        std::fs::write(
            mount.path().join("@search").join("oauth"),
            "# Search: oauth\n\n\
             - **guides/auth.md** (score: 0.0164, ~3217 tokens)\n  \
             ...OAuth device code grant for the CLI...\n  \
             - **Bold bullet:** inside a snippet is NOT a hit header\n  \
             next: cat 'guides/auth.md@outline' | 'guides/auth.md@fit/2000'\n\n\
             - **notes.txt** (score: 0.0101, ~120 tokens)\n  \
             token refresh cadence\n  \
             next: cat 'notes.txt@outline'\n",
        )
        .unwrap();
        let hits = magic_search(mount.path(), "oauth", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "guides/auth.md");
        assert!(hits[0].snippet.contains("OAuth device code"));
        assert!(!hits[0].snippet.contains("next:"));
        assert_eq!(hits[1].path, "notes.txt");
        // the limit is enforced client-side (no `?n=` upstream)
        assert_eq!(magic_search(mount.path(), "oauth", 1).unwrap().len(), 1);
    }

    #[test]
    fn test_offline_mount_errors_cleanly() {
        let (state, _tmp) = state_with(Path::new("/no/such/mount/xyz"), "imported");
        let def = resolve(&state, "hearthfs").unwrap();
        let err = search(&def, "anything", 10).unwrap_err().to_string();
        assert!(err.contains("offline"), "got: {err}");
    }

    #[test]
    fn test_promote_trusted_materializes_with_provenance() {
        let mount = fixture_mount();
        let (state, _tmp) = state_with(mount.path(), "imported");
        let actor = Actor::new("human:test");

        let outcome = promote(&state, &actor, "hearthfs", "auth.md", None, None, None).unwrap();
        let episode_id = outcome.episode_id.expect("materialized");
        assert!(outcome.quarantined.is_none());

        let episode = state.store.episode(&episode_id).unwrap().unwrap();
        assert_eq!(episode.source, "hearthfs"); // provenance: origin source
        assert_eq!(episode.trust, "imported");
        let body = state.store.episode_body(&episode_id).unwrap().unwrap();
        assert!(body.contains("OAuth"));

        // Re-promote is idempotent.
        let again = promote(&state, &actor, "hearthfs", "auth.md", None, None, None).unwrap();
        assert!(again.existing);
    }

    #[test]
    fn test_promote_untrusted_quarantines_first() {
        let mount = fixture_mount();
        let (state, _tmp) = state_with(mount.path(), "untrusted");
        let actor = Actor::new("agent:test");

        let outcome = promote(&state, &actor, "hearthfs", "notes.txt", None, None, None).unwrap();
        assert!(outcome.episode_id.is_none());
        assert!(outcome.quarantined.is_some(), "untrusted → quarantine");

        let pending = state.store.quarantine_pending(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].content["source"], "hearthfs");
    }

    #[test]
    fn test_prompt_section_lists_registered_sources() {
        let mount = fixture_mount();
        let (state, _tmp) = state_with(mount.path(), "imported");
        let section = rlm_prompt_section(&state).unwrap();
        assert!(section.contains("hearthfs"));
        assert!(section.contains("vsearch"));
        assert!(section.contains("vfetch"));
        assert!(
            !section.contains("OFFLINE"),
            "live mount must not be flagged"
        );
    }

    #[test]
    fn test_prompt_section_flags_offline_mounts() {
        let (state, _tmp) = state_with(Path::new("/no/such/mount/xyz"), "imported");
        let section = rlm_prompt_section(&state).unwrap();
        assert!(section.contains("OFFLINE"), "dead mount must be flagged");
    }

    #[test]
    fn test_vquery_reads_sidecar_and_locks_with_vault() {
        use hearthloom_adapters::apple_health::{DayTable, Sample};

        // A mount dir with a sidecar in it.
        let mount = tempfile::tempdir().unwrap();
        let xml = mount.path().join("export.xml");
        std::fs::write(&xml, "<HealthData/>").unwrap();
        let mut table = DayTable::default();
        let mut writer = crate::health_index::SidecarWriter::create(&xml).unwrap();
        let s = Sample {
            metric: "RestingHeartRate".into(),
            value: Some(52.0),
            category: None,
            unit: Some("count/min".into()),
            source: "WHOOP".into(),
            device: "WHOOP".into(),
            start: "2026-04-02 08:00:00 -0400".into(),
            day: "2026-04-02".into(),
        };
        writer.add_sample(&s).unwrap();
        table.add(&s);
        writer.finish(&table).unwrap();

        let home_tmp = tempfile::tempdir().unwrap();
        let home = Home::at(home_tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        store
            .register_virtual_source(
                &Actor::new("human:test"),
                "apple-health",
                "apple-health",
                &json!({"mount": mount.path().to_string_lossy(), "trust": "imported"}),
                &["query".into()],
            )
            .unwrap();
        let state = AppState {
            store: Arc::new(store),
            home: home.clone(),
            embedder: None,
            llm: None,
            auth_locked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };

        // No private health episodes: vquery works even vault-locked.
        let (cols, rows) = query(&state, "apple-health", "SELECT day, avg FROM daily", 10).unwrap();
        assert_eq!(cols, vec!["day", "avg"]);
        assert_eq!(rows.len(), 1);

        // Guardrails pass through.
        assert!(query(&state, "apple-health", "DELETE FROM daily", 10).is_err());

        // vsearch/vfetch point at vquery instead.
        let def = resolve(&state, "apple-health").unwrap();
        assert!(search(&def, "x", 5)
            .unwrap_err()
            .to_string()
            .contains("vquery"));

        // One private episode from this source → the handle locks with
        // the vault, and unlocks with it.
        let batch = hearthloom_core::IngestBatch {
            actor: "human:test".into(),
            session_id: None,
            episodes: vec![hearthloom_core::EpisodeDraft {
                source: "apple-health".into(),
                source_ref: "day:2026-04-02".into(),
                kind: "health_day".into(),
                title: "Health: 2026-04-02".into(),
                body: "rhr 52".into(),
                event_time: hearthloom_core::EventTime::unknown(),
                trust: "imported".into(),
                scope: Some("private".into()),
                entities: vec![],
                edges: vec![],
            }],
        };
        state.store.apply_batch(&home, &batch).unwrap();
        let err = query(&state, "apple-health", "SELECT * FROM daily", 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("vault-locked"), "got: {err}");
        state.store.vault_unlock(15);
        assert!(query(&state, "apple-health", "SELECT * FROM daily", 10).is_ok());
    }

    /// 0.5.86: archive mail meets the vault like a memory does — held back
    /// while locked when a privacy rule matches it (its text included) or
    /// when the memory it became is labelled private; all of it with the
    /// vault open. A held-back message reads as not there.
    #[test]
    fn test_archive_mail_honours_privacy_rules_and_the_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        let message = |id: &str, subject: &str, body: &str| {
            format!(
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: {subject}\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\
                 X-Gmail-Labels: Inbox\n\n{body}\n\n"
            )
        };
        std::fs::write(
            &mbox,
            [
                message(
                    "sail1@x",
                    "plans for saturday",
                    "The regatta is on the 12th.",
                ),
                message(
                    "deal1@x",
                    "plans for the numbers",
                    "Draft from the mediation session.",
                ),
                message("lunch1@x", "plans for lunch", "Noon at the usual place."),
            ]
            .concat(),
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let home_tmp = tempfile::tempdir().unwrap();
        let home = Home::at(home_tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let actor = Actor::new("human:test");
        store
            .register_virtual_source(
                &actor,
                "gmail-archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into(), "promote".into()],
            )
            .unwrap();
        // a rule over the text, and a memory of one message marked private
        store.add_privacy_rule(&actor, "term", "mediation").unwrap();
        let single = tmp.path().join("one.mbox");
        std::fs::write(
            &single,
            message("lunch1@x", "plans for lunch", "Noon at the usual place."),
        )
        .unwrap();
        let drafts = hearthloom_adapters::mbox::ingest(&single, Some("private")).unwrap();
        store
            .apply_batch(
                &home,
                &hearthloom_core::IngestBatch {
                    actor: "adapter:mbox".into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap();
        let state = AppState {
            store: Arc::new(store),
            home,
            embedder: None,
            llm: None,
            auth_locked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        let def = resolve(&state, "gmail-archive").unwrap();
        let paths = |hits: Vec<VirtualHit>| {
            let mut p: Vec<String> = hits.into_iter().map(|h| h.path).collect();
            p.sort();
            p
        };

        // locked: only the plain message
        assert_eq!(
            paths(search_for_viewer(&state, &def, "plans", 10).unwrap()),
            ["sail1@x"]
        );
        for held in ["deal1@x", "lunch1@x"] {
            let err = fetch_for_viewer(&state, &def, held, None)
                .unwrap_err()
                .to_string();
            assert_eq!(
                err,
                format!("no message {held:?} in the index"),
                "reads as not there"
            );
            assert!(promote(&state, &actor, "gmail-archive", held, None, None, None).is_err());
        }
        assert!(fetch_for_viewer(&state, &def, "sail1@x", None)
            .unwrap()
            .contains("regatta"));

        // open: all of it
        state.store.vault_unlock(15);
        assert_eq!(
            paths(search_for_viewer(&state, &def, "plans", 10).unwrap()),
            ["deal1@x", "lunch1@x", "sail1@x"]
        );
        assert!(fetch_for_viewer(&state, &def, "deal1@x", None)
            .unwrap()
            .contains("mediation"));
    }

    /// Land messages as private memories the way an import does.
    fn import_private(state: &AppState, drafts: Vec<EpisodeDraft>) {
        state
            .store
            .apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: "adapter:mbox".into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap();
    }

    /// The key a mail import gives a Message-ID header is found from the
    /// id a MIME parser reads in the same header, however the header is
    /// written — and isn't taken for another message's.
    #[test]
    fn test_a_parsed_message_id_finds_the_imports_key() {
        for header in [
            "Message-ID: <plain@x>",
            "Message-ID: (desk) <hush@x>",
            "Message-ID: <after@x> (trailing)",
            "Message-ID: (a) <both@x> (b)",
            "Message-ID:\r\n <folded@x>",
            "Message-ID: < spaced@x >",
        ] {
            let raw = format!(
                "From: Ann <ann@x.example>\r\nSubject: s\r\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\r\n{header}\r\n\r\nbody\r\n"
            );
            let key = hearthloom_adapters::mbox::message_id_ref(&raw).unwrap();
            let parsed = mail_parser::MessageParser::default()
                .parse(raw.as_bytes())
                .unwrap()
                .message_id()
                .unwrap()
                .to_string();
            assert!(id_in_key(&key, &parsed), "{header}: {key:?} / {parsed:?}");
        }
        assert!(!id_in_key("(desk) <hush@x.example", "hush@x"));
        assert!(!id_in_key("ahush@x", "hush@x"));
        assert!(!id_in_key("anything", ""));
    }

    /// An archive message whose memory is private stays held back while
    /// locked whatever path reaches it: its Message-ID, or the
    /// `at:<offset>` of a duplicate copy — judged by the import's own key.
    /// A message without a Message-ID is keyed by a hash of the copy the
    /// import read, which no read rebuilds: while any such memory is
    /// private, every one of them is held back.
    #[test]
    fn test_a_private_archive_message_is_held_back_by_its_import_key() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        let message = |id: Option<&str>, subject: &str, body: &str| {
            format!(
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: {subject}\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\n{}X-Gmail-Labels: Inbox\n\n{body}\n\n",
                id.map(|id| format!("Message-ID: {id}\n"))
                    .unwrap_or_default()
            )
        };
        let secret = message(
            Some("(desk) <hush1@x>"),
            "plans for the clinic",
            "Results are in.",
        );
        let unkeyed = message(None, "plans for the picnic", "Bring the blanket.");
        std::fs::write(
            &mbox,
            [
                message(
                    Some("<sail1@x>"),
                    "plans for saturday",
                    "The regatta is on the 12th.",
                ),
                secret.clone(),
                secret.clone(), // a second copy: indexed as at:<offset>
                unkeyed.clone(),
            ]
            .concat(),
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let at = |q: &str| {
            crate::mbox_index::search(&mbox, q, 10)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .find(|id| id.starts_with("at:"))
                .unwrap()
        };
        let (copy, picnic) = (at("clinic"), at("picnic"));

        let (state, _home) = state_with(tmp.path(), "imported");
        state
            .store
            .register_virtual_source(
                &Actor::new("human:test"),
                "gmail-archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into()],
            )
            .unwrap();
        let def = resolve(&state, "gmail-archive").unwrap();
        let ingest = |raw: &str| {
            let one = tmp.path().join("one.mbox");
            std::fs::write(&one, raw).unwrap();
            hearthloom_adapters::mbox::ingest(&one, Some("private")).unwrap()
        };
        let found = |state: &AppState| -> Vec<String> {
            let mut p: Vec<String> = search_for_viewer(state, &def, "plans", 10)
                .unwrap()
                .into_iter()
                .map(|h| h.path)
                .collect();
            p.sort();
            p
        };
        let held = |path: &str| {
            fetch_for_viewer(&state, &def, path, None)
                .unwrap_err()
                .to_string()
                == format!("no message {path:?} in the index")
        };

        // the owner made the clinic message's memory private
        import_private(&state, ingest(&secret));
        let mut expect = vec![picnic.clone(), "sail1@x".to_string()];
        expect.sort();
        assert_eq!(found(&state), expect);
        assert!(held("(desk) <hush1@x"));
        assert!(held(&copy));
        assert!(fetch_for_viewer(&state, &def, &picnic, None).is_ok());

        // and a message without a Message-ID: its copy here can't be told
        // apart from it, so it is held back too
        import_private(&state, ingest(&unkeyed));
        assert_eq!(found(&state), ["sail1@x"]);
        assert!(held(&picnic));
        assert!(fetch_for_viewer(&state, &def, "sail1@x", None).is_ok());

        // open: all of it
        state.store.vault_unlock(15);
        assert_eq!(found(&state).len(), 4);
        for path in [copy.as_str(), picnic.as_str()] {
            assert!(fetch_for_viewer(&state, &def, path, None).is_ok());
        }
    }

    /// A Gmail message whose memory the live pull made private is held back
    /// while locked, whatever path reaches it — a `uid:` path, or another
    /// parser's spelling of its Message-ID — and a message without a
    /// Message-ID while any memory keyed that way is private.
    #[test]
    fn test_a_private_gmail_message_is_held_back_by_its_import_key() {
        use hearthloom_adapters::gmail_imap::{message_text, PulledMessage};
        let tmp = tempfile::tempdir().unwrap();
        let (state, _home) = state_with(tmp.path(), "imported");
        let raw = |id: Option<&str>, subject: &str| {
            format!(
                "From: Ann <ann@x.example>\r\nSubject: {subject}\r\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\r\n{}\r\nSee you then.\r\n",
                id.map(|id| format!("Message-ID: {id}\r\n"))
                    .unwrap_or_default()
            )
            .into_bytes()
        };
        // what the live pull materializes (gmail_pull.rs)
        let pulled = |raw: &[u8]| {
            let text = message_text(&PulledMessage {
                uid: 7,
                labels: vec!["Inbox".into()],
                raw: raw.to_vec(),
            });
            hearthloom_adapters::mbox::draft_from_message_filtered(
                &text,
                &Default::default(),
                Some("private"),
            )
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>()
        };
        let shown = |raw: &[u8], path: &str, locked: bool| gmail_read(&state, raw, path, locked);
        let held = |raw: &[u8], path: &str| gmail_held(&state, raw, path);

        let clinic = raw(Some("(desk) <hush2@x>"), "the clinic");
        let sail = raw(Some("<sail2@x>"), "saturday");
        let bare = raw(None, "the picnic");
        let other_bare = raw(None, "the parade");
        import_private(&state, pulled(&clinic));
        assert!(held(&clinic, "uid:48211"));
        assert!(held(&clinic, "hush2@x"), "the path another parser reads");
        assert!(shown(&sail, "uid:48212", true).is_ok());
        assert!(shown(&bare, "uid:48213", true).is_ok());

        import_private(&state, pulled(&bare));
        assert!(held(&bare, "uid:48213"));
        assert!(held(&other_bare, "uid:48214"), "can't be told apart");
        assert!(shown(&sail, "uid:48212", true).is_ok());

        // open
        for (raw, path) in [(&clinic, "uid:48211"), (&bare, "uid:48213")] {
            assert!(shown(raw, path, false).is_ok());
        }
    }

    fn gmail_def() -> VirtualSourceDef {
        VirtualSourceDef {
            name: crate::gmail_search::SOURCE.into(),
            adapter: crate::gmail_search::ADAPTER.into(),
            config: json!({}),
            capabilities: vec!["search".into(), "fetch".into()],
        }
    }

    /// What vfetch on the `gmail` source gives for this raw message reached
    /// by `path` (its uid read from a `uid:` path), read whole.
    fn gmail_read(state: &AppState, raw: &[u8], path: &str, locked: bool) -> Result<String> {
        let uid = path
            .strip_prefix("uid:")
            .and_then(|u| u.parse().ok())
            .unwrap_or(1);
        gmail_read_uid(state, raw, uid, path, locked)
    }

    /// `gmail_read` of the message with this uid.
    fn gmail_read_uid(
        state: &AppState,
        raw: &[u8],
        uid: u32,
        path: &str,
        locked: bool,
    ) -> Result<String> {
        let message = hearthloom_adapters::gmail_imap::FetchedMessage {
            uid,
            labels: vec![],
            raw: raw.to_vec(),
        };
        let fetched = crate::gmail_search::as_text(&message, None, usize::MAX);
        shown_to(state, &gmail_def(), path, fetched, locked).map(|f| f.shown)
    }

    fn gmail_held(state: &AppState, raw: &[u8], path: &str) -> bool {
        gmail_read(state, raw, path, true)
            .is_err_and(|e| e.to_string() == format!("no message {path:?} in Gmail"))
    }

    /// The privacy rules read the whole Gmail message, decoded — every
    /// address header and all of its text — not the part vfetch shows or
    /// the start a search reads. A message that can't be read whole counts
    /// as private while a rule exists.
    #[test]
    fn test_gmail_privacy_rules_read_the_whole_message() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _home) = state_with(tmp.path(), "imported");
        let actor = Actor::new("human:test");
        let message = |id: &str, cc: &str, body: &str| {
            format!(
                "From: Ann <ann@x.example>\r\nTo: Jon <jon@x.example>\r\n{cc}Subject: notes\r\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\r\nMessage-ID: <{id}>\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
            )
            .into_bytes()
        };
        let filler = "the weekly notes go on at some length. ".repeat(600);
        assert!(filler.len() > 20_000);
        let late = message("late@x", "", &format!("{filler}Then the mediation."));
        let copied = message(
            "copied@x",
            "Cc: Counsel <counsel@firm.example>\r\n",
            "See below.",
        );
        let plain = message("plain@x", "", "Lunch at noon.");
        let cut_at = Privacy::ReadAt("uid:nosuchbox:5".into());
        let gmail = gmail_def();

        // no rules: nothing to read the text for
        assert!(!item_private(&state, &gmail, "x@x", &ItemKey::Path, &cut_at).unwrap());

        state
            .store
            .add_privacy_rule(&actor, "term", "mediation")
            .unwrap();
        state
            .store
            .add_privacy_rule(&actor, "term", "counsel@firm.example")
            .unwrap();
        // what vfetch shows stops before the term …
        let shown = gmail_read(&state, &late, "late@x", false).unwrap();
        assert!(!shown.contains("mediation"));
        // … and the message is held back all the same
        assert!(gmail_held(&state, &late, "late@x"));
        assert!(gmail_held(&state, &copied, "copied@x"), "a Cc only");
        assert!(gmail_read(&state, &plain, "plain@x", true).is_ok());
        // a search hit judged on the whole of what it read
        let whole = |raw: &[u8]| Privacy::Message {
            raw: raw.to_vec(),
            labels: String::new(),
        };
        assert!(item_private(&state, &gmail, "late@x", &ItemKey::Path, &whole(&late)).unwrap());
        assert!(!item_private(&state, &gmail, "plain@x", &ItemKey::Path, &whole(&plain)).unwrap());
        // a label the import would show counts too
        let labelled = Privacy::Message {
            raw: plain.clone(),
            labels: "Inbox, Mediation".into(),
        };
        assert!(item_private(&state, &gmail, "plain@x", &ItemKey::Path, &labelled).unwrap());
        // a message that can't be read whole
        assert!(item_private(&state, &gmail, "x@x", &ItemKey::Path, &cut_at).unwrap());

        // open
        assert!(gmail_read(&state, &late, "late@x", false).is_ok());
    }

    /// A term rule matches a message whose text breaks the term across a
    /// line or spaces it out, as it matches the text a reader is shown.
    #[test]
    fn test_a_rule_matches_across_a_line_break() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _home) = state_with(tmp.path(), "imported");
        state
            .store
            .add_privacy_rule(&Actor::new("human:test"), "term", "secret project")
            .unwrap();
        let message = |id: &str, body: &str| {
            format!(
                "From: Ann <ann@x.example>\r\nSubject: notes\r\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\r\nMessage-ID: <{id}>\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
            )
            .into_bytes()
        };
        let broken = message("broken@x", "The secret\r\nproject starts Monday.");
        let spaced = message("spaced@x", "The secret   project starts Monday.");
        let other = message("other@x", "The secret garden project starts Monday.");
        let shown = gmail_read(&state, &broken, "broken@x", false).unwrap();
        assert!(shown.contains("secret project"), "{shown}");
        assert!(gmail_held(&state, &broken, "broken@x"));
        assert!(gmail_held(&state, &spaced, "spaced@x"));
        assert!(gmail_read(&state, &other, "other@x", true).is_ok());
    }

    /// An archive message is judged on its decoded text: a body in base64
    /// or quoted-printable whose words a rule names is held back while
    /// locked — from search (which finds it by the decoded words) and from
    /// vfetch — though no memory was ever made of it.
    #[test]
    fn test_archive_privacy_rules_read_the_decoded_text() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        let message = |id: &str, subject: &str, encoding: &str, body: &str| {
            format!(
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: {subject}\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\
                 Content-Type: text/plain; charset=utf-8\n\
                 Content-Transfer-Encoding: {encoding}\n\n{body}\n\n"
            )
        };
        std::fs::write(
            &mbox,
            [
                message(
                    "b64@x",
                    "plans for tuesday",
                    "base64",
                    "VGhlIG1lZGlhdGlvbiBpcyBvbiBUdWVzZGF5IGF0IG5vb24u",
                ),
                message(
                    "qp@x",
                    "plans for wednesday",
                    "quoted-printable",
                    "The medi=\nation is on Wednesday.",
                ),
                message(
                    "plain@x",
                    "plans for lunch",
                    "7bit",
                    "Noon at the usual place.",
                ),
            ]
            .concat(),
        )
        .unwrap();
        let raw_archive = std::fs::read_to_string(&mbox).unwrap();
        assert!(
            !raw_archive.contains("mediation"),
            "only the decoded text says it"
        );
        crate::mbox_index::build(&mbox).unwrap();
        crate::mbox_text::index_batch(&mbox, 10).unwrap();

        let (state, _home) = state_with(tmp.path(), "imported");
        let actor = Actor::new("human:test");
        state
            .store
            .register_virtual_source(
                &actor,
                "gmail-archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into(), "promote".into()],
            )
            .unwrap();
        state
            .store
            .add_privacy_rule(&actor, "term", "mediation")
            .unwrap();
        let def = resolve(&state, "gmail-archive").unwrap();
        let paths = |q: &str| {
            let mut p: Vec<String> = search_for_viewer(&state, &def, q, 10)
                .unwrap()
                .into_iter()
                .map(|h| h.path)
                .collect();
            p.sort();
            p
        };

        assert!(paths("mediation").is_empty());
        assert_eq!(paths("plans"), ["plain@x"]);
        for held in ["b64@x", "qp@x"] {
            assert_eq!(
                fetch_for_viewer(&state, &def, held, None)
                    .unwrap_err()
                    .to_string(),
                format!("no message {held:?} in the index")
            );
        }
        assert!(fetch_for_viewer(&state, &def, "plain@x", None).is_ok());

        state.store.vault_unlock(15);
        assert_eq!(paths("mediation"), ["b64@x", "qp@x"]);
        assert!(fetch_for_viewer(&state, &def, "b64@x", None).is_ok());
    }

    /// A promoted message is kept under its own Message-ID, whichever path
    /// promoted it, so a private promotion is held back on every path to
    /// the message — and so is one an earlier version kept under the path
    /// it was asked for (an archive copy's `at:<offset>`, a Gmail `uid:`).
    #[test]
    fn test_a_private_promotion_holds_on_every_path_to_the_message() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        let message = |id: &str, subject: &str| {
            format!(
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: {subject}\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\nSee you then.\n\n"
            )
        };
        let first = message("dup1@x", "plans for the clinic");
        let second = message("dup2@x", "plans for the dentist");
        std::fs::write(
            &mbox,
            [
                first.clone(),
                first.clone(), // a second copy: indexed as at:<offset>
                second.clone(),
                second.clone(),
                message("sail@x", "plans for saturday"),
            ]
            .concat(),
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let copy_of = |q: &str| {
            crate::mbox_index::search(&mbox, q, 10)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .find(|id| id.starts_with("at:"))
                .unwrap()
        };
        let (clinic_copy, dentist_copy) = (copy_of("clinic"), copy_of("dentist"));

        let (state, _home) = state_with(tmp.path(), "imported");
        let actor = Actor::new("human:test");
        state
            .store
            .register_virtual_source(
                &actor,
                "gmail-archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into(), "promote".into()],
            )
            .unwrap();
        let def = resolve(&state, "gmail-archive").unwrap();
        let found = || {
            let mut p: Vec<String> = search_for_viewer(&state, &def, "plans", 10)
                .unwrap()
                .into_iter()
                .map(|h| h.path)
                .collect();
            p.sort();
            p
        };
        let held = |path: &str| fetch_for_viewer(&state, &def, path, None).is_err();

        // promoted privately from its second copy: kept under its Message-ID
        state.store.vault_unlock(15);
        let promoted = promote(
            &state,
            &actor,
            "gmail-archive",
            &clinic_copy,
            None,
            Some("private"),
            None,
        )
        .unwrap();
        assert_eq!(
            promoted.episode_id.as_deref(),
            Some(ids::episode_id("gmail-archive", "dup1@x").as_str())
        );
        // the same message by its other path is the same memory
        let again = promote(&state, &actor, "gmail-archive", "dup1@x", None, None, None).unwrap();
        assert!(again.existing);
        // one an earlier version kept under the copy's own path
        state
            .store
            .apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: "human:test".into(),
                    session_id: None,
                    episodes: vec![EpisodeDraft {
                        source: "gmail-archive".into(),
                        source_ref: dentist_copy.clone(),
                        kind: "email".into(),
                        title: "plans for the dentist".into(),
                        body: second.clone(),
                        event_time: EventTime::unknown(),
                        trust: "imported".into(),
                        scope: Some("private".into()),
                        entities: vec![],
                        edges: vec![],
                    }],
                },
            )
            .unwrap();
        state.store.vault_lock();

        for path in [
            "dup1@x",
            clinic_copy.as_str(),
            "dup2@x",
            dentist_copy.as_str(),
        ] {
            assert!(held(path), "{path}");
        }
        assert_eq!(found(), ["sail@x"]);

        // Gmail: a promotion kept under its Message-ID holds for its uid
        // path; one kept under a uid path holds for its Message-ID
        let gmail = |id: &str| {
            format!(
                "From: Ann <ann@x.example>\r\nSubject: s\r\nDate: Mon, 05 Jan 2026 10:00:00 -0400\r\n\
                 Message-ID: <{id}>\r\n\r\nSee you then.\r\n"
            )
            .into_bytes()
        };
        let private_gmail = |source_ref: &str| {
            state
                .store
                .apply_batch(
                    &state.home,
                    &hearthloom_core::IngestBatch {
                        actor: "human:test".into(),
                        session_id: None,
                        episodes: vec![EpisodeDraft {
                            source: "gmail".into(),
                            source_ref: source_ref.into(),
                            kind: "file".into(),
                            title: "s".into(),
                            body: "s".into(),
                            event_time: EventTime::unknown(),
                            trust: "imported".into(),
                            scope: Some("private".into()),
                            entities: vec![],
                            edges: vec![],
                        }],
                    },
                )
                .unwrap();
        };
        let new_style = gmail("g1@x");
        let key = mail_key(&String::from_utf8_lossy(&new_style), None);
        assert_eq!(promotion_ref("uid:41", &key, "s"), "g1@x");
        private_gmail("g1@x");
        assert!(gmail_held(&state, &new_style, "uid:41"));
        assert!(gmail_read_uid(&state, &gmail("g3@x"), 43, "g3@x", true).is_ok());
        // the archive's private promotion holds when Gmail finds the message
        assert!(gmail_held(&state, &gmail("dup1@x"), "uid:77"));
        // one kept under a uid path names no Message-ID: no message can be
        // told apart from it (`test_an_unidentified_legacy_promotion_holds_every_message`)
        private_gmail("uid:42");
        assert!(gmail_read_uid(&state, &gmail("g2@x"), 42, "g2@x", true)
            .is_err_and(|e| e.to_string() == "no message \"g2@x\" in Gmail"));
        assert!(gmail_read_uid(&state, &gmail("g3@x"), 43, "g3@x", true).is_err());
    }

    /// A private memory as an earlier version stored it, under `source`.
    fn stored_private(source: &str, source_ref: &str, body: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: source.into(),
            source_ref: source_ref.into(),
            kind: "email".into(),
            title: "promoted".into(),
            body: body.into(),
            event_time: EventTime::unknown(),
            trust: "imported".into(),
            scope: Some("private".into()),
            entities: vec![],
            edges: vec![],
        }
    }

    /// A private promotion an earlier version kept under an archive copy's
    /// `at:<offset>` is known by the Message-ID its stored text names: the
    /// message is held back on every path to it — the same archive (however
    /// many such promotions there are), another archive, Gmail — and from
    /// triage and an ember's delivery. One whose message has no Message-ID
    /// counts as a private message without one.
    #[test]
    fn test_a_legacy_private_promotion_holds_on_every_mail_path() {
        let tmp = tempfile::tempdir().unwrap();
        let message = |id: &str, subject: &str| {
            format!(
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: {subject}\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\nSee you then.\n\n"
            )
        };
        let clinic = message("clinic@x", "plans for the clinic");
        let sail = message("sail@x", "plans for saturday");
        let first = tmp.path().join("first.mbox");
        std::fs::write(
            &first,
            [clinic.clone(), clinic.clone(), sail.clone()].concat(),
        )
        .unwrap();
        let second = tmp.path().join("second.mbox");
        std::fs::write(&second, [clinic.clone(), sail.clone()].concat()).unwrap();
        for mbox in [&first, &second] {
            crate::mbox_index::build(mbox).unwrap();
        }
        let copy = crate::mbox_index::search(&first, "clinic", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .find(|id| id.starts_with("at:"))
            .unwrap();

        let (state, _home) = state_with(tmp.path(), "imported");
        let actor = Actor::new("human:test");
        for (name, mbox) in [("first-archive", &first), ("second-archive", &second)] {
            state
                .store
                .register_virtual_source(
                    &actor,
                    name,
                    "mbox",
                    &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                    &["search".into(), "fetch".into(), "promote".into()],
                )
                .unwrap();
        }
        // what an earlier version kept: the clinic message promoted private
        // from its second copy, under that copy's path — after a thousand
        // other private promotions whose paths come first
        let mut kept: Vec<EpisodeDraft> = (0..1000)
            .map(|i| {
                stored_private(
                    "first-archive",
                    &format!("at:0{i:04}"),
                    &message(&format!("other{i}@x"), "other"),
                )
            })
            .collect();
        kept.push(stored_private("first-archive", &copy, &clinic));
        import_private(&state, kept);

        let fetch = |source: &str, path: &str| {
            fetch_for_viewer(&state, &resolve(&state, source).unwrap(), path, None)
        };
        // the same archive, by the message's own id
        assert!(fetch("first-archive", "clinic@x").is_err());
        // another archive
        assert!(fetch("second-archive", "clinic@x").is_err());
        let second_def = resolve(&state, "second-archive").unwrap();
        let found: Vec<String> = search_for_viewer(&state, &second_def, "plans", 10)
            .unwrap()
            .into_iter()
            .map(|h| h.path)
            .collect();
        assert_eq!(found, ["sail@x"]);
        assert!(fetch("second-archive", "sail@x").is_ok());
        // Gmail
        let gmail = |id: &str| {
            format!(
                "From: Ann <ann@x.example>\r\nSubject: s\r\nDate: Mon, 05 Jan 2026 10:00:00 -0400\r\n\
                 Message-ID: <{id}>\r\n\r\nSee you then.\r\n"
            )
            .into_bytes()
        };
        assert!(gmail_held(&state, &gmail("clinic@x"), "uid:77"));
        assert!(gmail_read_uid(&state, &gmail("sail@x"), 78, "sail@x", true).is_ok());
        // triage, and an ember's delivery (which holds only the parsed id)
        let key = |raw: &str| mail_key(raw, None);
        assert!(mail_memory_private(&state, &key(&clinic)).unwrap());
        assert!(!mail_memory_private(&state, &key(&sail)).unwrap());
        let ids = private_mail_ids(&state, &["clinic@x".into(), "sail@x".into()]).unwrap();
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), ["clinic@x"]);

        // one whose stored message has no Message-ID counts as a private
        // message without one
        assert!(!mail_memory_private(&state, &ItemKey::Unkeyed).unwrap());
        import_private(
            &state,
            vec![stored_private(
                "first-archive",
                "at:9999999",
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: picnic\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\n\nBring the blanket.\n",
            )],
        );
        assert!(mail_memory_private(&state, &ItemKey::Unkeyed).unwrap());
        assert!(!mail_memory_private(&state, &key(&sail)).unwrap());

        // open
        state.store.vault_unlock(15);
        assert!(fetch("second-archive", "clinic@x").is_ok());
    }

    /// A private promotion an earlier version kept under an archive copy's
    /// `at:` path is matched by the message's identity, not by how its
    /// Message-ID header is written: a comment beside the id in the stored
    /// copy, or in the copy read, still finds it — on an archive read, in a
    /// search, in triage's check and in an ember's delivery alike.
    #[test]
    fn test_a_legacy_promotion_is_matched_by_message_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let message = |id_header: &str, subject: &str| {
            format!(
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: {subject}\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: {id_header}\n\nSee you then.\n\n"
            )
        };
        // stored with a comment beside the id, read without one
        let clinic_stored = message("<clinic@x> (archive copy)", "plans for the clinic");
        let clinic_read = message("<clinic@x>", "plans for the clinic");
        // stored without one, read with one
        let dentist_stored = message("<dentist@x>", "plans for the dentist");
        let dentist_read = message("<dentist@x> (archive copy)", "plans for the dentist");
        let sail = message("<sail@x>", "plans for saturday");
        let mbox = tmp.path().join("archive.mbox");
        std::fs::write(
            &mbox,
            [clinic_read.clone(), dentist_read.clone(), sail.clone()].concat(),
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let path_of = |q: &str| {
            crate::mbox_index::search(&mbox, q, 10).unwrap()[0]
                .0
                .clone()
        };
        let (clinic_path, dentist_path) = (path_of("clinic"), path_of("dentist"));
        assert_eq!(clinic_path, "clinic@x");
        assert_eq!(dentist_path, "dentist@x> (archive copy)");

        let (state, _home) = state_with(tmp.path(), "imported");
        state
            .store
            .register_virtual_source(
                &Actor::new("human:test"),
                "archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into()],
            )
            .unwrap();
        // what an earlier version kept: both promoted private from another
        // archive's copies, under those copies' paths
        import_private(
            &state,
            vec![
                stored_private("old-archive", "at:100", &clinic_stored),
                stored_private("old-archive", "at:200", &dentist_stored),
            ],
        );
        let def = resolve(&state, "archive").unwrap();
        let held = |path: &str| {
            fetch_for_viewer(&state, &def, path, None)
                .is_err_and(|e| e.to_string() == format!("no message {path:?} in the index"))
        };
        assert!(held(&clinic_path));
        assert!(held(&dentist_path));
        assert!(fetch_for_viewer(&state, &def, "sail@x", None).is_ok());
        let found: Vec<String> = search_for_viewer(&state, &def, "plans", 10)
            .unwrap()
            .into_iter()
            .map(|h| h.path)
            .collect();
        assert_eq!(found, ["sail@x"]);
        // triage's check, and an ember's delivery (the parsed id)
        let key = |raw: &str| mail_key(raw, None);
        assert!(mail_memory_private(&state, &key(&clinic_read)).unwrap());
        assert!(mail_memory_private(&state, &key(&dentist_read)).unwrap());
        assert!(!mail_memory_private(&state, &key(&sail)).unwrap());
        let mut ids: Vec<String> = private_mail_ids(
            &state,
            &["clinic@x".into(), "dentist@x".into(), "sail@x".into()],
        )
        .unwrap()
        .into_iter()
        .collect();
        ids.sort();
        assert_eq!(ids, ["clinic@x", "dentist@x"]);

        // open
        state.store.vault_unlock(15);
        for path in [&clinic_path, &dentist_path] {
            assert!(fetch_for_viewer(&state, &def, path, None).is_ok());
        }
    }

    /// A Gmail promotion an earlier version kept under a `uid:` path can't
    /// be identified: Gmail's fetch took a uid for any message, and the
    /// promotion stored only the text shown, which names no Message-ID. While
    /// it is private, no message can be told apart from it, so every message
    /// is held back on every mail path — archives, Gmail, triage's check, an
    /// ember's delivery — until the vault is opened. A `uid:` ref under a
    /// source that doesn't read Gmail is not such a promotion.
    #[test]
    fn test_an_unidentified_legacy_promotion_holds_every_message() {
        let tmp = tempfile::tempdir().unwrap();
        let message = |id: &str, subject: &str| {
            format!(
                "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: {subject}\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\nSee you then.\n\n"
            )
        };
        let clinic = message("clinic@x", "plans for the clinic");
        let sail = message("sail@x", "plans for saturday");
        let mbox = tmp.path().join("archive.mbox");
        std::fs::write(&mbox, [clinic.clone(), sail.clone()].concat()).unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let (state, _home) = state_with(tmp.path(), "imported");
        state
            .store
            .register_virtual_source(
                &Actor::new("human:test"),
                "archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into()],
            )
            .unwrap();
        let def = resolve(&state, "archive").unwrap();
        let shown = |path: &str| fetch_for_viewer(&state, &def, path, None).is_ok();
        let found = || search_for_viewer(&state, &def, "plans", 10).unwrap().len();
        let key = |raw: &str| mail_key(raw, None);
        let private_ids = || {
            private_mail_ids(&state, &["clinic@x".into(), "sail@x".into()])
                .unwrap()
                .len()
        };
        let gmail = |id: &str| {
            format!(
                "From: Ann <ann@x.example>\r\nSubject: s\r\nDate: Mon, 05 Jan 2026 10:00:00 -0400\r\n\
                 Message-ID: <{id}>\r\n\r\nSee you then.\r\n"
            )
            .into_bytes()
        };

        // a private memory under a `uid:` ref of a source that doesn't read
        // Gmail holds no message back
        import_private(
            &state,
            vec![stored_private("ics", "uid:7", "the clinic visit")],
        );
        assert!(shown("clinic@x") && shown("sail@x"));
        assert_eq!(found(), 2);
        assert!(!mail_memory_private(&state, &key(&clinic)).unwrap());
        assert_eq!(private_ids(), 0);

        // the clinic message, promoted private through its uid
        import_private(
            &state,
            vec![stored_private(
                crate::gmail_search::SOURCE,
                "uid:42",
                "From: Ann <ann@x.example>\nTo: Jon <jon@x.example>\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nSubject: plans for the clinic\n\nSee you then.",
            )],
        );
        assert!(!shown("clinic@x") && !shown("sail@x"));
        assert_eq!(found(), 0);
        assert!(gmail_held(&state, &gmail("clinic@x"), "uid:77"));
        assert!(gmail_held(&state, &gmail("sail@x"), "uid:78"));
        for raw in [&clinic, &sail] {
            assert!(mail_memory_private(&state, &key(raw)).unwrap());
        }
        assert!(mail_memory_private(&state, &ItemKey::Unkeyed).unwrap());
        assert_eq!(private_ids(), 2);

        // open
        state.store.vault_unlock(15);
        assert!(shown("clinic@x") && shown("sail@x"));
        assert_eq!(found(), 2);
    }

    /// A private promotion holds wherever the message is read, whether or
    /// not the source it was promoted from is registered here: after it is
    /// unregistered (archives move between nodes), and when it was never
    /// registered on this node (the promotion synced from another).
    #[test]
    fn test_a_private_promotion_outlasts_its_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        std::fs::write(
            &mbox,
            "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: the clinic\n\
             Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <clinic@x>\n\nSee you then.\n\n",
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let (state, _home) = state_with(tmp.path(), "imported");
        let actor = Actor::new("human:test");
        state
            .store
            .register_virtual_source(
                &actor,
                "old-archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into(), "promote".into()],
            )
            .unwrap();
        state.store.vault_unlock(15);
        promote(
            &state,
            &actor,
            "old-archive",
            "clinic@x",
            None,
            Some("private"),
            None,
        )
        .unwrap();
        state.store.vault_lock();
        assert!(state
            .store
            .unregister_virtual_source(&actor, "old-archive")
            .unwrap());
        // promoted on another node from an archive never registered here
        import_private(
            &state,
            vec![stored_private("laptop-archive", "dentist@x", "promoted")],
        );

        let gmail = |id: &str| {
            format!(
                "From: Ann <ann@x.example>\r\nSubject: s\r\nDate: Mon, 05 Jan 2026 10:00:00 -0400\r\n\
                 Message-ID: <{id}>\r\n\r\nSee you then.\r\n"
            )
            .into_bytes()
        };
        assert!(gmail_held(&state, &gmail("clinic@x"), "uid:5"));
        assert!(gmail_held(&state, &gmail("dentist@x"), "uid:6"));
        assert!(gmail_read_uid(&state, &gmail("sail@x"), 7, "sail@x", true).is_ok());
        let key = |id: &str| mail_key(&String::from_utf8_lossy(&gmail(id)), None);
        assert!(mail_memory_private(&state, &key("clinic@x")).unwrap());
        assert!(mail_memory_private(&state, &key("dentist@x")).unwrap());
        let ids = private_mail_ids(&state, &["dentist@x".into(), "sail@x".into()]).unwrap();
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), ["dentist@x"]);
    }

    /// 0.5.86: Ask is told what each mail archive holds (rounded, so its
    /// instructions don't change with every message) and how mail search
    /// works.
    #[test]
    fn test_ask_is_told_what_a_mail_archive_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        std::fs::write(
            &mbox,
            "From a@x Mon Jan 05 10:00:00 2026\nFrom: Ann <ann@x.example>\nSubject: one\n\
             Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <a1@x>\n\nhi\n\n\
             From b@x Sat Mar 01 10:00:00 2025\nFrom: Bo <bo@x.example>\nSubject: two\n\
             Date: Sat, 01 Mar 2025 10:00:00 +0000\nMessage-ID: <b1@x>\n\nyo\n\n\
             From c@x Sat Mar 01 10:00:00 1970\nFrom: Spam <s@x.example>\nSubject: three\n\
             Date: Thu, 01 Jan 1970 00:00:00 +0000\nMessage-ID: <c1@x>\n\nold clock\n",
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();
        let s = crate::mbox_index::compute_summary(&mbox).unwrap();
        assert_eq!(s.messages, 3);
        assert_eq!(
            (s.first.as_deref(), s.last.as_deref()),
            (Some("2025-03"), Some("2026-01"))
        );
        assert_eq!(
            archive_summary_line(&s),
            " — about 3 messages, 2025-03 → 2026-01"
        );
        let big = crate::mbox_index::Summary {
            messages: 664_921,
            first: None,
            last: None,
        };
        assert_eq!(archive_summary_line(&big), " — about 664,900 messages");
        assert_eq!(
            text_status_line(Some((19_544, 19_544))),
            "; its message text and links are searchable"
        );
        assert_eq!(
            text_status_line(Some((300_000, 664_921))),
            "; message text searchable for about 40% so far (headers for all)"
        );
        assert!(text_status_line(None).contains("headers only"));
        let live = crate::mbox_index::Summary {
            messages: 19_544,
            first: None,
            last: None,
        };
        assert_eq!(archive_summary_line(&live), " — about 19,500 messages");
        // the background count lands for the next ask
        let mut seen = None;
        for _ in 0..50 {
            seen = crate::mbox_index::summary(&mbox);
            if seen.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(seen, Some(s));
    }

    /// The places that read mounted sources for a caller go through the
    /// vault-aware readers; a new one that calls the raw ones fails here.
    #[test]
    fn test_archive_readers_go_through_the_vault_check() {
        for (file, src) in [
            ("rlm.rs", include_str!("rlm.rs")),
            ("http.rs", include_str!("http.rs")),
            ("mcp.rs", include_str!("mcp.rs")),
            ("network_out.rs", include_str!("network_out.rs")),
        ] {
            for raw in [
                "virtual_source::search(",
                "virtual_source::fetch(",
                "mbox_index::search(",
                "mbox_index::fetch(",
            ] {
                assert!(
                    !src.contains(raw),
                    "{file} calls {raw}…) — use search_for_viewer / fetch_for_viewer"
                );
            }
        }
    }

    #[test]
    fn test_mbox_virtual_source_search_fetch_promote() {
        let tmp = tempfile::tempdir().unwrap();
        let mbox = tmp.path().join("archive.mbox");
        std::fs::write(
            &mbox,
            "From a@x Mon Jan 05 10:00:00 2026\n\
             From: Ann <ann@x.example>\n\
             Subject: sailing plans\n\
             Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
             Message-ID: <sail1@x>\n\
             X-Gmail-Labels: Archived,Opened\n\
             \n\
             The regatta is on the 12th.\n",
        )
        .unwrap();
        crate::mbox_index::build(&mbox).unwrap();

        let home_tmp = tempfile::tempdir().unwrap();
        let home = Home::at(home_tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        store
            .register_virtual_source(
                &Actor::new("human:test"),
                "gmail-archive",
                "mbox",
                &json!({"mount": mbox.to_string_lossy(), "trust": "imported"}),
                &["search".into(), "fetch".into(), "promote".into()],
            )
            .unwrap();
        let state = AppState {
            store: Arc::new(store),
            home,
            embedder: None,
            llm: None,
            auth_locked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        let def = resolve(&state, "gmail-archive").unwrap();

        let hits = search(&def, "sailing", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "sail1@x");
        assert!(hits[0].snippet.contains("sailing plans"));

        let read = fetch(&def, "sail1@x", None).unwrap();
        assert!(read.shown.contains("regatta"));
        assert_eq!(read.key, ItemKey::Refs(vec!["sail1@x".into()]));

        let actor = Actor::new("human:test");
        let outcome =
            promote(&state, &actor, "gmail-archive", "sail1@x", None, None, None).unwrap();
        let id = outcome.episode_id.expect("materialized");
        let episode = state.store.episode(&id).unwrap().unwrap();
        assert_eq!(episode.kind, "email");
        assert_eq!(episode.title, "sailing plans");
        assert_eq!(episode.source, "gmail-archive");
        assert!(
            episode
                .event_time
                .as_deref()
                .unwrap_or("")
                .starts_with("2026-01-05"),
            "promoted email carries its Date, got {:?}",
            episode.event_time
        );
    }

    #[test]
    fn test_email_meta_first_header_wins() {
        let (title, time) = email_meta(
            "Subject: sailing plans\nDate: Mon, 05 Jan 2026 10:00:00 -0400\n\
             Subject: URGENT: wire the deposit\nDate: Fri, 01 Jan 2016 00:00:00 +0000\n\nbody\n",
            "sail1@x",
        );
        assert_eq!(title, "sailing plans");
        assert!(
            time.utc.as_deref().unwrap_or("").starts_with("2026-01-05"),
            "got {:?}",
            time.utc
        );
        // Missing headers still fall back.
        assert_eq!(email_meta("\nbody", "m@x").0, "mail m@x");
    }
}

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
    /// The item's text when the search already has it (Gmail search), for
    /// the privacy check — never sent anywhere.
    #[serde(skip)]
    pub text: Option<String>,
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
                text: None,
            })
            .collect());
    }
    if uses_magic(def) {
        magic_search(&mount, query, limit)
    } else {
        fs_search(&mount, query, limit)
    }
}

/// The memory sources an item of this source can have become, keyed by
/// its path: a promoted item keeps the source's own name; a mail archive's
/// messages are also the `mbox` memories the Gmail pull and the Takeout
/// ingest made (keyed by the same Message-ID).
fn materialized_sources(def: &VirtualSourceDef) -> Vec<&str> {
    if def.adapter == "mbox" || def.adapter == crate::gmail_search::ADAPTER {
        vec!["mbox", def.name.as_str()]
    } else {
        vec![def.name.as_str()]
    }
}

/// Which of these paths became a memory labelled private.
fn marked_private(
    state: &AppState,
    def: &VirtualSourceDef,
    paths: &[String],
) -> Result<std::collections::HashSet<String>> {
    let mut out = std::collections::HashSet::new();
    for source in materialized_sources(def) {
        out.extend(state.store.private_source_refs(source, paths)?);
    }
    Ok(out)
}

/// Is this item private for the vault (0.5.86)? The tests a memory meets:
/// the memory made from it is labelled private (by a rule or by hand), or
/// a privacy rule matches its text. Archives are read in place and never
/// pass through ingest, so until 0.5.86 neither test ran on them: mail a
/// rule makes private was searchable and readable through the archive
/// with the vault locked.
fn item_private(state: &AppState, def: &VirtualSourceDef, path: &str, text: &str) -> Result<bool> {
    Ok(
        marked_private(state, def, &[path.to_string()])?.contains(path)
            || state.store.privacy_rules_match(text)?,
    )
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
        let text = if let Some(text) = &hit.text {
            text.clone()
        } else if def.adapter == "mbox" {
            fetch(def, &hit.path, None).unwrap_or_else(|_| hit.snippet.clone())
        } else {
            format!("{}\n{}", hit.path, hit.snippet)
        };
        if state.store.privacy_rules_match(&text)? {
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
    let text = fetch(def, path, budget)?;
    if locked && item_private(state, def, path, &text)? {
        return Err(if def.adapter == "mbox" {
            anyhow!("no message {path:?} in the index")
        } else {
            anyhow!("nothing at {path:?}")
        });
    }
    Ok(text)
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
                text: None,
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
                    text: None,
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
pub fn fetch(def: &VirtualSourceDef, path: &str, budget: Option<u32>) -> Result<String> {
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
        return crate::mbox_index::fetch(&mount, path, MAX_FETCH_CHARS);
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
    Ok(String::from_utf8_lossy(&bytes)
        .chars()
        .take(MAX_FETCH_CHARS)
        .collect())
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
/// fetch time. Idempotent on `(source, path)` — re-promoting is a no-op.
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
    let content = fetch_for_viewer(state, &def, path, budget)?;
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
            "source_ref": path,
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
            source_ref: path.to_string(),
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
        episode_id: Some(ids::episode_id(&def.name, path)),
        quarantined: None,
        existing: report.episodes_new == 0 && report.episodes_existing > 0,
    })
}

/// The RLM system-prompt fragment describing available virtual sources and
/// their read handles — None when none are registered (base experience
/// unchanged; the system stays degraded-but-useful without them).
/// The live mailbox's line in Ask's instructions (0.5.89).
fn gmail_line(account: Option<&str>, node: Option<&str>, reachable: bool) -> String {
    format!(
        "- gmail [gmail]{} — the WHOLE Gmail mailbox{}, searched live with Gmail's own search: \
         every message the account holds, old and new{}\n",
        node.map(|n| format!(" on node \"{n}\""))
            .unwrap_or_default(),
        account.map(|a| format!(" of {a}")).unwrap_or_default(),
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
mailbox, current to the minute. Its vsearch takes Gmail search syntax: words anywhere in the \
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
                source.config["account"].as_str(),
                None,
                crate::gmail_search::account().is_some(),
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
                    archive["account"].as_str(),
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
             apple-health tables: daily(day,metric,source,n,sum,min,max,avg) — one row per \
             day/metric/source, PREFER this for trends/comparisons; \
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
            start: "2026-04-02 08:00:00 -0400".into(),
            day: "2026-04-02".into(),
        };
        writer.add_sample(&s).unwrap();
        table.add(&s);
        writer.finish(&table.map).unwrap();

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

        let raw = fetch(&def, "sail1@x", None).unwrap();
        assert!(raw.contains("regatta"));

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

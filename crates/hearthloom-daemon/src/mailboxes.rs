//! The mailboxes this node reads (0.5.98).
//!
//! A node can watch several Google mailboxes: a personal @gmail.com and a
//! Google Workspace address are both IMAP on imap.gmail.com with an app
//! password. Each has its own watch (gmail_watch.rs), its own pull cursor
//! and live archive (gmail_pull.rs), and its own refusal: a password Google
//! refuses stops only that mailbox — in the watch, the pull, the search and
//! drafts alike — until it is saved again (Google locks accounts that keep
//! failing).
//!
//! The FIRST mailbox keeps the names it always had: GMAIL_IMAP_USER /
//! GMAIL_IMAP_PASSWORD, `archives/gmail-live.mbox` (source `gmail-live`)
//! and the `gmail.watch*` settings, so a node upgrades with nothing
//! re-pulled and an older build still reads it after a rollback. Added
//! mailboxes keep their app password sealed as
//! HEARTHLOOM_MAIL_<ID>_{USER,PASSWORD}, their own live archive
//! `archives/gmail-live-<id>.mbox` (source `gmail-live-<id>`) and settings
//! suffixed `.<id>`. The id is a hash of the lowercased address, so every
//! node agrees on it.
//!
//! The owner's addresses are every mailbox's address and the aliases listed
//! on it (mail that reaches it under another address of the owner's, like
//! a Workspace alias), here and on the owner's other nodes
//! (`roles.mailboxes` in the node list). Mail from any of them is the
//! owner's own: it opens no thread and never reaches an ember.
//!
//! One node handles each mailbox's threads and drafts (`duty`): of the live
//! nodes that watch it, the one with the smallest node id. Mail of a
//! mailbox no live node watches falls to the smallest live node that
//! watches any.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::AppState;

pub(crate) const FIRST_USER: &str = "GMAIL_IMAP_USER";
pub(crate) const FIRST_PASSWORD: &str = "GMAIL_IMAP_PASSWORD";
const ADDED_PREFIX: &str = "HEARTHLOOM_MAIL_";
/// The live archive's source name (and file stem) of the first mailbox;
/// added mailboxes use `gmail-live-<id>`.
pub(crate) const LIVE_SOURCE: &str = "gmail-live";
/// Addresses that also reach a mailbox (node-local, `mail.aliases.<id>`).
pub(crate) const KEY_ALIASES: &str = "mail.aliases";
/// Whether embers with `mail.watch` see this mailbox (`mail.embers.<id>`):
/// on by default for the first, off for an added one.
pub(crate) const KEY_EMBERS: &str = "mail.embers";
const MAX_ALIASES: usize = 20;
/// A node counts as live for `duty` when seen this recently.
const LIVE_MINUTES: i64 = 15;

#[derive(Clone)]
pub(crate) struct Mailbox {
    pub id: String,
    pub address: String,
    password: String,
    /// keeps the original names (GMAIL_IMAP_*, gmail-live.mbox, gmail.watch)
    pub first: bool,
}

// never print the password
impl std::fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailbox")
            .field("id", &self.id)
            .field("address", &self.address)
            .field("first", &self.first)
            .finish()
    }
}

impl Mailbox {
    pub fn password(&self) -> &str {
        &self.password
    }

    /// The lowercased address: how mailboxes are compared and stored.
    pub fn account(&self) -> String {
        self.address.to_lowercase()
    }

    /// The live archive's virtual-source name.
    pub fn source_name(&self) -> String {
        if self.first {
            LIVE_SOURCE.to_string()
        } else {
            format!("{LIVE_SOURCE}-{}", self.id)
        }
    }

    pub fn live_mbox_path(&self, state: &AppState) -> PathBuf {
        state
            .home
            .root()
            .join("archives")
            .join(format!("{}.mbox", self.source_name()))
    }

    /// A setting that predates several mailboxes: the first keeps the
    /// original key, an added one gets `<base>.<id>`.
    pub fn key(&self, base: &str) -> String {
        if self.first {
            base.to_string()
        } else {
            format!("{base}.{}", self.id)
        }
    }

    /// A setting every mailbox keeps under its id (`<base>.<id>`).
    pub fn id_key(&self, base: &str) -> String {
        format!("{base}.{}", self.id)
    }

    /// The two sealed-env names that hold its address and app password.
    pub fn env_names(&self) -> (String, String) {
        if self.first {
            (FIRST_USER.into(), FIRST_PASSWORD.into())
        } else {
            added_env_names(&self.id)
        }
    }
}

/// A mailbox's id: the first 8 hex characters of SHA-256 of its lowercased
/// address — the same on every node.
pub(crate) fn id_for(address: &str) -> String {
    hearthloom_core::ids::content_hash(address.trim().to_lowercase().as_bytes())[..8].to_string()
}

fn added_env_names(id: &str) -> (String, String) {
    let up = id.to_uppercase();
    (
        format!("{ADDED_PREFIX}{up}_USER"),
        format!("{ADDED_PREFIX}{up}_PASSWORD"),
    )
}

/// The mailboxes in an environment: the first (GMAIL_IMAP_*), then the
/// added ones by address. An empty value counts as unset; an added entry
/// whose name doesn't match its address's id (edited by hand) is ignored.
pub(crate) fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Vec<Mailbox> {
    let vars: BTreeMap<String, String> = vars
        .into_iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .collect();
    let mut out = Vec::new();
    if let (Some(user), Some(password)) = (vars.get(FIRST_USER), vars.get(FIRST_PASSWORD)) {
        let address = user.trim().to_string();
        out.push(Mailbox {
            id: id_for(&address),
            address,
            password: password.clone(),
            first: true,
        });
    }
    let mut added: Vec<Mailbox> = vars
        .iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix(ADDED_PREFIX)?.strip_suffix("_USER")?;
            if id.len() != 8 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let id = id.to_lowercase();
            let address = v.trim().to_string();
            if id_for(&address) != id {
                return None;
            }
            let password = vars.get(&added_env_names(&id).1)?.clone();
            Some(Mailbox {
                id,
                address,
                password,
                first: false,
            })
        })
        .filter(|m| !out.iter().any(|f: &Mailbox| f.account() == m.account()))
        .collect();
    added.sort_by_key(|m| m.account());
    out.extend(added);
    out
}

/// Every mailbox connected on this node.
pub(crate) fn all() -> Vec<Mailbox> {
    from_vars(std::env::vars())
}

/// The mailbox with this address or id, when it's connected here.
pub(crate) fn find(address_or_id: &str) -> Option<Mailbox> {
    let q = address_or_id.trim().to_lowercase();
    all().into_iter().find(|m| m.id == q || m.account() == q)
}

/// The one mailbox a command means when it names none: the only one, or
/// the first. `None` when there are none, or several without a first.
pub(crate) fn default_one() -> Option<Mailbox> {
    let all = all();
    if all.len() == 1 {
        return all.into_iter().next();
    }
    all.into_iter().find(|m| m.first)
}

/// Where a connect for `address` saves: an address already here keeps its
/// slot; on a node with no mailbox, the first slot (an older build reads
/// it); otherwise an added one. Returns (first, id, env names).
pub(crate) fn slot_for(existing: &[Mailbox], address: &str) -> (bool, String, (String, String)) {
    let account = address.trim().to_lowercase();
    if let Some(m) = existing.iter().find(|m| m.account() == account) {
        return (m.first, m.id.clone(), m.env_names());
    }
    let id = id_for(&account);
    if existing.is_empty() {
        (true, id, (FIRST_USER.into(), FIRST_PASSWORD.into()))
    } else {
        let names = added_env_names(&id);
        (false, id, names)
    }
}

/// Is this archive one a mailbox is still writing? (never moved or
/// released by archives_place.rs)
pub(crate) fn is_live_archive(source_name: &str) -> bool {
    source_name == LIVE_SOURCE || source_name.starts_with(&format!("{LIVE_SOURCE}-"))
}

/* ————— per-mailbox settings ————— */

fn list_setting(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .collect()
}

pub(crate) fn aliases(state: &AppState, mb: &Mailbox) -> Vec<String> {
    list_setting(
        state
            .store
            .consolidation_state(&mb.id_key(KEY_ALIASES))
            .ok()
            .flatten(),
    )
}

/// Check and tidy a list of aliases: addresses, lowercased, each once, not
/// the mailbox's own, at most 20.
pub(crate) fn clean_aliases(mb_account: &str, given: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for a in given {
        let a = a
            .trim()
            .trim_matches(|c| c == '<' || c == '>')
            .to_lowercase();
        if a.is_empty() || a == mb_account || out.contains(&a) {
            continue;
        }
        let ok = a.len() <= 254
            && a.matches('@').count() == 1
            && !a.starts_with('@')
            && !a.ends_with('@')
            && a.contains('.')
            && !a
                .chars()
                .any(|c| c.is_whitespace() || c == ',' || c.is_control());
        if !ok {
            bail!("{a:?} isn't an email address");
        }
        out.push(a);
    }
    if out.len() > MAX_ALIASES {
        bail!("at most {MAX_ALIASES} aliases per mailbox");
    }
    Ok(out)
}

pub(crate) fn set_aliases(state: &AppState, mb: &Mailbox, given: &[String]) -> Result<Vec<String>> {
    let clean = clean_aliases(&mb.account(), given)?;
    state
        .store
        .set_consolidation_state(&mb.id_key(KEY_ALIASES), &clean.join(","))?;
    Ok(clean)
}

/// Whether embers with `mail.watch` may see this mailbox's mail.
pub(crate) fn embers_see(state: &AppState, mb: &Mailbox) -> bool {
    match state
        .store
        .consolidation_state(&mb.id_key(KEY_EMBERS))
        .ok()
        .flatten()
        .as_deref()
    {
        Some("on") => true,
        Some("off") => false,
        _ => mb.first,
    }
}

pub(crate) fn set_embers(state: &AppState, mb: &Mailbox, on: bool) -> Result<()> {
    state
        .store
        .set_consolidation_state(&mb.id_key(KEY_EMBERS), if on { "on" } else { "off" })
}

/// Embers may see mail that arrived in `account` (a mailbox here)? One
/// that isn't here: no — unless no mailbox is connected here at all (a
/// node from before several mailboxes, replaying what it saw).
pub(crate) fn embers_see_account(state: &AppState, account: &str) -> bool {
    let boxes = all();
    let account = account.trim().to_lowercase();
    match boxes.iter().find(|mb| mb.account() == account) {
        Some(mb) => embers_see(state, mb),
        None => boxes.is_empty(),
    }
}

/* ————— refused sign-ins ————— */

/// Mailboxes (ids) whose sign-in Google refused, and whether the owner has
/// been told. Cleared when the password is saved again.
static REFUSED: Mutex<BTreeMap<String, bool>> = Mutex::new(BTreeMap::new());

pub(crate) fn refused(id: &str) -> bool {
    REFUSED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(id)
}

/// Google refused this mailbox's sign-in: nothing tries it again until it
/// is saved again. Tells the owner once (when `state` is at hand; the watch
/// loop tells them otherwise).
pub(crate) fn refuse(state: Option<&AppState>, mb: &Mailbox, err: &anyhow::Error) {
    let new = {
        let mut map = REFUSED.lock().unwrap_or_else(|p| p.into_inner());
        if map.contains_key(&mb.id) {
            false
        } else {
            map.insert(mb.id.clone(), false);
            true
        }
    };
    if new {
        tracing::warn!(
            "mail: Gmail refused the sign-in for {} — stopped until it's saved again: {err:#}",
            mb.address
        );
    }
    if let Some(state) = state {
        tell_refused(state, mb);
    }
}

/// The one notice for a refused mailbox (idempotent).
pub(crate) fn tell_refused(state: &AppState, mb: &Mailbox) {
    {
        let mut map = REFUSED.lock().unwrap_or_else(|p| p.into_inner());
        match map.get_mut(&mb.id) {
            Some(told) if !*told => *told = true,
            _ => return,
        }
    }
    crate::provisioner::notify_system(
        state,
        "warn",
        "Gmail stopped accepting Hearthloom's sign-in",
        &format!(
            "New mail for {} isn't arriving. Reconnect it on the Integrations page (the app \
             password may have been revoked or changed).",
            mb.address
        ),
        Some(&if mb.first {
            "gmail-watch:refused".to_string()
        } else {
            format!("gmail-watch:refused:{}", mb.id)
        }),
    );
}

/// The password was saved again (or the mailbox removed): try it again.
pub(crate) fn clear_refused(id: &str) {
    REFUSED.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
}

/// Refuse to use a mailbox Google refused; on an auth failure, mark it so.
pub(crate) fn check_refused(mb: &Mailbox) -> Result<()> {
    if refused(&mb.id) {
        bail!(
            "Gmail refused the sign-in for {} — reconnect it on the Integrations page",
            mb.address
        );
    }
    Ok(())
}

/* ————— the owner's addresses, and who handles what ————— */

/// What this node publishes in the node list (`roles.mailboxes`).
pub(crate) fn published(state: &AppState) -> Value {
    json!(all()
        .iter()
        .map(|mb| json!({
            "id": mb.id,
            "address": mb.account(),
            "watching": crate::gmail_watch::watching_mailbox(state, mb),
            "aliases": aliases(state, mb),
        }))
        .collect::<Vec<_>>())
}

/// Every address the owner's mail comes from: the mailboxes here and on the
/// owner's other nodes, and the aliases listed on them.
pub(crate) fn owner_addresses(state: &AppState) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for mb in all() {
        out.insert(mb.account());
        out.extend(aliases(state, &mb));
    }
    for node in state.store.nodes().unwrap_or_default() {
        for m in node["roles"]["mailboxes"].as_array().into_iter().flatten() {
            if let Some(a) = m["address"].as_str() {
                out.insert(a.to_lowercase());
            }
            for a in m["aliases"].as_array().into_iter().flatten() {
                if let Some(a) = a.as_str() {
                    out.insert(a.to_lowercase());
                }
            }
        }
    }
    out
}

/// Which mailboxes' threads and drafts this node handles.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Duty {
    /// lowercased addresses
    pub mine: Vec<String>,
    /// Set when this node also takes mail of a mailbox no live node watches
    /// (or "" from before 0.5.98): the mailboxes live nodes do watch.
    pub unwatched: Option<Vec<String>>,
}

impl Duty {
    pub fn is_idle(&self) -> bool {
        self.mine.is_empty() && self.unwatched.is_none()
    }

    /// Does this node handle mail of `account`?
    #[cfg(test)]
    pub fn handles(&self, account: &str) -> bool {
        let a = account.trim().to_lowercase();
        self.mine.contains(&a)
            || self
                .unwatched
                .as_ref()
                .is_some_and(|watched| !watched.contains(&a))
    }
}

pub(crate) fn duty(state: &AppState) -> Duty {
    let local: Vec<String> = all()
        .iter()
        .filter(|mb| crate::gmail_watch::watching_mailbox(state, mb))
        .map(Mailbox::account)
        .collect();
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(LIVE_MINUTES)).to_rfc3339();
    duty_from(
        &crate::agents::node_id(state),
        &local,
        &state.store.nodes().unwrap_or_default(),
        &cutoff,
    )
}

/// `duty`, from this node's id, the mailboxes it watches and the node list.
/// A live node from before 0.5.98 (`roles.mail` without `roles.mailboxes`)
/// with a smaller id handles everything, as it would itself.
pub(crate) fn duty_from(me: &str, local: &[String], nodes: &[Value], cutoff: &str) -> Duty {
    let mut others: Vec<(String, Vec<String>)> = Vec::new();
    let mut older_smaller = false;
    let mut smallest_watcher: Option<String> = (!local.is_empty()).then(|| me.to_string());
    for n in nodes {
        let Some(id) = n["node_id"].as_str() else {
            continue;
        };
        if id == me || n["last_seen"].as_str().is_none_or(|t| t <= cutoff) {
            continue;
        }
        let watched: Vec<String> = match n["roles"]["mailboxes"].as_array() {
            Some(list) => list
                .iter()
                .filter(|m| m["watching"] == true)
                .filter_map(|m| m["address"].as_str().map(str::to_lowercase))
                .collect(),
            None => {
                if n["roles"]["mail"] == true && id < me {
                    older_smaller = true;
                }
                Vec::new()
            }
        };
        let watches_any = !watched.is_empty() || n["roles"]["mail"] == true;
        if watches_any && smallest_watcher.as_deref().is_none_or(|s| id < s) {
            smallest_watcher = Some(id.to_string());
        }
        others.push((id.to_string(), watched));
    }
    if older_smaller {
        return Duty::default();
    }
    let mine: Vec<String> = local
        .iter()
        .filter(|a| {
            !others
                .iter()
                .any(|(id, watched)| id.as_str() < me && watched.contains(a))
        })
        .cloned()
        .collect();
    let unwatched = (smallest_watcher.as_deref() == Some(me)).then(|| {
        let mut watched: BTreeSet<String> = local.iter().cloned().collect();
        for (_, w) in &others {
            watched.extend(w.iter().cloned());
        }
        watched.into_iter().collect()
    });
    Duty { mine, unwatched }
}

/// A message of the live archives here, by Message-ID: the mailbox
/// `prefer`'s archive first, then the others.
pub(crate) fn read_local(
    state: &AppState,
    prefer: &str,
    message_id: &str,
    cap: usize,
) -> Option<(Mailbox, String)> {
    read_local_in(state, &all(), prefer, message_id, cap)
}

/// `read_local` over the given mailboxes.
pub(crate) fn read_local_in(
    state: &AppState,
    boxes: &[Mailbox],
    prefer: &str,
    message_id: &str,
    cap: usize,
) -> Option<(Mailbox, String)> {
    let prefer = prefer.trim().to_lowercase();
    let mut boxes = boxes.to_vec();
    boxes.sort_by_key(|mb| mb.account() != prefer);
    boxes.into_iter().find_map(|mb| {
        let path = mb.live_mbox_path(state);
        if !path.exists() {
            return None;
        }
        crate::mbox_index::fetch(&path, message_id, cap)
            .ok()
            .map(|raw| (mb, raw))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_the_first_mailbox_keeps_its_names_and_added_ones_get_their_own() {
        let work = "jon@work.example";
        let id = id_for(work);
        assert_eq!(id, id_for("  JON@Work.Example "), "the id ignores case");
        let (user, pass) = added_env_names(&id);
        let found = from_vars(vars(&[
            (FIRST_USER, "me@mail.example"),
            (FIRST_PASSWORD, "abcd efgh"),
            (&user, work),
            (&pass, "wxyz"),
            // a name that doesn't match its address: ignored
            ("HEARTHLOOM_MAIL_00000000_USER", "else@x.example"),
            ("HEARTHLOOM_MAIL_00000000_PASSWORD", "p"),
            // an emptied (disconnected) one: ignored
            (&added_env_names(&id_for("gone@x.example")).0, ""),
            ("GMAIL_IMAP_BACKFILL_DAYS", "30"),
        ]));
        assert_eq!(found.len(), 2, "{found:?}");
        let (first, added) = (&found[0], &found[1]);
        assert!(first.first && !added.first);
        assert_eq!(first.source_name(), "gmail-live");
        assert_eq!(first.key("gmail.watch"), "gmail.watch");
        assert_eq!(added.source_name(), format!("gmail-live-{id}"));
        assert_eq!(added.key("gmail.watch"), format!("gmail.watch.{id}"));
        assert_eq!(added.id_key(KEY_ALIASES), format!("mail.aliases.{id}"));
        assert_eq!(added.password(), "wxyz");
        assert!(
            !format!("{added:?}").contains("wxyz"),
            "Debug never prints it"
        );
        assert!(is_live_archive(&first.source_name()) && is_live_archive(&added.source_name()));
        assert!(!is_live_archive("gmail") && !is_live_archive("takeout-2024"));
    }

    #[test]
    fn test_a_connect_keeps_an_address_in_its_slot() {
        let none: Vec<Mailbox> = Vec::new();
        // a node with no mailbox: the first slot, so an older build reads it
        let (first, _, names) = slot_for(&none, "me@mail.example");
        assert!(first);
        assert_eq!(names.0, FIRST_USER);
        let have = from_vars(vars(&[
            (FIRST_USER, "me@mail.example"),
            (FIRST_PASSWORD, "p"),
        ]));
        // the same address again: the same slot, whatever its case
        assert!(slot_for(&have, "ME@mail.example").0);
        // another: an added slot named after its id
        let (first, id, names) = slot_for(&have, "jon@work.example");
        assert!(!first);
        assert_eq!(id, id_for("jon@work.example"));
        assert_eq!(names, added_env_names(&id));
    }

    #[test]
    fn test_aliases_are_addresses_once_each() {
        let given = vec![
            "Legal@Work.example".to_string(),
            "legal@work.example".into(),
            " <info@work.example> ".into(),
            "jon@work.example".into(), // the mailbox itself
            "".into(),
        ];
        assert_eq!(
            clean_aliases("jon@work.example", &given).unwrap(),
            vec!["legal@work.example", "info@work.example"]
        );
        assert!(clean_aliases("jon@work.example", &["not an address".into()]).is_err());
        assert!(clean_aliases("jon@work.example", &["a@b@c.example".into()]).is_err());
        assert!(clean_aliases("jon@work.example", &["a,b@c.example".into()]).is_err());
    }

    fn node(id: &str, seen: &str, roles: Value) -> Value {
        json!({"node_id": id, "last_seen": seen, "roles": roles})
    }

    #[test]
    fn test_each_mailbox_is_handled_by_one_node() {
        let cutoff = "2026-11-06T10:00:00Z";
        let fresh = "2026-11-06T10:05:00Z";
        let stale = "2026-11-06T09:00:00Z";
        let local = vec![
            "me@mail.example".to_string(),
            "jon@work.example".to_string(),
        ];
        // alone: both mailboxes, and anything nobody watches
        let d = duty_from("n-b", &local, &[], cutoff);
        assert_eq!(d.mine, local);
        assert!(d.handles("") && d.handles("old@x.example") && d.handles("JON@work.example"));
        // a live node with a smaller id watching the work mailbox takes it,
        // and is the fallback too
        let other = node(
            "n-a",
            fresh,
            json!({"mail": true, "mailboxes": [{"address": "jon@work.example", "watching": true}]}),
        );
        let d = duty_from("n-b", &local, std::slice::from_ref(&other), cutoff);
        assert_eq!(d.mine, vec!["me@mail.example"]);
        assert!(d.unwatched.is_none());
        assert!(!d.handles("jon@work.example") && !d.handles(""));
        // ...unless it went quiet
        let quiet = node("n-a", stale, other["roles"].clone());
        let d = duty_from("n-b", &local, &[quiet], cutoff);
        assert_eq!(d.mine, local);
        // the smaller node watching it with the watch off doesn't count
        let off = node(
            "n-a",
            fresh,
            json!({"mail": false, "mailboxes": [{"address": "jon@work.example", "watching": false}]}),
        );
        assert_eq!(duty_from("n-b", &local, &[off], cutoff).mine, local);
        // an older node (no roles.mailboxes) watching mail with a smaller id
        // handles everything, as it does itself
        let older = node("n-a", fresh, json!({"mail": true}));
        assert!(duty_from("n-b", &local, std::slice::from_ref(&older), cutoff).is_idle());
        // ...but not when its id is larger
        let d = duty_from("n-0", &local, &[older], cutoff);
        assert_eq!(d.mine, local);
        // a node watching nothing handles nothing
        assert!(duty_from("n-b", &[], &[], cutoff).is_idle());
    }
}

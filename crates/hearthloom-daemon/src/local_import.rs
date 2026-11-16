//! Readying a local import's drafts for the store — every path that reads
//! this machine's files (the idle refresh, `/v1/ingest/local`, the inbox,
//! the CLI through `/v1/ingest`) passes its drafts through [`prepare`]:
//!
//! - A day or session of a growing source (`GROWING_SOURCES`: a Chrome or
//!   iMessage day, a Claude Code session) lands named by its count
//!   (`<base>#r<n>`) — the first read too, so two nodes' first reads of
//!   one day that hold different numbers of items are different copies,
//!   and each node keeps the fuller one. One already stored lands again
//!   only when it holds more than the fullest stored copy; the store then
//!   keeps that copy live. A read holding no more (history Chrome has
//!   since expired, a deleted message) changes nothing. Unversioned refs
//!   (`<base>`) are what earlier versions stored.
//! - A Chrome day keyed by its profile is left out when the day an earlier
//!   version stored under the profile's shown name already holds every
//!   visit of it — so upgrading stores no day twice. One that holds more
//!   lands under the profile's key, and the store puts the named day
//!   behind it (store/growing.rs); a same-named day it doesn't hold (another
//!   profile's, on this machine or another) stays in view.
//! - A git commit an earlier version stored under its repository's address
//!   as `git remote` gave it (a credential in it) is left out: the clean
//!   address names the same commit ([`FormerAddresses`]). An import labelled
//!   private names it under that address too ([`private_refs`]), so the
//!   stored copy goes into the vault.
//!
//! Peers' batches never pass through here: they replicate copies already
//! decided on the node that read the files. Callers run this on a blocking
//! thread: each lookup takes the store briefly, with a pause after every
//! [`BATCH`].

use hearthloom_core::store::{chrome_day_holds, chrome_named_ref, growing_copy, growing_items};
use hearthloom_core::{EpisodeDraft, Home, Store};

/// Lookups between pauses, so a large import never keeps the store from
/// the requests waiting on it.
const BATCH: usize = 100;

pub(crate) fn prepare(
    store: &Store,
    home: &Home,
    drafts: Vec<EpisodeDraft>,
) -> anyhow::Result<Vec<EpisodeDraft>> {
    let mut out = Vec::with_capacity(drafts.len());
    let mut looked = 0usize;
    // read once a commit comes along
    let mut former_addresses: Option<FormerAddresses> = None;
    for mut draft in drafts {
        if draft.source == "git" {
            let former = match &mut former_addresses {
                Some(former) => former,
                None => former_addresses.insert(FormerAddresses::read(store)?),
            };
            if former.holds(store, &draft, &mut looked)? {
                continue;
            }
            out.push(draft);
            continue;
        }
        // a draft that already names a revision passes as it is
        let Some((base, 0)) = growing_copy(&draft.source, &draft.source_ref) else {
            out.push(draft);
            continue;
        };
        if looked > 0 && looked.is_multiple_of(BATCH) {
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        looked += 1;
        let base = base.to_string();
        match store.growing_latest(home, &draft.source, &base)? {
            None => {
                if named_day_holding(store, home, &draft)?.is_some() {
                    continue;
                }
                // named by what it holds, like every later read: a first
                // read of the same day or session on another node that
                // holds a different number of items is then a copy of its
                // own, so each node receives the other's and keeps the
                // fuller one live (one shared ref would let each keep its
                // own text forever)
                if let Some(n) = growing_items(&draft.source, &draft.body).filter(|n| *n > 0) {
                    draft.source_ref = format!("{base}#r{n}");
                }
                out.push(draft);
            }
            Some(stored) => {
                if stored.content_hash == hearthloom_core::ids::content_hash(draft.body.as_bytes())
                {
                    continue;
                }
                if let Some(now) = growing_items(&draft.source, &draft.body) {
                    if now > stored.items {
                        draft.source_ref = format!("{base}#r{now}");
                        out.push(draft);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// For a Chrome day keyed by its profile: the day an earlier version
/// stored under the profile's shown name, when that day holds every visit
/// of this one — this day's earlier copy.
fn named_day_holding(
    store: &Store,
    home: &Home,
    draft: &EpisodeDraft,
) -> anyhow::Result<Option<String>> {
    let Some(named) = chrome_named_ref(&draft.source_ref, &draft.body) else {
        return Ok(None);
    };
    let held = store
        .growing_latest(home, "chrome", &named)?
        .and_then(|day| day.body(home))
        .is_some_and(|day| chrome_day_holds(&day, &draft.body));
    Ok(held.then_some(named))
}

/// The addresses earlier versions stored repositories under: a commit's
/// ref is `<address>#<sha>`, and before remote addresses lost their
/// credentials (`git::without_credentials`) the address was kept as `git
/// remote` gave it — `https://user:token@host/…`. A commit stored under
/// such an address is the same commit: storing it again under the clean
/// address would store the repository's whole history twice.
struct FormerAddresses(std::collections::HashMap<String, Vec<String>>);

impl FormerAddresses {
    /// Each clean address, with the stored addresses that clean to it.
    fn read(store: &Store) -> anyhow::Result<Self> {
        let mut former: std::collections::HashMap<String, Vec<String>> = Default::default();
        for stored in store.known_repo_urls()? {
            let clean = hearthloom_adapters::git::without_credentials(&stored);
            if clean != stored {
                former.entry(clean).or_default().push(stored);
            }
        }
        Ok(FormerAddresses(former))
    }

    /// The refs this commit (`<address>#<sha>`) has under its repository's
    /// former addresses.
    fn refs_of<'a>(&'a self, source_ref: &'a str) -> impl Iterator<Item = String> + 'a {
        source_ref
            .rsplit_once('#')
            .into_iter()
            .flat_map(move |(address, sha)| {
                self.0
                    .get(address)
                    .into_iter()
                    .flatten()
                    .map(move |former| format!("{former}#{sha}"))
            })
    }

    /// Is this commit stored under a former address of its repository?
    fn holds(
        &self,
        store: &Store,
        draft: &EpisodeDraft,
        looked: &mut usize,
    ) -> anyhow::Result<bool> {
        for former in self.refs_of(&draft.source_ref) {
            if *looked > 0 && looked.is_multiple_of(BATCH) {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            *looked += 1;
            if store.episode_exists("git", &former)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// (source, source ref) of the drafts an import labels private — taken
/// before [`prepare`], which leaves out a day or session stored before
/// whose copy may still sit outside the vault, a Chrome day an earlier
/// version stored under the profile's shown name, and a commit stored
/// under a former address of its repository: that day, and that commit
/// under each of them, are named too.
pub(crate) fn private_refs(
    store: &Store,
    home: &Home,
    drafts: &[EpisodeDraft],
) -> anyhow::Result<Vec<(String, String)>> {
    // read once a private commit comes along
    let mut former_addresses: Option<FormerAddresses> = None;
    let mut refs = Vec::new();
    let mut looked = 0usize;
    for draft in drafts
        .iter()
        .filter(|d| d.scope.as_deref() == Some("private"))
    {
        refs.push((draft.source.clone(), draft.source_ref.clone()));
        if draft.source == "chrome" {
            if looked > 0 && looked.is_multiple_of(BATCH) {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            looked += 1;
            if let Some(named) = named_day_holding(store, home, draft)? {
                refs.push(("chrome".to_string(), named));
            }
        }
        if draft.source == "git" {
            let former = match &mut former_addresses {
                Some(former) => former,
                None => former_addresses.insert(FormerAddresses::read(store)?),
            };
            refs.extend(
                former
                    .refs_of(&draft.source_ref)
                    .map(|former| ("git".to_string(), former)),
            );
        }
    }
    Ok(refs)
}

/// What the owner's import read as private — its drafts labelled private,
/// and the `flagged` records it made no draft of — moves the copies stored
/// before into the vault (`Store::label_private_on_reimport`), and the
/// owner's other nodes hear of it at once (`privacy.changed`), as after a
/// relabel by hand — also when the import's own apply already moved them
/// (`landed`: its reports' `private_copies`, a fuller copy that landed
/// private taking the copies of its day or session with it). Only for the
/// owner's own imports. Call from a blocking thread of the runtime. Returns
/// the records relabelled, with the other copies of their records the label
/// reached.
pub(crate) fn vault_stored_copies(
    state: &crate::AppState,
    actor: &hearthloom_core::Actor,
    drafts: &[EpisodeDraft],
    flagged: &[(String, String)],
    landed: usize,
) -> anyhow::Result<usize> {
    let moved = state
        .store
        .label_private_on_reimport(actor, drafts, flagged)?;
    if moved + landed > 0 {
        crate::bus::touch(state, "privacy.changed");
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::{EventTime, IngestBatch, TimeUncertainty};
    use std::time::{Duration, SystemTime};

    fn node() -> (Store, Home, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        (store, home, tmp)
    }

    /// What an import path does: prepare, then apply.
    fn import(store: &Store, home: &Home, actor: &str, drafts: Vec<EpisodeDraft>) -> usize {
        let drafts = prepare(store, home, drafts).unwrap();
        apply(store, home, actor, drafts)
    }

    fn apply(store: &Store, home: &Home, actor: &str, drafts: Vec<EpisodeDraft>) -> usize {
        store
            .apply_batch(
                home,
                &IngestBatch {
                    actor: actor.into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap()
            .episodes_new
    }

    /// (source_ref, body) of the live episodes of `source`.
    fn live(store: &Store, home: &Home, source: &str) -> Vec<(String, String)> {
        let (_, rows) = store
            .read_query(
                &format!(
                    "SELECT source_ref, content_ref FROM episodes
                     WHERE source = '{source}' AND superseded_by IS NULL ORDER BY source_ref"
                ),
                100,
            )
            .unwrap();
        rows.iter()
            .map(|r| {
                let body =
                    std::fs::read_to_string(home.root().join(r[1].as_str().unwrap())).unwrap();
                (r[0].as_str().unwrap().to_string(), body)
            })
            .collect()
    }

    fn turn(kind: &str, text: &str) -> String {
        serde_json::json!({"type": kind, "sessionId": "sess-1", "cwd": "/Users/you/proj",
            "timestamp": "2026-11-02T10:00:00.000Z",
            "message": {"role": kind, "content": text}})
        .to_string()
    }

    fn read_sessions(dir: &std::path::Path) -> Vec<EpisodeDraft> {
        let after =
            SystemTime::now() + hearthloom_adapters::claude_code::QUIET + Duration::from_secs(60);
        hearthloom_adapters::claude_code::ingest_window(
            dir,
            None,
            &[],
            hearthloom_adapters::claude_code::Window::at(after, None),
            None,
        )
        .unwrap()
        .drafts
    }

    fn commit(address: &str, sha: &str, scope: Option<&str>) -> EpisodeDraft {
        EpisodeDraft {
            source: "git".into(),
            source_ref: format!("{address}#{sha}"),
            kind: "commit".into(),
            title: format!("commit {sha}"),
            body: format!("commit {sha}\nrepo: {address}\n"),
            event_time: EventTime::unknown(),
            trust: "imported".into(),
            scope: scope.map(String::from),
            entities: vec![hearthloom_core::EntityRef {
                entity_type: "Project".into(),
                name: "r".into(),
                attributes: vec![("repo-url".into(), address.into())],
                role: "repo".into(),
            }],
            edges: vec![],
        }
    }

    /// A commit an earlier version stored under its repository's address
    /// with the credential in it isn't stored again under the clean
    /// address; a commit not stored before is, and another repository's
    /// commit of the same id isn't held back.
    #[test]
    fn test_a_commit_stored_under_its_former_address_is_not_stored_again() {
        let (store, home, _tmp) = node();
        let commit = |address: &str, sha: &str| commit(address, sha, None);
        let former = "https://user:tok@git.example/o/r.git";
        let clean = "https://git.example/o/r.git";
        let other = "https://git.example/o/fork.git";
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        assert_eq!(
            apply(&store, &home, "adapter:git", vec![commit(former, &a)]),
            1
        );
        let landed = import(
            &store,
            &home,
            "adapter:git",
            vec![commit(clean, &a), commit(clean, &b), commit(other, &a)],
        );
        assert_eq!(landed, 2);
        let mut refs: Vec<String> = live(&store, &home, "git")
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        refs.sort();
        let mut expected = vec![
            format!("{former}#{a}"),
            format!("{clean}#{b}"),
            format!("{other}#{a}"),
        ];
        expected.sort();
        assert_eq!(refs, expected);
    }

    /// The same, imported again as private: the commit stored under the
    /// former address goes into the vault, though the import stores
    /// nothing under the clean one.
    #[test]
    fn test_a_private_reimport_vaults_a_commit_stored_under_its_former_address() {
        let (store, home, _tmp) = node();
        let former = "https://user:tok@git.example/o/r.git";
        let clean = "https://git.example/o/r.git";
        let sha = "a".repeat(40);
        assert_eq!(
            apply(
                &store,
                &home,
                "adapter:git",
                vec![commit(former, &sha, None)]
            ),
            1
        );
        let drafts = vec![commit(clean, &sha, Some("private"))];
        let private = private_refs(&store, &home, &drafts).unwrap();
        assert_eq!(import(&store, &home, "adapter:git", drafts), 0);
        let moved = store
            .label_private_on_reimport(&hearthloom_core::Actor::new("adapter:git"), &[], &private)
            .unwrap();
        assert_eq!(moved, 1);
        store.vault_unlock(15);
        let (_, rows) = store
            .read_query(
                "SELECT source_ref, scope FROM episodes /* all-episodes: a label check */
                 WHERE source = 'git'",
                10,
            )
            .unwrap();
        store.vault_lock();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0].as_str(),
            Some(format!("{former}#{sha}").as_str())
        );
        assert_eq!(rows[0][1].as_str(), Some("private"));
    }

    /// A session left quiet long enough to be stored, then picked up
    /// again: the later turns land, as a fuller copy that replaces the
    /// first; reading it again unchanged adds nothing. A session stored by
    /// an earlier version (no turn count in its text) counts its turns.
    #[test]
    fn test_a_session_picked_up_again_lands_its_new_turns() {
        let (store, home, tmp) = node();
        let projects = tmp.path().join("projects");
        let proj = projects.join("-Users-you-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join("sess-1.jsonl");
        let mut lines = vec![
            turn("user", "Plan the release"),
            turn("assistant", "Here is a plan"),
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();
        // as an earlier version stored it
        let mut first = read_sessions(&projects);
        first[0].body = first[0].body.replace("turns: 2\n", "");
        assert_eq!(apply(&store, &home, "adapter:claude-code", first), 1);
        // the same session read now: nothing new
        assert_eq!(
            import(
                &store,
                &home,
                "adapter:claude-code",
                read_sessions(&projects)
            ),
            0
        );

        lines.push(turn("user", "Now ship it"));
        lines.push(turn("assistant", "Shipped"));
        std::fs::write(&file, lines.join("\n")).unwrap();
        assert_eq!(
            import(
                &store,
                &home,
                "adapter:claude-code",
                read_sessions(&projects)
            ),
            1
        );
        let kept = live(&store, &home, "claude-code");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "sess-1#r4");
        assert!(kept[0].1.contains("you: Now ship it"));

        // read again as it is: nothing new
        assert_eq!(
            import(
                &store,
                &home,
                "adapter:claude-code",
                read_sessions(&projects)
            ),
            0
        );
        assert_eq!(live(&store, &home, "claude-code").len(), 1);
    }

    fn chat_db(path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, date INTEGER, text TEXT,
                 is_from_me INTEGER, handle_id INTEGER);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, display_name TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             INSERT INTO handle VALUES (1, '+15551234567');
             INSERT INTO chat VALUES (1, '+15551234567', '');",
        )
        .unwrap();
        conn
    }

    fn message(conn: &rusqlite::Connection, id: i64, apple_ns: i64, text: &str) {
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, 0, 1)",
            rusqlite::params![id, apple_ns, text],
        )
        .unwrap();
        conn.execute("INSERT INTO chat_message_join VALUES (1, ?1)", [id])
            .unwrap();
    }

    /// A chat-day stored with part of its messages (imported before the
    /// rest were sent, or before they synced in) gains them on a later
    /// import; an import that holds fewer leaves the stored day alone.
    #[test]
    fn test_a_partly_stored_day_gains_its_later_messages() {
        let (store, home, tmp) = node();
        let db = tmp.path().join("chat.db");
        let conn = chat_db(&db);
        // 2026-03-16 UTC
        message(&conn, 1, 795362400000000000, "lunch?");
        let read = || hearthloom_adapters::imessage::ingest(&db, None).unwrap();
        assert_eq!(import(&store, &home, "adapter:imessage", read()), 1);

        message(&conn, 2, 795362460000000000, "yes! noon");
        message(&conn, 3, 795362520000000000, "see you there");
        assert_eq!(import(&store, &home, "adapter:imessage", read()), 1);
        let kept = live(&store, &home, "imessage");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "+15551234567#2026-03-16#r3");
        assert!(kept[0].1.contains("see you there"));

        // a message deleted since: the fuller copy stays
        conn.execute("DELETE FROM message WHERE ROWID = 3", [])
            .unwrap();
        assert_eq!(import(&store, &home, "adapter:imessage", read()), 0);
        assert_eq!(
            live(&store, &home, "imessage")[0].0,
            "+15551234567#2026-03-16#r3"
        );
    }

    /// A chat-day an earlier version stored (no count line) that holds a
    /// message with lines shaped like messages — a pasted chat log — still
    /// gains the messages sent after it: those lines weigh the same in
    /// every read of the day.
    #[test]
    fn test_a_day_holding_a_pasted_log_still_gains_its_later_messages() {
        let (store, home, tmp) = node();
        let db = tmp.path().join("chat.db");
        let conn = chat_db(&db);
        let pasted: String = (0..50).map(|i| format!("\n[10:{i:02}] x: y")).collect();
        message(&conn, 1, 795362400000000000, &format!("look{pasted}"));
        message(&conn, 2, 795362460000000000, "ok");
        let read = || hearthloom_adapters::imessage::ingest(&db, None).unwrap();
        // the day as an earlier version wrote it: the text without its count
        let mut stored = read();
        let body = stored[0].body.clone();
        stored[0].body = body.split_once('\n').unwrap().1.to_string();
        assert!(!stored[0].body.contains("messages: "));
        assert_eq!(apply(&store, &home, "adapter:imessage", stored), 1);

        message(&conn, 3, 795362520000000000, "one more");
        assert_eq!(import(&store, &home, "adapter:imessage", read()), 1);
        let kept = live(&store, &home, "imessage");
        assert_eq!(kept.len(), 1);
        assert!(kept[0].1.contains("one more"));
    }

    /// Today's messages land with the import that reads them, and the next
    /// import — from the live database or a copy of it — adds the ones sent
    /// since: nothing waits on an import that may never come.
    #[test]
    fn test_todays_messages_land_now_and_grow_on_the_next_import() {
        let (store, home, tmp) = node();
        let db = tmp.path().join("chat.db");
        let conn = chat_db(&db);
        let apple_ns = |unix: i64| (unix - 978_307_200) * 1_000_000_000;
        let now = chrono::Utc::now().timestamp();
        message(&conn, 1, apple_ns(now - 120), "on my way");
        let read =
            |path: &std::path::Path| hearthloom_adapters::imessage::ingest(path, None).unwrap();
        assert_eq!(import(&store, &home, "adapter:imessage", read(&db)), 1);
        assert!(live(&store, &home, "imessage")[0].1.contains("on my way"));

        message(&conn, 2, apple_ns(now - 60), "here");
        let copy = tmp.path().join("copy/chat.db");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::copy(&db, &copy).unwrap();
        assert_eq!(import(&store, &home, "adapter:imessage", read(&copy)), 1);
        let kept = live(&store, &home, "imessage");
        assert_eq!(kept.len(), 1);
        assert!(kept[0].1.contains("on my way") && kept[0].1.contains("here"));
    }

    fn chrome_day(source_ref: &str, name: &str, date: &str, visits: &[&str]) -> EpisodeDraft {
        let mut body = format!(
            "browsing ({name}) on {date}: {} visits\n\ntop sites: example.com ({})\n\n",
            visits.len(),
            visits.len()
        );
        for v in visits {
            body.push_str(&format!("[{v}] Example — example.com\n"));
        }
        EpisodeDraft {
            source: "chrome".into(),
            source_ref: source_ref.into(),
            kind: "browsing".into(),
            title: format!("Browsing ({name}): example.com ({})", visits.len()),
            body,
            event_time: EventTime {
                utc: Some(format!("{date}T{}:00Z", visits[0])),
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Hour,
            },
            trust: "self".into(),
            scope: None,
            entities: vec![],
            edges: vec![],
        }
    }

    /// Days an earlier version stored under a profile's shown name: a
    /// profile's day they hold already isn't stored again; one that grew
    /// replaces it; a same-named profile's day (here, imported on its own,
    /// or another machine's) is stored beside it, never in its place.
    #[test]
    fn test_days_stored_under_a_profile_name_are_taken_over_only_by_their_profile() {
        let (store, home, _tmp) = node();
        let date = "2026-11-10";
        // what earlier versions stored here
        apply(
            &store,
            &home,
            "adapter:chrome",
            vec![
                chrome_day("Work#2026-11-10", "Work", date, &["09:00", "10:00"]),
                chrome_day("Personal#2026-11-10", "Personal", date, &["20:00"]),
            ],
        );
        // and what another machine stored under a name used here too
        apply(
            &store,
            &home,
            "peer:laptop",
            vec![chrome_day("Laptop#2026-11-10", "Laptop", date, &["07:00"])],
        );

        // a second "Work" profile imported on its own first: its day is
        // its own, the stored one stays
        let other_work = chrome_day("profile:5#2026-11-10", "Work", date, &["15:00"]);
        assert_eq!(import(&store, &home, "adapter:chrome", vec![other_work]), 1);
        let read = vec![
            // the Work day, whole now
            chrome_day(
                "profile:1#2026-11-10",
                "Work",
                date,
                &["09:00", "10:00", "11:00"],
            ),
            // two profiles named Personal: the stored day is the first's
            chrome_day("profile:2#2026-11-10", "Personal", date, &["20:00"]),
            chrome_day(
                "profile:3#2026-11-10",
                "Personal",
                date,
                &["08:00", "21:00"],
            ),
            // this machine's own profile named Laptop
            chrome_day("profile:4#2026-11-10", "Laptop", date, &["13:00"]),
        ];
        assert_eq!(import(&store, &home, "adapter:chrome", read.clone()), 3);
        let refs: Vec<String> = live(&store, &home, "chrome")
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(
            refs,
            [
                "Laptop#2026-11-10",
                "Personal#2026-11-10",
                "profile:1#2026-11-10#r3",
                "profile:3#2026-11-10#r2",
                "profile:4#2026-11-10#r1",
                "profile:5#2026-11-10#r1",
            ]
        );
        // the same read again changes nothing
        assert_eq!(import(&store, &home, "adapter:chrome", read), 0);
    }

    /// A private import of a profile's day that a day stored under the
    /// profile's name already holds stores nothing, and takes that day into
    /// the vault; a same-named day it isn't the copy of stays out.
    #[test]
    fn test_a_private_import_vaults_the_day_stored_under_the_profile_name() {
        let (store, home, _tmp) = node();
        let date = "2026-11-10";
        apply(
            &store,
            &home,
            "adapter:chrome",
            vec![
                chrome_day("Work#2026-11-10", "Work", date, &["09:00", "10:00"]),
                chrome_day("Personal#2026-11-10", "Personal", date, &["20:00"]),
            ],
        );
        let mut drafts = vec![
            chrome_day("profile:1#2026-11-10", "Work", date, &["09:00", "10:00"]),
            chrome_day("profile:2#2026-11-10", "Personal", date, &["08:00"]),
        ];
        for d in &mut drafts {
            d.scope = Some("private".into());
        }
        let private = private_refs(&store, &home, &drafts).unwrap();
        assert_eq!(import(&store, &home, "adapter:chrome", drafts), 1);
        store
            .label_private_on_reimport(
                &hearthloom_core::Actor::new("adapter:chrome"),
                &[],
                &private,
            )
            .unwrap();
        store.vault_unlock(15);
        let (_, rows) = store
            .read_query(
                "SELECT source_ref, scope FROM episodes /* all-episodes: a label check */
                 WHERE source = 'chrome' ORDER BY source_ref",
                10,
            )
            .unwrap();
        store.vault_lock();
        let labels: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r[0].as_str().unwrap(), r[1].as_str().unwrap()))
            .collect();
        assert_eq!(
            labels,
            [
                ("Personal#2026-11-10", "personal"),
                ("Work#2026-11-10", "private"),
                ("profile:2#2026-11-10#r1", "private"),
            ]
        );
    }

    /// Two Macs that both stored a "Work" day under the name, upgraded:
    /// each one's profile day replaces its own named copy, and once they
    /// sync every node shows both profiles' days — neither hides the other.
    #[test]
    fn test_two_upgraded_machines_keep_their_same_named_days_apart() {
        let date = "2026-11-10";
        let (one, one_home, _a) = node();
        let (two, two_home, _b) = node();
        apply(
            &one,
            &one_home,
            "adapter:chrome",
            vec![chrome_day("Work#2026-11-10", "Work", date, &["09:00"])],
        );
        apply(
            &two,
            &two_home,
            "adapter:chrome",
            vec![chrome_day("Work#2026-11-10", "Work", date, &["13:00"])],
        );
        let one_day = vec![chrome_day(
            "profile:1#2026-11-10",
            "Work",
            date,
            &["09:00", "10:00"],
        )];
        let two_day = vec![chrome_day(
            "profile:2#2026-11-10",
            "Work",
            date,
            &["13:00", "14:00"],
        )];
        let mine_one = prepare(&one, &one_home, one_day).unwrap();
        let mine_two = prepare(&two, &two_home, two_day).unwrap();
        apply(&one, &one_home, "adapter:chrome", mine_one.clone());
        apply(&two, &two_home, "adapter:chrome", mine_two.clone());
        // sync both ways
        apply(&one, &one_home, "peer:two", mine_two);
        apply(&two, &two_home, "peer:one", mine_one);
        for (store, home) in [(&one, &one_home), (&two, &two_home)] {
            let refs: Vec<String> = live(store, home, "chrome")
                .into_iter()
                .map(|(r, _)| r)
                .collect();
            assert_eq!(refs, ["profile:1#2026-11-10#r2", "profile:2#2026-11-10#r2"]);
        }
    }

    /// A day an earlier version stored under the profile's shown name with
    /// one visit doesn't hold a read of two visits under the same title,
    /// though both show one line: the fuller read lands, and takes its place.
    #[test]
    fn test_a_read_with_more_visits_under_one_title_lands() {
        let (store, home, _tmp) = node();
        let date = "2026-11-10";
        apply(
            &store,
            &home,
            "adapter:chrome",
            vec![chrome_day("Work#2026-11-10", "Work", date, &["09:00"])],
        );
        let read = vec![chrome_day(
            "profile:1#2026-11-10",
            "Work",
            date,
            &["09:00", "09:00"],
        )];
        assert_eq!(import(&store, &home, "adapter:chrome", read.clone()), 1);
        let refs: Vec<String> = live(&store, &home, "chrome")
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(refs, ["profile:1#2026-11-10#r2"]);
        assert_eq!(import(&store, &home, "adapter:chrome", read), 0);
    }

    /// Two nodes that first read the same chat-day with different numbers
    /// of messages each end up showing the fuller read once they sync, in
    /// either direction; reading their own files again adds nothing.
    #[test]
    fn test_first_reads_of_one_day_on_two_nodes_converge_on_the_fuller() {
        let (one, one_home, one_tmp) = node();
        let (two, two_home, two_tmp) = node();
        let read = |tmp: &tempfile::TempDir, texts: &[&str]| {
            let db = tmp.path().join("chat.db");
            let conn = chat_db(&db);
            for (i, text) in texts.iter().enumerate() {
                message(
                    &conn,
                    i as i64 + 1,
                    795362400000000000 + i as i64 * 60_000_000_000,
                    text,
                );
            }
            drop(conn);
            move || hearthloom_adapters::imessage::ingest(&db, None).unwrap()
        };
        let read_one = read(&one_tmp, &["lunch?"]);
        let read_two = read(&two_tmp, &["lunch?", "yes! noon", "see you there"]);
        assert_eq!(import(&one, &one_home, "adapter:imessage", read_one()), 1);
        assert_eq!(import(&two, &two_home, "adapter:imessage", read_two()), 1);
        let sync = |from: &Store, from_home: &Home, to: &Store, to_home: &Home| {
            let page = from.sync_export_for(from_home, 0, 100, Some("to")).unwrap();
            to.apply_batch_from(
                to_home,
                &IngestBatch {
                    actor: "peer:from".into(),
                    session_id: None,
                    episodes: page.episodes,
                },
                &page.origins,
            )
            .unwrap();
        };
        sync(&one, &one_home, &two, &two_home);
        sync(&two, &two_home, &one, &one_home);
        for (store, home) in [(&one, &one_home), (&two, &two_home)] {
            let kept = live(store, home, "imessage");
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].0, "+15551234567#2026-03-16#r3");
            assert!(kept[0].1.contains("see you there"));
        }
        assert_eq!(import(&one, &one_home, "adapter:imessage", read_one()), 0);
        assert_eq!(import(&two, &two_home, "adapter:imessage", read_two()), 0);
    }
}

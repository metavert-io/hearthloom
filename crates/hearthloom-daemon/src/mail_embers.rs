//! Mail for embers that watch for it (0.5.93) — the daemon half of core
//! store/mail_watch.rs.
//!
//! Where Gmail is watched, each new message the pull sees is checked
//! against every ember's watches (`on_seen`); a watch that waits for a
//! triage class is checked when triage labels the thread (`on_classified`,
//! or at once when an owner rule decided it). Matches are queued per
//! ember and announced with `mail.matched` — a hint, no data. The ember
//! reads them over `/v1/plugins/self/mail/matches` (`deliver`), which
//! applies the owner's view for that ember and holds back anything that
//! is private now, whatever it was when it matched.
//!
//! An ember needs the `mail.watch` scope, checked on the node it calls
//! (registration and delivery). An ember installed on this node without
//! it gets nothing queued here; one installed only elsewhere is matched
//! here on the strength of its synced watches, and its own node checks
//! again when it reads.
use hearthloom_core::store::mail::{MailSeen, MailThread};
use hearthloom_core::store::mail_watch::{EmberView, MailMatch, MailWatch, WatchInput};
use serde_json::{json, Value};

use crate::AppState;

pub const SCOPE: &str = "mail.watch";
pub const TOPIC: &str = "mail.matched";

/// Whether an ember installed here holds the scope.
pub fn granted(state: &AppState, plugin: &str) -> bool {
    matches!(state.store.plugin(plugin), Ok(Some(row)) if row.granted_scopes.iter().any(|s| s == SCOPE))
}

/// The watches this node matches for: enabled, and not withheld by an
/// install here that lacks the scope.
fn live_watches(state: &AppState) -> Vec<MailWatch> {
    state
        .store
        .mail_watches(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|w| w.enabled)
        .filter(|w| match state.store.plugin(&w.plugin) {
            Ok(Some(row)) => row.granted_scopes.iter().any(|s| s == SCOPE),
            Ok(None) => true,
            Err(_) => false,
        })
        .collect()
}

/// A class the thread has been given (not "waiting for triage").
fn decided(thread: Option<&MailThread>) -> Option<&MailThread> {
    thread.filter(|t| !matches!(t.state.as_str(), "new" | "replied" | "done"))
}

fn queue(watch: &MailWatch, seen: &MailSeen, thread: Option<&MailThread>) -> MailMatch {
    let decided = decided(thread);
    MailMatch {
        plugin: watch.plugin.clone(),
        watch: watch.id.clone(),
        watch_name: watch.name.clone(),
        thread_id: thread.map(|t| t.id.clone()),
        message_id: seen.message_id.clone(),
        sender: seen.from.clone(),
        subject: seen.subject.clone(),
        date: seen.date.clone(),
        labels: seen.labels.clone(),
        class: decided.map(|t| t.state.clone()),
        why: decided.and_then(|t| t.why.clone()),
        text: seen.text.clone(),
        ..Default::default()
    }
}

fn input(seen: &MailSeen) -> WatchInput<'_> {
    WatchInput {
        from: &seen.from,
        from_addr: &seen.from_addr,
        subject: &seen.subject,
        text: &seen.text,
        labels: &seen.labels,
    }
}

/// New mail: queue it for the watches it matches. `thread` is
/// `Some(None)` when the message joined a thread this caller can't see —
/// a private one — and nothing is queued then.
pub fn on_seen(state: &AppState, seen: &MailSeen, thread: Option<Option<&MailThread>>) -> usize {
    if seen.private || seen.from_me {
        return 0;
    }
    let thread = match thread {
        Some(None) => return 0,
        Some(Some(t)) if t.scope == "private" => return 0,
        Some(t) => t,
        None => None,
    };
    let class = decided(thread).map(|t| t.state.as_str());
    let mut queued = 0;
    for w in live_watches(state) {
        // a watch waiting for a class is checked once the thread has one
        if w.filter.needs_class() && class.is_none() {
            continue;
        }
        if !w.filter.matches(&input(seen), class) {
            continue;
        }
        match state.store.mail_match_add(&queue(&w, seen, thread)) {
            Ok(true) => queued += 1,
            Ok(false) => {}
            Err(err) => tracing::warn!("mail: a match for {} didn't queue: {err:#}", w.plugin),
        }
    }
    if queued > 0 {
        crate::bus::touch(state, TOPIC);
    }
    queued
}

/// Triage labelled a thread: the watches waiting for a class look at
/// its last message (read again from the live archive, labels and all).
pub fn on_classified(state: &AppState, thread: &MailThread) -> usize {
    if thread.scope == "private" {
        return 0;
    }
    let waiting: Vec<MailWatch> = live_watches(state)
        .into_iter()
        .filter(|w| w.filter.needs_class())
        .collect();
    if waiting.is_empty() {
        return 0;
    }
    let (Some(id), Some(account)) = (&thread.last_message_id, crate::gmail_search::account())
    else {
        return 0;
    };
    let mbox = crate::gmail_pull::live_mbox_path(state);
    let Ok(raw) = crate::mbox_index::fetch(&mbox, id, 2_000_000) else {
        return 0;
    };
    let labels = hearthloom_adapters::mbox::gmail_labels(&raw);
    let Some(seen) = crate::mail_triage::seen(state, &account, &labels, raw.as_bytes()) else {
        return 0;
    };
    if seen.private || seen.from_me {
        return 0;
    }
    let mut queued = 0;
    for w in waiting {
        if w.filter.matches(&input(&seen), Some(thread.state.as_str()))
            && state
                .store
                .mail_match_add(&queue(&w, &seen, Some(thread)))
                .unwrap_or(false)
        {
            queued += 1;
        }
    }
    if queued > 0 {
        crate::bus::touch(state, TOPIC);
    }
    queued
}

/// Private now (a privacy rule, or a private memory of the message)?
/// Fails closed.
pub fn held_back(state: &AppState, m: &MailMatch) -> bool {
    state
        .store
        .privacy_rules_match(&format!("{}\n{}\n{}", m.sender, m.subject, m.text))
        .unwrap_or(true)
        || state
            .store
            .private_source_refs("mbox", std::slice::from_ref(&m.message_id))
            .map(|s| !s.is_empty())
            .unwrap_or(true)
}

/// One match as the ember may see it.
pub fn shown(m: &MailMatch, view: &EmberView) -> Value {
    let mut v = json!({
        "seq": m.seq,
        "id": m.id,
        "watch": m.watch_name,
        "thread": m.thread_id,
        "message_id": m.message_id,
        "from": m.sender,
        "subject": m.subject,
        "date": m.date,
        "labels": m.labels,
        "class": m.class,
        "matched_at": m.created_at,
    });
    match view.view.as_str() {
        "snippet" => {
            v["why"] = json!(m.why);
            v["text"] = json!(m
                .text
                .chars()
                .take(view.chars.max(0) as usize)
                .collect::<String>());
        }
        "full" => {
            v["why"] = json!(m.why);
            v["text"] = json!(m.text);
        }
        _ => {}
    }
    v
}

/// An ember reads its queue past its cursor.
pub fn deliver(state: &AppState, plugin: &str, after: i64, limit: usize) -> anyhow::Result<Value> {
    let view = state.store.mail_ember_view(plugin)?;
    let rows = state.store.mail_matches_after(plugin, after, limit)?;
    let next = rows.last().map(|m| m.seq).unwrap_or(after);
    let matches: Vec<Value> = rows
        .iter()
        .filter(|m| !held_back(state, m))
        .map(|m| shown(m, &view))
        .collect();
    Ok(json!({
        "matches": matches,
        "next": next,
        "view": view.view,
        "chars": if view.view == "snippet" { json!(view.chars) } else { Value::Null },
    }))
}

/// The owner's overview: every ember with watches or the scope, what it
/// watches and what it sees.
pub fn overview(state: &AppState) -> anyhow::Result<Value> {
    let watches = state.store.mail_watches(None)?;
    let mut names: std::collections::BTreeSet<String> =
        watches.iter().map(|w| w.plugin.clone()).collect();
    for row in state.store.list_plugins()? {
        if row.granted_scopes.iter().any(|s| s == SCOPE) {
            names.insert(row.name);
        }
    }
    let own: std::collections::BTreeSet<String> = state
        .store
        .mail_ember_views()?
        .into_iter()
        .map(|v| v.ember)
        .collect();
    let embers: Vec<Value> = names
        .into_iter()
        .map(|name| {
            let view = state.store.mail_ember_view(&name).ok();
            let installed = state.store.plugin(&name).ok().flatten();
            json!({
                "name": name,
                "installed_here": installed.is_some(),
                "granted": installed.map(|r| r.granted_scopes.iter().any(|s| s == SCOPE)),
                "view": view.as_ref().map(|v| v.view.clone()),
                "chars": view.as_ref().filter(|v| v.view == "snippet").map(|v| v.chars),
                "own_view": own.contains(&name),
                "watches": watches.iter().filter(|w| w.plugin == name).collect::<Vec<_>>(),
            })
        })
        .collect();
    let all = state
        .store
        .mail_ember_view(hearthloom_core::store::mail_watch::ALL_EMBERS)?;
    Ok(json!({
        "default": {"view": all.view, "chars": if all.view == "snippet" { json!(all.chars) } else { Value::Null }},
        "embers": embers,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::store::mail_watch::WatchFilter;
    use hearthloom_core::Actor;

    #[test]
    fn test_what_an_ember_sees_follows_the_owners_view() {
        let m = MailMatch {
            seq: 4,
            id: "mm_1".into(),
            watch_name: "leads".into(),
            message_id: "m1@x".into(),
            sender: "Dana <dana@x.example>".into(),
            subject: "Intro".into(),
            class: Some("opportunity".into()),
            why: Some("An introduction to a fund.".into()),
            text: "a".repeat(500),
            ..Default::default()
        };
        let headers = shown(
            &m,
            &EmberView {
                ember: "*".into(),
                view: "headers".into(),
                chars: 0,
            },
        );
        assert!(headers.get("text").is_none() && headers.get("why").is_none());
        assert_eq!(headers["from"], "Dana <dana@x.example>");
        let snippet = shown(
            &m,
            &EmberView {
                ember: "*".into(),
                view: "snippet".into(),
                chars: 300,
            },
        );
        assert_eq!(snippet["text"].as_str().unwrap().len(), 300);
        let full = shown(
            &m,
            &EmberView {
                ember: "*".into(),
                view: "full".into(),
                chars: 8000,
            },
        );
        assert_eq!(full["text"].as_str().unwrap().len(), 500);
        // an unknown view shows the least
        let odd = shown(
            &m,
            &EmberView {
                ember: "*".into(),
                view: "everything".into(),
                chars: 0,
            },
        );
        assert!(odd.get("text").is_none());
    }

    #[tokio::test]
    async fn test_matches_queue_for_the_watching_ember_and_private_mail_never_does() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path());
        home.init().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(
                hearthloom_core::store::Store::open(&home.db_path()).unwrap(),
            ),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        let ember = Actor::new("plugin:leadfinder");
        state
            .store
            .mail_watch_set(
                &ember,
                "leadfinder",
                "fund mail",
                &WatchFilter {
                    domains: vec!["fund.example".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        state
            .store
            .mail_watch_set(
                &ember,
                "leadfinder",
                "leads",
                &WatchFilter {
                    classes: vec!["opportunity".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        let seen = |id: &str, private: bool| MailSeen {
            account: "me@x.example".into(),
            message_id: id.into(),
            from: "Dana <dana@deals.fund.example>".into(),
            from_addr: "dana@deals.fund.example".into(),
            subject: "Intro".into(),
            text: "We'd love to talk about your seed round.".into(),
            private,
            ..Default::default()
        };
        // non-thread mail: the domain watch matches, the class watch waits
        assert_eq!(on_seen(&state, &seen("m1@x", false), None), 1);
        // private mail, or mail in a thread we can't see, never queues
        assert_eq!(on_seen(&state, &seen("m2@x", true), None), 0);
        assert_eq!(on_seen(&state, &seen("m3@x", false), Some(None)), 0);
        // a rule-decided opportunity thread: both watches match
        let thread = MailThread {
            id: "mt_1".into(),
            account: "me@x.example".into(),
            subject: "Intro".into(),
            last_from: "Dana".into(),
            last_from_me: false,
            last_message_id: Some("m4@x".into()),
            last_at: None,
            message_count: 1,
            snippet: String::new(),
            state: "opportunity".into(),
            why: Some("your rule: domain fund.example".into()),
            confidence: None,
            due_by: None,
            rule_id: None,
            scope: "personal".into(),
            triaged_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_eq!(
            on_seen(&state, &seen("m4@x", false), Some(Some(&thread))),
            2
        );
        let got = deliver(&state, "leadfinder", 0, 50).unwrap();
        let matches = got["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(got["view"], "snippet");
        assert_eq!(matches[2]["class"], "opportunity");
        assert!(deliver(&state, "other", 0, 50).unwrap()["matches"]
            .as_array()
            .unwrap()
            .is_empty());
        // a privacy rule added later holds queued mail back
        state
            .store
            .add_privacy_rule(&Actor::new("human:test"), "term", "seed round")
            .unwrap();
        let after = deliver(&state, "leadfinder", 0, 50).unwrap();
        assert!(after["matches"].as_array().unwrap().is_empty());
        assert_eq!(
            after["next"], got["next"],
            "the cursor still moves past them"
        );
    }
}

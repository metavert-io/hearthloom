//! Where mail archives live (0.5.90).
//!
//! An archive (a Takeout mbox) lands on whatever machine it was imported
//! on — often a laptop that sleeps. Copying it to every node would cost
//! its size per node and spread raw mail around; instead each archive gets
//! a home on a node that is always on, and every other node reaches it
//! through the hub (lookups.rs).
//!
//! **Keepers.** Every node publishes whether it is always on (a hosted
//! node, or a Mac whose system sleep is off) and how much room it has. A
//! node the owner set to keep archives (`node.archives = keep`) keeps
//! them; when none is set, the always-on node with the most room does.
//! Setting a second node to keep archives gives a second copy.
//!
//! **Gather.** A keeper pulls each archive another node holds and it
//! doesn't, through the hub, in checked 4 MB chunks (`vchunk`, a lookup
//! only nodes can ask for), into `<name>.mbox.part` — resumable after a
//! sleep, a restart or a dropped link: the part file's length is where
//! it continues. Done, it registers the archive and builds its index.
//! Only archives that have stopped changing move (a live archive belongs
//! to the node that writes it).
//!
//! **Release.** A node that isn't a keeper, seeing a keeper hold the
//! same archive (same name and size), stops offering its own copy and
//! tells the owner the file can be deleted (`hearthloom source forget
//! <name> --delete`). Nothing is ever deleted without the owner.
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::Digest;

use crate::AppState;

pub const CHUNK: u64 = 4 * 1024 * 1024;
/// An archive that hasn't changed for this long can move.
const SETTLED: Duration = Duration::from_secs(24 * 3600);
pub const KEY: &str = "node.archives";
/// `archives.released.<name>` = the file a released archive left here.
pub const RELEASED: &str = "archives.released.";

/* ————— what a node says about itself ————— */

/// The owner's choice for this node: Some(true) keep archives, Some(false)
/// never, None let Hearthloom choose.
pub fn setting(state: &AppState) -> Option<bool> {
    match state
        .store
        .consolidation_state(KEY)
        .ok()
        .flatten()
        .as_deref()
    {
        Some("keep") => Some(true),
        Some("no") => Some(false),
        _ => None,
    }
}

static ALWAYS_ON: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// A hosted node, or a Mac whose system sleep is off (`pmset -g`:
/// `sleep 0`) — checked once an hour.
pub fn always_on() -> bool {
    if crate::monitor::node_kind() == "cloud" {
        return true;
    }
    let mut guard = ALWAYS_ON.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, v)) = *guard {
        if at.elapsed() < Duration::from_secs(3600) {
            return v;
        }
    }
    let v = sleep_off();
    *guard = Some((Instant::now(), v));
    v
}

fn sleep_off() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pmset")
            .arg("-g")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .is_some_and(|out| parse_sleep_off(&out))
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// `pmset -g`'s line ` sleep   0 (sleep prevented by …)` — system sleep off.
#[cfg(any(target_os = "macos", test))]
fn parse_sleep_off(out: &str) -> bool {
    out.lines().any(|l| {
        let mut words = l.split_whitespace();
        words.next() == Some("sleep") && words.next() == Some("0")
    })
}

/// What this node publishes about archives in the node list.
pub fn roles(state: &AppState) -> Value {
    json!({
        "always_on": always_on(),
        "free_bytes": crate::mbox_text::free_bytes(&state.home.root().join("archives")),
        "keeps_archives": setting(state),
        "pulling": pulling(),
    })
}

/* ————— who keeps archives ————— */

fn seen_within(last_seen: &str, d: chrono::Duration) -> bool {
    chrono::DateTime::parse_from_rfc3339(last_seen)
        .map(|t| chrono::Utc::now().signed_duration_since(t) < d)
        .unwrap_or(false)
}

fn roles_of(node: &Value) -> Value {
    match &node["roles"] {
        Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// The nodes that keep archives: the ones the owner chose (seen in the
/// last day); if none, the always-on node with the most room (seen in
/// the last hour). Every node reads the same synced list, so they agree.
pub fn keepers(nodes: &[Value]) -> Vec<String> {
    let chosen: Vec<String> = nodes
        .iter()
        .filter(|n| roles_of(n)["keeps_archives"] == json!(true))
        .filter(|n| {
            seen_within(
                n["last_seen"].as_str().unwrap_or(""),
                chrono::Duration::days(1),
            )
        })
        .filter_map(|n| n["node_id"].as_str().map(String::from))
        .collect();
    if !chosen.is_empty() {
        return chosen;
    }
    // a machine of yours before the hosted node (it pays for its storage),
    // then the most room
    let mut auto: Vec<(bool, u64, String)> = nodes
        .iter()
        .filter(|n| {
            let r = roles_of(n);
            r["always_on"] == json!(true) && r["keeps_archives"] != json!(false)
        })
        .filter(|n| {
            seen_within(
                n["last_seen"].as_str().unwrap_or(""),
                chrono::Duration::hours(1),
            )
        })
        .filter_map(|n| {
            Some((
                n["kind"] == "cloud",
                roles_of(n)["free_bytes"].as_u64().unwrap_or(0),
                n["node_id"].as_str()?.to_string(),
            ))
        })
        .collect();
    auto.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    auto.into_iter().take(1).map(|(_, _, id)| id).collect()
}

/* ————— an archive's file ————— */

/// (size, settled) of an archive file: settled = unchanged for a day and
/// not the live Gmail archive this node writes to.
pub fn file_facts(path: &Path, name: &str) -> Option<(u64, bool)> {
    let meta = std::fs::metadata(path).ok()?;
    let quiet = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age >= SETTLED);
    // a mailbox's live archive is still being written (mailboxes.rs)
    Some((
        meta.len(),
        quiet && !crate::mailboxes::is_live_archive(name),
    ))
}

/// Serve one checked chunk of an archive this node holds (a `vchunk`
/// lookup another of the owner's nodes asked for): base64 bytes, their
/// sha256, and the archive's size.
pub fn chunk(state: &AppState, args: &Value) -> Result<String> {
    let name = args["source"]
        .as_str()
        .ok_or_else(|| anyhow!("vchunk needs args.source"))?;
    let def = crate::virtual_source::resolve(state, name)?;
    let path = crate::virtual_source::mbox_path_if_online(&def)
        .ok_or_else(|| anyhow!("{name} isn't a mail archive online here"))?;
    let offset = args["offset"].as_u64().unwrap_or(0);
    let want = args["length"].as_u64().unwrap_or(CHUNK).min(CHUNK);
    let mut file = std::fs::File::open(&path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; want.min(size.saturating_sub(offset)) as usize];
    file.read_exact(&mut buf)?;
    Ok(json!({
        "b64": base64::engine::general_purpose::STANDARD.encode(&buf),
        "sha256": hex(&sha2::Sha256::digest(&buf)),
        "size": size,
    })
    .to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/* ————— pulling ————— */

static PULLING: Mutex<Option<HashMap<String, (u64, u64)>>> = Mutex::new(None);

fn set_pulling(name: &str, done: u64, total: u64) {
    let mut guard = PULLING.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if done >= total {
        map.remove(name);
    } else {
        map.insert(name.to_string(), (done, total));
    }
}

/// Archives this node is bringing in: [{name, done, total}].
pub fn pulling() -> Value {
    let guard = PULLING.lock().unwrap_or_else(|p| p.into_inner());
    Value::Array(
        guard
            .iter()
            .flat_map(|m| m.iter())
            .map(|(n, (d, t))| json!({"name": n, "done": d, "total": t}))
            .collect(),
    )
}

/// Write one verified chunk at `offset` of the part file.
fn write_chunk(part: &Path, offset: u64, answer: &str) -> Result<u64> {
    let v: Value = serde_json::from_str(answer)?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(v["b64"].as_str().unwrap_or(""))
        .map_err(|e| anyhow!("a chunk didn't decode: {e}"))?;
    if data.is_empty() {
        bail!("an empty chunk");
    }
    if hex(&sha2::Sha256::digest(&data)) != v["sha256"].as_str().unwrap_or("") {
        bail!("a chunk arrived damaged; it will be fetched again");
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&data)?;
    file.sync_data()?;
    Ok(data.len() as u64)
}

/// Bring one archive here from the node holding it, from where the part
/// file stops. True when it's complete and registered.
async fn pull(state: &AppState, holder: &str, archive: &Value) -> Result<bool> {
    let name = archive["name"].as_str().unwrap_or_default().to_string();
    let total = archive["bytes"].as_u64().unwrap_or(0);
    if name.is_empty() || total == 0 {
        return Ok(false);
    }
    let dir = state.home.root().join("archives");
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(format!("{name}.mbox"));
    let part = dir.join(format!("{name}.mbox.part"));
    let mut done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let started = Instant::now();
    while done < total {
        set_pulling(&name, done, total);
        let answer = crate::lookups::ask_remote(
            state,
            holder,
            "vchunk",
            &json!({"source": name, "offset": done, "length": CHUNK}),
            true,
        )
        .await?;
        let p = part.clone();
        let wrote = tokio::task::spawn_blocking(move || write_chunk(&p, done, &answer))
            .await
            .map_err(|e| anyhow!("{e}"))??;
        done += wrote;
        if started.elapsed() > Duration::from_secs(3 * 3600) {
            break; // let the pass breathe; the next one continues
        }
    }
    set_pulling(&name, done, total);
    if done < total {
        return Ok(false);
    }
    std::fs::rename(&part, &target)?;
    // register it and build its header index (minutes for a big one)
    state.store.register_virtual_source(
        &hearthloom_core::Actor::new("system:archives"),
        &name,
        "mbox",
        &json!({"mount": target.display().to_string(), "magic": false, "trust": "imported"}),
        &["search".into(), "fetch".into(), "promote".into()],
    )?;
    let t = target.clone();
    tokio::task::spawn_blocking(move || crate::mbox_index::build(&t))
        .await
        .map_err(|e| anyhow!("{e}"))??;
    let _ = state.store.notify(
        &hearthloom_core::Actor::new("system:archives"),
        "system:archives",
        "info",
        &format!("Your {name} mail archive now lives here too"),
        "It came from another of your machines; Ask on any of them can search it here.",
        None,
        Some(&format!("archive-arrived:{name}")),
    );
    crate::bus::touch(state, "notification.created");
    crate::agents::publish_self(state);
    Ok(true)
}

/// The archives this node holds: (name, path, size, settled).
fn held_here(state: &AppState) -> Vec<(String, PathBuf, u64, bool)> {
    state
        .store
        .list_virtual_sources()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|def| {
            let path = crate::virtual_source::mbox_path_if_online(&def)?;
            let (size, settled) = file_facts(&path, &def.name)?;
            Some((def.name, path, size, settled))
        })
        .collect()
}

/// One pass: a keeper gathers what it lacks; any other node releases
/// what a keeper now holds.
pub async fn pass(state: &AppState) -> Result<()> {
    let nodes = state.store.nodes()?;
    let me = crate::agents::node_id(state);
    let keepers = keepers(&nodes);
    let here = held_here(state);
    let elsewhere = crate::lookups::archives_elsewhere(state);
    if keepers.contains(&me) {
        for (holder, _, seen, archive) in &elsewhere {
            let moveable = archive["adapter"] == "mbox"
                && archive["settled"] == json!(true)
                && archive["bytes"].as_u64().unwrap_or(0) > 0;
            let held = here.iter().any(|(n, _, size, _)| {
                Some(n.as_str()) == archive["name"].as_str()
                    && Some(*size) == archive["bytes"].as_u64()
            });
            if moveable && !held && crate::lookups::recently_seen(seen) {
                let room = crate::mbox_text::free_bytes(&state.home.root().join("archives"))
                    .is_none_or(|free| free > archive["bytes"].as_u64().unwrap_or(0) * 2);
                if !room {
                    continue;
                }
                pull(state, holder, archive).await?;
                return Ok(()); // one archive per pass
            }
        }
    } else {
        for (name, path, size, settled) in &here {
            let kept = elsewhere.iter().any(|(id, _, _, a)| {
                keepers.contains(id)
                    && a["name"].as_str() == Some(name.as_str())
                    && a["bytes"].as_u64() == Some(*size)
            });
            if *settled && kept {
                state.store.unregister_virtual_source(
                    &hearthloom_core::Actor::new("system:archives"),
                    name,
                )?;
                // remembered so `source forget <name> --delete` finds the file
                state.store.set_consolidation_state(
                    &format!("{RELEASED}{name}"),
                    &path.display().to_string(),
                )?;
                let _ = state.store.notify(
                    &hearthloom_core::Actor::new("system:archives"),
                    "system:archives",
                    "info",
                    &format!("Your {name} mail archive moved to an always-on machine"),
                    &format!(
                        "Ask now searches it there from any of your machines. This machine's copy \
                         ({:.1} GB at {}) can be deleted: hearthloom source forget {name} --delete",
                        *size as f64 / 1e9,
                        path.display()
                    ),
                    None,
                    Some(&format!("archive-released:{name}")),
                );
                crate::bus::touch(state, "notification.created");
                crate::agents::publish_self(state);
            }
        }
    }
    Ok(())
}

static KICK: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Run a pass now (the owner changed where archives live).
pub fn kick() {
    KICK.notify_one();
}

pub async fn run_loop(state: AppState) {
    tokio::select! {
        _ = KICK.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(90)) => {}
    }
    loop {
        if let Err(err) = pass(&state).await {
            tracing::warn!("archives: {err:#}");
        }
        tokio::select! {
            _ = KICK.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(600)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, always: bool, free: u64, keep: Option<bool>, mins_ago: i64) -> Value {
        json!({
            "node_id": id,
            "last_seen": (chrono::Utc::now() - chrono::Duration::minutes(mins_ago)).to_rfc3339(),
            "roles": json!({"always_on": always, "free_bytes": free, "keeps_archives": keep}).to_string(),
        })
    }

    #[test]
    fn test_the_always_on_node_with_room_keeps_archives_unless_the_owner_chose() {
        let gb = 1u64 << 30;
        let laptop = node("nd_laptop", false, 20 * gb, None, 1);
        let mini = node("nd_mini", true, 280 * gb, None, 1);
        let cloud = node("nd_cloud", true, 8 * gb, None, 1);
        assert_eq!(
            keepers(&[laptop.clone(), mini.clone(), cloud.clone()]),
            ["nd_mini"]
        );
        // a home server before the hosted node, even with less room
        let mut big_cloud = node("nd_cloud", true, 900 * gb, None, 1);
        big_cloud["kind"] = json!("cloud");
        assert_eq!(keepers(&[mini.clone(), big_cloud.clone()]), ["nd_mini"]);
        assert_eq!(keepers(&[laptop.clone(), big_cloud]), ["nd_cloud"]);
        // the owner's choice wins, and two choices make two copies
        let cloud_keep = node("nd_cloud", true, 8 * gb, Some(true), 1);
        let laptop_keep = node("nd_laptop", false, 20 * gb, Some(true), 30);
        assert_eq!(
            keepers(&[laptop_keep, mini.clone(), cloud_keep]),
            ["nd_laptop", "nd_cloud"]
        );
        // a node that said no, or went quiet, isn't chosen automatically
        let mini_no = node("nd_mini", true, 280 * gb, Some(false), 1);
        assert_eq!(keepers(&[mini_no, cloud.clone()]), ["nd_cloud"]);
        let mini_gone = node("nd_mini", true, 280 * gb, None, 120);
        assert_eq!(keepers(&[mini_gone, cloud]), ["nd_cloud"]);
        assert!(
            keepers(&[laptop]).is_empty(),
            "no always-on node: archives stay put"
        );
    }

    #[test]
    fn test_sleep_off_is_read_from_pmset() {
        assert!(parse_sleep_off(
            " standby 0\n sleep                0 (sleep prevented by backupd)\n"
        ));
        assert!(!parse_sleep_off(
            " sleep                1\n displaysleep 0\n"
        ));
        assert!(!parse_sleep_off(" displaysleep         0\n"));
    }

    #[test]
    fn test_chunks_are_checked_before_they_are_written() {
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("a.mbox.part");
        let data = b"From x\nSubject: hi\n\nhello\n";
        let good = json!({"b64": base64::engine::general_purpose::STANDARD.encode(data),
                          "sha256": hex(&sha2::Sha256::digest(data)), "size": 100})
        .to_string();
        assert_eq!(write_chunk(&part, 0, &good).unwrap(), data.len() as u64);
        assert_eq!(
            write_chunk(&part, data.len() as u64, &good).unwrap(),
            data.len() as u64
        );
        assert_eq!(std::fs::read(&part).unwrap().len(), data.len() * 2);
        let bad = json!({"b64": base64::engine::general_purpose::STANDARD.encode(data),
                         "sha256": "00", "size": 100})
        .to_string();
        assert!(write_chunk(&part, 0, &bad).is_err());
    }
}

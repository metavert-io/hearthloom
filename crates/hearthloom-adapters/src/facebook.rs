//! Facebook data-dump adapter: the extracted "Download Your Information"
//! archive (JSON format). Structural only — entities are minted from the
//! data itself, never via LLM extraction (facebook is deliberately
//! OUTSIDE the extraction allowlist, the mbox precedent: token budget).
//!
//! What materializes (source `facebook`, trust `imported`):
//!   fb_post    — one per post/check-in; place tags become Place entities
//!                and `[HH:MM] name (lat,lon)` body lines the location
//!                index parses (626 posts carry coordinates)
//!   fb_album   — one per photo album (name, span, photo count)
//!   fb_message — one per thread per UTC day (the iMessage bundling);
//!                DM partners always become Person entities; group-chat
//!                members only after ≥5 messages in that thread (the
//!                casual-connection guard)
//!   fb_event   — events you joined or hosted (an RSVP is weak presence
//!                evidence: no location points are emitted for events)
//!   fb_group   — your group posts/comments, bundled per group per day
//!   fb_profile — one episode carrying your own emails/phones as
//!                identity attributes (cross-source resolution)
//!
//! The friends LIST does not materialize: `friends()` feeds the
//! social_connections registry instead — 3k mostly-casual names would
//! pollute mentions/browse/merge-scan as evidence-less entities.
//!
//! Encoding: Facebook exports encode UTF-8 bytes as latin-1 escapes
//! ("ð¥°" is 🥰). `fix()` repairs every string.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use serde_json::Value;

const BODY_CAP: usize = 64 * 1024;
const GROUP_MEMBER_MIN_MESSAGES: usize = 5;
const MAX_ENTITIES_PER_EPISODE: usize = 8;

/// Repair Facebook's mojibake: JSON strings whose chars are all ≤ 0xFF
/// are latin-1-masked UTF-8 bytes.
pub fn fix(s: &str) -> String {
    if s.is_ascii() || s.chars().any(|c| c as u32 > 0xFF) {
        return s.to_string();
    }
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    String::from_utf8(bytes).unwrap_or_else(|_| s.to_string())
}

fn load_json(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn ts_to_event(ts: i64) -> EventTime {
    EventTime {
        utc: DateTime::from_timestamp(ts, 0).map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        tz_offset_minutes: None,
        uncertainty: TimeUncertainty::Hour, // export gives UTC; local offset unknown
    }
}

fn day_of(ts: i64) -> Option<String> {
    DateTime::from_timestamp(ts, 0).map(|dt| dt.format("%Y-%m-%d").to_string())
}

fn hhmm_of(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "12:00".into())
}

fn short_hash(text: &str) -> String {
    // stable tiny content hash for source_refs (fnv-1a)
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..8].to_string()
}

/// One registry row for the social-connections table.
#[derive(Debug, Clone)]
pub struct FbConnection {
    pub name: String,
    pub connected_at: Option<String>,
    pub ended_at: Option<String>,
    pub status: String, // active | removed
}

/// The friends list + removals → the registry (NOT entities).
pub fn friends(root: &Path) -> Result<Vec<FbConnection>> {
    let mut out = Vec::new();
    let friends_path = root.join("connections/friends/your_friends.json");
    if friends_path.is_file() {
        let v = load_json(&friends_path)?;
        for f in v["friends_v2"].as_array().into_iter().flatten() {
            let Some(name) = f["name"].as_str() else { continue };
            out.push(FbConnection {
                name: fix(name),
                connected_at: f["timestamp"].as_i64().and_then(day_of),
                ended_at: None,
                status: "active".into(),
            });
        }
    }
    let removed_path = root.join("connections/friends/removed_friends.json");
    if removed_path.is_file() {
        let v = load_json(&removed_path)?;
        for f in v["deleted_friends_v2"].as_array().into_iter().flatten() {
            let Some(name) = f["name"].as_str() else { continue };
            out.push(FbConnection {
                name: fix(name),
                connected_at: None,
                ended_at: f["timestamp"].as_i64().and_then(day_of),
                status: "removed".into(),
            });
        }
    }
    Ok(out)
}

/// Your own profile: (full name, [(attr kind, value)]) — emails and
/// phones become identity attributes on your Person entity.
pub fn profile(root: &Path) -> Result<(String, Vec<(String, String)>)> {
    let path = root.join("personal_information/profile_information/profile_information.json");
    let v = load_json(&path)?;
    let p = &v["profile_v2"];
    let name = fix(p["name"]["full_name"].as_str().unwrap_or("me"));
    let mut attrs = Vec::new();
    for key in ["emails", "previous_emails"] {
        for e in p["emails"][key].as_array().into_iter().flatten() {
            if let Some(email) = e.as_str() {
                attrs.push(("email".to_string(), fix(email).to_lowercase()));
            }
        }
    }
    for key in ["phone_numbers", "phones"] {
        for ph in p[key].as_array().into_iter().flatten() {
            let number = ph["phone_number"].as_str().or_else(|| ph.as_str());
            if let Some(number) = number {
                attrs.push(("phone".to_string(), fix(number)));
            }
        }
    }
    Ok((name, attrs))
}

/// Everything that materializes. `owner` is the profile full name (so
/// message bundles can label your own lines "me").
pub fn ingest(root: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let activity = root.join("your_facebook_activity");
    if !activity.is_dir() {
        return Err(anyhow!(
            "{} does not look like an extracted Facebook archive \
             (no your_facebook_activity/)",
            root.display()
        ));
    }
    let (owner, profile_attrs) = profile(root).unwrap_or(("me".into(), vec![]));
    let mut drafts = Vec::new();
    drafts.extend(posts(&activity, scope)?);
    drafts.extend(albums(&activity, scope)?);
    drafts.extend(messages(&activity, scope, &owner)?);
    drafts.extend(events(&activity, scope)?);
    drafts.extend(groups(&activity, scope)?);
    // The profile episode: your identities, structurally claimed.
    if !profile_attrs.is_empty() {
        let lines: Vec<String> = profile_attrs
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        drafts.push(EpisodeDraft {
            source: "facebook".into(),
            source_ref: "profile".into(),
            kind: "fb_profile".into(),
            title: format!("Facebook profile — {owner}"),
            body: format!("Identities from the Facebook profile export.\n{}", lines.join("\n")),
            event_time: EventTime {
                utc: None,
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Unknown,
            },
            trust: "imported".into(),
            scope: scope.map(String::from),
            entities: vec![EntityRef {
                entity_type: "Person".into(),
                name: owner.clone(),
                attributes: profile_attrs,
                role: "subject".into(),
            }],
            edges: vec![],
        });
    }
    Ok(drafts)
}

/* ————— posts ————— */

fn posts(activity: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let mut drafts = Vec::new();
    for entry in glob_files(&activity.join("posts"), "your_posts__")? {
        let v = load_json(&entry)?;
        let arr = match &v {
            Value::Array(a) => a.clone(),
            Value::Object(o) => o
                .values()
                .find_map(|x| x.as_array().cloned())
                .unwrap_or_default(),
            _ => vec![],
        };
        for post in &arr {
            let Some(ts) = post["timestamp"].as_i64().filter(|t| *t > 0) else { continue };
            let title = fix(post["title"].as_str().unwrap_or(""));
            let mut text_parts: Vec<String> = Vec::new();
            for d in post["data"].as_array().into_iter().flatten() {
                if let Some(t) = d["post"].as_str() {
                    text_parts.push(fix(t));
                }
            }
            let mut entities = Vec::new();
            let mut place_lines = Vec::new();
            let mut attachment_names = Vec::new();
            for att in post["attachments"].as_array().into_iter().flatten() {
                for d in att["data"].as_array().into_iter().flatten() {
                    if let Some(place) = d.get("place") {
                        let pname = fix(place["name"].as_str().unwrap_or(""));
                        if !pname.is_empty() {
                            entities.push(EntityRef {
                                entity_type: "Place".into(),
                                name: pname.clone(),
                                attributes: vec![],
                                role: "location".into(),
                            });
                        }
                        if let (Some(lat), Some(lon)) = (
                            place["coordinate"]["latitude"].as_f64(),
                            place["coordinate"]["longitude"].as_f64(),
                        ) {
                            // (raw, one-line): the stored line can't break
                            // into a forged `[HH:MM] x (lat,lon)` of its own.
                            place_lines.push((
                                format!("[{}] {pname} ({lat},{lon})", hhmm_of(ts)),
                                format!("[{}] {} ({lat},{lon})", hhmm_of(ts), hearthloom_core::one_line(&pname)),
                            ));
                        }
                    }
                    if let Some(n) = d["name"].as_str() {
                        attachment_names.push(fix(n));
                    }
                }
            }
            for tag in post["tags"].as_array().into_iter().flatten() {
                if let Some(n) = tag["name"].as_str() {
                    if entities.len() < MAX_ENTITIES_PER_EPISODE {
                        entities.push(EntityRef {
                            entity_type: "Person".into(),
                            name: fix(n),
                            attributes: vec![],
                            role: "tagged".into(),
                        });
                    }
                }
            }
            let text = text_parts.join("\n\n");
            // Place and attachment names are other people's text: stored
            // one line each. The REF hashes the body as it always was, so
            // a re-import still matches the posts already in the record.
            let assemble = |clean: bool| -> String {
                let mut body = String::new();
                if !text.is_empty() {
                    body.push_str(&text);
                    body.push('\n');
                }
                if !attachment_names.is_empty() {
                    let names: Vec<String> = attachment_names
                        .iter()
                        .map(|n| if clean { hearthloom_core::one_line(n) } else { n.clone() })
                        .collect();
                    body.push_str(&format!("\nattached: {}\n", names.join(", ")));
                }
                if !place_lines.is_empty() {
                    body.push('\n');
                    for (raw, cleaned) in &place_lines {
                        body.push_str(if clean { cleaned } else { raw });
                        body.push('\n');
                    }
                }
                body.truncate(BODY_CAP);
                body
            };
            let body = assemble(true);
            let ref_body = assemble(false);
            if body.trim().is_empty() && title.is_empty() {
                continue;
            }
            let display_title = if title.is_empty() {
                let first = text.lines().next().unwrap_or("").chars().take(80).collect::<String>();
                if first.is_empty() { "Facebook post".into() } else { first }
            } else {
                title
            };
            drafts.push(EpisodeDraft {
                source: "facebook".into(),
                source_ref: format!("post:{ts}:{}", short_hash(&ref_body)),
                kind: "fb_post".into(),
                title: display_title,
                body,
                event_time: ts_to_event(ts),
                trust: "imported".into(),
                scope: scope.map(String::from),
                entities,
                edges: vec![],
            });
        }
    }
    Ok(drafts)
}

/* ————— albums ————— */

fn albums(activity: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let mut drafts = Vec::new();
    let dir = activity.join("posts/album");
    if !dir.is_dir() {
        return Ok(drafts);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let v = load_json(&path)?;
        let name = fix(v["name"].as_str().unwrap_or("Untitled album"));
        let photos = v["photos"].as_array().cloned().unwrap_or_default();
        let mut times: Vec<i64> = photos
            .iter()
            .filter_map(|p| p["creation_timestamp"].as_i64())
            .filter(|t| *t > 0)
            .collect();
        times.sort_unstable();
        let Some(&first) = times.first() else { continue };
        let span = match (day_of(first), times.last().and_then(|t| day_of(*t))) {
            (Some(a), Some(b)) if a != b => format!("{a} → {b}"),
            (Some(a), _) => a,
            _ => String::new(),
        };
        let description = fix(v["description"].as_str().unwrap_or(""));
        let mut body = format!("album: {name}\nphotos: {}\nspan: {span}\n", photos.len());
        if !description.is_empty() {
            body.push_str(&format!("\n{description}\n"));
        }
        drafts.push(EpisodeDraft {
            source: "facebook".into(),
            source_ref: format!("album:{}:{first}", short_hash(&name)),
            kind: "fb_album".into(),
            title: format!("Album — {name}"),
            body,
            event_time: ts_to_event(first),
            trust: "imported".into(),
            scope: scope.map(String::from),
            entities: vec![],
            edges: vec![],
        });
    }
    Ok(drafts)
}

/* ————— messages ————— */

fn messages(activity: &Path, scope: Option<&str>, owner: &str) -> Result<Vec<EpisodeDraft>> {
    let mut drafts = Vec::new();
    for folder in ["inbox", "archived_threads", "e2ee_cutover", "filtered_threads"] {
        let dir = activity.join("messages").join(folder);
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let thread_dir = entry?.path();
            if !thread_dir.is_dir() {
                continue;
            }
            let slug = thread_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            match thread_episodes(&thread_dir, &slug, scope, owner) {
                Ok(mut eps) => drafts.append(&mut eps),
                Err(err) => tracing::warn!("facebook thread {slug}: {err:#}"),
            }
        }
    }
    Ok(drafts)
}

fn thread_episodes(
    thread_dir: &Path,
    slug: &str,
    scope: Option<&str>,
    owner: &str,
) -> Result<Vec<EpisodeDraft>> {
    // message_1.json .. message_N.json (newest first within each file)
    let mut msg_files: Vec<PathBuf> = std::fs::read_dir(thread_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("message_") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    if msg_files.is_empty() {
        return Ok(vec![]);
    }
    msg_files.sort();

    struct Msg {
        ts_ms: i64,
        sender: String,
        content: String,
    }
    let mut all: Vec<Msg> = Vec::new();
    let mut title = String::new();
    let mut participants: Vec<String> = Vec::new();
    for file in &msg_files {
        let v = load_json(file)?;
        if title.is_empty() {
            title = fix(v["title"].as_str().unwrap_or(""));
        }
        if participants.is_empty() {
            participants = v["participants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p["name"].as_str().map(fix))
                .collect();
        }
        for m in v["messages"].as_array().into_iter().flatten() {
            let Some(ts_ms) = m["timestamp_ms"].as_i64() else { continue };
            let Some(content) = m["content"].as_str() else { continue };
            let content = fix(content);
            if content.trim().is_empty() {
                continue;
            }
            all.push(Msg {
                ts_ms,
                sender: fix(m["sender_name"].as_str().unwrap_or("unknown")),
                content,
            });
        }
    }
    if all.is_empty() {
        return Ok(vec![]);
    }
    all.sort_by_key(|m| m.ts_ms);

    // Who becomes an entity: DM partner always; group members only with
    // ≥5 messages sent in this thread (the casual-connection guard).
    let mut sent: BTreeMap<String, usize> = BTreeMap::new();
    for m in &all {
        *sent.entry(m.sender.clone()).or_default() += 1;
    }
    let others: Vec<String> = participants.iter().filter(|p| *p != owner).cloned().collect();
    let is_dm = others.len() == 1;
    let qualified: Vec<String> = if is_dm {
        others.clone()
    } else {
        sent.iter()
            .filter(|(name, n)| *name != owner && **n >= GROUP_MEMBER_MIN_MESSAGES)
            .map(|(name, _)| name.clone())
            .take(MAX_ENTITIES_PER_EPISODE)
            .collect()
    };
    let chat_label = if !title.is_empty() {
        title.clone()
    } else if is_dm {
        others[0].clone()
    } else {
        slug.to_string()
    };

    // Bundle per UTC day (the iMessage granularity).
    let mut buckets: BTreeMap<String, Vec<&Msg>> = BTreeMap::new();
    for m in &all {
        if let Some(day) = day_of(m.ts_ms / 1000) {
            buckets.entry(day).or_default().push(m);
        }
    }
    let mut drafts = Vec::new();
    for (date, msgs) in buckets {
        let mut body = format!("chat: {chat_label}\ndate: {date}\n\n");
        for m in &msgs {
            let sender = if m.sender == owner { "me" } else { m.sender.as_str() };
            let line = format!("[{}] {sender}: {}\n", hhmm_of(m.ts_ms / 1000), m.content.trim());
            if body.len() + line.len() > BODY_CAP {
                body.push_str("[truncated]\n");
                break;
            }
            body.push_str(&line);
        }
        let entities = qualified
            .iter()
            .map(|name| EntityRef {
                entity_type: "Person".into(),
                name: name.clone(),
                attributes: if is_dm {
                    // the thread slug is a stable per-person handle
                    vec![("facebook".into(), slug.to_string())]
                } else {
                    vec![]
                },
                role: "participant".into(),
            })
            .collect();
        drafts.push(EpisodeDraft {
            source: "facebook".into(),
            source_ref: format!("msg:{slug}#{date}"),
            kind: "fb_message".into(),
            title: format!("Messenger with {chat_label}"),
            body,
            event_time: ts_to_event(msgs[0].ts_ms / 1000),
            trust: "imported".into(),
            scope: scope.map(String::from),
            entities,
            edges: vec![],
        });
    }
    Ok(drafts)
}

/* ————— events ————— */

fn events(activity: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let mut drafts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let push = |name: String, ts: i64, description: String, role: &str,
                    drafts: &mut Vec<EpisodeDraft>,
                    seen: &mut std::collections::HashSet<String>,
                    scope: Option<&str>| {
        let key = format!("{name}:{ts}");
        if ts <= 0 || name.is_empty() || !seen.insert(key.clone()) {
            return;
        }
        let mut body = format!("event: {name}\nrole: {role}\n");
        if !description.is_empty() {
            let mut d = description;
            d.truncate(2048);
            body.push_str(&format!("\n{d}\n"));
        }
        drafts.push(EpisodeDraft {
            source: "facebook".into(),
            source_ref: format!("event:{ts}:{}", short_hash(&name)),
            kind: "fb_event".into(),
            title: format!("Event — {name}"),
            body,
            event_time: ts_to_event(ts),
            trust: "imported".into(),
            scope: scope.map(String::from),
            entities: vec![],
            edges: vec![],
        });
    };

    let joined_path = activity.join("events/your_event_responses.json");
    if joined_path.is_file() {
        let v = load_json(&joined_path)?;
        for e in v["event_responses_v2"]["events_joined"].as_array().into_iter().flatten() {
            push(
                fix(e["name"].as_str().unwrap_or("")),
                e["start_timestamp"].as_i64().unwrap_or(0),
                fix(e["description"].as_str().unwrap_or("")),
                "went",
                &mut drafts,
                &mut seen,
                scope,
            );
        }
    }
    let yours_path = activity.join("events/your_events.json");
    if yours_path.is_file() {
        let v = load_json(&yours_path)?;
        for e in v["your_events_v2"].as_array().into_iter().flatten() {
            push(
                fix(e["name"].as_str().unwrap_or("")),
                e["start_timestamp"].as_i64().unwrap_or(0),
                fix(e["description"].as_str().unwrap_or("")),
                "joined",
                &mut drafts,
                &mut seen,
                scope,
            );
        }
    }
    let hosted_path = activity.join("events/events_you_hosted.json");
    if hosted_path.is_file() {
        let v = load_json(&hosted_path)?;
        let arr = match &v {
            Value::Array(a) => a.clone(),
            Value::Object(o) => o.values().find_map(|x| x.as_array().cloned()).unwrap_or_default(),
            _ => vec![],
        };
        for e in &arr {
            push(
                fix(e["title"].as_str().unwrap_or("")),
                e["timestamp"].as_i64().unwrap_or(0),
                String::new(),
                "hosted",
                &mut drafts,
                &mut seen,
                scope,
            );
        }
    }
    Ok(drafts)
}

/* ————— groups ————— */

fn groups(activity: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    struct Item {
        ts: i64,
        group: String,
        text: String,
    }
    let mut items: Vec<Item> = Vec::new();
    for file in [
        "groups/your_comments_in_groups.json",
        "groups/group_posts_and_comments.json",
    ] {
        let path = activity.join(file);
        if !path.is_file() {
            continue;
        }
        let v = load_json(&path)?;
        let arr = match &v {
            Value::Array(a) => a.clone(),
            Value::Object(o) => o.values().find_map(|x| x.as_array().cloned()).unwrap_or_default(),
            _ => vec![],
        };
        for it in &arr {
            let Some(ts) = it["timestamp"].as_i64().filter(|t| *t > 0) else { continue };
            let mut group = String::new();
            let mut text = String::new();
            for d in it["data"].as_array().into_iter().flatten() {
                if let Some(c) = d.get("comment") {
                    group = fix(c["group"].as_str().unwrap_or(""));
                    text = fix(c["comment"].as_str().unwrap_or(""));
                } else if let Some(p) = d["post"].as_str() {
                    text = fix(p);
                }
            }
            if group.is_empty() {
                // group posts carry the group in the title ("… posted in X")
                let title = fix(it["title"].as_str().unwrap_or(""));
                if let Some(idx) = title.find(" in ") {
                    group = title[idx + 4..].trim_end_matches('.').to_string();
                }
            }
            if text.trim().is_empty() {
                continue;
            }
            items.push(Item { ts, group, text });
        }
    }
    // bundle per (group, day)
    let mut buckets: BTreeMap<(String, String), Vec<Item>> = BTreeMap::new();
    for item in items {
        let Some(day) = day_of(item.ts) else { continue };
        let group = if item.group.is_empty() { "unknown group".into() } else { item.group.clone() };
        buckets.entry((group, day)).or_default().push(item);
    }
    let mut drafts = Vec::new();
    for ((group, date), items) in buckets {
        let mut body = format!("group: {group}\ndate: {date}\n\n");
        for item in &items {
            let line = format!("[{}] {}\n", hhmm_of(item.ts), item.text.trim());
            if body.len() + line.len() > BODY_CAP {
                body.push_str("[truncated]\n");
                break;
            }
            body.push_str(&line);
        }
        let entities = if group != "unknown group" {
            vec![EntityRef {
                entity_type: "Group".into(),
                name: group.clone(),
                attributes: vec![],
                role: "context".into(),
            }]
        } else {
            vec![]
        };
        drafts.push(EpisodeDraft {
            source: "facebook".into(),
            source_ref: format!("group:{}#{date}", short_hash(&group)),
            kind: "fb_group".into(),
            title: format!("In {group}"),
            body,
            event_time: ts_to_event(items[0].ts),
            trust: "imported".into(),
            scope: scope.map(String::from),
            entities,
            edges: vec![],
        });
    }
    Ok(drafts)
}

fn glob_files(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with(prefix) && name.ends_with(".json") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mojibake_repair() {
        // "ð¥°" as chars → UTF-8 bytes for 🥰
        let masked: String = [0xf0u8, 0x9f, 0xa5, 0xb0].iter().map(|b| *b as char).collect();
        assert_eq!(fix(&masked), "🥰");
        // em dash
        let masked: String = [0xe2u8, 0x80, 0x94].iter().map(|b| *b as char).collect();
        assert_eq!(fix(&masked), "—");
        // already-clean text is untouched
        assert_eq!(fix("plain ascii"), "plain ascii");
        assert_eq!(fix("real 🥰 emoji"), "real 🥰 emoji");
    }

    #[test]
    fn test_end_to_end_fixture_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let act = root.join("your_facebook_activity");
        std::fs::create_dir_all(act.join("posts")).unwrap();
        std::fs::create_dir_all(act.join("messages/inbox/ada_123")).unwrap();
        std::fs::create_dir_all(act.join("events")).unwrap();
        std::fs::create_dir_all(root.join("connections/friends")).unwrap();
        std::fs::create_dir_all(root.join("personal_information/profile_information")).unwrap();

        std::fs::write(
            root.join("personal_information/profile_information/profile_information.json"),
            r#"{"profile_v2":{"name":{"full_name":"Jon Hale"},
                "emails":{"emails":["jon@example.com"],"previous_emails":[]}}}"#,
        )
        .unwrap();
        std::fs::write(
            act.join("posts/your_posts__check_ins__photos_and_videos_1.json"),
            r#"[{"timestamp": 1262347200,
                "data": [{"post": "Test post"}],
                "title": "Jon Hale shared a game.",
                "tags": [{"name": "Ada Lovelace"}],
                "attachments": [{"data": [{"place": {"name": "Lakeside Park",
                  "coordinate": {"latitude": 12.34, "longitude": -56.78}}}]}]}]"#,
        )
        .unwrap();
        std::fs::write(
            act.join("messages/inbox/ada_123/message_1.json"),
            r#"{"participants": [{"name": "Ada Lovelace"}, {"name": "Jon Hale"}],
                "title": "Ada Lovelace",
                "messages": [
                  {"sender_name": "Ada Lovelace", "timestamp_ms": 1600000000000, "content": "hi"},
                  {"sender_name": "Jon Hale", "timestamp_ms": 1600000060000, "content": "hello"}]}"#,
        )
        .unwrap();
        std::fs::write(
            act.join("events/your_event_responses.json"),
            r#"{"event_responses_v2":{"events_joined":[
                {"name":"GDC Party","start_timestamp":1500000000,"description":"games"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("connections/friends/your_friends.json"),
            r#"{"friends_v2":[{"name":"Ada Lovelace","timestamp":1400000000}]}"#,
        )
        .unwrap();

        let drafts = ingest(root, Some("personal")).unwrap();
        let kinds: Vec<&str> = drafts.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"fb_post"));
        assert!(kinds.contains(&"fb_message"));
        assert!(kinds.contains(&"fb_event"));
        assert!(kinds.contains(&"fb_profile"));

        let post = drafts.iter().find(|d| d.kind == "fb_post").unwrap();
        // the exact line the location index reads (core store/locations.rs
        // parse_location_lines tests the same shape): the pair closes it
        assert!(post.body.contains("\n[12:00] Lakeside Park (12.34,-56.78)\n"), "coordinate line for the location index");
        assert!(post.entities.iter().any(|e| e.entity_type == "Place" && e.name == "Lakeside Park"));
        assert!(post.entities.iter().any(|e| e.name == "Ada Lovelace" && e.role == "tagged"));

        let msg = drafts.iter().find(|d| d.kind == "fb_message").unwrap();
        assert!(msg.body.contains("me: hello"), "owner lines say 'me'");
        assert_eq!(msg.entities.len(), 1, "DM partner becomes the one entity");
        assert_eq!(msg.entities[0].attributes[0], ("facebook".into(), "ada_123".into()));

        let connections = friends(root).unwrap();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].status, "active");
        assert_eq!(connections[0].connected_at.as_deref(), Some("2014-05-13"));
    }
}

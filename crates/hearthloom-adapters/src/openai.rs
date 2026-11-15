//! OpenAI ChatGPT data-export adapter — the MULTI-CHUNK export
//! (`conversations-000.json` … `conversations-NNN.json`), distinct from the
//! single-file `chatlog` adapter. One conversation = one episode, source
//! `openai`. The export is large (tens of thousands of turns, 11M+ tokens):
//! chunks are read one at a time and their raw JSON dropped before the next,
//! so peak memory stays bounded. Unlike `chatlog`'s `[media: …]`
//! placeholders, image assets are PRESERVED (Notion-style) — the export
//! ships them as `file-<id>.dat` blobs plus a `conversation_asset_file_names
//! .json` map back to their original names; referenced images are copied
//! under a per-conversation media dir and named in the transcript.
//!
//! Metadata carried through: `is_starred` → a `starred` preamble line (the
//! draft has no importance field — this keeps it FTS-discoverable);
//! `is_do_not_remember` → scope `private` (the vault hides it, and it never
//! reaches a cloud LLM), overriding any passed scope.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use serde_json::Value;

/// Per-message and per-episode text caps: the condensed transcript is the
/// searchable body; the export directory itself remains the deep archive.
const MESSAGE_CAP: usize = 2000;
const BODY_CAP: usize = 64 * 1024;

/// Ingest an extracted ChatGPT export directory.
///
/// `media_out`, when set, is the base directory under which referenced image
/// assets are copied (`<media_out>/<conversation_id>/<original-name>`); `None`
/// leaves an `[image: …]` placeholder without copying (used by the pure unit
/// tests and any caller without a home to write into).
pub fn ingest(
    dir: &Path,
    media_out: Option<&Path>,
    scope: Option<&str>,
) -> Result<Vec<EpisodeDraft>> {
    let read = read_export(dir, media_out, scope)?;
    if read.drafts.is_empty() {
        return Err(anyhow!("no conversations found in {}", dir.display()));
    }
    Ok(read.drafts)
}

/// What a ChatGPT export read gives: the conversations to store, and every
/// conversation the owner flagged "do not remember" — kept apart from the
/// drafts, because a flagged conversation with no turns on the branch it
/// ended on gets no draft, while a copy an earlier reader stored of it (from
/// another branch) must still go into the vault
/// (`Store::label_private_on_reimport`).
pub struct ExportRead {
    pub drafts: Vec<EpisodeDraft>,
    /// (source, conversation id) of each flagged conversation.
    pub flagged: Vec<(String, String)>,
}

/// `ingest`, with the flagged conversations. An error only when the export
/// holds neither a conversation to store nor a flagged one.
pub fn read_export(
    dir: &Path,
    media_out: Option<&Path>,
    scope: Option<&str>,
) -> Result<ExportRead> {
    if !dir.is_dir() {
        return Err(anyhow!("{} is not a directory", dir.display()));
    }

    // The export's user.json names the account owner by email (no full name,
    // unlike Claude's users.json) — attaching them as author lets scope
    // inference key off the identity's scope.
    let owner = std::fs::read_to_string(dir.join("user.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|u| Some(u.get("email")?.as_str()?.to_lowercase()));

    // file-<id>.dat → original filename (e.g. IMG_1234.jpeg).
    let asset_names: HashMap<String, String> =
        std::fs::read_to_string(dir.join("conversation_asset_file_names.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();

    // The multi-chunk export; fall back to a lone conversations.json so a
    // single-file export drag still works through this adapter.
    let mut chunks: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("conversations-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    chunks.sort();
    if chunks.is_empty() && dir.join("conversations.json").is_file() {
        chunks.push(dir.join("conversations.json"));
    }
    if chunks.is_empty() {
        return Err(anyhow!(
            "no conversations-*.json chunks in {}",
            dir.display()
        ));
    }

    let mut drafts = Vec::new();
    let mut flagged = Vec::new();
    for chunk in &chunks {
        let raw = std::fs::read_to_string(chunk)
            .with_context(|| format!("reading {}", chunk.display()))?;
        let convs: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", chunk.display()))?;
        let Some(list) = convs.as_array() else {
            tracing::warn!("skipping {}: not a JSON array", chunk.display());
            continue;
        };
        for conversation in list {
            let mut preserve = |pointer: &str, conversation_id: &str| {
                preserve_asset(pointer, conversation_id, dir, media_out, &asset_names)
            };
            let read = match read_chatgpt(conversation, Some(&mut preserve)) {
                Ok(read) => read,
                Err(err) => {
                    tracing::warn!("skipping conversation: {err}");
                    continue;
                }
            };
            // the flag counts whether or not a transcript is left
            if read.do_not_remember {
                flagged.push(("openai".to_string(), read.id.clone()));
            }
            if let Some(draft) = build_conversation(read, scope, owner.as_deref()) {
                drafts.push(draft);
            }
        }
        // `raw`/`convs` drop here before the next chunk loads.
    }

    if drafts.is_empty() && flagged.is_empty() {
        return Err(anyhow!("no conversations found in {}", dir.display()));
    }
    Ok(ExportRead { drafts, flagged })
}

fn cap(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// `file-service://file-XYZ` (or a bare `file-XYZ`) → the on-disk id
/// `file-XYZ`, whose blob is `file-XYZ.dat` in the export root.
fn asset_id(pointer: &str) -> Option<&str> {
    let last = pointer.rsplit('/').next()?.trim();
    if last.starts_with("file-") && !last.is_empty() {
        Some(last)
    } else {
        None
    }
}

/// A conversation id usable as one folder name: letters, digits, `-` and
/// `_` (ChatGPT's ids are UUIDs), at most 128 of them.
fn folder_name_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A name that is exactly one plain path component: not empty, not `.` or
/// `..`, no separator (`/` or `\`) and no NUL.
fn plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains(['/', '\\', '\0'])
        && Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
}

/// Copy one referenced image asset into the conversation's media dir and
/// return the reference name to embed. Returns None if the blob is missing
/// (DALL·E-only pointers, partial exports) — the caller still notes it.
/// The export names both the folder (its conversation id) and the file
/// (its asset map), so both are checked to be single plain names inside
/// `media_out`; an original name that isn't one falls back to the blob's
/// own `file-<id>.dat`. Only a regular file is copied, only into a real
/// folder (not a link to one elsewhere), and nothing already at the target
/// name (a file or a link) is written through.
fn preserve_asset(
    pointer: &str,
    conversation_id: &str,
    dir: &Path,
    media_out: Option<&Path>,
    asset_names: &HashMap<String, String>,
) -> Option<String> {
    let id = asset_id(pointer)?;
    let dat = format!("{id}.dat");
    if !plain_file_name(&dat) {
        return None;
    }
    let original = asset_names
        .get(&dat)
        .filter(|name| plain_file_name(name))
        .cloned()
        .unwrap_or_else(|| dat.clone());
    let Some(base) = media_out else {
        return Some(original); // no home to copy into — just name it
    };
    if !folder_name_ok(conversation_id) {
        return None;
    }
    let src = dir.join(&dat);
    match std::fs::symlink_metadata(&src) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return None,
    }
    let target_dir = base.join(conversation_id);
    if !real_dir(&target_dir) {
        return None;
    }
    let target = target_dir.join(&original);
    // `create_new` never opens what is already at the name — a file from
    // an earlier import (idempotent: nothing to copy) or a link.
    let mut out = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
    {
        Ok(out) => out,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Some(original),
        Err(_) => return None,
    };
    let copied = std::fs::File::open(&src).and_then(|mut from| std::io::copy(&mut from, &mut out));
    if copied.is_err() {
        drop(out);
        let _ = std::fs::remove_file(&target); // not a half copy for the next import to keep
        return None;
    }
    Some(original)
}

/// `dir` is a real directory, made here if missing: a link at its name
/// (to a folder anywhere) is refused, never followed.
fn real_dir(dir: &Path) -> bool {
    if let Some(parent) = dir.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return false,
    }
    std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.file_type().is_dir())
}

/// One conversation read from a ChatGPT export: the transcript on the
/// branch the conversation ended on, and the flags the owner set on it.
/// The one reader of ChatGPT's `mapping` format — this adapter and the
/// single-file `chatlog` adapter both use it, so a conversation's flags
/// mean the same on either route.
pub(crate) struct ChatgptConversation {
    pub id: String,
    pub title: String,
    pub event_time: EventTime,
    /// (speaker, text) in conversation order; speakers `you` / `chatgpt`.
    pub turns: Vec<(String, String)>,
    pub starred: bool,
    /// "Do not remember": the owner told ChatGPT to forget it. Stored
    /// private (vault-locked, never sent to a cloud LLM), over any scope
    /// the caller passed.
    pub do_not_remember: bool,
}

/// The mapping's nodes on the branch the conversation ended on: from
/// `current_node` up through each node's `parent`, then root first. The
/// tree also keeps every regenerated answer and edited prompt the owner
/// moved away from; those aren't part of what was said. An export without
/// a usable `current_node` falls back to every node in message-time order.
fn selected_branch(conversation: &Value) -> Vec<&Value> {
    let Some(mapping) = conversation.get("mapping").and_then(|m| m.as_object()) else {
        return vec![];
    };
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut at = conversation.get("current_node").and_then(|c| c.as_str());
    while let Some(id) = at {
        if !seen.insert(id) {
            break; // a cycle
        }
        let Some(node) = mapping.get(id) else {
            break;
        };
        chain.push(node);
        at = node.get("parent").and_then(|p| p.as_str());
    }
    if !chain.is_empty() {
        chain.reverse();
        return chain;
    }
    let time = |node: &Value| {
        node.pointer("/message/create_time")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0)
    };
    let mut nodes: Vec<&Value> = mapping.values().collect();
    nodes.sort_by(|a, b| {
        time(a)
            .partial_cmp(&time(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    nodes
}

/// Names an image part's asset, given its pointer and the conversation id
/// (None: the asset isn't available).
pub(crate) type ImageNamer<'a> = &'a mut dyn FnMut(&str, &str) -> Option<String>;

/// Read one ChatGPT conversation. `image` names an image part's asset;
/// without one, image parts are left out. Returns the conversation even
/// when it has no turns.
pub(crate) fn read_chatgpt(
    conversation: &Value,
    mut image: Option<ImageNamer<'_>>,
) -> Result<ChatgptConversation> {
    let id = conversation
        .get("conversation_id")
        .or_else(|| conversation.get("id"))
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow!("chatgpt conversation without id"))?
        .to_string();
    let title = conversation
        .get("title")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .unwrap_or("(untitled conversation)")
        .to_string();
    let event_time = conversation
        .get("create_time")
        .and_then(|c| c.as_f64())
        .and_then(|epoch| DateTime::from_timestamp(epoch as i64, 0))
        .map(|dt| EventTime {
            utc: Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            tz_offset_minutes: None, // export carries no timezone
            uncertainty: TimeUncertainty::Exact,
        })
        .unwrap_or_else(EventTime::unknown);
    let flag = |name: &str| {
        conversation
            .get(name)
            .and_then(|s| s.as_bool())
            .unwrap_or(false)
    };

    let mut turns = Vec::new();
    for node in selected_branch(conversation) {
        let Some(message) = node.get("message") else {
            continue;
        };
        let role = message
            .pointer("/author/role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        if role != "user" && role != "assistant" {
            continue; // system/tool turns aren't conversation
        }
        let speaker = if role == "user" { "you" } else { "chatgpt" };
        let mut text = String::new();
        for part in message
            .pointer("/content/parts")
            .and_then(|p| p.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if let Some(t) = part.as_str() {
                text.push_str(t);
                text.push('\n');
            } else if part.get("content_type").and_then(|c| c.as_str())
                == Some("image_asset_pointer")
            {
                let Some(image) = image.as_mut() else {
                    continue;
                };
                let pointer = part
                    .get("asset_pointer")
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                match image(pointer, &id) {
                    Some(name) => text.push_str(&format!("[image: {name}]\n")),
                    None => text.push_str("[image: unavailable]\n"),
                }
            }
        }
        if !text.trim().is_empty() {
            turns.push((speaker.to_string(), text));
        }
    }
    Ok(ChatgptConversation {
        id,
        title,
        event_time,
        turns,
        starred: flag("is_starred"),
        do_not_remember: flag("is_do_not_remember"),
    })
}

/// The draft of a conversation read from the export; None when it has no
/// turns.
fn build_conversation(
    read: ChatgptConversation,
    scope: Option<&str>,
    owner: Option<&str>,
) -> Option<EpisodeDraft> {
    let ChatgptConversation {
        id,
        title,
        event_time,
        turns,
        starred,
        do_not_remember: private,
    } = read;
    if turns.is_empty() {
        return None;
    }

    let entities = owner
        .map(|email| {
            vec![EntityRef {
                entity_type: "Person".into(),
                name: "me".into(),
                attributes: vec![("email".into(), email.to_string())],
                role: "author".into(),
            }]
        })
        .unwrap_or_default();

    let mut body = format!("conversation: {title}\nprovider: chatgpt\n");
    if starred {
        body.push_str("starred: true\n");
    }
    body.push('\n');
    for (speaker, text) in &turns {
        let entry = format!("{speaker}: {}\n\n", cap(text.trim(), MESSAGE_CAP));
        if body.len() + entry.len() > BODY_CAP {
            body.push_str("[transcript truncated]\n");
            break;
        }
        body.push_str(&entry);
    }

    let scope = if private {
        Some("private".to_string())
    } else {
        scope.map(String::from)
    };

    Some(EpisodeDraft {
        source: "openai".into(),
        source_ref: id,
        kind: "conversation".into(),
        title,
        body,
        event_time,
        trust: "self".into(),
        scope,
        entities,
        edges: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn test_multichunk_export_with_media_and_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(
            dir,
            "user.json",
            r#"{"email":"Jon@Example.com","id":"user-1"}"#,
        );
        write(
            dir,
            "conversation_asset_file_names.json",
            r#"{"file-abc.dat":"cat.jpeg"}"#,
        );
        std::fs::write(dir.join("file-abc.dat"), b"\xff\xd8\xff\x00pretend-jpeg").unwrap();
        // Two chunks; second is starred + do-not-remember (→ private).
        write(
            dir,
            "conversations-000.json",
            r#"[{
              "conversation_id":"c1","title":"Cats","create_time":1720992777.0,
              "mapping":{
                "n2":{"message":{"author":{"role":"assistant"},"create_time":1720992800.0,
                      "content":{"content_type":"text","parts":["Nice cat."]}}},
                "n1":{"message":{"author":{"role":"user"},"create_time":1720992778.0,
                      "content":{"content_type":"multimodal_text","parts":[
                        "Look at this",
                        {"content_type":"image_asset_pointer","asset_pointer":"file-service://file-abc"}
                      ]}}},
                "n0":{"message":{"author":{"role":"system"},"create_time":0.0,
                      "content":{"content_type":"text","parts":["you are helpful"]}}}
              }
            }]"#,
        );
        write(
            dir,
            "conversations-001.json",
            r#"[{
              "conversation_id":"c2","title":"Secret","create_time":1720999999.0,
              "is_starred":true,"is_do_not_remember":true,
              "mapping":{"m":{"message":{"author":{"role":"user"},"create_time":1721000000.0,
                    "content":{"content_type":"text","parts":["hush"]}}}}
            }]"#,
        );

        let media = tmp.path().join("media_out");
        let drafts = ingest(dir, Some(&media), None).unwrap();
        assert_eq!(drafts.len(), 2);

        let c1 = drafts.iter().find(|d| d.source_ref == "c1").unwrap();
        assert_eq!(c1.source, "openai");
        // System turn dropped; user before assistant by create_time.
        let u = c1.body.find("you:").unwrap();
        let a = c1.body.find("chatgpt:").unwrap();
        assert!(u < a, "user turn should precede assistant");
        assert!(
            c1.body.contains("[image: cat.jpeg]"),
            "media named by original"
        );
        assert!(!c1.body.contains("system"), "system turn excluded");
        // Owner attached as author with lowercased email.
        assert_eq!(c1.entities[0].attributes[0].1, "jon@example.com");
        // The image blob was copied under <media>/<conversation>/<name>.
        assert!(media.join("c1").join("cat.jpeg").is_file());

        let c2 = drafts.iter().find(|d| d.source_ref == "c2").unwrap();
        assert_eq!(
            c2.scope.as_deref(),
            Some("private"),
            "do-not-remember → private"
        );
        assert!(c2.body.contains("starred: true"));
    }

    /// The export names the media folder (a conversation id) and each
    /// file (its asset map); neither can place a file outside `media_out`.
    /// A name that isn't one plain file name falls back to the blob's own.
    #[test]
    fn test_export_names_stay_inside_the_media_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("export");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let media = tmp.path().join("media");
        let absolute = outside.join("b.plist");
        write(
            &dir,
            "conversation_asset_file_names.json",
            &serde_json::json!({
                "file-a.dat": "../../outside/a.plist",
                "file-b.dat": absolute.to_string_lossy(),
                "file-c.dat": "c.jpeg",
                "file-d.dat": "..",
            })
            .to_string(),
        );
        for blob in ["file-a.dat", "file-b.dat", "file-c.dat", "file-d.dat"] {
            std::fs::write(dir.join(blob), b"bytes").unwrap();
        }
        let image = |id: &str| {
            serde_json::json!({"content_type": "image_asset_pointer",
                               "asset_pointer": format!("file-service://{id}")})
        };
        let conversation = |id: &str, images: &[&str]| {
            let mut parts = vec![serde_json::json!("see")];
            parts.extend(images.iter().map(|i| image(i)));
            serde_json::json!({
                "conversation_id": id, "title": id, "create_time": 1720992777.0,
                "mapping": {"n1": {"message": {"author": {"role": "user"},
                    "create_time": 1720992778.0,
                    "content": {"content_type": "multimodal_text", "parts": parts}}}}
            })
        };
        write(
            &dir,
            "conversations-000.json",
            &serde_json::json!([
                conversation("c-ok", &["file-a", "file-b", "file-c", "file-d"]),
                conversation("../outside", &["file-c"]),
                conversation(&outside.join("conv").to_string_lossy(), &["file-c"]),
            ])
            .to_string(),
        );

        let drafts = ingest(&dir, Some(&media), None).unwrap();
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing lands outside the media dir"
        );
        assert!(!absolute.exists());
        let ok = drafts.iter().find(|d| d.source_ref == "c-ok").unwrap();
        for name in ["file-a.dat", "file-b.dat", "c.jpeg", "file-d.dat"] {
            assert!(ok.body.contains(&format!("[image: {name}]")), "{}", ok.body);
            assert!(media.join("c-ok").join(name).is_file(), "{name}");
        }
        let odd = drafts
            .iter()
            .find(|d| d.source_ref == "../outside")
            .unwrap();
        assert!(odd.body.contains("[image: unavailable]"), "{}", odd.body);
        let entries: Vec<_> = std::fs::read_dir(&media)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("c-ok")]);
    }

    /// Only a regular file in the export is copied, never a link out of it.
    #[cfg(unix)]
    #[test]
    fn test_a_linked_blob_is_not_copied() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("export");
        std::fs::create_dir_all(&dir).unwrap();
        let elsewhere = tmp.path().join("elsewhere.txt");
        std::fs::write(&elsewhere, b"not part of the export").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("file-x.dat")).unwrap();
        write(
            &dir,
            "conversations.json",
            r#"[{"conversation_id":"c1","title":"t","create_time":1720992777.0,
                "mapping":{"n1":{"message":{"author":{"role":"user"},"create_time":1720992778.0,
                  "content":{"content_type":"multimodal_text","parts":[
                    {"content_type":"image_asset_pointer","asset_pointer":"file-service://file-x"}
                  ]}}}}}]"#,
        );
        let media = tmp.path().join("media");
        let drafts = ingest(&dir, Some(&media), None).unwrap();
        assert!(drafts[0].body.contains("[image: unavailable]"));
        assert!(!media.join("c1").join("file-x.dat").exists());
    }

    /// A conversation's media folder that is a link (to a folder anywhere)
    /// is never written through, and neither is a link at an asset's name.
    #[cfg(unix)]
    #[test]
    fn test_media_lands_only_in_a_real_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("export");
        let outside = tmp.path().join("outside");
        let media = tmp.path().join("media");
        for d in [&dir, &outside, &media] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::os::unix::fs::symlink(&outside, media.join("c1")).unwrap();
        std::fs::create_dir_all(media.join("c2")).unwrap();
        let kept = outside.join("kept.jpeg");
        std::fs::write(&kept, b"left alone").unwrap();
        std::os::unix::fs::symlink(&kept, media.join("c2").join("b.jpeg")).unwrap();
        write(
            &dir,
            "conversation_asset_file_names.json",
            r#"{"file-a.dat": "a.jpeg", "file-b.dat": "b.jpeg", "file-c.dat": "c.jpeg"}"#,
        );
        for blob in ["file-a.dat", "file-b.dat", "file-c.dat"] {
            std::fs::write(dir.join(blob), b"bytes").unwrap();
        }
        let conversation = |id: &str, images: &[&str]| {
            let parts: Vec<_> = images
                .iter()
                .map(|i| {
                    serde_json::json!({"content_type": "image_asset_pointer",
                                       "asset_pointer": format!("file-service://{i}")})
                })
                .collect();
            serde_json::json!({
                "conversation_id": id, "title": id, "create_time": 1720992777.0,
                "mapping": {"n1": {"message": {"author": {"role": "user"},
                    "create_time": 1720992778.0,
                    "content": {"content_type": "multimodal_text", "parts": parts}}}}
            })
        };
        write(
            &dir,
            "conversations.json",
            &serde_json::json!([
                conversation("c1", &["file-a"]),
                conversation("c2", &["file-b", "file-c"]),
            ])
            .to_string(),
        );

        let drafts = ingest(&dir, Some(&media), None).unwrap();
        let body = |id: &str| &drafts.iter().find(|d| d.source_ref == id).unwrap().body;
        assert!(
            body("c1").contains("[image: unavailable]"),
            "{}",
            body("c1")
        );
        let mut outside_now: Vec<_> = std::fs::read_dir(&outside)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        outside_now.sort();
        assert_eq!(
            outside_now,
            ["kept.jpeg"],
            "nothing new outside the media dir"
        );
        assert_eq!(std::fs::read(&kept).unwrap(), b"left alone");
        assert!(media.join("c2").join("c.jpeg").is_file());
        assert!(body("c2").contains("[image: c.jpeg]"));
    }

    #[test]
    fn test_empty_dir_errors() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ingest(tmp.path(), None, None).is_err());
    }

    /// A conversation flagged "do not remember" is reported as flagged
    /// whether or not its kept branch leaves a transcript to draft; an
    /// export holding only such a conversation reads without error.
    #[test]
    fn test_flagged_conversations_are_reported_without_a_draft() {
        let flagged_empty = r#"{
          "conversation_id":"c9","title":"Clinic visit","create_time":1720992777.0,
          "is_do_not_remember":true,"current_node":"sys",
          "mapping":{
            "sys":{"parent":null,"message":{"author":{"role":"system"},
                   "content":{"parts":["You are a helpful assistant."]}}},
            "u1":{"parent":"sys","message":{"author":{"role":"user"},
                  "content":{"parts":["about the results"]}}}
          }
        }"#;
        let ordinary = r#"{
          "conversation_id":"c10","title":"Garden","create_time":1720992800.0,
          "mapping":{"u1":{"message":{"author":{"role":"user"},
                     "content":{"parts":["what to plant"]}}}}
        }"#;
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "conversations-000.json",
            &format!("[{flagged_empty},{ordinary}]"),
        );
        let read = read_export(tmp.path(), None, None).unwrap();
        let ids: Vec<&str> = read.drafts.iter().map(|d| d.source_ref.as_str()).collect();
        assert_eq!(ids, ["c10"]);
        assert_eq!(read.flagged, [("openai".to_string(), "c9".to_string())]);

        let alone = tempfile::tempdir().unwrap();
        write(
            alone.path(),
            "conversations-000.json",
            &format!("[{flagged_empty}]"),
        );
        let read = read_export(alone.path(), None, None).unwrap();
        assert!(read.drafts.is_empty());
        assert_eq!(read.flagged.len(), 1);
        assert!(
            ingest(alone.path(), None, None).is_err(),
            "nothing to store"
        );
    }
}

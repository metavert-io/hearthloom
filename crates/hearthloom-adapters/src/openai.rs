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
            match build_conversation(
                conversation,
                dir,
                media_out,
                &asset_names,
                scope,
                owner.as_deref(),
            ) {
                Ok(Some(draft)) => drafts.push(draft),
                Ok(None) => {} // empty conversation
                Err(err) => tracing::warn!("skipping conversation: {err}"),
            }
        }
        // `raw`/`convs` drop here before the next chunk loads.
    }

    if drafts.is_empty() {
        return Err(anyhow!("no conversations found in {}", dir.display()));
    }
    Ok(drafts)
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

/// Copy one referenced image asset into the conversation's media dir and
/// return the reference name to embed. Returns None if the blob is missing
/// (DALL·E-only pointers, partial exports) — the caller still notes it.
fn preserve_asset(
    pointer: &str,
    conversation_id: &str,
    dir: &Path,
    media_out: Option<&Path>,
    asset_names: &HashMap<String, String>,
) -> Option<String> {
    let id = asset_id(pointer)?;
    let dat = format!("{id}.dat");
    let original = asset_names
        .get(&dat)
        .cloned()
        .unwrap_or_else(|| dat.clone());
    let Some(base) = media_out else {
        return Some(original); // no home to copy into — just name it
    };
    let src = dir.join(&dat);
    if !src.is_file() {
        return None;
    }
    let target_dir = base.join(conversation_id);
    if std::fs::create_dir_all(&target_dir).is_err() {
        return None;
    }
    let target = target_dir.join(&original);
    // Idempotent: skip the copy if it's already there (re-imports).
    if !target.exists() && std::fs::copy(&src, &target).is_err() {
        return None;
    }
    Some(original)
}

fn build_conversation(
    conversation: &Value,
    dir: &Path,
    media_out: Option<&Path>,
    asset_names: &HashMap<String, String>,
    scope: Option<&str>,
    owner: Option<&str>,
) -> Result<Option<EpisodeDraft>> {
    let id = conversation
        .get("conversation_id")
        .or_else(|| conversation.get("id"))
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow!("conversation without id"))?;
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

    let starred = conversation
        .get("is_starred")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    // "Do not remember" is the user telling ChatGPT to forget it — honor that
    // as private (vault-locked, never sent to a cloud LLM), over any passed scope.
    let private = conversation
        .get("is_do_not_remember")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // The mapping is a tree; each message's create_time orders the turns.
    let mut messages: Vec<(f64, String, String)> = Vec::new();
    for node in conversation
        .get("mapping")
        .and_then(|m| m.as_object())
        .map(|m| m.values().collect::<Vec<_>>())
        .unwrap_or_default()
    {
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
                let pointer = part
                    .get("asset_pointer")
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                match preserve_asset(pointer, id, dir, media_out, asset_names) {
                    Some(name) => {
                        text.push_str(&format!("[image: {name}]\n"));
                    }
                    None => text.push_str("[image: unavailable]\n"),
                }
            }
        }
        if text.trim().is_empty() {
            continue;
        }
        let at = message
            .get("create_time")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0);
        messages.push((at, speaker.to_string(), text));
    }
    if messages.is_empty() {
        return Ok(None);
    }
    messages.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

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
    for (_, speaker, text) in &messages {
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

    Ok(Some(EpisodeDraft {
        source: "openai".into(),
        source_ref: id.to_string(),
        kind: "conversation".into(),
        title,
        body,
        event_time,
        trust: "self".into(),
        scope,
        entities,
        edges: vec![],
    }))
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

    #[test]
    fn test_empty_dir_errors() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ingest(tmp.path(), None, None).is_err());
    }
}

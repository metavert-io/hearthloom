//! AI conversation-export adapter: Claude.ai and ChatGPT `conversations.json`
//! exports — your actual thinking log. One conversation = one episode.
//!
//! Format is auto-detected: Claude exports carry `chat_messages`, ChatGPT
//! exports carry a `mapping` tree keyed by node id.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EpisodeDraft, EventTime, TimeUncertainty};
use serde_json::Value;

/// Per-message and per-episode text caps: the condensed transcript is the
/// searchable body; the export file itself remains the deep archive.
const MESSAGE_CAP: usize = 2000;
const BODY_CAP: usize = 64 * 1024;

pub fn ingest(path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    // Claude exports ship a sibling users.json naming the account owner;
    // attaching them as author makes identity-based scope inference work
    // (a work-email account's conversations default to `work`).
    ingest_with_owner(path, scope, owner_of(path).as_ref())
}

pub fn ingest_with_owner(
    path: &Path,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
) -> Result<Vec<EpisodeDraft>> {
    let read = read_with_owner(path, scope, owner)?;
    if read.drafts.is_empty() {
        return Err(anyhow!("no conversations found in {}", path.display()));
    }
    Ok(read.drafts)
}

/// `ingest`, with every ChatGPT conversation the owner flagged "do not
/// remember" (`openai::ExportRead`): one with no turns on the branch it
/// ended on gets no draft, but a copy stored before is still vaulted.
pub fn read(path: &Path, scope: Option<&str>) -> Result<crate::openai::ExportRead> {
    read_with_owner(path, scope, owner_of(path).as_ref())
}

/// The account owner a Claude export names in the `users.json` beside it.
fn owner_of(path: &Path) -> Option<(String, String)> {
    path.parent()
        .map(|dir| dir.join("users.json"))
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|users| {
            let user = users.get(0)?;
            Some((
                user.get("email_address")?.as_str()?.to_lowercase(),
                user.get("full_name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("me")
                    .to_string(),
            ))
        })
}

/// An error only when the file holds neither a conversation to store nor
/// a flagged one.
fn read_with_owner(
    path: &Path,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
) -> Result<crate::openai::ExportRead> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    let conversations = value
        .as_array()
        .ok_or_else(|| anyhow!("expected a top-level array of conversations"))?;

    let mut drafts = Vec::new();
    let mut flagged = Vec::new();
    for conversation in conversations {
        let result = if conversation.get("chat_messages").is_some() {
            claude_conversation(conversation, scope, owner)
        } else if conversation.get("mapping").is_some() {
            chatgpt_conversation(conversation, scope, owner, &mut flagged)
        } else {
            Err(anyhow!("unrecognized conversation shape"))
        };
        match result {
            Ok(Some(draft)) => drafts.push(draft),
            Ok(None) => {} // empty conversation
            Err(err) => tracing::warn!("skipping conversation: {err}"),
        }
    }
    if drafts.is_empty() && flagged.is_empty() {
        return Err(anyhow!("no conversations found in {}", path.display()));
    }
    Ok(crate::openai::ExportRead { drafts, flagged })
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

/// One conversation's transcript, whichever provider exported it.
struct Transcript {
    id: String,
    title: String,
    event_time: EventTime,
    turns: Vec<(String, String)>,
    starred: bool,
}

fn build_draft(
    provider: &str,
    transcript: Transcript,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
) -> Option<EpisodeDraft> {
    let Transcript {
        id,
        title,
        event_time,
        turns,
        starred,
    } = transcript;
    if turns.is_empty() {
        return None;
    }
    let entities = owner
        .map(|(email, name)| {
            vec![hearthloom_core::EntityRef {
                entity_type: "Person".into(),
                name: name.clone(),
                attributes: vec![("email".into(), email.clone())],
                role: "author".into(),
            }]
        })
        .unwrap_or_default();
    let mut body = format!("conversation: {title}\nprovider: {provider}\n");
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
    Some(EpisodeDraft {
        source: format!("chatlog-{provider}"),
        source_ref: id,
        kind: "conversation".into(),
        title,
        body,
        event_time,
        trust: "self".into(),
        scope: scope.map(String::from),
        entities,
        edges: vec![],
    })
}

fn claude_conversation(
    conversation: &Value,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
) -> Result<Option<EpisodeDraft>> {
    let id = conversation
        .get("uuid")
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("claude conversation without uuid"))?;
    let title = conversation
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("(untitled conversation)")
        .to_string();
    let event_time = conversation
        .get("created_at")
        .and_then(|c| c.as_str())
        .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
        .map(|dt| EventTime::from_fixed(dt, TimeUncertainty::Exact))
        .unwrap_or_else(EventTime::unknown);

    let mut turns = Vec::new();
    for message in conversation
        .get("chat_messages")
        .and_then(|m| m.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let sender = match message.get("sender").and_then(|s| s.as_str()) {
            Some("human") => "you",
            Some("assistant") => "claude",
            other => other.unwrap_or("unknown"),
        };
        // Newer exports use content blocks; older ones a bare `text`.
        let mut text = String::new();
        if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                        text.push('\n');
                    }
                }
            }
        }
        if text.trim().is_empty() {
            if let Some(t) = message.get("text").and_then(|t| t.as_str()) {
                text = t.to_string();
            }
        }
        if !text.trim().is_empty() {
            turns.push((sender.to_string(), text));
        }
    }
    Ok(build_draft(
        "claude",
        Transcript {
            id: id.to_string(),
            title,
            event_time,
            turns,
            starred: false,
        },
        scope,
        owner,
    ))
}

/// A ChatGPT conversation, read by the export adapter's reader
/// (`openai::read_chatgpt`) so its flags mean the same on both routes:
/// "do not remember" lands private over any scope passed, and is noted in
/// `flagged` whether or not a transcript is left. Image parts are left out
/// (this route has no home to copy them into).
fn chatgpt_conversation(
    conversation: &Value,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
    flagged: &mut Vec<(String, String)>,
) -> Result<Option<EpisodeDraft>> {
    let read = crate::openai::read_chatgpt(conversation, None)?;
    if read.do_not_remember {
        flagged.push(("chatlog-chatgpt".to_string(), read.id.clone()));
    }
    let scope = if read.do_not_remember {
        Some("private")
    } else {
        scope
    };
    Ok(build_draft(
        "chatgpt",
        Transcript {
            id: read.id,
            title: read.title,
            event_time: read.event_time,
            turns: read.turns,
            starred: read.starred,
        },
        scope,
        owner,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_export() {
        let json = serde_json::json!([{
            "uuid": "conv-1",
            "name": "Designing the second brain",
            "created_at": "2026-07-01T14:00:00-04:00",
            "chat_messages": [
                {"sender": "human", "content": [{"type": "text", "text": "How should I store episodes?"}]},
                {"sender": "assistant", "content": [{"type": "text", "text": "Append-only, in SQLite."}]},
                {"sender": "human", "text": ""}
            ]
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conversations.json");
        std::fs::write(&path, json.to_string()).unwrap();

        let drafts = ingest(&path, None).unwrap();
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(draft.source, "chatlog-claude");
        assert_eq!(draft.source_ref, "conv-1");
        assert_eq!(draft.title, "Designing the second brain");
        assert_eq!(
            draft.event_time.utc.as_deref(),
            Some("2026-07-01T18:00:00Z")
        );
        assert!(draft.body.contains("you: How should I store episodes?"));
        assert!(draft.body.contains("claude: Append-only, in SQLite."));
    }

    #[test]
    fn test_chatgpt_export() {
        let json = serde_json::json!([{
            "id": "gpt-conv-9",
            "title": "Rust lifetimes",
            "create_time": 1751378400.5,
            "mapping": {
                "n2": {"message": {"author": {"role": "assistant"}, "create_time": 1751378460.0,
                        "content": {"parts": ["They borrow."]}}},
                "n1": {"message": {"author": {"role": "user"}, "create_time": 1751378400.5,
                        "content": {"parts": ["Explain lifetimes"]}}},
                "n0": {"message": {"author": {"role": "system"}, "create_time": 0.0,
                        "content": {"parts": ["ignored"]}}}
            }
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conversations.json");
        std::fs::write(&path, json.to_string()).unwrap();

        let drafts = ingest(&path, None).unwrap();
        let draft = &drafts[0];
        assert_eq!(draft.source, "chatlog-chatgpt");
        // Turns ordered by create_time despite mapping-key order.
        let you = draft.body.find("you: Explain lifetimes").unwrap();
        let bot = draft.body.find("chatgpt: They borrow.").unwrap();
        assert!(you < bot);
        assert!(!draft.body.contains("ignored"));
        // No timezone in ChatGPT exports: offset honestly absent.
        assert_eq!(draft.event_time.tz_offset_minutes, None);
    }

    /// A lone `conversations.json` reads the owner's flags as the export
    /// adapter does: "do not remember" lands private over the scope
    /// passed, and a starred conversation says so.
    #[test]
    fn test_chatgpt_flags_hold_on_the_single_file_route() {
        let json = serde_json::json!([
            {
                "id": "gpt-forget",
                "title": "Forget this",
                "create_time": 1751378400.0,
                "is_do_not_remember": true,
                "is_starred": true,
                "mapping": {
                    "n1": {"message": {"author": {"role": "user"}, "create_time": 1751378400.0,
                            "content": {"parts": ["keep this between us"]}}}
                }
            },
            {
                "id": "gpt-plain",
                "title": "Plain",
                "create_time": 1751378400.0,
                "mapping": {
                    "n1": {"message": {"author": {"role": "user"}, "create_time": 1751378400.0,
                            "content": {"parts": ["hello"]}}}
                }
            }
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conversations.json");
        std::fs::write(&path, json.to_string()).unwrap();

        let drafts = ingest(&path, Some("work")).unwrap();
        let forget = drafts
            .iter()
            .find(|d| d.source_ref == "gpt-forget")
            .unwrap();
        assert_eq!(forget.source, "chatlog-chatgpt");
        assert_eq!(forget.scope.as_deref(), Some("private"));
        assert!(forget.body.contains("starred: true"));
        let plain = drafts.iter().find(|d| d.source_ref == "gpt-plain").unwrap();
        assert_eq!(plain.scope.as_deref(), Some("work"));
        assert!(!plain.body.contains("starred"));
    }

    /// The transcript follows the branch the conversation ended on: an
    /// answer the owner regenerated away from isn't part of it.
    #[test]
    fn test_chatgpt_transcript_follows_the_kept_branch() {
        let json = serde_json::json!([{
            "id": "gpt-branch",
            "title": "Branches",
            "create_time": 1751378400.0,
            "current_node": "a2",
            "mapping": {
                "root": {"message": null, "parent": null, "children": ["u1"]},
                "u1": {"parent": "root", "children": ["a1", "a2"],
                       "message": {"author": {"role": "user"}, "create_time": 1751378400.0,
                                   "content": {"parts": ["Which route?"]}}},
                "a1": {"parent": "u1", "children": [],
                       "message": {"author": {"role": "assistant"}, "create_time": 1751378500.0,
                                   "content": {"parts": ["The discarded answer."]}}},
                "a2": {"parent": "u1", "children": [],
                       "message": {"author": {"role": "assistant"}, "create_time": 1751378450.0,
                                   "content": {"parts": ["The kept answer."]}}}
            }
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conversations.json");
        std::fs::write(&path, json.to_string()).unwrap();

        let drafts = ingest(&path, None).unwrap();
        let body = &drafts[0].body;
        let you = body.find("you: Which route?").unwrap();
        let kept = body.find("chatgpt: The kept answer.").unwrap();
        assert!(you < kept);
        assert!(!body.contains("discarded"), "{body}");
    }
}

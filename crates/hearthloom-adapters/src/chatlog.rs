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
    let owner = path
        .parent()
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
        });
    ingest_with_owner(path, scope, owner.as_ref())
}

pub fn ingest_with_owner(
    path: &Path,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
) -> Result<Vec<EpisodeDraft>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    let conversations = value
        .as_array()
        .ok_or_else(|| anyhow!("expected a top-level array of conversations"))?;

    let mut drafts = Vec::new();
    for conversation in conversations {
        let result = if conversation.get("chat_messages").is_some() {
            claude_conversation(conversation, scope, owner)
        } else if conversation.get("mapping").is_some() {
            chatgpt_conversation(conversation, scope, owner)
        } else {
            Err(anyhow!("unrecognized conversation shape"))
        };
        match result {
            Ok(Some(draft)) => drafts.push(draft),
            Ok(None) => {} // empty conversation
            Err(err) => tracing::warn!("skipping conversation: {err}"),
        }
    }
    if drafts.is_empty() {
        return Err(anyhow!("no conversations found in {}", path.display()));
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

fn build_draft(
    provider: &str,
    id: &str,
    title: String,
    event_time: EventTime,
    turns: Vec<(String, String)>,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
) -> Option<EpisodeDraft> {
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
    let mut body = format!("conversation: {title}\nprovider: {provider}\n\n");
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
        source_ref: id.to_string(),
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
        "claude", id, title, event_time, turns, scope, owner,
    ))
}

fn chatgpt_conversation(
    conversation: &Value,
    scope: Option<&str>,
    owner: Option<&(String, String)>,
) -> Result<Option<EpisodeDraft>> {
    let id = conversation
        .get("conversation_id")
        .or_else(|| conversation.get("id"))
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow!("chatgpt conversation without id"))?;
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

    // The mapping is a tree; message create_time orders the turns.
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
            continue;
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
    messages.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let turns = messages.into_iter().map(|(_, s, t)| (s, t)).collect();
    Ok(build_draft(
        "chatgpt", id, title, event_time, turns, scope, owner,
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
}

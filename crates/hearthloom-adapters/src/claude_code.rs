//! Claude Code session adapter: local `~/.claude/projects/*/*.jsonl`
//! transcripts — the richest "what was I thinking/deciding" signal that
//! already lives on disk. One session = one episode, linked to the
//! Project entity for the workspace it ran in.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use serde_json::Value;

const MESSAGE_CAP: usize = 1500;
const BODY_CAP: usize = 64 * 1024;

/// Ingest all sessions under a Claude Code projects dir (default
/// `~/.claude/projects`).
pub fn ingest(dir: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let mut files = Vec::new();
    collect_session_files(dir, &mut files)?;
    if files.is_empty() {
        return Err(anyhow!("no session files under {}", dir.display()));
    }
    let mut drafts = Vec::new();
    for file in files {
        match session_to_draft(&file, scope) {
            Ok(Some(draft)) => drafts.push(draft),
            Ok(None) => {} // empty / agent-only session
            Err(err) => tracing::warn!("skipping {}: {err}", file.display()),
        }
    }
    if drafts.is_empty() {
        return Err(anyhow!("no non-empty sessions under {}", dir.display()));
    }
    Ok(drafts)
}

fn collect_session_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            collect_session_files(&path, out)?;
        } else if name.ends_with(".jsonl") && !name.starts_with("agent-") {
            // agent-*.jsonl are subagent transcripts of a parent session.
            out.push(path);
        }
    }
    Ok(())
}

/// Pull displayable text out of a message content value (string or an
/// array of typed blocks).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                        text.push('\n');
                    }
                }
            }
            text
        }
        _ => String::new(),
    }
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

fn session_to_draft(path: &Path, scope: Option<&str>) -> Result<Option<EpisodeDraft>> {
    let raw = std::fs::read_to_string(path)?;
    let mut session_id = None;
    let mut cwd: Option<String> = None;
    let mut first_time: Option<String> = None;
    let mut turns: Vec<(String, String)> = Vec::new();

    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if session_id.is_none() {
            session_id = value
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(String::from);
        }
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(|c| c.as_str()).map(String::from);
        }
        let kind = value.get("type").and_then(|t| t.as_str());
        if kind != Some("user") && kind != Some("assistant") {
            continue;
        }
        if first_time.is_none() {
            first_time = value
                .get("timestamp")
                .and_then(|t| t.as_str())
                .map(String::from);
        }
        // Skip tool_result-only user turns and meta entries.
        if value.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue;
        }
        let Some(content) = value.pointer("/message/content") else {
            continue;
        };
        let text = content_text(content);
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.starts_with("<command-name>") {
            continue;
        }
        let speaker = if kind == Some("user") { "you" } else { "claude" };
        turns.push((speaker.into(), text));
    }

    // Only sessions where the human actually said something count.
    if !turns.iter().any(|(s, _)| s == "you") {
        return Ok(None);
    }
    let session_id = session_id.unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let event_time = first_time
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| EventTime::from_fixed(dt, TimeUncertainty::Exact))
        .unwrap_or_else(EventTime::unknown);

    let first_prompt = turns
        .iter()
        .find(|(s, _)| s == "you")
        .map(|(_, t)| t.trim().replace('\n', " "))
        .unwrap_or_default();
    let title: String = first_prompt.chars().take(80).collect();

    let workspace = cwd.clone().unwrap_or_default();
    let project = Path::new(&workspace)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty());

    let mut body = format!("claude code session {session_id}\nworkspace: {workspace}\n\n");
    for (speaker, text) in &turns {
        let entry = format!("{speaker}: {}\n\n", cap(text.trim(), MESSAGE_CAP));
        if body.len() + entry.len() > BODY_CAP {
            body.push_str("[transcript truncated]\n");
            break;
        }
        body.push_str(&entry);
    }

    let mut entities = Vec::new();
    if let Some(name) = project {
        entities.push(EntityRef {
            entity_type: "Project".into(),
            name,
            attributes: vec![("repo-path".into(), workspace)],
            role: "workspace".into(),
        });
    }

    Ok(Some(EpisodeDraft {
        source: "claude-code".into(),
        source_ref: session_id,
        kind: "agent_session".into(),
        title,
        body,
        event_time,
        trust: "self".into(),
        scope: scope.map(String::from),
        entities,
        edges: vec![],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_session_parsing() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-Users-jon-myproj");
        fs::create_dir_all(&proj).unwrap();
        let session = proj.join("11111111-2222-3333-4444-555555555555.jsonl");
        let lines = [
            serde_json::json!({"type":"user","sessionId":"sess-1","cwd":"/Users/you/myproj",
                "timestamp":"2026-07-02T15:00:00.000Z",
                "message":{"role":"user","content":"Fix the flaky login test"}}),
            serde_json::json!({"type":"assistant","sessionId":"sess-1",
                "timestamp":"2026-07-02T15:00:10.000Z",
                "message":{"role":"assistant","content":[{"type":"text","text":"Found the race in setup."},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}),
            serde_json::json!({"type":"user","isMeta":true,
                "message":{"role":"user","content":"meta noise"}}),
        ];
        fs::write(
            &session,
            lines.map(|l| l.to_string()).join("\n"),
        )
        .unwrap();
        // Subagent transcript must be skipped.
        fs::write(proj.join("agent-abc.jsonl"), "{}").unwrap();
        // Session with no human turns must be skipped.
        fs::write(
            proj.join("99999999-8888-7777-6666-555555555555.jsonl"),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":"hi"}})
                .to_string(),
        )
        .unwrap();

        let drafts = ingest(tmp.path(), None).unwrap();
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(draft.source_ref, "sess-1");
        assert_eq!(draft.kind, "agent_session");
        assert_eq!(draft.title, "Fix the flaky login test");
        assert_eq!(draft.event_time.utc.as_deref(), Some("2026-07-02T15:00:00Z"));
        assert!(draft.body.contains("you: Fix the flaky login test"));
        assert!(draft.body.contains("claude: Found the race in setup."));
        assert!(!draft.body.contains("meta noise"));
        assert_eq!(draft.entities[0].entity_type, "Project");
        assert_eq!(draft.entities[0].name, "myproj");
    }
}

//! Claude Code session adapter: local `~/.claude/projects/*/*.jsonl`
//! transcripts — the richest "what was I thinking/deciding" signal that
//! already lives on disk. One session = one episode, linked to the
//! Project entity for the workspace it ran in.
//!
//! A session is read only once its file has stopped changing for
//! [`QUIET`], so a session still running waits for a later run. A session
//! picked up again after it was stored lands as a fuller copy (the store's
//! growing-source revisions): the text's second line says how many turns
//! it holds (`turns: <n>`), and a session too long to keep whole keeps its
//! opening and its latest turns.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use serde_json::Value;

const MESSAGE_CAP: usize = 1500;
const BODY_CAP: usize = 64 * 1024;
/// How long a session file must go unchanged before it is read: long
/// enough to span a night's pause in a session left open.
pub const QUIET: Duration = Duration::from_secs(12 * 3600);

/// Which session files a run reads, by when each last changed
/// ([`changed_at`]).
#[derive(Debug, Clone, Copy)]
pub struct Window {
    /// Files that last changed before this were read by an earlier run
    /// (`None`: read them all).
    pub changed_since: Option<SystemTime>,
    /// Files that changed at or after this are still in use and wait.
    pub settled_before: SystemTime,
}

impl Window {
    /// The files settled at `now` that changed since `changed_since` —
    /// pass the previous run's `settled_before` to read only what that run
    /// hadn't.
    pub fn at(now: SystemTime, changed_since: Option<SystemTime>) -> Window {
        Window {
            changed_since,
            settled_before: now.checked_sub(QUIET).unwrap_or(SystemTime::UNIX_EPOCH),
        }
    }

    fn holds(&self, changed: SystemTime) -> bool {
        changed < self.settled_before && self.changed_since.is_none_or(|since| changed >= since)
    }
}

/// When a file last changed: its modification time, or the time its entry
/// last changed if that is later. A file copied or moved in with an old
/// modification time (a restore, a sync) still counts as new.
pub fn changed_at(meta: &std::fs::Metadata) -> Option<SystemTime> {
    let modified = meta.modified().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ctime = u64::try_from(meta.ctime())
            .ok()
            .zip(u32::try_from(meta.ctime_nsec()).ok())
            .and_then(|(s, ns)| SystemTime::UNIX_EPOCH.checked_add(Duration::new(s, ns)));
        if let Some(ctime) = ctime {
            return Some(modified.max(ctime));
        }
    }
    Some(modified)
}

/// Ingest the settled sessions under a Claude Code projects dir (default
/// `~/.claude/projects`).
pub fn ingest(dir: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    ingest_except(dir, scope, &[])
}

/// `ingest`, also leaving out every session that ran inside one of
/// `agent_dirs` (the working folders of the node's agents). Sessions a
/// Hearthloom agent ran are left out wherever they ran (they open with
/// [`hearthloom_core::AGENT_PREAMBLE_MARK`]): an agent's transcript is not
/// the owner's words, and its outcome is recorded as an `agent_task`.
pub fn ingest_except(
    dir: &Path,
    scope: Option<&str>,
    agent_dirs: &[PathBuf],
) -> Result<Vec<EpisodeDraft>> {
    Ok(ingest_window(
        dir,
        scope,
        agent_dirs,
        Window::at(SystemTime::now(), None),
        None,
    )?
    .drafts)
}

/// What [`ingest_window`] read.
#[derive(Debug)]
pub struct SessionRead {
    pub drafts: Vec<EpisodeDraft>,
    /// The next run's `changed_since`: the window's `settled_before`, or
    /// earlier when a file in the window couldn't be read — back to when
    /// that file last changed, so the next run reads it again.
    pub read_through: SystemTime,
    /// The project folders under the dir now: the next run's
    /// `known_folders`.
    pub folders: Vec<String>,
}

/// `ingest_except` over the session files in `window`. A project folder
/// not in `known_folders` (when given) is read whole, settled files of
/// any age: a folder moved in keeps its files' times, which can predate
/// `changed_since`. Nothing in the window is not an error; a dir with no
/// session files at all is.
pub fn ingest_window(
    dir: &Path,
    scope: Option<&str>,
    agent_dirs: &[PathBuf],
    window: Window,
    known_folders: Option<&[String]>,
) -> Result<SessionRead> {
    let mut files = Vec::new();
    collect_session_files(dir, None, &mut files)?;
    if files.is_empty() {
        return Err(anyhow!("no session files under {}", dir.display()));
    }
    let folders: BTreeSet<String> = files.iter().filter_map(|f| f.folder.clone()).collect();
    let is_new = |folder: &Option<String>| {
        folder
            .as_ref()
            .is_some_and(|f| known_folders.is_some_and(|known| !known.contains(f)))
    };
    let mut drafts = Vec::new();
    let mut read_through = window.settled_before;
    for file in files {
        let changed = match file.changed {
            Ok(changed) => changed,
            // gone since it was listed: nothing to read
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                tracing::warn!("{} can't be read now: {err}", file.path.display());
                read_through =
                    read_through.min(window.changed_since.unwrap_or(SystemTime::UNIX_EPOCH));
                continue;
            }
        };
        let in_window = if is_new(&file.folder) {
            changed < window.settled_before
        } else {
            window.holds(changed)
        };
        if !in_window {
            continue;
        }
        match session_to_draft(&file.path, scope, agent_dirs) {
            Ok(Some(draft)) => drafts.push(draft),
            Ok(None) => {} // empty / agent-only session
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!("{} can't be read now: {err}", file.path.display());
                read_through = read_through.min(changed);
            }
        }
    }
    Ok(SessionRead {
        drafts,
        read_through,
        folders: folders.into_iter().collect(),
    })
}

/// A session file found under the projects dir.
struct SessionFile {
    path: PathBuf,
    /// When it last changed ([`changed_at`]).
    changed: std::io::Result<SystemTime>,
    /// The top-level project folder it sits in (`None` at the top).
    folder: Option<String>,
}

/// Session files under `dir`, each with when it last changed and the
/// project folder it is in.
fn collect_session_files(
    dir: &Path,
    folder: Option<&str>,
    out: &mut Vec<SessionFile>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            collect_session_files(&path, Some(folder.unwrap_or(&name)), out)?;
        } else if name.ends_with(".jsonl") && !name.starts_with("agent-") {
            // agent-*.jsonl are subagent transcripts of a parent session.
            let changed = std::fs::metadata(&path).and_then(|m| {
                changed_at(&m).ok_or_else(|| std::io::Error::other("no modification time"))
            });
            out.push(SessionFile {
                path,
                changed,
                folder: folder.map(String::from),
            });
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

fn session_to_draft(
    path: &Path,
    scope: Option<&str>,
    agent_dirs: &[PathBuf],
) -> std::io::Result<Option<EpisodeDraft>> {
    let raw = std::fs::read(path)?;
    let raw = String::from_utf8_lossy(&raw);
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
        let speaker = if kind == Some("user") {
            "you"
        } else {
            "claude"
        };
        turns.push((speaker.into(), text));
    }

    // Only sessions where the human actually said something count.
    if !turns.iter().any(|(s, _)| s == "you") {
        return Ok(None);
    }
    // A ref never carries `#`: `<id>#r<n>` names a fuller copy of `<id>`.
    let session_id = session_id
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .replace('#', "%23");
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
    // A Hearthloom agent's run, or a session in an agent's folder.
    if first_prompt.starts_with(hearthloom_core::AGENT_PREAMBLE_MARK) {
        return Ok(None);
    }
    let title: String = first_prompt.chars().take(80).collect();

    let workspace = cwd.clone().unwrap_or_default();
    if !workspace.is_empty()
        && agent_dirs
            .iter()
            .any(|d| Path::new(&workspace).starts_with(d))
    {
        return Ok(None);
    }
    let project = Path::new(&workspace)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty());

    // The turn count heads the text, so a session picked up again counts as
    // fuller even when the text can't hold every turn; a session too long
    // to keep whole keeps its opening and its latest turns.
    let entries: Vec<String> = turns
        .iter()
        .map(|(speaker, text)| format!("{speaker}: {}\n\n", cap(text.trim(), MESSAGE_CAP)))
        .collect();
    let body = crate::fit_entries(
        format!(
            "claude code session {session_id}\nturns: {}\nworkspace: {workspace}\n\n",
            entries.len()
        ),
        &entries,
        BODY_CAP,
        |n| format!("[{n} turns not shown]\n\n"),
    );

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

    /// A window far enough ahead that every file written now has settled.
    fn later() -> Window {
        Window::at(SystemTime::now() + QUIET + Duration::from_secs(60), None)
    }

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
        fs::write(&session, lines.map(|l| l.to_string()).join("\n")).unwrap();
        // Subagent transcript must be skipped.
        fs::write(proj.join("agent-abc.jsonl"), "{}").unwrap();
        // Session with no human turns must be skipped.
        fs::write(
            proj.join("99999999-8888-7777-6666-555555555555.jsonl"),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":"hi"}})
                .to_string(),
        )
        .unwrap();

        let drafts = ingest_window(tmp.path(), None, &[], later(), None)
            .unwrap()
            .drafts;
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(draft.source_ref, "sess-1");
        assert_eq!(draft.kind, "agent_session");
        assert_eq!(draft.title, "Fix the flaky login test");
        assert_eq!(
            draft.event_time.utc.as_deref(),
            Some("2026-07-02T15:00:00Z")
        );
        assert!(draft.body.contains("you: Fix the flaky login test"));
        assert!(draft.body.contains("claude: Found the race in setup."));
        assert!(!draft.body.contains("meta noise"));
        assert_eq!(draft.entities[0].entity_type, "Project");
        assert_eq!(draft.entities[0].name, "myproj");
    }

    /// An agent's runs are not the owner's words: a session that opens with
    /// the agent preamble is left out wherever it ran, and so is any session
    /// inside an agent's working folder. The owner's own sessions stay.
    #[test]
    fn test_agent_sessions_are_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |dir: &str, id: &str, cwd: &str, prompt: &str| {
            let proj = tmp.path().join(dir);
            fs::create_dir_all(&proj).unwrap();
            let line = serde_json::json!({"type":"user","sessionId":id,"cwd":cwd,
                "timestamp":"2026-11-02T10:00:00.000Z",
                "message":{"role":"user","content":prompt}});
            fs::write(proj.join(format!("{id}.jsonl")), line.to_string()).unwrap();
        };
        write(
            "-Users-you-notes",
            "mine",
            "/Users/you/notes",
            "Tidy my reading list",
        );
        write(
            "-Users-you-elsewhere",
            "preamble",
            "/Users/you/elsewhere",
            &format!(
                "{} (a personal memory system), as the agent named \"Desk hand\". Book it.",
                hearthloom_core::AGENT_PREAMBLE_MARK
            ),
        );
        write(
            "-Users-you-agents-desk",
            "in-folder",
            "/Users/you/agents/desk",
            "Summarise the inbox",
        );
        write(
            "-Users-you-agents-desk-sub",
            "in-subfolder",
            "/Users/you/agents/desk/sub",
            "Look deeper",
        );
        // A sibling whose name merely starts the same way is not inside it.
        write(
            "-Users-you-agents-desktop",
            "sibling",
            "/Users/you/agents/desktop",
            "Mine too",
        );

        let agent_dirs = [PathBuf::from("/Users/you/agents/desk")];
        let mut kept: Vec<String> = ingest_window(tmp.path(), None, &agent_dirs, later(), None)
            .unwrap()
            .drafts
            .into_iter()
            .map(|d| d.source_ref)
            .collect();
        kept.sort();
        assert_eq!(kept, ["mine", "sibling"]);

        // Without the folder list, only the preamble rule applies.
        let mut kept: Vec<String> = ingest_window(tmp.path(), None, &[], later(), None)
            .unwrap()
            .drafts
            .into_iter()
            .map(|d| d.source_ref)
            .collect();
        kept.sort();
        assert_eq!(kept, ["in-folder", "in-subfolder", "mine", "sibling"]);
    }

    fn write_session(dir: &Path, id: &str) {
        let proj = dir.join("-Users-you-proj");
        fs::create_dir_all(&proj).unwrap();
        let line = serde_json::json!({"type":"user","sessionId":id,"cwd":"/Users/you/proj",
            "timestamp":"2026-11-02T10:00:00.000Z",
            "message":{"role":"user","content":"Plan the release"}});
        fs::write(proj.join(format!("{id}.jsonl")), line.to_string()).unwrap();
    }

    fn read(dir: &Path, window: Window) -> Vec<String> {
        let mut refs: Vec<String> = ingest_window(dir, None, &[], window, None)
            .unwrap()
            .drafts
            .into_iter()
            .map(|d| d.source_ref)
            .collect();
        refs.sort();
        refs
    }

    /// A session still being written is left for a later run: its episode
    /// is stored once.
    #[test]
    fn test_a_session_still_changing_waits() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(tmp.path(), "running");
        assert!(ingest(tmp.path(), None).unwrap().is_empty());
        assert!(read(tmp.path(), Window::at(SystemTime::now() + QUIET / 2, None)).is_empty());
        assert_eq!(read(tmp.path(), later()), ["running"]);
    }

    /// A run that passes the previous run's `settled_before` reads only the
    /// files changed since, including one moved in with an old
    /// modification time.
    #[test]
    fn test_a_run_reads_only_what_changed_since_the_last() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(tmp.path(), "first");
        std::thread::sleep(Duration::from_millis(50));
        let mark = SystemTime::now();
        std::thread::sleep(Duration::from_millis(50));
        write_session(tmp.path(), "second");
        // copied in with a modification time from long before the mark
        let elsewhere = tempfile::tempdir().unwrap();
        write_session(elsewhere.path(), "restored");
        let old = SystemTime::now() - Duration::from_secs(90 * 24 * 3600);
        let from = elsewhere.path().join("-Users-you-proj/restored.jsonl");
        fs::File::options()
            .write(true)
            .open(&from)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let to = tmp.path().join("-Users-you-proj/restored.jsonl");
        fs::rename(&from, &to).unwrap();
        assert!(fs::metadata(&to).unwrap().modified().unwrap() < mark);

        let after = SystemTime::now() + QUIET + Duration::from_secs(60);
        assert_eq!(
            read(tmp.path(), Window::at(after, None)),
            ["first", "restored", "second"]
        );
        let since_mark = Window::at(after, Some(mark));
        assert_eq!(read(tmp.path(), since_mark), ["restored", "second"]);
        // the next run starts where that one stopped: nothing new
        let next = Window::at(after, Some(since_mark.settled_before));
        assert!(read(tmp.path(), next).is_empty());
    }

    fn refs_of(read: &SessionRead) -> Vec<String> {
        let mut refs: Vec<String> = read.drafts.iter().map(|d| d.source_ref.clone()).collect();
        refs.sort();
        refs
    }

    /// A project folder moved in whole keeps its files' times, older than
    /// the last run: a folder the last run didn't list is read whole.
    #[test]
    fn test_a_project_folder_moved_in_is_read_whole() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(tmp.path(), "first");
        let after = SystemTime::now() + QUIET + Duration::from_secs(60);
        let one = ingest_window(tmp.path(), None, &[], Window::at(after, None), None).unwrap();
        assert_eq!(refs_of(&one), ["first"]);
        assert_eq!(one.folders, ["-Users-you-proj"]);

        // an older project, written elsewhere long ago, moved in whole
        let elsewhere = tempfile::tempdir().unwrap();
        let old_project = elsewhere.path().join("-Users-you-old");
        fs::create_dir_all(&old_project).unwrap();
        let line = serde_json::json!({"type":"user","sessionId":"old","cwd":"/Users/you/old",
            "timestamp":"2025-01-02T10:00:00.000Z",
            "message":{"role":"user","content":"An old plan"}});
        let file = old_project.join("old.jsonl");
        fs::write(&file, line.to_string()).unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(400 * 24 * 3600);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        fs::rename(&old_project, tmp.path().join("-Users-you-old")).unwrap();

        // what the time window alone would read: not the moved folder's
        let since = Window::at(after, Some(one.read_through));
        let two = ingest_window(tmp.path(), None, &[], since, Some(&one.folders)).unwrap();
        assert_eq!(refs_of(&two), ["old"]);
        assert_eq!(two.folders, ["-Users-you-old", "-Users-you-proj"]);
        // once listed, it is read by time like the rest
        let three = ingest_window(
            tmp.path(),
            None,
            &[],
            Window::at(after, Some(two.read_through)),
            Some(&two.folders),
        )
        .unwrap();
        assert!(three.drafts.is_empty());
    }

    /// A session file that can't be read keeps the next run's start at or
    /// before the time it last changed, so it is read once it can be.
    #[cfg(unix)]
    #[test]
    fn test_a_session_that_cannot_be_read_is_read_next_time() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        write_session(tmp.path(), "readable");
        write_session(tmp.path(), "locked");
        let locked = tmp.path().join("-Users-you-proj/locked.jsonl");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&locked).is_ok() {
            return; // permissions don't bind this user (root): nothing to test
        }
        let after = SystemTime::now() + QUIET + Duration::from_secs(60);
        let one = ingest_window(tmp.path(), None, &[], Window::at(after, None), None).unwrap();
        assert_eq!(refs_of(&one), ["readable"]);
        let locked_changed = changed_at(&fs::metadata(&locked).unwrap()).unwrap();
        assert!(one.read_through <= locked_changed);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        let two = ingest_window(
            tmp.path(),
            None,
            &[],
            Window::at(after, Some(one.read_through)),
            Some(&one.folders),
        )
        .unwrap();
        assert!(refs_of(&two).contains(&"locked".to_string()));
        assert_eq!(two.read_through, Window::at(after, None).settled_before);
    }

    #[test]
    fn test_turn_count_and_refs_without_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-Users-you-proj");
        fs::create_dir_all(&proj).unwrap();
        let lines = [
            serde_json::json!({"type":"user","sessionId":"odd#r9","cwd":"/Users/you/proj",
                "timestamp":"2026-11-02T10:00:00.000Z",
                "message":{"role":"user","content":"First ask"}}),
            serde_json::json!({"type":"assistant","sessionId":"odd#r9",
                "message":{"role":"assistant","content":"An answer"}}),
            serde_json::json!({"type":"user","sessionId":"odd#r9",
                "message":{"role":"user","content":"Second ask"}}),
        ];
        fs::write(
            proj.join("odd.jsonl"),
            lines.map(|l| l.to_string()).join("\n"),
        )
        .unwrap();
        let drafts = ingest_window(tmp.path(), None, &[], later(), None)
            .unwrap()
            .drafts;
        assert_eq!(drafts[0].source_ref, "odd%23r9");
        assert_eq!(
            hearthloom_core::store::growing_items("claude-code", &drafts[0].body),
            Some(3)
        );
    }

    /// A session too long to keep whole keeps its opening and its latest
    /// turns and counts every turn: picked up again, its new turns show and
    /// the copy counts as fuller.
    #[test]
    fn test_a_long_session_keeps_its_latest_turns_and_counts_them_all() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-Users-you-proj");
        fs::create_dir_all(&proj).unwrap();
        let file = proj.join("long.jsonl");
        let words = "w".repeat(1200);
        let mut lines: Vec<String> = (0..120)
            .map(|i| {
                let kind = if i % 2 == 0 { "user" } else { "assistant" };
                serde_json::json!({"type": kind, "sessionId": "long", "cwd": "/Users/you/proj",
                    "timestamp": "2026-11-02T10:00:00.000Z",
                    "message": {"role": kind, "content": format!("turn {i} {words}")}})
                .to_string()
            })
            .collect();
        fs::write(&file, lines.join("\n")).unwrap();
        let read = || {
            ingest_window(tmp.path(), None, &[], later(), None)
                .unwrap()
                .drafts
                .remove(0)
                .body
        };
        let count = |body: &str| hearthloom_core::store::growing_items("claude-code", body);
        let before = read();
        assert!(before.len() <= BODY_CAP);
        assert_eq!(count(&before), Some(120));
        assert!(before.contains("you: turn 0 "));
        assert!(before.contains("claude: turn 119 "));
        assert!(before.contains("turns not shown]"));

        lines.push(
            serde_json::json!({"type": "user", "sessionId": "long",
                "message": {"role": "user", "content": "picked up again"}})
            .to_string(),
        );
        fs::write(&file, lines.join("\n")).unwrap();
        let after = read();
        assert_eq!(count(&after), Some(121));
        assert!(after.contains("you: picked up again"));
    }
}

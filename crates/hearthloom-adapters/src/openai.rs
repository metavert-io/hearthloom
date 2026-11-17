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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use serde_json::Value;

use crate::cap;

/// Per-message and per-episode text caps: the condensed transcript is the
/// searchable body; the export directory itself remains the deep archive.
const MESSAGE_CAP: usize = 2000;
const BODY_CAP: usize = 64 * 1024;

/// Ingest an extracted ChatGPT export directory.
///
/// `media_out`, when set, is the base directory under which referenced image
/// assets are copied (`<media_out>/<conversation_id>/<original-name>`, or
/// `<stem>-<file-id>.<ext>` when another image of the conversation would
/// take the same name — [`names_among`]); `None`
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
    let mut chunks = chunk_files(dir).with_context(|| format!("reading {}", dir.display()))?;
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
            let names = image_names(conversation, &asset_names);
            let mut preserve = |pointer: &str, conversation_id: &str| {
                preserve_asset(
                    pointer,
                    conversation_id,
                    dir,
                    media_out,
                    &asset_names,
                    &names,
                )
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

/// One conversation of a ChatGPT export as each import route stores it
/// with this reader: the full export's (`openai`, images named as that
/// route names them) and the single file's (`chatlog-chatgpt`, image parts
/// left out). What `hearthloom repair chatgpt` compares stored copies with.
#[derive(Debug, Clone)]
pub struct ConversationRoutes {
    pub id: String,
    /// The owner flagged it "do not remember".
    pub do_not_remember: bool,
    /// The full export route's draft; None when the kept branch holds no
    /// turns (and so for the other route).
    pub export: Option<EpisodeDraft>,
    /// The single-file route's draft.
    pub single_file: Option<EpisodeDraft>,
    /// The image pointers the export route's transcript names.
    pub images: Vec<String>,
    /// Every turn of the conversation's tree — the branch it ended on and
    /// every branch moved away from — as the export route writes a turn
    /// ([`turn_entry`]), in no particular order. A transcript an earlier
    /// reader made of this export holds only these.
    pub every_turn_export: Vec<String>,
    /// The same, as the single-file route writes a turn (no image parts).
    pub every_turn_single_file: Vec<String>,
}

/// One turn as both routes write it in a transcript: `<speaker>: <text>`,
/// the text trimmed and capped (each followed by a blank line).
pub fn turn_entry(speaker: &str, text: &str) -> String {
    format!("{speaker}: {}", cap(text.trim(), MESSAGE_CAP))
}

/// Read an extracted ChatGPT export (chunk files, or one
/// `conversations.json`) the way both import routes read it now, writing
/// nothing: an image is named as the export route would name it when
/// copied under `media_out` (see [`copy_images`] for the copy itself).
pub fn read_routes(dir: &Path, media_out: Option<&Path>) -> Result<Vec<ConversationRoutes>> {
    if !dir.is_dir() {
        return Err(anyhow!("{} is not a directory", dir.display()));
    }
    let owner = std::fs::read_to_string(dir.join("user.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|u| Some(u.get("email")?.as_str()?.to_lowercase()));
    let single_owner = crate::chatlog::owner_of(&dir.join("conversations.json"));
    let asset_names: HashMap<String, String> =
        std::fs::read_to_string(dir.join("conversation_asset_file_names.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();
    let mut chunks = chunk_files(dir).with_context(|| format!("reading {}", dir.display()))?;
    if chunks.is_empty() && dir.join("conversations.json").is_file() {
        chunks.push(dir.join("conversations.json"));
    }
    if chunks.is_empty() {
        return Err(anyhow!(
            "no conversations.json or conversations-*.json in {}",
            dir.display()
        ));
    }
    let mut out = Vec::new();
    for chunk in &chunks {
        let raw = std::fs::read_to_string(chunk)
            .with_context(|| format!("reading {}", chunk.display()))?;
        let convs: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", chunk.display()))?;
        for conversation in convs.as_array().into_iter().flatten() {
            let names = image_names(conversation, &asset_names);
            let mut images = Vec::new();
            let mut name = |pointer: &str, conversation_id: &str| {
                images.push(pointer.to_string());
                match plan_asset(
                    pointer,
                    conversation_id,
                    dir,
                    media_out,
                    &asset_names,
                    &names,
                )? {
                    AssetPlan::Named(name) | AssetPlan::Copy { name, .. } => Some(name),
                }
            };
            let Ok(read) = read_chatgpt(conversation, Some(&mut name)) else {
                continue;
            };
            let mut name_only = |pointer: &str, conversation_id: &str| match plan_asset(
                pointer,
                conversation_id,
                dir,
                media_out,
                &asset_names,
                &names,
            )? {
                AssetPlan::Named(name) | AssetPlan::Copy { name, .. } => Some(name),
            };
            let every_turn_export = every_turn(conversation, &read.id, Some(&mut name_only));
            let every_turn_single_file = every_turn(conversation, &read.id, None);
            let id = read.id.clone();
            let do_not_remember = read.do_not_remember;
            let export = build_conversation(read, None, owner.as_deref());
            let single_file =
                crate::chatlog::single_file_draft(conversation, single_owner.as_ref())
                    .ok()
                    .flatten();
            out.push(ConversationRoutes {
                id,
                do_not_remember,
                export,
                single_file,
                images,
                every_turn_export,
                every_turn_single_file,
            });
        }
    }
    Ok(out)
}

/// Copy the images a conversation's transcript names (`pointers`, as
/// [`read_routes`] lists them) from the export in `dir` into `media_out`,
/// as the export route's import does: nothing already there is written
/// over. Returns how many are in place.
pub fn copy_images(
    dir: &Path,
    media_out: &Path,
    conversation_id: &str,
    pointers: &[String],
) -> usize {
    let asset_names: HashMap<String, String> =
        std::fs::read_to_string(dir.join("conversation_asset_file_names.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();
    let names = names_among(pointers, &asset_names);
    pointers
        .iter()
        .filter(|p| {
            preserve_asset(
                p,
                conversation_id,
                dir,
                Some(media_out),
                &asset_names,
                &names,
            )
            .is_some()
        })
        .count()
}

/// The name each image of this conversation's transcript is kept under,
/// by blob id ([`names_among`]) — so one image never takes another's
/// place. Read from the images the transcript names — the branch the
/// conversation ended on — as [`copy_images`] is given them.
fn image_names(conversation: &Value, asset_names: &HashMap<String, String>) -> ImageNames {
    let mut pointers = Vec::new();
    let mut collect = |pointer: &str, _: &str| {
        pointers.push(pointer.to_string());
        None
    };
    let _ = read_chatgpt(conversation, Some(&mut collect));
    names_among(&pointers, asset_names)
}

/// Blob id → the file name its image is kept under.
type ImageNames = HashMap<String, String>;

/// The names these image pointers' blobs are kept under, no two the same:
/// each its original name ([`asset_name`]); where two would take one name
/// (two pasted screenshots both called `image.png`, or `image-file-a.png`
/// beside the first of them once named by its blob), each of them is named
/// by its blob as well, and where that still meets another, by the blob's
/// own `<id>.dat` — distinct for distinct blobs. Names that differ only in
/// case or Unicode form are one name ([`disk_key`]): a Mac's disk keeps them
/// as one file.
fn names_among(pointers: &[String], asset_names: &HashMap<String, String>) -> ImageNames {
    let ids: HashSet<&str> = pointers
        .iter()
        .filter_map(|p| asset_id(p))
        .filter(|id| plain_file_name(&format!("{id}.dat")))
        .collect();
    let mut level: HashMap<&str, u8> = ids.iter().map(|id| (*id, 0)).collect();
    loop {
        let names: HashMap<&str, String> = ids
            .iter()
            .map(|id| (*id, asset_name(id, asset_names, level[id])))
            .collect();
        let mut claims: HashMap<String, Vec<&str>> = HashMap::new();
        for (id, name) in &names {
            claims.entry(disk_key(name)).or_default().push(id);
        }
        let raise: Vec<&str> = claims
            .values()
            .filter(|claimants| claimants.len() > 1)
            .flatten()
            .copied()
            .filter(|id| level[id] < 2)
            .collect();
        if raise.is_empty() {
            return names
                .into_iter()
                .map(|(id, name)| (id.to_string(), name))
                .collect();
        }
        for id in raise {
            *level.entry(id).or_default() += 1;
        }
    }
}

/// What a disk that ignores case and Unicode form (a Mac's, by default)
/// makes of `name`: `CAFÉ.png` and `café.png` written as `e` plus a
/// combining accent are one file there. Decomposed, each character folded
/// through its upper and then its lower case (`ß` and `SS` meet), decomposed
/// again — folding more than a disk does only names an image by its blob.
fn disk_key(name: &str) -> String {
    let nfd = icu_normalizer::DecomposingNormalizerBorrowed::new_nfd();
    let folded: String = nfd
        .normalize(name)
        .chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect();
    nfd.normalize(&folded).into_owned()
}

/// The file name blob `id`'s image takes at `level`: 0 its original name,
/// 1 that name with the blob's id in it ([`named_by_blob`]), 2 the blob's
/// own `<id>.dat` — also what a missing or unusable original name gives.
fn asset_name(id: &str, asset_names: &HashMap<String, String>, level: u8) -> String {
    let dat = format!("{id}.dat");
    let original = asset_names.get(&dat).filter(|name| plain_file_name(name));
    match (original, level) {
        (Some(name), 0) => name.clone(),
        (Some(name), 1) => Some(named_by_blob(name, id))
            .filter(|name| plain_file_name(name))
            .unwrap_or(dat),
        _ => dat,
    }
}

/// `name` with the blob's id before its extension: `image.png` of
/// `file-B` is `image-file-B.png`.
fn named_by_blob(name: &str, id: &str) -> String {
    match name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => format!("{stem}-{id}.{ext}"),
        None => format!("{name}-{id}"),
    }
}

/// A chunk of the multi-chunk export (`conversations-000.json`, …).
fn is_chunk_name(name: &str) -> bool {
    name.starts_with("conversations-") && name.ends_with(".json")
}

/// The export's chunk files in `dir`, in order.
fn chunk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut chunks: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_chunk_name)
        })
        .collect();
    chunks.sort();
    Ok(chunks)
}

/// Whether `dir` is a ChatGPT data export this adapter reads: chunk files,
/// or a single-file export's `conversations.json` beside its `chat.html`
/// (a Claude export names its file `conversations.json` too, so that name
/// alone doesn't say which).
pub fn is_export_dir(dir: &Path) -> bool {
    chunk_files(dir).is_ok_and(|chunks| !chunks.is_empty())
        || (dir.join("conversations.json").is_file() && dir.join("chat.html").is_file())
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
/// own `file-<id>.dat`, and an image of the conversation is kept under the
/// name `names` gives it ([`names_among`]: no two the same; one not there
/// under its original name). Only a regular file is copied, only into a
/// real folder (not a link to one elsewhere), and nothing already at the
/// target name (a file or a link) is written through.
fn preserve_asset(
    pointer: &str,
    conversation_id: &str,
    dir: &Path,
    media_out: Option<&Path>,
    asset_names: &HashMap<String, String>,
    names: &ImageNames,
) -> Option<String> {
    match plan_asset(pointer, conversation_id, dir, media_out, asset_names, names)? {
        AssetPlan::Named(name) => Some(name), // no home to copy into — just name it
        AssetPlan::Copy {
            src,
            target_dir,
            name,
        } => copy_asset(&src, &target_dir, name),
    }
}

/// What preserving one asset takes: naming it only (no home to copy
/// into), or copying the export's blob into the conversation's folder.
enum AssetPlan {
    Named(String),
    Copy {
        src: PathBuf,
        target_dir: PathBuf,
        name: String,
    },
}

/// The checks `preserve_asset` makes before it writes anything: the blob's
/// and the original's names are plain file names, the conversation id a
/// plain folder name, the blob a regular file.
fn plan_asset(
    pointer: &str,
    conversation_id: &str,
    dir: &Path,
    media_out: Option<&Path>,
    asset_names: &HashMap<String, String>,
    names: &ImageNames,
) -> Option<AssetPlan> {
    let id = asset_id(pointer)?;
    let dat = format!("{id}.dat");
    if !plain_file_name(&dat) {
        return None;
    }
    let original = names
        .get(id)
        .cloned()
        .unwrap_or_else(|| asset_name(id, asset_names, 0));
    let Some(base) = media_out else {
        return Some(AssetPlan::Named(original));
    };
    if !folder_name_ok(conversation_id) {
        return None;
    }
    let src = dir.join(&dat);
    match std::fs::symlink_metadata(&src) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return None,
    }
    Some(AssetPlan::Copy {
        src,
        target_dir: base.join(conversation_id),
        name: original,
    })
}

/// Copy a blob into `target_dir` under `original` (see `preserve_asset`).
fn copy_asset(src: &Path, target_dir: &Path, original: String) -> Option<String> {
    if !real_dir(target_dir) {
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
    let copied = std::fs::File::open(src).and_then(|mut from| std::io::copy(&mut from, &mut out));
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

    let turns = selected_branch(conversation)
        .into_iter()
        .filter_map(|node| node_turn(node, &id, &mut image))
        .collect();
    Ok(ChatgptConversation {
        id,
        title,
        event_time,
        turns,
        starred: flag("is_starred"),
        do_not_remember: flag("is_do_not_remember"),
    })
}

/// One node's turn: (speaker, text) for a user's or the assistant's
/// message with text; None for anything else (system and tool turns
/// aren't conversation). `image` names an image part's asset; without one,
/// image parts are left out.
fn node_turn(
    node: &Value,
    conversation_id: &str,
    image: &mut Option<ImageNamer<'_>>,
) -> Option<(String, String)> {
    let message = node.get("message")?;
    let role = message
        .pointer("/author/role")
        .and_then(|r| r.as_str())
        .unwrap_or("unknown");
    if role != "user" && role != "assistant" {
        return None;
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
        } else if part.get("content_type").and_then(|c| c.as_str()) == Some("image_asset_pointer") {
            let Some(image) = image.as_mut() else {
                continue;
            };
            let pointer = part
                .get("asset_pointer")
                .and_then(|p| p.as_str())
                .unwrap_or("");
            match image(pointer, conversation_id) {
                Some(name) => text.push_str(&format!("[image: {name}]\n")),
                None => text.push_str("[image: unavailable]\n"),
            }
        }
    }
    (!text.trim().is_empty()).then(|| (speaker.to_string(), text))
}

/// Every turn of a conversation's tree, on any branch, as a transcript
/// writes it ([`turn_entry`]), in the mapping's key order.
fn every_turn(
    conversation: &Value,
    conversation_id: &str,
    mut image: Option<ImageNamer<'_>>,
) -> Vec<String> {
    let Some(mapping) = conversation.get("mapping").and_then(|m| m.as_object()) else {
        return vec![];
    };
    let mut keys: Vec<&String> = mapping.keys().collect();
    keys.sort();
    keys.into_iter()
        .filter_map(|k| node_turn(&mapping[k], conversation_id, &mut image))
        .map(|(speaker, text)| turn_entry(&speaker, &text))
        .collect()
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

    /// Two images of one conversation with the same original name are
    /// both kept, each under its own name, and the transcript names each
    /// one — on the import and when the images are copied again; an image
    /// whose name no other of the conversation's has keeps it.
    #[test]
    fn test_images_sharing_a_name_in_one_conversation_are_each_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("export");
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "conversation_asset_file_names.json",
            r#"{"file-a.dat":"image.png","file-b.dat":"image.png","file-c.dat":"chart.png"}"#,
        );
        for (id, bytes) in [
            ("file-a", "first"),
            ("file-b", "second"),
            ("file-c", "third"),
        ] {
            write(&dir, &format!("{id}.dat"), bytes);
        }
        let turn = |id: &str| {
            serde_json::json!({"message": {"author": {"role": "user"},
                "content": {"content_type": "multimodal_text", "parts": [
                    {"content_type": "image_asset_pointer",
                     "asset_pointer": format!("file-service://{id}")}]}}})
        };
        let export = serde_json::json!([{
            "conversation_id": "c1", "title": "Screens", "create_time": 1720992777.0,
            "mapping": {"n1": turn("file-a"), "n2": turn("file-b"), "n3": turn("file-c")}
        }]);
        write(&dir, "conversations-000.json", &export.to_string());

        let media = tmp.path().join("media");
        let drafts = ingest(&dir, Some(&media), None).unwrap();
        let body = &drafts[0].body;
        for (name, bytes) in [
            ("image-file-a.png", "first"),
            ("image-file-b.png", "second"),
            ("chart.png", "third"),
        ] {
            assert!(body.contains(&format!("[image: {name}]")), "{body}");
            assert_eq!(
                std::fs::read_to_string(media.join("c1").join(name)).unwrap(),
                bytes
            );
        }
        // named the same way when only read, and copied again the same way
        let routes = read_routes(&dir, Some(&media)).unwrap();
        assert_eq!(routes[0].export.as_ref().unwrap().body, *body);
        let again = tmp.path().join("again");
        assert_eq!(copy_images(&dir, &again, "c1", &routes[0].images), 3);
        for name in ["image-file-a.png", "image-file-b.png", "chart.png"] {
            assert!(again.join("c1").join(name).is_file(), "{name}");
        }
    }

    /// An image whose original name is the one another image of the
    /// conversation is given by its blob (`image-file-a.png` beside two
    /// `image.png`s), in any case (a Mac's disk keeps `IMAGE-FILE-A.PNG`
    /// as the same file), doesn't take that image's place: every image is
    /// kept under a name of its own, on the import and when copied again.
    #[test]
    fn test_an_image_named_like_anothers_blob_name_is_kept_apart() {
        for third in ["image-file-a.png", "IMAGE-FILE-A.PNG"] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("export");
            std::fs::create_dir_all(&dir).unwrap();
            let asset_names = serde_json::json!({
                "file-a.dat": "image.png",
                "file-b.dat": "image.png",
                "file-c.dat": third,
            });
            write(
                &dir,
                "conversation_asset_file_names.json",
                &asset_names.to_string(),
            );
            for (id, bytes) in [
                ("file-a", "first"),
                ("file-b", "second"),
                ("file-c", "third"),
            ] {
                write(&dir, &format!("{id}.dat"), bytes);
            }
            let turn = |id: &str| {
                serde_json::json!({"message": {"author": {"role": "user"},
                    "content": {"content_type": "multimodal_text", "parts": [
                        {"content_type": "image_asset_pointer",
                         "asset_pointer": format!("file-service://{id}")}]}}})
            };
            let export = serde_json::json!([{
                "conversation_id": "c1", "title": "Screens", "create_time": 1720992777.0,
                "mapping": {"n1": turn("file-a"), "n2": turn("file-b"), "n3": turn("file-c")}
            }]);
            write(&dir, "conversations-000.json", &export.to_string());
            let named = |body: &str| -> Vec<String> {
                body.split("[image: ")
                    .skip(1)
                    .filter_map(|rest| rest.split_once(']'))
                    .map(|(name, _)| name.to_string())
                    .collect()
            };

            let media = tmp.path().join("media");
            let drafts = ingest(&dir, Some(&media), None).unwrap();
            let names = named(&drafts[0].body);
            assert_eq!(names.len(), 3, "{third}: {}", drafts[0].body);
            // no two of them one file on a disk that ignores case
            let folded: HashSet<String> = names.iter().map(|name| disk_key(name)).collect();
            assert_eq!(folded.len(), 3, "{third}: {names:?}");
            let mut kept: Vec<String> = names
                .iter()
                .map(|name| std::fs::read_to_string(media.join("c1").join(name)).unwrap())
                .collect();
            kept.sort();
            assert_eq!(kept, ["first", "second", "third"], "{third}");

            let routes = read_routes(&dir, Some(&media)).unwrap();
            assert_eq!(routes[0].export.as_ref().unwrap().body, drafts[0].body);
            let again = tmp.path().join("again");
            assert_eq!(copy_images(&dir, &again, "c1", &routes[0].images), 3);
            let mut copied: Vec<String> = names
                .iter()
                .map(|name| std::fs::read_to_string(again.join("c1").join(name)).unwrap())
                .collect();
            copied.sort();
            assert_eq!(copied, ["first", "second", "third"], "{third}");
        }
    }

    /// Two images whose original names differ only in case and Unicode form
    /// — `CAFÉ.png` with a precomposed `É`, `café.png` with an `e` and a
    /// combining accent — are one file on a Mac's disk, so each is named by
    /// its blob: neither takes the other's place, on the import and when
    /// copied again.
    #[test]
    fn test_images_named_alike_in_another_unicode_form_are_kept_apart() {
        let composed_upper = "CAF\u{C9}.png";
        let decomposed_lower = "cafe\u{301}.png";
        assert_eq!(disk_key(composed_upper), disk_key(decomposed_lower));
        assert_eq!(disk_key("STRASSE.png"), disk_key("stra\u{DF}e.png"));

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("export");
        std::fs::create_dir_all(&dir).unwrap();
        let asset_names = serde_json::json!({
            "file-a.dat": composed_upper,
            "file-b.dat": decomposed_lower,
        });
        write(
            &dir,
            "conversation_asset_file_names.json",
            &asset_names.to_string(),
        );
        write(&dir, "file-a.dat", "first");
        write(&dir, "file-b.dat", "second");
        let turn = |id: &str| {
            serde_json::json!({"message": {"author": {"role": "user"},
                "content": {"content_type": "multimodal_text", "parts": [
                    {"content_type": "image_asset_pointer",
                     "asset_pointer": format!("file-service://{id}")}]}}})
        };
        let export = serde_json::json!([{
            "conversation_id": "c1", "title": "Menus", "create_time": 1720992777.0,
            "mapping": {"n1": turn("file-a"), "n2": turn("file-b")}
        }]);
        write(&dir, "conversations-000.json", &export.to_string());

        let media = tmp.path().join("media");
        let drafts = ingest(&dir, Some(&media), None).unwrap();
        let body = &drafts[0].body;
        let expected = [
            ("CAF\u{C9}-file-a.png", "first"),
            ("cafe\u{301}-file-b.png", "second"),
        ];
        for (name, bytes) in expected {
            assert!(body.contains(&format!("[image: {name}]")), "{body}");
            assert_eq!(
                std::fs::read_to_string(media.join("c1").join(name)).unwrap(),
                bytes
            );
        }

        let routes = read_routes(&dir, Some(&media)).unwrap();
        assert_eq!(routes[0].export.as_ref().unwrap().body, *body);
        let again = tmp.path().join("again");
        assert_eq!(copy_images(&dir, &again, "c1", &routes[0].images), 2);
        for (name, bytes) in expected {
            assert_eq!(
                std::fs::read_to_string(again.join("c1").join(name)).unwrap(),
                bytes
            );
        }
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

    /// The transcript is the branch the conversation ended on: a prompt
    /// the owner edited and the answers to it, and an answer they
    /// regenerated, are left out.
    #[test]
    fn test_edited_prompts_and_regenerated_answers_stay_out() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "conversations-000.json",
            r#"[{
              "conversation_id":"c1","title":"Trip","create_time":1720992777.0,
              "current_node":"a2b",
              "mapping":{
                "root":{"parent":null,"children":["u1"],"message":null},
                "u1":{"parent":"root","children":["a1"],
                      "message":{"author":{"role":"user"},"create_time":1720992778.0,
                                 "content":{"parts":["Plan a trip"]}}},
                "a1":{"parent":"u1","children":["u2","u2e"],
                      "message":{"author":{"role":"assistant"},"create_time":1720992780.0,
                                 "content":{"parts":["Where to?"]}}},
                "u2":{"parent":"a1","children":["a2"],
                      "message":{"author":{"role":"user"},"create_time":1720992790.0,
                                 "content":{"parts":["The first wording"]}}},
                "a2":{"parent":"u2","children":[],
                      "message":{"author":{"role":"assistant"},"create_time":1720992791.0,
                                 "content":{"parts":["An answer to the first wording"]}}},
                "u2e":{"parent":"a1","children":["a2a","a2b"],
                       "message":{"author":{"role":"user"},"create_time":1720992800.0,
                                  "content":{"parts":["Lisbon in May"]}}},
                "a2a":{"parent":"u2e","children":[],
                       "message":{"author":{"role":"assistant"},"create_time":1720992801.0,
                                  "content":{"parts":["A regenerated-away answer"]}}},
                "a2b":{"parent":"u2e","children":[],
                       "message":{"author":{"role":"assistant"},"create_time":1720992802.0,
                                  "content":{"parts":["Lisbon it is"]}}}
              }
            }]"#,
        );
        let drafts = ingest(tmp.path(), None, None).unwrap();
        let body = &drafts[0].body;
        let order: Vec<usize> = [
            "you: Plan a trip",
            "chatgpt: Where to?",
            "you: Lisbon in May",
            "chatgpt: Lisbon it is",
        ]
        .iter()
        .map(|turn| {
            body.find(turn)
                .unwrap_or_else(|| panic!("{turn} in {body}"))
        })
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{body}");
        for left_out in ["first wording", "regenerated-away"] {
            assert!(!body.contains(left_out), "{body}");
        }
    }

    /// An export is recognised by its chunk files alone, or by a single
    /// `conversations.json` with its `chat.html`.
    #[test]
    fn test_export_dirs_are_recognised() {
        let tmp = tempfile::tempdir().unwrap();
        let chunked = tmp.path().join("chunked");
        std::fs::create_dir(&chunked).unwrap();
        write(&chunked, "conversations-000.json", "[]");
        write(&chunked, "conversations-001.json", "[]");
        assert!(is_export_dir(&chunked));
        let single = tmp.path().join("single");
        std::fs::create_dir(&single).unwrap();
        write(&single, "conversations.json", "[]");
        assert!(!is_export_dir(&single), "a Claude export has the same name");
        write(&single, "chat.html", "");
        assert!(is_export_dir(&single));
        assert!(!is_export_dir(&tmp.path().join("missing")));
    }
}

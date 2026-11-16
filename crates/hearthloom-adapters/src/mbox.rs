//! Mbox adapter (Gmail Takeout et al.): one email = one episode; sender
//! and recipients become Person entities keyed by email address, so mail
//! participates in the same cross-source identity as git and calendars.
//!
//! The message is read with `mail-parser` (the same reader as the daemon's
//! archive text index): the subject and display names decoded (RFC 2047),
//! the body's first text part decoded from its transfer encoding and
//! charset, wherever it sits in the MIME tree. The episode body is that
//! readable text (capped) — for mail with only an HTML part, followed by
//! its links' and images' targets; the original bytes stay in the archive. The
//! message's identity (Message-ID, or the hash of its text) and its Gmail
//! labels are read from the raw header block, as before.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EdgeAssertion, EntityRef, EpisodeDraft, EventTime, TimeUncertainty};

/// Body cap per episode — enough for any real email text, small enough
/// that a 10GB archive doesn't balloon the store.
const BODY_CAP: usize = 64 * 1024;
/// Recipients beyond this are listed in the body but not minted as
/// entities (mailing-list blasts shouldn't flood the graph).
const MAX_RECIPIENT_ENTITIES: usize = 10;

/// Labels that are never worth an episode, whatever the filter says.
const ALWAYS_EXCLUDE_LABELS: &[&str] = &["spam", "trash"];

/// Which messages of a Gmail-Takeout mbox become episodes, decided on
/// the `X-Gmail-Labels` header (label match is case-insensitive, on
/// whole labels — `"Sent"`, `"Category Personal"`). `Spam`/`Trash` are
/// always dropped. An empty filter keeps everything else.
#[derive(Debug, Clone, Default)]
pub struct LabelFilter {
    /// Keep only messages carrying at least one of these labels.
    /// Empty = no include restriction. A message with no
    /// `X-Gmail-Labels` header at all fails a non-empty include filter.
    pub include_any: Vec<String>,
    /// Drop messages carrying any of these labels (on top of the
    /// built-in Spam/Trash exclusion).
    pub exclude_any: Vec<String>,
}

impl LabelFilter {
    /// The Core strata the 2026-07 Takeout materialized (Tier 1): the
    /// mail worth episodes. The live Gmail pull uses the SAME filter so
    /// live and archived materialization stay one policy.
    pub fn core_strata() -> Self {
        Self {
            include_any: vec!["Sent".into(), "Category Personal".into(), "Starred".into()],
            exclude_any: vec![],
        }
    }

    /// Whether a message with these labels passes the filter.
    pub fn keeps(&self, labels: &[String]) -> bool {
        let has = |list: &[String]| {
            list.iter()
                .any(|want| labels.iter().any(|l| l.eq_ignore_ascii_case(want)))
        };
        if labels
            .iter()
            .any(|l| ALWAYS_EXCLUDE_LABELS.contains(&l.to_ascii_lowercase().as_str()))
        {
            return false;
        }
        if has(&self.exclude_any) {
            return false;
        }
        self.include_any.is_empty() || has(&self.include_any)
    }
}

/// Pull the `X-Gmail-Labels` values out of a raw message without a full
/// header parse — the filter rejects most of a Takeout archive, so this
/// runs 600k+ times and must stay cheap. Handles RFC 5322 folding.
pub fn gmail_labels(message: &str) -> Vec<String> {
    let mut value: Option<String> = None;
    for line in message.lines() {
        if line.trim().is_empty() {
            break; // end of headers
        }
        if let Some(v) = value.as_mut() {
            if line.starts_with(' ') || line.starts_with('\t') {
                v.push(' ');
                v.push_str(line.trim());
                continue;
            }
            break; // header ended, un-folded
        }
        if let Some((name, v)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("X-Gmail-Labels") {
                value = Some(v.trim().to_string());
            }
        }
    }
    value
        .map(|v| {
            v.split(',')
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub fn ingest(path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let mut drafts = Vec::new();
    ingest_batched(path, scope, usize::MAX, |batch| {
        drafts.extend(batch);
        Ok(())
    })?;
    if drafts.is_empty() {
        return Err(anyhow!("no messages found in {}", path.display()));
    }
    Ok(drafts)
}

/// Streaming parse with the default (keep-everything-but-Spam/Trash)
/// filter. See [`ingest_batched_filtered`].
pub fn ingest_batched(
    path: &Path,
    scope: Option<&str>,
    batch_size: usize,
    sink: impl FnMut(Vec<EpisodeDraft>) -> Result<()>,
) -> Result<usize> {
    ingest_batched_filtered(path, scope, &LabelFilter::default(), batch_size, sink)
}

/// Streaming parse: messages are read line by line and delivered to
/// `sink` in batches of `batch_size`, so a multi-GB Takeout archive
/// never has to fit in memory. Messages failing `filter` are skipped
/// before draft construction. Returns the number of drafts produced.
pub fn ingest_batched_filtered(
    path: &Path,
    scope: Option<&str>,
    filter: &LabelFilter,
    batch_size: usize,
    sink: impl FnMut(Vec<EpisodeDraft>) -> Result<()>,
) -> Result<usize> {
    ingest_batched_checked(path, scope, filter, &|_| false, batch_size, sink)
}

/// [`ingest_batched_filtered`], with `private` asked of each kept message's
/// bytes: one it answers true for is drafted private. The stored text is
/// cut ([`BODY_CAP`], the link targets' room), so a check of the draft
/// alone misses what was cut; the caller checks the whole message.
pub fn ingest_batched_checked(
    path: &Path,
    scope: Option<&str>,
    filter: &LabelFilter,
    private: &dyn Fn(&[u8]) -> bool,
    batch_size: usize,
    mut sink: impl FnMut(Vec<EpisodeDraft>) -> Result<()>,
) -> Result<usize> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // 1MB buffer: mbox lines are short but plentiful.
    let reader = std::io::BufReader::with_capacity(1 << 20, file);

    // The message's bytes as they are (old mail is full of 8-bit text in
    // legacy charsets — decoded by the charset it declares), lines ending
    // in LF.
    let mut current: Vec<u8> = Vec::new();
    let mut in_message = false;
    let mut prev_blank = true;
    let mut batch: Vec<EpisodeDraft> = Vec::new();
    let mut total = 0usize;

    let flush_message =
        |current: &mut Vec<u8>, batch: &mut Vec<EpisodeDraft>, total: &mut usize| {
            if current.is_empty() {
                return;
            }
            match draft_from_bytes_filtered(current, filter, scope) {
                Ok(Some(mut draft)) => {
                    if private(current) {
                        draft.scope = Some("private".into());
                    }
                    batch.push(draft);
                    *total += 1;
                }
                Ok(None) => {}
                Err(err) => tracing::warn!("skipping message: {err}"),
            }
            current.clear();
        };

    for line in reader.split(b'\n') {
        let mut line = line?;
        while line.last() == Some(&b'\r') {
            line.pop();
        }
        if prev_blank && line.starts_with(b"From ") {
            flush_message(&mut current, &mut batch, &mut total);
            if batch.len() >= batch_size {
                sink(std::mem::take(&mut batch))?;
            }
            in_message = true;
            prev_blank = false;
            continue;
        }
        if in_message {
            current.extend_from_slice(&line);
            current.push(b'\n');
        }
        prev_blank = String::from_utf8_lossy(&line).trim().is_empty();
    }
    flush_message(&mut current, &mut batch, &mut total);
    if !batch.is_empty() {
        sink(batch)?;
    }
    Ok(total)
}

/// One already-separated message (no mbox `From ` line) → a draft, if
/// its `X-Gmail-Labels` pass `filter`. The live Gmail pull materializes
/// through this so a live message and its archived twin parse (and
/// dedupe, via `source="mbox"` + Message-ID) identically.
pub fn draft_from_message_filtered(
    message: &str,
    filter: &LabelFilter,
    scope: Option<&str>,
) -> Result<Option<EpisodeDraft>> {
    draft_from_bytes_filtered(message.as_bytes(), filter, scope)
}

/// [`draft_from_message_filtered`] from the message's bytes, so 8-bit text
/// is decoded by the charset the message declares rather than read as
/// UTF-8 first.
pub fn draft_from_bytes_filtered(
    message: &[u8],
    filter: &LabelFilter,
    scope: Option<&str>,
) -> Result<Option<EpisodeDraft>> {
    let labels = gmail_labels(&String::from_utf8_lossy(message));
    if !filter.keeps(&labels) {
        return Ok(None);
    }
    message_to_draft(message, &labels, scope).map(Some)
}

/// One message's draft as the import reads it now, with the labels given
/// rather than its own header's — a repair keeps the labels its memory was
/// made under (`hearthloom repair mail`).
pub fn draft_with_labels(
    message: &[u8],
    labels: &[String],
    scope: Option<&str>,
) -> Result<EpisodeDraft> {
    message_to_draft(message, labels, scope)
}

/// Split on mbox `From ` separator lines (start-of-line, after a blank
/// line or at file start). Test-only reference implementation; the
/// production path is the streaming `ingest_batched`.
#[cfg(test)]
fn split_mbox(raw: &str) -> Vec<&str> {
    let mut messages = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0;
    let mut prev_blank = true;
    for line in raw.split_inclusive('\n') {
        if prev_blank && line.starts_with("From ") {
            if let Some(s) = start {
                messages.push(&raw[s..offset]);
            }
            start = Some(offset + line.len());
        }
        prev_blank = line.trim_end().is_empty();
        offset += line.len();
    }
    if let Some(s) = start {
        messages.push(&raw[s..]);
    }
    messages
}

struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse the header block (with RFC 5322 unfolding).
fn parse_headers(message: &str) -> Headers {
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in message.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if (line.starts_with(' ') || line.starts_with('\t')) && !entries.is_empty() {
            let last = entries.last_mut().unwrap();
            last.1.push(' ');
            last.1.push_str(trimmed.trim_start());
        } else if let Some((name, value)) = trimmed.split_once(':') {
            entries.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Headers { entries }
}

/// (email, display name) pairs of an address header, decoded. An address
/// without `@` or with whitespace is skipped; a missing name is the
/// mailbox's local part. Names are one line: the body lists them one per
/// line.
fn people(list: Option<mail_parser::AddressList<'_>>) -> Vec<(String, String)> {
    let Some(list) = list else {
        return vec![];
    };
    list.mailboxes()
        .filter_map(|mailbox| {
            let email = mailbox.address()?.trim().to_lowercase();
            if !email.contains('@') || email.chars().any(char::is_whitespace) {
                return None;
            }
            let name = mailbox
                .name()
                .map(hearthloom_core::one_line)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| email.split('@').next().unwrap_or(&email).to_string());
            Some((email, name))
        })
        .collect()
}

/// The targets of an HTML part's links and images (the values of its
/// `href` and `src` attributes), in order, once each and one line each,
/// at most [`BODY_CAP`] bytes together: a target too long for the room left
/// is left out, and the ones after it still go in. A target left out keeps
/// its host in its place (`target_host`, once each, in a room of their own
/// as large), so a domain rule made after the import still finds the
/// stored memory. Inline data (`data:`) is left out. One pass over the
/// text, however many links it repeats. The rest of a target left out is
/// not stored, so the importing caller checks the privacy rules over the
/// whole message (`ingest_batched_checked`). The flag: a host didn't fit
/// its room either — a rule made later can't find that one, so the caller
/// drafts the memory private.
fn link_targets(html: &str) -> (Vec<String>, bool) {
    // ASCII lowercasing keeps every byte where it was, so positions found
    // in `lower` slice `html`
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let blank = |i: usize| bytes.get(i).is_some_and(|b| b.is_ascii_whitespace());
    let mut out: Vec<String> = Vec::new();
    let mut kept: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut total = 0;
    let mut hosts_total = 0;
    let mut host_left_out = false;
    let mut at = 0;
    while at < bytes.len() {
        // the next `href` or `src`: the search only ever moves forward
        let Some((start, len)) = (at..bytes.len()).find_map(|i| {
            if bytes[i..].starts_with(b"href") {
                Some((i, 4))
            } else if bytes[i..].starts_with(b"src") {
                Some((i, 3))
            } else {
                None
            }
        }) else {
            break;
        };
        at = start + len;
        // a whole attribute name, then `=`
        if start > 0
            && !matches!(
                bytes[start - 1],
                b'"' | b'\'' | b'/' | b' ' | b'\t' | b'\r' | b'\n'
            )
        {
            continue;
        }
        let mut i = at;
        while blank(i) {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while blank(i) {
            i += 1;
        }
        let (from, to) = match bytes.get(i) {
            Some(&quote @ (b'"' | b'\'')) => {
                let from = i + 1;
                let to = lower[from..]
                    .find(quote as char)
                    .map_or(bytes.len(), |e| from + e);
                (from, to)
            }
            Some(_) => {
                let to = lower[i..]
                    .find(|c: char| c.is_ascii_whitespace() || c == '>')
                    .map_or(bytes.len(), |e| i + e);
                (i, to)
            }
            None => break,
        };
        at = to;
        let target = hearthloom_core::one_line(&html[from..to]);
        if target.is_empty()
            || target
                .get(..5)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
            || kept.contains(&target)
        {
            continue;
        }
        if total + target.len() + 1 > BODY_CAP {
            let host = target_host(&target);
            // each host with its line break, in a room as large
            if !host.is_empty() && !kept.contains(host) {
                if hosts_total + host.len() < BODY_CAP {
                    hosts_total += host.len() + 1;
                    kept.insert(host.to_string());
                    out.push(host.to_string());
                } else {
                    host_left_out = true;
                }
            }
            continue;
        }
        total += target.len() + 1;
        kept.insert(target.clone());
        out.push(target);
    }
    (out, host_left_out)
}

/// A link target up to the end of its host: `https://portal.example/r?d=1`
/// → `https://portal.example`, `mailto:desk@clinic.example?subject=x` →
/// `mailto:desk@clinic.example`.
fn target_host(target: &str) -> &str {
    let start = if target.starts_with("//") {
        2
    } else {
        match target.find("://") {
            Some(i) if !target[..i].contains(['/', '?', '#']) => i + 3,
            _ => 0,
        }
    };
    let end = target[start..]
        .find(['/', '?', '#'])
        .map_or(target.len(), |e| start + e);
    &target[..end]
}

/// The `source_ref` prefix of a message without a Message-ID: its memory
/// is keyed by a hash of the exact text the import read, which depends on
/// how that copy was framed (the archive's line endings, the labels the
/// live pull wrote) — so no later read of the message can rebuild it.
pub const UNKEYED_REF_PREFIX: &str = "nomsgid:";

/// The `source_ref` an import gives a message that has a Message-ID: the
/// first `Message-ID` header, unfolded and trimmed, without the `<` `>`
/// at its ends. None: the message has none (its memory is keyed under
/// [`UNKEYED_REF_PREFIX`]). The one rule for this — every reader that
/// looks for a message's memory uses it, so they agree with the import.
pub fn message_id_ref(message: &str) -> Option<String> {
    id_ref(&parse_headers(message))
}

fn id_ref(headers: &Headers) -> Option<String> {
    headers
        .get("Message-ID")
        .map(|id| id.trim_matches(|c| c == '<' || c == '>').to_string())
}

fn message_to_draft(raw: &[u8], labels: &[String], scope: Option<&str>) -> Result<EpisodeDraft> {
    // The text the identity is read from: the header block, and for a
    // message without a Message-ID, the hash of this text (the same text
    // earlier versions hashed, so a re-import finds the same memory).
    let message = String::from_utf8_lossy(raw);
    let headers = parse_headers(&message);
    // Everything shown is decoded: encoded words, transfer encodings,
    // charsets, nested parts.
    let parsed = mail_parser::MessageParser::default().parse(raw);
    let subject = parsed
        .as_ref()
        .and_then(|m| m.subject())
        .or_else(|| headers.get("Subject"))
        .map(hearthloom_core::one_line)
        .unwrap_or_else(|| "(no subject)".to_string());
    let date = headers
        .get("Date")
        .ok_or_else(|| anyhow!("message without Date: {subject}"))?;
    let event_time = match DateTime::parse_from_rfc2822(date) {
        Ok(dt) => EventTime::from_fixed(dt, TimeUncertainty::Exact),
        Err(_) => EventTime::unknown(),
    };

    // The first of each address header, as the archive's index reads them
    // (mail-parser's `from()` and the like answer the last).
    let (from, recipients) = match parsed.as_ref() {
        Some(m) => {
            let fields = m.headers();
            let mut recipients = people(fields.all_to().next());
            recipients.extend(people(fields.all_cc().next()));
            (people(fields.all_from().next()), recipients)
        }
        None => (vec![], vec![]),
    };

    // Source-native identity: Message-ID, else a content hash stand-in.
    let source_ref = id_ref(&headers).unwrap_or_else(|| {
        format!(
            "{UNKEYED_REF_PREFIX}{}",
            hearthloom_core::ids::content_hash(message.as_bytes())
        )
    });

    let mut text = parsed
        .as_ref()
        .and_then(|m| m.body_text(0))
        .map(|t| t.replace("\r\n", "\n"))
        .unwrap_or_default();
    if text.len() > BODY_CAP {
        text.truncate(text.floor_char_boundary(BODY_CAP));
        text.push_str("\n[truncated]");
    }
    // HTML turned into text keeps only what a reader sees: the targets of
    // its links and images go after it, so the stored memory names the
    // domains its message links to (the importing caller checks the
    // privacy rules over the whole message, past any cut). A message
    // linking to more hosts than their room holds is drafted private: a
    // domain rule made later couldn't find the ones left out.
    let (links, host_left_out) = parsed
        .as_ref()
        .and_then(|m| m.text_body().next())
        .filter(|part| matches!(part.kind(), mail_parser::PartKind::Html))
        .and_then(|part| part.text())
        .map(|html| link_targets(&html))
        .unwrap_or_default();
    let scope = if host_left_out {
        Some("private")
    } else {
        scope
    };
    let mut body = format!("subject: {subject}\n");
    if let Some((email, name)) = from.first() {
        body.push_str(&format!("from: {name} <{email}>\n"));
    }
    for (email, name) in &recipients {
        body.push_str(&format!("to: {name} <{email}>\n"));
    }
    if !labels.is_empty() {
        // Provenance: which Gmail strata this message sat in (lets a
        // later Core→Extended widening reason about what's present).
        body.push_str(&format!("labels: {}\n", labels.join(", ")));
    }
    body.push('\n');
    body.push_str(text.trim());
    if !links.is_empty() {
        body.push_str("\n\nlinks:\n");
        body.push_str(&links.join("\n"));
    }

    let mut entities = Vec::new();
    let mut edges = Vec::new();
    if let Some((email, name)) = from.first() {
        entities.push(EntityRef {
            entity_type: "Person".into(),
            name: name.clone(),
            attributes: vec![("email".into(), email.clone())],
            role: "author".into(),
        });
    }
    for (email, name) in recipients.iter().take(MAX_RECIPIENT_ENTITIES) {
        entities.push(EntityRef {
            entity_type: "Person".into(),
            name: name.clone(),
            attributes: vec![("email".into(), email.clone())],
            role: "recipient".into(),
        });
        if !from.is_empty() {
            edges.push(EdgeAssertion {
                src: 0,
                relation: "corresponded_with".into(),
                dst: entities.len() - 1,
            });
        }
    }

    Ok(EpisodeDraft {
        source: "mbox".into(),
        source_ref,
        kind: "email".into(),
        title: subject,
        body,
        event_time,
        trust: "imported".into(),
        scope: scope.map(String::from),
        entities,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "From jon@example.com Mon Mar 16 10:00:00 2026\n\
From: Jon Hale <jon@harbor.example>\n\
To: Jane Doe <jane@example.com>, sam@example.com\n\
Subject: Re: roadmap\n\
Date: Mon, 16 Mar 2026 09:30:00 -0400\n\
Message-ID: <abc123@mail.example.com>\n\
\n\
Sounds good, let's ship it.\n\
\n\
From jane@example.com Mon Mar 16 11:00:00 2026\n\
From: =?UTF-8?Q?Jane?= <jane@example.com>\n\
Subject: multipart test\n\
Date: Mon, 16 Mar 2026 10:30:00 -0400\n\
Message-ID: <def456@mail.example.com>\n\
Content-Type: multipart/alternative; boundary=\"XYZ\"\n\
\n\
--XYZ\n\
Content-Type: text/plain; charset=UTF-8\n\
\n\
plain part here\n\
--XYZ\n\
Content-Type: text/html\n\
\n\
<b>html part</b>\n\
--XYZ--\n";

    #[test]
    fn test_parse_mbox() {
        let messages = split_mbox(SAMPLE);
        assert_eq!(messages.len(), 2);

        let draft = message_to_draft(messages[0].as_bytes(), &[], None).unwrap();
        assert_eq!(draft.title, "Re: roadmap");
        assert_eq!(draft.source_ref, "abc123@mail.example.com");
        assert_eq!(draft.kind, "email");
        // 09:30 -0400 = 13:30 UTC.
        assert_eq!(
            draft.event_time.utc.as_deref(),
            Some("2026-03-16T13:30:00Z")
        );
        assert_eq!(draft.event_time.tz_offset_minutes, Some(-240));
        // Author + 2 recipients, corresponded_with edges.
        assert_eq!(draft.entities.len(), 3);
        assert_eq!(draft.entities[0].role, "author");
        assert_eq!(draft.entities[0].attributes[0].1, "jon@harbor.example");
        assert_eq!(draft.entities[2].name, "sam");
        assert_eq!(draft.edges.len(), 2);
        assert!(draft.body.contains("Sounds good"));
    }

    /// `message_id_ref` is the import's own key for a message, however its
    /// Message-ID header is written; a message without one has none.
    #[test]
    fn test_message_id_ref_is_the_imports_key() {
        for header in [
            "Message-ID: <plain@x>",
            "Message-ID: (comment) <hush1@x>",
            "message-id:\n  <folded@x>",
            "Message-ID: <a@x> (trailing)",
        ] {
            let message = format!(
                "From: Ann <ann@x.example>\r\nSubject: s\r\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\r\n{}\r\n\r\nbody\r\n",
                header.replace('\n', "\r\n")
            );
            let draft = message_to_draft(message.as_bytes(), &[], None).unwrap();
            assert_eq!(
                message_id_ref(&message).as_deref(),
                Some(draft.source_ref.as_str()),
                "{header}"
            );
        }
        let none = "From: a@x\nDate: Mon, 05 Jan 2026 10:00:00 -0400\n\nbody\n";
        assert_eq!(message_id_ref(none), None);
        assert!(message_to_draft(none.as_bytes(), &[], None)
            .unwrap()
            .source_ref
            .starts_with(UNKEYED_REF_PREFIX));
    }

    /// A message without a Message-ID keeps the key earlier versions gave
    /// it (the hash of its text, invalid UTF-8 replaced), so importing it
    /// again finds the same memory.
    #[test]
    fn test_an_unkeyed_message_keeps_its_key() {
        let raw = b"From: a@x\nDate: Mon, 05 Jan 2026 10:00:00 -0400\n\ncaf\xe9\n";
        let text = String::from_utf8_lossy(raw);
        let draft = message_to_draft(raw, &[], None).unwrap();
        assert_eq!(
            draft.source_ref,
            format!(
                "{UNKEYED_REF_PREFIX}{}",
                hearthloom_core::ids::content_hash(text.as_bytes())
            )
        );
    }

    #[test]
    fn test_multipart_prefers_text_plain() {
        let messages = split_mbox(SAMPLE);
        let draft = message_to_draft(messages[1].as_bytes(), &[], None).unwrap();
        assert!(draft.body.contains("plain part here"));
        assert!(!draft.body.contains("<b>html part</b>"));
        // An encoded-word display name is decoded.
        assert_eq!(draft.entities[0].name, "Jane");
    }

    /// Mail with only an HTML part keeps the targets of its links and
    /// images beside the text it shows: a privacy rule naming a link's
    /// domain reads the memory as it reads the message.
    #[test]
    fn test_html_only_mail_keeps_its_link_targets() {
        let message = "From: Ann <ann@x.example>\nSubject: your visit\n\
             Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <html1@x>\n\
             Content-Type: text/html; charset=utf-8\n\n\
             <p>Your results are ready: <a class=\"btn\" HREF = 'https://portal.clinic-one.example/r?id=7'>view them</a>\
             <img src=https://img.clinic-one.example/logo.png alt=logo>\
             <img src=\"data:image/png;base64,AAAA\"><a data-href=\"x\" href=\"mailto:desk@clinic-one.example\">mail us</a></p>\n";
        let draft = message_to_draft(message.as_bytes(), &[], None).unwrap();
        assert!(draft.body.contains("view them"), "{}", draft.body);
        assert!(
            draft.body.ends_with(
                "\n\nlinks:\nhttps://portal.clinic-one.example/r?id=7\n\
                 https://img.clinic-one.example/logo.png\nmailto:desk@clinic-one.example"
            ),
            "{}",
            draft.body
        );
        // a message with a text part stores that part alone, as before
        let messages = split_mbox(SAMPLE);
        let draft = message_to_draft(messages[1].as_bytes(), &[], None).unwrap();
        assert!(!draft.body.contains("links:"), "{}", draft.body);
    }

    /// An image address longer than the room for targets is left out but
    /// for its host, and the link after it is kept: a domain rule naming
    /// that link's domain makes the memory private when it is stored.
    #[test]
    fn test_a_target_too_long_to_keep_leaves_the_rest_for_the_rules() {
        use hearthloom_core::store::{Actor, Store};
        let pixel = format!("https://track.example/p?d={}", "a".repeat(BODY_CAP + 10));
        let message = format!(
            "From: Ann <ann@x.example>\nSubject: your visit\n\
             Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <long1@x>\n\
             Content-Type: text/html; charset=utf-8\n\n\
             <p><img src=\"{pixel}\">Your results are ready: \
             <a href=\"https://portal.clinic-one.example/r?id=7\">view them</a></p>\n"
        );
        let draft = message_to_draft(message.as_bytes(), &[], None).unwrap();
        assert!(
            draft.body.ends_with(
                "\n\nlinks:\nhttps://track.example\nhttps://portal.clinic-one.example/r?id=7"
            ),
            "{}",
            &draft.body[draft.body.len().saturating_sub(200)..]
        );

        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .add_privacy_rule(&Actor::new("human:test"), "domain", "clinic-one.example")
            .unwrap();
        store
            .apply_batch(
                &home,
                &hearthloom_core::IngestBatch {
                    actor: "adapter:mbox".into(),
                    session_id: None,
                    episodes: vec![draft],
                },
            )
            .unwrap();
        store.vault_unlock(15);
        let stored = store
            .episode(&hearthloom_core::ids::episode_id("mbox", "long1@x"))
            .unwrap()
            .unwrap();
        assert_eq!(stored.scope, "private");
    }

    /// A link the room leaves out keeps its host in the stored text, so a
    /// domain rule made after the import finds the memory — whether an image
    /// address took the whole room before the link, or the link alone is
    /// longer than the room.
    #[test]
    fn test_a_rule_made_after_the_import_finds_the_host_of_a_link_left_out() {
        use hearthloom_core::store::{Actor, Store};
        let prefix = "https://track.example/p?d=";
        let filler = format!("{prefix}{}", "a".repeat(BODY_CAP - 1 - prefix.len()));
        let message = |id: &str, html: &str| {
            format!(
                "From: Ann <ann@x.example>\nSubject: your visit\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <{id}>\n\
                 Content-Type: text/html; charset=utf-8\n\n{html}\n"
            )
        };
        let messages = [
            message(
                "filled@x",
                &format!(
                    "<p><img src=\"{filler}\">Your results are ready: \
                     <a href=\"https://portal.clinic-one.example/r\">view them</a></p>"
                ),
            ),
            message(
                "long@x",
                &format!(
                    "<p>Your results are ready: \
                     <a href=\"https://portal.clinic-one.example/r?d={}\">view them</a></p>",
                    "a".repeat(BODY_CAP + 10)
                ),
            ),
        ];
        let drafts: Vec<EpisodeDraft> = messages
            .iter()
            .map(|m| message_to_draft(m.as_bytes(), &[], None).unwrap())
            .collect();
        for draft in &drafts {
            let tail = &draft.body[draft.body.len().saturating_sub(200)..];
            assert!(
                draft.body.ends_with("\nhttps://portal.clinic-one.example"),
                "{tail}"
            );
            assert!(!draft.body.contains("clinic-one.example/r"), "{tail}");
        }

        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .apply_batch(
                &home,
                &hearthloom_core::IngestBatch {
                    actor: "adapter:mbox".into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap();
        let (_, relabeled) = store
            .add_privacy_rule(&Actor::new("human:test"), "domain", "clinic-one.example")
            .unwrap();
        assert_eq!(relabeled, 2);
        assert_eq!(
            store
                .private_source_refs("mbox", &["filled@x".into(), "long@x".into()])
                .unwrap()
                .len(),
            2
        );
    }

    /// A message linking to more hosts than the hosts' room holds goes into
    /// the vault as it is stored: a domain rule made later couldn't find
    /// the hosts left out.
    #[test]
    fn test_a_message_with_more_hosts_than_their_room_is_stored_private() {
        use hearthloom_core::store::Store;
        let prefix = "https://track.example/p?d=";
        let filler = format!("{prefix}{}", "a".repeat(BODY_CAP - 1 - prefix.len()));
        let mut html = format!("<p><img src=\"{filler}\">");
        for i in 0..3_000 {
            html.push_str(&format!("<a href=\"https://h{i:04}.example/x\">h</a>"));
        }
        html.push_str("<a href=\"https://portal.clinic-one.example/r\">view them</a></p>");
        let message = format!(
            "From: Ann <ann@x.example>\nSubject: your visit\n\
             Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <hosts@x>\n\
             Content-Type: text/html; charset=utf-8\n\n{html}\n"
        );
        let draft = message_to_draft(message.as_bytes(), &[], None).unwrap();
        assert!(!draft.body.contains("clinic-one.example"));
        assert_eq!(draft.scope.as_deref(), Some("private"));

        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .apply_batch(
                &home,
                &hearthloom_core::IngestBatch {
                    actor: "adapter:mbox".into(),
                    session_id: None,
                    episodes: vec![draft],
                },
            )
            .unwrap();
        assert_eq!(
            store
                .private_source_refs("mbox", &["hosts@x".into()])
                .unwrap()
                .len(),
            1
        );
    }

    /// A large body repeating links is read in one pass: the targets are
    /// the distinct ones, found in time proportional to its length.
    #[test]
    fn test_repeated_links_are_read_in_one_pass() {
        let mut html = String::new();
        for i in 0..5_000 {
            html.push_str(&format!("<a href=d{i}>d</a>"));
        }
        html.push_str(&"<a href=d4999>d</a>".repeat(60_000));
        let started = std::time::Instant::now();
        let (targets, _) = link_targets(&html);
        let took = started.elapsed();
        assert_eq!(targets.len(), 5_000);
        assert_eq!(targets.last().map(String::as_str), Some("d4999"));
        assert!(
            took < std::time::Duration::from_secs(3),
            "{} bytes took {took:?}",
            html.len()
        );
    }

    /// With From, To or Cc written twice, the memory names the first of
    /// each, as the archive's index does.
    #[test]
    fn test_a_repeated_address_header_is_read_first_one_first() {
        let message = "From: Ann <ann@x.example>\nTo: Bob <bob@x.example>\n\
             Cc: Cy <cy@x.example>\nFrom: Dee <dee@y.example>\nTo: Eve <eve@y.example>\n\
             Cc: Fay <fay@y.example>\nSubject: s\n\
             Date: Mon, 05 Jan 2026 10:00:00 -0400\nMessage-ID: <dup1@x>\n\nbody\n";
        let draft = message_to_draft(message.as_bytes(), &[], None).unwrap();
        let people: Vec<(&str, &str)> = draft
            .entities
            .iter()
            .map(|e| (e.role.as_str(), e.attributes[0].1.as_str()))
            .collect();
        assert_eq!(
            people,
            vec![
                ("author", "ann@x.example"),
                ("recipient", "bob@x.example"),
                ("recipient", "cy@x.example"),
            ]
        );
        assert!(draft.body.contains("from: Ann <ann@x.example>\n"));
    }

    /// What is stored is readable text: encoded subjects and names decoded,
    /// base64 and quoted-printable bodies decoded, the text part found
    /// inside nested multiparts, attachments left out, 8-bit text read by
    /// its declared charset.
    #[test]
    fn test_stored_mail_is_decoded() {
        let header = |extra: &str| {
            format!(
                "From: =?UTF-8?B?Sm9zw6kgR2FyY8OtYQ==?= <jose@x.example>\n\
                 To: \"Doe, Jane\" <Jane@X.example>, bob@y.example, =?ISO-8859-1?Q?Ren=E9?= <rene@z.example>\n\
                 Subject: =?UTF-8?Q?Caf=C3=A9_plans?=\n\
                 Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
                 Message-ID: <dec1@x>\n{extra}"
            )
        };
        let base64 = format!(
            "{}Content-Type: text/plain; charset=utf-8\nContent-Transfer-Encoding: base64\n\n\
             TWVldCBhdCB0aGUgY2Fmw6kgb24gVHVlc2RheS4K\n",
            header("")
        );
        let draft = message_to_draft(base64.as_bytes(), &[], None).unwrap();
        assert_eq!(draft.title, "Café plans");
        assert!(draft.body.starts_with("subject: Café plans\n"));
        assert!(
            draft.body.contains("Meet at the café on Tuesday."),
            "{}",
            draft.body
        );
        assert_eq!(draft.entities[0].name, "José García");
        let people: Vec<(&str, &str)> = draft
            .entities
            .iter()
            .map(|e| (e.attributes[0].1.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(
            people,
            vec![
                ("jose@x.example", "José García"),
                ("jane@x.example", "Doe, Jane"),
                ("bob@y.example", "bob"),
                ("rene@z.example", "René"),
            ]
        );

        let quoted_printable = format!(
            "{}Content-Type: text/plain; charset=utf-8\nContent-Transfer-Encoding: quoted-printable\n\n\
             I=E2=80=99ll bring the =\nreceipts.\n",
            header("")
        );
        let draft = message_to_draft(quoted_printable.as_bytes(), &[], None).unwrap();
        assert!(
            draft.body.contains("I\u{2019}ll bring the receipts."),
            "{}",
            draft.body
        );

        let nested = format!(
            "{}Content-Type: multipart/mixed; boundary=\"outer\"\n\n\
             --outer\n\
             Content-Type: multipart/alternative; boundary=\"inner\"\n\n\
             --inner\n\
             Content-Type: text/plain; charset=utf-8\n\
             Content-Transfer-Encoding: base64\n\n\
             VGhlIGJpb3BzeSByZXN1bHRzIGFyZSBpbi4K\n\
             --inner\n\
             Content-Type: text/html; charset=utf-8\n\n\
             <p>The biopsy results are in.</p>\n\
             --inner--\n\
             --outer\n\
             Content-Type: application/pdf; name=\"scan.pdf\"\n\
             Content-Transfer-Encoding: base64\n\n\
             JVBERi0xLjQKJcOkw7zDtsOfCjIgMCBvYmoK\n\
             --outer--\n",
            header("")
        );
        let draft = message_to_draft(nested.as_bytes(), &[], None).unwrap();
        assert!(
            draft.body.contains("The biopsy results are in."),
            "{}",
            draft.body
        );
        assert!(!draft.body.contains("JVBERi0"), "{}", draft.body);
        assert!(!draft.body.contains("Content-Type"), "{}", draft.body);

        let eight_bit = [
            header("Content-Type: text/plain; charset=windows-1252\nContent-Transfer-Encoding: 8bit\n\n")
                .into_bytes(),
            b"Cr\xe8me br\xfbl\xe9e at eight.\n".to_vec(),
        ]
        .concat();
        let draft = message_to_draft(&eight_bit, &[], None).unwrap();
        assert!(
            draft.body.contains("Crème brûlée at eight."),
            "{}",
            draft.body
        );
    }

    /// A decoded subject or name can't add a line to the body's header
    /// lines (`labels:` among them).
    #[test]
    fn test_decoded_header_text_stays_on_one_line() {
        let raw = "From: =?UTF-8?Q?Ann=0Alabels:_Starred?= <ann@x.example>\n\
                   Subject: =?UTF-8?Q?hello=0Alabels:_Starred?=\n\
                   Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
                   Message-ID: <line1@x>\n\nbody\n";
        let draft = message_to_draft(raw.as_bytes(), &[], None).unwrap();
        assert_eq!(draft.title, "hello labels: Starred");
        assert_eq!(draft.entities[0].name, "Ann labels: Starred");
        assert!(
            !draft.body.lines().any(|l| l.starts_with("labels:")),
            "{}",
            draft.body
        );
    }

    #[test]
    fn test_streaming_matches_split_and_batches() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mail.mbox");
        std::fs::write(&path, SAMPLE).unwrap();

        // Whole-file API sees both messages.
        let drafts = ingest(&path, None).unwrap();
        assert_eq!(drafts.len(), 2);

        // Streaming with batch_size=1 delivers two single-item batches
        // with identical content.
        let mut batches = Vec::new();
        let total = ingest_batched(&path, None, 1, |batch| {
            batches.push(batch);
            Ok(())
        })
        .unwrap();
        assert_eq!(total, 2);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0][0].source_ref, drafts[0].source_ref);
        assert_eq!(batches[1][0].title, drafts[1].title);
    }

    const LABELED: &str = "From a@x Mon Jan 05 10:00:00 2026\n\
From: Ann <ann@x.example>\n\
Subject: hello\n\
Date: Mon, 05 Jan 2026 10:00:00 -0400\n\
Message-ID: <keep1@x>\n\
X-Gmail-Labels: Archived,Category Personal,Opened\n\
\n\
personal note\n\
\n\
From b@x Mon Jan 05 11:00:00 2026\n\
From: Shop <noreply@shop.com>\n\
Subject: SALE\n\
Date: Mon, 05 Jan 2026 11:00:00 -0400\n\
Message-ID: <promo1@x>\n\
X-Gmail-Labels: Inbox,Category Promotions,\n\
 Unread\n\
\n\
buy things\n\
\n\
From c@x Mon Jan 05 12:00:00 2026\n\
From: Evil <evil@spam.example>\n\
Subject: prince needs help\n\
Date: Mon, 05 Jan 2026 12:00:00 -0400\n\
Message-ID: <spam1@x>\n\
X-Gmail-Labels: Spam,Unread\n\
\n\
send money\n";

    #[test]
    fn test_label_filter_core_strata() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mail.mbox");
        std::fs::write(&path, LABELED).unwrap();

        // Core filter: only the Category Personal message survives.
        let core = LabelFilter {
            include_any: vec!["Sent".into(), "Category Personal".into(), "Starred".into()],
            exclude_any: vec![],
        };
        let mut kept = Vec::new();
        let total = ingest_batched_filtered(&path, None, &core, usize::MAX, |batch| {
            kept.extend(batch);
            Ok(())
        })
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(kept[0].source_ref, "keep1@x");
        // Labels are recorded in the body for later strata reasoning.
        assert!(kept[0]
            .body
            .contains("labels: Archived, Category Personal, Opened"));
    }

    #[test]
    fn test_label_filter_spam_always_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mail.mbox");
        std::fs::write(&path, LABELED).unwrap();

        // Even an unfiltered (default) run drops Spam/Trash; the folded
        // Promotions message and the personal one survive.
        let mut kept = Vec::new();
        ingest_batched(&path, None, usize::MAX, |batch| {
            kept.extend(batch);
            Ok(())
        })
        .unwrap();
        let refs: Vec<&str> = kept.iter().map(|d| d.source_ref.as_str()).collect();
        assert_eq!(refs, vec!["keep1@x", "promo1@x"]);

        // An explicit include of Spam still loses to the hard exclusion.
        let filter = LabelFilter {
            include_any: vec!["Spam".into()],
            exclude_any: vec![],
        };
        let total = ingest_batched_filtered(&path, None, &filter, usize::MAX, |_| Ok(())).unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_gmail_labels_folded_header() {
        let msg =
            "Subject: x\nX-Gmail-Labels: Inbox,Category Promotions,\n Unread\nDate: y\n\nbody";
        assert_eq!(
            gmail_labels(msg),
            vec!["Inbox", "Category Promotions", "Unread"]
        );
        assert_eq!(gmail_labels("Subject: x\n\nbody"), Vec::<String>::new());
    }
}

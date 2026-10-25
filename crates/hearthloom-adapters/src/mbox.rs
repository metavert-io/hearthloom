//! Mbox adapter (Gmail Takeout et al.): one email = one episode; sender
//! and recipients become Person entities keyed by email address, so mail
//! participates in the same cross-source identity as git and calendars.
//!
//! Deliberately shallow MIME handling: headers are unfolded and the first
//! text/plain part is preferred, but no full MIME tree is built. The raw
//! message text is kept as the episode body (capped), so a better parser
//! later can re-derive everything.

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
            include_any: vec![
                "Sent".into(),
                "Category Personal".into(),
                "Starred".into(),
            ],
            exclude_any: vec![],
        }
    }

    fn keeps(&self, labels: &[String]) -> bool {
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
fn gmail_labels(message: &str) -> Vec<String> {
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
    mut sink: impl FnMut(Vec<EpisodeDraft>) -> Result<()>,
) -> Result<usize> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // 1MB buffer: mbox lines are short but plentiful.
    let reader = std::io::BufReader::with_capacity(1 << 20, file);

    let mut current = String::new();
    let mut in_message = false;
    let mut prev_blank = true;
    let mut batch: Vec<EpisodeDraft> = Vec::new();
    let mut total = 0usize;

    let flush_message =
        |current: &mut String, batch: &mut Vec<EpisodeDraft>, total: &mut usize| {
            if current.is_empty() {
                return;
            }
            let labels = gmail_labels(current);
            if !filter.keeps(&labels) {
                current.clear();
                return;
            }
            match message_to_draft(current, &labels, scope) {
                Ok(draft) => {
                    batch.push(draft);
                    *total += 1;
                }
                Err(err) => tracing::warn!("skipping message: {err}"),
            }
            current.clear();
        };

    for line in reader.split(b'\n') {
        let line = line?;
        // Tolerate non-UTF8 bytes (old mail is full of them).
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end_matches('\r');
        if prev_blank && line.starts_with("From ") {
            flush_message(&mut current, &mut batch, &mut total);
            if batch.len() >= batch_size {
                sink(std::mem::take(&mut batch))?;
            }
            in_message = true;
            prev_blank = false;
            continue;
        }
        if in_message {
            current.push_str(line);
            current.push('\n');
        }
        prev_blank = line.trim().is_empty();
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
    let labels = gmail_labels(message);
    if !filter.keeps(&labels) {
        return Ok(None);
    }
    message_to_draft(message, &labels, scope).map(Some)
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

struct Headers<'a> {
    entries: Vec<(String, String)>,
    body: &'a str,
}

impl Headers<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse headers (with RFC 5322 unfolding) and locate the body.
fn parse_headers(message: &str) -> Headers<'_> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut body_start = message.len();
    let mut offset = 0;
    for line in message.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            body_start = offset + line.len();
            break;
        }
        if (line.starts_with(' ') || line.starts_with('\t')) && !entries.is_empty() {
            let last = entries.last_mut().unwrap();
            last.1.push(' ');
            last.1.push_str(trimmed.trim_start());
        } else if let Some((name, value)) = trimmed.split_once(':') {
            entries.push((name.trim().to_string(), value.trim().to_string()));
        }
        offset += line.len();
    }
    Headers {
        entries,
        body: &message[body_start..],
    }
}

/// Extract (email, display name) pairs from an address header value.
fn parse_addresses(value: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Split on commas outside quotes/angle brackets.
    let mut depth = 0;
    let mut in_quotes = false;
    let mut current = String::new();
    let mut parts = Vec::new();
    for c in value.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => depth += 1,
            '>' if !in_quotes => depth -= 1,
            ',' if !in_quotes && depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);

    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, email) = match (part.find('<'), part.find('>')) {
            (Some(lt), Some(gt)) if gt > lt => {
                (part[..lt].trim().trim_matches('"'), part[lt + 1..gt].trim())
            }
            _ => ("", part),
        };
        let email = email.trim().to_lowercase();
        if !email.contains('@') || email.contains(' ') {
            continue;
        }
        // RFC 2047 encoded-word display names are left undecoded; fall
        // back to the mailbox local part instead of showing gibberish.
        let name = if name.is_empty() || name.starts_with("=?") {
            email.split('@').next().unwrap_or(&email).to_string()
        } else {
            name.to_string()
        };
        out.push((email, name));
    }
    out
}

/// Prefer the first text/plain part of a multipart body; otherwise the
/// body as-is. Shallow: looks for the boundary marker, not a MIME tree.
fn extract_text(headers: &Headers<'_>) -> String {
    let body = headers.body;
    let content_type = headers.get("Content-Type").unwrap_or("text/plain");
    if let Some(boundary) = content_type
        .split("boundary=")
        .nth(1)
        .map(|b| b.trim_matches(|c| c == '"' || c == ' ' || c == ';'))
    {
        let marker = format!("--{boundary}");
        for part in body.split(&marker) {
            if part.trim().is_empty() || part.trim() == "--" {
                continue; // preamble / closing delimiter
            }
            let parsed = parse_headers(part.trim_start_matches(['\r', '\n']));
            let part_type = parsed.get("Content-Type").unwrap_or("text/plain");
            if part_type.starts_with("text/plain") && !parsed.body.trim().is_empty() {
                return parsed.body.to_string();
            }
        }
    }
    body.to_string()
}

fn message_to_draft(
    message: &str,
    labels: &[String],
    scope: Option<&str>,
) -> Result<EpisodeDraft> {
    let headers = parse_headers(message);
    let subject = headers.get("Subject").unwrap_or("(no subject)").to_string();
    let date = headers
        .get("Date")
        .ok_or_else(|| anyhow!("message without Date: {subject}"))?;
    let event_time = match DateTime::parse_from_rfc2822(date) {
        Ok(dt) => EventTime::from_fixed(dt, TimeUncertainty::Exact),
        Err(_) => EventTime::unknown(),
    };

    let from = headers.get("From").map(parse_addresses).unwrap_or_default();
    let mut recipients = Vec::new();
    for header in ["To", "Cc"] {
        if let Some(value) = headers.get(header) {
            recipients.extend(parse_addresses(value));
        }
    }

    // Source-native identity: Message-ID, else a content hash stand-in.
    let source_ref = headers
        .get("Message-ID")
        .map(|id| id.trim_matches(|c| c == '<' || c == '>').to_string())
        .unwrap_or_else(|| {
            format!(
                "nomsgid:{}",
                hearthloom_core::ids::content_hash(message.as_bytes())
            )
        });

    let mut text = extract_text(&headers);
    if text.len() > BODY_CAP {
        let mut cut = BODY_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[truncated]");
    }
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

        let draft = message_to_draft(messages[0], &[], None).unwrap();
        assert_eq!(draft.title, "Re: roadmap");
        assert_eq!(draft.source_ref, "abc123@mail.example.com");
        assert_eq!(draft.kind, "email");
        // 09:30 -0400 = 13:30 UTC.
        assert_eq!(draft.event_time.utc.as_deref(), Some("2026-03-16T13:30:00Z"));
        assert_eq!(draft.event_time.tz_offset_minutes, Some(-240));
        // Author + 2 recipients, corresponded_with edges.
        assert_eq!(draft.entities.len(), 3);
        assert_eq!(draft.entities[0].role, "author");
        assert_eq!(
            draft.entities[0].attributes[0].1,
            "jon@harbor.example"
        );
        assert_eq!(draft.entities[2].name, "sam");
        assert_eq!(draft.edges.len(), 2);
        assert!(draft.body.contains("Sounds good"));
    }

    #[test]
    fn test_multipart_prefers_text_plain() {
        let messages = split_mbox(SAMPLE);
        let draft = message_to_draft(messages[1], &[], None).unwrap();
        assert!(draft.body.contains("plain part here"));
        assert!(!draft.body.contains("<b>html part</b>"));
        // Encoded-word display name falls back to the local part.
        assert_eq!(draft.entities[0].name, "jane");
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
            include_any: vec![
                "Sent".into(),
                "Category Personal".into(),
                "Starred".into(),
            ],
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
        assert!(kept[0].body.contains("labels: Archived, Category Personal, Opened"));
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
        let total = ingest_batched_filtered(&path, None, &filter, usize::MAX, |_| Ok(()))
            .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_gmail_labels_folded_header() {
        let msg = "Subject: x\nX-Gmail-Labels: Inbox,Category Promotions,\n Unread\nDate: y\n\nbody";
        assert_eq!(
            gmail_labels(msg),
            vec!["Inbox", "Category Promotions", "Unread"]
        );
        assert_eq!(gmail_labels("Subject: x\n\nbody"), Vec::<String>::new());
    }

    #[test]
    fn test_address_parsing() {
        let addrs = parse_addresses("\"Doe, Jane\" <jane@x.example>, bob@y.example");
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], ("jane@x.example".into(), "Doe, Jane".into()));
        assert_eq!(addrs[1], ("bob@y.example".into(), "bob".into()));
    }
}

//! Substack adapter: a publication's public archive — the author's
//! professional-interest timeline. Uses Substack's public JSON API
//! (`/api/v1/archive` for the index, `/api/v1/posts/{slug}` for bodies),
//! which also works on custom domains backed by Substack.

use std::path::Path;

use anyhow::{anyhow, Result};
use chrono::DateTime;
use hearthloom_core::{EpisodeDraft, EventTime, TimeUncertainty};
use serde_json::Value;

const PAGE: usize = 12;
const BODY_CAP: usize = 96 * 1024;

/// `location` is the publication URL (subdomain or custom domain).
pub fn ingest(location: &str, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let base = location
        .trim_end_matches('/')
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let base = format!("https://{base}");
    let client = reqwest::blocking::Client::builder()
        .user_agent("hearthloom/0.1 (+local personal archive)")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let fetch_page = |offset: usize| -> Result<Value> {
        let url = format!("{base}/api/v1/archive?sort=new&offset={offset}&limit={PAGE}");
        let mut last_err = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(3 * attempt as u64));
            }
            match client
                .get(&url)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.json::<Value>())
            {
                Ok(page) => return Ok(page),
                Err(err) => last_err = Some(err),
            }
        }
        Err(anyhow!("fetching {url}: {}", last_err.unwrap()))
    };

    let mut drafts = Vec::new();
    let mut offset = 0usize;
    loop {
        let page = match fetch_page(offset) {
            Ok(page) => page,
            Err(err) => {
                // A mid-archive failure shouldn't discard fetched posts:
                // re-runs dedupe, so partial progress is progress.
                tracing::warn!("archive paging stopped early: {err:#}");
                break;
            }
        };
        let posts = page
            .as_array()
            .ok_or_else(|| anyhow!("unexpected archive shape"))?;
        if posts.is_empty() {
            break;
        }
        for post in posts {
            match post_to_draft(&client, &base, post, scope) {
                Ok(draft) => drafts.push(draft),
                Err(err) => tracing::warn!("skipping post: {err:#}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        offset += posts.len();
        if posts.len() < PAGE {
            break;
        }
    }
    if drafts.is_empty() {
        return Err(anyhow!("no posts found at {base}"));
    }
    Ok(drafts)
}

/// Path-based entry point for API symmetry with other adapters (unused).
pub fn ingest_path(_path: &Path) -> Result<Vec<EpisodeDraft>> {
    Err(anyhow!("substack ingests from a URL, not a path"))
}

fn post_to_draft(
    client: &reqwest::blocking::Client,
    base: &str,
    post: &Value,
    scope: Option<&str>,
) -> Result<EpisodeDraft> {
    let title = post
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("(untitled)")
        .to_string();
    let slug = post
        .get("slug")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("post without slug: {title}"))?;
    let post_date = post
        .get("post_date")
        .and_then(|d| d.as_str())
        .ok_or_else(|| anyhow!("post without date: {title}"))?;
    let event_time = DateTime::parse_from_rfc3339(post_date)
        .map(|dt| EventTime {
            utc: Some(dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            tz_offset_minutes: None, // publish time is UTC; local offset unknown
            uncertainty: TimeUncertainty::Exact,
        })
        .unwrap_or_else(|_| EventTime::unknown());
    let subtitle = post
        .get("subtitle")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let canonical = post
        .get("canonical_url")
        .and_then(|u| u.as_str())
        .unwrap_or(slug)
        .to_string();

    // Full body needs the per-post endpoint; paid-only posts may return
    // no body for anonymous fetches — keep the metadata regardless.
    let text = match client
        .get(format!("{base}/api/v1/posts/{slug}"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json::<Value>())
    {
        Ok(full) => full
            .get("body_html")
            .and_then(|b| b.as_str())
            .map(html_to_text)
            .unwrap_or_default(),
        Err(err) => {
            tracing::warn!("no body for {slug}: {err}");
            String::new()
        }
    };

    let mut body = format!("article: {title}\n");
    if !subtitle.is_empty() {
        body.push_str(&format!("subtitle: {subtitle}\n"));
    }
    body.push_str(&format!("url: {canonical}\n\n"));
    let mut text = text;
    if text.len() > BODY_CAP {
        let mut cut = BODY_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[truncated]");
    }
    body.push_str(text.trim());

    Ok(EpisodeDraft {
        source: "substack".into(),
        source_ref: canonical,
        kind: "article".into(),
        title,
        body,
        event_time,
        trust: "self".into(),
        scope: scope.map(String::from),
        entities: vec![],
        edges: vec![],
    })
}

/// Minimal HTML→text: drop tags, keep paragraph/list structure, decode
/// the common entities. Not a sanitizer (output is plain text).
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let t = tag.trim_start_matches('/').split_whitespace().next().unwrap_or("");
                if matches!(t, "p" | "br" | "div" | "h1" | "h2" | "h3" | "h4" | "li" | "blockquote" | "tr")
                    && !out.ends_with("\n\n")
                {
                    out.push('\n');
                    if tag.starts_with('/') && matches!(t, "p" | "h1" | "h2" | "h3" | "h4") {
                        out.push('\n');
                    }
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .replace("\n\n\n", "\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_to_text() {
        let html = "<h2>Notes &amp; Links</h2><p>First <strong>bold</strong> point.</p>\
                    <ul><li>one</li><li>two &gt; three</li></ul>";
        let text = html_to_text(html);
        assert!(text.contains("Notes & Links"));
        assert!(text.contains("First bold point."));
        assert!(text.contains("one"));
        assert!(text.contains("two > three"));
        assert!(!text.contains('<'));
    }
}

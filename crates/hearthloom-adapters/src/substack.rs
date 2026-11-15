//! Substack adapter: a publication's public archive — the author's
//! professional-interest timeline. Uses Substack's public JSON API
//! (`/api/v1/archive` for the index, `/api/v1/posts/{slug}` for bodies),
//! which also works on custom domains backed by Substack. Posts land at
//! trust `imported`: the URL can name any publication.
//!
//! A post's ref is its canonical address, and "this source is mine" (core
//! store/owned.rs) reads the publication from that address's host. The
//! address comes from the server's JSON, so a post is kept only when its
//! host is the host that served the archive (after redirects: a
//! `name.substack.com` that redirects to its own domain serves from
//! there), and its text is fetched from that same host. A post that names
//! another host is left out: any server could say it is someone else's.
//! Each post's text begins `served from: <that host's address>`; posts an
//! earlier version stored lack the line, and never count as the owner's.

use std::collections::BTreeSet;

use anyhow::{anyhow, Result};
use chrono::DateTime;
use hearthloom_core::{EpisodeDraft, EventTime, TimeUncertainty};
use reqwest::Url;
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
    ingest_from(&client, &base, scope)
}

/// Host and port a URL names, the way publications are compared: host
/// lowercased without a leading `www.`, port spelled out.
fn site_of(url: &Url) -> Option<(String, u16)> {
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    Some((host, url.port_or_known_default()?))
}

/// `scheme://host[:port]` of a URL (where its pages were served from).
fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn ingest_from(
    client: &reqwest::blocking::Client,
    base: &str,
    scope: Option<&str>,
) -> Result<Vec<EpisodeDraft>> {
    // A page and the URL it was finally served from.
    let fetch_page = |offset: usize| -> Result<(Value, Url)> {
        let url = format!("{base}/api/v1/archive?sort=new&offset={offset}&limit={PAGE}");
        let mut last_err = None;
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_secs(3 * attempt as u64));
            }
            match client.get(&url).send().and_then(|r| r.error_for_status()) {
                Ok(response) => {
                    let served = response.url().clone();
                    match response.json::<Value>() {
                        Ok(page) => return Ok((page, served)),
                        Err(err) => last_err = Some(err),
                    }
                }
                Err(err) => last_err = Some(err),
            }
        }
        Err(anyhow!("fetching {url}: {}", last_err.unwrap()))
    };

    let mut drafts = Vec::new();
    let mut elsewhere = BTreeSet::new();
    let mut offset = 0usize;
    loop {
        let (page, served) = match fetch_page(offset) {
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
            match post_to_draft(client, &served, post, scope) {
                Ok(Kept::Draft(draft)) => drafts.push(*draft),
                Ok(Kept::Elsewhere(host)) => {
                    tracing::warn!(
                        "leaving out a post at {base}: its address is on {host}, not the host \
                         that served it"
                    );
                    elsewhere.insert(host);
                }
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
        if !elsewhere.is_empty() {
            return Err(anyhow!(
                "the posts at {base} give their address on {} — import that address instead",
                elsewhere.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        return Err(anyhow!("no posts found at {base}"));
    }
    Ok(drafts)
}

/// What became of one post of the archive.
enum Kept {
    Draft(Box<EpisodeDraft>),
    /// Its canonical address is on another host (named here).
    Elsewhere(String),
}

/// One post of an archive page served from `served`.
fn post_to_draft(
    client: &reqwest::blocking::Client,
    served: &Url,
    post: &Value,
    scope: Option<&str>,
) -> Result<Kept> {
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
    let site = site_of(served).ok_or_else(|| anyhow!("the archive came from no host"))?;
    let canonical = match post.get("canonical_url").and_then(|u| u.as_str()) {
        Some(url) => {
            let parsed = Url::parse(url.trim())
                .ok()
                .filter(|u| matches!(u.scheme(), "https" | "http"));
            match parsed.as_ref().and_then(site_of) {
                Some(theirs) if theirs == site => url.trim().to_string(),
                Some((host, _)) => return Ok(Kept::Elsewhere(host)),
                None => return Err(anyhow!("{title:?} gives an address that can't be read")),
            }
        }
        // no address: the slug alone, which names no publication
        None => slug.to_string(),
    };

    // Full body needs the per-post endpoint, asked of the host that served
    // the archive (and kept only when that host answered); paid-only posts
    // may return no body for anonymous fetches — keep the metadata
    // regardless.
    let text = match client
        .get(format!("{}/api/v1/posts/{slug}", origin_of(served)))
        .send()
        .and_then(|r| r.error_for_status())
    {
        Ok(response) if site_of(response.url()).as_ref() != Some(&site) => {
            tracing::warn!("no body for {slug}: another host answered");
            String::new()
        }
        Ok(response) => match response.json::<Value>() {
            Ok(full) => full
                .get("body_html")
                .and_then(|b| b.as_str())
                .map(html_to_text)
                .unwrap_or_default(),
            Err(err) => {
                tracing::warn!("no body for {slug}: {err}");
                String::new()
            }
        },
        Err(err) => {
            tracing::warn!("no body for {slug}: {err}");
            String::new()
        }
    };

    // First, before anything the server wrote: the host that served the
    // archive, which the address below was checked against. "This source
    // is mine" counts a post only with this line (an earlier version wrote
    // none and checked nothing), so keep it first and keep its format.
    let mut body = format!("served from: {}\n", origin_of(served));
    body.push_str(&format!("article: {title}\n"));
    if !subtitle.is_empty() {
        body.push_str(&format!("subtitle: {subtitle}\n"));
    }
    body.push_str(&format!("url: {canonical}\n\n"));
    let mut text = text;
    if text.len() > BODY_CAP {
        text.truncate(text.floor_char_boundary(BODY_CAP));
        text.push_str("\n[truncated]");
    }
    body.push_str(text.trim());

    Ok(Kept::Draft(Box::new(EpisodeDraft {
        source: "substack".into(),
        source_ref: canonical,
        kind: "article".into(),
        title,
        body,
        event_time,
        // any publication's URL can be given, so its posts are not taken
        // as the owner's own words (core `OTHERS_WORDS_SOURCES`) — unless
        // the owner marked the host in this ref as theirs (core
        // store/owned.rs); the ref's host is the host that served it
        trust: "imported".into(),
        scope: scope.map(String::from),
        entities: vec![],
        edges: vec![],
    })))
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
                let t = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if matches!(
                    t,
                    "p" | "br" | "div" | "h1" | "h2" | "h3" | "h4" | "li" | "blockquote" | "tr"
                ) && !out.ends_with("\n\n")
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
    use std::io::{BufRead, BufReader, Write};

    /// A publication server on loopback: the archive lists `posts` (slug,
    /// canonical address — None: under this server's own address) and each
    /// post's text is "text of <slug>". Returns the server's address.
    fn serve(posts: Vec<(&'static str, Option<String>)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let posts: Vec<(&str, String)> = posts
            .into_iter()
            .map(|(slug, canonical)| (slug, canonical.unwrap_or(format!("{base}/p/{slug}"))))
            .collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = if path.starts_with("/api/v1/archive") {
                    serde_json::Value::Array(
                        posts
                            .iter()
                            .map(|(slug, canonical)| {
                                serde_json::json!({
                                    "title": slug, "slug": slug,
                                    "post_date": "2026-11-01T00:00:00Z",
                                    "canonical_url": canonical,
                                })
                            })
                            .collect(),
                    )
                    .to_string()
                } else {
                    let slug = path.rsplit('/').next().unwrap_or("");
                    serde_json::json!({"body_html": format!("<p>text of {slug}</p>")}).to_string()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap()
    }

    /// A post is kept only under an address on the host that served it: a
    /// server can't file its text under someone else's publication.
    #[test]
    fn test_a_post_is_kept_only_under_the_host_that_served_it() {
        let own = serve(vec![
            ("mine", None),
            ("forged", Some("https://owner.substack.com/p/forged".into())),
        ]);
        let drafts = ingest_from(&client(), &own, None).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].source_ref, format!("{own}/p/mine"));
        assert!(drafts[0].body.contains("text of mine"));
        // the host that served it is the first line, ahead of anything the
        // server wrote (core store/owned.rs reads it)
        assert_eq!(
            drafts[0].body.lines().next(),
            Some(format!("served from: {own}").as_str())
        );
        assert_eq!(
            hearthloom_core::store::substack_served_from(
                "served from: https://owner.substack.com\narticle: x"
            )
            .as_deref(),
            hearthloom_core::store::key_of_ref("substack", "https://owner.substack.com/p/x")
                .as_deref()
        );

        // nothing on the serving host: refused, naming where to import
        let elsewhere = serve(vec![(
            "forged",
            Some("https://owner.substack.com/p/forged".into()),
        )]);
        let err = ingest_from(&client(), &elsewhere, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("owner.substack.com"), "{err}");
    }

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

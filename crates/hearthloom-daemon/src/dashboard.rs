//! The dashboard: a static, offline-first SPA embedded in the binary and
//! served by the daemon (the dashboard is a *view* over the brain; all
//! data flows through the same /v1 API agents use). No build chain, no
//! CDN — three.js is vendored so the brain stays useful with no network.

use std::sync::LazyLock;

use axum::extract::{Path, State};
use axum::http::{header, HeaderName, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::AppState;

/// The dashboard shell, embedded at build time.
const INDEX_HTML: &str = include_str!("../assets/index.html");

/// The Content-Security-Policy on the dashboard shell and its assets: the
/// XSS backstop. If an escaping bug ever lets markup into a view, the
/// browser still runs only script files served by this origin — no
/// inline handlers (`onclick="…"`), no `javascript:` URLs, no eval.
/// `test_no_inline_event_handlers_in_dashboard_assets` keeps inline
/// handlers out of the assets so the policy never has to be loosened.
///
/// Loosened from all-`'self'` only where the dashboard needs it:
/// - `script-src` adds the SHA-256 of each inline `<script>` in
///   index.html — today only the import map that resolves three.js's bare
///   `three` specifier. Hashed from the embedded file, never hardcoded,
///   so editing index.html can't silently block it.
/// - `style-src 'unsafe-inline'`: the views set `style="…"` attributes
///   throughout (inline styles can't run script).
/// - `img-src https: http:`: notes render web images from markdown
///   (`![](https?://…)`, notes.js renderMarkdown) and avatars on profile
///   cards may be `https://` URLs (app.js friendAvatar). `data:` covers
///   the favicon and uploaded avatars.
/// - `frame-src 'self'`: ember pages and Desk widgets are sandboxed
///   same-origin iframes (`/plugins/{name}/ui/…`).
///
/// Ember UI responses (`/plugins/{name}/ui/*`, http.rs plugin_ui_asset)
/// don't get this policy: they run the ember's own code in a sandbox.
static DASHBOARD_CSP: LazyLock<String> = LazyLock::new(|| dashboard_csp(INDEX_HTML));

fn dashboard_csp(index_html: &str) -> String {
    let hashes: String = inline_scripts(index_html)
        .iter()
        .map(|script| format!(" {}", csp_hash(script)))
        .collect();
    format!(
        "default-src 'self'; script-src 'self'{hashes}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob: https: http:; connect-src 'self'; font-src 'self'; \
         frame-src 'self'; worker-src 'self' blob:; object-src 'none'; base-uri 'none'; \
         frame-ancestors 'self'; form-action 'self'"
    )
}

/// A CSP hash-source for an inline script's text.
fn csp_hash(script: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(script.as_bytes());
    format!("'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(digest))
}

/// The text of each inline `<script>` (one without `src`) in `html`, as
/// the browser hashes it: newlines normalized the way the HTML parser
/// does (CR LF and a lone CR become LF), so a CRLF checkout can't skew
/// the hash. Scripts inside `<!-- … -->` are skipped.
fn inline_scripts(html: &str) -> Vec<String> {
    // ASCII lowercasing keeps every byte offset, so positions found in
    // `lower` index `html` too (all of them sit on ASCII `<` or `>`).
    let lower = html.to_ascii_lowercase();
    let mut scripts = Vec::new();
    let mut at = 0;
    while let Some(open) = lower[at..].find("<script").map(|i| at + i) {
        if let Some(comment) = lower[at..open].find("<!--").map(|i| at + i) {
            at = lower[comment..].find("-->").map_or(lower.len(), |i| comment + i + 3);
            continue;
        }
        let attrs_start = open + "<script".len();
        // `<scripts>` or `<script-x>` would be some other element
        if !matches!(
            lower.as_bytes().get(attrs_start),
            Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/' | b'>')
        ) {
            at = attrs_start;
            continue;
        }
        // the start tag ends at the first `>` outside a quoted value
        let mut quote = None;
        let Some(tag_end) = lower[attrs_start..]
            .char_indices()
            .find(|&(_, c)| match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                    false
                }
                None if c == '"' || c == '\'' => {
                    quote = Some(c);
                    false
                }
                None => c == '>',
            })
            .map(|(i, _)| attrs_start + i)
        else {
            break;
        };
        let body_start = tag_end + 1;
        let body_end = lower[body_start..].find("</script").map_or(lower.len(), |i| body_start + i);
        if !names_src(&lower[attrs_start..tag_end]) {
            scripts.push(html[body_start..body_end].replace("\r\n", "\n").replace('\r', "\n"));
        }
        at = body_end;
    }
    scripts
}

/// Whether a start tag's attribute text (lowercased) has a `src` attribute.
fn names_src(attrs: &str) -> bool {
    let mut rest = attrs;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        if rest.is_empty() {
            return false;
        }
        let name_len = rest
            .find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        if &rest[..name_len] == "src" {
            return true;
        }
        rest = rest[name_len..].trim_start();
        if let Some(value) = rest.strip_prefix('=') {
            let value = value.trim_start();
            rest = match value.chars().next() {
                Some(q @ ('"' | '\'')) => value[1..].find(q).map_or("", |i| &value[i + 2..]),
                _ => value.find(|c: char| c.is_ascii_whitespace()).map_or("", |i| &value[i..]),
            };
        }
    }
}

/// Headers on every dashboard response, the shell and its assets alike.
/// On a script or stylesheet the CSP itself is inert (it governs
/// documents and workers); it is there for any asset opened as a page.
/// `nosniff` makes the browser trust the declared types — no JSON or CSS
/// run as script. `same-origin` keeps the node's address out of the
/// Referer sent to outside sites.
fn hardening_headers() -> [(HeaderName, &'static str); 3] {
    [
        (header::CONTENT_SECURITY_POLICY, DASHBOARD_CSP.as_str()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
    ]
}

pub async fn index() -> Response {
    (
        hardening_headers(),
        [(header::CACHE_CONTROL, "no-cache, must-revalidate")],
        Html(INDEX_HTML),
    )
        .into_response()
}

pub async fn asset(Path(name): Path<String>) -> Response {
    let (body, mime): (&'static [u8], &'static str) = match name.as_str() {
        "app.css" => (include_bytes!("../assets/app.css"), "text/css"),
        "app.js" => (
            include_bytes!("../assets/app.js"),
            "text/javascript",
        ),
        "atlas.js" => (
            include_bytes!("../assets/atlas.js"),
            "text/javascript",
        ),
        "integrations.js" => (
            include_bytes!("../assets/integrations.js"),
            "text/javascript",
        ),
        // Vendored MIT qrcode-generator (Kazuhiko Arase 1.4.4) — the
        // pair-a-device QR; no CDN per design law #1.
        "qrcode.js" => (
            include_bytes!("../assets/qrcode.js"),
            "text/javascript",
        ),
        "peering.js" => (
            include_bytes!("../assets/peering.js"),
            "text/javascript",
        ),
        "network.js" => (
            include_bytes!("../assets/network.js"),
            "text/javascript",
        ),
        "monitor.js" => (
            include_bytes!("../assets/monitor.js"),
            "text/javascript",
        ),
        "agents.js" => (
            include_bytes!("../assets/agents.js"),
            "text/javascript",
        ),
        "retire.js" => (
            include_bytes!("../assets/retire.js"),
            "text/javascript",
        ),
        "spend.js" => (
            include_bytes!("../assets/spend.js"),
            "text/javascript",
        ),
        "keys.js" => (
            include_bytes!("../assets/keys.js"),
            "text/javascript",
        ),
        "about.js" => (
            include_bytes!("../assets/about.js"),
            "text/javascript",
        ),
        "settings.js" => (
            include_bytes!("../assets/settings.js"),
            "text/javascript",
        ),
        "vendor/world-land-110m.json" => (
            include_bytes!("../assets/vendor/world-land-110m.json"),
            "application/json",
        ),
        "pulse.js" => (
            include_bytes!("../assets/pulse.js"),
            "text/javascript",
        ),
        "desk.js" => (
            include_bytes!("../assets/desk.js"),
            "text/javascript",
        ),
        "loom.js" => (
            include_bytes!("../assets/loom.js"),
            "text/javascript",
        ),
        "notes.js" => (
            include_bytes!("../assets/notes.js"),
            "text/javascript",
        ),
        "orrery.js" => (
            include_bytes!("../assets/orrery.js"),
            "text/javascript",
        ),
        "relationships.js" => (
            include_bytes!("../assets/relationships.js"),
            "text/javascript",
        ),
        "globe.js" => (
            include_bytes!("../assets/globe.js"),
            "text/javascript",
        ),
        "biography.js" => (
            include_bytes!("../assets/biography.js"),
            "text/javascript",
        ),
        "rewind.js" => (
            include_bytes!("../assets/rewind.js"),
            "text/javascript",
        ),
        "dial.js" => (
            include_bytes!("../assets/dial.js"),
            "text/javascript",
        ),
        "chart-svg.js" => (
            include_bytes!("../assets/chart-svg.js"),
            "text/javascript",
        ),
        "constellation.js" => (
            include_bytes!("../assets/constellation.js"),
            "text/javascript",
        ),
        "galaxy.js" => (
            include_bytes!("../assets/galaxy.js"),
            "text/javascript",
        ),
        "vendor/three.module.min.js" => (
            include_bytes!("../assets/vendor/three.module.min.js"),
            "text/javascript",
        ),
        "vendor/OrbitControls.js" => (
            include_bytes!("../assets/vendor/OrbitControls.js"),
            "text/javascript",
        ),
        _ => {
            return (StatusCode::NOT_FOUND, hardening_headers(), "no such asset").into_response()
        }
    };
    // The dashboard assets are compiled into the binary and change on
    // every deploy; without a cache directive browsers heuristically
    // cache them forever and serve stale JS (the "my fix isn't showing,
    // clicks do nothing" class of bug). `no-cache` = always revalidate,
    // so a normal reload after a deploy is enough — no hard-refresh.
    (
        hardening_headers(),
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-cache, must-revalidate"),
        ],
        body,
    )
        .into_response()
}

/// Temporal ribbons: months axis + per-entity monthly evidence counts.
pub async fn ribbons(State(state): State<AppState>) -> Response {
    let result = (|| -> anyhow::Result<serde_json::Value> {
        let (months, lanes) =
            state
                .store
                .ribbons(&["Project", "Person", "Place", "Org"], 18)?;
        Ok(json!({
            "months": months,
            "ribbons": lanes
                .into_iter()
                .map(|(entity, counts)| json!({
                    "name": entity.name,
                    "type": entity.entity_type,
                    "counts": counts,
                }))
                .collect::<Vec<_>>(),
        }))
    })();
    match result {
        Ok(value) => Json(value).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// Aggregate numbers + latest digest for the dashboard's header/home.
///
/// Cached per vault-state for 60s and computed on the blocking pool:
/// these are whole-table aggregates, the masthead counter polls it
/// from every open dashboard tab, and a synchronous run on a runtime
/// worker is the starvation shape THE PASS CONTRACT bans for passes —
/// a train of uncancellable /v1/stats calls (36s+ each at tenant
/// scale) wedged the tenant for four hours on 2026-07-19.
///
/// Two more rules since the 2026-10-06 laptop freeze (cold page cache
/// after a restart, 26s per compute, every poll starting its own):
/// SINGLE-FLIGHT — at most one compute runs; and STALE-WHILE-REVALIDATE
/// — once a value exists for this vault state, every caller gets it
/// immediately and an expired one is refreshed in the background. Only
/// the first load after a restart (or a vault flip) ever waits. A value
/// computed under one vault state is NEVER served under the other — and
/// since the vault is per viewer, both views are kept side by side: a
/// locked phone and an unlocked browser polling together must not evict
/// each other into back-to-back recomputes.
pub async fn stats(State(state): State<AppState>) -> Response {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    // One slot per vault view, indexed by `locked`.
    type Slots = [Option<(Instant, serde_json::Value)>; 2];
    static CACHE: OnceLock<Mutex<Slots>> = OnceLock::new();
    static FLIGHT: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    static REFRESHING: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];
    const FRESH: Duration = Duration::from_secs(60);
    let (locked, _) = state.store.vault_status();
    let view = usize::from(locked);
    let cache = CACHE.get_or_init(|| Mutex::new([None, None]));
    let cached = |max_age: Option<Duration>| -> Option<serde_json::Value> {
        let guard = cache.lock().unwrap();
        let (at, value) = guard[view].as_ref()?;
        max_age.is_none_or(|age| at.elapsed() < age).then(|| value.clone())
    };
    if let Some(value) = cached(Some(FRESH)) {
        return Json(value).into_response();
    }
    if let Some(stale) = cached(None) {
        // Serve what we have; refresh once, off the request path.
        if !REFRESHING[view].swap(true, Ordering::SeqCst) {
            let state = state.clone();
            // Scoped: the refresh must compute the same view it refills.
            crate::scoped::spawn(async move {
                let result = crate::scoped::spawn_blocking(move || compute_stats(&state)).await;
                if let Ok(Ok(value)) = result {
                    cache.lock().unwrap()[view] = Some((Instant::now(), value));
                }
                REFRESHING[view].store(false, Ordering::SeqCst);
            });
        }
        return Json(stale).into_response();
    }
    // Nothing usable yet: compute, one caller at a time — the rest wait
    // here (parked, not grinding) and read the winner's result.
    let _flight = FLIGHT.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    if let Some(value) = cached(Some(FRESH)) {
        return Json(value).into_response();
    }
    let result = crate::scoped::spawn_blocking(move || compute_stats(&state))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
    match result {
        Ok(value) => {
            cache.lock().unwrap()[view] = Some((Instant::now(), value.clone()));
            Json(value).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// The stats payload. Blocking — call from the blocking pool only.
fn compute_stats(state: &AppState) -> anyhow::Result<serde_json::Value> {
    {
        let store = &state.store;
        // Per-part clock (the PassClock idea): when this endpoint is slow,
        // the log names the query instead of leaving it to guesswork.
        let started = std::time::Instant::now();
        let mut laps: Vec<String> = Vec::new();
        let mut last = started;
        let mut lap = |name: &str| {
            let now = std::time::Instant::now();
            laps.push(format!("{name}={}ms", now.duration_since(last).as_millis()));
            last = now;
        };
        let counts = store.counts()?;
        lap("counts");
        let coverage = store.embedding_coverage()?;
        lap("coverage");
        // The top-entities ranking is one GROUP BY over every
        // episode↔entity link — 2.5s+ under the store lock on a real
        // record, and it barely moves minute to minute. Keep it for ten
        // minutes (the galaxy's cadence) so the per-minute refresh costs
        // a few hundred ms instead. Keyed by vault state, like the rest.
        let top = {
            use std::sync::{Mutex, OnceLock};
            use std::time::{Duration, Instant};
            type TopSlots = [Option<(Instant, serde_json::Value)>; 2];
            static TOP: OnceLock<Mutex<TopSlots>> = OnceLock::new();
            let view = usize::from(store.vault_status().0);
            let slot = TOP.get_or_init(|| Mutex::new([None, None]));
            let kept = slot.lock().unwrap()[view].as_ref().and_then(|(at, value)| {
                (at.elapsed() < Duration::from_secs(600)).then(|| value.clone())
            });
            match kept {
                Some(value) => value,
                None => {
                    let value = json!(store
                        .top_entities(12)?
                        .into_iter()
                        .map(|(entity, n)| {
                            json!({"name": entity.name, "type": entity.entity_type, "episodes": n})
                        })
                        .collect::<Vec<_>>());
                    slot.lock().unwrap()[view] = Some((Instant::now(), value.clone()));
                    value
                }
            }
        };
        lap("top_entities");
        let digest = store.latest_summary()?.map(|episode| {
            let body = store.episode_body(&episode.id).ok().flatten().unwrap_or_default();
            json!({
                "date": hearthloom_core::store::base_ref(&episode.source_ref).strip_prefix("summary:").unwrap_or(""),
                "body": body,
            })
        });
        lap("digest");
        // ONE scan for the whole composition (was four, plus the total).
        let composition = store.episode_composition()?;
        lap("composition");
        let tasks_due = store.open_tasks_today()?;
        lap("tasks_due");
        let total_ms = started.elapsed().as_millis();
        if total_ms >= 500 {
            tracing::info!("stats compute {total_ms}ms: {}", laps.join(" "));
        }
        Ok(json!({
            "episodes": counts.episodes,
            "entities": counts.entities,
            "edges": counts.edges,
            "mutations": counts.mutations,
            "review_pending": counts.review_pending,
            "quarantine_pending": counts.quarantine_pending,
            "propositions_pending": counts.propositions_pending,
            "opportunities_pending": counts.opportunities_pending,
            "lint_pending": counts.lint_pending,
            "trips_pending": counts.trips_pending,
            "embedded": coverage.0,
            "chunks": coverage.1,
            "top_entities": top,
            "digest": digest,
            "sources": composition.sources,
            "tasks_due": tasks_due,
            "kinds": composition.kinds,
            "derivations": composition.derivations,
            "scopes": composition.scopes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CSP spec's own example: `alert('Hello, world.');` hashes to this
    /// source — so the encoding (SHA-256, standard padded base64) is right.
    #[test]
    fn test_csp_hash_matches_the_spec_example() {
        assert_eq!(
            csp_hash("alert('Hello, world.');"),
            "'sha256-qznLcsROx4GACP2dm0UCKCzCG+HiZ1guq6ZZDob/Tng='"
        );
    }

    /// The policy carries the hash of exactly the import map's text — found
    /// here by a plain search, independent of `inline_scripts` — or the 3-D
    /// views' bare `three` import stops resolving.
    #[test]
    fn test_csp_allows_the_importmap_by_hash() {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        const OPEN: &str = r#"<script type="importmap">"#;
        let start = INDEX_HTML.find(OPEN).expect("index.html has its import map") + OPEN.len();
        let body = &INDEX_HTML[start..];
        let body = body[..body.find("</script>").unwrap()].replace("\r\n", "\n");
        assert!(body.contains(r#""imports""#) && body.contains(r#""three""#), "not the import map: {body}");
        let digest = Sha256::digest(body.as_bytes());
        let expected = format!("'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(digest));
        let script_src = DASHBOARD_CSP
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("script-src "))
            .expect("a script-src directive");
        assert!(
            script_src.split_whitespace().any(|source| source == expected),
            "{script_src} lacks the import map's {expected}"
        );
    }

    /// Script stays same-origin files plus index.html's own hashed inline
    /// scripts: never 'unsafe-inline', 'unsafe-eval', a scheme or a host.
    #[test]
    fn test_csp_keeps_script_locked_down() {
        let directives: Vec<&str> = DASHBOARD_CSP.split(';').map(str::trim).collect();
        let script_src = directives.iter().find(|d| d.starts_with("script-src ")).unwrap();
        for source in script_src.split_whitespace().skip(1) {
            assert!(
                source == "'self'" || source.starts_with("'sha256-"),
                "script-src may only hold 'self' and hashes, found {source}"
            );
        }
        for directive in [
            "default-src 'self'",
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'self'",
            "form-action 'self'",
        ] {
            assert!(directives.contains(&directive), "missing `{directive}` in {directives:?}");
        }
        assert!(axum::http::HeaderValue::from_str(&DASHBOARD_CSP).is_ok());
    }

    #[test]
    fn test_inline_scripts_finds_only_inline_scripts() {
        let html = "<head><script src=\"/a.js\"></script>\r\n<!-- <script>old()</script> -->\
                    <SCRIPT type='importmap' data-x=\"a>b\">\r\n{}\r\n</SCRIPT>\
                    <script type=module src=/b.js></script><scripts>x</scripts>\
                    <script data-src=\"x\">y()</script></head>";
        assert_eq!(inline_scripts(html), ["\n{}\n", "y()"]);
        assert!(inline_scripts("<p>no scripts</p>").is_empty());
    }

    #[tokio::test]
    async fn test_dashboard_responses_carry_the_hardening_headers() {
        for response in [
            index().await,
            asset(Path("app.js".to_string())).await,
            asset(Path("no-such-file.js".to_string())).await,
        ] {
            let headers = response.headers();
            assert_eq!(headers[header::CONTENT_SECURITY_POLICY], DASHBOARD_CSP.as_str());
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert_eq!(headers[header::REFERRER_POLICY], "same-origin");
        }
    }

    /// Every `on<letters>=` where an attribute can begin (after whitespace,
    /// a quote, a backtick or `/`): markup like `<a onclick="…">`, not code
    /// like `el.onclick = …` or `const onKey = …`.
    fn inline_handlers(src: &str) -> Vec<(usize, String)> {
        let bytes = src.as_bytes();
        let mut found = Vec::new();
        for i in 1..bytes.len().saturating_sub(2) {
            let attribute_start =
                matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'`' | b'/');
            if !attribute_start || !bytes[i..i + 2].eq_ignore_ascii_case(b"on") {
                continue;
            }
            let name_end = i + 2 + bytes[i + 2..].iter().take_while(|b| b.is_ascii_alphabetic()).count();
            if name_end > i + 2
                && bytes.get(name_end) == Some(&b'=')
                && bytes.get(name_end + 1) != Some(&b'=')
            {
                let line = src[..i].matches('\n').count() + 1;
                found.push((line, src[i..=name_end].to_string()));
            }
        }
        found
    }

    #[test]
    fn test_inline_handler_scan_catches_markup_not_code() {
        let markup = "<input readonly onclick=\"this.select()\">\n<svg/onload=go()>\n<a\n  ONMouseOver='x'>";
        let caught: Vec<String> =
            inline_handlers(markup).into_iter().map(|(line, attr)| format!("{line}:{attr}")).collect();
        assert_eq!(caught, ["1:onclick=", "2:onload=", "4:ONMouseOver="]);
        let code = "el.onclick = () => go();\nconst onKey = (e) => e;\nlet onboardingState = \"done\";\n\
                    `<b data-confirm=\"sure?\" data-month=\"${m}\" contenteditable=\"true\">`;\nif (a.on==b) {}";
        assert!(inline_handlers(code).is_empty(), "{:?}", inline_handlers(code));
    }

    /// No dashboard asset may emit an inline event-handler attribute
    /// (`onclick="…"`, `onerror=…`): the CSP refuses to run them, so one
    /// would be a dead control. Wire it with addEventListener instead.
    /// Every file under assets/ is embedded (`index`, `asset`); the
    /// vendored libraries (vendor/, qrcode.js) are not ours and are skipped.
    #[test]
    fn test_no_inline_event_handlers_in_dashboard_assets() {
        const VENDORED: &[&str] = &["vendor", "qrcode.js"];
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets"));
        let mut stack = vec![root.clone()];
        let (mut files, mut offenders) = (0, Vec::new());
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.strip_prefix(&root).unwrap().display().to_string();
                if VENDORED.contains(&name.as_str()) {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !matches!(path.extension().and_then(|e| e.to_str()), Some("js" | "html")) {
                    continue;
                }
                files += 1;
                let src = std::fs::read_to_string(&path).unwrap();
                for (line, attribute) in inline_handlers(&src) {
                    offenders.push(format!("assets/{name}:{line}: {attribute}"));
                }
            }
        }
        assert!(files >= 25, "the scan found only {files} dashboard files");
        assert!(
            offenders.is_empty(),
            "{} inline event handler(s) in the dashboard — the CSP blocks them; \
             attach the listener with addEventListener after rendering:\n{}",
            offenders.len(),
            offenders.join("\n")
        );
    }
}

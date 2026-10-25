//! The hearthloom CLI. All reads and writes go through the daemon's HTTP
//! API (spawned on demand); the only in-process work is adapter parsing
//! and pretty-printing.

mod bench;
mod client;
mod eval;
mod ops;
mod registry;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use client::Client;
use hearthloom_core::Home;

#[derive(Parser)]
#[command(name = "hearthloom", version, about = "A local-first personal second brain")]
struct Cli {
    /// Target another node, e.g. https://brain.example.com
    /// (remote targets are never auto-spawned).
    #[arg(long, global = true)]
    url: Option<String>,
    /// Machine-readable JSON output where supported.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign in to a node and keep a CLI key (keychain-stored).
    Auth {
        #[command(subcommand)]
        command: AuthCmd,
    },
    /// Query-path latency (P50/P95) over the live HTTP surface — the
    /// regression guard on "retrieval is the product".
    Bench {
        /// Measured iterations per endpoint (default 20).
        #[arg(long)]
        iterations: Option<usize>,
    },
    /// Shell completions (bash|zsh|fish) to stdout.
    Completions { shell: String },
    /// Run a consolidation cycle now (embeddings backfill, intents,
    /// BRAIN.md refresh).
    Consolidate,
    /// Run or inspect the daemon (the sole writer of the store).
    Daemon {
        #[command(subcommand)]
        command: DaemonCmd,
    },
    /// Diagnostics — every wedge we've ever debugged, as checks.
    Doctor,
    /// Embers: search/install from a registry (--registry or HEARTHLOOM_REGISTRY_URL) + dev tools.
    Ember {
        #[command(subcommand)]
        command: EmberCmd,
    },
    /// Entity dossier: attributes, relations, cited episodes.
    Entity { name: Vec<String> },
    /// Run an eval suite against your own record. Suites are your data and
    /// live in ~/.hearthloom/eval/ (`personal-eval.yaml` by default; the
    /// first run copies an example there). `--ask` runs `ask-eval.yaml`
    /// through the full Ask engine instead — real LLM tokens; reports
    /// rounds, tokens, cost and latency per question.
    Eval {
        /// A suite file to run instead of the one in ~/.hearthloom/eval/.
        file: Option<PathBuf>,
        #[arg(long)]
        ask: bool,
        /// Run ~/.hearthloom/eval/<SUITE>.yaml (e.g. a private set kept apart).
        #[arg(long)]
        suite: Option<String>,
        /// With --ask: run only these question ids (comma-separated).
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },
    /// Change feed: mutations after a cursor.
    Events {
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Full export to a zip (requires the vault unlocked).
    Export {
        /// Output path (default: hearthloom-export-<ts>.zip)
        out: Option<PathBuf>,
    },
    /// Manage identities: many emails/handles, one human (or org/project).
    Identity {
        #[command(subcommand)]
        command: IdentityCmd,
    },
    /// Regenerate ~/.hearthloom/BRAIN.md.
    Index,
    /// Ingest a source into the episode log.
    Ingest {
        #[command(subcommand)]
        source: IngestCmd,
    },
    /// Create ~/.hearthloom (db, raw content dir, config). Asks nothing.
    Init,
    /// Standing queries checked by the consolidation cycle.
    Intent {
        #[command(subcommand)]
        command: IntentCmd,
    },
    /// API keys: mint, list, revoke.
    Key {
        #[command(subcommand)]
        command: KeyCmd,
    },
    /// Tail the daemon log (ANSI-stripped).
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Notes from the shell ($EDITOR flow).
    Note {
        #[command(subcommand)]
        command: NoteCmd,
    },
    /// Ring the dashboard bell (for scripts and cron jobs).
    Notify {
        title: String,
        body: Option<String>,
    },
    /// Hybrid peer sync: pairing status, cursors, last outcome.
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Review agent-proposed memories (the human gate).
    Quarantine {
        #[command(subcommand)]
        command: QuarantineCmd,
    },
    /// Deep query: an LLM iterates over the brain's tools and returns a
    /// cited synthesis (spends tokens — the one query-path exception).
    Query {
        question: Vec<String>,
        /// Show the tool-call transcript.
        #[arg(long)]
        verbose: bool,
    },
    /// Recall a day (--on), a span (--from/--to), or an entity's episodes.
    Recall {
        #[arg(long)]
        on: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        entity: Option<String>,
    },
    /// Correct a scope label (work|personal|family|health|private).
    Scope {
        /// Entity name (e.g. "orrery"), or a source with --by-source.
        target: String,
        scope: String,
        /// For Projects: also relabel episodes citing it as their repo.
        #[arg(long)]
        cascade: bool,
        /// Treat target as an episode source (e.g. chatlog-claude) and
        /// relabel all of its episodes.
        #[arg(long)]
        by_source: bool,
    },
    /// Full-text search over episodes.
    Search {
        query: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Virtual sources: data kept at origin, queried at need, promoted on demand.
    Source {
        #[command(subcommand)]
        command: SourceCmd,
    },
    /// One screen: version, record size, vault, thinking, peer.
    Status,
    /// Peer sync: status, or `--now` to force a cycle.
    Sync {
        /// Run a sync cycle right now.
        #[arg(long)]
        now: bool,
        /// With --now: a full (non-incremental) pass.
        #[arg(long)]
        full: bool,
    },
    /// Upgrade the hearthloom host binary to the latest official build.
    Upgrade {
        /// Report the available version; change nothing.
        #[arg(long)]
        check: bool,
    },
    /// Tasks from the shell (the Desk's quick capture).
    Task {
        #[command(subcommand)]
        command: TaskCmd,
    },
    /// The vault: status, unlock (minutes), lock.
    Vault {
        action: Option<String>,
        #[arg(long)]
        minutes: Option<u32>,
    },
    /// Operator secrets in ~/.hearthloom/.env(.sealed) — set reads the
    /// VALUE from stdin (the bootstrap.sh discipline: never argv, never
    /// shell history). The daemon picks changes up on its next restart.
    Secret {
        #[command(subcommand)]
        command: SecretCmd,
    },
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Set KEY to the value read from stdin (e.g. `printf '%s' "$V" | hearthloom secret set SENTRY_DSN`).
    Set { key: String },
    /// List configured secret NAMES (never values).
    List,
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Password sign-in; mints + stores a CLI key for this node.
    Login {
        #[arg(long)]
        email: Option<String>,
    },
    /// Can this shell reach the node?
    Status,
    /// Forget (and revoke) this node's CLI key.
    Logout,
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Add a task — #tags and @[mentions] work like the Desk.
    Add { description: Vec<String> },
}

#[derive(Subcommand)]
enum NoteCmd {
    /// New note in $EDITOR.
    New {
        #[arg(long)]
        title: Option<String>,
    },
    /// Recent notes.
    List,
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Mint a key (prints the secret ONCE).
    Mint {
        name: String,
        /// e.g. --scopes read,notify
        #[arg(long, value_delimiter = ',')]
        scopes: Vec<String>,
    },
    List,
    Revoke { id: String },
}

#[derive(Subcommand)]
enum EmberCmd {
    /// Search the ember registry (--registry or HEARTHLOOM_REGISTRY_URL).
    Search {
        query: Vec<String>,
        /// Another registry's URL (must serve /.well-known/hearth-registry).
        #[arg(long, env = "HEARTHLOOM_REGISTRY_URL")]
        registry: Option<String>,
        /// Only embers approved for hosted (cloud) nodes.
        #[arg(long)]
        cloud: bool,
    },
    /// An ember's registry page: versions, scopes, review status.
    Info {
        name: String,
        #[arg(long, env = "HEARTHLOOM_REGISTRY_URL")]
        registry: Option<String>,
    },
    /// Fetch, verify (sha256 + Ed25519), approve scopes, install.
    Install {
        name: String,
        /// A specific version (default: latest).
        #[arg(long)]
        version: Option<String>,
        #[arg(long, env = "HEARTHLOOM_REGISTRY_URL")]
        registry: Option<String>,
        /// Skip the confirmation prompt (scripting).
        #[arg(long)]
        yes: bool,
        /// If the ember needs a newer hearthloom, upgrade the host first.
        #[arg(long)]
        upgrade_host: bool,
    },
    /// Check installed embers against the registry; update outdated ones.
    Update {
        /// One ember (default: all installed).
        name: Option<String>,
        /// Report only; change nothing.
        #[arg(long)]
        check: bool,
        #[arg(long, env = "HEARTHLOOM_REGISTRY_URL")]
        registry: Option<String>,
        /// Skip prompts — EXCEPT when an update wants wider scopes.
        #[arg(long)]
        yes: bool,
    },
    /// Stop, revoke, and remove an ember (its data folder stays).
    Uninstall {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Lint a package dir with the daemon's real parser.
    Validate { path: PathBuf },
    /// Validate + zip a package for distribution.
    Pack {
        path: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Installed embers on the node.
    List,
    /// Tail an ember's sidecar log.
    Logs {
        name: String,
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Subcommand)]
enum SourceCmd {
    /// Register (or update) a virtual source mounted at a path.
    Register {
        /// Source name (e.g. "hearthfs").
        name: String,
        /// Mount path (a HearthFS mount, or any directory).
        #[arg(long)]
        mount: String,
        /// Adapter id; defaults to the source name.
        #[arg(long)]
        adapter: Option<String>,
        /// Trust tier for promoted episodes (imported|self|untrusted).
        #[arg(long, default_value = "imported")]
        trust: String,
        /// Treat the mount as a plain directory (no HearthFS magic paths).
        #[arg(long)]
        plain: bool,
    },
    /// Search a virtual source without materializing anything.
    Search {
        name: String,
        query: Vec<String>,
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// Promote a file from a virtual source into an episode (or quarantine).
    Promote {
        name: String,
        /// Origin-relative path within the mount.
        path: String,
        /// Episode kind (default "file").
        #[arg(long)]
        kind: Option<String>,
        /// Explicit scope; omit to infer.
        #[arg(long)]
        scope: Option<String>,
        /// Token-fit budget (HearthFS mounts only).
        #[arg(long)]
        budget: Option<u32>,
    },
}

#[derive(Subcommand)]
enum PeerCmd {
    /// Pairing status, cursors, and the last sync outcome.
    Status,
    /// Run one push+pull cycle right now.
    Sync {
        /// Re-offer EVERYTHING from the beginning (after re-pairing).
        #[arg(long)]
        full: bool,
    },
}

#[derive(Subcommand)]
enum QuarantineCmd {
    /// List pending proposals.
    List,
    /// Accept a proposal (it becomes an agent-trust episode).
    Accept { id: String },
    /// Reject a proposal.
    Reject { id: String },
}

#[derive(Subcommand)]
enum IntentCmd {
    /// Watch for future episodes matching an FTS query.
    Add { name: String, query: Vec<String> },
    /// List intents with unseen-match counts.
    List,
    /// Remove an intent and its recorded matches.
    Rm { name: String },
    /// Show an intent's matches (marks them seen unless --peek).
    Matches {
        name: String,
        #[arg(long)]
        peek: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground.
    Run,
    /// Check whether a daemon is up.
    Status,
}

#[derive(Subcommand)]
enum IngestCmd {
    /// Walk a git repository's history into episodes.
    Git {
        path: PathBuf,
        /// Explicit scope; omit to infer (known project > identity > personal).
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest an ICS calendar file.
    Ics {
        file: PathBuf,
        /// Explicit scope; omit to infer (known event > identity > personal).
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest an mbox mail archive (e.g. Gmail Takeout).
    Mbox {
        file: PathBuf,
        #[arg(long)]
        scope: Option<String>,
        /// Keep only messages carrying any of these X-Gmail-Labels
        /// (comma-separated, e.g. "Sent,Category Personal,Starred").
        /// With a filter set, the scan runs daemon-side in the background.
        #[arg(long, value_delimiter = ',')]
        include_labels: Vec<String>,
        /// Drop messages carrying any of these labels (Spam/Trash are
        /// always dropped either way).
        #[arg(long, value_delimiter = ',')]
        exclude_labels: Vec<String>,
    },
    /// Ingest a Claude.ai or ChatGPT conversations.json export.
    Chatlog {
        file: PathBuf,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest local Claude Code session transcripts.
    ClaudeCode {
        /// Projects directory (default ~/.claude/projects).
        dir: Option<PathBuf>,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest macOS iMessage history (chat.db; needs Full Disk Access).
    Imessage {
        /// Path to chat.db (default ~/Library/Messages/chat.db).
        db: Option<PathBuf>,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest Apple Photos metadata: timestamps, GPS, and place names —
    /// a location trace without decoding images (needs Full Disk Access).
    Photos {
        /// Photos library path (default ~/Pictures/Photos Library.photoslibrary).
        library: Option<PathBuf>,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest a Substack publication's public archive (subdomain or
    /// custom domain).
    Substack {
        /// Publication URL, e.g. meditations.example.com
        url: String,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest Chrome browsing history (all profiles; no permissions needed).
    Chrome {
        /// Chrome data dir or a single profile dir (default: all profiles).
        dir: Option<PathBuf>,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Ingest an extracted ChatGPT data export (the multi-chunk
    /// conversations-*.json directory). Runs daemon-side in the background —
    /// it's thousands of conversations plus image assets.
    Openai {
        /// The extracted export directory.
        dir: PathBuf,
        #[arg(long)]
        scope: Option<String>,
    },
}

#[derive(Subcommand)]
enum IdentityCmd {
    /// Claim an identity for an entity: "email jon@harbor.example is also me."
    /// If another entity owns it, that entity is merged in (reversibly).
    Claim {
        /// Attribute kind: email | phone | handle | repo-url …
        kind: String,
        value: String,
        /// The entity this identity belongs to.
        #[arg(long = "as")]
        entity: String,
        /// Context this identity carries; material ingested through it
        /// defaults to this scope (e.g. a company email is `work`).
        #[arg(long)]
        scope: Option<String>,
    },
    /// List pending "is this the same entity?" questions.
    Reviews,
    /// Answer a review question.
    Resolve {
        review_id: String,
        #[arg(long, conflicts_with = "distinct")]
        same: bool,
        #[arg(long)]
        distinct: bool,
    },
    /// Merge one entity into another directly (reversible).
    Merge {
        loser: String,
        #[arg(long)]
        into: String,
    },
    /// List recent merges.
    Merges,
    /// Undo a merge by id.
    Revert { merge_id: String },
}

// The daemon must keep serving WHILE consolidation grinds: on a
// 1-vCPU cloud machine the default runtime gets ONE worker and a
// boot-time backfill starves the accept loop into 502s (2026-07-18).
#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // The daemon narrates (inbox, consolidation) at info; one-shot CLI
    // commands stay quiet.
    let default_filter = if matches!(cli.command, Cmd::Daemon { .. }) {
        "info"
    } else {
        "warn"
    };
    // Sentry bridge attached unconditionally — inert unless the daemon
    // path (`hearthloom daemon run`, what launchd runs) later finds a
    // SENTRY_DSN and binds a client (hearthloom_daemon::apm).
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| default_filter.into()),
            )
            .with(tracing_subscriber::fmt::layer())
            .with(hearthloom_daemon::apm::tracing_layer())
            .init();
    }
    let home = Home::resolve()?;
    let url = cli.url.clone();
    let as_json = cli.json;

    match cli.command {
        Cmd::Init => {
            home.init()?;
            let suites = eval::scaffold(home.root())?;
            println!("initialized {}", home.root().display());
            for path in &suites {
                println!("  eval:   {} (example — replace with your own ground truth)", path.display());
            }
            println!("  db:     {}", home.db_path().display());
            println!("  raw:    {}", home.raw_dir().display());
            println!("  config: {}", home.config_dir().join("development.yaml").display());
            println!(
                "  settings: optional ones (registry, provisioner, HearthFS…) go in {} — see README → Configuration",
                home.root().join(".env").display()
            );
        }
        Cmd::Daemon { command } => match command {
            DaemonCmd::Run => hearthloom_daemon::run(home).await?,
            DaemonCmd::Status => {
                let client = Client::new(home)?;
                if client.daemon_running().await {
                    println!("daemon running at {}", client.base_url());
                } else {
                    println!("daemon not running (expected at {})", client.base_url());
                }
            }
        },
        Cmd::Ingest { source } => {
            // TCC-gated sources: if this process can't read them, the
            // daemon (which can hold its own Full Disk Access grant) tries.
            if let Some((adapter, path, scope)) = match &source {
                IngestCmd::Imessage { db, scope } => {
                    Some(("imessage", db.clone(), scope.clone()))
                }
                IngestCmd::Photos { library, scope } => {
                    Some(("photos", library.clone(), scope.clone()))
                }
                _ => None,
            } {
                let client = Client::new(home)?;
                return ingest_tcc_source(&client, adapter, path, scope).await;
            }
            // Filtered mbox runs go daemon-side: a Takeout archive can be
            // tens of GB, so the daemon scans it detached and reports
            // through daemon.log instead of holding this process open.
            if let IngestCmd::Mbox {
                file,
                scope,
                include_labels,
                exclude_labels,
            } = &source
            {
                if !include_labels.is_empty() || !exclude_labels.is_empty() {
                    let client = Client::new(home)?;
                    let result = client
                        .post(
                            "/v1/ingest/local",
                            &json!({
                                "actor": "human:cli",
                                "adapter": "mbox",
                                "path": file.display().to_string(),
                                "scope": scope,
                                "include_labels": include_labels,
                                "exclude_labels": exclude_labels,
                            }),
                        )
                        .await?;
                    println!(
                        "{}",
                        result["note"].as_str().unwrap_or("started daemon-side")
                    );
                    return Ok(());
                }
            }
            // ChatGPT exports run daemon-side: thousands of conversations plus
            // image assets the daemon copies into its raw/ tree.
            if let IngestCmd::Openai { dir, scope } = &source {
                let client = Client::new(home)?;
                let result = client
                    .post(
                        "/v1/ingest/local",
                        &json!({
                            "actor": "human:cli",
                            "adapter": "openai",
                            "path": dir.display().to_string(),
                            "scope": scope,
                        }),
                    )
                    .await?;
                println!(
                    "{}",
                    result["note"].as_str().unwrap_or("started daemon-side")
                );
                return Ok(());
            }
            let (drafts, actor, label) = match source {
                IngestCmd::Git { path, scope } => (
                    hearthloom_adapters::git::ingest(&path, scope.as_deref())?,
                    "adapter:git",
                    path.display().to_string(),
                ),
                IngestCmd::Ics { file, scope } => (
                    hearthloom_adapters::ics::ingest(&file, scope.as_deref())?,
                    "adapter:ics",
                    file.display().to_string(),
                ),
                IngestCmd::Mbox { file, scope, .. } => (
                    hearthloom_adapters::mbox::ingest(&file, scope.as_deref())?,
                    "adapter:mbox",
                    file.display().to_string(),
                ),
                IngestCmd::Chatlog { file, scope } => (
                    hearthloom_adapters::chatlog::ingest(&file, scope.as_deref())?,
                    "adapter:chatlog",
                    file.display().to_string(),
                ),
                IngestCmd::ClaudeCode { dir, scope } => {
                    let dir = dir.unwrap_or_else(|| {
                        PathBuf::from(std::env::var("HOME").unwrap_or_default())
                            .join(".claude/projects")
                    });
                    (
                        hearthloom_adapters::claude_code::ingest(&dir, scope.as_deref())?,
                        "adapter:claude-code",
                        dir.display().to_string(),
                    )
                }
                IngestCmd::Imessage { db, scope } => {
                    let db = db.unwrap_or_else(|| {
                        PathBuf::from(std::env::var("HOME").unwrap_or_default())
                            .join("Library/Messages/chat.db")
                    });
                    (
                        hearthloom_adapters::imessage::ingest(&db, scope.as_deref())?,
                        "adapter:imessage",
                        db.display().to_string(),
                    )
                }
                IngestCmd::Photos { library, scope } => {
                    let library = match library {
                        Some(library) => library,
                        None => hearthloom_adapters::photos::default_library()?,
                    };
                    (
                        hearthloom_adapters::photos::ingest(&library, scope.as_deref())?,
                        "adapter:photos",
                        library.display().to_string(),
                    )
                }
                IngestCmd::Substack { url, scope } => (
                    hearthloom_adapters::substack::ingest(&url, scope.as_deref())?,
                    "adapter:substack",
                    url,
                ),
                IngestCmd::Chrome { dir, scope } => {
                    let dir = match dir {
                        Some(dir) => dir,
                        None => hearthloom_adapters::chrome::default_data_dir()?,
                    };
                    (
                        hearthloom_adapters::chrome::ingest(&dir, scope.as_deref())?,
                        "adapter:chrome",
                        dir.display().to_string(),
                    )
                }
                // Intercepted and routed daemon-side above.
                IngestCmd::Openai { .. } => unreachable!("openai runs daemon-side"),
            };
            println!("parsed {} episodes from {label}", drafts.len());
            let client = Client::new(home)?;
            let mut totals = (0usize, 0usize, 0usize, 0usize);
            // Chunk by payload size, not draft count: bodies vary from a
            // one-line commit to a 64KB transcript.
            const CHUNK_BYTES: usize = 8 * 1024 * 1024;
            let mut chunks: Vec<Vec<&hearthloom_core::EpisodeDraft>> = vec![vec![]];
            let mut current_bytes = 0usize;
            for draft in &drafts {
                let size = draft.body.len() + 512;
                if current_bytes + size > CHUNK_BYTES && !chunks.last().unwrap().is_empty() {
                    chunks.push(vec![]);
                    current_bytes = 0;
                }
                chunks.last_mut().unwrap().push(draft);
                current_bytes += size;
            }
            for chunk in chunks {
                let batch = json!({
                    "actor": actor,
                    "episodes": chunk,
                });
                let report = client.post("/v1/ingest", &batch).await?;
                totals.0 += report["episodes_new"].as_u64().unwrap_or(0) as usize;
                totals.1 += report["episodes_existing"].as_u64().unwrap_or(0) as usize;
                totals.2 += report["entities_created"].as_u64().unwrap_or(0) as usize;
                totals.3 += report["edges_created"].as_u64().unwrap_or(0) as usize;
            }
            println!(
                "ingested: {} new episodes ({} already present), {} new entities, {} new edges",
                totals.0, totals.1, totals.2, totals.3
            );
        }
        Cmd::Search { query, limit } => {
            let query = query.join(" ");
            let client = Client::new(home)?;
            let result = client
                .get("/v1/search", &[("q", query.clone()), ("limit", limit.to_string())])
                .await?;
            let hits = result["hits"].as_array().cloned().unwrap_or_default();
            if hits.is_empty() {
                println!("no results for {query:?}");
            }
            for hit in hits {
                println!(
                    "{}  [{}/{}]  {}",
                    hit["event_time"].as_str().map(|t| &t[..10]).unwrap_or("????-??-??"),
                    hit["source"].as_str().unwrap_or("?"),
                    hit["kind"].as_str().unwrap_or("?"),
                    hit["title"].as_str().unwrap_or(""),
                );
                if let Some(snippet) = hit["snippet"].as_str() {
                    println!("    {}", snippet.replace('\n', " "));
                }
                println!("    id: {}", hit["episode_id"].as_str().unwrap_or(""));
            }
        }
        Cmd::Recall { on, from, to, entity } => {
            let client = Client::new(home)?;
            let mut query: Vec<(&str, String)> = Vec::new();
            if let Some(on) = on {
                query.push(("on", on));
            }
            if let (Some(from), Some(to)) = (from, to) {
                query.push(("from", from));
                query.push(("to", to));
            }
            if let Some(entity) = entity {
                query.push(("entity", entity));
            }
            if query.is_empty() {
                anyhow::bail!("recall needs --on DATE, --from/--to, or --entity NAME");
            }
            // The terminal printer is already compact; ask for full rows.
            query.push(("detail", "full".into()));
            let result = client.get("/v1/recall", &query).await?;
            print_recall(&result);
        }
        Cmd::Entity { name } => {
            let name = name.join(" ");
            let client = Client::new(home)?;
            let dossier = client
                .get(&format!("/v1/entity/{}", urlencode(&name)), &[])
                .await?;
            print_dossier(&dossier);
        }
        Cmd::Identity { command } => {
            let client = Client::new(home)?;
            handle_identity(&client, command).await?;
        }
        Cmd::Scope { target, scope, cascade, by_source } => {
            let client = Client::new(home)?;
            let mut body = json!({
                "actor": "human:cli",
                "scope": scope,
                "cascade": cascade,
            });
            body[if by_source { "source" } else { "entity" }] = json!(target);
            let result = client.post("/v1/scope", &body).await?;
            println!(
                "{} is now scope {} ({} episodes relabeled)",
                result["target"].as_str().unwrap_or("?"),
                result["scope"].as_str().unwrap_or("?"),
                result["episodes_relabeled"]
            );
        }
        Cmd::Index => {
            let client = Client::new(home)?;
            let result = client.post("/v1/index", &json!({})).await?;
            println!(
                "wrote {} ({} lines, {} bytes)",
                result["path"].as_str().unwrap_or("?"),
                result["lines"],
                result["bytes"]
            );
        }
        Cmd::Peer { cmd } => {
            let client = Client::new(home)?;
            let result = match cmd {
                PeerCmd::Status => client.get("/v1/peer", &[]).await?,
                PeerCmd::Sync { full } => {
                    client
                        .post("/v1/peer/sync-now", &serde_json::json!({"actor": "human:cli", "full": full}))
                        .await?
                }
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Events { since, limit } => {
            let client = Client::new(home)?;
            let result = client
                .get(
                    "/v1/events",
                    &[("since", since.to_string()), ("limit", limit.to_string())],
                )
                .await?;
            for event in result["events"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}  {}  {}  {}",
                    event["seq"],
                    event["ts"].as_str().unwrap_or(""),
                    event["actor"].as_str().unwrap_or(""),
                    event["kind"].as_str().unwrap_or(""),
                );
            }
            println!("next cursor: {}", result["next_cursor"]);
        }
        Cmd::Eval { file, ask, suite, only } => {
            let name = suite.unwrap_or_else(|| if ask { "ask-eval" } else { "personal-eval" }.to_string());
            let path = eval::resolve(home.root(), &name, file)?;
            let client = Client::new(home)?;
            if ask {
                eval::run_ask(&client, &path, &only).await?;
            } else {
                eval::run(&client, &path).await?;
            }
        }
        Cmd::Bench { iterations } => {
            let client = Client::new(home)?;
            bench::run(&client, iterations).await?;
        }
        Cmd::Query { question, verbose } => {
            let question = question.join(" ");
            if question.trim().is_empty() {
                anyhow::bail!("usage: hearthloom query <question>");
            }
            let client = Client::new(home)?;
            eprintln!("(thinking across the brain — this spends tokens…)");
            let result = client
                .post("/v1/query", &json!({"actor": "human:cli", "question": question}))
                .await?;
            if verbose {
                for step in result["transcript"].as_array().cloned().unwrap_or_default() {
                    eprintln!(
                        "  round {}: {} {} — {}",
                        step["round"],
                        step["tool"].as_str().unwrap_or("?"),
                        step["args"],
                        step["why"].as_str().unwrap_or("")
                    );
                }
            }
            println!("{}", result["answer"].as_str().unwrap_or(""));
            let citations = result["citations"].as_array().cloned().unwrap_or_default();
            if !citations.is_empty() {
                println!(
                    "\ncitations: {}",
                    citations
                        .iter()
                        .filter_map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            eprintln!("({} tool rounds)", result["rounds"]);
        }
        Cmd::Quarantine { command } => {
            let client = Client::new(home)?;
            match command {
                QuarantineCmd::List => {
                    let result = client.get("/v1/quarantine", &[]).await?;
                    let pending = result["pending"].as_array().cloned().unwrap_or_default();
                    if pending.is_empty() {
                        println!("no pending proposals.");
                    }
                    for entry in pending {
                        println!(
                            "{}  {}  [{}]  {}",
                            entry["id"].as_str().unwrap_or("?"),
                            entry["proposed_at"].as_str().unwrap_or(""),
                            entry["actor"].as_str().unwrap_or("?"),
                            entry["content"].to_string().chars().take(100).collect::<String>(),
                        );
                    }
                }
                ref resolved @ (QuarantineCmd::Accept { ref id }
                | QuarantineCmd::Reject { ref id }) => {
                    let accept = matches!(resolved, QuarantineCmd::Accept { .. });
                    let result = client
                        .post(
                            &format!("/v1/quarantine/{id}"),
                            &json!({
                                "actor": "human:cli",
                                "resolution": if accept { "accept" } else { "reject" },
                            }),
                        )
                        .await?;
                    match result["episode_id"].as_str() {
                        Some(episode_id) => {
                            println!("accepted → episode {episode_id}")
                        }
                        None => println!("{}ed", if accept { "accept" } else { "reject" }),
                    }
                }
            }
        }
        Cmd::Source { command } => {
            let client = Client::new(home)?;
            client.ensure_daemon().await?;
            handle_source(&client, command).await?;
        }
        Cmd::Intent { command } => {
            let client = Client::new(home)?;
            handle_intent(&client, command).await?;
        }
        Cmd::Consolidate => {
            let client = Client::new(home)?;
            let summary = client.post("/v1/consolidate", &json!({})).await?;
            println!(
                "embedded {} chunks (coverage {}/{})",
                summary["chunks_embedded"],
                summary["embedding_coverage"]["embedded"],
                summary["embedding_coverage"]["total"],
            );
            for fired in summary["intents_fired"].as_array().cloned().unwrap_or_default() {
                println!(
                    "intent {} fired: {} new matches",
                    fired["intent"].as_str().unwrap_or("?"),
                    fired["new_matches"]
                );
            }
            match (
                summary["llm_configured"].as_bool().unwrap_or(false),
                summary["semantic_summary_written"].as_bool().unwrap_or(false),
            ) {
                (false, _) => println!("semantic digest: no LLM provider configured"),
                (true, true) => println!("semantic digest: written"),
                (true, false) => println!("semantic digest: up to date"),
            }
            println!(
                "BRAIN.md {}",
                if summary["brain_index_updated"].as_bool().unwrap_or(false) {
                    "updated"
                } else {
                    "unchanged"
                }
            );
        }
        Cmd::Auth { command } => match command {
            AuthCmd::Login { email } => {
                let base = match url.clone() {
                    Some(url) => url,
                    // this machine's daemon, wherever its config puts it
                    None => Client::with_url(home.clone(), None)?.base_url().to_string(),
                };
                ops::auth_login(base.trim_end_matches('/'), email).await?;
            }
            AuthCmd::Status => {
                let client = Client::with_url(home, url.clone())?;
                ops::auth_status(&client).await?;
            }
            AuthCmd::Logout => {
                let client = Client::with_url(home, url.clone())?;
                ops::auth_logout(&client).await?;
            }
        },
        Cmd::Status => {
            let client = Client::with_url(home, url.clone())?;
            ops::status(&client, as_json).await?;
        }
        Cmd::Doctor => {
            let client = Client::with_url(home, url.clone())?;
            ops::doctor(&client).await?;
        }
        Cmd::Task { command } => match command {
            TaskCmd::Add { description } => {
                let client = Client::with_url(home, url.clone())?;
                ops::task_add(&client, &description.join(" ")).await?;
            }
        },
        Cmd::Note { command } => {
            let client = Client::with_url(home, url.clone())?;
            match command {
                NoteCmd::New { title } => ops::note_new(&client, title).await?,
                NoteCmd::List => ops::note_list(&client).await?,
            }
        }
        Cmd::Key { command } => {
            let client = Client::with_url(home, url.clone())?;
            match command {
                KeyCmd::Mint { name, scopes } => ops::key_mint(&client, &name, &scopes).await?,
                KeyCmd::List => ops::key_list(&client, as_json).await?,
                KeyCmd::Revoke { id } => ops::key_revoke(&client, &id).await?,
            }
        }
        Cmd::Vault { action, minutes } => {
            let client = Client::with_url(home, url.clone())?;
            ops::vault(&client, action.as_deref(), minutes).await?;
        }
        Cmd::Secret { command } => match command {
            SecretCmd::Set { key } => ops::secret_set(&home, &key)?,
            SecretCmd::List => ops::secret_list(&home)?,
        },
        Cmd::Export { out } => {
            let client = Client::with_url(home, url.clone())?;
            ops::export(&client, out).await?;
        }
        Cmd::Sync { now, full } => {
            let client = Client::with_url(home, url.clone())?;
            ops::sync(&client, now, full).await?;
        }
        Cmd::Upgrade { check } => {
            let client = Client::with_url(home, url.clone())?;
            ops::upgrade(&client, check).await?;
        }
        Cmd::Ember { command } => match command {
            EmberCmd::Search { query, registry, cloud } => {
                let reg = registry::Registry::connect(registry).await?;
                reg.search(&query.join(" "), cloud).await?;
            }
            EmberCmd::Info { name, registry } => {
                let reg = registry::Registry::connect(registry).await?;
                reg.info(&name).await?;
            }
            EmberCmd::Install { name, version, registry, yes, upgrade_host } => {
                let client = Client::with_url(home, url.clone())?;
                let reg = registry::Registry::connect(registry).await?;
                reg.install(&client, &name, version, yes, upgrade_host).await?;
            }
            EmberCmd::Update { name, check, registry, yes } => {
                let client = Client::with_url(home, url.clone())?;
                let reg = registry::Registry::connect(registry).await?;
                reg.update(&client, name, check, yes).await?;
            }
            EmberCmd::Uninstall { name, yes } => {
                let client = Client::with_url(home, url.clone())?;
                registry::uninstall(&client, &name, yes).await?;
            }
            EmberCmd::Validate { path } => ops::ember_validate(&path)?,
            EmberCmd::Pack { path, out } => ops::ember_pack(&path, out)?,
            EmberCmd::List => {
                let client = Client::with_url(home, url.clone())?;
                ops::ember_list(&client).await?;
            }
            EmberCmd::Logs { name, follow } => ops::ember_logs(&name, follow)?,
        },
        Cmd::Notify { title, body } => {
            let client = Client::with_url(home, url.clone())?;
            ops::notify(&client, &title, body).await?;
        }
        Cmd::Logs { follow } => ops::logs(follow)?,
        Cmd::Completions { shell } => {
            use clap::CommandFactory;
            let shell: clap_complete::Shell = shell
                .parse()
                .map_err(|_| anyhow::anyhow!("shell must be bash|zsh|fish"))?;
            clap_complete::generate(shell, &mut Cli::command(), "hearthloom", &mut std::io::stdout());
        }
    }
    Ok(())
}

async fn handle_source(client: &Client, command: SourceCmd) -> Result<()> {
    match command {
        SourceCmd::Register {
            name,
            mount,
            adapter,
            trust,
            plain,
        } => {
            let adapter = adapter.unwrap_or_else(|| name.clone());
            let result = client
                .post(
                    "/v1/sources/virtual",
                    &json!({
                        "actor": "human:cli",
                        "name": name,
                        "adapter": adapter,
                        "config": {"mount": mount, "magic": !plain, "trust": trust},
                    }),
                )
                .await?;
            println!(
                "registered virtual source {} [{}] — handles: {}",
                result["registered"].as_str().unwrap_or(&name),
                result["adapter"].as_str().unwrap_or(&adapter),
                &result["capabilities"],
            );
        }
        SourceCmd::Search { name, query, limit } => {
            let query = query.join(" ");
            let result = client
                .get(
                    &format!("/v1/sources/virtual/{}/search", urlencode(&name)),
                    &[("q", query.clone()), ("limit", limit.to_string())],
                )
                .await?;
            let hits = result["hits"].as_array().cloned().unwrap_or_default();
            if hits.is_empty() {
                println!("no matches in {name} for {query:?}.");
            }
            for hit in hits {
                println!(
                    "{}  {}",
                    hit["path"].as_str().unwrap_or("?"),
                    hit["snippet"].as_str().unwrap_or(""),
                );
            }
        }
        SourceCmd::Promote {
            name,
            path,
            kind,
            scope,
            budget,
        } => {
            let mut body = json!({"actor": "human:cli", "path": path});
            if let Some(kind) = kind {
                body["kind"] = json!(kind);
            }
            if let Some(scope) = scope {
                body["scope"] = json!(scope);
            }
            if let Some(budget) = budget {
                body["budget"] = json!(budget);
            }
            let result = client
                .post(
                    &format!("/v1/sources/virtual/{}/promote", urlencode(&name)),
                    &body,
                )
                .await?;
            match (
                result["episode"].as_str(),
                result["quarantined"].as_str(),
                result["existing"].as_bool().unwrap_or(false),
            ) {
                (Some(id), _, true) => println!("already materialized → episode {id}"),
                (Some(id), _, false) => println!("promoted → episode {id}"),
                (None, Some(id), _) => println!("untrusted → quarantined {id} (review to accept)"),
                _ => println!("{result}"),
            }
        }
    }
    Ok(())
}

async fn handle_intent(client: &Client, command: IntentCmd) -> Result<()> {
    match command {
        IntentCmd::Add { name, query } => {
            let query = query.join(" ");
            client
                .post(
                    "/v1/intents",
                    &json!({"actor": "human:cli", "name": name, "query": query}),
                )
                .await?;
            println!("watching for {query:?} as intent {name:?}");
        }
        IntentCmd::List => {
            let result = client.get("/v1/intents", &[]).await?;
            let intents = result["intents"].as_array().cloned().unwrap_or_default();
            if intents.is_empty() {
                println!("no intents. add one: hearthloom intent add <name> <query>");
            }
            for intent in intents {
                println!(
                    "{}  {:?}  ({} unseen)  last checked {}",
                    intent["name"].as_str().unwrap_or("?"),
                    intent["query"].as_str().unwrap_or("?"),
                    intent["unseen_matches"],
                    intent["last_checked"].as_str().unwrap_or("never"),
                );
            }
        }
        IntentCmd::Rm { name } => {
            client
                .delete(&format!("/v1/intents/{}", urlencode(&name)), &[("actor", "human:cli".into())])
                .await?;
            println!("removed intent {name:?}");
        }
        IntentCmd::Matches { name, peek } => {
            let result = client
                .get(
                    &format!("/v1/intents/{}/matches", urlencode(&name)),
                    &[("mark_seen", (!peek).to_string())],
                )
                .await?;
            let matches = result["matches"].as_array().cloned().unwrap_or_default();
            if matches.is_empty() {
                println!("no matches yet.");
            }
            for m in matches {
                let episode = &m["episode"];
                println!(
                    "{}  [{}/{}]  {}{}",
                    episode["event_time"].as_str().map(|t| &t[..10]).unwrap_or("????-??-??"),
                    episode["source"].as_str().unwrap_or("?"),
                    episode["kind"].as_str().unwrap_or("?"),
                    episode["title"].as_str().unwrap_or(""),
                    if m["seen"].as_bool().unwrap_or(false) { "" } else { "  [new]" },
                );
            }
        }
    }
    Ok(())
}

/// Ingest a TCC-protected source: parse locally when this shell has
/// access, otherwise ask the daemon to do it server-side.
async fn ingest_tcc_source(
    client: &Client,
    adapter: &str,
    path: Option<PathBuf>,
    scope: Option<String>,
) -> Result<()> {
    let local = match adapter {
        "imessage" => {
            let db = path.clone().unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join("Library/Messages/chat.db")
            });
            hearthloom_adapters::imessage::ingest(&db, scope.as_deref())
        }
        "photos" => {
            let library = match path.clone() {
                Some(library) => library,
                None => hearthloom_adapters::photos::default_library()?,
            };
            hearthloom_adapters::photos::ingest(&library, scope.as_deref())
        }
        other => anyhow::bail!("not a TCC adapter: {other}"),
    };
    match local {
        Ok(drafts) => {
            println!("parsed {} episodes locally", drafts.len());
            let mut totals = (0u64, 0u64);
            for chunk in drafts.chunks(500) {
                let report = client
                    .post(
                        "/v1/ingest",
                        &json!({"actor": format!("adapter:{adapter}"), "episodes": chunk}),
                    )
                    .await?;
                totals.0 += report["episodes_new"].as_u64().unwrap_or(0);
                totals.1 += report["episodes_existing"].as_u64().unwrap_or(0);
            }
            println!(
                "ingested: {} new episodes ({} already present)",
                totals.0, totals.1
            );
            Ok(())
        }
        Err(err) => {
            let message = format!("{err:#}");
            let tcc = message.contains("authorization denied")
                || message.contains("Operation not permitted")
                || message.contains("unable to open");
            if !tcc {
                return Err(err);
            }
            eprintln!("(this shell lacks disk access — asking the daemon to ingest instead)");
            let result = client
                .post(
                    "/v1/ingest/local",
                    &json!({
                        "actor": "human:cli",
                        "adapter": adapter,
                        "path": path.map(|p| p.display().to_string()),
                        "scope": scope,
                    }),
                )
                .await?;
            println!(
                "daemon ingested {}: {} parsed, {} new episodes ({} already present)",
                adapter, result["parsed"], result["episodes_new"], result["episodes_existing"]
            );
            Ok(())
        }
    }
}

async fn handle_identity(client: &Client, command: IdentityCmd) -> Result<()> {
    match command {
        IdentityCmd::Claim { kind, value, entity, scope } => {
            let result = client
                .post(
                    "/v1/identity/claim",
                    &json!({
                        "actor": "human:cli",
                        "entity": entity,
                        "kind": kind,
                        "value": value,
                        "scope": scope,
                    }),
                )
                .await?;
            match result["outcome"].as_str() {
                Some("attached") => println!("attached {kind}={value} to {entity}"),
                Some("already_yours") => println!("{kind}={value} already resolves to {entity}"),
                Some("merged_from") => println!(
                    "{kind}={value} belonged to {:?} — merged into {entity} (merge {}; revert with `hearthloom identity revert`)",
                    result["merged_entity"]["name"].as_str().unwrap_or("?"),
                    result["merge_id"].as_str().unwrap_or("?"),
                ),
                _ => println!("{result}"),
            }
        }
        IdentityCmd::Reviews => {
            let result = client.get("/v1/reviews", &[]).await?;
            let pending = result["pending"].as_array().cloned().unwrap_or_default();
            if pending.is_empty() {
                println!("no pending reviews.");
            }
            for item in pending {
                println!(
                    "{}  {} ({})  ≟  {} ({})  — {}",
                    item["id"].as_str().unwrap_or("?"),
                    item["entity_a"]["name"].as_str().unwrap_or("?"),
                    item["entity_a"]["entity_type"].as_str().unwrap_or("?"),
                    item["entity_b"]["name"].as_str().unwrap_or("?"),
                    item["entity_b"]["entity_type"].as_str().unwrap_or("?"),
                    item["reason"].as_str().unwrap_or(""),
                );
            }
        }
        IdentityCmd::Resolve { review_id, same, distinct } => {
            if same == distinct {
                anyhow::bail!("pass exactly one of --same or --distinct");
            }
            let result = client
                .post(
                    &format!("/v1/reviews/{review_id}"),
                    &json!({
                        "actor": "human:cli",
                        "resolution": if same { "same" } else { "distinct" },
                    }),
                )
                .await?;
            if let Some(merge_id) = result["merge_id"].as_str() {
                println!("merged (merge {merge_id})");
            } else {
                println!("marked {}", if same { "same" } else { "distinct" });
            }
        }
        IdentityCmd::Merge { loser, into } => {
            // Direct merge is a claim in disguise: fetch ids via dossiers.
            let winner = client
                .get(&format!("/v1/entity/{}", urlencode(&into)), &[])
                .await?;
            let loser_dossier = client
                .get(&format!("/v1/entity/{}", urlencode(&loser)), &[])
                .await?;
            let result = client
                .post(
                    "/v1/identity/merge",
                    &json!({
                        "actor": "human:cli",
                        "winner": winner["entity"]["id"],
                        "loser": loser_dossier["entity"]["id"],
                    }),
                )
                .await?;
            println!(
                "merged {loser} into {into} (merge {})",
                result["merge_id"].as_str().unwrap_or("?")
            );
        }
        IdentityCmd::Merges => {
            let result = client.get("/v1/merges", &[]).await?;
            for merge in result["merges"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}  {}  {} ← {}  {}{}",
                    merge["id"].as_str().unwrap_or("?"),
                    merge["merged_at"].as_str().unwrap_or(""),
                    merge["winner"]["name"].as_str().unwrap_or("?"),
                    merge["loser_name"].as_str().unwrap_or("?"),
                    merge["reason"].as_str().unwrap_or(""),
                    if merge["reverted_at"].is_string() { "  [reverted]" } else { "" },
                );
            }
        }
        IdentityCmd::Revert { merge_id } => {
            client
                .post(
                    &format!("/v1/merges/{merge_id}/revert"),
                    &json!({"actor": "human:cli"}),
                )
                .await?;
            println!("reverted merge {merge_id}");
        }
    }
    Ok(())
}

fn print_recall(result: &Value) {
    if let Some(episodes) = result["episodes"].as_array() {
        // Day view (objects with nested entities) or plain episode lists.
        for episode in episodes {
            let title = episode["title"].as_str().unwrap_or("");
            let time = episode["event_time"].as_str().unwrap_or("");
            let source = episode["source"].as_str().unwrap_or("?");
            let kind = episode["kind"].as_str().unwrap_or("?");
            println!("{time}  [{source}/{kind}]  {title}");
            if let Some(entities) = episode["entities"].as_array() {
                for entity in entities {
                    if let (Some(name), Some(etype), Some(role)) = (
                        entity[0].as_str(),
                        entity[1].as_str(),
                        entity[2].as_str(),
                    ) {
                        println!("    {role}: {name} ({etype})");
                    }
                }
            }
            println!("    id: {}", episode["id"].as_str().unwrap_or(""));
        }
        if episodes.is_empty() {
            println!("no episodes.");
        }
    }
    if let Some(edges) = result["valid_edges"].as_array() {
        if !edges.is_empty() {
            println!("\nedges valid this day:");
            for edge in edges {
                println!(
                    "  {} —{}→ {}",
                    edge["src"]["name"].as_str().unwrap_or("?"),
                    edge["relation"].as_str().unwrap_or("?"),
                    edge["dst"]["name"].as_str().unwrap_or("?"),
                );
            }
        }
    }
}

fn print_dossier(dossier: &Value) {
    println!(
        "{} ({}, scope: {})",
        dossier["entity"]["name"].as_str().unwrap_or("?"),
        dossier["entity"]["entity_type"].as_str().unwrap_or("?"),
        dossier["entity"]["scope"].as_str().unwrap_or("?"),
    );
    if let Some(attributes) = dossier["attributes"].as_array() {
        for attribute in attributes {
            if let (Some(kind), Some(value)) = (attribute[0].as_str(), attribute[1].as_str()) {
                println!("  {kind}: {value}");
            }
        }
    }
    if let Some(edges) = dossier["edges"].as_array() {
        if !edges.is_empty() {
            println!("\nrelations:");
        }
        for edge in edges {
            let arrow = if edge["direction"] == "out" { "→" } else { "←" };
            println!(
                "  {arrow} {} {} ({} citing episodes)",
                edge["relation"].as_str().unwrap_or("?"),
                edge["other"]["name"].as_str().unwrap_or("?"),
                edge["provenance"].as_array().map(Vec::len).unwrap_or(0),
            );
        }
    }
    println!(
        "\ncited by {} episodes; recent:",
        dossier["episode_count"]
    );
    for episode in dossier["recent_episodes"].as_array().cloned().unwrap_or_default() {
        println!(
            "  {}  [{}]  {}  ({})",
            episode[3].as_str().map(|t| &t[..10]).unwrap_or("????-??-??"),
            episode[1].as_str().unwrap_or("?"),
            episode[2].as_str().unwrap_or(""),
            episode[0].as_str().unwrap_or(""),
        );
    }
}

/// Percent-encode a path segment (entity names may contain spaces).
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

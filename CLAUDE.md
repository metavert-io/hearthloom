**Need the whole system in your head?** Read `architecture.md` (repo
root) first: the parts, the data model, how data flows, and which rules
hold it together. This file stays the edit-time rulebook (invariants,
deploy, gotchas) — put system description there, not here, and update
`architecture.md` when a change alters the shape it describes.

# Hearthloom

A local-first personal second brain: an episode-sourced, bi-temporal knowledge
graph in one SQLite file on the user's disk, with an embedded dashboard.
`architecture.md` describes the system; this file is the edit-time
rulebook. If a `CLAUDE.local.md` exists next to this file, read it too: it
holds the maintainer's local deploy settings and points to local work
notes (design log cited below by date, plans, research), which are
not part of the repository.

## Build checks (run all three before calling anything done)

```
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

Deploying: `cargo build --release && install -m 755 target/release/hearthloom
~/.local/bin/hearthloom && launchctl kickstart -k "gui/$(id -u)/com.hearthloom.daemon"`.
The daemon runs under launchd on the developer's machine — deploy after changes or
the running daemon won't have them. **Do not kickstart while a consolidation
or ingest is in flight** (check daemon.log); it kills the request.
**Deploy footguns (2026-07-27):** (1) a stale/orphan daemon can keep serving
OLD embedded assets through several kickstarts — if a CSS/JS change "won't
take," kill the `:43278` port-holder directly (`lsof -nP -iTCP:43278 -sTCP:LISTEN`
→ `kill -9`) THEN kickstart, and confirm the served asset changed
(`curl -s localhost:43278/assets/app.js | grep <marker>`). (2) NEVER bump a
version with `open("Cargo.toml","w").write(open("Cargo.toml").read()...)` — the
`"w"` truncates the file BEFORE the read runs and wipes Cargo.toml; use the
two-statement read-then-write form (or `sed -i ''`).
**Wedged-daemon trap (2026-07-14):** if `/health` times out (HTTP 000) and
daemon.log has stopped growing, look for ORPHAN daemons contending for :43278 —
`hearthloom` CLI/build commands auto-spawn a daemon when they can't reach one,
so a hung launchd process leaves `target/debug` and `target/release` daemons
fighting the port. Fix: `pkill -f "target/.*hearthloom daemon"` then
`launchctl kickstart -k`, and confirm exactly ONE survivor with
`pgrep -fl "hearthloom daemon"` (only the `~/.local/bin` one is launchd-managed).
**The deployed artifact is `hearthloom` (the CLI) — launchd runs
`hearthloom daemon run`.** There is ALSO a standalone `hearthloom-daemon`
binary; installing/kickstarting THAT changes nothing (launchd never runs
it) and your fix silently "doesn't take" — a full debugging detour on
2026-07-11. Verify with `launchctl print gui/$(id -u)/com.hearthloom.daemon
| grep program`. Dashboard assets now send `Cache-Control: no-cache`
(dashboard.rs), so a normal browser reload picks up a deploy — before that,
stale cached JS made fixes "not show" (clicks did nothing).

Verify UI changes with Playwright driving installed Chrome
(`chromium.launch({ channel: "chrome", headless: true })` from a /tmp
workspace with `npm i playwright`) — screenshot + pageerror check.

## Design laws (non-negotiable)

1. **User owns the substrate.** One SQLite file (`~/.hearthloom/hearthloom.db`)
   + plain files under `~/.hearthloom/raw/`; every index rebuildable from the
   episode log; no cloud dependency to function (three.js is vendored; no CDN).
2. **Zero gardening.** No feature may depend on the user maintaining links,
   tags, or merges. The one exception: yes/no prompts on the Tend page
   (merge reviews, quarantined memories).
3. **Retrieval is the product.** LLM tokens are spent at ingest and idle time,
   never on the hot query path. FTS and graph traversal are always fully local.
4. **The daemon is the sole writer.** No process opens the SQLite file except
   `hearthloom-daemon` — the CLI, adapters, agents, and the dashboard go
   through its HTTP API on loopback (127.0.0.1:43278). Every write carries
   actor identity (`human:… | agent:… | adapter:…` + session id) and lands as
   an append-only event with an idempotency key in the `mutations` log.
5. **Provenance everywhere.** Entities/edges back-point to supporting episodes;
   episodes carry source, trust tier, event time (with tz offset AND
   uncertainty — never optional), ingest time, and scope label
   (`work|personal|family|health|private`).

## Layout

- `crates/hearthloom-core` — schema (migrations via `PRAGMA user_version`,
  **currently v60**: v60 the movement layer (`movement_observation`
  with `subject` me|vehicle:<ember>:<id>, `vehicle_profile` +
  `trip_attribution` synced LWW via /v1/sync/vehicles, movements via
  /v1/sync/movements, node-local `trip_day`; daemon trips.rs decides
  whose drive each car drive was — see the TRIPS invariant), v59 reversible retirement (`retirements` synced,
  `retirement_rows` the node-local undo record, `summary_inputs` synced,
  `entities.hidden_by`, `sources.retired_by` — see the RETIREMENT
  invariant), v58 `nodes` (synced node list; `agents::publish_self`
  every sync; the thinking lease reads it), v57 relay stamps (`synced_at` on every timestamp-synced
  table — see SYNC LEGS PAGE BY A LOCAL STAMP), v56 agent_directory, v55 agents (`agents` node-local, `governance`,
  `agent_tasks`, `agent_approvals` synced; `ask_log.tainted`), v54 friend groups (`friend_groups` + the
  node-local `peer_groups` cache — see the FRIEND GROUPS invariant),
  v53 `llm_usage.items` (runs of on-machine models
  — the embedder — are ledger rows: model `local:<name>`, no tokens,
  priced $0 by `pricing_for`; the monthly spend budget lives in
  daemon budget.rs — an ALERT at 80%/100%, never a cap, whose
  notification must not state the amount spent, the ledger being
  vault-gated), v52 scoped keys (`api_keys.grants` + the
  `key_requests` ask-then-approve queue — see the SCOPED KEYS invariant),
  v51 the iOS device-feed wave (design log 2026-08-16) — `location_observation`, THE unified
  location layer every geolocated source feeds (photo geotags
  backfilled + dual-written, CLVisit dwells via POST /v1/ingest/location,
  calendar venues, scan captures; UNIQUE(source, source_ref) = per-source
  idempotency only), clustered per closed day by the `visits_finalize`
  pass (daemon visits.rs: ~100 m grown by accuracy, ≤20 min gap; a lone
  photo is NOT a visit — dwell or ≥3 obs over ≥20 min, AND at least one non-calendar observation unless the owner's synced `visits.calendar_alone` is on (default off since 0.5.71; Integrations → Pair a device; the `visit_rule` envelope rides the operational mirror BOTH ways, LWW by its stamp, a tie goes to off). Calendar-only visits the setting counts mint as `visit:<day>:c<n>`, so a node's setting never moves a sensed `visit:<day>:<n>` ref; visits the pre-0.5.71 rule minted from a calendar entry alone are withdrawn once on every node (`withdraw_calendar_only`: superseded by the id of `visit:<day>#calendar-only`, begun by the first pass run with the setting off — a start that syncs; episodes-rowid watermark). Switching changes later days only) into `visit`
  episodes citing every contributing source — cross-source dedup happens
  at CLUSTER time, never by trusting client keys; `visit_day` is the
  minted ledger (photo_day precedent, superset guard, `visit:<day>:<idx>#r<n>`
  supersession, shrunken re-clusters absorb via supersede_stale_visits;
  visit_day itself deliberately does NOT sync — nodes re-cluster synced
  observations and converge); `contact_card` (the identity backbone:
  latest card per CNContact key, LWW peer leg /v1/sync/contacts, applied
  into Person entities attribute-first + `contact_card` episodes,
  changed cards re-mint under #r<n>); `device_state` (source-GENERIC
  attention/presence interval stream — ios-focus first, the hpresence
  ember's app/idle spans can join the SAME stream later, Jon 2026-08-16);
  ingest endpoints /v1/ingest/{contacts,calendar,reminders,location,
  motion,media,scan,focus} (daemon live_ingest.rs; calendar claims
  source="ics" so live + .ics import dedupe on event UID; reminders
  upsert tasks under DETERMINISTIC ids tk_rem_<hash>, journaled,
  completion one-way; media mints media_play + Track/Artist/Show;
  ios-scan mints receipt/business_card/document with NO structural
  entities — it's in the extraction allowlist, the LLM pass mines Org/
  Person from OCR; motion mints `trip`); `ingest.device.scope=private`
  consolidation-state toggle vaults motion/visits/focus output; peer
  legs /v1/sync/{locations,device_state} (telemetry pattern) +
  /v1/sync/contacts (LWW); /v1/ingest/status covers all ten sources,
  batches log human refs; RLM tool `llm_usage {days?, ask_id?}` lets
  Ask introspect its own v50 spend when explicitly asked (blanket
  vault gate unchanged),
  v50 llm_usage (the token-accounting ledger — one
  append-only row per LLM API call from the daemon's `llm::call`
  wrapper, the ONE way to call the provider: purpose/ref/model/tier,
  input/output/cache tokens, latency, status; tokens are stored truth,
  cost computed at read from provider::pricing_for + `llm_pricing`
  config overrides; peer-synced via /v1/sync/llm_usage on the
  telemetry-leg pattern; read surfaces /v1/llm/usage + /v1/ask/{id}/trace
  are BLANKET-vault-gated — 403 while locked, traces can quote vaulted
  content),
  v49 vec_chunks (the sqlite-vec ANN index behind vector_search —
  dual-write + coverage-gated cutover, brute-force fallback kept),
  v48 ingest_log (append-only raw-device-batch audit
  trail — /v1/ingest/health|photos append; read surfaces GET
  /v1/ingest/status + /v1/ingest/recent, keyless-loopback-or-read-PAT,
  the iOS top-to-bottom ingestion check),
  v47 photo-location union + THE SUPERSESSION PATTERN
  (photo_point: durable per-photo store keyed
  `<unix-capture-secs>|<filename>` — the identity BOTH the Mac
  Photos.sqlite scan and the iOS geotag stream can see, so they UNION
  instead of duplicating; photo_day: the minted ledger (count +
  keys-hash) the photos_day_finalize pass reasons from;
  episodes.superseded_by + grants.updated_at; peer legs
  /v1/sync/photos (point union — the Mac scan lives on the laptop, the
  phone posts to the tenant, neither alone holds the union) and
  /v1/sync/sharing (grants + item shares LWW by updated_at — ONE
  sharing posture editable on any of a user's own nodes, enforced
  wherever the friend actually connects; friend KEYS stay node-local)),
  v46 granular friend sharing (store/sharing.rs — the
  friend surface's consent matrix: grants row `__default__` = the sharing
  TEMPLATE with LIVE inheritance, per-friend tri-state overrides in
  grants.sharing JSON; `shared_items` = explicit per-item shares that
  punch through the matrix (journaled, peer-mirrored; grant ROWS sync
  via the v47 /v1/sync/sharing leg — only the friend KEYS are
  node-local);
  classes episodes/tasks/notes/relationships/profile;
  `effective_grant` resolves template∪override at READ time — private
  never grantable/shareable, a privacy relabel silently retracts a
  share; the ONE friend write is `friend_toggle_checklist` on
  collaborate shares, toggle-only, text-verified, actor friend:<name>;
  connecting seeds a Person + declared friend relationship from the
  peer's profile card via the stable `brain:<name>` handle claim,
  idempotent + retroactive; RLM handles ntasks/nrels/nnotes/nnote/
  noffer/sharing),
  v45 idx_episodes_fts_rowid (the reverse FTS join index — see the
  perf invariant below),
  v44 health_sample (durable live-HealthKit sample
  store for the iOS companion's POST /v1/ingest/health — PRIMARY data,
  uuid-keyed idempotent, aggregated into health_day episodes by the
  health_day_finalize consolidation pass through the SAME
  apple_health::DayTable as export.xml, converging on the shared
  day:YYYY-MM-DD source_ref, first-writer-wins),
  v43 op_journal (the operational durability journal —
  append-only, content-bearing; see the OPERATIONAL DURABILITY invariant
  below ),
  v42 plugins registry + plugin_kv (the plugin
  contract — see docs/plugins.md SHIPPED summary: sidecar processes
  with scoped PATs, DEFAULT-DENY route ceiling in daemon plugins.rs,
  daemon-mediated storage under ~/.hearthloom/plugins/<name>/,
  /v1/sync/plugins peering leg for mirror-sync'd plugin data,
  manifest-declared UI (top-nav/Visualize/hamburger/Desk widgets/
  ui.card links, `dial: true` mounts the core Dial above the sandbox
  and streams {from,to} via postMessage) rendered in
  sandboxed iframes with a 12h plugin-ui token — plugin JS NEVER runs
  in the dashboard origin; manifest also carries placement
  local|cloud|any, runner single|per-node (per-node skips the
  __runner lease), sync mirror|local-only, and config[] (declarative
  settings the card renders into the ember's config KV); an ember
  REGISTRY is any server speaking the registry protocol (Hearthlaunch is
  one), configured by `registry.url` / `HEARTHLOOM_REGISTRY_URL` and OFF
  by default; CLI installs verify sha256 + Ed25519 signatures; reference
  plugin: https://github.com/metavert-io/hpresence),
  v41 private-id partial index (`idx_episodes_private_id`
  — vault guards over episode_entities aggregates are `episode_id NOT IN
  (SELECT id FROM episodes WHERE scope = 'private')`, never a correlated
  NOT EXISTS into episodes: the correlated form probes the whole episodes
  table per outer row — 36s+ per /v1/stats call on the tenant, the
  2026-07-19 wedge), v40 backlog partial indexes (scope/extraction/
  embedding candidate queries were O(record) full scans — each index's
  WHERE matches its query VERBATIM; don't reword one without the
  other), v39 extraction_verdicts (the verdict ledger — links regenerate from tier-stamped verdicts; replication via /v1/sync/extractions + extraction.applied bus topic; supersession diffs enable tier re-runs; apply marks extracted = single-spend across peers; tri-class LLM failure handling + llm.blocked state; design locked design log 2026-07-17), v38 notifications (the queue: hints never data, upsert by origin+dedupe_key, notify PAT scope, link allowlist, two-way peer exchange + notification.created bus topic; the provisioner control plane + killswitch live in daemon provisioner.rs — HEARTHLOOM_PROVISIONER_SECRET machine env, HMAC purpose=control above ALL PATs, 402 + allowlist while active; design locked design log 2026-07-16), v36 note_links (wiki-links) + note_neighbors (the
  resonant lane) + lint_findings, v37 entity_renames (append-only rename
  audit trail — carries TYPE + both names because entity ids differ
  across peers; the operational mirror REPLAYS it on the receiver by
  (type, from_name), the relationships_by_name precedent; renames go
  through `rename_entity` BY ID, which refuses a taken same-type name:
  that's a merge, not a rename),
  v4 scope_classified, v5 extracted, v6 ask_log, v7
  sources registry, v8 privacy_rules, v9 ask_log.sensitive, v10
  ask_log.thread — multi-turn conversations, v11 location_points +
  episodes.location_scanned — the Atlas index, v12 episodes.fts_rowid,
  v13 edge strength backfilled from edge_provenance — assert_edge
  increments it; never add rankings on unwritten columns,
  v16 tasks.tags (hashtag capture: #urgent/#important order, #daily…
  frequency, #checklist/#workout kind, #template-name binds a form,
  #today/#tomorrow due date),
  v17 notes views counter, v18 notes origin (importers — and the WRITER, `NoteWriter`: `hearthloom` = the owner, `agent` = an agent (MCP commits, `agent:` actors, an owner's note an agent edited — permanently), `loom` → agent, anything else imported; snapshots carry trust self|agent|imported, `derivation_for(source, trust)` is `declared` only at trust self, and mentions in non-owner notes link existing entities only), v19 notes.kind
  (templates are notes), v20 relationships (predicted-then-confirmed
  kinds, me detector), v21 note_mentions (@[Name] pick-to-link index;
  settle snapshots carry mentions into the graph), v22 task_mentions
  (same pattern for task descriptions; reindexed on add/edit),
  v23 social_connections (network friend lists as a REGISTRY, not
  entities — a friend list is a claim, not evidence; the merge-candidate
  scan is evidence-gated: zero-episode persons never enter the feed),
  v24 semantic_pairs (the Galaxy's comets: cross-source nearest-neighbor
  episodes from existing chunk embeddings, recomputed at idle — top
  entity-rich episodes PER SOURCE, substance-filtered; a global notable
  feed is all chapters and pairs nothing),
  v25 social_connections detail (company/position/handle — what the
  network knows about each name; LinkedIn fills all three),
  v26 notes_index.origin_ref (source-native id — Notion page-id — so
  note re-imports UPSERT: changed pages update, nothing duplicates;
  embedded IMAGES are preserved under notes/media/<page-id>/ and served
  by the vault-guarded /notes-media route — the one rich-media
  exception; other media stay [media: name] placeholders),
  v27 nullable note dates (a dateless source imports as NULL — an
  honest unknown that sorts OLDEST under recency; created-only sources
  get modified=created; native notes always stamp now),
  v30 api_keys (PATs: SHA-256-hashed scoped bearer tokens, secret shown
  once, revoke = timestamp; ☰ → API Keys mints them; enforcement in
  http.rs auth_middleware — loopback stays keyless unless
  HEARTHLOOM_REQUIRE_KEYS=1, but the /brain manifest surface requires
  scope `read` ALWAYS and serves the vault-LOCKED view ALWAYS via
  store/brain_fs.rs, whose queries hardcode the private exclusion —
  never swap them for vault-aware readers),
  v31+v32 users+sessions (the login layer, store/auth.rs: argon2id
  admin keyed by EMAIL (v32; lowercased, validated — the same key
  a provisioner's issuer tokens use as `sub`),
  hashed session tokens in an HttpOnly hl_session cookie; THE POSTURE
  RULE: auth is required iff a user row exists — scripts/bootstrap.sh
  creates the admin (stdin, never argv) and the door locks in the same
  gesture;
  HEARTHLOOM_AUTH_OVERRIDE=open is the lost-password escape hatch,
  HEARTHLOOM_TRUSTED_ISSUER_SECRET enables a provisioner's HMAC login;
  --remote posture = HEARTHLOOM_HOST=0.0.0.0 + TLS at the edge),
  **v14 the epistemic ledger + reasoning layer**: episodes.derivation
  (observed|declared|inferred — inferred must cite evidence; Ask can
  filter to facts-only), self_model (propositions per sphere/kind,
  origin declared|inferred, Tend-gated), standing_queries (decomposed
  intents + embeddings), opportunities, consolidation_state watermarks,
  entities.declared_importance (0-3 tier — outranks evidence counts in
  top_entities/traverse_top/notable),
  **v15 the productivity layer**: tasks (plain|checklist|workout|form,
  recurrence, soft delete), task_templates, notes_index + notes_fts
  (notes are markdown FILES under ~/.hearthloom/notes/ — DB is index
  only), metric_points (derived, rebuildable), goal columns on intents
  — tasks/notes/goals are OPERATIONAL tables (mutable, the intents
  precedent); completion/settling mints episodes: sources desk/notes/
  codi → derivation declared),
  store (incl.
  the vault — runtime
  lock state on `Store`, see privacy invariant below), config
  (`${VAR:default}` YAML + `~/.hearthloom/.env`), provider trait (Anthropic
  default), local ONNX embeddings, BRAIN.md generator (self-model section
  renders confirmed propositions first, private sphere excluded always)
- `crates/hearthloom-adapters` — git, ICS, mbox (streaming; `LabelFilter`
  over `X-Gmail-Labels` — Spam/Trash always dropped, kept episodes record a
  `labels:` preamble line), chatlog
  (Claude/ChatGPT, owner-aware via users.json), claude-code, iMessage, Apple
  Photos (location trace), Chrome history, **Substack** (archive API, works
  on custom domains), **apple-health** (export.xml streaming scan — raw
  samples are NOT episodes: one `health_day` aggregate/day + per-workout/
  ECG/clinical episodes, source-priority dedupe, Device entities per day so
  Ribbons shows coverage gaps; numbers go to the vquery sidecar),
  **facebook** (extracted DYI dump at ~/.hearthloom/archives/facebook —
  structural only, mojibake repair (latin-1-masked UTF-8) on EVERY
  string: fb_post (place tags → Place entities + `[HH:MM] name
  (lat,lon)` lines the location index parses — `parse_location_lines`
  reads ONLY photos, `fb_post` and the visit's canonical last line, and
  only lines that open `[HH:MM] ` and END with the pair; third-party
  titles/places are written one line (`hearthloom_core::one_line`) so
  none can forge such a line. A new parser of stored bodies must follow
  the same two rules), fb_album, fb_message
  (thread-day bundles; DM partner always an entity w/ facebook:slug
  attribute, group members only at ≥5 msgs — the casual-connection
  guard), fb_event (RSVP ≠ presence: no location points), fb_group,
  fb_profile (your emails/phones → identity attributes); the 3k friends
  LIST goes to the v23 social_connections REGISTRY, never to entities —
  materialize via /v1/connections/materialize or by real interaction),
  **linkedin** (extracted export at ~/.hearthloom/archives/linkedin —
  own RFC-4180 CSV parser, no csv crate: li_position (career spine,
  dated at start, employer Org entities)/li_education/li_honor/
  li_publication/li_event/li_article (authored HTML → text)/li_profile
  (emails → identity attrs); the ~30k connections → the registry WITH
  company/position/email detail (v25) — the richest disambiguation any
  network exports; built to absorb LinkedIn's later FULL export through
  the same source_refs);
  adapters emit `EpisodeDraft`s and never touch the store
- `crates/hearthloom-daemon` — axum HTTP API + stateless streamable-HTTP MCP
  at `/mcp` (exactly four tools: search, traverse, recall, propose_memory);
  consolidation loop (source refresh ≤1/20h, location backfill,
  embeddings backfill, intents, daily LLM digest, scope classification,
  entity extraction, monthly CHAPTER headings — kind=chapter episodes,
  inputs hard-exclude private, Dial weight 100 — plus the 2026-07-09
  reasoning passes: intent decomposition (standing_queries + embeddings;
  run_intents matches FTS + semantic legs), cascades (fresh matches ×
  entity/temporal/place/semantic-LITE — never vector_search, it loads
  every chunk embedding — → kind=cascade episodes; explicit non-private
  everywhere), self-model growth (digests+chapters → proposed
  propositions, Tend-gated, ≤6/cycle, backlog cap 12), opportunity scan
  (confirmed seeking × new observed episodes, embeddings only, local),
  note settling (48h-quiet actives snapshot kind=note), merge candidates
  (shared whole name token REQUIRED + embedding ≥0.88 — name embeddings
  alone cluster all person names; see design log 2026-07-09) — and
  BRAIN.md);
  **the push bus** (`bus.rs`, 15b SHIPPED 2026-07-15 — notify-then-fetch
  event HINTS, never data: GET /v1/bus/stream (SSE, Last-Event-ID replay
  ring 256/24h, `resync` past the gap) + POST /v1/bus/publish, both peer
  scope; `touch()` hooks on Desk/goal/note/relationship/selfmodel/project
  writes → operational.changed, rename → entity.renamed, scope/privacy →
  privacy.changed; EVERY hint (and resync) triggers one debounced
  sync_once, single-flighted with the 5-min tick via automation key
  `peer-sync-early`; 404/405 → poll-only feature-detect both directions;
  willingness filter `bus::offered` at fan-out — friend grants slot in
  there with federation; notes in the design log 2026-07-15);
  inbox drop-folder watcher (`~/.hearthloom/inbox/` — zips, mbox, ics,
  conversations.json, Finder-dragged chat.db/Photos.sqlite);
  `/v1/ingest/local` runs TCC-gated adapters daemon-side;
  **RLM deep-query engine** (`rlm.rs` — `hearthloom query`, POST /v1/query,
  SSE stream at /v1/query/stream; ≤8 rounds over search/recall/traverse/
  read_episode/guarded-sql handles, plus read-only vsearch/vfetch when a
  virtual source is registered; every ask traced in `ask_log`; since
  2026-10-05: `{"calls":[…]}` batches ≤4 lookups per round, a local
  pre-search seeds round 1, `map {sql, instruction}` is the ONE depth-1
  fan-out (Low tier, ≤40 rows, ≤2/ask), citations are checked against
  the evidence, and friend handles sit behind the outbound gate — see
  the Ask invariants below);
  **virtual sources** (`virtual_source.rs` — HearthFS, `mbox` mail
  archives (`mbox_index.rs`: sidecar `<archive>.vidx.sqlite` next to the
  archive, byte offsets + from/subject/labels/date FTS; derived and
  rebuildable, NOT the substrate db, so law #4 is untouched; register
  rebuilds it in the background when the size+mtime fingerprint is stale),
  and `apple-health` numeric sources (`health_index.rs`: samples + daily
  rollups sidecar; **vquery** = read-only single-SELECT/WITH SQL handle
  for the RLM + `GET /{name}/query`; vault gate is coarse —
  `source_has_private` locks the whole handle if any episode of the
  source is private);
  magic-path, plain-dir, or mbox-file mount, `POST /v1/sources/virtual`
  register, `/{name}/search`, and explicit `/{name}/promote` = materialize
  on demand with origin+fetch-time provenance (promoted mail parses
  Subject/Date → kind=email), untrusted → quarantine-first);
  **inbox watcher refuses mbox > 250MB** — full Takeouts go through the
  filtered pipeline (`hearthloom ingest mbox <file> --include-labels …`,
  runs daemon-side detached), never unfiltered through the drop folder;
  **embedded dashboard** (`assets/` — index.html, app.css, app.js,
  constellation.js, vendored three.js) served at `/` — NEW asset files
  must be registered in `dashboard.rs`'s include_bytes! table or they
  404 (desk.js learned this)
- `crates/hearthloom-cli` — `hearthloom` binary; talks HTTP, auto-spawns the
  daemon, falls back to daemon-side ingest when TCC blocks its shell; also
  installed at `~/.local/bin/hearthloom` (install.sh adds that to PATH).
  **Operator layer (0.5.13, ops.rs)**: `auth login|status|logout` (mints a
  CLI-owned admin PAT named `cli:<host>`, stored in the login keychain
  service `hearthloom-cli` keyed by base URL — THE fix for "sign in
  required" strangling curl/CLI work on an auth-locked node; every command
  sends it automatically), global `--url` (target the tenant; remote nodes
  never auto-spawn) + `--json`, `status` (one-screen node summary),
  `doctor` (the CLAUDE.md gotchas as executable checks: orphan daemons,
  launchd-runs-wrong-binary, CLI/daemon version skew, auth, model, WAL
  bloat, PATH), `task add`/`note new|list` (Desk capture), `key
  mint|list|revoke`, `vault status|unlock|lock`, `export` (needs unlocked
  vault), `sync [--now]`, `ember search|info|install|uninstall` (the configured
  registry, or `--registry`; install = sha256 +
  Ed25519 release-sig verify + counter-sig when present, then a scope
  prompt — the human gate survives; stages through the daemon's normal
  upload+install) + `ember validate|pack|list|logs` (dev loop),
  `notify`, `logs -f`, `completions`. For agent shells: `hearthloom
  doctor` first when anything seems wedged; `status --json` for parsing.
- **Eval suites live in the data home, not the repo**: `hearthloom eval`
  runs `~/.hearthloom/eval/personal-eval.yaml` (retrieval checks; extend it
  whenever a new source lands), `--ask` runs `ask-eval.yaml` (the full Ask
  engine per question, REAL tokens, reports rounds/tokens/cost/latency —
  run it before and after any model, prompt or Ask-loop change), and
  `--suite <name>` any other set. The first run (and `hearthloom init`)
  copies the invented examples from `eval/examples/`. Real-life ground
  truth never goes in `eval/` (see `eval/README.md`).
- `scripts/install.sh` — binary + launchd agent install

## Dashboard (http://127.0.0.1:43278)

"Embers at night" design system (native CSS tokens in app.css, dual theme
with contrast parity, serif display + mono data, no webfonts). Nav order
(Jon 2026-07-10: "Desk is now the dominant system"): **Desk (HOME —
default route + wordmark)** / Notes (full page #/notes) / Memories /
Relationships / Day /
Search + a **Visualize** dropdown (Loom, Orrery, Constellation,
Atlas, Pulse, Biography, Ribbons) + Tend; Privacy and the episode counter
live in the ☰ hamburger (counter → #/analytics, composition of the record
via chart-svg.js over /v1/stats kinds/derivations/scopes). **Canvas rule:**
absolutely-positioned canvases need explicit CSS width/height (replaced
elements ignore inset-stretching and fall back to buffer size ×dpr —
the constellation lower-right bug); verify canvas layouts at
deviceScaleFactor 2. **Loom** (loom.js, lazy — source-threads
streamgraph across the months; hover readout, click → Memories at that
month, legend rests threads). Views: Day
(source filter chips, expandable cards with rendered markdown + provenance),
Search, entity Dossiers, **Constellation — two altitudes** (#/constellation
with no focus = the GALAXY, galaxy.js: sources as suns on a ring, each
sun's most-cited entities as planets whose force layout hangs
multi-source people BETWEEN their suns — the bridges become visually
central; co-citation gravity lines; semantic comet pairs from v24
riding above the plane; dial = time-lapse of the galaxy forming; an amber NOTES sun floats
above the plane (live notes clustered by tags, tethered by lines to
@-mentioned people/orgs; click note→editor, sun→panel);
**clicking a sun zooms into its SYSTEM via a continuous camera flight**
(glide to the sun, swap scenes at arrival with the relative view
preserved; ascend = the ride home) (per_source=24
roster, ranked golden-angle orbits, all labeled, orbit threads;
background/Esc/◌ ascends); clicking a planet: Person → the person
MODAL (stay in context), else → Memories prefilled q=name (+source
inside a system); /v1/galaxy is two grouped
window-CTE queries + json_each co-citation, 10-min response cache keyed
by window+vault-state; a focus = the classic three.js 1–2 hop
neighborhood with "◌ whole galaxy" back — hard caps, never a global
hairball; orbit ⟲⟳/tilt/tour nav pad + arrow keys),
Ribbons (monthly evidence bands per entity), **Atlas** (canvas ember
world map over vendored Natural Earth land — no tiles/geocoder; points
from the v11 location index, hover→place, click opens the VISIT panel
(client-side stay-point segmentation: 25km/2d-gap; >14d runs or >20%
share = home-base anchor → stats + ±3d window, never monster lists;
stays picker; Browse handoff via window.__browsePrefill; Day view gets
a "part of visit" chip via sessionStorage hl-visit), time-lapse play
with fading journey arcs; atlas.js lazy-loads like constellation.js;
Google-Maps-as-configured-option deferred per the cloud doctrine in
the design log), **Desk** (desk.js, lazy — the PRESENT tense: tasks
with per-kind cards (checklist toggles, exercise logger, template form
fields), quick-add, backlog, goals rail (intents grown up: create /
achieve / let go, decomposed-query chips, fresh-match links), notes
rail + slide-over markdown editor (optimistic-lock save, archive =
snapshot into the record); completing a form-template task with
note_on_complete offers a prefilled linked note — the task→research→
note loop; @ in task fields uses the shared pick-to-link typeahead from
notes.js — descriptions render @[Name] as mention links),
**Relationships** (relationships.js — evidence-tiered directory
you/inner/active/drifting with everyone/people/orgs chips (?type= gives
orgs their OWN tiers — people's citation volumes crowd them out of
shared caps) + a server-searched "beyond the tiers"
section (the filter must find ANYONE the record knows, not just the
loaded top), ghost-skeleton while loading, per-row email/phone handle
for same-name disambiguation, predicted-kind chips ✓/✗, declare select
with "is my…"/"was my… (former)" optgroups (former = declared confirmed
+ ended in one gesture; partner/parent/child are kinds, former spouse
is partner+ended; `contact` + the freeform note QUALIFIER — surfaced as
an inline input after picking a kind, rendered on chips as
`kind · qualifier` — covers cases like "accountant — since 2019"
without kind-list inflation), ✎ notes chip on EVERY row (0 notes → new
note prefilled with the linked @mention), ⋯ → merge panel: candidates
EXPAND on click (full attributes, episode count, latest evidence) and
only explicit buttons act — merge into this row / different person
(resolves Tend review distinct) / open their card / "merge into someone
else…" third-party redirect typeahead; already-merged-in list with
per-row UNMERGE; /v1/identity/merge accepts entity ids OR
exact names (the silent-failure bug: dashboard sends names)),
**person modal** (app.js openPersonModal — EVERY `#/entity/…` link
app-wide opens the dossier as a modal that keeps your place; boxed
`.pm-section` cards with EXPLICIT titles — relationship / "tasks
mentioning them" (before notes: time-sensitive) / "notes mentioning
them" / recent evidence — plus declare-with-qualifier and importance
stars; class `no-modal` or ⌘-click opts back into real navigation to
the full dossier page, which carries the same tasks/notes sections;
Search also has a live-notes leg above episode hits), Tend (merge reviews — rendered a
dozen at a time / quarantine
/ intents, badge in nav), **Memories** (nav label; routes stay #/browse — the workbench: faceted grids over
episodes/entities/identities/edges/sources, sortable columns, infinite
scroll, per-row + bulk scope correction incl. mark-private; the
episodes tab carries **the Dial** — dial.js, a fisheye drum timeline
that IS the from/to filter: momentum spin, pinch/ctrl-wheel time zoom,
semantic year→month→day tiers; labels are NOTABLE EVENTS
(GET /v1/timeline/notable — importance-ranked viewport top-K, the
map-label pattern; BM25 mode under search; hidden in motion, fade on
settle); Search mounts the same component zoomed to the hit span,
bucket-click → Browse handoff; Day view mounts it as the day-scrubber
(settle = navigate, span in sessionStorage); density from
GET /v1/timeline/histogram, vault-guarded), **Privacy**
(vault card + privacy rules + the notes card: vault/unvault whole
note ORIGINS — live notes AND their settled snapshots; unvault re-runs
the rules, which always win; single notes vault via the editor's scope
selector; masthead 🔒 toggle), **Pulse** (pulse.js —
the oscilloscope: metric registry over vquery health series +
histogram introspective series, ticker playback past a fixed now-line,
odometer value, milestone flashes, compare overlay, compact dial),
**Biography** (biography.js — life-heatmap cover + year spreads:
chapter headings, radial year-clocks via vquery), **Rewind**
(rewind.js — masthead ▶ self-playing reel), **Ask Hearthloom**
(slide-in SSE chat over the RLM: **multi-turn** — follow-ups carry the
thread's prior turns as vault-guarded context, failed turns included so
"try again" works; history replays and CONTINUES whole conversations;
searchable infinite-scroll history with
permanent delete behind a styled confirm modal, debug
toggle default-off with humanized "alive" status, and **inline SVG
charts** — the RLM emits ```chart fenced JSON specs; chart-svg.js is the
ONE dependency-free renderer (bar/line/area/pie/donut/scatter/heatmap),
colors ONLY via --chart-* CSS vars for dual-theme parity), and the **first-run wizard**
(#/welcome — auto-entered on an empty brain; renders the UI admin
bootstrap ("protect this brain", auth-open non-provisioner nodes only,
POST /v1/auth/bootstrap), restore-from-export, identity claims, and the
Integrations cards mounted as steps via integrations.js exports
paintAnthropic/paintGmail/paintPair/paintEmbeddings + initHelpers — keep
those exported;
step numbers are computed, don't hardcode).

- **`confirmDanger({title, body, action, input})` takes the confirm
  BUTTON'S LABEL as `action` and resolves true/false** — it does not run a
  callback. `const ok = await confirmDanger({…, action: "delete"}); if
  (!ok) return; …`. Passing a function made the button show its source and
  do nothing: the Network page's revoke/disconnect were dead until 0.5.64.
- **Top-bar breakpoints (0.5.66):** the full bar needs ~1300px; ≤1340px
  tightens spacing, ≤1140px turns the primary nav into the ☰ drawer, ≤680px
  is the phone trim, ≤400px hides the wordmark text. Adding a nav item
  means re-measuring (Playwright: no element of `.masthead` past
  `innerWidth`, widths 320–1600) and raising 1140 if needed.
- **Ask recipes** are `kind='recipe'` notes tagged `#ask-recipe` (title
  = chip, body = the question) that only the ☆ button makes
  (`askSaveRecipe`). The store refuses kind=recipe unless origin is
  `hearthloom` and turns a recipe back into a plain note on an agent's
  edit; the panel shows and runs only kind=recipe + origin=hearthloom
  (`isRecipe`, re-checked on click) — a tag alone (an import, an agent's
  note) never makes a chip: a chip's body is sent as the user's own
  words. A question asked with the vault open saves as a private note.
- **CSS `hidden`-attribute gotcha** (fixed bug — don't re-break): the Tend
  nav badge is toggled via the HTML `hidden` attribute (`el.hidden = !count`),
  but `hidden` only sets `display:none` at the UA level — any element with an
  explicit `display` (e.g. `.navbadge{display:inline-block}`) silently
  overrides it and the element shows *always*. Every such element needs a
  companion `.foo[hidden]{display:none}` rule. Badge counts
  `review_pending + quarantine_pending + propositions_pending +
  opportunities_pending` (NOT intents).

## Materialized vs virtualized sources (architectural direction)

All 10 current adapters **materialize** (clone data into the episode log —
the substrate; law #1 applies fully). **Virtualized** sources (SHIPPED:
HearthFS mounts, and `mbox` mail archives via sidecar index — the Gmail
Takeout long tail; PLANNED: federated/live APIs, other brains)
stay at origin and are queried at need — surfaced through the source
registry, exposed as read-only RLM handles (vsearch/vfetch), and
**promoted** (materialized *on demand* via an explicit write, provenance
recording origin + fetch time, untrusted origins quarantine-first) when
worth keeping. The RLM query loop stays read-only; promotion is the
separate deliberate step. Virtual sources are best-effort/online; the
substrate never depends on them (an offline mount degrades to a clear
error). Big archives can serve BOTH tiers: a Gmail Takeout can be
Tier-1-materialized for its Core strata (Sent/Personal/Starred, filtered
ingest) and Tier-2-virtualized for the whole archive (a `gmail-archive`
source over `~/.hearthloom/archives/`). **Gmail live federation (Tier 3)
SHIPPED 2026-08-10** (design log entry same date): the `gmail_pull`
automation (cycle cadence, silent no-op until `GMAIL_IMAP_USER` +
`GMAIL_IMAP_PASSWORD` land in `~/.hearthloom/.env`) pulls new mail over
app-password IMAP (`imap 3.0.0-alpha` DELIBERATELY — the first line
whose imap-proto parses X-GM-LABELS; don't "upgrade" it back to 2.4)
into a rolling `archives/gmail-live.mbox` + incrementally-appended
sidecar (`mbox_index::append_index` — NEVER append to the Takeout mbox:
its fingerprint invalidates and a full rebuild follows), annotates
categories via one ranged X-GM-RAW search (X-GM-LABELS does NOT carry
them), materializes Core strata as `source="mbox"` + Message-ID so the
archive overlap dedupes via UNIQUE(source,source_ref), and advances its
`uidvalidity:last_uid` cursor (sidecar meta, rebuild-surviving) only
after everything else succeeded — replays are idempotent. First run
backfills `GMAIL_IMAP_BACKFILL_DAYS` (default 90), caps 2000/run, and
registers the `gmail-live` virtual source. See
`virtual_source.rs`, `mbox_index.rs`, `gmail_pull.rs` and `gmail_imap.rs`.

## Peering vs federation (two surfaces, DIFFERENT trust models — never unify)

Two nodes talk in two fundamentally different regimes; keep them separate
protocols with separate endpoints (peering = `/v1/sync/*` peer-scope;
federation = `/brain` + network-hello + RLM vsearch, friend-scope). See
the design log 2026-07-14.

- **Peering = ONE owner, two co-deployed nodes, FULL trust** (private
  replicates both ways, vault-locked on both). Version skew is only the
  transient dual-deploy window. So peering stays DUMB: no capability
  negotiation, no willingness. It needs only the wire-compat discipline
  below to survive the window. Do NOT add manifest machinery to peering
  (Phase 5 key-sync already works this way — feature-detect + degrade).
- **Federation = MANY owners, independent deploys, BOUNDED trust.** Mixed
  versions are PERMANENT and a peer may be old/forked/lying. This is where
  versioning + manifests live (deferred until federation is actually built).
- **The CAN / MAY / WILL-ENFORCE model** (federation): **CAN** = capability
  (the op exists; impersonal, version-tied, cacheable). **MAY** = willingness
  = the per-requester OFFER computed from the sender's local scoping ∩ the
  grants given to THAT key (directional/asymmetric, dynamic — NO version
  bump, short TTL, invalidate on grant change). **WILL-ENFORCE** = the vault
  + `Store::scope_guard` at read time. The offer is an advertised CEILING and
  a courtesy, NEVER the enforcement — the guard filters every read regardless
  of what was advertised (same rule as "capabilities are claims"). Express an
  offer as a scope descriptor, never a data enumeration.
- **Gate behavior on a `protocol` integer + capability strings, NEVER on the
  semver release string** (a forked build carries any version). Version
  capabilities when a wire format breaks (`search-v1`→`search-v2`). Keep the
  wire EXCHANGE schema (EpisodeDraft, manifest JSON) decoupled from the
  internal `PRAGMA user_version`.
- **Wire-compat discipline (BOTH surfaces, every peer/federation message):**
  additive fields only — never remove, rename, or repurpose a field within a
  protocol; ignore unknown fields (serde default); feature-detect-and-degrade
  per-op (404/405 → skip that op, never fail the whole exchange).

## Invariants to preserve when changing code

- **RETIREMENT IS SUPERSESSION BY NOTHING (v59, D.12 — design log
  2026-10-17).** A retired episode is marked `superseded_by =
  'retired:<retirement id>'` (core `store/retire.rs`), so EVERY reader that
  honours supersession honours retirement. The rule a new query must keep:
  a SQL statement that reads episodes or what hangs off them
  (episode_entities, edge_provenance, chunks, episode_fts, intent_matches,
  semantic_pairs, opportunities) carries `superseded_by IS NULL` (or the
  NOT IN form over the partial index: `episode_id NOT IN (SELECT id FROM
  episodes WHERE superseded_by IS NOT NULL)`), or says why it must see
  every episode with `/* all-episodes: <why> */` in the statement (a `//
  all-episodes:` line just above, for prompt text). Sync exports, export,
  existence/dedupe checks, privacy relabels and the Memories forensic view
  are the legitimate all-episodes readers. `test_every_reader_honours_
  supersession` scans core + daemon sources and FAILS THE BUILD otherwise.
  What a retirement switches off is DELETED WITH ITS PRIOR STATE SAVED in
  `retirement_rows` (links, edge provenance — an edge left with none gets
  `t_expired` —, intent matches, opportunities, map points, semantic
  pairs) or updated likewise (`entities.hidden_by`, beliefs back to
  `proposed` + journaled, `sources.retired_by`); the undo replays that
  record newest-first and conditionally (a memory superseded since, a
  belief decided since, stay as they are). The EVENT syncs
  (/v1/sync/retirements, LWW); the undo record never does — entity and
  edge ids differ per node, so each node applies the event itself
  (`retirement_apply` / `retirement_undo_local`, caught up every cycle by
  daemon `retire::catch_up`). New episodes a live retirement covers land
  retired inside the ingest transaction (`ArrivalRules`: a retired
  source, a late retired memory or ticked summary, a cascade of a retired
  seed, a `#r<n>` revision of a retired memory). Ticked summaries are
  REWRITTEN, never patched, under `<replaced ref>@<retirement id>` by
  `consolidate.rs::rewrite_summaries` on the thinking node; a rewrite
  that comes out empty is marked done (`rewrite.skip.<ref>`) so it never
  loops. Summaries record what they read (`record_summary_inputs`) —
  a new summary pass must too. `latest_summary` is the live digest;
  `latest_digest_written` (retired included) answers "is today's done?".
  Retiring needs the owner; a whole source needs the vault open; a
  private memory only retires while it is visible. `apply_extraction`
  links nothing for a superseded episode (a peer's verdict must not
  bring retired links back); a new live link un-hides an entity.
- **OPERATIONAL DURABILITY (v43, the 2026-07-27 task-loss fix).** Operational
  tables (tasks, notes, intents/goals, self_model, privacy_rules) are PRIMARY
  mutable state — NOT rebuildable from the episode log. Every create/change/
  delete MUST append a full-content post-image to `op_journal` via
  `store::journal_row`/`journal_op` (hard-deletes journal a `delete` tombstone
  BEFORE removing the row; notes also splice the markdown body under
  `__body__`). A new operational write path that skips this makes its content
  UNRECOVERABLE — that is the exact defect that lost a checklist. `restore_*`
  methods rebuild from the journal (`/v1/tasks/{id}/restore`,
  `/v1/tasks/deleted`). op_journal is Exported in TABLE_CLASSES.
- **SYNC LEGS PAGE BY A LOCAL STAMP OR SEQUENCE, NEVER BY THE SENDER'S
  (v57, multi-node M4).** A row relayed through a hub keeps its original
  stamp; a third node's cursor is past it, so a leg that pages by
  `updated_at`/`ts` silently never forwards it. Timestamp legs page by
  `synced_at` (millisecond UTC, set by triggers on every local write AND
  every peer apply — `store/mod.rs::RELAY_STAMPED` + `relay_stamp_sql`;
  the cursor is `page_cursor`); episodes' scope leg by `scope_synced_at`;
  rowid/seq legs (episodes, photos, journal) were already safe. A new
  timestamp leg: add its table to RELAY_STAMPED in a migration and page by
  synced_at. Last-writer-wins decisions still use the TRUE stamps.
- **PEER-SYNC IS THE DEFAULT FOR EVERY NEW SYSTEM (Jon 2026-08-16,
  recorded as law).** Any new table/subsystem holding user-visible state
  gets a peering leg unless a specific exclusion is decided and written
  down (in code comments + design log, like the export TABLE_CLASSES
  discipline). The telemetry-leg pattern (append-only, id-keyed
  INSERT OR IGNORE, ts watermark, feature-detect-degrade) is the
  cheapest template; llm_usage (v50) is the worked example. Node-local
  by design remains legitimate ONLY with a stated reason (e.g. friend
  KEYS, api_keys/sessions — secrets never sync).
- **Notifications from idle passes use a stable `dedupe_key`**
  (`consolidate.rs::notify_written`: `weekly-review:<week>`,
  `chapter:<YYYY-MM>`; budget.rs: `llm-budget:<month>:<level>`). Both
  of a user's nodes run the same passes; the notification exchange merges
  rows by (origin, dedupe_key), so a random or time-based key would ping
  twice. Announce only what is NEW to the user (the chapter pass
  announces only the month that just ended, never the backfill).
- **Every LLM call goes through daemon `llm::call`** (llm.rs) — it
  times the call and lands the v50 llm_usage ledger row (success AND
  error). A direct `provider.complete()` call site silently escapes
  token accounting — the exact defect v50 fixed; don't reintroduce one.
- **ASK INVARIANTS (2026-10-05, design log same date).**
  (a) The Ask system prompt must stay BYTE-STABLE within an ask — it is
  sent as a cached block; a date, counter or per-question string in it
  silently zeroes the cache (volatile text goes in the conversation;
  check `cache_read_tokens`). (b) THE OUTBOUND GATE (`rlm.rs::Flow`):
  any handle that sends a model-chosen string to another person's node
  must be in `FRIEND_TOOLS` — it then runs only when the user's own
  words name that friend and before the ask has read non-`trust='self'`
  content. Never move that check into the prompt. (c) No recursion past
  depth 1: `map` sub-calls get no tools. (d) Rows that fan out to a
  model in bulk go through `Store::read_query_public` (private excluded
  even with the vault open), never `read_query`. (e) Tool results enter
  the conversation inside `<tool_result>` fences. (f) STREAMED ANSWER
  TEXT IS A PREVIEW (`delta` events from `rlm.rs::AnswerStream`, only
  on /v1/query/stream): the `answer` event stays the one authoritative
  text — citation check, ask_log, what the panel keeps. Any path where a
  streamed reply does not become the answer must send `answer_reset`
  (`retract()` in `run_query`); a new provider's `complete_streaming`
  must return the same `Completion`, usage included, as `complete` — or
  not override it (why `custom` endpoints are not streamed).
- **AN ASK CONVERSATION IS PRIVATE AS A WHOLE (Jon 2026-10-06).** One
  turn asked with the vault open seals every turn of its thread:
  `ask_start` stamps `sensitive` across the thread, and EVERY ask_log
  read goes through `Store::ask_guard` (thread-aware — it also seals
  conversations recorded before the stamping) or the matching TEMP VIEW
  in `VAULT_SHADOW_VIEWS`. Never filter ask_log on `sensitive = 0`
  alone: that shows half of a private chat. `sensitive` carries WHY:
  1 = a vault-open turn (PERMANENT — never offer to unseal it), 2 =
  sealed by hand via `ask_set_private` (reversible, vault open only);
  test `!= 0`, never `= 1`. The history lists
  conversations (`ask_history` → one row per thread), and the panel must
  re-read on every lock change (`app.js::askVaultChanged`, fired from
  `paintVault`).
- **ASK MAY ACT — ONLY THROUGH agents/ (0.5.67; supersedes "Ask never
  acts", Jon 2026-10-17).** A `delegate` reply from the Ask model becomes
  an `agent_tasks` row via `agents::propose`, and EVERY action is decided
  by `store/agents.rs::decide` (auto / ask / never per action class;
  unknown classes and an empty list are `other`; THE TAINT RULE: after a
  task has read content the user didn't write, EVERY action asks, whatever
  its class, mode or preset — `never` still wins, and only the owner's
  `taint_guard` switch turns the rule off). Per-tool governance
  (`agents/mod.rs::tool_permission`, the Hermes hook, `/mcp/governance`)
  goes through the same `decide`. Never add a path where Ask, an
  agent, or a harness acts without going through `decide`. Default policy
  is ask-for-everything. Ask's `action` fences (Desk prefill) are
  unchanged. Agent CONFIG routes need owner + vault; approvals need owner
  only (vault too when the task is `sensitive`). Agent configs are
  node-local (secrets in .env.sealed as HEARTHLOOM_AGENT_<ID>_{KEY,TOKEN,
  HOOK}); policy/tasks/approvals sync (/v1/sync/agents, LWW). Harness
  callbacks (`/mcp/governance`, `/v1/agents/tasks/{id}/report`,
  `/v1/agents/hook/{agent}`) bypass API-key auth in the middleware and
  check their OWN tokens (per-task in memory, or the sealed hook token).
  The Claude Code bridge MUST keep `--setting-sources project
  --strict-mcp-config --permission-prompt-tool …` so the owner's own
  Claude Code allow-rules can't skip governance. The Ask card arrives as
  a `task` SSE event BEFORE `answer` (the panel closes on `answer`).
- **Ask's outbound guard (0.5.67, hardened 0.5.71):** taint is recorded
  per ask (`ask_log.tainted`) and inherited by later turns; `sql` and
  note hits taint, and so does `traverse` unless every name it shows is
  the user's own (`Store::entity_names_unverified`, fails closed); the
  check fails CLOSED. An ACTING reply (a `delegate`, a friend call) is
  taken only when the whole reply is one JSON object
  (`whole_reply_object`) — never fished out of prose that may quote a
  tool result. Every spelling of a fence tag inside results is defused
  (`defuse_tag`: `<tool_result>`, the map step's `<record>`).
  `rlm.rs::brain_for_ask` keeps BRAIN.md's Recent activity, Top entities
  and the digest out of the system prompt, matching section headings
  EXACTLY (the digest's own "## Self-model notes" once switched the
  filter off); BRAIN.md writes titles, names and propositions on one line
  and demotes the digest's headings below `## `.
- **MCP version negotiation:** answer `initialize` with the version the
  client asked for when supported (`mcp.rs::negotiate_version`).
- **TRIPS: A CAR'S DRIVE IS THE CAR'S UNTIL EVIDENCE SAYS YOU WERE IN
  IT (v60, design log 2026-10-23).** Embers post only `vehicle:*`
  movements (receiver-stamped); only `trips.rs` turns a drive into the
  person's `trip`, and only those place the person (map points, Place
  entities, `trips` location observations). Trip refs carry a FINGERPRINT
  revision of the day's output and the pass re-asserts its minted set
  (`assert_trip_day`) — never switch it to a counter revision (a peer's
  stale revision would win). Re-open a day by setting `trip_day.dirty`,
  never by deleting the ledger row.
- **EMBERS ON HOSTED NODES (0.5.72, design log 2026-10-23).**
  A hosted (cloud-kind) node installs ONLY packages
  `registry_client::stage` verified in this process
  (`is_verified_stage`) unless `HEARTHLOOM_ALLOW_UNREVIEWED_EMBERS=1` —
  never add an install path that skips `install_from_dir`'s guard.
  Ember secrets (`plugins/<name>/secrets/`, sealed by core
  `secrets::seal_node`) never export (export.rs `walk` carve-out +
  `.ember-secrets.key` Excluded) and never mirror (they live outside
  `data/`); a UI token may set but never read one (`bound_sidecar`).
  `publish` addresses stay under `/.well-known/` and are pre-auth +
  killswitch-allowed by design. Ember programs run under launchd on a
  Mac and under the daemon's own supervisor (`sidecar.rs`) elsewhere.
- **THE FRIEND CEILING (0.5.63 — a fixed security hole, don't re-open
  it).** A key whose only scope is `friend` (what connecting a friend
  mints — it belongs to ANOTHER PERSON) reaches only
  `http.rs::friend_route_allowed`, default-deny, enforced in
  `auth_middleware`. Before this, such a key passed blanket auth on
  every route that doesn't call `require_scope` and could read the owner
  API and unlock the vault. A new friend-surface route must be added to
  that table AND call `require_scope(&auth, "friend")`; owner-side routes
  under `/v1/network/` (mycard, accesses) must stay out of it. Every
  other key that isn't the owner's meets THE KEY CEILING.
- **THE KEY CEILING (0.5.71 — a fixed security hole, don't re-open
  it).** An ordinary key that is not the owner's (`is_owner_key`), not
  friend-only, not bound to a plugin and minted without grants reaches
  only `http.rs::KEY_ROUTES` — default-deny, matched on the router's
  TEMPLATE (axum `MatchedPath`, so /v1/notes/vaulted never reads as
  /v1/notes/{id}), enforced in `auth_middleware` after the grant ceiling.
  Before it, ~200 routes checked no scope: a `notify` key could mint an
  admin key, install a plugin or relabel private records. A new route is
  owner-only for such keys until listed — list it under the narrowest
  scope; `test_every_listed_route_exists` keeps the table honest. Key
  management also needs `require_key_admin` — except the paired phone
  minting a SCOPED key (`device_mints_scoped_key`: grants, no scopes), as
  the iOS app does for its capture key. A write by any key that
  isn't the owner's lands under `agent:key:<name>` (`write_actor`) —
  never the body's actor.
- **THE BROWSER RULE (0.5.71).** CORS is granted only to bearer-key
  requests (`cors_bearer`: a preflight that won't send `authorization`,
  and any keyless response, get no Allow-Origin). A KEYLESS request a
  page on another site made (`Sec-Fetch-Site` cross-site/same-site, or an
  `Origin` that isn't the Host) is refused on every non-static path, and
  a node with no login answers keyless calls only on a loopback Host or
  the host of `HEARTHLOOM_DASHBOARD_URL` (DNS rebinding; the installer
  asks for that address). On a no-login node a keyless request IS the owner: never
  widen `cors_bearer` or exempt an API path from
  `foreign_browser_request`.
- **FRIEND GROUPS (v54): A UNION OF GRANTS, NEVER A CROSS-PRODUCT.**
  Every friend-surface read starts from `Store::access(friend)`
  (store/groups.rs → `FriendAccess`): the direct grant plus one grant per
  group the friend is in. Ask it per question —
  `access.spheres(|g| g.has_class("tasks") && …)` — so a class from one
  grant never pairs with a life area from another; `access.ceiling()` is
  for the advertised `offer` ONLY, never enforcement. Item-share lookups
  use `access.share_target(column, param)`, never `friend = ?`. A group
  inherits no template and has no floor (`resolve_group`); membership
  counts only behind a live direct grant (`access` begins with
  `effective_grant`). Share targets `group:<id>` live in
  `shared_items.friend`; no connection may be named `group:…`. Group
  rows sync whole on every /v1/sync/sharing page, outside the cursor
  (their stamps can run ahead of the clock — `next_stamp`). A new friend
  read that calls `effective_grant` directly silently ignores groups;
  one that flattens grants over-shares.
- **THE OWNER GATE (0.5.65).** Only the owner unlocks the vault or turns
  a key into a session: `http.rs::require_owner` — a signed-in session
  (or open-posture loopback), an admin key, or a paired-device key
  (`is_owner_key`: holds `read` + `ingest` + `notify`, what pairing
  mints; the phone unlocks with its PAT after Face ID, so do NOT narrow
  this without changing the app). Any new route that reveals private
  material or confers the owner's reach goes behind `require_owner`,
  never `require_scope("read")`. Don't mint integration keys with all
  three device scopes — that makes them owner keys.
- **SCOPED KEYS (v52): THE GRANT CEILING.** A key minted with `grants`
  reaches only `KeyGrants::permits` (store/api_keys.rs) — default-deny,
  enforced in `auth_middleware` right after the plugin ceiling. A new
  route a scoped key should reach must be added to THAT table; nothing
  else opens it. A scoped key never sees private data (it can't unlock,
  and the vault is per viewer), `/mcp` re-checks grants per tool, and
  every write by a scoped key is recorded under the server-fixed actor
  `agent:key:<name>` (`write_actor` / `ApiKeyClaims::agent_actor`) —
  except a key the owner minted `own_device` (one of their own devices:
  the phone's lock-screen capture, a Shortcut), whose writes are the
  owner's words (`human:device:<name>`; captures `adapter:capture:<device>`)
  and which reaches nothing more —
  the activity list and Undo trust that, so a new write handler a scoped
  key can reach MUST use `write_actor`, never the body's actor. A
  `grants` column that fails to parse reads as deny-all, never as an
  unscoped key. `key_requests` holds a minted secret for ONE collection
  and erases it.
- **AREA-LIMITED KEYS (`grants.areas`).** The limit is part of the
  per-THREAD read scope (`ReadScope` = vault viewer + areas) that
  `Store::scope_guard` / `scope_hider` apply; `auth_middleware` enters it
  around every poll of the request (`crate::scoped::Scoped`). Work a
  request hands to another thread keeps it only through
  `crate::scoped::{spawn_blocking, spawn}`, and reads must go through
  `scope_guard`, never a hand-written `!= 'private'`. That is why Ask is refused to these keys
  (`KeyGrants::validate`) and `read_query` errors under a filter. Before
  adding a GET to the scoped-key table, check both; see
  `test_area_filter_limits_read_surfaces`.
- **`custom` endpoint + per-tier routes are NODE-LOCAL** (`llm.custom.
  base_url`, `llm.route.<tier>.{provider,model}`): they never join the
  LLM sync envelope, and `llm.model.<tier>` keeps meaning "the ACTIVE
  provider's model" because it does sync. A loopback address pushed to
  the peer would silence that node's thinking passes.
- **PEER SYNC IS MULTI-MASTER MERGE, NEVER REPLACE-ALL.** Both nodes are
  authoritative (the owner works on both). `apply_operational`
  (store/sync.rs) upserts each row by `id` under last-writer-wins (newer
  `updated_at`, else `created_at`) and NEVER `DELETE FROM <table>`; rows unique
  to the receiver survive (union). The relationships leg merges per logical
  key (entity + kind + t_valid), NOT by the per-node-random relationship id.
  The LWW gate reads the local timestamp as `Option<String>` (NULL-tolerant —
  a legacy NULL 500'd the whole mirror on 2026-07-27). Re-introducing any
  `DELETE FROM`/replace-all here re-breaks the data-loss incident. Deletes
  propagate ONLY via the journal-sync leg (0.5.39, `/v1/sync/journal`):
  `apply_journal` replays entries idempotently by `entry_id` and a `delete`
  tombstone at-or-after the local row's stamp removes the row (LWW — a newer
  local edit survives). That is the ONE cross-node deleting leg; the mirror
  has a matching resurrection gate (skips a sender row older than a local
  delete-tombstone) — remove either and purge/resurrection bugs return.
  Received journal entries get actor `peer:<name>` (that IS the echo guard;
  `journal_export` filters them).
  **An apply must count only rows it CHANGED** (0.5.70): a sync step that
  sees changes touches the bus, and the bus schedules the next sync — so
  re-applying identical rows and counting them made the laptop sync every
  ten seconds forever (0.5.68). `apply_operational` skips a row identical
  to the local one; the LWW legs skip `>=`-stamped rows.
- Identity is attribute-based and claimable: `identity claim` binds an
  email/handle to an entity (creating a missing Person, merging a previous
  owner reversibly); reads must stay merge-group-aware (dossier/recall/
  traverse/ribbons resolve the group). Identities and entities carry scopes;
  episode scope is inferred (project > identity > `personal`) when not
  explicit; scope labels are correctable (`hearthloom scope`, incl.
  `--by-source`, and the idle-time LLM classifier), episode content is not.
- Episodes are append-only; edges are bi-temporal
  (`t_valid/t_invalid/t_created/t_expired`) — contradicted edges are
  invalidated, never deleted; merges are reversible events (bias to
  under-merge; low-confidence candidates go to `merge_review_queue`).
  **`sweep_case_duplicates` must never absorb a human-declared entity**
  (`declared_importance>0`) or two entities joined by a `contains` edge:
  a declared Project ("Hearthloom") named like its auto-minted repo
  ("hearthloom") is intended, not a duplicate — the sweep ate it twice
  before this guard (2026-07-11). `declare_project` also retires any
  pending same-name review as `distinct`. `project_repos` filters
  contains-children to `type='Project'` so a repo's file Artifacts never
  render as "repos"; `entity_dossier` resolves id-or-name (id-first) so a
  modal can open the exact entity past a case-collision.
- Raw episode content is never discarded after extraction.
- **`#r<n>` SUPERSEDES ONLY FOR `REVISIONED_SOURCES`** (ingest_apply.rs:
  photos, visits, contacts — 0.5.66). A source whose `source_ref` a
  third party can choose (mail Message-ID, calendar UID, a URL) must
  never be added: it lets a sender hide a known record.
- **THE SUPERSESSION PATTERN (v47 — reuse this for any "append-only
  episode needs a fuller replacement" situation).** Episodes stay
  append-only in CONTENT; `superseded_by` is lifecycle METADATA (the
  scope-correction precedent). To replace an episode with a fuller
  version: (1) mint the replacement under a VERSIONED source_ref —
  `<base>#r2`, `<base>#r3` — so UNIQUE(source, source_ref) holds and the
  deterministic episode id (hash of source+ref) makes concurrent minting
  on two peers CONVERGE on the identical row; (2) the marking is
  receiver-side and derived purely from the ref (`ingest_apply` marks
  base + lower revisions `superseded_by = <new id>` whenever a
  revisioned episode lands) — supersession therefore syncs with ZERO
  wire changes; (3) guard a re-mint with a SUPERSET check against a
  minted-ledger table (photo_day: count + keys-hash) so a node that
  hasn't synced the other source's data yet can never clobber a rich
  episode with a subset — and legacy episodes adopt via a canonical
  body line ("photos: N") rather than re-minting equals; (4) superseded
  rows keep content but are excluded from guarded read surfaces
  (every reader adds `superseded_by IS NULL` — the RETIREMENT invariant's
  test holds them to it; Browse, the forensic workbench, shows them only
  under "retired & replaced") and their location_points are deleted, the
  new revision re-indexing its own. Candidates for reuse:
  health_day absorbing a later fuller export, chapter re-mints.
- BRAIN.md regeneration must stay byte-stable for unchanged store contents
  (≤200 lines / ≤25KB); the self-model section renders the latest stored
  consolidation digest, never fresh LLM output.
- The MCP tool count stays at four; add detail levels, not tools (search and
  recall both take `detail: summary|full`; summary is the default).
- The system must run degraded-but-useful with NO LLM provider configured
  and with no embedding model present (search falls back to FTS-only).
- Embedding/LLM spend happens only in the consolidation cycle — never on the
  query path — with TWO user-invoked exceptions: `hearthloom query` / Ask
  (the RLM deep-query engine; every run traced in ask_log) and goal
  creation/restatement (decomposition + backlog scan — a deliberate act).
  **Ask is ALWAYS read-only, but it may SUGGEST write operations** via
  ```action fences rendered as buttons that PREFILL the Desk (task or
  goal); a human must complete every save — two-phase, per Jon 2026-07-09. LLM writes land
  as episodes/labels with mutations, never as silent edits (digest = episode
  source `consolidation`; extraction links role `mentioned`; classification
  updates scope with `scope_classified`). Sources `mbox` and
  `apple-health` are deliberately OUTSIDE the extraction allowlist
  (37k+ emails / numeric aggregates would burn months of budget; both
  adapters mint their entities structurally) — don't re-add them.
- Hybrid search fusion stays at low vector weight (RRF k=60, weight 0.05);
  the vector leg is recall backfill, not a ranker (the design log).
- **THE VAULT IS PER VIEWER (0.5.71, Jon).** An unlock belongs to the
  viewer that made it — `key:<id>`, `session:<digest>`, or `local` for the
  keyless caller of a node with no login — and only that viewer's reads
  see private; reads with no viewer (consolidation passes, sync) are
  locked. Request work that changes thread MUST use
  `crate::scoped::{spawn_blocking, spawn}`; plain `tokio::spawn` only for
  background work a request merely starts (it must not inherit anyone's
  unlock). A read that loses its viewer shows the locked view — it fails
  safe, but the owner then misses private data there. Caches keyed by
  vault state keep one slot per view (dashboard.rs `stats`). A locked
  viewer can move things INTO the vault but never out:
  `Store::relabel_keeps_private` on every relabel, and `update_note`
  refuses a vaulted note (importer refreshes excepted — they never change
  the label). Logout forgets the session's unlock.
- **A SETTING A PASS CLUSTERS BY IS SYNCED INPUT (2026-10-25,
  `visits.calendar_alone`).** Both nodes run the same passes over synced
  data, so a setting that changes what a pass mints must reach both — an
  operational-mirror envelope with its own stamp riding BOTH legs (push in
  sync.rs, the hub's GET /v1/sync/operational) — and the output must
  converge while the nodes still disagree: never let the setting shift
  refs that don't depend on it (the `c<n>` series), and sync any one-shot
  decision it triggers (the cleanup start). Known gaps: `ingest.device.
  scope` has no setter and doesn't sync; `profile` and `llm_budget` ride
  the push leg only.
- **PROPOSALS CAN'T CHOOSE HOW THEY LAND (0.5.71).** Outside proposals
  (MCP `propose_memory`, `/v1/propose`) pass `outside_proposal`: their
  words and a kind from `PROPOSAL_KINDS`; provenance fields are the
  server's. Accepting lands source `memory`, trust `agent`; a
  virtual-source promotion keeps its provenance only when that source is
  registered; topic pages only from `agent:consolidation`. On
  `/v1/ingest`, a key that is neither the owner's nor a peer's can't use
  `OWNER_ONLY_SOURCES` and its `self` trust becomes `imported`.
- **The vault defaults LOCKED**: every server-side read path excludes
  `scope='private'` while locked (search/recall/dossier/traverse/ribbons/
  browse/stats/MCP/Ask; raw-sql via TEMP-VIEW shadowing in `read_query`),
  with zero leakage (no "N hidden"). Ask history/detail (v9): an ask run
  while unlocked is flagged `ask_log.sensitive` and hides again while
  locked (its answer/transcript can quote private episodes). Persisted
  artifacts (BRAIN.md, daily
  digest) and cloud-LLM passes (classification, extraction) exclude
  private ALWAYS — vault open or not. Privacy rules relabel retroactively
  and at ingest (outranking all other scope inference); the classifier may
  never overwrite a private mark. New read queries MUST carry the guard
  (`Store::scope_guard`) — see `test_vault_locks_every_read_surface`.

## Full export (THE classification contract — never bypass it)

`GET /v1/export/*` builds one zip that can rebuild the whole system on a
fresh node: `manifest.json` + `hearthloom.db` (VACUUM INTO snapshot,
sessions stripped, the owner's login preserved) + `tables/*.json` (the
readable hierarchy) + `files/…` (raw/, notes/, config). Starting it
REQUIRES the vault unlocked (an export contains everything). Restore =
the wizard's "restoring from an export?" upload → stages
`hearthloom.db.incoming` + files, daemon exits, and
`export::apply_incoming_restore` swaps it in at next boot (fresh/empty
nodes only, previous db kept as a timestamped backup).

**The enshrined contract — new data CANNOT dodge the exporter:**
- Every DB table must be classified in
  `hearthloom-core/src/store/export_all.rs::TABLE_CLASSES` as Exported
  or Excluded(reason). `test_every_table_is_classified` walks
  sqlite_master and FAILS THE BUILD on an unclassified table — a
  migration that adds a table forces the author to decide, in code,
  with a reason.
- Every dir/file at the home root must be classified in
  `hearthloom-daemon/src/export.rs::DIR_CLASSES` — same test discipline
  (`test_known_home_entries_are_classified`); and at RUNTIME an
  unclassified directory is EXPORTED with a manifest warning, never
  silently dropped. A new integration that writes files under
  `~/.hearthloom/<anything>/` is therefore exported automatically.
- Secrets NEVER export: users' credential hash is the one deliberate
  exception (it's the owner's own — restore keeps their password);
  sessions/api_keys/.env/.env.sealed are excluded with reasons in code.
- Killswitch interaction (Jon 2026-07-18, supersedes allow_export
  gating): `/v1/export/*` and `/v1/vault` ALWAYS survive the killswitch
  — a user mid-shutdown or in a grace period can unlock and take their
  data; the killswitch screen carries the unlock→prepare→download path.
  `killswitch.allow_export` is stored but no longer gates anything.

## Performance invariants (each encodes a fixed daemon-freeze — see
the design log 2026-07-08 post-mortem)

- **Traverse must be capped** (`traverse_top`, per-node strongest-first
  LIMIT): uncapped depth-2 over the full graph is 140s + a held store
  lock. HTTP defaults top=25; the constellation uses depth=2&top=12.
- **Never join two CTEs on computed columns at substrate scale** —
  SQLite nested-loops unindexed intermediates (timeline_notable took
  60s at a full-life window). Use window functions or correlated
  subqueries against indexed base tables — but never a per-row
  correlated AGGREGATE as an ORDER BY key over the whole substrate:
  semantic_pairs ranked 252k episodes by `(SELECT COUNT(*) …)` each —
  31s/cycle under the store lock (0.5.57 fix: GROUP BY the child table
  once, join the parent by primary key).
- **Never join episode_fts on episode_id equality** — FTS5 can't index
  it; that join is a full FTS scan per outer row (quadratic). Join
  `f.rowid = e.fts_rowid` (v12 pointer, set at insert) or use MATCH.
  The REVERSE direction (`FROM episode_fts MATCH … JOIN episodes ON
  e.fts_rowid = f.rowid`) needs `idx_episodes_fts_rowid` (v45) — it was
  missing for a year of migrations and a friend-connect probe (q=hello,
  thousands of FTS matches × a 252k-row episodes scan each) ground a
  hosted tenant into a full wedge on 2026-07-28. Friend-surface
  handlers (network_search/network_fetch) are spawn_blocking'd — remote
  callers' timed-out retries must never stack grinders on runtime
  workers.
- **Any loop that repeatedly takes the store lock must sleep ≥40ms
  between batches AND keep each hold short** (embedding backfill, bulk
  applies, source refresh): HTTP handlers block on the lock inline on
  runtime workers, so one long hold parks every worker. The store mutex
  is a parking_lot **FairMutex** since 0.5.6 (FIFO handoff — a grinding
  pass queues behind waiting handlers on re-acquire; the old unfair std
  mutex let the grinder re-win every release and a 1-vCPU tenant 502'd
  through its whole boot cycle). Fairness fixes ordering, NOT hold
  length — a single monster query still freezes everything for its
  duration.
- **THE HANDLER CONTRACT (2026-07-19, the /v1/stats wedge):** a heavy
  read handler must (a) run its store work via `spawn_blocking` and
  (b) cache if anything polls it (galaxy 10min, stats 60s — keyed by
  vault state). A synchronous monster query on a runtime worker is
  uncancellable (client disconnect can't interrupt rusqlite), holds
  the store lock for its whole duration, and eats one of only four
  workers; the store gate bounds how many requests PARK, not how long
  one GRINDS. The monitor loop's per-tick store touches are
  spawn_blocking'd for the same reason (it was the third eaten worker
  in every wedge). Diagnosis recipe that found it: `eu-stack -p <pid>`
  in the machine + `addr2line -f -C -e <binary> <hex offsets>`
  (offset = addr − first-map base from /proc/<pid>/maps) names the
  exact function every thread is stuck in — evidence, not suspicion.
- **A polled endpoint needs single-flight AND stale-while-revalidate,
  not just a TTL** (2026-10-06, the laptop "clearing the desk…" freeze):
  /v1/stats had a 60s cache and ran on the blocking pool, and still
  wedged the node — on a cold page cache one compute took 26s and every
  poll during it started ANOTHER, each holding the store lock in turn.
  The pattern in `dashboard.rs::stats` is the template: one compute at a
  time, serve the last value while refreshing in the background, never
  serve a value across a vault-state change. And never ask for N
  GROUP BYs of the same big table when one scan can feed them all
  (`Store::episode_composition`).
- **NEVER touch a network/userspace mount on a thread of the daemon**
  (2026-10-07): a wedged mount (stale NFS, dead FUSE) blocks the thread
  in the kernel uninterruptibly, and a process holding such a thread
  cannot even EXIT — the daemon hung in exit, kept :43278, and launchd
  could not replace it until a reboot. Liveness goes through
  `virtual_source::mount_alive` (a child-process probe with a deadline,
  failures cached 30s) BEFORE any read, and every Ask tool goes through
  `rlm::run_tool_bounded` (blocking pool + 40s timeout), never
  `run_tool` inline. Known gap: a mount that dies MID-read still strands
  a pool thread; moving virtual-source I/O wholly out of process is the
  real fix.
- **No consolidation pass may run >100ms between awaits on a runtime
  thread** (THE PASS CONTRACT, consolidate.rs top comment, 2026-07-19):
  pure store+CPU passes are spawn_blocking'd WHOLE and still batch
  their lock holds small (the refresh_local_sources pattern); on a
  1-vCPU tenant a single long hold parks every worker, the tokio IO
  driver goes unpolled, and the node 502s while "working". Every pass
  logs its elapsed time (PassClock) — read one cycle's log to name a
  hog, never guess. IN-lists over sync backlogs must chunk ≤500 ids
  (SQLite's 32766-variable cap; a 48k-episode delivery aborted every
  tenant consolidation cycle in run_intents before this).
- **Deploy builds MUST be `--target aarch64-apple-darwin`** — the
  default toolchain host is x86_64 (Rosetta): everything runs ~5-30x
  slower and tract loses AMX.

## Environment gotchas

- **TCC/Full Disk Access is per-app** and only affects newly launched
  processes; a shell started by another tool may not have it.
  The daemon does — use `/v1/ingest/local` (the CLI falls back automatically),
  or the inbox drop folder.
- Secrets live in `~/.hearthloom/.env.sealed` (sealed 2026-08-13:
  ChaCha20-Poly1305, master key in the login keychain service
  `hearthloom-master-key` — core secrets.rs). The daemon decrypts in
  memory at boot; there is NO plaintext .env anymore.
- **Throwaway ("smoke") daemons**: `HEARTHLOOM_HOME=<scratch dir>
  HEARTHLOOM_PORT=<free port> HEARTHLOOM_MASTER_KEY=<anything>
  target/debug/hearthloom daemon run` (the master key keeps it off the
  owner's Keychain entry). Its first source refresh imports THIS Mac's
  local sources (Claude Code sessions, Chrome history) into the scratch
  home — delete the home when done. For a
  real model, copy `~/.hearthloom/.env.sealed` into that home (the
  keychain master key opens it) AND set `HEARTHLOOM_LLM=anthropic` —
  without the latter the provider is `none`. A fresh node is open (no
  login) until `POST /v1/auth/bootstrap {email,password}`; test auth
  behaviour AFTER bootstrapping, never on an open node. Check a port is
  free before using it (`lsof -iTCP:<port>`). In zsh never name a
  shell variable `GID`/`UID` (read-only specials).
- **Throwaway daemons and embers:** on a Mac a test daemon that
  installs an ember with a launch entry writes a REAL LaunchAgent
  (`~/Library/LaunchAgents/com.hearthloom.plugin.<name>.plist`) unless
  started with `HEARTHLOOM_SIDECAR_BACKEND=supervisor`. Always set it
  for smoke tests; if you forget, `launchctl unload` + delete the plist.
- `POST /v1/sql` takes `{"query": …}` (not `sql`); it is vault-shadowed.
- Shell working directory persists between Bash calls — `cd` back to the
  repo; cargo/node may need to be added to
  PATH in fresh shells.

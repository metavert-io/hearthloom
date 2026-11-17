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

## Build checks (run all of these before calling anything done)

```
cargo fmt --all
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

GitHub Actions runs the same checks on Linux (how hosted nodes run) and
macOS for every push to `main` and every pull request
(`.github/workflows/ci.yml`: unformatted code and clippy warnings fail
it), with CodeQL and a gitleaks secret scan alongside: check `gh run
list` after pushing. CI uses the newest stable Rust, so a Rust release
can add clippy lints that fail it until they are fixed. Because of the
Linux job, a shell-out to a macOS-only tool (`ditto`, `launchctl`,
`osascript`…) needs a fallback or a `cfg` gate. `audit.yml` checks
`Cargo.lock` against RustSec when dependencies change and once a week
(`deny.toml` holds the policy). GitHub's advisory database, which
Dependabot uses, misses most RustSec advisories, so a red audit is the
signal to `cargo update -p <crate>`.

**CodeQL false positives.** CodeQL treats every argument of an axum
handler as outside input, the server's own `State` too, so a path built
on the home folder read as user-controlled wherever a handler reached a
file read (most of the path-injection alerts dismissed before
2026-11-17). The model pack in `.github/codeql/extensions/hearthloom-models/`
(code scanning loads it by itself) marks what `Home`'s folder methods
return as safe for path injection; a new `Home` method that returns a
folder gets a row there. Path parts that come from a request are still
tracked. CodeQL runs from `.github/workflows/codeql.yml` (advanced
setup; GitHub's default setup stays off, or it refuses the workflow's
results), whose Rust extractor leaves out `#[cfg(test)]` code
(`CODEQL_EXTRACTOR_RUST_OPTION_CARGO_CFG_OVERRIDES: "-test"`): test
fixtures were most of what it reported. The logging query still flags a
value whose NAME sounds sensitive (`uid`, user id, session id, account,
token, password, health terms) once it reaches a printed or logged
message, numbers included, so don't name such values that way.

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
  **currently v73**: v73 data only — git and Substack rows an earlier
  build stored at trust `self` become `imported`, as v61 did for the
  sources it knew (`provenance::v73_relabel_sql`; no table changes, so an
  older build still opens the file), plus `idx_agent_tasks_finished` and
  `idx_agent_tasks_span` (the owned-sources check looks up only an agent
  run going while a commit was made — see THIS SOURCE IS MINE; it replaces
  the `idx_agent_tasks_long` an earlier build of v73 made) and
  `agent_tasks.harness` (the harness each run started on, synced with the
  task; an added column an older build ignores) and
  `repair_copies.recorded_at` (when each run's copy was recorded on this
  node, the relay stamp's clock: the repairs leg sends with a run only the
  copies recorded since the peer's cursor; rows from before take their
  run's stamp; an added column an older build ignores; its index
  `idx_repair_copies_recorded` finds the copies a page's stamps cover
  without reading the whole set) — these five are
  made whenever a v73 database lacks them, as one an earlier build of v73
  made can, v72 `repair_revived` (the copies an undone owner
  repair run made that a later read of the owner's files put back in view
  — a set every node unions, carried with each run on /v1/sync/repairs, so
  every node puts the copy back; see OWNER REPAIRS), v71 `repair_copies`
  (the copies each owner repair
  run made, on any node — a set every node unions, carried with each run
  on /v1/sync/repairs; an undo reaches a copy through any run that made
  it — see OWNER REPAIRS), v70 `mail_thread_messages.memory_refs` (the keys
  each message's memory can be stored under, kept in the observing node's
  thread index so triage's last look before a model finds a private
  memory by the import's key, as `observe` does — see MAIL TRIAGE), v69
  `repairs` (owner repair runs — synced on
  /v1/sync/repairs, LWW by updated_at, an undo stays undone — plus the
  partial index `idx_episode_origin_repair` that finds a run's corrected
  copies; see OWNER REPAIRS), v68 `owned_sources` ("this source is mine": the
  repositories and publications the owner says are theirs, keyed per
  source — git by remote host/path, substack by host; mirrored +
  journaled like mail_rules, deterministic id from source + key; read
  only when the taint checks run — see THIS SOURCE IS MINE), v67
  `photo_replaced` (every key a photo point took
  the place of: a Mac scan records the key an earlier scan gave each photo
  — the library's file name — whether or not it is stored here, and moves
  a point stored under it to its original-name key, the phone's identity;
  each point's export carries all its replaced keys as `replaces`, so every
  peer drops the old points, and a replaced key is never stored again, as
  a point or a `photos` location observation. A point that moves or
  arrives takes a position past every removed point's
  (`photo_point.removed_position` in consolidation_state) — the photos leg
  pages by rowid, and a reused rowid sits behind a peer's cursor. A
  duplicate dropped from a day this node minted lowers that day's ledger
  count only when the day was minted with it (the point stored no later
  than the day's `minted_at`), and the point that takes a minted point's
  place in that day is dated to the mint, so the photo's next merge still
  finds it counted; a union that equals the minted set puts the
  ledger count back to the union's in the finalize pass. An observation
  under a replaced key moves with the photo even when its point never
  arrived — store/photo.rs), v66 several mailboxes (`mail_drafts.account`,
  `mail_matches.account`: the mailbox a draft answers from and a match
  came in — see SEVERAL MAILBOXES), v65 reply drafts (`mail_drafts`
  mirrored + journaled — see DRAFTS NEVER SEND), v64 embers' mail watches (`mail_watches`,
  `mail_ember_views` mirrored + journaled; `mail_matches` the delivery
  queue, its own id-keyed leg /v1/sync/mail_matches — see EMBERS SEE
  MAIL), v63 mail threads + the owner's mail rules
  (`mail_threads`, `mail_rules` mirrored + journaled; `mail_thread_messages`
  the observing node's index — see MAIL TRIAGE), v62 goals carry `updated_at` (`intents`; bumped by
  every write to a mirrored column, never by the node-local watch
  progress — see THE OPERATIONAL MIRROR invariant), v61 provenance (`episode_origin` — where each
  episode first entered one of the owner's nodes and the route it took;
  `agent_tasks.inputs`/`outputs`; and the one-time relabel of others'
  words from `self` to `imported` — see the PROVENANCE invariant), v60
  the movement layer (`movement_observation`
  with `subject` me|vehicle:<ember>:<id>, `vehicle_profile` +
  `trip_attribution` synced LWW via /v1/sync/vehicles, movements via
  /v1/sync/movements, node-local `trip_day`; daemon trips.rs decides
  whose drive each car drive was — see the TRIPS invariant), v59 reversible retirement (`retirements` synced,
  `retirement_rows` the node-local undo record, `summary_inputs` synced,
  `entities.hidden_by`, `sources.retired_by` — see the RETIREMENT
  invariant), v58 `nodes` (synced node list; `agents::publish_self`
  every sync AND every 5 minutes from the agent runner — the hub runs no
  sync cycle of its own, so before 0.5.74 its entry aged from its last
  restart; the thinking lease reads it), v57 relay stamps (`synced_at` on every timestamp-synced
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
- `crates/hearthloom-adapters` — git, ICS (recurrence is hand-written on
  purpose: read the decision note at the top of `ics/rrule.rs` before
  proposing a library), mbox (streaming; `LabelFilter`
  over `X-Gmail-Labels` — Spam/Trash always dropped, kept episodes record a
  `labels:` preamble line; the first From/To/Cc name the people, as the
  archive index reads them; mail with only an HTML part keeps its links'
  and images' targets after the text, a `links:` block — capped like the
  text, a target the room leaves out keeping its host, so a domain rule
  made later still finds the memory (hosts have a room as large; a message
  with more than it holds is drafted private, and the mail repair's
  corrected copy of one ends private); every mail import (Gmail
  pull and re-read, inbox, the detached mbox run — `ingest_batched_checked`
  —, mail repair) also checks the privacy rules over the whole decoded
  message, `mbox_text::rules_match_message`, and drafts it private on a
  match: a rule matches the memory as it matches the archived message.
  That label, like the hosts-left-out one, lands on the NEW memory only,
  never on a copy stored before under the Message-ID (the Gmail pull and
  re-read, the inbox and the detached run pass no such draft to
  `label_private_on_reimport`): the stored text is cut and leaves parts
  and headers out, so what made the message private can lie where no
  comparison with it reaches, and other mail written under a known
  Message-ID compares the same there. Only an import asked to be private
  (`--scope private`) moves a stored copy into the vault, and only one
  with the same date, sender and text (`same_mail_stored`,
  `same_mail_text`: the header lines and text equal but for the `labels:`
  line, the stored copy allowed to lack the `links:` block; a copy an
  earlier version left encoded counts through its repair's corrected copy.
  A Message-ID, date and sender are the sender's choice, and other mail
  under a known one must not vault it). A rule made after the import sees only the stored
  text — what the cut left out past a host it doesn't), chatlog
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
  writes → operational.changed, rename → entity.renamed, scope/privacy
  (an import that moves a stored copy into the vault too,
  `local_import::vault_stored_copies`) → privacy.changed, a node's own
  row → nodes.changed (0.5.99,
  `agents::publish_self`: only when `announce_fingerprint` changes, ≤1 a
  minute — keep readings that change by themselves (free space, archive
  sizes/dates/progress, `last_seen`) OUT of the fingerprint, or every
  publish becomes a hint and nodes wake each other); EVERY hint (and resync) triggers one debounced
  sync_once, single-flighted with the 5-min tick via automation key
  `peer-sync-early`; 404/405 → poll-only feature-detect both directions;
  willingness filter `bus::offered` at fan-out — friend grants slot in
  there with federation; notes in the design log 2026-07-15);
  inbox drop-folder watcher (`~/.hearthloom/inbox/` — zips, mbox, ics,
  conversations.json, Finder-dragged chat.db/Photos.sqlite; zips AND tgz
  unpack in-process writing only plain files and folders inside the
  folder, within `unpack_budget` (what the temp and home disks can spare
  less 1 GB, counted as written for a tgz); staging an export into
  `archives/<key>` refuses a tree with links/special files
  (`downloads::only_plain_files`) and holds the run-now import's
  single-flight key (`downloads::import_claim_key`), so a folder is never
  replaced while its import reads it); an owner's import that reads a
  record as private (ChatGPT "do not remember", or `--scope private`)
  relabels the copy stored before (`Store::label_private_on_reimport`:
  label only, into the vault only; never for a peer's or another key's
  batch, nor for mail a privacy rule or the hosts' room made private — see
  the mbox adapter above; a flagged conversation counts even when no draft is made of it —
  `openai::read_export`/`chatlog::read` report every flagged one, so a
  kept branch with no turns still vaults what an earlier reader stored;
  every copy of a day — a growing source's or a photo day's — goes in
  under the import's stamp, however recently one was taken out, and the
  copies the record's label reaches count as moved;
  a few hundred records per hold of the store; every owner's import path
  — `/v1/ingest`, `/v1/ingest/local` incl. the detached mbox and openai
  runs, the inbox (its mbox drops too), the Gmail pull and its label
  re-read — goes through `local_import::vault_stored_copies` (or
  reports the count up, the inbox), which sends `privacy.changed` — also
  when the apply itself moved the stored copies, a fuller copy landing
  private taking its day's with it (`IngestReport::private_copies`); a
  private commit is named under its repository's former addresses too,
  `local_import::private_refs`);
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
  `ember install <folder|zip>` (0.5.84, cli embers.rs — unreviewed, the
  same consent screen; `--yes` + `--grant <scopes>`; an upgrade keeps
  held scopes and WITHHOLDS new ones unless granted — never grant what
  nobody named; exit codes 0/3/4/5/1 and the `--json` `status` strings
  are a contract scripts (minidash) depend on: don't renumber them),
  `ember enable|disable|config`, `mine list|add|remove` ("this source is
  mine", v68 — `add` needs the vault unlocked), `repair mail|calendar
  --file <ics>…|chatgpt --export <path>|health --export <path>|copies|
  photo-days|list|undo <run> --yes` (v69, cli repair.rs — owner + vault
  open; a dry run unless `--apply`, refused while a node of yours doesn't
  know repairs unless `--allow-older-nodes`; `undo` explains itself
  unless `--yes`; prints ids and short reasons, never
  content; `chatgpt`/`health` name an export on this machine that the
  node reads in place, so they refuse `--url`),
  `notify`, `logs -f`, `completions`. **A command that talks to a node
  builds its client with `Client::with_url(home, url.clone())`** —
  `Client::new` ignores `--url` (query and a dozen others did until
  0.5.84); one that reads this machine's files calls `local_only` and
  refuses `--url`. For agent shells: `hearthloom
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
source over `~/.hearthloom/archives/`). **The live watch (0.5.81,
`gmail_watch.rs`)**: one IMAP IDLE session on All Mail per MAILBOX
connected on the node (several since 0.5.98 — see SEVERAL MAILBOXES),
re-issued every minute (so settings, a disconnect or a fence are noticed
within one), runs `gmail_pull::pull_once(state, mailbox)` on any change
and at the catch-up interval (`gmail.watch` on|off, `gmail.watch.every_minutes`
1–60, node-local; `.<id>` suffixed for an added mailbox). `pull_once` is
THE one pull path of a mailbox and each mailbox's pulls are serialized by
its own lock (`gmail_pull::pull_lock`; `pull_all` skips a mailbox whose
watch is mid-pull) — never add a second puller of a mailbox with its own
cursor, and never a node-wide lock (one stalled mailbox would hold every
other one and the consolidation cycle). Every Gmail connection opens
through `gmail_imap::open_gmail`: the address lookup (`RESOLVE_TIMEOUT`,
on a thread of its own — the system resolver has no deadline; past
`MAX_STALLED_LOOKUPS` stuck ones a connect fails at once), connect, TLS
handshake and every read and write have a deadline (`CONNECT_TIMEOUT`,
`IO_TIMEOUT`; the `Bounded` stream keeps one after IDLE clears it) —
never `imap::ClientBuilder`, which sets none. A refused sign-in STOPS that mailbox — its watch, pull,
search session and drafts all check `mailboxes::refused` — until it's
saved again (`recheck_mailbox(id, true)`): retrying a bad Gmail password
risks Google locking the account. "Refused" is a TYPE
(`gmail_imap::SignInRefused`, made only from a `NO` reply to LOGIN — every
one but one that says it passes with time: an RFC 5530 `[UNAVAILABLE]`,
`[INUSE]`, `[LIMIT]` or `[SERVERBUG]`, or Gmail's connection or bandwidth
limits, `refuses_credentials`; never a list of refusal phrases, which
Gmail words many ways), never words in an error message: a dropped
connection or `[UNAVAILABLE]` at sign-in is an ordinary retry. A pull
also re-reads the labels of mail it already holds (`gmail_pull::relabel`,
≤ every 10 min: the newest ≤500 of the last 30 days, plus the archive's
next 500 records by index row, `gmail_relabel_row`, wrapping — so every
pulled message is read again in time). A record's UID counts only while
Gmail still has that Message-ID under it (`gmail_imap::current_labels`):
after a UIDVALIDITY change the old numbers name other messages, so a
message its UID doesn't find is looked up by Message-ID — searched by the
id its angle brackets hold (`searchable_id`: a key with a comment beside
the id, or folded, is no search term) and matched back by the import's
key. The `Category Personal` search asks for exactly the UIDs read (a
stretch's first and last UID can be a mailbox apart). A batch Gmail
refuses or answers unreadably is read again one message at a time and
only that message is left out (`per_item`); a connection failure still
fails the re-read. Mail that now
passes the Core strata becomes a memory (drafted from its archived copy);
the labels then land in the index (`mbox_index::set_labels`, kept in its
`label_updates` table, which a rebuild carries and indexes under — the
archive keeps the labels as received) in the SAME transaction as the list
of messages still to observe (`gmail_observe_pending` meta); then mail
that GAINED a label goes through `mail_triage::seen`/`observe` like new
mail, so a thread forms and the embers' watches see it, and leaves the
list only once recorded — a pull stopped in between, or an observation
that failed (arrival's too, the first sort's), is finished by the next
pull, which observes the list from the index before it connects (it
waits neither on Gmail answering nor on the new mail). Never
record labels without that list: a recorded label is never looked at
again. `mbox_index::meta_get` and `labels_of` return None only for a
missing key or message — an index they can't read (busy, locked) is an
error, and an error keeps the list and the message on it; never read it
as an empty list or a message gone. So is every other read of the index's
meta: the pull's cursor (`read_cursor` — read as none, the pull would
append its whole backfill window again), the stretch's position
(`sweep_position`), when labels were last re-read (`relabel_due`: an
error means not now) and `ids_current` (an error, never a rebuild). The
first sort of a mailbox's mail (`mail_triage::backfill`) reads archived
messages as bytes (`fetch_bytes`, never the lossy `fetch`: a rule's word
in a legacy charset must match), and what it couldn't read or record
goes on that list (`gmail_pull::keep_pending`, which takes the
mailbox's pull lock — the pull rewrites the list). Index ids are the import's key (`mbox::message_id_ref`, folded
headers included — `mbox_index::scan_headers`); a live archive indexed
before that (no `ids` meta, `ids_current`) is rebuilt once by its pull.
A rebuild holds the index's writer lock (`mbox_index::writer`) for its
whole run, and so do `set_labels`, `meta_set` and `append_index` — a
write that raced a rebuild was lost when the rebuilt file took its place.
A rebuild that can't read the old index's meta rows and label updates
(busy, locked, out of reach) fails and leaves it in place
(`mbox_index::carry_over`, one read transaction): the meta rows hold the
list. Only a file that is no database at all, or damaged, is rebuilt
over, without what can't be read: damaged meta rows take the label
updates with them (the labels are read from Gmail again), damaged label
updates go alone (the meta rows read — cursor, list — are kept). Never
make the carry best-effort again.
A message's privacy is read again in `observe` (`private_now`): the pull
makes its memory after reading it, and the memory can arrive private
(the sender's identity is). A message observed again keeps the thread it
joined — that thread's privacy and class decide what the watches see
(`observe`, `Store::mail_thread_of_message`). One that joins a thread
only later (it gained a label) and was written before the thread's
latest counts, but leaves that latest message — what triage reads — its
text and the thread's state as they are (`mail.rs::written_before`; a
latest dated after now isn't believed — the Date header is the sender's,
and a future one would keep every later message from the thread). Found private now, it
makes that thread private (`Store::mail_observe`, journaled; nothing
there makes a thread un-private), so triage never takes it. The label
re-read makes the threads of the memories it just made private at once,
from the record alone (`mail_triage::close_private_threads`, before it
reads the index again — a sorted thread is never looked at by triage
again). A memory that turns private some other way (a copy from another
node, the owner's label) leaves the thread as it was, so triage also
looks up the memories of each thread's messages right before a model
reads it, by the keys the thread index kept (`memory_refs`, v70;
`mail_triage::for_model`; an error fails the round): a thread with a
private one is made private (`Store::mail_thread_make_private`) and left
out. A message recorded before v70 (keys NULL) has them read again from
the live archive that holds it under its id (`keys_from_archive`,
`mbox_index::head_of`) and kept; so does a thread's latest message this
node's index doesn't hold (another node recorded it — threads mirror, the
index doesn't). One no live archive holds under that id (the import keyed
it otherwise — a second Message-ID header — this node hasn't pulled it,
or the archive is gone) can't be checked, so its thread WAITS — never
sent on its id alone, its label left as it is (a node that took over
another's mailbox holds none of its archive, and a private label is for
good and syncs back); triage pages past waiting threads
(`Store::mail_threads_to_triage_from`, a page at a time on the blocking
pool with a pause between pages) so they never hold up the rest,
and passes over one it found waiting without reading the archives again
(`mail_triage::waits_still`, in memory) until the thread changes, the set
of live archives here changes, or `WAIT_RECHECK` (10 min) passes — a
backlog of waiting threads is looked up once, not by every round. And
whatever reads a
thread's message later (class-waiting watches, drafts) takes its labels
from the index (`mailboxes::labels_now`), never the archived header. A
memory keeps the labels it was made under (episodes are append-only; mail
refs can't be revisioned). It pauses while the node is fenced and publishes
`roles.mail` (any mailbox watched) and `roles.mailboxes` in the node list.
**Gmail live federation (Tier 3)
SHIPPED 2026-08-10** (design log entry same date): the `gmail_pull`
automation (cycle cadence, `gmail_pull::pull_all`; a silent no-op until a
mailbox is connected) pulls new mail over
app-password IMAP (`imap 3.0.0-alpha` DELIBERATELY — the first line
whose imap-proto parses X-GM-LABELS; don't "upgrade" it back to 2.4)
into each mailbox's rolling live archive (`archives/gmail-live.mbox`
for the first, `gmail-live-<id>.mbox` for an added one) + incrementally-appended
sidecar (`mbox_index::append_index` — NEVER append to the Takeout mbox:
its fingerprint invalidates and a full rebuild follows), annotates
categories via an X-GM-RAW search over the UIDs pulled (X-GM-LABELS does NOT carry
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

- **PROVENANCE IS RECORDED ON ARRIVAL, NEVER REWRITTEN (v61, 0.5.78 —
  design log 2026-11-03).** `apply_batch_from` records every newly landed
  episode's origin in the same transaction (`store/provenance.rs::
  record_arrival`, INSERT OR IGNORE: first arrival wins). A synced
  episode's origin travels BESIDE it (pull page `origins`, push body
  `origins`, keyed by source + source_ref) — never inside EpisodeDraft,
  and only a `peer` key's are believed (`http.rs::ingest` clears the rest:
  anyone else would be choosing an episode's provenance). A new path that
  inserts episodes without `apply_batch*` skips provenance; a new sync
  path that applies a peer's episodes must pass its `origins`. Trust is
  decided at the same door: `trust_for` turns a `self` claim from a
  source others write (`OTHERS_WORDS_SOURCES`) into `imported` — add a
  new such importer's source there, never trust its `self`. Owner-only
  views: the provenance names nodes and agents (`/v1/episode/{id}`'s
  owner branch, the CLI, Ask's `read_episode` line).
- **THIS SOURCE IS MINE IS READ AT CHECK TIME, NEVER STORED (v68, core
  `store/owned.rs`).** The owner's list (`owned_sources`) names parts of
  sources in `OTHERS_WORDS_SOURCES` whose text is the owner's: a git
  repository by its remote address (`git_repo_key`: host/path,
  lowercased, no user name, no `.git`; a repo with no remote can't be
  named), counting only commits whose author address (the body's
  `author:` line, checked against the `commit`/`repo` lines and the ref —
  `git_author`) is one of the owner's (`owner_emails`: every connected
  mailbox on any node and its aliases from the synced node list, plus
  the profile card's addresses — NEVER the owner Person's attributes,
  which any commit naming the owner fills) and that carries no one
  else's words (`git_commit_concern`: its record carries the adapter's
  `committed:` and `parents:` head lines (`git_commit_head`) — a commit
  an earlier version imported has neither, can't show whether it is a
  merge or when it was made, and stays outside text (re-importing
  doesn't replace it), and so does one at a shallow clone's cut-off,
  where git shows no parents whatever it had (the adapter reads the
  clone's `shallow` file and writes `parents: not fetched`,
  `GIT_PARENTS_NOT_FETCHED` — never an empty list, which reads as a first
  commit); not a merge — more than one parent, or a default
  merge subject — no `Co-authored-by:` trailer naming another address
  (Claude Code adds one), text read whole up to `COMMIT_TEXT_CHARS`) and
  with no run of one of the owner's agent tasks anywhere from its author
  date to its commit time (`agent_tasks` started_at → finished_at; an
  unfinished one — its agent deleted mid-run, its node gone — runs until
  `LONGEST_AGENT_RUN_SECS` past its latest stamp, the longest agent
  timeout plus the runner's grace, only when the run started on Claude
  Code (`agent_tasks.harness`, kept as the run starts —
  `start_agent_task_run`, the one way the runner starts a run — never
  the agent's setting now, which the owner can change while a job is
  still out), which the daemon kills at its time limit; a Hermes or
  OpenClaw run, a task started again on another harness, or one started
  before harnesses were kept (none recorded), runs on until it is
  closed — Hearthloom doesn't stop those; an amend, rebase or cherry-pick keeps the author date
  and sets the commit time, and the next one sets it again, so an agent's
  rewrite in between leaves no time of its own — never go back to
  checking the two times alone). Git doesn't check an author address, or either date —
  anyone whose commit reaches a marked repository can write the owner's
  — and the UI says so; never describe it as "commits by anyone else
  stay outside". A substack publication by its host, every post counting
  (guest posts too); the adapter keeps a post only under the host that
  served the archive, after redirects, fetches its text there (a
  `canonical_url` naming another host is left out) and writes that host
  as the post's FIRST line (`served from: …`, `substack_served_from`) —
  a post without it (an earlier version's, which filed posts under
  whatever address the server's JSON gave) or naming another host stays
  outside text.
  `first_untrusted_episode` and `entity_names_unverified` (so
  Ask's outbound gate, traverse names, and the agents' taint rule) judge
  each candidate through `OwnedWords::covers`; stored trust never moves.
  Both look at everything outside the listed sources in one cheap
  `LIMIT 1` query first and read only the listed sources' candidates
  whole; `OwnedWords` reads the owner's addresses only when a candidate
  gets that far, once per check, and the agents' runs only for such a
  commit: the ones never closed once per check, and two looks for a
  finished run that ended after its author date and started before its
  commit time — one of a day or less among the runs that finished from
  its author date to a day and five minutes past its commit time
  (`RUN_FINISHED_DURING`, by `idx_agent_tasks_finished`), and a longer
  one among the runs that may have lasted more than a day, a length at a
  time (`long_run_between`: `LONG_RUN_DURING` once for each count of
  digits in a run's whole days, 1 to 6 — a run under 10^d days going at
  the commit time finished within 10^d days of it, so only the runs of
  that length that finished then are read — and `ODD_RUN_DURING` for runs
  whose times can't be read or lie a million days apart, none a clock
  wrote; all by the partial expression index
  `idx_agent_tasks_span`, whose expression and condition the queries
  repeat word for word) — never the task history before or after it,
  however long it grows (a bound on one end only let an old commit read
  every run since, long ones too). They run on every Ask tool result
  and agent record read — keep them off the common path.
  The idle refresh leaves out a known checkout that overlaps one of this
  node's agents' working folders (`consolidate.rs::refresh_git_repos`),
  as it does those agents' Claude Code sessions.
  It fails closed: a writer other than the source's own importer
  (`adapter:<source>` — only the owner may name an adapter, on
  `/v1/ingest` AND the virtual-source register/promote routes an
  ember's `ingest` key reaches; a peer's origin is believed only from a
  peer key), an unknown writer, a trust other than `self`/`imported`, an
  unreadable ref or author line, an unknown author or commit time, a
  record without its parents or serving host, or no known owner address
  all stay outside text. A new source joins only by
  defining its key (and an author reader when items can have different
  authors) in `OWNABLE_SOURCES`; one whose items mix authors or carry
  fetched content goes in `NOT_OWNABLE` with its reason. Adding needs
  `require_owner` AND an open vault (it relaxes the gate); removing needs
  the owner. The owner's addresses are part of the gate: while a
  repository is listed, adding one (profile card, a mailbox's aliases,
  connecting a mailbox — its address and the aliases kept for it, which
  a reconnect brings back; checked before the sign-in is tried and again
  after it) needs the owner and an open vault too
  (`http.rs::may_add_owner_addresses`; only what reads as an address,
  `owned::owner_address`, as the addresses are read — a card line that
  doesn't can never be the owner's, so it is never "new"), and every address change — a
  disconnect too — is journaled into the list's history
  (`owned_addresses_changed`, op `addresses`). These changes take turns
  (`http.rs::owner_addresses_turn`, an async lock — a waiting change
  yields its runtime thread — held from the last check until the
  change is saved, published and journaled, that work running on the
  blocking pool, `in_turn`, so a slow Keychain save holds no runtime
  thread — the turn goes into that work and ends with it, so a request
  that ends early (its client gone) never lets another change in
  before its save is done; never held while waiting on
  something outside the node, such as Gmail's answer to a sign-in): a
  new route that changes the owner's addresses takes the turn too, or an
  address taken off between a check and its save comes back. Stamps
  have whole seconds and a tombstone at or after a row's stamp removes
  it on every node, so a re-add is stamped after the entry's last
  tombstone and a removal's tombstone never before the row's stamp
  (`journal_op_at`). The list shows how many
  stored memories each entry names (`owned_source_stored`, as the viewer
  sees the record — private ones only with their vault open; the count
  steps through the source a repository or publication at a time on the
  `(source, source_ref)` index and reads only the parts an entry's key
  may be in, `owned::ref_part` — a key holding a `#` only the parts whose
  key begins it and the few with no key that can hold one, never a local
  checkout's —, one look per other part — never every
  memory of the source; `owned_entry_stored`, and adding one counts only it), so an entry
  that names none is visible. No MCP tool, ember route (it's on the ember
  deny list) or scoped key reaches it (`test_no_key_but_the_owners_
  reaches_the_list` runs requests through the router). A new taint check
  must go through `OwnedWords` too, or it disagrees with Ask about whose
  words a commit is.
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
- **A HUB LEAVES OUT ONLY WHAT THE ASKING PEER SENT (0.5.74).** Every
  export a peer pulls takes the asker's key name and skips `actor =
  'peer:<asker>'` (`sync_export_for`, `journal_export_for`); the blanket
  `actor NOT LIKE 'peer:%'` is only for a spoke pushing to its hub. Until
  0.5.74 the episode export used the blanket rule on the hub too, so a
  second spoke never received the first one's episodes (the comment said
  "v1 is exactly two nodes"). A new pulled leg follows the same rule.
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
  `taint_guard` switch turns the rule off). A task's taint is NOT fixed
  when it is made (0.5.77): `tool_permission` taints it after any allowed
  tool `outside_content` names (WebFetch, WebSearch, a non-Hearthloom MCP
  tool, a web or friends tool, a network shell command), and an agent's
  key reading the record through `/mcp` taints its running tasks when the
  result holds someone else's words (`taint_from_record_read`, the same
  check as Ask's `untrusted_episodes` — via
  `Store::first_untrusted_episode`, never `read_query`, which refuses
  under an agent key's area filter and would fail closed on every read).
  A new tool that brings in outside content goes in `outside_content`.
  Dispatch to a harness Hearthloom can't see inside (depth partial or
  dispatch-only: Hermes, OpenClaw) is judged on the model's `needs` PLUS
  everything the harness can do (`dispatch_classes`); never on the claim
  alone — and once such a task RUNS it is tainted (`taint_if_unseen`,
  0.5.100: Hearthloom can't see what it reads), so every action Hermes
  routes back asks the owner.
  APPROVAL CARDS SHOW THE WHOLE INPUT (0.5.100): `agent_approvals.detail`
  keeps the full tool input up to `DETAIL_CHARS` (`fit_detail` clips the
  input, never the JSON — a cut used to lose the input AND the reason),
  and the API's `input_shown` / `brief` / `request` / `summary` reveal
  every unseen character (`unseen::reveal`, ⟦U+…⟧, counted in `hidden`).
  Never show the owner a clipped or raw version of what they approve. Per-tool governance
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
  per ask (`ask_log.tainted`) and inherited by later turns; a friend's
  answer taints the ask too (0.5.77, `Flow::friend_taint` — another person
  wrote it) but, unlike record content, doesn't close the gate to that
  same friend (`outbound_block` checks `untrusted`, the record's taint);
  `sql` and note hits taint, and so does `traverse` unless every name it shows is
  the user's own (`Store::entity_names_unverified`, fails closed); the
  check fails CLOSED. An ACTING reply (a `delegate`, a friend call) is
  taken only when the whole reply is one JSON object
  (`whole_reply_object`) — never fished out of prose that may quote a
  tool result. Every spelling of a fence tag inside results is defused
  (`fence::outside`: `<tool_result>`, the map step's `<record>`, triage's
  `<thread>`, drafting's `<message>` — ONE defuser, never a per-module
  `replace`).
- **UNSEEN CHARACTERS LEAVE AT THE MODEL BOUNDARY, NEVER AT INGEST
  (0.5.100, core `unseen.rs`).** Records keep every byte (the episode
  law); text going TO a model drops what a reader can't see
  (`unseen::for_model`: tag letters, bidi controls, zero-widths, runs of
  selectors; a joiner between letters and one selector stay). It runs in
  `llm::call_streaming` (every API prompt), triage's Claude Code `-p`,
  each harness brief (`agents::brief_for_model`) and every MCP result
  (`mcp.rs`). A new path that hands text to a model must do the same.
  `fence::reads_as_tag` skips the SAME set, so a character dropped later
  can't join a fence tag back together — keep the two on `is_unseen`.
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
- **A PROGRAM THE DAEMON STARTS NEVER INHERITS ITS ENVIRONMENT (0.5.84 —
  a fixed security hole, don't re-open it).** The daemon's environment
  holds every sealed secret (core config.rs `apply_dotenv`). An agent run
  and a supervised ember start `.env_clear().envs(child_env::inherited())`
  — PATH, HOME, user, locale, clock — plus what they're handed on purpose.
  Before, an ember on a hosted node could read the issuer secret that signs
  the owner in, and `claude -p` billed an API key it found instead of the
  subscription. A new program started for an agent or an ember does the
  same; `test_agent_and_ember_programs_start_clean` checks the two files.
- **A MOUNTED SOURCE IS READ FOR A CALLER ONLY THROUGH
  `virtual_source::{search,fetch}_for_viewer` (0.5.86 — a fixed privacy
  hole, don't re-open it).** Archives (mbox) and mounts are read in place
  and never pass through ingest, so the raw `search`/`fetch` skip the
  privacy rules and the vault. The `_for_viewer` pair holds back, while the
  caller's vault is locked, an item a privacy rule matches (mail: the whole
  message) or whose memory is private — the path under the source's own
  name; a message's Message-ID under EVERY source the record holds
  (`Store::private_refs_anywhere`), never only the registered ones: a
  promotion keeps the name of a source that may since be unregistered, or
  never registered on this node (it synced) — silently, and a held-back
  fetch reads as not there. Triage and ember delivery make the same
  lookups — all three through `virtual_source::MailPrivacy` (made once per
  batch: the private set's not-bare keys, the bases of private corrected
  copies (`Store::private_replacement_bases` — an owner repair's
  `<id>#repair:<hash>` keeps its message private also on a node its
  original never reached), the legacy promotions, whether
  a memory keyed by its text is private and the privacy rules are read
  once, not per message or search hit) — and all three match
  a message by IDENTITY, never by header spelling: a reader's key carries
  the import's key AND a MIME parser's reading of the Message-ID
  (`mail_key`), matched by `MailPrivacy::found`
  (`id_in_key`: a comment beside the id on either copy still matches). Archive search is owner-only (`search_virtual`) and on the
  ember deny list. `test_archive_readers_go_through_the_vault_check` scans
  rlm/http/mcp/network_out for raw calls. The rules read a message WHOLE
  and DECODED (`virtual_source::Privacy`, `mbox_text::privacy_text`: every
  occurrence of every address header incl. Cc/Bcc — the import names the
  first From/To/Cc, a parser's own accessors the last — labels, every
  text/HTML part, uncut, each
  part also with its whitespace runs read as one space so a term matches
  across a line break) —
  never the text a reader shows, which is cut and, for an archive, still
  MIME-encoded; a Gmail message read only in part (a search's 64 KB, a
  vfetch's 2 MB) is read whole by its uid first, and one that can't be
  read whole counts as private while any rule exists. A mail promotion is
  stored under the message's own Message-ID (`promotion_ref`), whichever
  path promoted it; a private promotion an older build kept under an
  `at:` path is known by the Message-ID its STORED TEXT names
  (`legacy_promotions`: every such row, any source, no archive or mailbox
  needed; text read once per content hash) — one whose message names none
  counts as a private message without a Message-ID. One kept under a Gmail
  `uid:` path CAN'T be identified (the old fetch took a uid for any
  message and the promotion stored only the rendered text, no
  Message-ID), so while one is private mail is HELD — every message
  counts as private for a reader (archive reads and Gmail held back while
  locked, no triage, no ember delivery, no drafts); likewise a legacy row
  whose text can't be read. Never narrow this by guessing which message it
  was. Only a promotion holds: a row whose RECORDED writer is a person's
  (`human:…`, `promoted_by_a_person`; a peer's from the origin it sent)
  or not known (a peer sent no origin; a row synced before v61, whose
  actor names only the peer, `peer:…`) — never an importer's
  (`adapter:…`: a calendar UID can read `uid:7`), one the server names
  for a key that isn't the owner's (`agent:key:…`, `plugin:…`), or one of
  the owner's own devices (`human:device:…` — a scoped key no promotion
  route admits, whose feeds carry refs others choose), or else
  any ingest key could stop all mail. It holds under ANY source name —
  `gmail`, an alias, one unregistered since or only registered on another
  node, one registered here as a mount of files — whatever is registered
  now: a registration says what a name reads today, not what it read when
  the promotion was made, and a key that may register a source (an
  ember's `ingest`) could otherwise end a hold. While mail is held a
  viewer whose vault is locked reads NO mail thread (list, a thread,
  counts), no draft (of a thread or an ember's) and none of these, nor an
  ember's queued matches, through raw SQL (`Store::mail_held` — the
  daemon sets it on every read of the hold, at boot, and before the mail
  routes and raw SQL naming a mail table; `MAIL_HELD_SHADOW_VIEWS`): any
  thread or queued message could be the held message, and the threads
  keep their labels. Not held, locked raw SQL reads a draft or queued
  match only when its thread (the one it names, else the one its message
  joined here) is here and not private (`VAULT_SHADOW_VIEWS`).
  A hold is never a label (`virtual_source::MailHold`; readers use
  `MailPrivacy::key_private`, writers `found`/`key_found` and `hold`
  apart): nothing records a message's privacy on its strength, because a
  thread made private stays private for good — `observe` leaves the
  message to be observed later (`MailSeen::held`; `mail_observe` refuses
  it), triage skips its round without writing, an ember's queue waits
  with its cursor where it was, the backfill waits, and a draft request
  waits untaken (`mail_drafts::draft_one` — never finished as "private"
  for a hold). The hold is logged
  when it begins and ends, naming the memory (never its text), and the
  triage status (`held`) names it to an owner whose vault is open — only
  then (a locked view says nothing of what the vault holds).
- **ARCHIVED MAIL TEXT LIVES BESIDE THE HEADER INDEX (0.5.87,
  `mbox_text.rs`).** `texts_fts(id, body, links)` in the same sidecar,
  filled by a background pass behind the `text_rowid` meta watermark over
  `messages.rowid` — never inside `build`/`append_index` (a Takeout
  rebuild must stay header-fast), and `carry_over` must keep leaving
  `text_rowid` behind (a rebuilt file has no texts). Archives over
  `AUTO_LIMIT` messages need the owner's `mail.text.<source>` = on; none
  starts without 3× the projected size free. Text comes from
  `mail-parser` (decoded MIME), never the adapter's shallow `extract_text`.
  Header hits rank before text hits in `mbox_index::search`.
- **AN ARCHIVE HAS A HOME; FILES MOVE, NEVER MULTIPLY, NEVER VANISH
  (0.5.90, `archives_place.rs`).** Keepers come from the synced node list
  (`keepers`: the owner's `node.archives = keep` nodes, else the
  always-on local node with the most room, the hosted node only when
  there's none) so every node agrees. A keeper pulls settled archives
  (unchanged 24 h, never a mailbox's live archive —
  `mailboxes::is_live_archive`) through the hub with `vchunk`
  lookups — sha256-checked, written at the `.part` file's length, so a
  pull resumes anywhere. The releasing node unregisters and records
  `archives.released.<name>` = path; ONLY the owner deletes
  (`source forget --delete`). Never add an automatic delete.
- **THE `gmail` SOURCE IS READ-ONLY AND ITS OWN SESSION (0.5.89,
  `gmail_search.rs`).** EXAMINE (never SELECT) on All Mail, BODY.PEEK
  only — a search must never mark mail read or change it — and a session
  separate from the watch's IDLE one. Registered while Gmail is connected
  (`ensure_registered` from the watch loop); ONE `gmail` source searches
  every mailbox here (0.5.98: a warm session per mailbox, hits merged
  newest first and once per Message-ID, each naming its mailbox; a path
  without a Message-ID is `uid:<n>` in the first, `uid:<id>:<n>` in an
  added one); `recheck_mailbox(id, true)` resets that mailbox's session.
  A warm session whose call failed is dropped, never logged out
  (`gmail_search::warm_call`): it may be the one that stalled, and a
  LOGOUT on it would wait out its deadline again under the lock every
  mailbox's lookups take. Its hits carry the decoded text in `VirtualHit::text` (skipped
  on the wire) so the privacy check needs no second fetch; its messages
  are the pull's `mbox` memories (same Message-ID). Ask is told to search
  it first.
- **A LOOKUP ON ANOTHER NODE GOES THROUGH THE HUB AND RUNS UNDER THE
  HOLDER'S RULES (0.5.88, `lookups.rs`).** Only `vsearch`/`vfetch`, only
  peer keys (`/v1/sync/lookup*` in KEY_ROUTES under `peer`; embers are
  denied `/v1/sync`). The holder runs it with `search_with`/`fetch_with`
  and the asker's `vault_open` — never the raw readers. Requests live in
  the hub's memory only; the hint carries no content and is repeated
  while unclaimed (the bus is at-most-once). An unknown node is refused
  at once. Each node publishes `roles.archives` in the node list.
- **ASK NEVER ENDS EMPTY (0.5.86).** An "answer" must be non-empty text
  (`answer_text`); out of rounds it is asked twice for one; failing that,
  and on the other dead ends after work was done, `salvage` answers with
  where it looked and marks the outcome `incomplete` (the panel's "keep
  looking"). Only a provider safety refusal still fails the ask.
- **THE CLAUDE ACCOUNT POOL NEVER TOUCHES A LOGIN (0.5.85,
  agents/claude_pool.rs).** An extra account is a Claude Code home
  (`claude-accounts/<id>/`, passed as CLAUDE_CONFIG_DIR, Excluded from
  export) signed in by `claude auth login` in the owner's terminal; never
  read, store or forward its token, and never call Anthropic endpoints
  with it (Claude Code's legal page: apps "may not collect, store, or
  intermediate" Claude credentials or tokens). Usage readings come
  only from the run's own `rate_limit_event`s (undocumented: a missing
  reading is "unknown", never "free"). Switching and the API-key fallback
  default OFF; the key reaches a run only through the fallback seat
  (`Seat::env`), and a subscription run whose init event shows any
  `apiKeySource` but `none` is killed before it spends. A run that hit a
  limit restarts only when it used nothing outside `LOOK_ONLY` — a run
  that changed something must not run twice. Waiting reasons live in the
  queued task's `error` (prefix `waiting`), written only when they change
  (every write syncs).
- **AN UPGRADE SETS THE KEPT KEY'S SCOPES (0.5.84).** `install_from_dir`
  keeps an installed ember's key id and secret but sets its scopes to what
  was granted now (`set_api_key_scopes`). Before 0.5.84 the key kept its
  first scopes forever: narrowing didn't narrow, a granted widening didn't
  widen. `test_an_upgrade_gives_the_kept_key_exactly_what_was_granted`.
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
- **THE EMBER HANDSHAKE LIVES IN TWO FILES (0.5.74).** The page's half
  is `assets/ember.js` (served pre-auth; ember pages load it instead of
  carrying a copy): it believes only `ev.source === window.parent &&
  ev.origin === location.origin` and always calls `location.origin`,
  never a `base` from a message. The dashboard's half is
  `assets/ember-shell.js`: `handOver` posts the token ONCE per src (a
  sandboxed frame can only be addressed "*", so a frame that has
  navigated elsewhere must never get it — app.js/desk.js posted it on
  every load until 0.5.74), `openLinks`/`openExternal` open what a page
  asks in a new tab with no opener. The node serves ember pages with
  `frame-ancestors 'self'` and refuses them as a top-level document
  (`ember_page_top_level`). A new message type, or any other place that
  frames an ember, goes through these two files; `tests/ember-handshake`
  (CI, real Chromium) must keep passing.
- **AN AGENT'S RUN IS NEVER THE OWNER'S WORDS (0.5.74).** The idle local
  refresh imports Claude Code sessions at trust `self`, so it leaves out
  every session that opens with `hearthloom_core::AGENT_PREAMBLE_MARK`
  (what `agents::preamble` writes) or ran inside a configured agent's
  working folder (`agents::agent_workdirs`); the outcome lands as an
  `agent_task` episode instead. A new local importer that could capture
  agent activity must leave it out the same way. Read an agent's folder
  only through `agents::workdir` (it expands a leading `~`, 0.5.76): if
  the run and `agent_workdirs` saw different paths, the agent's sessions
  would come back as the owner's words. A node can stop
  importing its own machine's files altogether (`node.imports`, Peering
  page / `hearthloom node --imports off`).
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
  `foreign_browser_request`. The ONE exception (0.5.75) is the sign-in:
  hearthlaunch's "Open" is a form POST from its own site carrying an
  issuer token, so `POST /v1/auth/login` passes the middleware
  (`is_sign_in`) and `auth_login` admits only that token from another
  site (`foreign_sign_in_refused`; a foreign password would be login
  CSRF). 0.5.71–0.5.74 refused it, and a hosted node has no other way in.
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
- **THE EMBER READ SURFACE IS A DENY LIST (`plugins.rs::plugin_route_allowed`).**
  An ember's `read` key reaches every GET under `/v1/` not denied there,
  and the KEY CEILING does not apply to it. A GET that ACTS — runs Ask,
  spends tokens, hands work to agents — must be on that list AND check
  `require_owner` itself: `/v1/query/stream` was neither until 0.5.80.
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
- **MAIL TRIAGE LABELS, NEVER ACTS (0.5.92, `mail_triage.rs`,
  store/mail.rs).** The sorter has NO tools (Claude Code `--tools ""` +
  `--json-schema`, or `llm::call`), thread text sits inside defused
  `<thread>` fences, and a label whose quoted span isn't in the thread is
  only `fyi` (`checked`). Private threads (a privacy rule, or a private
  memory of the message) never reach a model (`mail_threads_to_triage`),
  and each batch's memories are looked up again just before, by every key
  `observe` used (the thread index's `memory_refs`, v70;
  `mail_triage::for_model`, which makes such a thread private; a message
  recorded before v70, or a latest message another node recorded, gets
  its keys from its live archive, and a thread whose keys can't be read
  there waits, unsorted and unlabelled; while mail is held
  it sorts nothing and writes nothing).
  `mail_triage::seen` makes the checks a message read in place meets: the
  memory looked up by the import's key (`virtual_source::mail_key` — a
  parser's reading of a Message-ID with a comment beside it, or of the
  last of two Message-ID headers, differs from it) in every mail source
  (`MailPrivacy::key_found`), and the rules over the
  whole decoded message (`mbox_text::privacy_text`); errors count as
  private.
  Rules are owner-only (`require_owner`; mail never writes one). Threads
  form only from decision-worthy mail (Primary not from the owner, rule
  matches, replies to an existing thread); thread ids hash the root
  message id so every node agrees. "From the owner" means from ANY of the
  owner's addresses (`mailboxes::owner_addresses`, passed to `seen` as
  `mine`) — never compare with one connected address. Each thread is
  sorted once, on the node that handles its mailbox (`mailboxes::duty`,
  `mail_threads_to_triage_for`). `/v1/mail` is on the ember deny list;
  embers get mail only through `mail.watch` (below). A new shadow view
  needs its DROP (`test_every_shadow_view_is_dropped`).
- **EMBERS SEE MAIL ONLY THROUGH `mail.watch`, AS MUCH AS THE OWNER
  SAYS (0.5.93, mail_embers.rs, store/mail_watch.rs).** An ember's
  watches are checked where Gmail is watched (`on_seen`; class-waiting
  watches at `on_classified`) and matches queue per ember. The view
  (`headers` | `snippet` default 300 chars | `full`) is applied at
  DELIVERY (`deliver` → `shown`), never at match time, so the owner's
  change reaches what's queued; delivery re-checks privacy (a rule or a
  private memory added since holds a match back, failing closed; a match
  keeps only the parsed Message-ID, so the memory is found under any key
  the import can have given it — `virtual_source::MailPrivacy::found`);
  while that can't be told (mail held, the record unreadable) it delivers
  nothing and the cursor stays, so nothing is passed over for good. Private
  or from-the-owner mail never queues, and neither does mail of a mailbox
  the owner hasn't let embers see (`mailboxes::embers_see`,
  `mail.embers.<id>`: on for the first mailbox, OFF for an added one —
  adding a mailbox never widens what an ember sees). An empty watch filter
  matches NOTHING (0.5.98): a filter this build can't read parses as
  empty, and before the fix it matched every message. The scope lives on the plugin row
  (`granted_scopes`), not the key — `mail.watch` isn't an API-key scope,
  so the key gets the rest and an ember can't hold `mail.watch` alone.
  The self routes check it in the handler (`mail_watcher`): every plugin
  key passes `/v1/plugins/self/*`. Uninstall forgets the ember's watches,
  view and queue (`mail_watches_forget`). A new route that hands embers
  mail goes through `deliver`.
- **SEVERAL MAILBOXES (0.5.98, mailboxes.rs).** Never read
  `GMAIL_IMAP_*` directly — go through `mailboxes::all()`/`find()`. The
  FIRST mailbox keeps the names it always had (GMAIL_IMAP_USER/PASSWORD,
  `archives/gmail-live.mbox`, source `gmail-live`, `gmail.watch*`,
  `mail.backfilled`) so an upgrade re-pulls nothing and an older build
  still reads it after a rollback; added ones are sealed as
  HEARTHLOOM_MAIL_<ID>_{USER,PASSWORD} with `gmail-live-<id>` and
  `.<id>`-suffixed settings (`Mailbox::key`). The id is a hash of the
  lowercased address (`id_for`), so every node agrees, and a connect of
  an address already here keeps its slot (`slot_for`). Who handles a
  mailbox's threads and drafts is `mailboxes::duty`: the smallest live
  node id among those that watch it; a mailbox nobody live watches (and
  "" from before 0.5.98) falls to the smallest live node that watches any;
  a live pre-0.5.98 mail node with a smaller id handles everything, as it
  does itself. The node list's `roles.mail` stays a boolean (older nodes
  compare it to `true`); `roles.mailboxes` is additive.
- **DRAFTS NEVER SEND, AND TOUCH NOTHING ELSE (0.5.94, mail_drafts.rs,
  gmail_imap.rs `save_draft`, adapters mail_compose.rs).** The only write
  to the mailbox is an APPEND of a new message, flagged `\Draft`, into
  the folder marked `\Drafts`. Never add SMTP, a send, or a STORE/EXPUNGE/
  MOVE on another message — asking again adds another draft rather than
  replacing one the owner may have edited. The drafter of a mailbox is
  one node (`mailboxes::duty`: the smallest live node id among those that
  watch it; a request records its mailbox in `mail_drafts.account`, from
  its thread or its ember match), the draft is saved in the mailbox the
  person wrote to, from that address when it's one of the owner's
  (`pick_from`: X-Gm-Original-To, To, Cc, Delivered-To — an alias's mail
  is answered from the alias), the model has no tools (`mail_triage::ask_structured`),
  other people's mail sits in defused `<message>` fences, and any private
  message the draft would read refuses the draft. Headers built from
  other people's mail go through mail_compose's cleaning (no CR/LF, ids
  and addresses validated) — the message stays 7-bit. An ember's draft
  (`mail.draft`, plugin row only, like `mail.watch`) answers only a
  message its own queue holds, to the sender, ≤20 a day.
- **THE SERVER NAMES EVERY WRITER THAT ISN'T THE OWNER (0.5.96).** On
  `/v1/ingest`, every device feed and the virtual-source register and
  promote routes (an ember's `ingest` key reaches both), `http.rs::ingest_actor` decides the
  actor: a peer by this node's name for it, an ember by its name, any
  other key via `write_actor` (`agent:key:<name>`, own-device keys
  `human:device:<name>`); only the owner names an adapter. MCP checks an
  ordinary key's scopes per tool (`mcp.rs::call_tool`: `read`|`mcp` read,
  `mcp`|`write` propose, `write` commit) and its writes go through
  `write_actor`. A new route that writes for a key uses one of the two.
  A registered name changes only for the owner or the writer the
  mutation log records as its maker, and a source row of another kind
  never does (`Store::register_virtual_source_as`). Each registration is
  recorded under a key of its own; a record under the name's bare key is
  an earlier build's, which recorded only a name's first registration, so
  it names no maker and only the owner changes that name
  (`browse.rs::registrant_of`).
- **THE FIRST LOGIN NEEDS PROOF (0.5.96, setup_code.rs).** `auth_bootstrap`
  takes the node's one-time setup code (`<home>/setup-code`, 0600, spent
  on use), or on a node with `HEARTHLOOM_PROVISIONER_SECRET` only the
  provisioner's control token (`X-Provisioner-Token`; a provisioner
  sends one). `GET /v1/auth/setup-code` answers only a connection whose SOCKET
  address is loopback (`ConnectInfo`, served with
  `into_make_service_with_connect_info`) with no forwarding headers —
  never trust a Host header for "this machine". Never log the code.
- **THE DASHBOARD HOLDS NO KEY, AND ASKS ONLY FOR ITS OWN API (0.5.96).**
  No API key in localStorage/sessionStorage: "sign in this browser" trades
  an owner key for the HttpOnly session (`/v1/auth/session-from-key`).
  `app.js::apiPath` refuses any path outside `/v1/` and `/health` or with
  a dot-segment (plain or `%2e`), a backslash or `//` — views build paths
  from the address bar, so keep every request on `api`/`apiPost`/
  `apiDelete`. An ember page opens only a page the ember declared.
- **A CALENDAR SIGN-IN BELONGS TO ITS CLIENT; CODES ONLY TRAVEL (0.5.97).**
  The refresh token is sealed with the client ID that made it
  (`GOOGLE_CALENDAR_CLIENT`; absent = the own client, pre-0.5.97) and
  refreshes use that client's secret (`secret_for`: own, shared Desktop
  from the registry's `/.well-known/hearthloom-oauth`, hosted Web from the
  provisioner) — never "the current client". A pending sign-in is used up
  only by its own state (`complete` checks before taking), so the loopback
  listener and `/oauth/google/callback` ignore stray requests and record no
  error for them. `/oauth/google/callback` is pre-auth AND static-class (a
  same-site navigation from Hearthlaunch): its credentials are the state
  this node signed and the PKCE verifier only it holds — never add cookie
  or session logic to it, and never forward or log the code. Tokens this
  node signs for Hearthlaunch (`provisioner::sign_for_provisioner`) carry a
  purpose (`google-relay`, `google-web-client`, `oauth-relay`) that
  Hearthlaunch checks; `test_node_tokens_match_hearthlaunchs_vector` pins
  the format to the provisioner's verifier.
- **GOOGLE CALENDAR IS FREE/BUSY ONLY (0.5.95, google_calendar.rs).**
  Scopes stay `calendar.freebusy` + `calendar.calendarlist.readonly`
  (+ `openid email` for the account's name): never a scope that reads
  event details or writes, without the owner deciding it. The client is
  the owner's own (Desktop app); the refresh token is sealed with
  `set_env_vars`, access tokens live in memory, and the sign-in's PKCE
  verifier and state never leave this node's memory (`PENDING`, 15 min).
  `/v1/calendar` is owner-only and on the ember deny list. `set_client`
  saves nothing Google hasn't accepted (`check_with_google`: a made-up
  refresh token — `invalid_client` = wrong pair, `invalid_grant` = right
  pair); the owner's input may be the downloaded JSON, so never log it or
  echo it back in an error. Drafts get free
  windows only for replies about a time (`mail_drafts::about_a_time`), and
  the model may offer only those.
- **THE OPERATIONAL MIRROR: NODE-LOCAL COLUMNS STAY HOME, TIES SETTLE
  (0.5.91, store/sync.rs).** `NODE_LOCAL_COLUMNS` (a goal's `last_seq` /
  `last_checked` — a position in THAT node's mutations log — and a note's
  `views`) are never exported, are dropped when an older peer sends them,
  and survive an INSERT OR REPLACE. A column the receiver lacks is
  ignored (wire-compat per field). LWW uses `updated_at` where a table
  has one; equal stamps with different contents go to the greater
  canonical form so every node keeps the same copy; a table that only has
  `created_at` still takes the incoming copy (its stamp can't tell an
  edit). A writer that changes a mirrored column must bump `updated_at`
  — goals didn't, and with two spokes their copies traded places at the
  hub every cycle (the "mirrored":3 of 2026-11-03→05).
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
  photos, visits, contacts, trips — 0.5.66). A source whose `source_ref` a
  third party can choose (mail Message-ID, calendar UID, a URL) must
  never be added: it lets a sender hide a known record. **The one other
  path (0.5.82): an ember revises only what IT wrote** —
  `provenance::plugin_revision` + `same_writer_sql`: a `#r<n>` whose writer
  is `plugin:<name>` (the batch actor here; the sent origin's writer on a
  peer) supersedes only base/lower revisions whose RECORDED writer
  (`episode_origin.writer`, or an unrecorded local row's own actor) is the
  same. Unknown writer → nothing (fail closed). The base is matched
  exactly (`base_or_revision_sql`), never with LIKE — the ember chooses
  the ref. ArrivalRules applies the same check. Never widen this to other
  writers: the owner, importers and other embers write under names an
  ember can also use. **And the growing sources** (`GROWING_SOURCES`:
  chrome, imessage, claude-code — owner-only; core `store/growing.rs`):
  every read of a day/session lands as `<base>#r<n>` — the first read
  too, so two nodes' first reads holding different counts are different
  episodes (one shared ref let each keep its own text forever); an
  unversioned `<base>` is an earlier version's — `n` = items
  held (`growing_items`: the TRUE count the adapter writes ahead of any
  third-party text — `<n> visits` / `messages: <n>` / `turns: <n>` — so a
  capped text still counts; a text from before is counted by its lines;
  a chat-day counts at least its `[HH:MM] ` lines too, so lines a message's
  own text holds weigh alike in every copy and an earlier version's day
  holding a pasted log never holds back the messages after it — a day
  whose messages hold such lines writes how many the whole day holds,
  `lines: <m>` on its second line, so a long day's text that leaves the
  log out still counts it),
  decided ONLY by daemon `local_import::prepare` on the paths that read
  this machine's files (refresh, `/v1/ingest/local`, inbox, non-peer
  `/v1/ingest`; always on a blocking thread) — never on a peer's batch. A
  stored one lands again only when it holds more than the fullest copy.
  On arrival (`Arriving`) the copy holding the most stays live: a
  revision ranks by its `n`, the unversioned base by its text's count
  (it can differ per node), a tie goes to the revision; it lands behind a
  standing (live or retired) copy that outranks it, else puts every one
  it outranks behind itself — a retired one via `retire::undo_behind`, so
  an undo leaves one copy in view. A RECORD is all copies of a base plus
  the copies `settle` put behind one another (a Chrome profile day and
  the named day it replaced) — `growing::other_copies`, which also reads
  a retired copy's saved mark, both ways (from a profile day, the named
  day's saved mark naming it). A new copy takes the record's label (the
  latest stamped label on any copy, else the fullest copy's) and its
  retirement (any retired copy). A PRIVATE LABEL HOLDS FOR THE RECORD:
  a copy landing private is stamped (the record's stamp, else now) —
  except a peer's copy private by its sender's label, or by a label here
  with no stamp: the episode leg carries no stamp, and the label can be
  older than the copy (one it inherited where it was made), so any stamp
  chosen here (now, when it was first written) could outrank a decision
  made in between. It lands stamped `growing::UNKNOWN_TIME`, which sorts
  below every real stamp: it reaches only the copies here that carry no
  stamp, any real stamp outranks it, and — being a stamp — it travels on
  the scope leg to the nodes holding those copies with none (never leave
  such a label NULL: the scope leg skips unstamped rows, and the node that
  sent the copy would keep its own copies out of the vault). The sender's
  own stamp comes on the scope leg and spreads as below.
  Every path that labels an episode private — arrival, `set_episode_scopes`,
  the scope leg (`apply_scope_changes`), privacy rules, a Project cascade,
  `label_private_on_reimport` — calls `growing::spread_private`, which
  gives the other copies the same stamp unless labelled since (LWW, a tie
  to private) — copies already private under an older stamp take the
  newer one (only a stored stamp spreads that way: a `now` fallback would
  restamp them on every call). The scope leg takes a newer stamp even
  when the label is the same; both statements keep `scope` in their SET
  list, which the relay-stamp trigger follows. A label taken OUT of the
  vault stays on the copy it was given to. Retiring one copy retires the
  record (`covered_episodes`, also when the target arrived retired
  first), and a copy arriving retired retires the record's other copies
  (gathered BEFORE it is marked — the mark replaces its link) and, at the
  end of the batch, what the copies it took seeded, and a cascade of the
  arriving copy that came first (`ArrivalRules::retire_derived` — only
  for a batch that took copies held here, or took an episode alone that a
  cascade here names: a peer's cascade is the one thing such an episode
  can have seeded here, found by one pass over the cascades' refs on the
  index, `retire::cascade_seeds_among`). An
  opportunity whose memory a retirement took before the
  opportunity's episode landed lands retired (`ArrivalRules::covering`
  reads the row the retirement saved).
  A Chrome day keyed by profile
  puts a day stored under the profile's shown name (`chrome_named_ref`)
  behind it only when it holds it (`chrome_day_holds`: at least its visit
  COUNT, the visit it opens with, and every other visit line it shows
  unless the holder leaves lines out there and the held day leaves lines
  out too — a day shown whole is held only where each of its lines shows,
  so another profile's short day under the same name stays — lines collapse repeated
  titles, an earlier version's long day stops at `[truncated]`, a long day
  now keeps its first and latest lines around `[<n> lines not shown]`;
  titles are written on one line, and an earlier version's title across
  lines reads as one), in either arrival order — a same-named profile's
  day stays. A private import names that day too
  (`local_import::private_refs`), since `prepare` drops a profile day it
  holds. Their adapters keep every base
  ending in a part they write (`#<date>`, a session id with `#` escaped)
  so no outside string reads as a revision.
- **OWNER REPAIRS: A CORRECTED COPY REPLACES ONLY WHAT ITS RECORDED WRITER
  SAYS (v69, core `store/repair.rs`, daemon `repair.rs`).** A memory an
  earlier version stored wrongly (mail left encoded, an event holding its
  alarm's text) can't be imported again — the ref keeps its first text,
  and a retired ref never lands again — so the owner replaces it: the
  corrected copy lands as `<base>#repair:<16 hex of hash(title\nbody)>`
  (`replacement_ref`) and the base goes behind it. A replacement is an
  episode of a `REPAIRABLE_SOURCES` source (mbox, ics, openai,
  chatlog-chatgpt, apple-health) whose RECORDED
  writer (`episode_origin.writer`; a peer's from the origin it sent) is
  `REPAIR_WRITER` = `repair:owner` (`repair_of`) — NEVER the ref's shape:
  Message-IDs and UIDs are senders' choices, so a look-alike ref from an
  importer, another key, an ember, a peer's other writer or a peer that
  sends no origin replaces nothing (fail closed). Only
  `Store::repair_apply` writes as that writer: `apply_batch` refuses any
  `repair:` actor from every other caller (`apply_batch_as`), and
  `/v1/ingest` refuses it with 400 — keep both. It supersedes only the
  exact base it names (a range on the unique index plus an exact
  `replacement_base` check, never LIKE), and refuses a base that is
  itself a replacement. A REPAIRED RECORD is the base plus its
  replacements; every node settles it from what it holds (`settle`, on
  every arrival of a copy or of the base, and per run): the newest
  replacement (by when the newest run that made it began —
  `repairs.created_at` over `repair_copies`, rows every node converges on;
  the origin's `at` only while no run of it is known here, as it differs
  per node — then ref) whose run isn't undone stands, the
  others behind it; with none, the base stands and the replacements stand
  behind it — behind the base's id even when the base hasn't reached the
  node (an undone correction never stands in its place). Arrival order, a
  second repair and an undo converge. It is
  one record like a growing source's: a copy takes the record's label
  first (`RecordArrival`; private with the record's stamp), a private
  label on any copy spreads (`growing::other_copies` delegates to
  `repair::other_copies`, `spread_private` covers repairable sources,
  `label_private_on_reimport` too — so Message-ID lookups that read every
  row still find the old copy private) — and, unlike a growing source's,
  a label the owner takes OUT of the vault (vault open,
  `set_episode_scopes`) comes out for the whole record with its stamp
  (`repair::spread_unvault`: the copies are one item, and a hidden private
  copy would keep the message held) — only when that relabel took the copy
  out (a copy outside already, or left as it was, takes nothing out). An
  import that reads a record as private (`label_private_on_reimport`)
  reaches its corrected copies by the record's ref also where the base
  isn't stored (`repair::replacement_ids`). Retiring one retires all, a copy
  of a retired record lands retired, a run settled on a node that held a
  copy from before it knew the rule retires that copy too
  (`retire_with_record`, in `repair_settle_run`), and a settle on a
  retired copy rewrites what its undo restores
  (`retire::set_prior_superseded`).
  Settling deletes the map points of a copy that goes behind (the
  supersession rule), re-queues one that comes back
  (`location_scanned = 0`), and carries the extraction pass's
  `mentioned` links to a standing replacement; structural links come from
  the corrected draft, so a leaked alarm attendee drops out. A run's
  copies are the set `repair_copies` (v71): every copy the owner repair
  lands is recorded under the run its origin names (`apply_batch_as`),
  each run row carries its set on the repairs leg, and every node unions
  it (`repairs_sync_apply`, a run per transaction with pauses, on the
  blocking pool like every repairs route and the sync step; a run, undone
  or not, that brings copies a node didn't know of is settled there again
  — the corrections it made rank by when it began). A row
  sent again carries only the copies recorded since the peer's cursor
  (`repair_copies.recorded_at`): whoever records a copy of a run stored
  here stamps the run in the same transaction (`restamp_runs`), and a
  run's row arriving stamps the copies recorded before it — break either
  and a peer misses copies. A copy goes out on the page its own stamp
  falls in: a run stamped again past a page's end rides that page too when
  a copy of it was recorded within it (`repairs_sync_export`), or a peer's
  cursor would pass the copy before the run reached it. Never go back to the origin's
  `session` alone: two nodes that make the same correction before they
  sync hold one copy (its id is its content's) whose first-arrival origin
  names a different run on each, and an undo of either must reach it
  everywhere. A copy is undone when ANY run that made it is undone
  (`undone_arrival`, `undone_copy`). Undo stamps the run
  (`stamp_after` — whole-second stamps), each node settles the
  run again (`repairs_pending_here` / `repair_settle_run`, daemon
  `repair::catch_up` every cycle and after the leg — each run on its own:
  one whose settling fails is tried again next cycle and holds up no
  other), and what a run ADDED
  (a missing calendar occurrence, an ordinary `adapter:ics` import) stays.
  An undone correction is never made again by asking for it again (same
  ref), so the CLI's `repair undo` only explains that unless `--yes`.
  Routes `/v1/repair/{mail,calendar,chatgpt,health,copies,
  photo-days}`, `/v1/repairs`, `/v1/repairs/{id}/undo`: `require_owner` +
  vault open (`repair_allowed`), on the ember deny list, not in
  KEY_ROUTES; one run OR UNDO at a time per node (`repair::try_start`,
  409), and `catch_up` waits while one runs (it would settle a run still
  landing); a run lands through `Store::repair_apply_work` (`repair_apply`
  wraps it; a record whose base never reached this node is repaired over
  its stored copies; its row records the counts of what landed also when a
  batch stops it part way). The repairs pull leg moves its cursor only past a
  page that applied (`sync.rs::take_repairs_page`: the row is the only
  carrier of an undo). Reports list episode ids, statuses and short
  reasons — never subjects, names or text. The mail scan reads EVERY
  archived copy under a Message-ID (`mbox_index::copies_of` +
  `offset_keyed_ids` — the index keeps later copies under `at:<offset>`)
  and takes only one outside Spam/Trash with the memory's event time and
  sender address; copies that pass and read differently → `ambiguous`; while
  ANY archive can't be read — index unreadable, out of date or missing, an
  enabled mount that doesn't answer (`mail_archives`) — every message is
  `archive_error` (never "not in archive", never a reading from the
  others: the unread one may hold the original or a second copy). Which
  memories it reads again is a heuristic (`still_encoded`, generous on
  purpose — short base64, a QP escape or two); the decoded original
  decides, and one that reads the same is only `unchanged`.
  The calendar scan replaces only a record whose original's recorded
  writer is `adapter:ics` (`RepairTarget::base_writer`; the phone's
  `/v1/ingest/calendar` writes the same source and UIDs in its own shape).
  An event stored under an earlier ref is found by UID and start
  (`Store::repair_same_event`), and a ref that begins `<uid>#` counts only
  when its tail is a start (`occurrence_shaped`) and the copy's Event
  entity holds `ics-uid` = that UID and not the whole ref
  (`event_is_uid`): UIDs are senders' choices (`meeting#other`). One
  that can't tell is `unmatched` — neither replaced nor imported. A file
  that can't be read is reported by name only (the importer's error can
  quote an event's title).
  A node older than v69 stores a replacement as a separate live episode
  — in view even where the base is retired, or private there alone, and as
  a hub it 404s the repairs leg so an undo doesn't pass — until it
  upgrades; the run row then reaches it and the catch-up settles it,
  labels and retirement included. So every node publishes `roles.repairs`
  = true (`agents::publish_self`, `repair::REPAIRS_ROLE`), the dry run
  names nodes seen in the last 30 days without it (`older_nodes`), and
  applying is refused (409) while one exists unless the request says
  `allow_older_nodes` (CLI `--allow-older-nodes`).
  **Sources with a revision rule of their own get no `#repair:` copy**: a
  run RE-MINTS them under that rule (`RepairWork::revisions`,
  `revision_of`: a growing source's `<base>#r<n>`, a photo day's
  `photos#<day>#r<n>`), still written as `REPAIR_WRITER` under the run's
  session so the run finds them. `copies` re-mints each day or session an
  earlier version stored under an unversioned base from its STORED text
  (`repair_copies_scan`; never the original files), named by its count; a
  base a copy holding as much already covers, one standing behind a copy
  that holds it, or one an undone run re-minted is left. Two nodes hold
  different texts under one such base, so each node re-mints ITS OWN when
  a `copies` run reaches it (`repair_settle_run`, kind `COPIES_KIND`) and
  the growing rule keeps the fullest in view everywhere — a re-mint keeps
  the base's links (`land_revisions`; extraction reads iMessage days
  again). Equal counts with different texts can't be told apart by the
  growing rule and stay as each node has them. A copy an undone run
  re-minted holds its `<base>#r<n>` ref for good, so it never counts as
  the fullest for a local read (`growing_latest` skips it), and a local
  read that names its ref puts it back in view by the growing rule
  (`growing::revive_undone_copy`, from `apply_batch`'s already-stored
  branch; never for a peer's or a repair's batch). That read is recorded
  (`repair::record_revived`, `repair_revived`) and travels with the runs
  that made the copy (`revived` on the repairs leg), and a copy in it is
  the read's, not the undone run's, on every node (`undone_arrival`): each
  node puts it back by the growing rule when the run settles
  (`growing::restore_revived`; a copy waiting behind the id of a base the
  node doesn't hold yet too, so the base arriving later lands behind it)
  — the other nodes can't read those files, and the read lands no episode
  of its own. Placing a copy so, and ranking a record's copies when a run
  settles, read no raw file with the store held: the copy's text and
  those it is measured against (an unversioned copy's, the Chrome days
  it's compared with) are read first (`growing::named_copy_texts`,
  `settle_texts`, `growing::Texts::Read`); one that wasn't read (it
  arrived since) leaves the copy where it is on a local read, and fails a
  run's settle (placing or ranking) for the next catch-up. `photo-days` re-mints
  ONLY an inflated count — the day shows a photo more than once, or the
  count it was minted with here is above the photos stored and keeps the
  finalize pass from adding the ones it lacks (a day that reads otherwise
  for another reason is counted `reads_otherwise` and left) — and ONLY when
  the points here — each by its own name and every replaced key's
  (`photo_replaced_into`) — cover every photo the standing copy shows (a
  cut list fails closed; `photos_missing` lets a point answer for one line
  per name it has had, so two lines of one name in one minute are two
  photos, never one point's duplicate) and name every place its `places:`
  line names;
  at a revision past every one stored, carrying the day's label; never
  again with the text an undone run re-minted (`PhotoDayRecord::undone`);
  and sets the ledger (`PhotoLedger`; the row before is kept under
  `repair.photo_days.<run>` and put back by the undo only while it still
  says what the run set — at the run's revision when that is higher, since
  the run's copy keeps its ref and the finalize pass mints at the ledger's
  revision + 1; a day with no ledger before gets the one the finalize pass
  adopts, `legacy` with the count in view). Every node checks coverage
  for itself: unlike a finalize revision (a superset of the union), a
  correction drops lines, so a peer's (writer `REPAIR_WRITER`) supersedes
  only the copies of the day in view there that it holds by THAT node's
  points and replaced keys (`photo::unheld_live_copy`; else it lands
  behind the copy in view — a node whose copy shows a photo the correction
  lacks keeps it; in either arrival order: a copy of the day arriving
  after the correction that the correction doesn't hold puts it behind
  itself, `photo::repair_copy_misses`), and is checked again when the run settles there and
  whenever the photos leg brings points of the day
  (`photo::settle_repair_copy`, from `photo_points_apply_and_settle` — the
  points and replaced keys come on their own leg, and a paged or failed
  one brings them after the run; the photos leg applies through that
  method, never `photo_points_apply`, on the blocking pool —
  `live_photos::apply_peer_points` — and moves its pull cursor only past a
  page that applied (`sync.rs::take_photos_page`); the day checked is the one the point
  stored here keeps, which a peer's time zone can put elsewhere) or this
  node's own feeds name points of the day (its Mac scan, the phone's
  stream: `photos_add_points_and_settle`, never `photos_add_points`; every
  point named that is stored here counts, changed or not, so a read again
  after a stop before the settling settles what that read changed). A photo day's base and revisions are
  one record for the private label: a copy arriving takes the day's
  (`photo::day_private` — a correction carries the label of the node that
  made it) and `spread_private` reaches every copy of the day
  (`photo::other_day_copies`). The finalize pass mints past every revision
  stored (`photo_day_max_revision`), never at a ref another node's
  revision already took — and past a revision its ledger didn't mint (with
  no ledger, past any copy, the one in view too) only when the points here hold
  every photo the day in view shows (`photo_points_hold_day`): the
  ledger's count says nothing about another node's copy, which can hold
  more. UNDOING A RE-MINT (`settle_revisions`): the copy
  that stood before stands again and the run's copies go behind it, and a
  copy of an undone run that arrives later lands behind the copy in view
  (`ingest_apply`: `undone_copy` → `Arriving::settle(…, true)`, or the
  revisioned branch) — or, no copy of the record being here yet, behind
  the base's id, so the base arriving later stands (`settle_revisions`
  does the same when only undone copies are here) — never let one
  supersede on arrival. `chatgpt`
  replaces per route (`openai` and `chatlog-chatgpt` copies compared with
  `openai::read_routes`) ONLY a transcript that holds turns off the branch
  it ended on (or in another order) and whose every turn is one of the
  export's (`transcript_verdict` against `every_turn_*`): a transcript from
  a later export, one with images the export names otherwise, or one that
  differs only in title is left (`other_export`). It puts "do not
  remember" records into the vault BEFORE the run through
  `label_private_on_reimport`, so their corrected copies land private (an
  undo doesn't take them out); `health` streams `export.xml` (from the zip
  in place, `apple_health::scan_reader`), leaves the day the export was
  made on, and replaces a day only when the export holds every tracker and
  every measure the stored day shows (`day_keeps` over `day_measures`:
  `body/body fat`, `heart/hrv`, `activity/min exercise`… one by one — a
  weight or a distance is one measure in any unit, since units are what
  the repair corrects; a sleep stage the corrected day leaves out is held
  when the export holds it from any tracker, `HealthReading::sleep_stages`
  — the night shown is one tracker's, so another's stages drop out).
- **Source and kind names in prompts and BRAIN.md go through
  `printable_name`** (`[a-z0-9_.-]{1,40}`): whoever ingests chooses them,
  and Ask's inventory sits in the system prompt. `source_lines` counts an
  unprintable one, never prints it. Dates there come only from an
  event_time that IS a date (`source_shares`' GLOB): the ingester sets it.
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
  revisioned episode lands; a base or lower revision arriving after a
  higher one lands behind the copy that stands — the highest in view or
  retired, `ingest_apply::standing_above`, unless that is a photo day's
  correction in view that doesn't hold it, which goes behind it
  (`ingest_apply::land_late`); not trips, whose revisions are
  fingerprints) — supersession therefore syncs with ZERO
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
  decision it triggers (the cleanup start). The profile card rides both
  legs too (`http.rs::profile_card_envelope` / `adopt_profile_card`): its
  addresses decide whose commits in an owned repository are the owner's.
  Known gaps: `ingest.device.scope` has no setter and doesn't sync;
  `llm_budget` rides the push leg only.
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
  browse/stats/MCP/Ask; raw-sql via TEMP-VIEW shadowing in `read_query`,
  which refuses a `main` schema qualifier in any spelling SQLite reads —
  quoted, any case, comments or whitespace as SQLite's tokenizer reads it
  (a vertical tab within a run, a U+FEFF where a token would begin) before
  the dot — and ATTACH, either of which reaches the tables behind the
  views: `names_another_schema` — and every other table judged by its
  class through SQLite's authorizer: see "Raw SQL" below),
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

**A restore writes only what an export writes (0.5.100,
`export::restorable`).** Never a top-level dotfile (`.env` is read into
the environment at the next boot — a zip carrying one could switch a
hosted node's settings), never the db files, nor a SQLite side file
(`-wal`/`-shm`/`-journal`) at any depth — so the export reads a database
with a `-wal`/`-journal` beside it through SQLite (`export::write_file`,
read-only `VACUUM INTO`, side files left out; what SQLite can't read goes
in as it is, side files too, with a manifest warning) — nothing `DIR_CLASSES`
excludes (archives excepted), never `plugins/<n>/key` or
`plugins/<n>/secrets/`; names compare IGNORING CASE (a Mac's disk does);
`config/` comes back as `<name>.restored`, never read. On a HOSTED node
(`!unreviewed_install_allowed`) embers' `pkg/` stays out too, and `after_restore` (boot, before any
sidecar starts) takes the restored embers off the list — key revoked,
data and `plugin_kv` kept — until the registry installs them again.
`plugins::sidecar_start` launches only a plain relative path inside the
ember's own package (a restored db's manifest can say anything), and a
hosted node ignores an agent's `claude_path` (`claude_code::find_claude`).
The restore marker is written BEFORE the swap on purpose: a crash after
the swap must still unlist the embers (fail closed). A new
home-root entry that holds secrets or steers the node must be classified
`Excluded` so restore refuses it.

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

## Raw SQL (the read classification — never bypass it)

`Store::read_query` / `read_query_public` (`/v1/sql`, Ask's `sql` and
`map`) read only what each table's class allows, enforced by SQLite's
authorizer while the query is prepared and run
(`hearthloom-core/src/store/raw_sql.rs`), never by reading the SQL text:
- Every DB table must be classified in `raw_sql.rs::RAW_SQL_CLASSES`
  (SQLite's own in `SQLITE_TABLES`) as **Filtered** (a shadow view in
  `VAULT_SHADOW_VIEWS` — while locked, and always for `read_query_public`,
  read only through it), **Open** (nothing private, nothing secret),
  **Vault** (private content or text that can quote it, no exact filter:
  only with the viewer's vault open, never for `read_query_public`;
  refused, naming the table, otherwise) or **Never** (credentials,
  secrets, sessions). Unsure → Vault. A virtual table's own tables
  (`<vtab>_<suffix>`: FTS5 `_content`/`_data`/…, vec0 `_chunks`/…) are
  Vault by rule (`internal_table_of`) — only those listed in
  `INDEX_TABLES` (what the modules make for this store's declarations)
  that SQLite also counts as shadow tables (`shadow` in `PRAGMA
  table_list`). The label alone isn't proof: SQLite gives it to any table
  with a suffix the module claims, made by the module or not (vec0 claims
  `_auxiliary`; vec_chunks has none). One that merely shares the prefix
  (vec0's `_vector_chunks00`, which vec0 doesn't claim), one with a
  claimed suffix the module didn't make, and a new index's tables until
  they are added to `INDEX_TABLES` need a class of their own. A table
  named like an allowed table function (`json_each`) is judged as a
  table, never as the function.
  `test_every_table_is_classified_for_raw_sql` FAILS THE BUILD on an
  unclassified table, a class naming no table, or an `INDEX_TABLES` name
  that is no virtual table's shadow table in a new store; until
  classified, the authorizer refuses it.
- SQL functions are allowed or refused by name (`FUNCTIONS` /
  `FUNCTIONS_REFUSED`, `test_every_sql_function_is_classified`); only the
  schema/version pragmas in `PRAGMAS` and the table functions in
  `TABLE_FUNCTIONS` run; transactions, ATTACH and writes never do.
- A shadow view reads its tables through a CTE whose name is drawn
  afresh per query (`ShadowViews`): the authorizer trusts a read as the
  view's only behind that name, so a query's CTE named like a view gains
  nothing. A new shadow view must name every table it reads as
  `main.<table>`, with no `;` in its body and comments only on lines of
  their own.

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

# Hearthloom architecture

This is the map of the whole system: what the parts are, how data moves
between them, and which rules hold it together. It describes the code as
of 2026-10-05 (schema v51, release 0.5.59 plus the unreleased Ask and
provider work in CHANGELOG).

Read it when a task needs the whole picture. For anything narrower:

| You need | Read |
|---|---|
| Rules you must not break while editing code, deploy steps, gotchas | `CLAUDE.md` |
| One subsystem's contract | `docs/plugins.md`, `docs/mobile-client-contract.md`, `docs/agent-surface.md`, `docs/sources.md`, `docs/cli.md` |

Where this document and the code disagree, the code is right and this
document needs fixing.

## 1. What Hearthloom is

A personal second brain that the user owns outright. Everything a person
produces or receives (mail, calendar, messages, code, notes, health,
location, photos, AI conversations) is absorbed into one SQLite file and
a folder of plain files on their own disk. From that record the system
builds a knowledge graph, keeps a timeline, answers questions with
citations, watches for things the user said they care about, and can
share chosen slices with other people's brains.

Five design laws shape everything else:

1. **The user owns the substrate.** One SQLite file plus plain files.
   Every index can be rebuilt from the episode log. Nothing needs a cloud
   service to function.
2. **Zero gardening.** No feature may depend on the user maintaining
   links, tags or merges. The one exception is yes/no prompts on the Tend
   page.
3. **Retrieval is the product.** LLM tokens are spent at ingest and idle
   time, not on the query path. Search and graph traversal are always
   local. The two user-invoked exceptions are Ask and goal creation.
4. **The daemon is the sole writer.** Only `hearthloom-daemon` opens the
   database. Every write carries an actor and lands in an append-only
   log with an idempotency key.
5. **Provenance everywhere.** Every derived thing points back at the
   episodes that support it. Every episode carries source, trust tier,
   event time with timezone and uncertainty, ingest time, and a scope.

## 2. The shape of the system

```
 sources                      one node (laptop, or a hosted tenant)
 ───────                ┌──────────────────────────────────────────────┐
 exports, mbox, ics ───▶│ adapters ──▶ IngestBatch ──▶ ingest_apply     │
 git, iMessage, Photos  │                                 │            │
 iOS app (live feeds) ──▶ /v1/ingest/*                     ▼            │
 inbox drop folder ────▶│                          ┌──────────────┐     │
 plugins (sidecars) ───▶│                          │   SQLite     │     │
                        │   consolidation loop ◀──▶│  episodes    │     │
                        │   (idle: embed, extract, │  graph       │     │
                        │    classify, digest,     │  operational │     │
                        │    chapters, intents…)   │  ledgers     │     │
                        │            │             └──────┬───────┘     │
                        │            ▼                    │            │
                        │      LLM provider         read surfaces      │
                        │   (tiered, optional)   search · recall ·     │
                        │                        traverse · sql · Ask  │
                        └───────────┬───────────────────┬──────────────┘
                                    │                   │
              dashboard (embedded SPA) · CLI · MCP · /brain · iOS app
                                    │
              peer node (same owner, full trust)   friends' brains
              /v1/sync/* + push bus                (bounded trust)
```

A person usually runs two nodes: a laptop and an always-on hosted tenant.
They replicate to each other as equals. The laptop holds things only it
can see (the Photos library, iMessage); the tenant receives the phone's
feeds and does the idle-time thinking.

## 3. Code layout

Four Rust crates in one workspace, plus two sibling repositories.

| Crate | Role | Notable files |
|---|---|---|
| `hearthloom-core` | The substrate: schema and migrations, the store, config, secrets, providers, embeddings | `store/mod.rs` (migrations, vault, guarded SQL), `store/graph.rs`, `store/sync.rs`, `provider.rs`, `secrets.rs`, `brain_index.rs` |
| `hearthloom-adapters` | Turn external material into episode drafts. Never touch the store | one file per source: `mbox.rs`, `git.rs`, `ics.rs`, `apple_health.rs`, `facebook.rs`, `linkedin.rs`, `openai.rs`, `chatlog.rs`, `photos.rs`, `imessage.rs`, `chrome.rs`, `claude_code.rs`, `substack.rs`, `gmail_imap.rs`, `rewind.rs` |
| `hearthloom-daemon` | The only process that opens the database: HTTP API, consolidation, Ask, sync, plugins, the embedded dashboard | `http.rs` (about 230 routes), `consolidate.rs`, `rlm.rs`, `sync.rs`, `bus.rs`, `plugins.rs`, `virtual_source.rs`, `llm.rs`, `assets/` |
| `hearthloom-cli` | The `hearthloom` binary. Talks HTTP to the daemon and can run the daemon in-process | `main.rs`, `ops.rs` (auth, status, doctor), `eval.rs`, `registry.rs` |

The deployed artifact is the CLI binary: launchd runs
`hearthloom daemon run`. A standalone `hearthloom-daemon` binary also
exists and is not what runs in production.

Sibling repositories:

- [hearthloom-ios](https://github.com/metavert-io/hearthloom-ios): the iPhone companion. It posts device feeds to a
  node and is a client of the HTTP API (`docs/mobile-client-contract.md`).
- Hosting provisioners (Hearthlaunch is one): a service that runs hosted
  nodes. It talks to a node through the provisioner control plane; a
  self-hosted node needs none.
- [HearthFS](https://github.com/metavert-io/hearthfs): a separate
  filesystem project that Hearthloom mounts as a virtual source.

## 4. The data model

Everything lives in `~/.hearthloom/` (override with `HEARTHLOOM_HOME`):
`hearthloom.db`, `raw/` (original content by date), `notes/` (markdown
files), `archives/` (large exports and their sidecar indexes), `inbox/`
(the drop folder), `plugins/`, `BRAIN.md`, and `.env.sealed`.

The tables fall into five families. Knowing which family a table belongs
to tells you how it may be written, whether it can be rebuilt, and how
it syncs.

### 4.1 Episodes: the ground truth

`episodes` is an append-only log. One row is one thing that happened:
an email, a commit, a calendar event, a workout, a day of health data, a
conversation. Its content is never edited after ingest.

Every episode carries:

- `source` and `source_ref`, unique together. The id is a hash of the
  two, so the same item ingested twice, or on two nodes, becomes the
  same row.
- `event_time` with timezone offset and an uncertainty level, plus
  `ingest_time`. The two clocks are the basis of the bi-temporal model.
- `trust`: `self` (the user's own activity), `imported` (other people's
  writing, such as inbound mail), or `agent` (something Hearthloom or an
  agent wrote).
- `scope`: `work`, `personal`, `family`, `health` or `private`.
- `derivation`: `observed` (absorbed from a data flow), `declared` (the
  human stated it) or `inferred` (Hearthloom concluded it, citing
  evidence). This is the epistemic ledger: Ask can answer from facts
  only.
- `superseded_by`: lifecycle metadata for the supersession pattern below.

Scope and supersession are metadata and may change. Content may not.

**Supersession.** When an episode needs a fuller replacement (a day's
photos after a second device syncs, a health day after a later export),
the replacement is minted under a versioned ref (`<base>#r2`) and the
older revisions are marked superseded. Because ids are deterministic,
two nodes minting the same revision converge on one row, and the marking
is derived from the ref on receipt, so it syncs with no wire change.

**Retirement** (schema v59) is supersession by nothing: the person takes
a memory, or a whole source, out of the record (a phishing mail, a bad
import) and `superseded_by` becomes `retired:<id>`. Every reader of the
record honours supersession (a test fails the build on one that
doesn't), so the memory leaves search, Ask, the timeline, the galaxy,
BRAIN.md and the friend surface at once. What it fed is switched off
with its prior state saved in a node-local undo record: its entity links,
its edge support (an edge left with none is retracted), goal matches,
opportunities, map points and galaxy pairings; the briefs and
opportunities derived from it retire with it; entities that existed only
because of it are hidden; about-me beliefs that cited it go back to Tend.
Summaries that read it (digests, weekly reviews, chapter headings — they
record their inputs) are rewritten without it rather than patched. The
retirement EVENT syncs; each node applies it to its own rows, and one
undo, anywhere, puts everything back everywhere. See `store/retire.rs`.

### 4.2 The graph: derived, bi-temporal, reversible

`entities`, `entity_attributes`, `episode_entities`, `edges` and
`edge_provenance` form a knowledge graph over the episodes.

- Entities are people, organisations, projects, places, devices and so
  on. Identity is attribute-based: an email or handle bound to an entity
  is what makes two mentions the same person.
- Edges carry four timestamps: when the fact was true in the world
  (`t_valid`, `t_invalid`) and when the system believed it (`t_created`,
  `t_expired`). A contradicted edge is invalidated, never deleted.
- Merges are reversible events (`merges`, `merge_review_queue`). The
  system prefers to under-merge and to ask on the Tend page.
- `entity_renames` is an append-only audit trail replayed on the peer by
  type and name, because entity ids differ between nodes.

The graph can be regenerated from episodes plus the verdict ledger
(`extraction_verdicts`), which records what each extraction pass found
and at which model tier.

### 4.3 Indexes: rebuildable

`episode_fts` and `notes_fts` (FTS5), `chunks` and `vec_chunks`
(embeddings, with a sqlite-vec index), `location_points`,
`metric_points`, `semantic_pairs`, `note_links`, `note_neighbors`,
`lint_findings`. Any of these can be dropped and rebuilt.

### 4.4 Operational tables: primary and mutable

Tasks, notes, goals and the self-model are the user's present tense.
They are edited in place and cannot be rebuilt from episodes.

- `tasks`, `task_templates`, `task_mentions`
- `notes_index` with the bodies as markdown files under `notes/`
- `intents`, `standing_queries`, `intent_matches` (goals and what they
  have caught)
- `self_model` (propositions about who the user is, each declared or
  inferred, gated by Tend)
- `relationships`, `opportunities`, `privacy_rules`, `quarantine`

Because they are primary, every create, change and delete appends a
full-content post-image to `op_journal`. That journal is what makes a
deleted task restorable, and it is the only channel through which
deletes cross between nodes.

When operational items complete or settle, they mint episodes (a
finished task, a settled note), which is how the present becomes part of
the record.

### 4.5 Registries, ledgers and device feeds

- Registries: `sources`, `social_connections` (a friend list is a claim,
  not evidence, so it is not turned into entities), `contact_card`,
  `plugins`, `plugin_kv`.
- Ledgers: `mutations` (every write), `ask_log` (every Ask with its
  trace), `llm_usage` (one row per LLM call), `ingest_log`, `syslog`,
  `health_points`, `notifications`.
- Device feeds: `health_sample`, `photo_point` and `photo_day`,
  `location_observation` and `visit_day`, `device_state`. Raw samples
  land here and a finalize pass turns each closed day into episodes.
- Access: `users`, `sessions`, `api_keys`, `grants`, `shared_items`.
- `consolidation_state`: a key-value table for watermarks and settings.

### 4.6 Schema changes

Migrations run on open, keyed by `PRAGMA user_version`. Every table must
be classified as exported or excluded-with-reason in
`store/export_all.rs`, and every directory under the home in the
daemon's `export.rs`. A test fails the build on an unclassified table,
so new data cannot silently escape the full export.

## 5. Ingestion

All ingestion ends in the same place: an `IngestBatch` of
`EpisodeDraft`s applied by `store/ingest_apply.rs`, the single write
path for episodes. A draft names its source, ref, kind, title, body,
event time, trust, scope, and any entities and edges the adapter could
determine structurally.

Four ways in:

1. **Adapters over exports and local stores.** Each adapter in
   `hearthloom-adapters` reads one kind of material and emits drafts.
   Those that need macOS privacy permissions (iMessage, Photos) run
   daemon-side through `/v1/ingest/local`.
2. **The inbox drop folder.** Files placed in `~/.hearthloom/inbox/`
   (zips, mbox, ics, conversation exports) are recognised and ingested.
   Very large mail archives are refused there and go through the
   filtered pipeline.
3. **Live device feeds.** The iOS app posts to `/v1/ingest/*`: health,
   photos, contacts, calendar, reminders, location, motion, media, scans
   and focus state.
4. **Live pulls.** `gmail_pull` fetches new mail over IMAP into a rolling
   archive on the consolidation cadence.

Adapters mint entities structurally where the source is unambiguous (a
sender's address, a calendar attendee, a repository). Anything needing
judgment is left for the extraction pass.

### Materialized and virtual sources

A source is either copied into the episode log or left where it is.

- **Materialized** sources become episodes, and law 1 applies in full.
- **Virtual** sources stay at origin and are queried on demand: HearthFS
  mounts, whole mail archives through a sidecar index, the Apple Health
  numeric series. They are best-effort; an offline mount degrades to a
  clear error and the substrate never depends on one.
- **Promotion** copies one virtual item into the record on purpose, with
  origin and fetch time recorded. Untrusted origins are quarantined
  first.

A large archive can be both. The Gmail Takeout is materialized for its
core strata (sent, personal, starred) and virtual for all of it.

## 6. Consolidation: the idle-time mind

`consolidate.rs` runs a cycle shortly after start and every six hours.
It is where derived knowledge is produced and where almost all LLM
spending happens. It works, with less to say, when no provider is
configured.

The fixed part of a cycle, in order: refresh local sources, index
locations, backfill embeddings, update the vector index, decompose new
goals into standing queries, run goals against new episodes, write the
daily digest, classify scopes, extract entities, write monthly chapter
headings, and expand fresh goal matches into cascades.

Further passes are user-configurable automations, each with its own
cadence, tier and on/off switch: daily digest, chapters, friend
activity, Gmail pull, the three day-finalize passes (photos, health,
visits), semantic pairs, opportunity scan, relationship briefs, lint,
topic pages and the weekly review.

Rules the passes follow:

- **Everything an LLM writes lands as an episode or a label with a
  mutation.** A digest is an episode from source `consolidation`;
  extraction adds links; classification sets a scope. Nothing is edited
  silently.
- **Inferred conclusions go through Tend.** Self-model propositions,
  merge candidates, opportunities and topic-page revisions are proposals
  the user confirms.
- **Private scope never goes to a cloud model**, whether or not the
  vault is open. It is also excluded from persisted artifacts such as
  BRAIN.md and digests.
- **Some sources are never sent for extraction** (`mbox`,
  `apple-health`): the volume would burn months of budget and the
  adapters already mint their entities.
- **A pass may not hold a runtime thread for long.** Store-heavy passes
  run on blocking threads, take the store lock in small batches, and
  sleep between them. Each pass logs its duration.

A laptop paired with a healthy, key-holding tenant delegates the LLM
passes to it and runs only the local ones.

`BRAIN.md` is regenerated at the end: a byte-stable index of at most 200
lines (self-model, top entities, recent activity, a schema cheat sheet)
that agents load at session start.

## 7. The LLM layer

`core/provider.rs` defines a tiered provider. Every call site declares
how much reasoning it needs, `low`, `medium` or `high`, and the active
provider maps each tier to a model the user chose.

- **Providers**: Anthropic (default), OpenAI, Gemini, Grok, and
  `custom`, which is any OpenAI-compatible endpoint by base URL (Ollama,
  LM Studio, vLLM, OpenRouter), key optional.
- **One active provider, with per-tier routes.** The active provider
  serves every tier unless a tier is routed elsewhere, for example low
  to a local model. `RoutedProvider` does the dispatch.
- **Every call goes through `daemon/llm.rs::call`.** It times the call
  and writes a row to `llm_usage` on success and on error. Tokens are
  the stored truth; dollar cost is computed at read time from a price
  table that config can override.
- **Prompt caching.** Anthropic calls send the system prompt as a cached
  block, so a multi-round caller pays for it once.
- **Stop reasons** are normalised (ended, truncated, refused) and passed
  up so a caller can tell a cut-off reply from a malformed one.
- **Keys** live in `.env.sealed`, encrypted with a master key in the
  login keychain. The daemon decrypts in memory at boot.
- **Failure handling.** A billing or credential refusal sets a blocked
  state that pauses the passes until the user resumes them.

What syncs and what does not: the active provider, its tier models and
the sealed key travel to the user's own peer. The custom endpoint's
address and per-tier routes are node-local, because a loopback address
is meaningless on another machine.

Embeddings are separate and always local: a small ONNX model run
in-process. Without the model files, search falls back to text only.

## 8. Reading the record

### 8.1 The fast paths

These spend no LLM tokens and are fully local.

- **Search** (`store/search.rs`): FTS5 with BM25, fused with a vector
  leg by reciprocal rank. The vector weight is deliberately low (0.05):
  it backfills recall and does not rank.
- **Recall** (`store/recall.rs`): what happened on a day, in a span, or
  around an entity.
- **Traverse** (`store/graph.rs`): the neighbourhood of an entity,
  always capped per node. An uncapped traversal of the full graph froze
  the daemon once.
- **Browse** (`store/browse.rs`): faceted, paged reads over the raw
  tables for the dashboard's workbench.
- **Guarded SQL** (`Store::read_query`): read-only, single statement,
  row-capped, with the vault enforced by shadowing table names with
  filtered temporary views.

### 8.2 Ask: the deep-query engine

`daemon/rlm.rs` is the one place a user-invoked question spends LLM
tokens. The model is put in charge of its own data access: each round it
either requests evidence or gives a cited answer.

- **Handles**: `search`, `recall`, `traverse`, `read_episode`, `sql`,
  `map`, `llm_usage`, the virtual-source handles (`vsearch`, `vfetch`,
  `vquery`), and the friend handles (`nsearch`, `nfetch`, `ntasks`,
  `nrels`, `nnotes`, `nnote`, `noffer`, `sharing`).
- **Budget**: at most 8 rounds on the high tier. A round may batch up to
  4 independent calls.
- **Seeded start**: the hybrid search on the question is run before
  round 1 and handed over, so a simple lookup needs one model call.
- **Previews, not dumps**: large results stay server-side and the model
  sees a capped preview, so it learns to aggregate in SQL.
- **`map`**: the single level of fan-out. It runs a query of up to 40
  rows and has the low tier read each row against one instruction. It
  exists for questions where every row must be read. Sub-calls get no
  tools, and private rows are always excluded.
- **Citations are checked.** An id the model cites that never appeared
  in its evidence is removed and the answer is flagged.
- **Read-only, with suggestions.** Ask never writes. It may embed an
  action block that the dashboard renders as a button prefilling a task,
  a goal, a promotion or a "keep this finding"; a human completes every
  save.
- **Charts**: the answer may carry chart specs that one dependency-free
  renderer draws as inline SVG.
- **Tracing**: every ask is stored in `ask_log` with its full
  transcript. Follow-ups share a thread, and the history lists one entry
  per conversation.
- **Conversations are private as a whole**: one question asked with the
  vault open seals every turn of that conversation while locked — it is
  absent from the history and unreachable by any route.

Two safety properties are enforced in code, not in the prompt. Tool
results enter the conversation fenced as data. And the friend handles,
which send a model-chosen string to another person's node, run only when
the user's own words name that friend and before the ask has read
content the user did not write.

The design deliberately stops at one level of sub-calls; the reasoning
is in the design log 2026-10-05.

## 9. Privacy and access

### The vault

Scope `private` is the vault. It is locked by default, and while locked
every server-side read path excludes private rows with no trace that
anything was hidden (no counts, no "N hidden"). `Store::scope_guard`
produces the filter, and a test walks every read surface.

The vault is **per viewer**. An unlock belongs to whoever made it — one
signed-in browser session, one API key, or (on a node with no login) the
keyless local caller — and only that viewer's reads see private rows.
Each request runs with its viewer in a thread-local read scope
(`ReadScope`, next to the area filter), carried into blocking work and
spawned tasks by the daemon's `scoped.rs`; work without a viewer —
consolidation passes, sync — reads locked. While a viewer's vault is
locked it can move things into the vault but not out of it: relabels
pass over private rows and a vaulted note can't be edited.

Three rules sit above the lock state:

- Persisted artifacts and cloud LLM passes exclude private always.
- Privacy rules (`privacy_rules`) relabel matching content at ingest and
  retroactively, and outrank every other scope inference. The classifier
  may never clear a private mark.
- Some surfaces are gated wholesale while locked because their content
  could quote private material: the usage ledger and Ask traces.

### Who may call the daemon

- **Posture rule**: authentication is required exactly when a user row
  exists. Creating the admin locks the door in the same gesture.
- **Sessions**: an argon2id password and a hashed session token in an
  HttpOnly cookie for the dashboard.
- **Personal access tokens** (`api_keys`): hashed bearer tokens, in two
  kinds. An ordinary key carries scopes (`read`, `write`, `ingest`,
  `notify`, `peer`, `mcp`, `friend`, `admin`). An `admin` key and the
  paired phone's key (`read`+`ingest`+`notify`) are the owner's and
  reach everything. Any other ordinary key reaches only the routes its
  scopes are listed for in the key ceiling (`KEY_ROUTES` in http.rs,
  default-deny, matched on the router's template): `read` the record and
  `/mcp`, `write` the Desk, `ingest` device feeds and proposals, `notify`
  the bell, `peer` the sync legs. Keys, plugins, export, upgrades,
  settings, privacy, sharing, agents and Ask stay the owner's. Unlocking
  the vault and trading a key for a session need the owner too. A key
  whose only scope is `friend` (another person's key) is held to the
  friend-surface route table. What a friend may read there is their own
  grant plus the grants of any groups the owner put them in, evaluated
  one grant at a time.
  A scoped key carries grants per class (tasks, notes, memory, ask,
  capture) and reaches only a default-deny route table; it can expire,
  is rate-capped, never sees private material (it can't unlock), and
  everything it writes is attributed to it and can be undone. An agent
  can request a scoped key and the owner approves it.
- **The browser rule**: cross-origin access (CORS) is granted only to
  requests carrying a bearer key. A keyless request that a page on
  another site made (`Sec-Fetch-Site`, or a foreign `Origin`) is refused
  on every API path, and a node with no login answers keyless calls only
  at its dashboard address — loopback, or the host of
  `HEARTHLOOM_DASHBOARD_URL` — so a web page can't act as the owner and
  DNS rebinding finds nothing.
- **Provisioner control plane**: an HMAC-authenticated channel above all
  tokens, used by the hosting service, with a killswitch. Export and
  vault access always survive the killswitch so a user can take their
  data.

### Trust in content

Agent-proposed memories go to `quarantine` and wait for the user.
Promoted content from untrusted origins is quarantined first.
Credential-shaped tokens are redacted from everything Ask shows a model.

## 10. More than one node

Two different relationships exist between nodes. They use different
endpoints and different trust models, and they must stay separate.

### Peering: one owner, full trust

Two nodes owned by the same person replicate everything, including
private data, under `/v1/sync/*` with the `peer` scope.

- **Multi-master merge.** Both nodes are authoritative. Operational rows
  are upserted by id under last-writer-wins and never replaced wholesale;
  rows unique to one side survive.
- **Deletes travel only through the journal.** A delete tombstone in
  `op_journal` is replayed on the peer, and the mirror refuses to
  resurrect a row older than a local tombstone.
- **Legs by kind**: episodes, the operational mirror, the journal,
  extraction verdicts, photos, locations, device state, contacts,
  sharing posture, plugin data, telemetry and the usage ledger.
- **The push bus** (`bus.rs`): a server-sent event stream of hints that
  something changed. A hint triggers a debounced sync; data never rides
  the bus.
- **Key sync**: the LLM key is sealed to the peer's public key so the
  always-on node can think.
- **Default**: any new table holding user-visible state gets a sync leg
  unless an exclusion is decided and written down.

Peering stays simple on purpose. There is no capability negotiation; a
missing endpoint is detected and skipped.

### Federation: many owners, bounded trust

Connected friends reach a deliberately small surface: `/v1/network/*`
and `/brain`, with the `friend` scope.

- **Consent matrix** (`store/sharing.rs`): a default template plus
  per-friend overrides across classes (episodes, tasks, notes,
  relationships, profile), and explicit per-item shares that punch
  through. Private is never grantable.
- **Can, may, will-enforce.** What a node can do is a capability; what
  it offers a given friend is computed from grants; what is enforced is
  the scope guard at read time. The offer is a courtesy and the guard is
  the control.
- **One friend write**: toggling an item on a shared checklist.
- **Compatibility**: gate on a protocol number and capability strings,
  never on a release version. Fields are only ever added.

## 11. Surfaces

- **Dashboard**: a single-page app embedded in the binary and served at
  `/`, with no build step and no CDN. Home is the Desk (tasks, goals,
  notes). Other views: Notes, Memories (the workbench with the Dial
  timeline), Relationships, Day, Search, Tend, Privacy, Integrations,
  Settings, Ask, and a Visualize group (Loom, Orrery, Constellation and
  Galaxy, Atlas, Pulse, Biography, Ribbons). New asset files must be
  registered in `dashboard.rs`.
- **CLI**: every command is an HTTP call. `hearthloom doctor` turns the
  known failure modes into checks; `status`, `auth`, `key`, `vault`,
  `export`, `sync`, `ember`, `eval` and `bench` cover operation.
- **About me** (in the menu): everything Hearthloom holds as true about
  its owner in one place, with origin, evidence and the controls to
  correct it, plus the exact briefing agents load.
- **Spend** (in the menu): every model call from the usage ledger,
  priced at read time and broken down by day, model, purpose, provider
  and tier, with per-question costs and a CSV report. Locked with the
  vault.
- **MCP** at `/mcp`: stateless, exactly four tools (search, traverse,
  recall, propose_memory). New capability is added as parameters, not
  tools: `propose_memory` can also add a task or note directly when the
  calling key holds write on that class.
- **`/brain`**: a read-only, file-shaped view (BRAIN.md, days, entities,
  goals) that always serves the vault-locked view. HearthFS mounts it.
- **Plugins ("embers")**: sidecar processes with scoped tokens behind a
  default-deny route ceiling. Their storage is mediated by the daemon,
  and their UI runs in sandboxed iframes, never in the dashboard origin.
  A registry distributes signed packages.
- **Notifications**: a queue of hints with a link allowlist, exchanged
  both ways between peers.
- **Full export**: one zip that can rebuild a node (database snapshot,
  readable tables, files). It requires the vault unlocked, and restore
  applies only to an empty node.

## 12. Running it

- **Local**: launchd keeps `hearthloom daemon run` alive on
  `127.0.0.1:43278`.
- **Hosted**: a Fly machine per tenant, one volume, deployed by updating
  the machine's image in place. Never `fly deploy` against a tenant app.
- **Observability**: `monitor.rs` samples vitals every minute into
  `syslog` and `health_points`. Sentry is optional and gated by both
  operator config and user consent.
- **Evaluation**: `hearthloom eval` checks the fast paths with no tokens
  spent. `hearthloom eval --ask` runs the Ask engine and reports rounds,
  tokens, cost and latency; use it before and after changing a model or
  the Ask loop. `hearthloom bench` measures query latency.

## 13. The rules that hold it together

Each of these encodes an incident or a hard decision. The detail and the
history are in `CLAUDE.md` and the design log.

**Data**
- Episodes are append-only; edges are invalidated, never deleted; merges
  are reversible.
- Operational writes journal a full post-image first.
- Every table and home directory is classified for export.
- Peer sync merges and never replaces; deletes cross only by journal.
- Every reader of the record honours supersession (so retirement); a
  build-failing test scans for readers that don't.

**Privacy**
- Every new read query carries the scope guard.
- Private never reaches a cloud pass or a persisted artifact.
- Enforcement lives in the store and the tool layer, not in a prompt.

**Cognition**
- LLM spend stays off the query path, except Ask and goal creation.
- Every LLM call goes through `llm::call`.
- LLM output lands as episodes or labels, and inferences pass through
  Tend.
- The Ask system prompt stays byte-stable so it caches.

**Performance** (the store is one fair mutex; a long hold freezes the
node)
- Cap every traversal.
- Never join FTS on an id; use the rowid pointer.
- Heavy handlers and passes run on blocking threads and cache if polled.
- Loops that retake the lock sleep between batches.

**Interfaces**
- The MCP tool count stays at four.
- Wire formats are additive; unknown fields are ignored; a missing
  operation is skipped, not fatal.
- The system stays useful with no LLM provider and no embedding model.

## 14. Known gaps

Things this document's author looked for and could not confirm, or that
are decided but not built:

- Retirement can't trace everything a memory touched: entity attributes
  (an email address it introduced) don't record their source, nor do
  relationship briefs and topic pages; digests and weekly reviews written
  before v59 are matched to a memory by time window, not by a recorded
  read. The preview says so each time.

- Derived episodes (digests, chapters, self-model propositions) are
  minted with trust `agent` regardless of what they summarised. Nothing
  carries the lowest input trust forward.
- Ask's conversation is one growing string, so only the system prompt
  caches.
- The Ask eval's retraction cases are templates until the owner supplies
  ground truth.
- A local distilled extractor for the sources kept off the cloud is
  scoped in the design log and not started.
- Federation's versioning and manifests are designed and deferred until
  federation grows beyond the current friend surface.

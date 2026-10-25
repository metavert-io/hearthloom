# Hearthloom

**A local-first personal second brain.** Hearthloom turns the exhaust of your
life — commits, calendars, email, messages, photos, browsing, AI
conversations — into an episode-sourced, bi-temporal knowledge graph that
lives in **one SQLite file on your own disk**, consumable by any agent, and
eventually networkable with other brains.

One sentence: *an episode-sourced personal knowledge graph on your own disk,
consumable by any agent through progressive-disclosure interfaces, with
selective sharing into realms shared with other brains running the same
software.* (See `architecture.md` for how it is built.)

## Design laws

1. **You own the substrate.** One SQLite file + plain files on disk; every
   index rebuildable from the episode log; zero cloud dependency to function.
2. **Zero gardening.** No feature depends on you maintaining links, tags, or
   merges. Background consolidation does the work. (One exception: yes/no
   "is this the same person?" review prompts.)
3. **Retrieval is the product.** LLM tokens are spent at ingest and idle
   time, never on the hot query path. Search and traversal are always local.
4. **The daemon is the sole writer.** Every write carries actor identity and
   lands as an append-only event with an idempotency key.
5. **Provenance everywhere.** Every entity and edge cites the episodes that
   support it; every episode carries source, trust tier, timezone-honest
   event time, and a scope label (`work|personal|family|health|private`).

## Features

### Ingestion (14 adapters + importers, all idempotent)
| Source | Command | What becomes an episode |
|---|---|---|
| Git repos | `hearthloom ingest git <path>` | each commit (author/repo/files as entities+edges) |
| Calendars (.ics) | `hearthloom ingest ics <file>` | each event; attendee emails drive person identity |
| Email (mbox/Takeout) | `hearthloom ingest mbox <file> [--include-labels "Sent,Category Personal,Starred"]` | each kept message (streaming — any size; Gmail-label strata filter, Spam/Trash always dropped; filtered runs execute daemon-side in the background with progress in daemon.log) |
| Claude.ai / ChatGPT exports | `hearthloom ingest chatlog <conversations.json>` | each conversation (account owner auto-detected from users.json) |
| Claude Code sessions | `hearthloom ingest claude-code` | each local session transcript |
| iMessage | `hearthloom ingest imessage` | one per chat per day (needs Full Disk Access) |
| Apple Photos | `hearthloom ingest photos` | one per day: timestamps, GPS, reverse-geocoded places → a location trace (needs FDA) |
| Chrome history | `hearthloom ingest chrome` | one per profile per day, with top-sites rollups |
| Substack | `hearthloom ingest substack <url>` | each article, full text (public archive API; custom domains work) |
| Apple Health | `POST /v1/ingest/local` `{"adapter":"apple-health","path":…}` on an unzipped export | one `health_day` aggregate per day (activity/heart/sleep/body/glucose/mindfulness, source-priority deduped) + each workout/ECG/clinical record; raw samples go to the vquery sidecar, never the episode log |
| ChatGPT full export | Integrations → ChatGPT (or the export zip in the inbox) | each conversation from the account export |
| Facebook DYI dump | Integrations → Facebook (extracted archive) | posts (with place tags), albums, message thread-days, events, groups, profile identity attrs; the friends *list* goes to a connections registry, never straight to entities |
| LinkedIn export | Integrations → LinkedIn (extracted archive) | positions (the career spine), education, honors, publications, authored articles; ~connections with company/position detail feed the same registry |
| Notion export | Integrations → Notion (export zip) | pages become editable notes (re-imports upsert by page id; embedded images preserved) |
| Markdown vault (Obsidian/Logseq/Reflect) | Integrations → Markdown vault (path) | every .md becomes a live note — wiki-links, tags, frontmatter dates, images; read in place, never moved; or **mount** it as a virtual source and stay in Obsidian |
| Rewind (rescue) | Integrations → Rewind | the remote-killed lifelogger's stranded database: app sessions → one episode/day, recorded conversations → transcripts |

**Live device feeds (the iOS companion's endpoints)**: the phone streams
straight into the daemon over PAT-authed `POST /v1/ingest/*` —
`health` (HealthKit samples + workouts), `photos` (geotags, never
images), and since 0.5.58 `contacts` (the identity backbone — cards
resolve into Person entities attribute-first), `calendar` (live events,
deduped against .ics imports by event UID), `reminders` (into the task
model), `location` (CLVisit dwells), `motion` (trip episodes), `media`
(songs/podcasts → a taste dimension), `scan` (receipt/card/document OCR;
the extraction pass mines merchants and people), and `focus` (attention
states into a generic device-state stream). Every geolocated signal —
photo geotags, GPS dwells, calendar venues, scan captures — lands in ONE
unified observation layer that the idle-time visit pass clusters into
canonical `visit` episodes, so a photo + a GPS dwell + a calendar event
at the same place/time become one visit citing all three sources, never
three episodes. A calendar event on its own becomes a visit only if you
allow it (Integrations → Pair a device → "Count a calendar event on its
own as a visit"; off by default, synced between your nodes): off, a
visit needs your phone or a photo to confirm you were there; on, any
event with an address becomes one, invites from other people included.
`GET /v1/ingest/status` + `/v1/ingest/recent` are the
device's audit trail that its streams actually land.

TCC-gated sources (iMessage, Photos) also work via `POST /v1/ingest/local`
— the daemon (which can hold its own Full Disk Access grant) runs the
adapter server-side, and the CLI falls back to it automatically. Known
local sources re-ingest themselves daily during consolidation.

**The inbox drop folder**: drag any export into `~/.hearthloom/inbox/`
(zip/tgz archives, .mbox, .ics, conversations.json, or Finder-copied
chat.db / Photos.sqlite) and the daemon extracts, ingests, and files it
under `inbox/processed/` — failures go to `inbox/failed/` with an error
note. Recognized big-export zips (ChatGPT/Facebook/LinkedIn/Notion/Apple
Health) are staged into `~/.hearthloom/archives/` ready to run. Nothing is
ever deleted. One guard: an mbox over 250MB is refused — a full Gmail
Takeout is mostly bulk mail and belongs to the filtered pipeline above,
not an unfiltered drop.

**Downloads auto-detect**: on a local node, the Integrations page scans
`~/Downloads` for export files it recognizes and offers a one-click
"found — import" that moves them into place; every import card also takes
a direct upload (the primary path on a cloud node).

### Identity: many identities, one human
- Identifying attributes (email, phone, handle, repo-url) resolve entities
  across sources: the same email in a commit, a calendar invite, and an
  mbox is the same Person.
- `hearthloom identity claim email you@example.com --as "Your Name" --scope work`
  declares an identity yours; if another entity owned it, it's merged in
  **reversibly** (`identity revert`). Reads are merge-group-aware.
- Scopes keep contexts distinct: a `work`-labeled identity's material
  defaults to `work`; a repo marked `personal` stays personal regardless of
  which email committed. Correct labels anytime: `hearthloom scope <entity>
  <scope> [--cascade]` or `--by-source` for a whole import.
- Ambiguous matches queue for a yes/no: `hearthloom identity reviews` /
  `resolve --same|--distinct`.

### Retrieval
- **Hybrid search**: SQLite FTS5 (BM25) fused with local vector recall
  (all-MiniLM ONNX via tract, brute-force cosine) using weighted RRF at a
  deliberately low vector weight — the vector leg backfills recall, it
  doesn't rank. Fully local, no network, degrades to FTS-only without the
  model.
- **Temporal recall**: `hearthloom recall --on 2026-03-15` reconstructs a
  day in *local* time (episodes honor per-source UTC offsets) with cited
  entities and the edges valid that day. Spans and entity recall too.
- **Bi-temporal graph**: edges carry t_valid/t_invalid/t_created/t_expired;
  `traverse` accepts `as_of` for time-travel reads.
- **Progressive disclosure**: search and recall take `detail: summary|full`;
  summary caps and rolls up so agent context stays lean.

### Cognition (idle-time only)
- **Consolidation cycle** (daemon loop + `hearthloom consolidate`) runs six
  jobs: re-ingest known local sources (≤1/20h), embed new chunks, check
  intents, write the **daily digest** (episodic→semantic compression +
  self-model notes, stored as an agent-trust episode), **classify scopes**
  (per-episode work/personal refinement of coarse account labels), and
  **extract entities** (explicitly-named People/Orgs/Projects/Places mined
  from conversations, messages, mail, and articles — linked with role
  `mentioned`, 48 episodes/cycle). All LLM output lands as episodes or
  labeled corrections in the mutations log — never silent edits.
- **Intents**: standing queries (`hearthloom intent add fusion-news fusion`)
  checked against new episodes each cycle; matches surface on the change
  feed; cascades stay lazy (viewing marks seen).
- **Writing for you**: monthly **chapter headings** (the timeline's and
  biography's widest labels), a **weekly review** (the ideas that
  mattered, old↔new connections, what to feed next), **cascades** that
  expand a goal's fresh matches, relationship briefs, topic-page drafts
  and self-model beliefs for Tend. The bell announces a new weekly review
  and the heading of the month that just ended.
- **BRAIN.md** (`~/.hearthloom/BRAIN.md`): a byte-stable ≤25KB index — the
  self-model digest, top entities, recent activity, schema cheat-sheet —
  loadable by any agent at session start.

### The dashboard (http://127.0.0.1:43278)

An offline-first SPA embedded in the daemon binary (three.js vendored, no
build chain, no CDN), in the "embers at night" design system — dual-theme,
serif display type, ember accents. The **Desk** is home (see above);
alongside the views below live **Loom** (source-thread streamgraph),
**Pulse** (metric oscilloscope over the health sidecars), **Biography**
(life-heatmap + year spreads), and the **Galaxy** — the constellation's
whole-record altitude: sources as suns, their most-cited entities as
planets, multi-source people hanging between their suns, with a
camera-flight zoom into any one source's system.

- **Day** — the composed day view: chronological, offset-aware timeline
  with source filter chips, place chips ("where was I"), and expandable
  episode cards that render markdown with full provenance (source, scope,
  trust, episode id).
- **Search** — hybrid recall with highlighted snippets, expandable hits.
- **Dossiers** — an entity's identities, relations (with citation counts),
  and recent evidence.
- **Constellation** — a three.js rendering of an entity's local 1–2 hop
  neighborhood: glowing type-colored bodies, warm threads, starfield, orbit
  + pan + zoom (on-screen nav pad and arrow keys), click to refocus,
  double-click to open the dossier. Hard caps keep it a constellation —
  never the global hairball the design doc forbids.
- **Ribbons** — temporal ribbons: top entities as monthly evidence-intensity
  bands across the full history; the visual form of the bi-temporal model.
- **Atlas** — the Atlas of Embers: your presence on a vendored vector
  world (no tiles, no geocoder — coordinates come from your own photos
  and workout routes). Glowing points per day, hover for the place,
  click for the day, and a **play button that animates your movements**
  across any timeframe as fading flight-map arcs. Ask can answer
  location questions with the same map inline (`map` chart type).
- **Tend** — the only gardening surface: merge-review yes/no cards,
  quarantined memory accept/reject, standing-watch management; a nav badge
  glows *only* when a review or quarantined memory is actually pending.
- **Memories** (first stop) — the data workbench: faceted, sortable, infinite-scroll grids
  over episodes / entities / identities / edges / sources, with facet chips
  (counts computed except-own-dimension), date-range + text filters,
  expandable episode rows, and per-row **and** bulk scope correction —
  including one-click *mark private*.
- **Privacy** — the vault card + privacy-rule manager (see below); a
  masthead 🔒 toggles the vault open/closed from anywhere.
- **First-run wizard** (#/welcome — an empty brain lands here
  automatically): create the admin login right in the browser (on a
  node without one, the door locks the moment it exists), restore a
  full export, claim your identities, connect an AI provider and live
  Gmail, pair your phone by QR, link a second node, and run the first
  local ingests — every step skippable, the reminder banner brings you
  back.
- **The Dial** — a fisheye drum timeline above Browse's episode grid and
  search results: your history wraps the edge of a cylinder — spin it
  with momentum, pinch to stretch from a week to four decades, watch
  ember density flow past the hairline. On Browse the drum *is* the
  date filter; after a search it zooms to where the hits live in your
  life. Perspective handles crowding: marks foreshorten toward the
  rims, and at most eight labeled leader lines ride above the drum.

### The Desk — the present tense
The record looks backward; the **Desk** (the dashboard's home) looks at
today: tasks (plain, checklists, workout loggers, template forms; hashtags
set priority/recurrence/due dates), markdown **notes** (plain files under
`~/.hearthloom/notes/`, the DB only indexes them; archiving a note
snapshots it into the record), and **goals** — intents grown up, LLM-
decomposed into standing watches that surface fresh evidence. `@[Name]`
mentions in notes and tasks (typed — `@[Org:Hearthloom]` disambiguates
same-name entities) link straight into the graph, and **Relationships**
gives the people in the record an evidence-tiered directory with
declare-a-kind chips. Projects are declarable entities with renameable
names — renames ride an append-only audit trail (`entity_renames`) so a
peer replays them by name. Completing a research-template task offers a
prefilled linked note: the task → research → note loop.

### Two nodes — peering
Pair your laptop with an always-on counterpart (a Fly.io app works out of
the box) and a background **hybrid sync** keeps them converged every five
minutes: episodes flow both ways, scope/privacy relabels replicate
last-writer-wins, and the operational layer (tasks/notes/goals/
relationships) mirrors to the peer. The **push bus** rides on top —
notify-then-fetch event hints over one outbound SSE subscription plus
outbound POSTs (NAT-friendly: the laptop dials, nothing dials the laptop)
— so a Desk edit, a rename, or a privacy change lands on the other node
in seconds, with the five-minute poll as the reconciling backstop and
graceful poll-only degradation against an older peer. The peer is a full
copy — private included — locked behind the same vault and login on both
ends. **LLM key-sync** (the credential sealed to the peer's public key,
adopted only into an empty slot) means you configure a provider once, on
either node. Both **Anthropic** (default) and **OpenAI** are supported
providers, selectable per node with per-tier model overrides; a synced
key lets the always-on node carry the idle-time LLM passes for both.

### Deep queries — Ask Hearthloom
`hearthloom query "reconstruct the kitchen renovation, month by month"`
(or the **Ask ✦** panel in the dashboard): an LLM iterates over the brain's
own tools — hybrid search, recall, graph traversal, episode reads, and
guarded read-only SQL — then synthesizes a cited answer. Tool rounds stream
live, and the answer itself appears in the panel as it is written (Anthropic,
OpenAI and Grok; the checked final text replaces the preview); a debug toggle reveals the engine's reasoning; every ask (including
failures) is traced in `ask_log` as searchable chat history and QA material
(items are deletable — permanently — behind a confirm modal). Conversations
are **multi-turn**: follow-ups ("try again", "now break that down by month")
carry the thread's earlier questions and answers as context, and opening a
past conversation from history replays and continues it. When the answer
is quantitative, the engine embeds **inline charts** (bar, line, area, donut,
pie, scatter, heatmap — picked to match the question's shape, rendered as
theme-aware SVG with no chart library). ☆ saves a question as a **recipe** —
a note tagged `#ask-recipe` — and an empty panel offers your recipes as
one-click chips.
This is the one *user-invoked* exception to idle-only token spend. An ask run
while the vault is unlocked is flagged sensitive and hides from the history
again once the vault re-locks (its answer may quote private material).

### Crash & performance reporting — optional, doubly opted-in (0.5.59)

Hearthloom can report crashes, error lines, and HTTP timings to a
[Sentry](https://sentry.io) instance — but only through two independent
gates. First, the **operator** must configure a DSN (`SENTRY_DSN` in
`~/.hearthloom/.env` — set it with `hearthloom secret set SENTRY_DSN`;
on a hosted node, machine env). No DSN — the default for every
self-hosted build; none ships in this repo — and the SDK never
activates. Second, the **user** must consent: the Settings →
diagnostics toggle ("Share error reports"), which gates every event
client-side and applies instantly. Hosted distributions may default the
toggle on via `SENTRY_DEFAULT_OPTIN=1` (disclosed in the card; an
explicit choice always wins). What can ship: panics, `tracing::error!`
lines, and route-level timings — never episode content, request bodies,
or auth headers (`send_default_pii` is hard-off). Incoming
`sentry-trace` headers are continued, so a client app's traces stitch
into the daemon's.

### LLM providers — tiers, a custom endpoint, per-tier routing

Every internal call declares a tier (low | medium | high) and the active
provider maps each tier to a model you choose. Besides Anthropic, OpenAI,
Gemini and Grok there is `custom`: any OpenAI-compatible endpoint (Ollama,
LM Studio, vLLM, OpenRouter) by base URL, key optional. One tier can be
sent to a different provider than the rest — Low to a local model, say —
under Integrations → AI provider → "Send one tier to a different
provider". The custom address and the routes stay on the machine they
were set on; they are not synced to a paired node.

`hearthloom eval --ask` runs `~/.hearthloom/eval/ask-eval.yaml` (your own
questions; see `eval/README.md`) through the full Ask
engine and prints pass/fail with rounds, tokens, cached share, cost and
latency per question. It spends real tokens, so it is separate from the
default `hearthloom eval`. Use it as the before/after baseline for any
model or prompt change.

### Keys, agents and what they may do

☰ → API Keys mints two kinds of key. An **ordinary key** carries scopes,
and each scope reaches its own part of the API only: `read` the record
(search, recall, entities, timelines, notes, tasks, goals, `/brain` and
MCP), `write` the Desk (tasks, notes, goals, relationships, projects),
`ingest` device feeds and proposals, `notify` the bell, `peer` your
other node's sync, `mcp` the MCP endpoint. Everything else — keys,
plugins, export, upgrades, settings, privacy, sharing, agents, Ask —
belongs to the owner: a signed-in session, an `admin` key, or your
paired phone. A **scoped key** carries grants — none, read or write for
tasks, notes and memory, plus Ask and a write-only Capture — and reaches
only those: it can expire, is rate-capped, and everything it writes is
listed under the key with an Undo. A scoped key can also be limited to
life areas (work, personal, family, health): it then reads only those,
and cannot use Ask. A scoped key you mint for one of your **own
devices** — your phone's lock-screen capture, a Shortcut you run — can be
marked "My own device": what it writes then counts as your own words,
fully trusted, instead of an agent's (it reaches nothing more). An agent
can also ask for a key (`POST /v1/keys/request`); you approve or deny it
on the same page.

**The vault is per viewer.** Unlocking it shows private material to
whoever unlocked — that browser session, or that key — and to no one
else: your phone, MCP clients and agents keep the locked view until they
unlock for themselves. Unlocking, and turning a key into a signed-in
session, are the owner's alone (a signed-in session, an admin key, or a
paired phone's key). While your view is locked you can move things into
the vault but not out of it. A key minted for a connected friend reaches
only the sharing routes. Over MCP the four tools are unchanged, but
`propose_memory` can add a task or note directly when the key holds
write on that class; a proposal keeps only its words, and an accepted
one lands as an agent-trust memory, never as your own.

**Web pages can't use your node.** A call from a page on another site
needs an API key, and a node without a login answers keyless calls only
at its dashboard address: `127.0.0.1` or `localhost`, or the address you
set in `HEARTHLOOM_DASHBOARD_URL` (see Configuration).

### Agents — Ask can act for you

Ask answers questions; ask it to *do* something ("email Ada the summary",
"tidy my downloads folder") and it hands a self-contained task to one of
your agents — Claude Code on this Mac, Hermes, or OpenClaw — and shows a
card you can approve, follow and stop. ☰ → Agents (behind the vault)
sets them up and sets the rules: for each kind of action (look things up,
send messages, calendar, spend money, change files, use websites…)
"do it", "ask me first" or "never", starting on "ask me first". After a
task has read things you didn't write, every action asks you first,
whatever you set (a switch on the same page turns that off). Each agent reads your record through its own
read-only key, limited to the life areas you choose; everything it does
is recorded, and approvals work from anywhere you're signed in.

### Sharing with other people's Hearthlooms

☰ → Network connects your record to other people's. What a connection
may read is set once as a default, overridden per person, and extended
item by item (a task, a note). **Groups** are named sets of connections
with their own additive sharing — share a packing list with "Family"
instead of with each person. A connection's key reaches only the
sharing routes; private material is never shareable.

### About me and Spend

☰ → About me shows everything Hearthloom holds as true about you, where
each belief came from, and lets you correct it — confirm, mark "no longer
true", **make private** (into the vault, out of every agent briefing), or
**remove** a belief that was never right (it is kept as rejected so it is
never proposed again). ☰ → Spend prices every
model call from the usage ledger and breaks it down by day, model,
purpose and question, with a CSV export — including the free embedding
runs on this machine. A **monthly budget** on that page sends one
notification at 80% and one at 100% of the figure you set (an alert, not
a cap). Both pages follow the vault.

### LLM observability — token accounting & query traces (v50)

Every LLM API call — each Ask round and every consolidation pass — lands
one append-only row in the `llm_usage` ledger: purpose, model, tier,
input/output/cache tokens, latency, ok/error. Tokens are the stored
truth; dollar figures are computed at read time from a built-in
per-model price table (overridable via `llm_pricing:` in config), so
price drift never corrupts history, and an unknown model shows "no
price on file" rather than an invented cost.

Two surfaces read the ledger. **☰ → Monitoring → LLM usage** is the
reconciliation view: totals, a cost-per-day chart, and by-purpose /
by-model tables over 7/30/90 days, scoped to either node or both (the
ledger peer-syncs like telemetry, so one dashboard shows both nodes'
spend against the provider's console). The **⌗ debug button** on every
Ask history row drills into a complete trace of that query: per-round
model, LLM latency, token counts, tool name/args/reasoning, the full
tool result, tool latency, and the ask's ledger rows with a priced
total. Both surfaces require the vault **unlocked** — traces can quote
vaulted content, so a locked vault refuses outright (403), with no
filtered fallback.

### Privacy — the vault and privacy rules
- **Scopes** already partition contexts (`work|personal|family|health|
  private`); `private` is the one *hidden* tier.
- **The vault** defaults **locked**. While locked, every server-side read
  surface — search, recall, dossiers, traverse, ribbons, Browse, stats, the
  four MCP tools, Ask history, and even raw read-only SQL (via transient
  view shadowing) — excludes `private`-scoped material with **zero leakage**
  (no "N hidden" counts). Unlock is a self-expiring window (15 min default,
  ≤60) via the masthead 🔒 or the Privacy page — and only by the owner
  (see Keys, above).
- **Privacy rules** (domain / entity / place / term) relabel matching
  material `private` both retroactively and at ingest, outranking all other
  scope inference; the idle-time classifier may never overwrite a private
  mark.
- **Private never leaves**, vault open or not: BRAIN.md, the daily digest,
  and every cloud-LLM pass (scope classification, entity extraction) always
  exclude `private` content.

### Retiring a memory — reversible repair (0.5.69)
- **Retire** any memory from its expanded card (Day view, Memories), or a
  whole source from Memories → Sources — a phishing mail, a bad import, a
  source you no longer trust. It leaves the whole record: search, Ask,
  the timeline, the galaxy, Relationships, BRAIN.md and what friends can
  see. **Nothing is deleted.**
- **Preview first.** Counts of what it fed (links to people and things,
  connections it supported, goal matches, opportunities, map points,
  pairings); the briefs and opportunities derived from it, which retire
  with it; the people and organisations that existed only because of it
  (hidden, never deleted); the digests, weekly reviews and chapter
  headings that read it, ticked to be **rewritten without it** (untick to
  keep); the about-me beliefs that go back to Tend; and what can't be
  traced.
- **One confirm, one undo.** Every row it changed is recorded with its
  prior state; Undo (the toast, the card, or Memories → Retired) puts all
  of it back. Retirements travel to your other nodes, and anything that
  arrives later from a retired source lands retired.

### Materialized and virtualized sources
Today all ten adapters **materialize**: data is cloned into the episode log
you own. Alongside them, **virtualized** sources stay at origin and are
queried at need — appearing in a source registry, surfacing as read-only
`vsearch`/`vfetch` tools in the deep-query loop, and *promoted* into the
substrate on demand via an explicit write (provenance recording their origin
and fetch time; untrusted fetches pass through quarantine first). The
substrate never depends on a virtual source being online — an offline mount
degrades to a clear error. Three virtualized source kinds are live:
**HearthFS mounts** (registered and ready —
`hearthloom source register|search|promote`) and **mbox mail archives** —
a sidecar index (`<archive>.vidx.sqlite`, built in the background at
registration, rebuildable any time) makes a Takeout-scale archive
searchable by from/subject/labels/date and promotable one message at a
time, so hundreds of thousands of bulk messages stay at origin while
remaining a `vsearch` away. A big archive serves both tiers at once:
materialize its high-value strata with the label filter, virtualize the
long tail. **Numeric sources** (Apple Health) get a different verb:
**vquery** — read-only SQL over a samples + daily-rollups sidecar, so
"compare my resting heart rate before and after I started running" is one local
`GROUP BY` at query time. Coverage gaps are themselves signal: every
device/app is an entity whose Ribbons band fades exactly when you
stopped wearing it. More kinds (other brains) will follow the same
contract.

**Gmail live federation** keeps the mail record current past the last
Takeout: set `GMAIL_IMAP_USER` and `GMAIL_IMAP_PASSWORD` (a Gmail app
password — requires 2FA) in `~/.hearthloom/.env` and the `gmail_pull`
automation pulls new mail over IMAP each consolidation cycle into a
rolling `archives/gmail-live.mbox` (registered as the `gmail-live`
virtual source, searchable like the archive). The same Core strata the
Takeout materialized (Sent ∪ Category Personal ∪ Starred) become
episodes automatically, deduped against the archive by Message-ID.
First run backfills the last 90 days (`GMAIL_IMAP_BACKFILL_DAYS` to
change), 2000 messages per cycle, remainder continues next cycle.
Without credentials the automation is a silent no-op.

### Agent surfaces — read-only by construction
Local MCP servers stopped being rare in 2026; the posture is the
differentiator now. Hearthloom's contract (`docs/agent-surface.md`):
**agents read; humans write** — enforced at read time by the vault and
scope guard, not advertised as policy. Contrast the field: Notion's
remote MCP offers agents write access "just like you can" by default;
screenpipe ships default-on telemetry under a local-first banner; Rewind
was remote-disabled on 2025-12-19 and took its users' recall with it
(Hearthloom *rescues* that data — see the adapter table). A record you
own can't be written to by an agent, silently phoned home, or shut off.

- **MCP** (streamable HTTP, stateless, `http://127.0.0.1:43278/mcp`):
  exactly four tools — `search`, `traverse`, `recall`, `propose_memory`.
  Proposed memories land in a quarantine queue, never directly in the graph.
- **Agent access card** (Integrations): active keys + scopes, friend-key
  reads, pending agent proposals — what touched your record, in one place.
- **HTTP API** under `/v1/…` (search, recall, entity, traverse, events,
  ingest, identity, scope, intents, consolidate, quarantine).
- **Change feed**: `hearthloom events --since <cursor>` over the append-only
  mutations log — the blackboard-readiness primitive.
- **Filesystem tier**: `~/.hearthloom/SKILL.md` teaches file-capable agents
  the query idioms; `raw/YYYY/MM/DD/` holds citable verbatim content.

## Performance

`hearthloom bench` measures P50/P95 wall-clock over the live HTTP surface
(the same one agents and the dashboard use), seeded with the record's own
top entities. On the reference substrate (97k episodes / 262k embedded
chunks, M-series laptop, vault locked):

| endpoint | p50 | p95 |
|---|---|---|
| traverse (depth 2, top 12) | 71ms | 369ms |
| search (hybrid FTS+vector) | 227ms | 517ms |
| timeline notable (year window) | 269ms | 390ms |
| recall (day reconstruction) | 426ms | 495ms |

Search rode the sqlite-vec ANN index in from 0.5.56: the brute-force
vector leg had degraded to p50 4.7s / p95 13.5s at this scale
(load-every-embedding-and-cosine); the `vec_chunks` KNN cutover took it
to a quarter second while keeping identical scope/vault semantics. This
table is the regression guard — re-run `hearthloom bench` after
anything that touches these paths. No LLM call is on any of them
(design law #3).

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│ INTERFACES   CLI · HTTP /v1 · MCP (4 tools) · BRAIN.md ·     │
│              SKILL.md · change feed (mutations cursor)       │
├──────────────────────────────────────────────────────────────┤
│ COGNITION    consolidation loop: embeddings backfill ·       │
│ (idle only)  intents · daily LLM digest → agent-trust episode│
├──────────────────────────────────────────────────────────────┤
│ GRAPH        entities + identifying attributes · bi-temporal │
│              edges · reversible merges · review queue ·      │
│              provenance → episodes · scopes everywhere       │
├──────────────────────────────────────────────────────────────┤
│ EPISODE LOG  append-only ground truth; raw content on disk;  │
│              trust tiers; tz-honest event times              │
├──────────────────────────────────────────────────────────────┤
│ INDEXES      FTS5 (BM25) + chunk embeddings (RRF fusion) —   │
│              all rebuildable from the episode log            │
├──────────────────────────────────────────────────────────────┤
│ PRIVACY      vault (private hidden until unlocked) · privacy │
│              rules · scope guard on every read surface       │
├──────────────────────────────────────────────────────────────┤
│ SOURCES      13 adapters (materialized) + virtual sources    │
│              (HearthFS, mbox archives, health vquery) +      │
│              inbox drop folder; only the daemon writes       │
└──────────────────────────────────────────────────────────────┘
```

The **daemon** (`hearthloom-daemon`, launchd agent `com.hearthloom.daemon`)
is the only process that opens the database. The CLI is an HTTP client that
auto-spawns it. Storage layout under `~/.hearthloom/`:

```
hearthloom.db     the substrate (WAL; schema migrations via PRAGMA user_version)
raw/YYYY/MM/DD/   verbatim episode content (never discarded)
inbox/            drop folder (processed/, failed/)
models/           local embedding model (optional; auto-detected)
config/           {env}.yaml with ${VAR:default} expansion
.env              secrets for the daemon (0600)
BRAIN.md SKILL.md daemon.log
```

## Install

```bash
./scripts/install.sh    # release build → ~/.local/bin + launchd agent
hearthloom init         # idempotent; creates ~/.hearthloom layout
```

Optional: drop `all-MiniLM-L6-v2.onnx` + `tokenizer.json` into
`~/.hearthloom/models/` for hybrid search; set an LLM provider (Anthropic
or OpenAI) with its API key under Integrations → AI provider — or
`ANTHROPIC_API_KEY=…` / `OPENAI_API_KEY=…` in `~/.hearthloom/.env` — for
digests, extraction, and Ask. Without either, everything still works
(FTS-only, no digests). `GMAIL_IMAP_USER`/`GMAIL_IMAP_PASSWORD` in the
same `.env` switch on the Gmail live pull (see the virtualized-sources
section). `scripts/bootstrap.sh` creates the admin login
(auth locks on the same gesture); `--remote` postures a public node.

## The CLI

`hearthloom` (installed to `~/.local/bin`, added to PATH by
`install.sh`) drives everything from the shell. It talks to the daemon's
HTTP API — the same surface the dashboard uses — and auto-spawns a local
daemon when none is running. `--url https://…` targets any node you own
(your always-on peer included; remote nodes are never auto-spawned), and
`--json` gives machine-readable output where it matters.

```bash
hearthloom auth login        # sign in once; a CLI key lands in the keychain
hearthloom status            # version · record size · vault · thinking · peer
hearthloom doctor            # diagnostics: orphan daemons, launchd target,
                             #   version skew, auth, model, WAL health, PATH
```

Capture (feeds the Desk):

```bash
hearthloom task add "call the accountant #today"
hearthloom note new --title "Reading list"     # $EDITOR flow
hearthloom note list
```

The record:

```bash
hearthloom search "consolidation freeze"       # hybrid FTS + vectors
hearthloom recall 2026-06-14                   # a day, with provenance
hearthloom query "what changed in my sleep since March?"   # the RLM (spends tokens)
hearthloom scope --by-source imessage personal # relabel scopes
hearthloom ingest git ~/myrepo                 # adapters; also mbox/ics/…
hearthloom quarantine list                     # the human gate for agent writes
```

Operations:

```bash
hearthloom key mint agent-x --scopes read      # PATs (secret prints once)
hearthloom vault unlock --minutes 15           # then: hearthloom export
hearthloom sync --now                          # force a peer cycle
hearthloom notify "backup finished"            # ring the dashboard bell
hearthloom logs -f                             # tail the daemon log
hearthloom completions zsh > ~/.zfunc/_hearthloom
```

Embers (from the registry, or your own):

```bash
hearthloom ember search presence               # discover (your configured registry)
hearthloom ember info hpresence                # versions, scopes, review status
hearthloom ember install hpresence             # verify sha256 + Ed25519, approve scopes
hearthloom ember update                        # verify + update anything outdated
hearthloom ember uninstall hpresence
hearthloom ember validate ./my-ember           # dev loop: the daemon's real parser
hearthloom ember pack ./my-ember               # validate + zip for publishing
hearthloom ember logs hpresence -f
```

`--registry <url>` points at any registry serving
`/.well-known/hearth-registry`; installs always show the requested
scopes and ask before granting (the dashboard flow's human gate,
preserved). Full command reference: `docs/cli.md` · ember developer
guide: `docs/plugin-guide.md`.

## Development

Rust workspace: `hearthloom-core` (schema/store/config/providers/embeddings),
`hearthloom-adapters`, `hearthloom-daemon` (axum + MCP + background loops),
`hearthloom-cli`. Checks: `cargo build --workspace && cargo clippy
--workspace --all-targets && cargo test --workspace`. Contributor and
agent rules: `CLAUDE.md`. Personal eval: `hearthloom eval` (see
`eval/README.md`).

## Roadmap

Live today: everything above — ingestion, identity, retrieval, the Ask
engine (multi-turn, charts, streamed answers, recipes), the Desk and
notes, peering between your own nodes, the plugin system and registry,
sqlite-vec search, live Gmail federation, scoped keys, sharing with other
people's records (defaults, overrides, item shares, groups), Spend with a
monthly budget, and compatibility with hosting provisioners (Hearthlaunch
is one).

Also live: several nodes per person (an always-on node relaying between
laptops and an always-on agent box, movable agents, a per-machine
thinking role) and reversible retirement of a memory or a whole source.

Next:
- Hearthloom as the memory provider for Hermes and OpenClaw; approvals
  from the phone; an internal agent.
- Per-node revoke on the Peering page; gating the extraction and
  classification passes on the thinking role.

## Configuration

Hearthloom is **self-hosted by default**: an unconfigured node talks to no
outside service. Optional settings are environment variables; put them in
`~/.hearthloom/.env` (one `KEY=value` per line). At its next start the
daemon seals that file into `~/.hearthloom/.env.sealed` (macOS keeps the key
in your login keychain) and merges it with anything already sealed, so
adding one line never loses another.

| Setting | What it does |
|---|---|
| `HEARTHLOOM_LLM`, `ANTHROPIC_API_KEY` | The model provider for idle-time passes and Ask (`none` by default). Per-tier models: `HEARTHLOOM_MODEL_LOW` / `_MEDIUM` / `HEARTHLOOM_MODEL` (high); blank uses the provider's defaults. |
| `HEARTHLOOM_HOST`, `HEARTHLOOM_PORT` | Where the daemon listens (`127.0.0.1:43278`). |
| `HEARTHLOOM_DASHBOARD_URL` | The address you open the dashboard at (`http://127.0.0.1:43278` by default; `scripts/install.sh` asks). A node without a login answers keyless calls only there and on loopback, so set it when you reach the dashboard by another name (a LAN host, say) — and set a password if that name is reachable from other machines. |
| `HEARTHLOOM_REGISTRY_URL` | An ember registry, also used to check for host updates. None by default. Hearthlaunch is one compatible registry. |
| `HEARTHLOOM_UPDATE_SIGNING_KEY` | A base64 Ed25519 public key; when set, self-update also requires a valid release signature. |
| `HEARTHLOOM_PROVISIONER_URL`, `HEARTHLOOM_PROVISIONER_SECRET` | Only for nodes run by a hosting provisioner (the control plane). |
| `GMAIL_IMAP_USER`, `GMAIL_IMAP_PASSWORD` | Live Gmail pull with an app password. |
| `HEARTHFS_BIN` | Where the HearthFS binary is, if not on the usual paths. |

**Eval suites** (`hearthloom eval`) are your own ground truth and live in
`~/.hearthloom/eval/`, not in this repository; see `eval/README.md`.

**Hosting a node** on Fly.io: copy `fly.example.toml` to `fly.toml`. To
publish an image for a provisioner, copy `deploy.env.example` to
`deploy.env` and run `scripts/build-tenant-image.sh`. Both copies are
gitignored. Before publishing a fork, `scripts/check-public.sh` scans for
secrets and personal data.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) 2026 Metavert LLC.

Bundled third-party files keep their own licenses: three.js and its
OrbitControls (MIT, Three.js Authors), the QR code generator
(qrcode-generator, MIT, Kazuhiko Arase) and the world land outline
(Natural Earth, public domain). The embedding model (all-MiniLM-L6-v2,
Apache-2.0) is downloaded at setup, not shipped in this repository.

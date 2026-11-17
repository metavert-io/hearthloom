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
- `trust`: `self` (the user's own words and activity), `imported`
  (other people's writing, such as inbound mail, message threads, web
  page titles, chat assistants' replies, calendar invitations, scans), or
  `agent` (something Hearthloom or an agent wrote). Since v61 a `self`
  claim from a source others write lands as `imported`
  (`store/provenance.rs::trust_for`, applied in `apply_batch`).
- **An origin** (v61, `episode_origin`): who first wrote it (an import,
  the user, an agent's key, the phone), on which of the user's nodes and
  when, who let it in when someone else proposed it, the agent task that
  produced it, and the route it took between the user's nodes to reach
  this one. Recorded on arrival, inside the ingest transaction; a synced
  episode's origin travels beside it on the episodes leg and each node
  adds itself to the route. Episodes from before v61 answer from the row
  itself: a local write names its writer, a synced one only the node
  that sent it. Shown as "How it got here" on every memory, by
  `hearthloom provenance <id>`, and to Ask through `read_episode`.
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
is derived from the ref on receipt, so it syncs with no wire change. Only
the record's own revisioned sources (photos, visits, contacts, trips) do
this for any writer; an ember's `#r<n>` (0.5.82) supersedes only what that
same ember wrote, judged by each episode's recorded writer (its origin),
so it too converges on every node and can never hide anyone else's
record. The growing sources (Chrome and iMessage days, Claude Code
sessions — `GROWING_SOURCES`, core `store/growing.rs`) name every copy,
the first read included, by how many items it holds (`<base>#r<n>`,
decided by the local import in daemon `local_import.rs` against the
fullest stored copy; each text carries its true count, so a size-capped
text still counts) — so two machines' first reads of one day are
different copies and both nodes end up with the fuller one. On receipt
the copy holding the most stays live — an unversioned base (an earlier
version's) counted from its text, so a smaller copy read on another
machine never hides a fuller one — whatever the order copies arrive in.
The copies of a day or session (and a Chrome day with the named day it
replaced) are one record: a new copy takes the record's label and its
retirement, a private label given to any copy, by any path, spreads to
the others with the same stamp — a copy private already takes the newer
stamp too, so the latest decision is what every copy is measured by (and
the scope leg takes it to every node whatever copies each holds) — and a
retirement takes every copy, and what any of them seeded, whichever
arrives first. A Chrome day keyed by its profile replaces a day earlier
versions stored under the profile's shown name only when it holds all of
it — at least its visit count, the visit it opens with, and every visit
line it shows except where the newer day leaves lines out (a long day
keeps its first and latest lines) and the older one does too (a day shown
whole must show in full) — so same-named profiles never displace each
other.

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

**Owner repairs** (schema v69) replace a memory an earlier version stored
wrongly — mail left encoded, a calendar event that took in its alarm's
text — which importing again can't fix (the ref keeps its first text; a
retired ref never lands again). The owner runs `hearthloom repair mail` or
`hearthloom repair calendar --file …` (a dry run unless `--apply`); the
daemon reads the original again with the current importer (mail from the
node's own archives: every archived copy under the Message-ID, the one
outside Spam and Trash with the memory's date and sender, left alone when
such copies disagree or while any archive can't be read; calendar events
only where the .ics importer stored them, never the phone's live feed, and
an event stored under an earlier ref only when its Event entity holds the
UID — a UID that merely begins the same way is another event) and the
corrected copy lands as
`<base>#repair:<hash>`, written as `repair:owner`. Whether an episode
replaces another is decided by that recorded writer, never by its ref —
mail and calendar refs are chosen by senders — and only the repair itself
may write as it, so a look-alike ref from anyone else replaces nothing, on
any node. Each node settles a repaired record (the memory and its
corrected copies) from what it holds: the newest copy (by when the
newest run that made it began, which every node knows alike) whose run
wasn't undone stands in view, the rest behind it; copies share the
record's label (a private one spreads — also to corrected copies whose
original never reached the node, which keep the message private there; one
the owner takes out of the vault comes out for the whole record), and its
retirement (also on a node that held a copy from before it knew the rule,
once the run reaches it). A run is a synced row carrying the copies it
made (`repair_copies`, v71, a set every node unions — two nodes can make
the same copy under runs of their own; a row sent again carries only the
copies recorded since the peer's cursor);
undoing it brings the original back in view on every node, hides the
copies it made even where the original hasn't arrived yet, and can't
itself be taken back. Every node publishes `roles.repairs` in
the node list; applying is refused while a node seen in the last 30 days
lacks it (it would show the corrected copy beside the original), unless
the owner says otherwise. The same
mechanism repairs ChatGPT conversations (`hearthloom repair chatgpt
--export …`: the branch each ended on, replacing only a transcript with
abandoned turns mixed in whose every turn the export holds; "do not
remember" into the vault before any copy lands) and Apple Health days and
workouts (`repair health --export …`; a day only when the export holds
every tracker and measure it shows — a sleep stage of a tracker whose
night isn't shown counts as held). Sources
with a revision rule of their own are re-minted under it instead of
getting a `#repair:` copy, still written as `repair:owner` under the run:
`repair copies` re-mints growing sources' days and sessions stored under
an unversioned ref from their stored text (each node re-mints its own
when the run reaches it, and the growing rule shows the fullest copy
everywhere), and `repair photo-days` re-mints a photo day whose count is
inflated by duplicate photos under its next revision when the photos held
here cover every one it shows, its ledger set with it. Every node checks
that coverage for itself: the correction stands on another node only over
the copies of the day it holds there (by that node's photos, whenever
they arrive), takes the day's private label there, and a node's finalize
pass mints past every revision it stores — past another node's only when
its photos hold every one that copy shows. Undoing a re-mint
puts the copy that stood before back in view (a photo day's ledger at the
run's revision, so the next finalize mint goes past it), a copy of an
undone run that arrives later stays behind — behind the original's id
when the original hasn't arrived yet — and asking again doesn't make
an undone correction again. A later read of the owner's files that puts
an undone `copies` copy back in view is recorded (`repair_revived`, v72)
and travels with the run, so every node puts it back (one that holds the
copy but not yet the original too). See
`store/repair.rs` (the rule) and daemon `repair.rs` (the scans).

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
- `owned_sources` (v68): the parts of sources others can write that the
  user says are theirs (see "Trust in content")

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
- Device feeds: `health_sample`, `photo_point`, `photo_day` and
  `photo_replaced` (the keys earlier scans gave photos, never stored
  again), `location_observation` and `visit_day`, `device_state`,
  `movement_observation` and `trip_day`. Raw samples land here and a
  finalize pass turns each closed day into episodes.
- The movement layer (v60): `movement_observation` holds spans of
  travel with a `subject` — `me` (the phone's motion segments) or
  `vehicle:<ember>:<id>` (a car's own drives, posted by an ember). A
  car's drive is the car's until the trips pass decides the person was
  in it: their phone in a vehicle at the same time makes it theirs
  (`trip`, absorbing the phone's own trip episode); their phone on foot
  or a dwell far away makes it someone else's (`vehicle_drive`, filed
  under the car's scope for others, never on the person's map). The car
  exposes no driver identity, so `vehicle_profile` (per-car settings:
  decide by phone / always me / never me, who drives when you're
  aboard, household drivers) and `trip_attribution` (a yes/no on Tend
  when the evidence can't tell) fill the gap. Trip refs carry a
  fingerprint of the day's output, so both nodes mint the same episodes,
  and the pass re-asserts its set if a peer's stale revision lands.
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
   (zips, tgz, mbox, ics, conversation exports) are recognised and
   ingested. Archives unpack in-process, plain files and folders only,
   within what the disk can spare; a recognised export is staged into
   `archives/<key>` for its run-now import, never while that import runs.
   Very large mail archives are refused there and go through the
   filtered pipeline.
3. **Live device feeds.** The iOS app posts to `/v1/ingest/*`: health,
   photos, contacts, calendar, reminders, location, motion, media, scans
   and focus state.
4. **Live pulls.** `gmail_pull` fetches new mail over IMAP into a rolling
   archive. Since 0.5.81 the live watch (`gmail_watch.rs`) keeps an IMAP
   IDLE connection open on the node where Gmail is connected and runs that
   same pull within seconds of new mail, with a catch-up every few
   minutes; the consolidation pass stays as its backstop. Since 0.5.98 a
   node can watch several mailboxes (`mailboxes.rs`): each has its own
   IDLE connection, pull cursor, live archive, pull lock and settings, and
   a refused password stops only that mailbox. Every Gmail connection has
   a deadline on each step, so a stalled one fails instead of holding its
   mailbox. A pull also re-reads the labels of the mail it already holds
   (the newest every time, the rest of its live archive in turn), so a
   message starred later becomes a memory, reaches the Desk and the
   embers' watches (as far as its thread's privacy and class allow), and
   shows its labels as they are now in search. The labels are recorded
   together with the list of messages still to be shown to triage and the
   watches, so work a stopped pull left undone is finished by the next
   pull, before it connects; a list or labels that can't be read stay for the next try,
   never read as done, and an index rebuild that can't read the old index
   leaves it in place (the pull's cursor and the re-read's place in the
   archive likewise: an index that can't be read is an error, never
   "nothing recorded"). A message Gmail can't answer about is left out of
   a re-read, not the rest of its batch, and the mail still to be shown is
   shown even when Gmail can't be reached; a message that joins a thread
   later but was written earlier leaves the thread's latest message — what
   triage reads — as it is (a latest dated in the future isn't believed: a
   sender chooses the date). A message's privacy is read again after its memory
   is made, since the memory can arrive private; a message found private
   then makes the thread it joined private, so no model sorts it — a label
   re-read does so straight from the record, before it reads the index
   again, so a sorted thread doesn't stay in view when that read fails.
   Triage also looks each thread's memories up again just before a model
   reads it, by every key the thread index kept for them (v70), so a
   memory that turned private some other way leaves no private mail to
   sort; a message recorded before v70, or one another node recorded in a
   thread that reached this node (the thread index is each node's own),
   has those keys read from its live archive here, and a thread whose
   keys can't be read there waits — not sorted, its label left as it is —
   until they can be (a node that took over another node's mailbox holds
   none of its archive); a thread found waiting is passed over unread until
   it or the live archives here change, or ten minutes pass. While a
   private promotion an earlier version kept under a Gmail uid is in the
   vault, no message can be told apart from it: mail is held (readers
   count every message private; a viewer whose vault is locked sees no
   mail thread, draft or ember's queued message at all, though no thread
   is relabelled; nothing records a thread as private on that account —
   the pull, triage, embers' queues, draft requests and the backfill
   wait), the log names
   that memory, and so does the triage status for an owner whose vault is
   open. Only a promotion a person may have made holds mail — not a
   memory an importer, someone else's key or one of the owner's own
   devices wrote; one whose writer isn't
   known does — whatever its source is registered as now, or whether it
   is registered at all.
   Mail from any of the owner's mailboxes, or an alias listed on one, is
   the owner's own.

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

**Mail threads (v63, `store/mail.rs`, `mail_triage.rs`).** The action
half of mail: each message the Gmail pull fetches is observed into a
thread (by its headers; the id hashes the root message id), owner rules
decide first, and a tool-less model sorts what is new (needs a reply, a
time, a look; fyi; ignore), quoting the words that decided it. Threads
and rules mirror; the Desk shows what waits on the owner.

**Embers watching mail (v64, `store/mail_watch.rs`, `mail_embers.rs`).**
An ember granted `mail.watch` registers watches (sender, domain, words,
labels, triage class). The node that watches Gmail checks each new
message against them, and each triage label too, and queues matches per
ember — only for mailboxes the owner lets embers see (an added mailbox
starts hidden from them). The queue syncs on its own leg, so the ember reads it, with a
cursor, on whichever node it runs. The owner's view for that ember
(headers, a snippet, or the text) and a fresh privacy check are applied
when it reads; the check finds the message's memory under any key the
mail import can have given it. Watches and views mirror like mail rules.

**Reply drafts (v65, `store/mail_draft.rs`, `mail_drafts.rs`).** A
request (the owner's words, or an ember's finished text) is a mirrored
row that remembers its mailbox. The node that handles that mailbox
(`mailboxes::duty`: of the live nodes that watch it, the one with the
smallest id) takes it, reads the thread from its live archive, has a
tool-less model write the reply, and APPENDs it to the folder Gmail marks
`\Drafts` with the `\Draft` flag in the mailbox the person wrote to,
from the address they wrote to when it's one of the owner's
(`gmail_imap::save_draft`; the message itself from `mail_compose`, 7-bit).
Nothing is sent or changed. Triage works the same way: each thread is
sorted once, on the node that handles its mailbox.

**Google Calendar free/busy (`google_calendar.rs`).** A PKCE sign-in with
the owner's own Desktop-app client (`calendar setup`) or, when a registry
is configured, Hearthloom's shared one (published at the registry's
`/.well-known/hearthloom-oauth`). On a local node a loopback listener
(ephemeral port, only while a sign-in is pending) finishes it when the
browser is on the same machine; elsewhere the landing address is pasted.
A hosted node signs in with Hearthlaunch's Web client: Google returns to
Hearthlaunch, which checks the state the node signed with its provisioner
secret and forwards code + state to the node's own
`/oauth/google/callback`; only the node holds the PKCE verifier and the
tokens. The refresh token is sealed with the client that made it. Free
windows = working hours (node settings) minus free/busy blocks; drafts
about a time get them in the prompt (in the other person's zone too, from
their Date header, and the owner's scheduling link). Embers get free
windows only, through `calendar.availability`.

A mail archive's sidecar indexes the headers when it is built, and its
message text and links in the background afterwards (`mbox_text.rs`; big
archives wait for the owner). It keys each message by the mail import's
key for its Message-ID, and a rebuild and the writes it carries over
(labels re-read, the pull's cursor) take turns on one lock per archive.
The mail imports read the privacy rules over each whole message too
(`mbox_text::rules_match_message`), so a memory, whose stored text is
cut, is labelled as its archived message reads. A copy stored before
under the same Message-ID keeps its own label: what the rule matched can
lie where its stored text doesn't reach, and other mail written under
that Message-ID (the sender's choice) reads the same there. Only an
import asked to be private moves such a copy into the vault, when it has
the same date, sender and text (a copy an earlier version left encoded
counts through its repair's corrected copy). A link target the cut leaves out keeps its
host in the stored text, so a domain rule made later still finds the
memory; a message with more hosts than their room holds is stored
private. Archives are read in place, so every read
goes through `virtual_source::{search,fetch}_for_viewer`, which applies
the privacy rules and the caller's vault — the rules to the whole message,
decoded, and the vault to the message's memory under any path that
reaches it (a promoted message is stored under its Message-ID, looked up
under every source the record holds, registered or not, and matched by
the message's identity rather than how its Message-ID header is written;
one an older build kept under an archive's `at:` path is known by the
Message-ID its stored text names, and while one kept under a Gmail `uid:`
path — which can't be identified — is private, every message counts as
private). Each node lists its archives in
the node list, and Ask on any node can search an archive another node
holds (see "Lookups on another node" below). Where Gmail is connected, the `gmail`
source (`gmail_search.rs`) searches the whole mailbox with Gmail's own
search, read-only — every mailbox connected there at once, newest first —
so no archive file has to travel for Ask to reach old mail. An archive file itself has a home: the node that keeps
archives (`archives_place.rs`) — always on, chosen from the synced node
list — brings it in through the hub in checked, resumable chunks, and
the node it came from stops offering its copy (deleting it is the
owner's call).

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
activity, Gmail pull, the day-finalize passes (photos, health, trip
reconcile, visits — trips first, since your drives' endpoints feed
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
lines (self-model, what each source holds, top entities, recent activity,
a schema cheat sheet) that agents load at session start. "## Sources"
(0.5.82) gives each source's live, non-private episode count, its kinds
and its date span (`Store::source_shares` + `source_lines`; names printed
only when they're plain identifiers). Ask gets the same per-source lines,
in the asker's own vault view, as its episode inventory — every source up
to 60, so a small source on a large record is never left out.

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
  filtered temporary views and by SQLite's authorizer, which holds every
  table a query reads to its class in `store/raw_sql.rs` (filtered, open,
  vault-only, never) and refuses writes, transactions, ATTACH, and any
  pragma or function not listed there.

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
  DNS rebinding finds nothing. The one exception is the sign-in, which
  accepts a hosting provisioner's issuer token from another site (its
  "Open" button is a form POST from its own site); a password from
  another site is still refused.
- **Provisioner control plane**: an HMAC-authenticated channel above all
  tokens, used by the hosting service, with a killswitch. Export and
  vault access always survive the killswitch so a user can take their
  data.

### Trust in content

Agent-proposed memories go to `quarantine` and wait for the user; once
let in, their origin names the agent that proposed them, the user who
accepted, and the agent's task. An agent task records everything outside
the user's words it read (`agent_tasks.inputs`) and everything it wrote
(`outputs`); the first outside read taints it, and every later action
waits for the user (the taint rule). A task on a harness Hearthloom can't
see inside (Hermes, OpenClaw) is tainted from the moment it runs.

What counts as the user's own words is decided when a check reads, not
when the text lands: `trust = 'self'` from a source the user writes. A
source others can write (`OTHERS_WORDS_SOURCES`: message threads, web
titles, assistants' replies, invitations, scans, and also git histories
and Substack publications) is outside text however it was stored (and
is stored `imported`: on arrival, and for rows stored before by the v61
and v73 relabels) — with
one owner setting on top, "this source is mine" (v68,
`store/owned.rs`). The user lists a repository (by its remote address)
or a publication (by its host); within a listed repository, commits whose
author address is one of the user's own (every connected mailbox and its
aliases on any node, and the addresses on the user's profile card) count
as the user's words — except merges, commits co-written with anyone else
(a `Co-authored-by:` trailer, which Claude Code adds) and commits with
a run of one of the user's agent tasks anywhere between their author
date and their commit time (a rewrite keeps the author date and the
last one sets the commit time, so an agent's rewrite in between leaves
no time of its own).
The git importer records each commit's parents and commit time for this;
a commit an earlier version imported has neither and stays outside text,
as does one at a shallow clone's cut-off, whose parents git doesn't show
(recorded as not fetched). An agent task never closed counts as running
for at most the longest run can last after its latest stamp when the run
started in Claude Code, which Hearthloom stops at its time limit (each
task keeps the harness its run started on; the agent's setting can
change while a job is out); a task sent to a harness that runs on its
own (Hermes, OpenClaw), or whose harness wasn't kept, counts as running
until it is closed. The idle
refresh leaves out a checkout that overlaps an agent's working folder.
Git doesn't check an author address, so a listed repository is only as
trustworthy as who can get commits into it. A listed publication counts
whole, guest posts included; the Substack importer files a post only
under the host that served it and records that host as the post's first
line — a post an earlier version imported, without that line, stays
outside text. The list is owner-only, needs the vault open to add to, is
mirrored and journaled between the user's nodes, and is applied in
`first_untrusted_episode` and `entity_names_unverified` — the two checks
behind Ask's outbound gate, traverse names and the agents' taint rule.
While a repository is listed, adding an address of the user's (on the
profile card, as a mailbox's alias, or by connecting a mailbox) needs the
vault open as well, and address changes appear in the list's history.
Stored trust never changes, and every doubt (an unknown writer, a writer
other than the source's own importer — an ember's write never passes for
one —, an unreadable author line, no known address of the user's) keeps
the text outside. Sources whose items mix authors or carry fetched
content (iMessage, Chrome, Claude Code, chat exports, invitations, scans,
media) can't be listed.

Promoted content from untrusted origins is quarantined first.
Credential-shaped tokens are redacted from everything Ask shows a model.
Records keep every character as it arrived; text going to a model
(every prompt, agent briefs, MCP results) drops the characters a reader
can't see (`hearthloom_core::unseen`), and approval cards show them as
marks. Other people's words in a prompt sit inside a tag that
`fence::outside` keeps them from closing.

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
- **Provenance rides the episodes leg**: each episode's origin is sent
  beside it (a pull page's `origins`, a push body's `origins`), only a
  `peer` key's are believed, and each node adds itself to the route.
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

### Lookups on another node

Spokes dial the hub; nothing dials a spoke. When Ask needs an archive
another node holds, the lookup goes through the hub (`lookups.rs`): the
asking node posts it (`/v1/sync/lookup`, peer keys only), the hub hints
its spokes over the push bus (`lookup.requested`, repeated while
unclaimed), the holder fetches what is for it, runs it under its own
privacy rules with the asker's vault state, and posts the answer, which
the hub hands back. Requests live in the hub's memory for seconds; a node
that is asleep doesn't answer and Ask says so.

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
  One handshake serves every ember: the node's `/assets/ember.js` in the
  page and `/assets/ember-shell.js` in the dashboard, which hands the
  page its token once per load; ember pages open only inside the
  dashboard. A registry distributes signed packages. On a Mac launchd runs their
  programs; elsewhere (hosted nodes) the daemon supervises them. An
  ember can publish files under `/.well-known/`, keep sealed secrets
  (never exported or mirrored), and borrow the node's address for an
  OAuth sign-in (`/plugins/<name>/oauth/callback`). Hosted nodes
  install only registry-verified packages.
- **Notifications**: a queue of hints with a link allowlist, exchanged
  both ways between peers.
- **Full export**: one zip that can rebuild a node (database snapshot,
  readable tables, files). It requires the vault unlocked, and restore
  applies only to an empty node and writes only what an export holds (no
  settings files, keys or database files). On a hosted node a restore
  brings no ember code: the restored embers are taken off until installed
  from the registry again, with their data kept.

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
- A ref's shape never decides supersession for a source whose refs come
  from outside: only the record's own revisioned sources, an ember's own
  writing, and the owner's repairs supersede — the last two judged by the
  recorded writer.

**Privacy**
- Every new read query carries the scope guard.
- Whose words a text is gets decided at check time, through one judge
  (`OwnedWords`) — never by rewriting stored trust.
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

- An owner repair re-derives a memory's entity links from the corrected
  text and carries over only the links the extraction pass found; the
  edges the old text asserted keep their support from the replaced copy
  (as with every superseded episode), so a leaked calendar attendee's
  `attended` edge stays until something retracts it. Repairs reach mail
  stored under `mbox` and events the `.ics` importer stored; a promoted
  message isn't covered, and an event the live calendar feed stored is
  left as it is (`other_writer`), as is a memory whose original the node
  didn't record the writer of (synced before provenance). A mail memory
  whose Message-ID names archived messages that read differently is left
  (`ambiguous`).
- `repair copies` can't settle two nodes' copies of one day that hold the
  same count with different texts (the growing rule names copies by their
  count). `repair photo-days` sets the minted ledger only on the node it
  runs on; another node keeps its own until the repair runs there too
  (the re-minted day reaches it either way). It checks coverage against
  the photos stored on its own node: a node holding photos not yet synced
  there shows the re-minted day until they arrive and the finalize pass
  mints the next revision (as with any finalize revision). A re-minted
  copy keeps the links of the text it was made from, not its edges.
- `repair chatgpt` and `repair health` can only tell an older export by
  what it lacks: a ChatGPT transcript is left when it holds a turn the
  export doesn't, a health day when it names a tracker or a kind of
  measure the export doesn't hold for it — fewer samples from the same
  tracker can't be seen. Give the newest export.
- Retirement can't trace everything a memory touched: entity attributes
  (an email address it introduced) don't record their source, nor do
  relationship briefs and topic pages; digests and weekly reviews written
  before v59 are matched to a memory by time window, not by a recorded
  read. The preview says so each time.

- Derived episodes (digests, chapters, self-model propositions) are
  minted with trust `agent` regardless of what they summarised. Nothing
  carries the lowest input trust forward (their recorded inputs are shown
  under "How it got here").
- Git commits stay `self` whoever wrote them: telling the user's commits
  from a collaborator's needs an author check against the claimed
  identities. Episodes that reached a node by sync before v61 have no
  origin beyond the node that sent them; the origin node knows, but no
  leg back-fills it.
- Ask's conversation is one growing string, so only the system prompt
  caches.
- The Ask eval's retraction cases are templates until the owner supplies
  ground truth.
- A local distilled extractor for the sources kept off the cloud is
  scoped in the design log and not started.
- Federation's versioning and manifests are designed and deferred until
  federation grows beyond the current friend surface.

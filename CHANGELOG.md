# Changelog

Versioning starts here (2026-07-16). Everything before today shipped as
`0.1.0` in nine days of continuous deployment; the 0.1.0 section below is
a coarse retrospective, not a release ledger — the design log
(newest last) is the fine-grained history. Tenant/cloud deploys should
pin release tags, never a branch.

## Unreleased

### Fixed
- The paired phone can set up Quick Capture again: its key may mint a
  SCOPED key (grants, no scopes — always narrower than itself), such as
  the capture key, which 0.5.71's admin-only key management refused. Every
  other key management still needs an admin key or a signed-in session.

## 0.5.71 — 2026-10-24 — first public release

### Security
- **Keys reach only their scope.** A key that isn't the owner's (an admin
  key or the paired phone) reaches only the routes its scopes are listed
  for: `read` the record and MCP, `write` the Desk, `ingest` device feeds
  and proposals, `notify` the bell, `peer` the sync legs. Before, most
  routes checked no scope, and any key — a `notify` key included — could
  mint itself an admin key, install a plugin, start an export, change
  privacy labels or connect a friend.
- **Web pages can't drive a node.** Cross-origin access is granted only
  to requests carrying a key; a keyless request from a page on another
  site is refused, and a node with no login answers keyless calls only at
  its dashboard address (DNS rebinding) — loopback, or the address set in
  the new `HEARTHLOOM_DASHBOARD_URL`, which `scripts/install.sh` now asks
  for. Before, any page a user visited could read and write a node that
  had no login yet.
- **The vault is per viewer.** An unlock belongs to the browser session
  or key that made it; everyone else keeps the locked view, and scoped
  keys are no longer refused while someone else's vault is open. A viewer
  whose vault is locked can't take anything out of it: before, a relabel
  by source could expose a whole source's private records.
- **Proposals can't choose how they land.** Accepting one kept the source,
  kind and trust the proposer wrote; it now lands as an agent-trust
  memory (a registered virtual source's promotion keeps its origin), and
  Tend shows how. Keys other than the owner's and peers' can't ingest
  under the record's own sources or claim the owner's trust or name.
- **Ask:** chart values are escaped (a value echoed from mail or a web
  page reached the page as markup); the system prompt no longer carries
  third-party titles, names or the digest; an acting reply is taken only
  from a reply that is one JSON object; `traverse` taints; every spelling
  of a fence tag inside a result is defused.
- **Agents:** after a task has read content you didn't write, every action
  asks first, whatever the preset.
- **Dashboard:** a Content-Security-Policy (scripts only from the
  dashboard itself; no inline handlers left), `nosniff` and a same-origin
  referrer policy; imported note media (an SVG) is served sandboxed.
- **Mail:** a sender can't forge Gmail labels (live pulls always carry our
  own label header; the archive index keeps the first header).
- **Location and parsers:** map points come only from photo days,
  Facebook posts and a visit's own line, and only from whole lines that
  end with the coordinates — a Messenger line, an event description or a
  calendar location can no longer put a point on your map (points already
  indexed are re-read once and re-derived). Calendar titles, places,
  photo names, Facebook place names and chapter titles are written on one
  line. Workout routes are read only from workouts and only inside the
  archive. Chapter provenance comes from the recorded inputs.
- **Visits:** a calendar event alone is no longer a visit by default; it
  needs your phone or a photo to confirm you were there (one geotagged
  photo is enough). Integrations → Pair a device → "Count a calendar event
  on its own as a visit" turns calendar-only visits back on (it syncs
  between your nodes and applies to days clustered from then on;
  scriptable as `POST /v1/settings/automations {"key": "visits_finalize",
  "calendar_alone": true}`). With it off, visits made from a calendar
  event alone before this release are withdrawn once, on every node: they
  stay in the record as replaced but leave the map and every live view;
  turning the setting on later doesn't bring them back.
- **Notes** record who wrote them: agent-written and imported notes settle
  with their own trust and create no new people, and a friend's forked
  note is the network's. Only ☆-saved questions are Ask recipes — save
  older recipes again. A scoped key minted for one of your own devices
  (the phone's lock-screen capture, a Shortcut) can be marked "My own
  device": its captures and notes count as your own words.

### Changed — self-hosted by default
- A node talks to no outside service unless its owner configures one: the
  ember registry and host-update channel (`HEARTHLOOM_REGISTRY_URL`) and
  the hosting provisioner (`HEARTHLOOM_PROVISIONER_URL`) have no built-in
  defaults. Hearthlaunch is one compatible registry and provisioner. The
  CLI's `--registry` reads the same setting.
- No Claude-specific default model: a blank `HEARTHLOOM_MODEL` lets each
  provider's own tier table decide.
- The relationship classifier names the record's own owner (the identity
  the record knows as "you"), never a fixed name.

### Added — your own eval suites, outside the repository
- `hearthloom eval` reads `~/.hearthloom/eval/<suite>.yaml`
  (`personal-eval` by default, `ask-eval` with `--ask`, any name with
  `--suite`); the first run and `hearthloom init` copy invented examples
  there. Personal-eval cases can be `skip:` templates.
- README → Configuration lists every optional setting.
- `scripts/check-public.sh` scans for secrets and personal data before a
  fork is published; deploy settings come from a gitignored `deploy.env`
  (`deploy.env.example`), and `fly.example.toml` replaces `fly.toml`.

### Fixed
- Self-update refuses a release that publishes no sha256 (it used to skip
  the check), verifies an Ed25519 release signature when
  `HEARTHLOOM_UPDATE_SIGNING_KEY` pins a key, and the dashboard says which
  check it makes.
- A plaintext `~/.hearthloom/.env` written next to an existing
  `.env.sealed` is now merged into it at start; before, it replaced the
  sealed file and every other secret was lost (re-running
  `bootstrap.sh --remote` did this).
- `install.sh` asks before editing your shell profile.

## 0.5.70 — 2026-10-17

Everything in 0.5.69 (which was tagged but never deployed), plus:

### Fixed — a sync loop between the laptop and the always-on node
- Since 0.5.68 a node that pulls the always-on node's tasks, notes and
  goals re-applied every row on every pull and reported thousands of
  changes, which scheduled the next sync ten seconds later — a sync
  every 10–14 seconds, rewriting ~3,000 unchanged rows each time. Rows
  identical to the local copy are now skipped and not counted, so a
  quiet record syncs on the five-minute tick again.

## 0.5.69 — 2026-10-17 (tagged, not deployed)

Schema **v59** (additive: `retirements`, `retirement_rows`,
`summary_inputs`, `entities.hidden_by`, `sources.retired_by`, an index on
edge provenance by episode). A peer still on 0.5.68 ignores the new
retirements leg; it catches up once it updates.

### Added — retire a memory, or a whole source, and undo it
- Any memory can be retired from its expanded card (Day view, Memories),
  and a whole source from Memories → Sources ("retire…"). A preview comes
  first: what the memory fed, in counts (links to people and things,
  connections that lose their support or are withdrawn, goal matches,
  opportunities, map points, galaxy pairings); the briefs and
  opportunities written from it; the people and organisations that
  existed only because of it (hidden, never deleted); the digests,
  weekly reviews and chapter headings that read it, ticked to be
  rewritten (untick any to keep); the about-me beliefs that go back to
  Tend; and what can't be traced. One confirm, one undo.
- A retired memory leaves the whole record — search, Ask, the timeline,
  the galaxy, Relationships, BRAIN.md, the /brain surface and what
  friends can see. Nothing is deleted: everything it changed is recorded
  with its prior state, and Undo (in the toast, on the card, or in
  Memories → Retired) puts all of it back.
- Retirements travel to your other nodes; each node applies one itself.
  Memories that arrive later from a retired source, a retired memory
  that reaches a node late, a fuller revision of a retired photos day,
  and briefs seeded by a retired memory all land retired.
- Summaries now record exactly what they read (digests, weekly reviews,
  chapter headings), so later retirements find them exactly. Ticked
  summaries are rewritten without the retired memory at the next idle
  cycle on the node that does the thinking, at the same place on the
  timeline.
- Tend says why a confirmed belief is back ("back to check after you
  retired …").

### Changed — replaced and retired memories stay out of every reader
- Older versions that a fuller one replaced (photos days, visits,
  contact cards) no longer count in dossier totals, chapter and review
  inputs, cascades, goal and opportunity scans, galaxy pairings, stats
  or the timeline. A test now fails the build when a new query that
  reads the record ignores replaced or retired memories.
- Memories → Episodes hides retired and replaced rows by default; tick
  "retired & replaced" to see them, labelled.
- Ask's raw-SQL handle no longer sees retired or replaced memories even
  with the vault open.
- The Memories filter row wraps on a phone, and wide tables scroll
  inside themselves instead of widening the page.

## 0.5.68 — 2026-10-17

Schema **v58** (additive: `agent_directory` v56, relay stamps `synced_at`
on the timestamp-synced tables v57, `nodes` v58). Peers on 0.5.67 stay
wire-compatible; until both nodes run 0.5.68, nothing new crosses.

### Added — all your machines on one page, and who does the thinking
- ☰ → Peering lists every machine running Hearthloom for you (name,
  cloud or computer, version, how many agents, when it was last seen),
  synced through the always-on node.
- Each machine has a thinking setting — "does the thinking" (unchanged
  default), "leaves the thinking to others", or "takes turns": among the
  machines set to take turns, one live one runs the paid idle passes, and
  another takes over if it goes quiet for 20 minutes. The page warns when
  several machines all pay for the same thinking.
- A task waiting for an agent on a machine that's offline says so.

### Changed — your edits reach every node through the always-on one
- A node that syncs with the always-on node now also receives what your
  OTHER machines sent it: their task and note edits (the change journal
  no longer drops everything another node sent), and the always-on
  node's tasks, notes and goals (pulled back after each push and merged
  — newest edit wins). With a laptop and a Mac mini both paired to the
  cloud node, each sees the other's work.

### Changed — sync can now relay through a hub (schema v57)
- Data synced between nodes by timestamp (the usage ledger,
  notifications, extraction results, logs, sharing settings, plugin data,
  privacy relabels, contacts, locations, agent tasks and approvals) now
  pages by when THIS node last wrote or received each row. Before, a row
  passed on by a middle node kept its original time and a third node
  that had already synced past that time never got it. Groundwork for
  more than two nodes; nothing changes with two.

### Added — agents can move between your machines (schema v56)
- Each machine has a name (☰ → Agents → rename). Agents on your other
  machines are listed there too, and Ask can hand work to them; the task
  card says where the agent runs.
- **Move an agent** to your paired machine, or bring one here: its
  settings and saved tokens travel sealed so only the receiving machine
  can open them, it gets its own read-only key there (same life areas),
  tasks waiting for it go with it, and the old machine forgets it. Start
  on your laptop; move to an always-on machine later.

## 0.5.67 — 2026-10-17

Schema **v55** (additive: `agents`, `governance`, `agent_tasks`,
`agent_approvals`, `ask_log.tainted`).

### Added — Ask can now act for you, through agents you set up
- **Ask tells "find out" from "do it".** Ask a question and it answers;
  ask for something to be done ("email Ada the summary", "tidy my
  downloads folder") and it gathers what's needed and hands a
  self-contained task to one of your agents. The task appears under the
  answer as a card: waiting for your OK → running → done, with Approve,
  Don't and Stop.
- **☰ → Agents** (seeing or changing anything needs the vault unlocked;
  approving what's waiting does not). Add an agent: **Claude Code** on
  this Mac (full control — every action it wants comes back to Hearthloom
  for a decision), **Hermes** (partial — the task, plus the commands Hermes
  flags as dangerous, through an approval-hook script the page generates),
  or **OpenClaw** (sends only — after that OpenClaw's own settings apply).
  Each gets its own read-only Hearthloom key limited to the life areas you
  pick, and its token is sealed.
- **Rules.** For each kind of action — look things up, add to Hearthloom,
  send messages, calendar, spend money, change files, use websites,
  friends' Hearthlooms, anything else — "do it", "ask me first" or
  "never", for all agents and per agent. Everything starts on "ask me
  first"; presets Cautious / Assistant / Delegate. **After a task has
  read things you didn't write** (email, web pages, imports), sending,
  buying, file changes and websites always ask — overridable, with a
  warning. Limits on tasks per day and actions per task.
- No agent set up? The card says what kind of agent would fit, which
  preset, and how to add it.
- Everything is recorded: each dispatch and each governed action is an
  approval row; the outcome lands in your record as an `agent_task`
  memory; the bell announces approvals and finished tasks. Policy, tasks
  and approvals sync between your nodes (agent setups stay on their own
  machine).

### Security — Ask's guard on friends' Hearthlooms, tightened
- What a conversation has read now carries across its turns; raw queries
  and note reads count as reading content you may not have written; the
  check refuses rather than allows when it can't tell; and the newest
  memory titles and the daily digest no longer sit in Ask's instructions
  (a stranger's email subject could steer Ask from there).

### Fixed
- Hearthloom's MCP server answered every client with its newest protocol
  version; clients that don't know it (Claude Code 2.1) dropped the
  server. It now answers with the version the client asks for.

## 0.5.66 — 2026-10-08

No schema change (v54).

### Security — a sender could hide a real email or calendar event
- A record whose id ends in `#r2`, `#r3`… marks earlier versions of the
  same id as replaced — how photo days, visits and contact cards are
  updated. That rule ran for every source, and a mail's id is its
  Message-ID and an event's is its UID, both chosen by the sender. So a
  mail with `Message-ID: <known-id#r2>` hid the real message `known-id`
  from search, recall and Ask (it stayed in Memories). The rule now
  applies only to the three sources whose ids Hearthloom mints itself.
  Checked on the laptop record: no mail or calendar item was ever hidden
  this way (the only replaced items are photo days, one contact, one
  visit; private items were not visible to the check).

### Fixed — the top bar spilled past the window on medium-width screens
- Between roughly 680px and 1300px wide the navigation bar was wider
  than the window on every page (the page scrolled sideways and the
  right-hand buttons fell off). The bar now tightens its spacing first,
  becomes the ☰ drawer below 1140px (it used to wait until 680px), and on
  the smallest phones the mark stands in for the name. Measured with no
  overflow at every width from 320px to 1600px.

## 0.5.65 — 2026-10-07

No schema change (v54).

### Security — only the owner can unlock the vault
- Unlocking the vault, and trading a key for a signed-in session (which
  can do everything, the vault included), now need the owner: a signed-in
  session, an admin key, or a paired device's key (the phone — its key
  holds `read` + `ingest` + `notify`). Any other key is refused with 403:
  `read` alone, `read`+`write`, `ingest`, `notify`, `peer`, `mcp`, plugin
  and scoped keys. Before this, any ordinary key could unlock the vault.
- Locking stays open to any authenticated caller. The unlock event now
  records which key or session did it, as the server saw it.
- Unchanged: those other keys still reach routes that don't check a
  scope — a `read` key can still write to the non-private record.

### Added — Ask recipes: questions worth asking again
- ☆ on any question in the Ask panel saves it as a recipe; an empty
  panel lists your recipes as chips, and a click runs one. A recipe is an
  ordinary note tagged `#ask-recipe` (title = the chip's name, body = the
  question), so you rename, edit or delete it in Notes. A question asked
  with the vault open is saved as a private note.

### Added — a notification when something was written for you
- When the weekly review is written, the bell says so and links to it.
  When the month that just ended gets its chapter heading, the bell
  shows the heading and links to the biography. The backfill of older
  months stays quiet. One notice each, even with two nodes.

## 0.5.64 — 2026-10-07

Schema **v54** (additive: `friend_groups`, `peer_groups`). Peers on
0.5.63 stay wire-compatible (they ignore the `groups` field).

### Added — groups: share with several connections at once (schema v54)
- **A group is a named set of your connections** (☰ → Network →
  groups). You create it, you choose who is in it, and only your record
  edits it. A group can be given its own sharing (kinds of things, life
  areas, detail) and items can be pinned to it — tasks and notes offer
  "<name> (group)" in their share pickers.
- **What a group grants is added, never multiplied.** Each member gets
  their own sharing plus what each of their groups grants, checked one
  grant at a time: tasks granted by a group in "family" and memories
  granted directly in "work" never add up to "tasks in work". A group
  inherits nothing from the default template, so an empty group grants
  nothing and joining one never undoes a per-person "off". A revoked
  connection gets nothing through a group. Private stays unshareable.
- **Members are told.** A member's Hearthloom learns the group's name
  and who is in it (as you name your connections) and lists it under
  "groups you're in". In this version only the owner shares into a
  group.
- Deleting a group un-shares what was pinned to it; what you share with
  each person directly is untouched.
- Groups sync between your own nodes with the rest of the sharing
  settings. `GET/POST /v1/groups` (owner), `GET /v1/network/groups`
  (friend surface, `groups-v1`); the friend `offer` gains a per-grant
  `grants` breakdown.

### Fixed — "stop sharing" and "disconnect" on the Network page did nothing
- Both confirmation dialogs showed program text on the confirm button
  and, when confirmed, did nothing — a connection could not be revoked
  or removed from the page (the API always worked). Fixed; the dialogs
  now say "stop sharing" / "disconnect" and act.

## 0.5.63 — 2026-10-07

Security release. No schema change (still v53).

### Security — a connected friend's key reached the whole owner API
- **What was wrong.** The key a friend's node uses to read what you
  share was only checked on the friend-facing routes. Every other route
  accepted it as an ordinary key: on a login-protected node it could
  read tasks, notes and search, list keys, write tasks, and unlock the
  vault. The sharing settings limited the friend routes only. Found
  2026-10-07 while building groups; confirmed on a throwaway node. The
  hole dates from the first friend keys (v30/v46) — every release up to
  and including 0.5.62 has it.
- **The fix.** A key whose only scope is `friend` now reaches a fixed
  list of friend-surface routes (`friend_route_allowed` in http.rs —
  default-deny) and is refused everywhere else with 403. Nothing a
  friend's node legitimately calls changed.
- **Not changed, and worth knowing.** Other ordinary keys (`read`,
  `ingest`, `notify`…) are still accepted on routes that don't check a
  scope — the long-standing "trusted integration" posture. A `read` key
  can write. Scoped keys (the ones with grants) and plugin keys were
  already confined.

## 0.5.62 — 2026-10-07

Schema **v53** (additive: `llm_usage.items`). Peers on 0.5.61 stay
wire-compatible.

### Added — monthly budget, and embeddings on the Spend page (schema v53)
- **Monthly budget.** The Spend page has a budget tile: set a dollar
  figure for the calendar month (UTC) and you get one notification at
  80% and one at 100%, with a meter on the tile. It is an alert, not a
  cap — nothing is switched off. Spend is counted across both nodes from
  the price table; models with no price on file add nothing. The alert
  names the threshold and the budget, never the amount spent (that stays
  behind the vault). `POST /v1/llm/budget {"monthly_usd": 50}`; null or
  0 clears it.
- The budget follows the laptop to the always-on node with the
  operational mirror. A budget set on the always-on node does NOT travel
  back to the laptop (the mirror is one-way, like the profile card); its
  alerts still reach both nodes.
- **Embedding runs are ledger rows.** Each embedding backfill that did
  work records one row (`local:all-MiniLM-L6-v2`, purpose
  `embed_backfill`, `items` = chunks embedded, no tokens), priced at $0 —
  "free", not "no price on file". New column `llm_usage.items`; it rides
  the existing usage sync leg as an additive field.

### Added — Ask answers appear as they are written
- The final answer now streams into the Ask panel instead of arriving
  all at once (first words after ~5–8s on a two-round question that
  used to show nothing for ~13s). Tool rounds are unchanged. The text
  shown while streaming is a preview: when the answer is complete it is
  replaced by the checked version (citations verified, chips resolved),
  and a reply that is cut off or has to be redone is taken back off the
  screen.
- Streamed for Anthropic, OpenAI and Grok. Gemini and custom endpoints
  still answer in one piece (a custom server may not report token usage
  on a stream, and the Spend ledger must not read zero). The OpenAI/Grok
  path is covered by tests only — it has not been run against a live key.
- `hearthloom query`, `POST /v1/query` and the Ask eval are not streamed
  and behave as before.

### Added — life-area limits for scoped keys
- **A scoped key can be limited to life areas** (work, personal, family,
  health). Its reads — search, recall, timeline, dossiers, traverse,
  tasks, notes, and the MCP read tools — then return only those areas;
  an entity from another area reads as unknown. The API Keys page has
  the checkboxes; no schema change (`grants.areas`).
- An area-limited key cannot hold the Ask grant (refused at mint and at
  request, denied at the route table): Ask's tools read through raw SQL
  and a thread pool the area filter does not cover.

### Added — About me: make private, remove
- Every belief has **make private** (moves it into the vault: hidden
  while locked, never in an agent's briefing) and, once answered,
  **remove** (it was never right — off the page for good, with a second
  click to confirm). A removed belief is kept as `rejected` so it is
  never proposed again. "No longer true" is unchanged.

### Fixed — Ask spent a repair round on chart questions
- The "neither tool nor answer" reply now has a name: the model sends a
  bare chart spec (keys `type`, `data`, `series`, `title`, `xKey`) with
  no answer around it — four times in the laptop log, each recovered by
  the retry. The repair message now says where a chart belongs, and an
  answer object followed by a stray chart fence is read as the answer
  instead of failing to parse.

### Fixed — reads a scoped key should never have reached
- `GET /v1/tasks/deleted`, `/v1/notes/origins` and `/v1/notes/vaulted`
  are now denied to every scoped key. They sat under the tasks/notes
  grants but are not filtered by scope: purged-task text ignored the
  vault, and the origins list included a per-origin private count.

## 0.5.61 — 2026-10-07

Scoped API keys with MCP writes, the About-me page, the capture endpoint
for the phone, and model choice in the provider panel. Schema **v52**
(additive: one column, one table).

### Added — scoped API keys (v52)
- **A key can be scoped by grants** instead of coarse scopes: none /
  read / write per class (tasks, notes, memory), plus `ask` and a
  write-only `capture`. A scoped key reaches ONLY its default-deny route
  table (`KeyGrants::permits`); everything else is 403. Ordinary keys
  are unchanged.
- **It never reads while the vault is open** (403 until it relocks);
  writes still land.
- **Expiry presets** when minting (1 hour to 90 days, or never), with a
  countdown in the list. **Caps**: writes per hour, questions per day
  (in memory; reset by a restart).
- **Ask, then approve**: `POST /v1/keys/request` lets an agent ask for
  grants; it appears under ☰ → API Keys (and as a notification), lapses
  after 10 minutes, and on approval the requester collects the secret
  exactly once.
- **Writes over MCP without a fifth tool**: `propose_memory` takes
  `kind` (memory | task | note) and `mode` (propose | commit). Commit
  adds a task or note directly where the key holds write on that class;
  memories can only be proposed. MCP's read tools honour the same grants
  and the vault rule.
- **Activity and Undo**: every write by a scoped key is recorded under
  `agent:key:<name>` (fixed server-side — the key cannot name its own
  actor). ☰ → API Keys → activity lists what it created, with Undo
  (archives the task or note; still restorable).
- Not built: limiting a key to parts of your life (work / personal /
  health). That needs a per-request filter in every read query.

### Added — About me (☰ → About me)
- One page for everything Hearthloom holds as true about you: the
  identities it treats as you, confirmed beliefs by kind with origin
  ("you said" / "Hearthloom inferred") and their evidence, beliefs
  waiting for your answer, people and roles, goals, what used to be
  true, and the exact briefing an agent session loads.
- Correct a belief (your wording replaces it; the old one moves to
  "used to be true"), mark one no longer true, answer a pending one, or
  add your own. `GET /v1/portrait`; no model call; private items are
  absent while the vault is locked.

### Added — one-tap capture (server side only)
- `POST /v1/ingest/capture`: files a captured thought as a task (if it
  reads like one, or carries a task hashtag) or a dated note. Write-only
  — it returns the new id and nothing else. Accepts the device `ingest`
  scope or a `capture`-only scoped key; dedupes on `client_ref`. The
  iPhone side is specified in the [hearthloom-ios](https://github.com/metavert-io/hearthloom-ios) repo's `CAPTURE-TASK.md` and not
  built.

### Changed
- AI provider panel: each tier's model field offers that provider's
  models (Opus 5.5, Sonnet 5.5 and Fable 5.1 among them), and a tier on
  an always-thinking model gets an effort selector. Defaults are
  unchanged (Opus 4.8 / Sonnet 4.6 / Haiku 4.5).

## 0.5.60 — 2026-10-07

The Ask engine reworked against the 2026 RLM literature, a Spend page,
conversation-level Ask history and privacy, a custom/per-tier provider
layer, and three freeze/hang fixes. Reasoning and what was deliberately
NOT adopted: the design log 2026-10-05
§1.3/§1.6; the system map: `architecture.md`.

Measured with `hearthloom eval --ask` (26 questions, Opus 4.8 high tier):
cost per question $0.31 → $0.11, rounds 3.6 → 2.6, 79% of input tokens
cached, latency unchanged at ~29s — 80% of which is the model writing
its answer.

### Fixed — a dead mount could hang a question forever, and the daemon with it
- **Every Ask tool runs off the async runtime with a 40s limit.** A tool
  that touched a wedged network mount (a stale HearthFS/NFS mount after
  a reboot) blocked a runtime worker for good and the question never
  returned; a timeout is now reported to the model as a failed tool.
- **Mount liveness is probed in a child process** with a 2s deadline,
  and a dead mount is remembered for 30s. A thread blocked on a dead
  mount cannot be cancelled and — found live — keeps the whole daemon
  from EXITING: it hung in exit holding its port until a reboot.
- Ask's source inventory (a full episodes scan before the first model
  call of every question) is kept for ten minutes.

### Added — effort levels and laptop-only model trials
- On always-thinking models (Opus 5.x, Sonnet 5.x, Fable) the provider
  sends `output_config.effort` = the tier's own name, adds reasoning
  headroom to `max_tokens`, and leaves every earlier model's request
  byte-identical. `llm.effort.<tier>` overrides the level.
- A per-tier route may name the SAME provider with a different model —
  a node-local model override that does not sync to the peer.
- Trial result, same 26 questions: Opus 5.5 at HIGH effort passed the
  same checks at $0.20/question and 54s (vs $0.11 and 29s on Opus 4.8) —
  3.5× the output tokens, all reasoning. Not adopted; medium effort is
  untested. Defaults stay on 4.8.
- The eval prints where the time went: model vs Hearthloom's tools vs
  other, output tokens/second, and the slowest tools by name.

### Added — Ask: fewer rounds, one fan-out, checked citations
- **Batched tool calls.** The model may return `{"calls": [...]}` with
  up to 4 independent lookups in one round; each saved round is a full
  LLM round trip.
- **Seeded first round.** The hybrid search on the question as asked
  runs before round 1 (local, no LLM) and is handed to the model, so a
  simple lookup can answer in a single call.
- **`map {sql, instruction}`** — the one depth-1 primitive. Reads up to
  40 rows and has the Low tier apply the instruction to each, 8 at a
  time, one line back per row; for questions where every row has to be
  read. ≤2 per ask; private scope is excluded even with the vault open
  (`Store::read_query_public`); sub-calls land in the usage ledger as
  `ask_map` under the ask's id.
- **Citation check.** A cited id that never appeared in the evidence
  gathered is removed, recorded in the trace (`citation_check`), and the
  answer says so. `POST /v1/query` returns `dropped_citations`.
- **`hearthloom eval --ask`** (`eval/ask-eval.yaml`) — opt-in, spends
  real tokens: runs each question through the full engine and reports
  pass/fail plus rounds, tokens, cached share, dollars and seconds. The
  baseline for judging a model swap or prompt change. Seed set of 26
  runnable questions and 6 skipped templates that need the owner's
  ground truth (the retraction cases). `--only id1,id2` reruns a subset;
  the run stops on a billing/credentials refusal instead of failing every
  remaining question; a content miss prints the answer.
- The Ask loop accepts near-miss call shapes (a bare list of calls, a
  lone call object under `calls`, `name` for `tool`) and asks the model
  to retry a reply that is neither a call nor an answer, where it used
  to fail the whole question.

### Added — `custom` LLM endpoint and per-tier routing
- **`custom` provider**: any OpenAI-compatible endpoint (Ollama, LM
  Studio, vLLM, OpenRouter) by base URL, key optional. Integrations →
  AI provider shows the address field only while something uses it.
- **Send one tier to a different provider** (e.g. Low → a local model)
  via `llm.route.<tier>.provider` / `.model`; the ledger records the
  provider that actually served each call.
- Both are **node-local**: the address and routes never sync to the
  peer, and a `custom` active provider sends no LLM envelope at all.

### Changed
- **Prompt caching on Anthropic calls.** The system prompt goes out as
  one cached block, and Ask's is now byte-stable (today's date moved
  into the conversation) — rounds after the first read ~10k tokens of
  schema + BRAIN.md at the cache-read price. Watch `cache_read_tokens`
  in the ledger; anything volatile added to that prompt zeroes it.
- **Ask round token cap 3000 → 12000** (a cap, not a spend; reasoning
  counts against it on always-thinking models).
- **Price table**: Opus 5.5 ($4/$20, cache read $0.20), Sonnet 5.x
  ($2/$10) and Fable 5.1 (cache read $0.25) are matched before their
  families. The import cost estimator now reads the same table (it had
  Opus input at $15).
- Tool results reach the Ask model inside `<tool_result>` fences
  declared as data, not instructions.

### Security
- **Outbound gate on friend handles** (`nsearch`, `nfetch`, `ntasks`,
  `nrels`, `nnotes`, `nnote`, `noffer`), enforced in code: they run only
  when the user's own question names that friend, and only before the
  ask has read imported content, a mount, a bulk `map` read, or another
  friend's brain. Closes the path where text in a mail or clip could
  steer Ask into sending private content out as a "query".

### Fixed
- A reply cut off at the token cap, or refused by the provider, is now
  named as such (`Completion.stop`) instead of surfacing as "no JSON in
  response".
- Switching provider with a tier left blank kept the previous provider's
  model id; it now falls back to the new provider's default.

### Added — Spend page (☰ → Spend)
- **What the thinking costs**: every AI model call priced and broken
  down. Headline tiles (today, this month, projected month, mean cost per
  question, cache savings, window total); spend per day as stacked bars
  switchable by model / purpose / provider / tier, with a hover readout;
  a model × purpose grid; a tier split; the cost distribution of
  questions and the ten dearest, each linking to its trace; and the full
  report table — sortable, with CSV export. Period presets, custom
  dates and a node filter.
- `GET /v1/llm/spend?from=&to=&node=` returns day × provider × model ×
  tier × purpose cells (priced, with cache savings), month-to-date and
  per-question figures. Vault-gated like `/v1/llm/usage`.
- A series keeps its color when the window or filter changes; past seven
  series the rest fold into "other". Unpriced models show tokens and "no
  price on file".
- Not built (proposed, deferred): recording non-LLM models (local
  embeddings) in the ledger, and a monthly budget alert.

### Changed — Ask history is conversations, and private ones are sealed whole
- **One history entry per conversation.** A follow-up no longer appears
  as its own item: the entry is the opening question with an "N
  questions" count, ordered by latest activity; opening it replays every
  turn and a new question continues it. Search matches any turn. Delete
  removes the whole conversation (`DELETE /v1/ask/{id}`).
- **A conversation is private as a whole.** One question asked with the
  vault open seals every turn of that conversation — earlier ones
  included. While locked it is absent from the history, unreachable by
  id, thread, delete or Ask's own `ask_log` SQL, with no count of what is
  hidden. Conversations recorded before this rule are sealed the same
  way.
- **Seal a conversation by hand.** A 🔒 button in the Ask header (for the
  open conversation) and on each history item makes it private without
  asking anything — for something you find sensitive that the vault
  logic didn't catch. Sealed while locked, it leaves the screen at once.
  A hand-sealed conversation can be made public again with the vault
  open; one that contains a question asked with the vault open cannot,
  ever. Private conversations carry a 🔒 mark in the history.
  `GET/POST /v1/ask/{id}/privacy`.
- **The panel follows the lock.** Locking or unlocking (the top-bar
  button, the Privacy card, or the timed relock) refreshes the history at
  once and clears a conversation from the screen if the lock now seals
  it.

### Fixed — Ask panel and plugin pages
- The Ask panel's top row ("New question", the title, debug, ×) sat
  under the menu bar; the panel now starts below the bar's measured
  height.
- Plugin pages (e.g. Presence map) show a spinner until the page is
  alive, wait and retry while Hearthloom isn't answering, and offer "try
  again" instead of leaving the browser's error page in the frame. Also
  fixed: a page that loaded before its token arrived never got it.
- Price lookups re-read the config (and the keychain) once per ROW;
  they now reuse it for a minute. The usage views were paying this too.

### Fixed — dashboard freeze on a cold start (2026-10-06)
- **`/v1/stats` could wedge the whole node** ("clearing the desk…" on
  every screen). It ran five full walks of the episodes table plus a
  ~40k-lookup entity ranking under the store lock, with no single-flight:
  on a cold page cache one compute took 26s and every poll started
  another. Now: the composition is ONE scan (`Store::episode_composition`),
  the locked episode count is index-only, the entity ranking resolves
  merges in memory and is kept for 10 minutes, at most one compute runs,
  and once a value exists every caller is served it immediately while an
  expired one refreshes in the background. Refreshes went from 3–26s to
  under half a second; a compute over 500ms logs its per-query timings.

### Not changed, on purpose
- Default model ids stay `claude-opus-4-8` / `claude-sonnet-4-6` /
  `claude-haiku-4-5`. Run `hearthloom eval --ask` before and after any
  swap to the 5.5 models.
- No recursion beyond depth 1, no Clef/Jev router, no fine-tuned Qwen
  for Ask — see the design log for why, and for the deferred router trial
  and the scoped local-extractor plan.

## 0.5.59 — 2026-08-17

### Added — optional Sentry APM (doubly gated; fleet-wide observability)
- **The daemon can report crashes/errors/performance to Sentry — behind
  TWO independent gates.** Gate 1, operator config: the SDK activates
  only when `SENTRY_DSN` is present at boot (sealed env on a personal
  node — new `hearthloom secret set|list` CLI writes it, value from
  stdin; machine env on a hosted node). No DSN ships in this repo;
  absent config is a quiet no-op. Gate 2, user consent: the Settings →
  diagnostics "Share error reports" toggle (consolidation_state
  `telemetry.error_reporting`) gates every event client-side via
  before_send + the traces sampler, applying instantly with no restart.
  `SENTRY_DEFAULT_OPTIN=1` lets a hosted distribution default consent
  ON (disclosed in the card; an explicit user choice always wins) —
  a hosting provisioner injects exactly that at machine create.
- **Privacy stricter than Sentry's own quickstart**: send_default_pii
  hard-off (request headers carry PATs/session cookies), no request
  bodies, no episode content — panics, `tracing::error!` lines, and
  route-templated HTTP transaction timings only. `/health` never
  traced. Environment auto-tags tenant/local; release
  `hearthloom-daemon@<version>`.
- **End-to-end distributed tracing**: the HTTP layer continues incoming
  `sentry-trace`/`baggage` headers — the iOS app's ingest POSTs and the
  hosting provisioner's control calls stitch into one cross-project
  trace. Default server trace rate 0.1 (`SENTRY_TRACES_SAMPLE_RATE`).
- **Surfaces**: `GET/POST /v1/settings/telemetry` (status + consent),
  `POST /v1/settings/telemetry/test` (sends one test event, reports
  whether the transport drained — honest about which gate blocked it
  otherwise), and the Settings diagnostics card (renders only on
  DSN-configured builds).

## 0.5.58 — 2026-08-17

Carries BOTH post-0.5.57 waves: v50 (llm_usage, committed 2026-08-16 but
never released) and v51 (the iOS device-feed wave).

### Added — v51: the iOS device-feed wave
- **Unified location model + cross-source dedup (Part A, the
  centerpiece)**: new `location_observation` table receives every
  geolocated signal from every source (photo geotags backfilled and
  dual-written, CLVisit dwells, calendar venues, scan captures) keyed
  UNIQUE(source, source_ref); the new `visits_finalize` consolidation
  pass clusters each closed day spatially (~100 m, grown by accuracy)
  and temporally (≤20 min gap) into canonical `visit` episodes citing
  every contributing source — photo + GPS + calendar at one place/time
  become ONE visit, never three episodes. A lone geotagged photo is not
  a visit (dwell or ≥3 observations over ≥20 min required). Minting
  rides the photo_day supersession precedent (`visit_day` ledger,
  superset guard, `visit:<day>:<idx>#r<n>` refs, shrink-absorption);
  visit episodes feed the Atlas.
- **Eight new ingest endpoints** (`POST /v1/ingest/…`, ingest-scope
  PAT, idempotent re-POST = pure duplicates): `contacts` (durable
  contact_card store → Person entities attribute-first, the identity
  backbone; changed cards re-mint under #r<n>), `calendar` (rides
  source="ics" so live phone events dedupe against .ics imports on
  event UID; venues with coordinates feed the location layer),
  `reminders` (upsert into tasks under deterministic tk_rem_<hash> ids,
  journaled, completion one-way), `location` (CLVisit dwells — the
  flagship), `motion` (`trip` episodes), `media` (`media_play` episodes
  + Track/Artist/Show entities), `scan` (receipt/business_card/document
  episodes from OCR text; `ios-scan` joins the extraction allowlist so
  the LLM pass mines merchants → Org and card people → Person;
  best-effort receipt total parsed), `focus` (attention states).
- **`device_state` is a source-generic attention stream** (harmonized
  with the hpresence ember's model): iOS Focus modes feed it first; a
  future hpresence release can post coalesced app/idle spans into the
  same stream. Re-POSTing an open span's key with an end time closes it
  in place.
- **Three new peer legs** under the peer-sync-default law:
  `/v1/sync/locations` + `/v1/sync/device_state` (telemetry pattern)
  and `/v1/sync/contacts` (LWW by updated_at); `visit_day` is
  deliberately node-local — each node re-clusters synced observations
  and converges.
- **Privacy**: `ingest.device.scope=private` consolidation-state toggle
  forces everything location/motion/focus mint to scope private;
  contacts/calendar/media/scan default personal.
- **Ingest audit polish (Part C)**: `/v1/ingest/status` reports all ten
  device sources (count / last_ingest_at / days_covered); every
  accepted batch logs a human-readable ref (day span, card count,
  state) into `ingest_log` for the iOS Sync-history screen.
- **Ask can introspect its own utilization**: new RLM tool
  `llm_usage {days?, ask_id?}` over the v50 ledger — priced aggregates
  or one ask's per-call trace — prompt-restricted to explicit
  usage/cost questions; the blanket vault gate applies unchanged.

### Added — v50: llm_usage token/cost accounting + Ask observability
- **Every LLM call lands one ledger row** (v50 `llm_usage`): daemon
  `llm::call` is now the ONE way to reach the provider (all 16 sites:
  14 consolidation passes + 2 Ask) — purpose/ref/model/tier,
  input/output/cache tokens, latency, status, on success AND error.
  Tokens are stored truth; dollar cost computes at read from
  `provider::pricing_for` + `llm_pricing` config overrides (unknown
  model = "no price on file", never fabricated).
- **Monitoring → "LLM usage"**: 7/30/90-day × both/local/cloud,
  cost-per-day chart, by-purpose and by-model tables.
- **⌗ debug on every Ask history row** → full trace modal: per-round
  model/latency/tokens, full tool results (capped 24KB), repair and
  finish steps, ledger rows with priced totals
  (`GET /v1/ask/{id}/trace`).
- **Blanket vault gate** on both read surfaces (403 while locked,
  enforced at the store and the raw-SQL shadow view too) — traces can
  quote vaulted content and spend patterns reveal vaulted activity.
  Peer-synced via `/v1/sync/llm_usage` (telemetry pattern).

## 0.5.57 — 2026-08-16

### Performance
- **semantic_pairs pass 27s→2s per cycle** (measured live: candidate
  query 31.0s→2.7s, pass 26940ms→1975ms, same 80 pairs): the candidate
  feed ranked all ~252k episodes with a correlated
  `COUNT(*) FROM episode_entities` per row — 252k index re-scans under
  the store lock every cycle. It now aggregates episode_entities once
  and joins episodes by primary key. Zero-entity episodes drop out of
  the feed by construction (they were accidental filler in sparse
  sources — the feed's stated intent is "entity-rich").
- **opportunity_scan deliberately unchanged** after evaluation for the
  v49 ANN index: it is watermark-incremental (~114ms/cycle, cost scales
  with new chunks); a global KNN cannot restrict to just-ingested
  episodes and would silently miss matches. Rationale documented in the
  code and the design log.

## 0.5.56 — 2026-08-13

### Added
- **sqlite-vec ANN search** (v49): `vec_chunks` vec0 index (float[384],
  cosine, rowid = chunks.rowid) behind `vector_search`. New embeddings
  dual-write; the consolidation cycle backfills history (≤500/batch,
  breaths between); once the index covers every embedded chunk the
  hybrid-search vector leg switches from the brute-force
  load-every-embedding scan to KNN (oversampled 8×, best-chunk-per-
  episode, identical scope/vault semantics — plus both paths now
  exclude superseded episodes, matching the FTS leg). Brute-force stays
  as the fallback until coverage, and forever for off-dimension
  vectors. The index is derived and rebuildable: excluded from the
  readable export, rebuilt from `chunks.embedding` on any node. RRF
  fusion weights untouched.
- **iOS ingestion verification surface** (v48): `ingest_log` audit
  trail + `GET /v1/ingest/status` (per-source sample counts, last
  ingest, days covered) and `GET /v1/ingest/recent` (paged accepted
  batches) — raw device batches finally have a read path before the
  daily finalize mints episodes. Keyless loopback or `read`-PAT.
- **Guarded SQL endpoint** `POST /v1/sql` (keyless loopback / session,
  or admin PAT) over the RLM's read_query surface; eval harness gains
  the `sql` tool (both yaml cases now runnable) and a SKIP verdict for
  offline virtual sources (vsearch-hearthfs no longer poisons the tally).
- **Semantic-search card** (Integrations + wizard): consented one-time
  ~90MB all-MiniLM download from Hugging Face — staged, progress-polled,
  disk-space-checked, validated by a real tract load + smoke embed
  before atomically installing and self-restarting. Failure leaves the
  node FTS-only, exactly as before.
- **Git repo auto-discovery** `GET /v1/ingest/discover-git` (bounded
  home walk, ≤3000-commit + authored-by-this-machine recommendation) +
  wizard "scan for my repos" confirm-then-ingest list.
- **First-value moment**: the wizard now renders your own last week
  inline after the first successful ingest (recall, not a promise).
- **Desk "recently deleted" drawer** over `/v1/tasks/deleted` +
  journal restore; **note version browser** ("history" mode in the
  editor) over new `GET /v1/notes/{id}/versions[/{entry}]` — op_journal
  post-images, vault-gated by the note's current visibility; loading an
  old version into the editor is the human save gesture.

### Fixed
- `hearthloom bench` now uses the CLI's stored keychain key (it only
  read `HEARTHLOOM_API_KEY` and 401'd on auth-locked nodes).
- A dead FUSE mount (mountpoint present but empty) now reads as
  **offline** for magic-path virtual sources — `@search` is the
  liveness probe — instead of 500ing mid-read; the eval SKIPs it.
- Journal restore of a task whose tombstone carried `status=deleted`
  now restores to `open` (an invisible restore isn't a restore).
- Playwright verify script (`scripts/wizard-verify.mjs`) extended to
  the 0.5.56 surfaces: embeddings card, git repo scan, deleted-tasks
  drawer + restore-to-archive, note history mode.

## 0.5.55 — 2026-08-13

### Added
- **The onboarding wizard grows up** (#/welcome — the 2026-07-12 design
  completed): a new **"protect this brain"** first step renders on any
  auth-OPEN node that isn't provisioner-managed and creates the admin
  login from the browser (`POST /v1/auth/bootstrap` — the UI
  counterpart of scripts/bootstrap.sh: the admin exists → the door
  locks, and the creator is signed straight in so the wizard just
  continues). Skippable, with an honest "the record stays open" note.
- **The Integrations cards ARE the wizard steps**: integrations.js now
  exports `paintAnthropic` / `paintGmail` / `paintPair` (+
  `initHelpers`), and #/welcome mounts them directly — the wizard's
  hand-rolled AI-key form is gone (replaced by the full multi-provider
  card with per-tier models), and two new optional steps appear: **keep
  mail current** (the Gmail live-pull card) and **bring your phone**
  (the pair-a-device QR card — first-run pairing becomes a scan). Step
  numbers are computed, so conditional steps never leave gaps.

### Fixed
- **First-boot Desk race**: the empty-brain redirect to #/welcome could
  replace the view while desk.js's async mount was mid-flight, throwing
  `Cannot set properties of null` on the quick-add wiring. The mount now
  bails cleanly when its hosts are gone.

## 0.5.54 — 2026-08-13

### Added
- **Pair a device (QR)** — Integrations → companions: one click mints a
  `read,ingest,notify` device PAT and renders it as a QR
  (`hearthloom://pair?url=…&pat=…`) the iOS app can scan — self-host
  pairing becomes a scan instead of a paste (server half of iOS ASK-4).
  QR encoder is the vendored MIT qrcode-generator 1.4.4 (no CDN).
- **Pulse "today so far"** — health series now end at the current
  moment, not the last export: `GET /v1/health/today` rolls up today's
  LIVE device-streamed samples (v44 `health_sample`, coarse-vault-gated
  like the vquery surface) and Pulse appends the intra-day point with
  each metric's own aggregate (steps sum, resting-HR min, sleep hours
  by wake-day, …).

## 0.5.53 — 2026-08-13

### Added
- **Google Mail integration card** (Integrations → companions) — connect
  the Gmail live pull from the dashboard: app-password instructions with
  a direct link (myaccount.google.com/apppasswords — Google hides the
  page from its menus), test-then-store (a real IMAP login must succeed
  before anything persists), pull-now/disconnect, last-pull status.
  `GET/POST /v1/integrations/gmail`; the credential is persisted via the
  sealed-secrets upsert (`set_env_vars`) — never a raw `.env` write,
  which would clobber the sealed file — and takes effect without a
  daemon restart.

## 0.5.52 — 2026-08-10

### Added
- **Gmail live federation (Tier 3)** — the `gmail_pull` automation pulls
  new mail over app-password IMAP each consolidation cycle into a rolling
  `archives/gmail-live.mbox`, registered as the `gmail-live` virtual
  source (searchable/fetchable/promotable like the Takeout archive). The
  archive's Core strata (Sent ∪ Category Personal ∪ Starred) materialize
  as episodes automatically, deduped against the Takeout by Message-ID
  (`source="mbox"` convergent identity). Gmail categories are annotated
  via a ranged X-GM-RAW search (X-GM-LABELS doesn't carry them). Silent
  no-op until `GMAIL_IMAP_USER`/`GMAIL_IMAP_PASSWORD` land in
  `~/.hearthloom/.env`; first run backfills `GMAIL_IMAP_BACKFILL_DAYS`
  (default 90), 2000 messages/cycle cap.
- **Incremental mbox sidecar indexing** — `mbox_index::append_index`
  indexes only newly appended bytes and refreshes the fingerprint in
  step, so live appends never trigger a Takeout-scale rebuild; sidecar
  `meta` (incl. the IMAP `uidvalidity:last_uid` cursor) survives full
  rebuilds.

## 0.5.51 — 2026-08-10

### Added
- **Tend "tidy the queue"** — re-applies the current precision rules to the
  EXISTING backlog (the rules previously only gated fresh scans):
  auto-resolves same-first-name pairs whose surnames conflict as distinct,
  never touching pairs that share an identifying attribute
  (`POST /v1/reviews/retriage`).
- **Manual merge from the person modal** — an "add identity…" field claims
  an email/handle onto that person, folding in any existing owner
  (reversible), so unifying someone's several addresses no longer needs
  the CLI or Tend.
- **Remove revoked grants** — a "remove" button on struck-through grant
  rows deletes them for good (`DELETE /v1/grants/{friend}`; refuses an
  active grant — revoke first; drops that friend's item shares too).

## 0.5.50 — 2026-07-29

### Changed
- **Tend uses email-derived full names as merge evidence** (Jon's
  insight): the record usually knows the full name behind an address, so
  a person whose name spells another person's email localpart — mreyes /
  morgan.reyes / morganreyes ↔ "Morgan Reyes" — queues at 0.99, the
  strongest signal short of an identical address. Same-first-name-only
  shapes (jsmith ↔ Morgan Reyes) never match; surnames under 4 chars are
  too ambiguous to anchor. This leg needs no embedding model, so it runs
  on nodes without one.

## 0.5.49 — 2026-07-29

### Added
- **Sharing activity notifications, both directions.** A friend ticking a
  checklist item notifies the owner instantly ("Sam checked off …");
  and each node watches what its connections share TO it (`friend_activity`
  automation + the Desk rail load), notifying on new shares, checklist
  progress, and completions — first sight baselines quietly, no replayed
  history.
- **Tend search** — a filter box over the WHOLE identity-review queue
  (names and identities, not just the dozen on screen).
- **Settings lists every claimed identity with a remove (×)** — mis-claims
  are undoable (`POST /v1/identity/unclaim`; the entity keeps existing,
  only the binding goes).

### Changed
- **Tend precision** (the same-first-name flood): conflicting surnames now
  count as affirmative evidence of distinctness (never queued); a bare
  single-token name ("Ben") queues at most ONE question — its best match
  at a higher (0.92) similarity bar — instead of one per acquaintance.
  Also: LinkedIn's `@reply.linkedin.com` / `@bounce.linkedin.com` relay
  addresses are machine addresses now (never Persons).
- **Avatar polish**: initials are first+last ("Morgan Reyes" → MR; your
  own card/email name → JR, never "ME"); the hamburger ☰ IS your avatar
  now (removed from the Settings row); avatars accept up to 1MB.
- **Task sharing UX**: the picker defaults to "(choose who)"; un-sharing
  updates the open card in place — badges and the panel redraw without a
  full repaint, so the card stays open where you were.
- Upgrade line reads "Running most recent" (no wrap).

### Fixed
- **Cloud node CPU/memory finally collected**: the monitor sampled via
  `ps`, which the tenant image doesn't ship — silent "not enough data"
  forever. Falls back to /proc/self (VmRSS + jiffies delta) on Linux.

## 0.5.48 — 2026-07-29

### Fixed
- **Task/note/relationship share pickers now offer everyone you share TO.**
  The pickers were populated from outbound friend connections only — but the
  common posture is a friend who connects INBOUND to your always-on node
  (a grant, no outbound entry), so the share controls never rendered. They
  now list active grants ∪ outbound connections; with grants peer-syncing
  (0.5.47), the picker works on every node.

## 0.5.47 — 2026-07-28

### Added
- **iOS photo-location ingest, reconciled with the Mac scan (schema v47).**
  `POST /v1/ingest/photos` (scope `ingest`) lands geotag points — never
  images — in the durable `photo_point` store, keyed on
  `<unix-capture-secs>|<filename>`: the identity BOTH the Mac
  Photos.sqlite scan (which now feeds the same store) and the phone can
  see, so the two sources UNION instead of duplicating. The
  `photos_day_finalize` pass mints `photos#YYYY-MM-DD` episodes from the
  union via the same adapter day-builder; a day that later GROWS (late
  phone photos) re-mints under **THE SUPERSESSION PATTERN** (documented
  in CLAUDE.md for reuse): a revisioned ref (`photos#DAY#r2`), receiver-
  side marking derived purely from the ref (zero wire changes, peers
  converge on identical episode ids), a superset guard so a points-poor
  node never clobbers a rich day, and `superseded_by` lifecycle metadata
  excluded from search/recall/timeline (Browse still shows history).
  Photo points replicate between peers (`/v1/sync/photos`) since the Mac
  scan lives on the laptop and the phone posts to the tenant.
- **Grants + shares sync between a user's own peers**
  (`/v1/sync/sharing`, LWW by the new `grants.updated_at`): the sharing
  posture — default template, per-friend matrix, revocations, item
  shares — is now ONE thing you can view and edit on any of your nodes;
  a change on the laptop reaches the tenant (where friends actually
  connect) within a sync cycle and vice versa. Friend keys stay on the
  node that minted them.

## 0.5.46 — 2026-07-28

### Fixed
- **Ask sharing introspection on a granting-only node.** The RLM's network
  section (and with it the `sharing` handle) only rendered when outbound
  friend connections existed — a node that GRANTS but doesn't connect out
  (a hosted node's posture) couldn't answer "what am I sharing with
  Sam?" from the actual sharing posture. The handle is now advertised
  whenever grants exist.

## 0.5.45 — 2026-07-28

### Added
- **Granular friend sharing (schema v46)** — the friend surface grows from
  episodes-only to a full consent matrix (design log 2026-07-28):
  - **Share matrix with live inheritance**: a DEFAULT sharing template
    (`grants` row `__default__`) every connection inherits, with per-friend
    tri-state overrides (on/off/inherit per content class — memories, tasks,
    notes, relationships, profile card — plus spheres, excerpts/full,
    completed-tasks and evidence-tier-people flags). Changing the default
    updates every inheriting connection instantly. Existing grants were
    frozen to their exact current behavior at migration.
  - **Explicit item shares** (`shared_items`): pin a specific task, note, or
    relationship to a named friend — readable even when its class is
    categorically off, full detail always, optional expiry, optional
    `collaborate` (checklist ticks — the ONE friend write, audited as
    `friend:<name>`, toggle-only). Un-share (and privacy relabels) take
    effect on their very next request; nothing is ever copied to their node.
  - **Operational reads over the friend surface**: `/v1/network/tasks`
    (open/backlog/done, done gated), `/v1/network/relationships` (declared
    rows; evidence-tier people when opted in), `/v1/network/notes` +
    `/v1/network/note/{id}`, `/v1/network/shared` (the explicit bundle),
    `/v1/network/offer` (the MAY descriptor), `/v1/network/profile` (my
    calling card, curated on the Network page). hello now advertises
    capability strings; older peers degrade cleanly.
  - **Relationship seeding on connect**: connecting a brain creates/claims a
    Person from their profile card (stable `brain:<name>` handle + their
    emails) and a declared `friend` relationship — existing connections are
    seeded retroactively on the next Network-page visit.
  - **Ask integration**: new RLM handles `ntasks`/`nrels`/`nnotes`/`nnote`
    (their side), `noffer` (what they offer me), and `sharing` (introspect MY
    posture — "what am I sharing with Sam?").
  - **UI**: default-sharing card + per-friend override matrix with
    inherited-vs-set rendering (Network), abbreviated "they share: …" line
    per connection, my-card editor, share pickers on task details and the
    note editor, relationship sharing in the person modal, mini friend
    avatars on shared records (hover for info, click for their card), and a
    "shared with you" Desk rail — live-queried from each friend's node, with
    collaborative checklist ticking and "⧉ fork to my desk" as the one
    explicit copy gesture.
  - Item shares mirror between a user's own peers (journal + LWW); grants
    stay node-local (they pair with the friend key minted on that node).

## 0.5.44 — 2026-07-28

### Fixed
- **Friend-connect probe could wedge the target node** (the 2026-07-28
  hosted-node outage). Three queries (network_search, /brain search,
  cascades) drive FROM episode_fts and probe episodes by `fts_rowid`,
  which was never indexed — every FTS match cost a full episodes scan,
  quadratic on common tokens like the connect probe's `q=hello`. Each
  timed-out retry stacked another uncancellable grinder on a runtime
  worker until the node stopped answering entirely. Schema **v45** adds
  `idx_episodes_fts_rowid` (both join directions are now point probes;
  EXPLAIN-plan regression test), and the friend-surface handlers
  (`/v1/network/search`, `/v1/network/fetch`) are spawn_blocking'd per
  the handler contract.

## 0.5.43 — 2026-07-28

### Fixed
- **Onboarding wizard looped back blank from its own "open the imports"
  button.** The leave-setup intercept treated every exit from #/welcome as
  abandonment — including the wizard's own step-4 CTAs — so a fresh
  hearthlaunch user who filled the forms and continued to imports got
  bounced to a blank re-rendered wizard (inputs were saved server-side;
  only the completion mark was missing). All three onward CTAs (open the
  imports / show me my last week / I'm all set) now mark onboarding done
  before navigating, which also retires the reminder banner.

## 0.5.42 — 2026-07-27

### Added
- **Checklist item edit.** Each checklist row now carries a ✎ next to the ×:
  it swaps the text for an inline editor (Enter/blur saves, Escape cancels; a
  blank edit is a no-op — deletion stays the explicit ×). Hover-revealed like
  the delete, accent on hover.

## 0.5.41 — 2026-07-27

### Added
- **Live HealthKit ingest for the iOS companion app** (spec:
  hearthloom-ios docs/server-implementation-spec.md; three ASKs).
  - `POST /v1/ingest/health` (scope `ingest`): device-streamed samples land
    in the new durable `health_sample` table (schema **v44** — uuid-keyed,
    so re-streamed batches are idempotent no-ops; response
    `{accepted, duplicates, workouts_new}`). Workouts mint episodes
    immediately, reusing the export path's body/title builder, keyed
    `workout:<HK uuid>`, trust `self`.
  - **Day-finalize consolidation pass** (`health_day_finalize` automation,
    daily, fully local): seals each closed day (device-timezone aware, 2h
    straggler grace) into its `health_day` episode via the SAME
    `apple_health::DayTable` aggregation as export.xml — live and export
    converge on the shared `day:YYYY-MM-DD` key, first-writer-wins.
  - `POST /v1/auth/session-from-key` (scope `read`): mints an `hl_session`
    cookie from a device PAT so the iOS webview renders the authenticated
    dashboard without re-prompting for a password.
  - Follow-ups noted, not included: Pulse "today so far" reading the
    durable table; health_day supersession by a later fuller export.

## 0.5.40 — 2026-07-27

### Changed
- **Provisioner-managed nodes hand sign-in back to the provisioner.** When a
  node is under an external provisioner's control plane
  (`HEARTHLOOM_PROVISIONER_SECRET` present — hearthlaunch tenants), the
  sign-in overlay no longer presents the confusing local password form:
  `/v1/auth/me` now advertises the provisioner's dashboard URL
  (`HEARTHLOOM_PROVISIONER_URL`) and the
  dashboard redirects there (with `?open=<node-origin>` so the provisioner
  can pick the handoff up and re-enter via the trusted-issuer token, which
  carries the right auth back). A "sign in with a password instead" link
  keeps the local path reachable — the user owns the substrate even when the
  provisioner is down. Self-hosted nodes are unaffected.

## 0.5.39 — 2026-07-27

### Added
- **Cross-node journal sync (operational-durability Phase 3 — the last
  phase).** Peers now exchange `op_journal` entries incrementally
  (`GET/POST /v1/sync/journal`, peer scope, seq cursors both directions).
  Apply is an idempotent replay keyed by `entry_id`; received entries land
  with actor `peer:<name>` (the episodes precedent), which is also the echo
  guard. Purges/deletes finally propagate: a `delete` tombstone at-or-after
  the local row's stamp removes the row on the other node — the ONE sync leg
  that deletes across nodes, LWW-guarded so a newer edit on the other node
  survives an older tombstone (and the next mirror cycle re-seeds the
  purging node with the survivor). The operational mirror gained a matching
  resurrection gate: a snapshot row older than a local delete-tombstone is
  skipped instead of re-inserted. Journal replication also makes cross-node
  restore possible (a task purged on one node is restorable from the
  other's copy of its journal). Older peers 404 the endpoint and the leg
  skips — wire discipline.

## 0.5.38 — 2026-07-27

### Added
- **Operational journal extended to notes (with body capture).** Note
  create/edit/archive/soft-delete now journal the notes_index row PLUS the
  markdown body (under a synthetic `__body__` key), so a note's content is
  recoverable even though the file is edited in place. `restore_note` rebuilds
  the index row and rewrites the file. Notes use soft-delete (status), so the
  soft-delete is the tombstone. All operational content (tasks, goals,
  self-model, privacy, notes) is now durably journaled.

## 0.5.37 — 2026-07-27

### Added
- **Operational journal extended to goals, self-model, and privacy rules.**
  Same durability as tasks: create/edit/delete now record full content
  post-images, hard-deletes (`remove_intent`, `remove_privacy_rule`) journal a
  delete-tombstone first, and `restore_intent`/`restore_privacy_rule` recover
  them. Closes the audit's remaining "primary mutable content, no journal"
  vulnerabilities. (Note body-versioning and journal-based cross-node sync
  remain.)

## 0.5.36 — 2026-07-27

### Added
- **Operational journal (append-only) for tasks + `restore`.** Every task
  create/edit/complete/uncomplete/checklist-change/purge now records a full
  content post-image to a new `op_journal` table (schema v43). A lost, edited-
  away, or purged task is recoverable: `POST /v1/tasks/{id}/restore`, and
  `GET /v1/tasks/deleted` lists recently-purged tasks. This closes the
  root cause of the 2026-07-27 loss (creation/edits left no recoverable
  record). 

### Fixed
- **Relationships peer-sync clobber.** `apply_operational` no longer
  `DELETE FROM relationships` + replace-all (which wiped any relationship the
  receiving node had that the sender lacked). It now merges per logical key
  (entity + kind + t_valid) under last-writer-wins — the last surviving
  instance of the replace-all bug class fixed in 0.5.34.

## 0.5.35 — 2026-07-27

### Fixed
- **Sync hotfix:** the new multi-master merge 500'd on the peer when a local
  row had a NULL `updated_at` (legacy data), so the operational mirror was
  refused (`mirror_ok:false`) and stopped syncing. The LWW gate now reads the
  local timestamp as nullable — sync resumes.

## 0.5.34 — 2026-07-27

### Fixed
- **Peer sync no longer loses tasks/notes (multi-master merge).** The
  operational mirror used to `DELETE FROM` + replace-all each table, so a
  task/note created or completed on one node was clobbered by the other's
  snapshot (data loss + reverted completions). It now does a per-row
  last-writer-wins MERGE keyed by `id` (newer `updated_at` wins), and never
  deletes rows the peer lacks — both nodes are authoritative, so changes
  from either side reconcile instead of overwriting. Hard deletes still
  don't propagate (soft-delete via status is the tombstone).

## 0.5.33 — 2026-07-26

### Changed
- **Adding a checklist item keeps focus in the input** — after pressing Enter,
  the cursor returns to the add-item field so you can type the next one.

## 0.5.32 — 2026-07-24

### Changed
- **Task cards drop the kind label; the completion button shows it instead.**
  The redundant "plain/checklist/workout/form" text is gone from the card
  meta — the left button now carries a kind-appropriate icon (✓ plain,
  list checklist, dumbbell workout, clipboard form).

## 0.5.31 — 2026-07-24

### Added
- **Mobile-responsive masthead/nav.** At phone widths (≤680px) the primary
  nav collapses behind a ☰ toggle into a full-width drawer (Visualize expands
  inline), the utility cluster compacts (Ask becomes icon-only, theme/rewind
  hidden), and slide-in panels fill the viewport — no more horizontal
  overflow. Groundwork for the iOS webview shell.

## 0.5.30 — 2026-07-23

### Added
- **Clickable links in tasks.** Bare http(s) URLs in a task description or a
  checklist item now render as clickable links (open in a new tab). Links
  coexist with @mentions in descriptions; only http/https is linked, the
  text stays escaped, and clicking a link in a checklist item doesn't toggle
  the item.

## 0.5.29 — 2026-07-23

### Added
- **Completed tasks drawer.** The Desk now has a "completed" drawer showing
  everything finished today plus up to 10 more recent, newest first — each
  card shows the date it was finished. Un-checking a completed item brings
  it back to the desk as an open task (new `/v1/tasks/{id}/uncomplete`;
  the original completion episode stays in the record).
- **Checklist tasks open instead of completing.** A checklist card now leads
  with a list icon that opens the checklist (check/add/delete items); a
  "Complete Task" button inside finishes it, and warns first if any items
  are still unchecked — a chance to review before closing it out.
- **Backlog and completed are reachable.** The backlog, completed, and
  archive drawers sit together under the task list.

## 0.5.28 — 2026-07-23

### Added
- **Delete a checklist item.** Each item now has a × that removes it,
  behind a styled confirm modal (the check/uncheck box is unchanged).
- **Checklist progress on the card.** A checklist task shows "x of y done"
  in its Desk card meta, updated live as items are checked or deleted.

## 0.5.27 — 2026-07-23

### Added
- **New tasks float to the top.** A just-added Desk task now animates in
  at the top of the list (it grows in while the cards beneath roll down),
  then settles into its real sorted position on the next paint / reload.
- **Checked checklist items are crossed off.** Ticking a checklist item
  now strikes its text through immediately (and on reload).

### Fixed
- **Checklist paste split now handles every line ending.** A pasted list
  using CR-only (classic-Mac) or Unicode line/paragraph separators is now
  split into items too — previously only LF/CRLF split, so some pastes
  landed as one long item.

## 0.5.26 — 2026-07-23

### Added
- **Checklist tasks accept a pasted list.** Pasting multi-line text into
  a checklist's "add an item…" box splits it into one item per line —
  trimmed, with a leading list marker stripped (bullets `- * • · – — +`,
  numbers `1.`/`1)`/`(1)`, and markdown checkboxes `[ ]`/`[x]`); blank
  and marker-only lines drop out. The add-one-at-a-time flow (type +
  Enter) is unchanged; a single-line paste still adds one item.

## 0.5.25 — 2026-07-22

### Added
- **Ember pages flow continuously** — the sandbox reports its content
  height (`hearthloom-height` postMessage; hearth.js auto-observes) and
  the shell sizes the iframe to it, so there's no inner scrollbar; the
  page scrolls in the dashboard's own flow.
- **Ember → host version dependency** (npm `engines` pattern): manifest
  `requires: { host: ">=X.Y.Z" }`. Install is refused when the running
  hearthloom is older; the gallery and `ember install` say "needs
  hearthloom ≥ X — upgrade first", and `ember install --upgrade-host`
  upgrades the host then proceeds.
- **Host self-update**: `GET /v1/version` (reads the release channel in
  the hearthlaunch well-known doc, cached 1h) and `POST /v1/upgrade`
  (downloads the platform build, sha256-verifies, atomically replaces
  the binary, restarts via launchd; refused on hosted nodes). CLI
  `hearthloom upgrade [--check]`. Degrades cleanly until the channel is
  published (shows "up to date").
- **Hamburger version + upgrade UI**: the ☰ menu shows "New version
  (X) available →" (with a badge dot) or "✓ running the most recent
  version" under the record counter; the link opens a `#/upgrade` view
  (current vs latest, changelog, one-click self-update).

### Changed
- **Treemap labels** (chart-svg): per-tile font sizing + ellipsis
  truncation to fit each tile, with a much lower size floor — far more
  tiles are labeled without a mouseover (the instant tooltip still
  covers the tiniest ones).

## 0.5.24 — 2026-07-22

### Added
- **Declarative ember configuration**: manifest `config[]` ({key,
  label, type: string|number|boolean|list, default?, help?}) renders as
  a "configure…" form on the ember's Integrations card and saves into
  the ember's own `config` KV (schema-checked — the form can't be
  repurposed as arbitrary KV writes; mirror-sync carries settings to
  the peer). Embers just read KV; no settings UI to build.
  hpresence 0.6.0 is the first user: sampling interval, ignored apps,
  tracked browsers, and a never-record domain exclusion list.

## 0.5.23 — 2026-07-22

### Added
- **`ui.card[]` extension point**: embers can put links on their own
  Integrations card ("View presence data →") — the right home for a
  destination that shouldn't crowd the top nav or Visualize. Card
  pages participate in the dial capability like every other entry.

## 0.5.22 — 2026-07-22

### Added
- **chart-svg `treemap` type** (`nameKey` + `valueKey`, squarified row
  packing, labels only where they fit, share-of-total tooltips) — the
  Ask RLM and ember pages can both emit it.
- plugin-guide: embers using `/assets/chart-svg.js` must style the
  chart text classes locally (the dashboard stylesheet never loads in
  the sandbox; unstyled SVG text renders black — the hpresence label
  bug).

## 0.5.21 — 2026-07-22

### Fixed
- **Dark heatmaps in ember pages**: chart-svg's heatmap paints cells as
  `--chart-heat` at value-scaled opacity, and that variable was
  missing from the sandbox palette handoff — unset SVG fill is BLACK,
  so the Presence heatmap rendered as a near-opaque dark sheet. The
  handoff now carries every chart variable including `--chart-heat`
  (app.js pages and Desk widgets both).

## 0.5.20 — 2026-07-21

### Added
- **The Dial for embers**: a Visualize/nav/hamburger ui entry may
  declare `"dial": true` — the shell mounts the REAL Dial above the
  sandboxed iframe and streams `{type:"hearthloom-timespan", from, to}`
  in on every settle (plus once at load). Embers get core-quality time
  scoping without the sandbox ever loading core code. The palette
  handoff now carries `--chart-1..8`, so ember pages can load the
  pre-auth `/assets/chart-svg.js` and render house-themed charts.
- Cross-project references scrubbed from source/comments (chart-svg,
  rlm, a placeholder, a doc-comment name).

## 0.5.19 — 2026-07-21

### Added
- Manifest `runner: "single" | "per-node"` (additive): `single`
  (default) keeps the single-runner lease for mirror-sync'd sidecars;
  `per-node` opts out for embers whose duplicates are MEANINGFUL —
  presence samplers being the canonical case (each machine's presence
  is its own fact). hpresence 0.2.0 uses it with host-scoped state and
  per-(machine, day) episode refs, making multi-computer setups safe.

## 0.5.18 — 2026-07-21

### Added
- **Version visibility**: the ☰ episode counter now reads
  "N episodes · vX.Y.Z"; the Peering page shows what version the peer
  runs (carried on the heartbeat both directions, additively).
- **Peer-ember awareness**: Integrations → embers shows an "on your
  other node" section — embers installed on the counterpart render as
  informational cards ("running there", why they live there by
  placement, "its data syncs here either way" for mirrored ones) with
  NO update/manage actions, so a cloud node can't be asked to touch a
  local-only ember and the user isn't left guessing. New peer-scope
  `GET /v1/sync/plugins/installed` + session `GET /v1/plugins/peer`
  proxy (60s cache; degrades honestly when the peer is unreachable or
  older).
- Gallery cards no longer show a "reviewed" chip derived from
  cloud-eligibility (wrong signal); review status still shows at
  install time from the verified counter-signature.

## 0.5.17 — 2026-07-21

### Fixed
- **The "isn't fully set up yet" banner on a 91k-episode record**: the
  onboarding hero judged freshness before the cold post-restart
  /v1/stats returned, read the unknown count as zero, and never
  re-checked. It now only renders once stats has loaded, and re-judges
  when the counts arrive.
- **Counter-signature verification vs served timestamps**: a registry
  may sign `approvedAt` at RFC3339 second precision but serve it with
  fractional seconds, so verbatim payload reconstruction failed on the
  first real registry install. Both clients (daemon registry_client +
  CLI) now verify against the served string AND the seconds-normalized
  form.
- First real publish completed end-to-end: hpresence 0.1.0 published to
  a registry (reviewed + counter-signed) and installed via `ember
  install hpresence` — keygen → publish → approve → search → verified
  install, all proven.

## 0.5.16 — 2026-07-21

### Added
- **The ember gallery** (Integrations → embers, now the LAST section,
  beneath imports): plain-language installed cards ("can see", on/off,
  "running here / on your always-on node", update-available badges) and
  an "available from the hearth" registry browse with search and
  one-click add — scope approval stays a deliberate human step. The
  folder/zip sideload flow is demoted to an "install one you built
  yourself" details block.
- **Daemon-side registry client** (`registry_client.rs`): the spec-§5
  verify chain (sha256 + Ed25519 + counter-sign) now lives in the
  daemon behind `GET /v1/plugins/registry/index` (60s cache;
  `context=cloud` auto-appended on hosted nodes) and
  `POST /v1/plugins/registry/stage`. Cloud policy is ENFORCED at stage
  time: hosted instances refuse non-eligible, non-counter-signed, and
  placement-local embers.
- **Placement + the single-runner lease**: manifest `placement:
  local|cloud|any` (localOnly = legacy alias for local); a mirror-
  sync'd sidecar ember runs in ONE place across a pair — the running
  node heartbeats a reserved `__runner` key in the ember's mirrored KV
  (60s tick), the peer stands down on a fresh foreign lease and takes
  over when it goes stale (~10 min). Plugin self-storage rejects
  `__`-prefixed keys. No migration: the lease rides plugin_kv and the
  existing sync leg.
- CLI: with `--url`, `ember install/update` stage on the TARGET node
  (its policy, its verification), then approve scopes locally — the
  widening guard applies there too.

## 0.5.15 — 2026-07-21

### Added
- **`ember update [name] [--check] [--yes]`**: checks installed embers
  against the registry (semver-aware) and updates outdated ones through
  the full verification chain. Guards: an installed version the
  registry never published is a LOCAL build — reported and skipped,
  never force-matched by name; and an update requesting scopes beyond
  the current grant always prompts with the delta, even under `--yes`
  (scope widening never rides an unattended update). Origin-pinning
  (recording which registry supplied a package) noted as
  the follow-up.

## 0.5.14 — 2026-07-21

### Added
- **Ember registry commands**: `ember search/info/install/uninstall`
  against the LIVE hearthlaunch registry by default (`--registry <url>`
  for any registry serving /.well-known/hearth-registry; feature-detect
  degrades to `<url>/registry/v1`). Install implements the registry
  spec's §5 client contract: resolve → download → sha256 verify →
  publisher Ed25519 release-signature verify → registry counter-sig
  verify when present ("registry-reviewed") → show scopes and ask
  (`--yes` for scripts) → stage through the daemon's existing
  upload+install path. The human scope gate survives the CLI.
- `docs/cli.md` — the full command reference, one example + explanation
  per command.

### Changed
- `hearthloom --help` lists commands alphabetically.

## 0.5.13 — 2026-07-21

### Added
- **The CLI operator + capture layer** (ops.rs; README "The CLI"):
  `auth login/status/logout` — password sign-in mints a CLI-owned admin
  PAT (`cli:<host>`, login-keychain-stored, auto-sent thereafter),
  un-stranding the CLI on auth-locked nodes; global `--url` to target
  any owned node (remote never auto-spawns — bench's missing --url
  falls out of this) and `--json`; `status` (one screen: version,
  record, vault, thinking incl. delegation, peer); `doctor` (the
  debugging playbook as checks: orphan daemons, launchd target, version
  skew, auth reach, model, WAL bloat, PATH); `task add` + `note
  new/list` ($EDITOR) capture; `key mint/list/revoke`; `vault
  status/unlock/lock` + `export` (polls, downloads the zip); `sync
  [--now [--full]]`; `ember validate/pack/list/logs` (dev tools — the
  validator IS the daemon's parser; installs remain a dashboard human
  act); `notify`; `logs -f` (ANSI-stripped); `completions
  bash|zsh|fish`.
- `scripts/install.sh` now ensures `~/.local/bin` is on PATH
  (idempotent profile append).

## 0.5.12 — 2026-07-20

### Changed
- **Plugins are branded "embers"** across the dashboard and the
  developer docs; the manifest's canonical name is `hearthember.json`
  (`hearthplugin.json` still accepted). Wire and disk identifiers
  (`/v1/plugins`, the plugins tables, `~/.hearthloom/plugins/`) are
  unchanged and frozen — brand on the surface, stable protocol
  underneath (the Memories/#browse precedent).

## 0.5.11 — 2026-07-20

### Added
- **The plugin architecture, shipped** (design docs/plugins.md
  2026-07-16; approved plan executed in full). A plugin is a sidecar
  process with a scoped PAT — never in-process code:
  - v42: `plugins` registry + `plugin_kv`; package format =
    `hearthplugin.json` + `bin/<os>` + `ui/` (dir or zip); local
    install with human scope approval on Integrations → plugins. NO
    central registry, NO CLI installer — distribution is deferred to
    the provisioner against this package format.
  - THE PLUGIN CEILING: plugin-bound keys are default-denied down to a
    scope-mapped route table (read/ingest/notify only; /v1/ingest
    re-stamps the actor to `plugin:<name>`).
  - Daemon-mediated self-storage (`/v1/plugins/self/kv|files`) under
    `~/.hearthloom/plugins/<name>/`, exported minus each plugin's key
    file; `sync: mirror` plugins replicate KV (LWW) + data files
    (fingerprint, newer-mtime-wins) over the new `/v1/sync/plugins`
    peering leg.
  - Manifest-declared UI: one top-nav entry, Visualize/hamburger items,
    Desk widgets — all rendered as sandboxed iframes with a 12h
    plugin-scoped token handed via postMessage; credential-less CORS
    makes opaque-origin fetches work without touching session auth.
  - Reference plugin: **[hpresence](https://github.com/metavert-io/hpresence)** (its own repo — the
    exact third-party path), the presence sampler: frontmost app +
    idle only, one `presence_day` episode per day, ingest-scope-only,
    all four UI hooks. E2E-verified on a scratch daemon: install →
    launchd sidecar sampling → scope 403s → forged-actor re-stamping →
    uninstall (unit removed, key revoked, data kept).

## 0.5.10 — 2026-07-20

### Added
- **Interpretation failures surface on the Integrations cards** (Jon:
  "if they were classed as failed, we should capture and report those
  errors"). A poison batch (unparseable LLM answer) used to mark its
  episodes `extracted=2` and they silently left the "N to go" count —
  never interpreted, never retried, invisible. Now: the failure reason
  is captured per source (`extract.<source>.last_error`), the card
  shows "⚠ N couldn't be interpreted — <why> · try again", and the
  retry link (`POST /v1/integrations/extraction/retry-failed`) puts
  them back in the queue. `/v1/integrations/imports` entries gain
  additive `failed` + `error` fields (and appear even when pending=0
  so failures can't hide behind a finished-looking card).
- Known limit: failure marks are node-local — a
  delegating laptop doesn't yet see failures that happened on its
  always-on peer; replicating failure state through the verdict sync
  is the follow-up.

## 0.5.9 — 2026-07-19

### Fixed
- **The interpreting card tells the truth now** (Jon watched "611 to
  go, ~7m left" sit frozen for hours): a keyed local node that has
  DELEGATED its LLM passes to the always-on peer no longer shows a
  local-throughput forecast — the card says "interpreting on your
  always-on node — N to go (progress lands here as it syncs back)",
  because a delegating node's backlog drains at the peer's pace, in
  jumps, as extraction verdicts replicate. `/v1/integrations/imports`
  gains an additive `llm_delegated` field.
- **The 🧠 status lines update live**: the 5s imports poll only
  refreshed the run-button labels; the "N to go" paragraphs were
  painted once at mount, so the counter only moved on a page reload.
  They now update in place from every poll.

## 0.5.8 — 2026-07-19

### Fixed
- **The /v1/stats starvation** (the fourth layer of the tenant onion,
  found by symbolized live stack traces): the masthead counter polls
  /v1/stats from every open dashboard tab; the handler ran eight
  whole-table aggregates synchronously on a runtime worker, and
  `top_entities`' vault guard — a correlated `NOT EXISTS` probing the
  full episodes table once per episode_entities row — took 36s+ per
  call once the tenant's 2GB record outgrew its 1GB page cache. A
  train of uncancellable stats calls held the store lock nearly
  continuously: the laptop's push decayed 18k → 135 episodes/hour and
  the node 502'd for four hours while `/health` starved (three of four
  workers eaten: the grinder, a gated waiter, and the monitor tick).
  Fixes, each independently sufficient:
  - **v41 migration**: `idx_episodes_private_id` partial index; both
    vault guards (top_entities, dossier count) rewritten to
    `episode_id NOT IN (SELECT id FROM episodes WHERE scope =
    'private')` — one indexed read of the small private set instead of
    219k random probes.
  - **/v1/stats is cached** (60s, keyed by vault state) and computed
    via `spawn_blocking` — a slow stats can no longer park a runtime
    worker, and a polling tab costs one query per minute.
  - **The monitor loop** (60s ticks) does all store reads/writes and
    its `ps` shell-out through `spawn_blocking` — it was the third
    parked worker in every wedge.

## 0.5.7 — 2026-07-19

### Fixed
- **v40 backlog partial indexes**: the scope-classification, extraction,
  and embedding-backfill candidate queries full-scanned episodes/chunks
  per call — on the freshly-synced tenant each call read the whole
  1.5GB db under the store lock (PassClock caught `run_intents:
  630632ms` and multi-GB reads in classify/extract). All backlog
  lookups are now O(backlog), not O(record).
- **The store gate**: an outermost middleware caps concurrent
  store-touching requests at 2 (streams and /health exempt). Store
  handlers block runtime workers on the lock inline, so with 4 workers,
  4 concurrent requests parked the whole runtime — the IO driver went
  unpolled and even /health stopped answering. The excess now queues
  asynchronously, off the workers; the node answers /health during ANY
  grind.

## 0.5.6 — 2026-07-19

### Fixed
- **The store mutex is FAIR** (parking_lot FairMutex): 0.5.5's
  well-behaved passes still starved the node — the unfair std mutex let
  the grinding pass re-win the lock on every release, so the four
  runtime workers stayed parked in handler tasks and the accept backlog
  never drained (verified live on the tenant: listener bound, ~35
  connections queued, zero served for 20+ minutes, loopback included).
  FIFO handoff hands each release to the longest waiter — handlers get
  through between every consolidation batch. Fairness bounds ordering,
  not hold length; the small-batch rule stands.

## 0.5.5 — 2026-07-19

### Fixed
- **Consolidation survives a bulk-sync backlog**: `run_intents` chunks
  its watermark IN-lists (≤500 ids) — a 48k-episode sync delivery blew
  SQLite's 32766-bound-variable cap and aborted EVERY consolidation
  cycle on the receiving tenant, which therefore never finished its
  boot grind and never served. A failing intents pass now skips, never
  kills the cycle.
- **THE PASS CONTRACT (1-vCPU driver starvation, the real fix)**: every
  consolidation pass is either spawn_blocking'd whole (intents, sweeps,
  opportunity scan, note settling, workout metrics, me-detector, brain
  index) or awaits naturally; long lock holds batch small with 40ms
  breaths (location backfill now 250-episode laps instead of one
  5000-body hold). A single long store-lock hold parked every runtime
  worker behind the unfair mutex, the tokio IO driver went unpolled,
  and the node 502'd while "working" — worker_threads=4 (0.5.4) was
  necessary but not sufficient.

### Added
- **Per-pass instrumentation**: each consolidation pass logs its
  elapsed wall time (`consolidation pass <name>: <N>ms`) — a grinding
  node names its hog in one cycle's log.

## 0.5.4 — 2026-07-18 (tag only)

- Fix: 4 runtime workers regardless of core count — 1-vCPU tenants
  starved the accept loop into 502s during consolidation (see 0.5.5).
- Sync: 60s failsafe retry after a failed cycle — interrupted pushes
  resume without waiting out the 5-min tick.

## 0.5.3 — 2026-07-18 (tag only)

- Sync: heartbeat at cycle start + every 25 push batches — the
  counterpart's inbound card stays live during long pushes.

## 0.5.2 — 2026-07-18 (tag only)

- Peering: re-offer-everything as a real button behind a styled confirm.

## 0.5.1 — 2026-07-18

### Changed
- **Export always survives the killswitch** (supersedes allow_export
  gating): a restricted node's screen carries unlock-vault → prepare →
  download, so a user in a grace period can always take their data.
  `/v1/vault` joins the killswitch allowlist for exactly this path.
- **Streaming restore upload**: `/v1/import/restore` streams the zip to
  disk chunk-by-chunk with no body-size cap — multi-GB exports/imports
  are the expected norm, never buffered in memory. (Export already
  streams both directions: incremental zip build, streamed download.)

## 0.5.0 — 2026-07-18

### Added
- **Full export** (Privacy page; vault must be UNLOCKED): one zip that
  rebuilds the whole system on a fresh node regardless of which node
  made it — consistent db snapshot (sessions stripped, owner's login
  preserved), readable tables/*.json hierarchy, and every referenced
  file. Background job with live progress + ETA and a download button.
- **Restore** (onboarding wizard, fresh nodes only): upload an export
  zip → files + db staged → daemon restarts through the boot-time swap;
  sign in with the exported record's credentials.
- **THE classification contract** (CLAUDE.md "Full export"):
  build-failing tests force every table (core TABLE_CLASSES) and every
  home-root dir (daemon DIR_CLASSES) to be classified Exported or
  Excluded-with-reason; unknown dirs export by safe default with a
  manifest warning. New integrations can't dodge the exporter.
- Killswitch honors `allow_export` on `/v1/export/*` (the provisioner
  contract lands).

## 0.4.0 — 2026-07-17

Extraction verdicts (v39; design locked in the design log 2026-07-17).

### Added
- **Verdict ledger**: every LLM extraction result is recorded per episode
  (entities, tier, model) and links REGENERATE from verdicts — the
  replication channel link-shaped outputs never had.
- **Verdict replication**: `/v1/sync/extractions` exchange + the
  `extraction.applied` bus topic — interpretation lands on BOTH nodes,
  and applying a peer's verdict marks the episode extracted, so only one
  node ever spends per episode (fixes the two-keyed double-spend).
- **Tier re-runs**: "re-interpret at high quality" per source — new
  verdicts supersede weaker ones as a diff (stale links removed, never
  piled up); `POST /v1/integrations/extraction/rerun`.
- **Tri-class LLM resilience**: transient errors retry with backoff and
  auto-resume via the pending pool; unparseable batches are marked
  failed and never block the queue; billing/credential failures STOP all
  spend, raise a critical bell notification, and pause visibly — resume
  is explicit (AI provider card button or key re-save).
- **Fleet visibility**: `GET /v1/provisioner/status` gains an additive
  `degraded` array ({component, class, since, detail}) — llm-blocked and
  sync-failure-streak first; distinct from the killswitch fence.

## 0.3.3 — 2026-07-17

### Added
- **Onboarding completion loop**: leaving #/welcome mid-setup gets one
  gentle confirm modal; an abandoned wizard shows a dismissable reminder
  hero on every page ("stop reminding me" persists server-side); Settings
  gains **New User Mode** (re-enables the wizard + banner, offers to jump
  to the welcome screen) and a "who you are" identity-claim section — the
  onboarding data not captured elsewhere. State lives at
  GET/POST /v1/onboarding ("" | active | done | dismissed); established
  records (>1000 episodes) are grandfathered out of the fresh-record nag.
- **Integrations polish**: Claude.ai and ChatGPT are separate cards;
  "found in Downloads" overflow collapses into a scrollable review modal
  (max 2 inline rows); cards sort alphabetically.

## 0.3.2 — 2026-07-17

The two node-side launch gates for the provisioner (its 2026-07-17 asks):

### Fixed
- `/v1/provisioner/*` is exempt from the session gate (it
  self-authenticates via X-Provisioner-Token) — on auth-locked tenants
  the control plane answered 401 "sign in required" before the
  control-token verifier ever ran.

### Added
- Browser login handoff: `POST /v1/auth/login` now accepts a
  form-encoded body (top-level form POST of `issuer_token`) and, for
  form/HTML requests, sets the session cookie and 302-redirects to `/` —
  "open my brain" lands the user in their app. JSON callers keep the
  JSON contract.

## 0.3.1 — 2026-07-16 (tag only)

Additive fleet-health fields on `GET /v1/provisioner/status`
(db_bytes, unread_notifications, auth_locked, last_peer_sync,
llm_configured) + `scripts/build-tenant-image.sh` (tenant images build
from release tags only). First tenant Docker image.

## 0.3.0 — 2026-07-16

Notifications + the provisioner control plane (design locked in
the design log 2026-07-16; reviewed by Jon, all six decisions resolved).

### Added
- **Notification queue** (v38): masthead 🔔 + panel; hints never data;
  upsert by (origin, dedupe_key); internal producers (peering paired /
  first sync, friend link, LLM key adopted, archive imports done,
  sync-failure streak, killswitch transitions); `POST /v1/notifications`
  for plugins behind the new `notify` PAT scope; structural link
  allowlist (own node / paired peer / provisioner-registered domains —
  reject, never strip); two-way peer replication (telemetry pattern,
  LWW read/dismiss) + `notification.created` bus topic.
- **Provisioner control plane** (structurally optional — exists only when
  HEARTHLOOM_PROVISIONER_SECRET is set at the machine level): HMAC
  control tokens (purpose=control, exp ≤300s, single-use jti) verified
  above and independent of ALL API keys, admin included; routes
  killswitch / notify / link-domains / status.
- **Killswitch**: the surface shrinks to notifications, key list+revoke,
  auth, the shell, /health, and the control plane; everything else — UI
  and API — answers HTTP 402 carrying the activator message + link;
  dedicated dashboard screen; provisioner-settable allow_export (node
  default true; the provisioner policy sets false).

## 0.2.0 — 2026-07-16

The audit-driven batch.

### Added
- **Markdown vault importer** (Obsidian / Logseq / Reflect Open):
  `.md` folders become live notes — wiki-links, frontmatter tags/dates,
  images preserved; vault read in place, upsert on re-import. Dual-verb
  Integrations card: import as notes, or **mount as virtual source** and
  stay in your editor.
- **Rewind rescue adapter**: the remote-killed lifelogger's stranded
  SQLite → screen-time day episodes (top apps as entities) + recorded
  conversation transcripts. Schema-drift-tolerant; encrypted installs get
  exact sqlcipher export instructions.
- **`hearthloom bench`**: P50/P95 latency over the live HTTP query surface,
  seeded from the record's own entities — the regression guard on
  "retrieval is the product" (and it immediately flagged hybrid search's
  vector leg as the next scaling item).
- **Agent access card** (Integrations) + `docs/agent-surface.md`: the
  read-only-by-construction contract, made visible — keys and scopes,
  friend-key reads, pending agent proposals.
- **Plugin architecture design** (`docs/plugins.md`, design locked):
  sidecar processes over scoped PATs; two-tier index (reviewed+signed
  central via the provisioner / caveat-emptor self-install).
- **First-run wizard grown up** (`#/welcome`): identity claims first (name
  + scoped emails — the step that makes every source resolve to one
  human), instant local kindling (AI coding sessions, browsing history,
  a git repo — no permissions, search works in minutes), then provider /
  peering / the big imports, with a "show me my own last week"
  first-value moment.
- This changelog, semver discipline, and tagged releases.

## 0.1.0 — 2026-07-07 … 2026-07-15 (retrospective)

- **The substrate**: episode-sourced bi-temporal knowledge graph in one
  SQLite file; append-only episodes, reversible merges, provenance
  everywhere; 37 schema migrations in nine days.
- **Ingestion**: 13 adapters (git, ICS, mbox w/ Gmail label strata,
  Claude/ChatGPT logs, Claude Code, iMessage, Apple Photos, Chrome,
  Substack, Apple Health, Facebook, LinkedIn, ChatGPT full export) +
  Notion importer, inbox drop folder, Downloads auto-detect, upload on
  every card.
- **Retrieval**: hybrid FTS+vector (local ONNX), temporal recall,
  capped traversal; the RLM deep-query engine (Ask) with multi-turn
  threads, inline charts, action prefills.
- **The dashboard**: Desk (tasks/notes/goals), Relationships, Memories +
  the Dial, Day, Search, Galaxy/Constellation, Atlas, Loom, Pulse,
  Biography, Ribbons, Tend, Rewind reel — embedded, offline, no CDN.
- **Privacy**: the vault (locked by default, zero leakage), privacy
  rules, scope guard on every read surface, sealed secrets.
- **Cognition**: idle-time consolidation (digests, chapters, extraction,
  classification, cascades, self-model, opportunities, merge candidates),
  multi-provider LLM (Anthropic/OpenAI) with per-tier models.
- **Two nodes**: login layer, peering (episodes both ways, operational
  mirror, scope LWW, telemetry exchange, LLM key-sync via sealed box),
  the push bus (notify-then-fetch SSE event hints), virtual sources
  (HearthFS mounts, mbox archives, health vquery), federation groundwork
  (friend keys, grants, /brain manifest).
- **Agent surfaces**: 4-tool MCP, scoped PATs, BRAIN.md, SKILL.md,
  change feed.

# Source import runbook

Every import here is **repeatable, not one-time**: episodes are
idempotent per `(source, source_ref)` — re-running an ingest adds only
what's new and never duplicates. This file is the operator's manual for
first imports AND re-imports (a fresh Facebook dump, a newer Takeout).

General rules:
- The daemon is the sole writer — every path below goes through
  `hearthloom` (CLI) or the HTTP API on 127.0.0.1:43278. Big archives
  run **detached daemon-side** (progress in `~/.hearthloom/daemon.log`,
  completion as a `*_ingest_done` mutations event).
- TCC-gated sources (iMessage, Photos) need Full Disk Access on the
  hearthloom binary, not your shell — the CLI falls back to
  `/v1/ingest/local` automatically.
- Scope can be corrected later, in bulk:
  `hearthloom scope --by-source <source> <label>`.
- The inbox drop folder `~/.hearthloom/inbox/` accepts zips, .tgz, .mbox
  (≤250MB), .ics, conversations.json, chat.db, Photos.sqlite — dragged
  files ingest automatically. Archives unpack only files and folders
  (links and special files are left out) and only as far as the disk can
  spare while keeping 1 GB free. A recognised export (ChatGPT, Facebook,
  LinkedIn, Notion, Apple Health) is staged into `archives/<key>` for its
  card's run-now — refused while that import is running; drop it again
  when it has finished.
- Importing a ChatGPT export again (either route) moves a conversation
  now flagged "do not remember" into the vault, even if it was stored
  before outside it — also when the branch it ended on has no turns left
  to import; so does any import you run with `--scope private` for the
  records it reads. Only the label changes.

## Facebook (DYI archive)
1. facebook.com → Settings → Download Your Information → **JSON**
   format, all of "Your Facebook activity" + "Connections" +
   "Personal information". May arrive as several zips (media overflow).
2. Extract ALL zips into `~/.hearthloom/archives/facebook/` (must
   contain `your_facebook_activity/`). Zips can be deleted after.
3. `curl -X POST localhost:43278/v1/ingest/local -H 'Content-Type: application/json' \
      -d '{"actor":"human:<you>","adapter":"facebook"}'`
   → detached; parses posts/messages/events/groups/albums/profile,
   loads the friends list into the social_connections REGISTRY (friends
   never become entities without interaction or an explicit click).
4. Optional Tier-2: register the raw archive as a virtual source so the
   RLM can search what didn't materialize:
   `POST /v1/sources/virtual {"name":"facebook-archive","adapter":"fs",
    "config":{"mount":"~/.hearthloom/archives/facebook","magic":false,"trust":"imported"}}`
5. Re-import: replace the extracted directory with the newer dump and
   re-run step 3. Old episodes stay; new material lands; the registry
   upserts (new friends added, removals stamped).

## LinkedIn (data export)
1. linkedin.com → Settings → Data privacy → Get a copy of your data.
   The "Basic" archive arrives fast; the FULL export (messages, shares)
   follows within ~a day — both extract into the same place and the
   adapter absorbs whichever files are present.
2. Extract into `~/.hearthloom/archives/linkedin/` (must contain
   `Connections.csv` or `Profile.csv`).
3. `curl -X POST localhost:43278/v1/ingest/local -H 'Content-Type: application/json' \
      -d '{"actor":"human:<you>","adapter":"linkedin"}'`
   → detached; career positions/education/honors/publications/articles/
   events become dated `li_*` episodes, profile emails become identity
   attributes, and the ~30k connections land in the REGISTRY with
   company/position/email per row (never entities without interaction
   or an explicit click).
4. Re-import: replace the directory with the newer export, re-run.

## Gmail / mail (Google Takeout mbox)
- Tier-1 (materialize the core): the big Takeout mbox NEVER goes through
  the drop folder — run the filtered ingest daemon-side:
  `hearthloom ingest mbox <file> --include-labels Sent,Personal,Starred`
- Tier-2 (virtualize everything): keep the archive at
  `~/.hearthloom/archives/`, register as an `mbox` virtual source — a
  sidecar index (`<archive>.vidx.sqlite`) is built in the background and
  rebuilt automatically when the file changes.
- Re-import: a newer Takeout replaces the file; Tier-1 re-run is
  idempotent, the Tier-2 sidecar rebuilds on fingerprint change.

## Apple Health
1. Health app → profile → Export All Health Data → move the zip's
   `export.xml` (or the folder) under `~/.hearthloom/archives/`.
2. `POST /v1/ingest/local {"adapter":"apple-health","path":"…/export.xml"}`
   → detached streaming scan: `health_day` aggregates + workouts + ECG +
   clinical episodes, and the numeric sidecar (`health.vidx.sqlite`)
   behind the `apple-health` vquery handle.
3. Re-import: newer export replaces the file, re-run — day aggregates
   and workouts dedupe by source_ref; the sidecar rebuilds.
4. Units: each metric's day rollups are in one unit, named in
   `daily.unit` (active energy kcal, exercise min, HRV ms; mass,
   distance and glucose in the export's own unit). A sidecar an earlier
   version built is brought up to date from its samples the first time
   it's read (a minute's wait); a re-import rebuilds it whole, which
   also re-reads device names (only "Apple Watch" names count as the
   watch; an app like SleepWatch is a tracker of its own).
5. Sleep: a night is one device's record — two trackers, or two
   watches, on the same night never add up. The sidecar keeps sleep
   stages per device (`daily.source` names the device for those rows).
   Pulse's "today so far" point comes from the live device stream in
   the unit the history is in (`/v1/health/today?unit.BodyMass=lb`).

## iMessage / Photos / Chrome / Claude Code (local, TCC)
`POST /v1/ingest/local {"adapter":"imessage"|"photos"|"chrome"|"claude-code"}`
(defaults find the library paths). Re-run any time — only new
messages/photos/visits/sessions land. Each iMessage chat-day and Chrome
day is one episode. Messages are imported only when you ask (here, the
CLI, or a `chat.db` dropped in the inbox), so every import reads every
day, today's included. A Chrome day is read once it has settled (a whole
day after it ends, UTC), and a Claude Code session once its file has gone
12 hours unchanged; today's browsing and a session still in use land on
a later run (the idle refresh reads both). Each read is stored as
`<ref>#r<n>` (`n` = messages, visits or turns it holds — every one, even
when the text is too long to keep whole and shows only the first and
latest); days stored by earlier versions keep their plain `<ref>`. A
later read that finds more in a day or session already stored (messages
sent since, late-syncing messages or visits, a session picked up again)
stores the fuller copy, which replaces the earlier one and keeps its
label (a private day stays private — marking any copy private marks them
all, on every node); a read holding no more changes nothing. Two Macs
that read one chat-day with different numbers of messages store
different copies, and both show the fuller one once they sync. The idle
refresh reads Chrome from where each profile's last run stopped (less a
week, for late visits) and only the session files that changed since —
or that sit in a project folder it hasn't seen before. Chrome days are
keyed by profile (`profile:<creation time>#<date>`; a profile with no
creation time by this machine and its folder's path); a day earlier
versions stored under a profile's shown name is replaced by that
profile's day only when the new day holds all of it (at least as many
visits, and every visit line), so same-named profiles (on one Mac or
two) keep their own days. Photos keys each photo by capture time and
ORIGINAL file name, the identity the phone sends too; a photo an earlier
scan stored under the library's file name moves to that key on the next
scan, on every synced node — also a node that holds only the old key, and
also when two Macs' libraries gave the photo different names. Photos also feeds the
location index (GPS lines parsed at consolidation).

## Git / ICS / chatlog / Substack
- `hearthloom ingest git <repo-path>` per repo (registered in sources;
  refreshed automatically ≤1/20h).
- `.ics` files → drop folder, or `hearthloom ingest ics <file>`. A
  recurring event becomes one episode per occurrence (RRULE/RDATE,
  minus EXDATEs; an overridden occurrence comes from its override, and a
  RANGE=THISANDFUTURE override carries on to the later ones), at most
  3,660 per series. Only occurrences that have begun by the import are
  made — a series' first one and an override's included (an event that
  doesn't recur imports whatever its date): a stored episode keeps the
  text it first landed with, so a later
  move or cancellation of one still to come is picked up by importing
  the file again later. Cancelled events and occurrences are left out.
  Time zones: IANA names, Windows names (Outlook/Exchange) or the file's
  own VTIMEZONE; a rule's start at a time a daylight-saving change skips
  is no occurrence; an event that can't be read is skipped with a
  warning and the rest of the file imports.
- Claude/ChatGPT exports: `conversations.json` → drop folder
  (owner-aware via users.json).
- Substack: `hearthloom ingest substack <publication-url>` (archive
  API; works on custom domains). A post is kept only under the host
  that served the archive (after redirects); posts whose address names
  another host are left out, and when all of them do the import says
  which address to give instead.
- Commits and posts are **outside text** (`imported`): a repository
  holds every contributor's commits, and a publication address can name
  anyone's newsletter. Reading outside text closes Ask's way to your
  friends and makes agents ask before they act. Say which ones are yours
  ("this source is mine"): Settings → *sources that are yours*, the
  **This is mine** button on a repository's card or an opened commit or
  post, or `hearthloom mine add git <remote address>` /
  `hearthloom mine add substack <publication address>` (the vault must be
  unlocked; `hearthloom mine` lists, `hearthloom mine remove` takes one
  back). In a repository you mark, only commits whose author address is
  one of yours count — a connected mailbox or its aliases, or an address
  on your profile card (friends you share your profile with see the
  card's addresses). Even then merges, commits co-written with anyone
  else (a `Co-authored-by:` line, which Claude Code adds) and commits
  written or rewritten (an amend, a rebase) while one of your agents was
  at work stay outside text — and since a later rewrite hides when an
  earlier one was made, so does any commit with an agent's run between
  when it was first written and its last rewrite (a commit you rebase a
  week after writing it stays out if an agent ran that week). Git
  doesn't check author addresses: anyone
  whose commit gets into the repository can put yours on it, so mark
  only repositories whose history you control. While a repository is
  marked, adding an address of yours (on the profile card, as an alias,
  or by connecting a mailbox) needs the vault unlocked too, and address
  changes show in the list's history. A repository with no remote
  address can't be marked. Every post of a publication you mark counts,
  guest posts included. Commits and posts imported by a version before
  this setting existed stay outside text even when marked: their records
  don't say a commit's parents or when it was made, or which host served
  a post, and importing again doesn't replace a stored memory. The
  list shows how many stored memories each entry names — none means the
  entry changes nothing yet (a publication with its own domain files its
  posts under that domain). Sources whose items mix authors (iMessage,
  Chrome, Claude Code, chat exports, calendar invitations, scans, media)
  can't be marked.

## What re-imports do NOT do
- Never delete or rewrite existing episodes (append-only law).
- Never re-run LLM extraction on bulk sources (facebook, mbox,
  apple-health are outside the extraction allowlist — entities are
  minted structurally).
- Never auto-merge new name variants — Tend gets suggestions, you
  confirm.

## Notion (workspace export → Notes, not episodes)
1. Notion → Settings → Export all workspace content: format **Markdown
   & CSV**, include content **Everything** (images embedded in pages
   are PRESERVED — copied under ~/.hearthloom/notes/media/ and served
   vault-guarded; PDFs/video/audio still become placeholders), include
   subpages ON, create folders for subpages ON. Notion emails the zip.
2. Extract into `~/.hearthloom/archives/notion/` (inner Part-*.zips too).
3. `curl -X POST localhost:43278/v1/ingest/local -H 'Content-Type: application/json' \
      -d '{"actor":"human:<you>","adapter":"notion"}'`
   → pages become live NOTES (origin='notion'), folder path → tags,
   embedded images preserved + rendered in the editor, other media →
   `[media: name]` placeholders, databases ≤50 rows → one
   markdown-table note (bigger ones NAMED in daemon.log, never silently
   dropped). Idempotent by page-id: re-import updates changed pages,
   duplicates nothing.
4. Known loss: plain pages carry no timestamps in Notion's export —
   imported notes date from import time.
5. Privacy: vault the whole origin from the Privacy page ("notes" card),
   or one note via its editor's scope selector.

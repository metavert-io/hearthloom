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
  LinkedIn, Notion, Apple Health), at any depth of the archive, is staged
  into `archives/<key>` for its card's run-now — refused while that
  import is running; drop it again when it has finished. Each is
  recognised by its own files (a Notion export: an `Export-…` folder
  holding Notion pages), so another folder of that name is walked like
  any other.
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
- A memory holds the message's decoded text (up to 64 KB); mail with only
  an HTML part also keeps its links' and images' targets after the text (a
  `links:` block, up to another 64 KB — a target too long for the room
  left is left out but for its host, the ones after it still go in; the
  hosts have 64 KB of their own, and a message linking to more hosts than
  that is stored private). Text
  or targets past those limits are in the archive only. Every mail import
  (the Gmail pull and its label re-read, the inbox, `hearthloom ingest
  mbox`, `hearthloom repair mail`) checks the privacy rules over the whole
  decoded message, as the archive's readers do, so a rule matching only
  what was left out still makes the memory private. A copy of the message
  stored before keeps its label: its stored text doesn't reach what the
  rule matched, so it can't be told from other mail written under the same
  Message-ID (the sender's choice) — mark it private in Memories if it is
  that message. `hearthloom ingest mbox --scope private` does move a copy
  stored before into the vault, when it is the same message (the same
  date, sender and text; a copy an earlier version left encoded counts
  only once `hearthloom repair mail` has corrected it). A rule made after the import is
  checked against the stored text: it finds a left-out link's domain (the
  host is kept), not a word only past the cut.

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
   distance and glucose in the export's own unit — the first one the
   import can convert that measures what the metric does, so a reading in
   a unit it doesn't know never decides it; a metric it doesn't know a
   measure for, like steps in `count`, keeps its first unit; a bare
   `mmol/L` reads as glucose). Readings in a unit that
   can't be converted to the day's are left out, with a warning in the
   log. A sidecar an earlier version built is brought up to date from its
   samples the first time it's read (a minute's wait; Pulse asks again on
   the next view); a re-import rebuilds it whole, which also re-reads
   device names (a name saying "Apple Watch", or ending in "Watch" — a
   watch you renamed — is the watch; an app like SleepWatch is a tracker
   of its own).
5. Sleep: a night is one device's record — two trackers, or two
   watches, on the same night never add up. The preferred tracker's
   night counts; of two equally preferred (two watches), the longer one,
   as Pulse shows it. The sidecar keeps sleep stages per device
   (`daily.source` names the device for those rows).
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
visits, its first visit, and every visit line the new day doesn't leave
out — a long day keeps its first and latest visits; a day the earlier
version stored whole must show whole), so same-named
profiles (on one Mac or two) keep their own days; importing a profile as
private takes such a day into the vault too. Photos keys each photo by capture time and
ORIGINAL file name, the identity the phone sends too; a photo an earlier
scan stored under the library's file name moves to that key on the next
scan, on every synced node — also a node that holds only the old key, and
also when two Macs' libraries gave the photo different names. Photos also feeds the
location index (GPS lines parsed at consolidation).

## Git / ICS / chatlog / Substack
- `hearthloom ingest git <repo-path>` per repo (registered in sources;
  refreshed automatically ≤1/20h). A remote address is stored without
  a token or password saved in it (an ssh user name stays); commits an
  earlier version stored under the address with its credential aren't
  stored a second time (an import with `--scope private` moves those
  into the vault).
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
  An overridden occurrence keeps the ref earlier versions gave it
  (`<UID>#<RECURRENCE-ID as written>`, the first occurrence's too), so
  importing a calendar again adds no second copy — except a
  RECURRENCE-ID written as a wall-clock time in another zone than the
  series', which is written by its time in UTC so it can't take another
  occurrence's ref. Time zones: IANA names, else the file's own
  VTIMEZONE, else Windows names (Outlook/Exchange); a rule's start at a
  time a daylight-saving change skips is no occurrence; an event that
  can't be read is skipped with a warning, one missing its END:VEVENT
  ends where the next event begins, and the rest of the file imports.
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
  a post, and importing again doesn't replace a stored memory. Nor do the
  oldest commits read from a shallow clone (`git clone --depth …`): git
  doesn't show their parents, so whether one is a merge can't be told,
  and fetching more history later doesn't change the stored record.
  Hearthloom's own re-reading of known repositories leaves out a
  checkout one of your agents works in (its working folder inside the
  checkout, or the checkout inside it). An agent task that never
  finished counts as at work for at most a day and five minutes after
  its last activity when the agent runs in Claude Code — Hearthloom stops
  a run there at its time limit. A task sent to Hermes or OpenClaw, which
  run on their own, or to an agent Hearthloom no longer knows, counts as
  at work until it finishes. The
  list shows how many stored memories each entry names (private ones
  only while the vault is unlocked) — none means the
  entry changes nothing yet (a publication with its own domain files its
  posts under that domain). Sources whose items mix authors (iMessage,
  Chrome, Claude Code, chat exports, calendar invitations, scans, media)
  can't be marked.

## What re-imports do NOT do
- Never delete or rewrite existing episodes (append-only law). A memory
  an earlier version stored wrongly is fixed by `hearthloom repair`
  instead — a corrected copy takes its place (see `docs/cli.md`): mail
  left encoded (`repair mail`), an event holding its alarm's text
  (`repair calendar --file …`), a ChatGPT transcript with abandoned
  branches mixed in or a "do not remember" flag ignored (`repair chatgpt
  --export …`), an Apple Health day or workout read in the wrong units
  (`repair health --export …`), Chrome/iMessage/Claude Code days stored
  under an unversioned ref with a different text on each node (`repair
  copies`), and photo days counted with duplicate photos (`repair
  photo-days`).
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

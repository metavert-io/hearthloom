# The `hearthloom` CLI

Everything goes through the daemon's HTTP API — the same surface the
dashboard uses. The CLI never opens the database itself (design law #4).
If no local daemon is running, one is spawned on demand.

**Global flags** (valid on every command):

| flag | meaning |
|---|---|
| `--url <base>` | Target another node you own (e.g. `--url https://brain.example.com`). Remote nodes are never auto-spawned. Commands that read this machine's files (`ingest`, `logs`, `ember logs`) refuse it rather than quietly work on this node. |
| `--json` | Machine-readable output where supported (`status`, `key list`, the `ember` commands…). |

**Authentication.** On an auth-locked node, run `auth login` once: it
stores a CLI key in your login keychain, and every command sends it
automatically afterward. `HEARTHLOOM_API_KEY` in the environment
overrides the stored key.

Commands, alphabetically:

---

### `auth` — sign in to a node and keep a CLI key

```
hearthloom auth login                 # prompts email + password
hearthloom auth login --email a@b.c
hearthloom auth setup-code              # a node with no login yet: its one-time setup code
hearthloom auth status
hearthloom auth logout
hearthloom --url https://my-node.example.com auth login
```

`login` opens a session just long enough to mint an admin API key named
`cli:<hostname>`, stores it in the macOS login keychain (service
`hearthloom-cli`, keyed by the node's URL; a 0600 file fallback on
linux), and discards the session. `status` reports whether this shell's
credentials reach the node. `logout` revokes the key server-side and
forgets it locally. Keys are also visible/revocable in ☰ → API Keys.

### `bench` — query-path latency guard

```
hearthloom bench
```

Measures search/recall P50/P95 over the live HTTP surface — the
regression guard on "retrieval is the product". Local node only.

### `completions` — shell completions

```
hearthloom completions zsh > ~/.zfunc/_hearthloom
hearthloom completions bash > /usr/local/etc/bash_completion.d/hearthloom
```

Prints a completion script for `bash`, `zsh`, or `fish`.

### `consolidate` — run a consolidation cycle now

```
hearthloom consolidate
```

Triggers the idle-time cycle immediately (embeddings backfill, intents,
scope classification, extraction, BRAIN.md refresh) instead of waiting
for the next tick. Spends LLM tokens if a provider is configured.

### `daemon` — run or inspect the daemon

```
hearthloom daemon run       # foreground (launchd runs exactly this)
hearthloom daemon status
```

`run` is the actual daemon process. `status` checks whether one answers.

### `doctor` — diagnostics

```
hearthloom doctor
```

Every wedge ever debugged, as executable checks: orphan daemons fighting
for the port, launchd pointing at the wrong binary, CLI/daemon version
skew, whether credentials reach past the auth door, embedding-model
presence, WAL bloat, and PATH. Run this first when anything seems off.

### `source text` — search inside a mail archive's messages

```
hearthloom node --archives keep            # this machine keeps your mail archives
hearthloom node --archives auto            # let Hearthloom choose (an always-on machine)
hearthloom source forget gmail-takeout --delete   # remove a copy that moved
hearthloom export --with-archives          # a full export that includes them
```

Archives move to the node that keeps them by themselves, through your
hub, resuming after any interruption; the machine they came from stops
offering its copy and tells you it can be deleted.

`hearthloom node` lists each node's mail archives under it (message
count, months, whether the text is searchable). Ask on any node can
search an archive another node holds; the lookup goes through your hub.

```
hearthloom source text gmail-live          # on or off, progress, size now / with text
hearthloom source text gmail-takeout on    # big archives (over 100,000 messages) wait for this
hearthloom source text gmail-takeout off   # stop, and give the space back
```

A mail archive's index covers sender, recipients, subject, labels and
date; with text on, it also covers each message's text and the web
addresses it links to, indexed in the background. About 9 KB per message
(measured on real Gmail); it won't start without three times that free.
Per node, `--json` for scripts.

### `calendar` — Google Calendar free/busy, for drafts that settle a time

```
hearthloom calendar setup        # your Google Cloud Desktop-app client: paste the JSON Google lets you download
                                 # (or drop the file in), or just the secret; checked with Google before it's saved
hearthloom calendar setup --file ~/Downloads/client_secret_….json
hearthloom calendar connect      # sign in with the account whose calendar you use (it suggests the Gmail this node
                                 # reads); finishes by itself when the browser is on this machine (or on a hosted node),
                                 # else paste the address the browser lands on
hearthloom calendar link https://calendly.com/you   # your scheduling page, offered in drafts beside free times
hearthloom calendar              # connected? which account, your working hours
hearthloom calendar hours --days mon,tue,wed,thu,fri --hours 09:00-18:00 --tz America/New_York --notice 12
hearthloom calendar calendars    # what the account can see; --calendars <id,id> on `hours` picks the busy ones
hearthloom calendar free --days 7 --minutes 30
hearthloom calendar disconnect   # forget the sign-in; the client stays
hearthloom calendar reset        # forget the client and the sign-in, then set up again
```

A node that uses a registry (Hearthlaunch) needs no `setup`: it signs in
with Hearthloom's shared Google client, and a hosted node signs in through
Hearthlaunch in one click. `setup` is for your own client, which wins.
`setup` reads the ID and secret out of the downloaded JSON (pasted whole,
a file dropped into the terminal, or `--file`), or takes the secret alone
and keeps the client ID saved on the node. It asks Google whether the
pair belongs together and saves nothing if Google says no. Delete the
downloaded file afterwards: it holds the secret in plain text.

Read-only: `calendar.freebusy` and `calendar.calendarlist.readonly` —
when you're busy and which calendars you have, never what the events are.
Publish your Google Cloud app ("In production"): in "Testing", Google's
sign-in expires after 7 days. Connect it on the machine that writes your
drafts (the one that watches Gmail).

### `mail` — what waits on you in your mail

```
hearthloom mail                             # threads that need a reply, a time or a look
hearthloom mail threads --state all
hearthloom mail done mt_1a2b…               # cleared
hearthloom mail not-needed mt_1a2b…         # nothing needed after all
hearthloom mail rules add domain mylawfirm.example --action needs_reply
hearthloom mail rules add plain "Anything from my accountant needs a reply"
hearthloom mail rules                       # list (rm <id> removes one)
hearthloom mail triage                      # how new mail is sorted; --off, --model haiku, --now
hearthloom mail draft mt_1a2b…              # a reply in your Gmail Drafts (never sent)
hearthloom mail draft mt_1a2b… --say "yes, but next week" --all   # your words; copy everyone
hearthloom mail drafts                      # the latest drafts, and which machine writes them
hearthloom mail rules add plain "Sign as Sam; keep replies short" --action draft
hearthloom mail embers                      # embers that watch your mail, and what they see
hearthloom mail embers --view headers       # every ember: sender, subject, date only
hearthloom mail embers --ember my-ember --view full      # one ember: the readable text
hearthloom mail embers --ember my-ember --view snippet --chars 600
hearthloom mail embers --ember my-ember --reset          # back to the setting for every ember
```

New mail from people becomes a thread; a model with no tools sorts each
new thread and must quote the words that decided it. It runs on the node
that watches Gmail: Claude Code (your subscription) on your own machine,
the node's AI key on a hosted one. Rules by sender, domain, subject or
list decide before any model; `plain` rules are instructions in your
words (`--action draft` ones guide reply drafts). Private mail never
reaches a model. A draft is written on the machine that watches Gmail and
saved in Gmail's Drafts folder over its IMAP connection — no Google
sign-in — and is never sent. `--json` everywhere.

### `agents accounts` — the Claude accounts agents run on

```
hearthloom agents accounts                        # each account: sign-in, usage, caps
hearthloom agents accounts set usual --cap-week 70  # agents may use 70% of the week
hearthloom agents accounts add work               # an extra subscription to pool
hearthloom agents accounts login work             # Claude Code's own sign-in, on this machine
hearthloom agents accounts switching on           # move on to the next at a limit (default off)
hearthloom agents accounts move work --to 1       # try it first
hearthloom agents accounts check work --usage     # read its usage now (a one-word run)
hearthloom agents accounts set work --models sonnet --off
hearthloom agents accounts fallback on            # no account can run → the API key (billed)
hearthloom agents accounts stop-at-cap on         # stop a run that crosses its cap
hearthloom agents accounts remove work --yes      # Claude Code signs it out here
```

The pool belongs to the machine the Claude Code agents run on: its
settings and usage readings stay on that node, and `login` runs there
(it refuses `--url`). `usual` names your usual sign-in. Caps are
percentages of Claude's 5-hour and weekly limits; Hearthloom stops five
points short of a cap below 100%, because a run spends while it runs.
Everything takes `--json` (the same object the Agents page reads), and
needs the vault open, like the rest of the agents' setup.

### `ember` — install, manage, develop

```
hearthloom ember search presence            # discover (your configured registry)
hearthloom ember info hpresence             # versions, scopes, review status
hearthloom ember install hpresence          # fetch → verify → approve scopes → install
hearthloom ember install ./my-ember         # a folder or .zip you built (unreviewed)
hearthloom ember install ./my-ember --grant read,notify --yes   # …from a script
hearthloom ember install hpresence --version 0.1.0 --yes
hearthloom ember install foo --registry https://other-registry.example
hearthloom ember install foo --upgrade-host   # upgrade hearthloom first if the ember needs it
hearthloom ember update                     # check ALL installed vs the registry, update outdated
hearthloom ember update hpresence --check   # report only
hearthloom ember uninstall hpresence
hearthloom ember list                       # installed on the node
hearthloom ember disable hpresence          # stop it; key, settings, data kept
hearthloom ember enable hpresence
hearthloom ember config hpresence           # its settings (manifest config[])
hearthloom ember config hpresence interval 15
hearthloom ember config hpresence interval --unset
hearthloom ember validate ./my-ember        # lint with the daemon's real parser
hearthloom ember pack ./my-ember            # validate + zip for publishing
hearthloom ember logs hpresence -f          # tail a sidecar's log
```

`search`/`info`/`install` talk to an ember registry — the one set in
`HEARTHLOOM_REGISTRY_URL`, or any URL serving `/.well-known/hearth-registry`
via `--registry` (there is no default; Hearthlaunch is one compatible
registry). `install` follows the registry spec's client contract: it
downloads the package, verifies the sha256 against the registry record,
verifies the publisher's Ed25519 release signature (and the registry's
counter-signature when present — shown as "registry-reviewed"), then
shows the scopes it will hold and asks before installing. `--yes` skips
the prompt for scripting. Installing stages through the daemon's normal
install path — identical on-disk result to a dashboard install.
`update` compares every installed
ember against the registry and re-runs the full verification chain on
anything outdated; two guards: a version the registry never published
is treated as a local build and skipped (never force-matched by name),
and an update requesting scopes beyond what you granted always prompts
with the delta — even under `--yes`. `uninstall` stops the sidecar,
revokes its key, and keeps its data folder. With `--url` against a
remote node, install/update are staged and verified BY that node — a
hosted instance's cloud policy (eligible + counter-signed only) is
enforced server-side no matter the client. `validate`/`pack` are the developer loop; `search --cloud`
limits results to embers approved for hosted nodes.

**From a folder or .zip** (anything with a `/`, starting with `.` or
`~`, or ending in `.zip`), `install` shows the dashboard's consent
screen for an unreviewed ember and asks. For scripts, `--yes` answers
and `--grant <scopes>` names what the ember may do; an upgrade keeps
what the installed version holds, so `--yes` alone covers it unless the
new version asks for more — then it installs without the extra, warns,
and prints the command that grants it. Exit codes: 0 done; 3 done with
scopes withheld; 4 nothing installed, approval missing; 5 refused by a
hosted node (registry-reviewed embers only); 1 anything else. With
`--json` the outcome is one object whose `status` names it
(`installed`, `upgraded`, `installed_scopes_withheld`,
`upgraded_scopes_withheld`, `needs_grant`, `needs_yes`,
`refused_hosted`, `declined`, `error`) and whose `rerun` holds the
command to run when something was withheld. Details:
[plugin-guide.md](plugin-guide.md#installing-from-the-command-line).

`config` values are checked against the ember's declared types (numbers;
`true`/`false`; comma-separated lists), on the node as well — the
dashboard's settings form meets the same check. `enable`, `disable`,
`config`, `list` and `uninstall` take `--json`; `uninstall` needs
`--yes` when no one is at a terminal.

### `entity` — an entity's dossier

```
hearthloom entity "Jane Smith"
```

Attributes, relations, and cited episodes for a person/org/project.

### `eval` — the personal eval harness

```
hearthloom eval                       # ~/.hearthloom/eval/personal-eval.yaml
hearthloom eval my-questions.yaml
```

Runs retrieval quality checks against your own record.

### `events` — the change feed

```
hearthloom events --after 12345
```

Mutations after a cursor — the append-only audit log, streamable.

### `export` — full export to a zip

```
hearthloom vault unlock && hearthloom export
hearthloom export ~/backups/brain.zip
```

Starts the full export (db snapshot + raw files + notes + config +
embers), polls until ready, downloads the zip. **Requires the vault
unlocked** — an export contains everything, private included; making
that a deliberate two-step is the point.

### `identity` — emails/handles → one human

```
hearthloom identity claim "Jane Smith" email jane@example.com --scope work
```

Binds an identity attribute to an entity; reads stay merge-group-aware.

### `index` — regenerate BRAIN.md

```
hearthloom index
```

Rewrites `~/.hearthloom/BRAIN.md` (byte-stable for unchanged contents).

### `ingest` — pull a source into the record

```
hearthloom ingest git ~/myrepo
hearthloom ingest mbox takeout.mbox --include-labels Sent,Starred
hearthloom ingest ics calendar.ics
```

Adapter-based ingestion (git, ics, mbox, chatlogs, iMessage, Photos,
and more — see `hearthloom ingest --help`). Falls back to daemon-side
ingestion when macOS TCC blocks the shell.

### `init` — create the home layout

```
hearthloom init
```

Creates `~/.hearthloom` (db, raw/, notes/, config). Idempotent.

### `intent` — standing queries

```
hearthloom intent add bike-search "used road bike, 56cm, under $800"
hearthloom intent list
hearthloom intent matches bike-search
hearthloom intent rm bike-search
```

Standing queries the consolidation cycle checks against new memories.

### `key` — API keys (PATs)

```
hearthloom key mint agent-x --scopes read,notify
hearthloom key list
hearthloom key revoke ak_1234
```

Mints scoped bearer tokens (the secret prints exactly once), lists all
keys with last-use, revokes by id. Scopes: read, write, mcp, peer,
friend, notify, ingest, admin.

### `logs` — the daemon log

```
hearthloom logs
hearthloom logs -f
```

Last 40 lines (ANSI-stripped); `-f` follows. Local node only.

### `note` — notes from the shell

```
hearthloom note new --title "Reading list"    # opens $EDITOR
hearthloom note list
```

Notes are markdown files you own; saving indexes them and they settle
into the record after 48 quiet hours.

### `notify` — ring the dashboard bell

```
hearthloom notify "backup finished" "23GB to the NAS"
```

Posts a notification (for scripts and cron jobs). The origin is stamped
from your key — scripts can't impersonate the system.

### `gmail` — the live mail watch

```
hearthloom gmail connect you@gmail.com           # prompts for the app password
printf '%s' "$APP_PASSWORD" | hearthloom gmail connect you@gmail.com
hearthloom gmail connect you@your-company.example  # another address adds a mailbox
hearthloom gmail                                 # every mailbox: watching, last check, last new mail
hearthloom gmail watch off                       # or on (--address A when several are connected)
hearthloom gmail watch --every 2 --address you@your-company.example
hearthloom gmail set you@your-company.example --aliases hello@your-company.example,legal@your-company.example
hearthloom gmail set you@your-company.example --embers on   # let mail-watching embers see it
hearthloom gmail disconnect you@your-company.example
```

Connect Gmail on one node, ideally one that's always on (a hosted node or
a home server). A node can watch several mailboxes — a personal Gmail and
a Google Workspace address alike, each with its own app password. New
mail arrives within seconds: the node keeps an IMAP IDLE connection open
per mailbox and pulls as soon as Gmail reports a change, plus a catch-up
at the interval in case a notice was missed. The app password comes from
stdin and is tested before it's kept. If Gmail refuses a sign-in, that
mailbox stops (and says so) until you connect it again — it never keeps
retrying a bad password; the others carry on.

`--aliases` lists your other addresses that arrive in a mailbox (a
Workspace alias, a catch-all): mail from any of them counts as yours, and
a reply drafted to mail sent to one goes out from it. Embers that watch
mail see the first mailbox; an added one only after `--embers on`.
`hearthloom node` shows which node watches which mailbox.

### `provenance` — how a memory got here

```
hearthloom provenance ep_0123456789abcdef
hearthloom --json provenance ep_0123456789abcdef
```

Who first wrote it (an import, you, an agent's key, your phone), on which
of your nodes and when, who let it in when someone else proposed it, and
the route it took between your nodes to reach this one. When an agent's
task produced it, the task is named with everything outside it read and
what it wrote; for a summary, what it summarises. Memories that arrived
from another node before 0.5.78 say only which node sent them. The same
view is under "How it got here" on every memory in the dashboard.

### `peer` — pairing

```
hearthloom peer status
hearthloom peer pair https://brain.example.com            # prompts for the key
printf '%s' "$KEY" | hearthloom peer pair https://brain.example.com --name cloud
hearthloom peer sync [--full]
hearthloom peer unpair
```

`status` shows the pairing state, sync cursors and the last outcome for the
always-on peer. `pair` does what the setup wizard's "I already run another
node" does: the key is a `peer`-scope key minted on THAT node (☰ → API
Keys), read from stdin and never taken as an argument. `unpair` stops
syncing and retires this node's key on the other one when it answers.

### `quarantine` — the human gate

```
hearthloom quarantine list
hearthloom quarantine accept qr_123
hearthloom quarantine reject qr_123
```

Reviews agent-proposed memories. Accepting writes the episode; this is
deliberately a human act — don't script it.

### `query` — the deep-query engine (RLM)

```
hearthloom query "how did my sleep change after the move?"
hearthloom query --verbose "…"        # show the tool-call transcript
```

An LLM iterates over search/recall/traverse/read handles and returns a
cited synthesis. Spends tokens (the one query-path exception); every
run is traced in the Ask history.

### `recall` — a day, a span, or an entity

```
hearthloom recall --on 2026-06-14
hearthloom recall --from 2026-06-01 --to 2026-06-07
hearthloom recall --entity "Acme Corp"
```

Episodes with provenance for a time window or an entity.

### `repair` — replace memories an earlier version stored wrongly

```
hearthloom vault unlock --minutes 15
hearthloom repair mail                            # dry run: what would change
hearthloom repair mail --apply                    # replace them
hearthloom repair calendar --file work.ics --file home.ics          # dry run
hearthloom repair calendar --file work.ics --file home.ics --apply
hearthloom repair chatgpt --export ~/Downloads/chatgpt-export.zip   # dry run
hearthloom repair health --export ~/Downloads/export.zip --apply
hearthloom repair copies                          # count first, then --apply
hearthloom repair photo-days --apply
hearthloom repair list                            # runs, newest first
hearthloom repair undo rp_…                       # what undoing it means
hearthloom repair undo rp_… --yes                 # put a run's memories back
```

Importing again can't fix a memory an earlier version stored wrongly: a
stored ref keeps its first text, and a retired one never lands again. A
repair reads the original again with the current importer and lands a
corrected copy that takes the memory's place — its label (private stays
private), its retirement, and the links the extraction pass found carry
over; the old copy stays in the record behind it.

- `mail` — mail memories still encoded (an encoded subject or name, a
  base64 or quoted-printable body, raw MIME parts). Each message's
  original is read from an archive this node holds (the Takeout's, each
  connected mailbox's live archive) and decoded, keeping the labels the
  memory was made under. A Message-ID is the sender's choice and an
  archive can hold several messages under one, so every archived copy
  under it is read: the original is a copy outside Spam and Trash with
  the memory's date and sender, and when such copies read differently the
  memory is left (`ambiguous`). A message no local archive holds, or one
  with no Message-ID, is listed and left, and so is every message while an
  archive can't be read — its index out of date or unreadable, or its
  drive not connected (`archive_error`; the notes name the archive). Run
  it on the node that holds the archives.
- `calendar --file <ics>…` — the files read by the current importer
  (they're sent to the node, so `--url` works): an event the .ics
  importer stored whose text differs (an alarm's attendee or description
  taken in, a time read another way) is replaced, an occurrence not
  stored yet is imported, and an event an earlier importer stored under
  another ref is found by its UID and start — never imported a second
  time. A stored event whose own UID only begins the same way
  (`meeting#…` for `meeting`) is another event and is left; one that
  can't be told apart is listed (`unmatched`) and neither replaced nor
  imported. An event your phone's calendar feed stored (same UID, its own
  shape and times), or one whose writer this node didn't record, is
  listed and left (`other_writer`).
- `chatgpt --export <folder|zip>` — a ChatGPT export (holding
  `conversations.json` or `conversations-NNN.json`) read again with the
  current reader. Earlier versions sorted every message of a conversation
  by time, so answers you regenerated and prompts you edited were mixed
  into the transcript; a stored conversation (imported from the full
  export or from its single file) whose transcript has turns off the
  branch it ended on is replaced by that branch — only when the export
  holds every turn the stored transcript shows. One stored from a later
  export (the conversation continued since, or its images named
  otherwise), or one whose only difference is its title, is listed and
  left (`other_export`): give the newest export. A conversation you flagged
  "do not remember" ends private, whether or not its text changes (the
  single-file route used to ignore the flag) — it goes into the vault
  before its corrected copy lands; an undo doesn't take it out. A stored
  conversation the export doesn't hold is listed and left.
- `health --export <export.zip|export.xml|folder>` — an Apple Health
  export read again (a zip is read in place, never unpacked). Earlier
  versions averaged readings in different units as one number, read
  workout distances in metres as kilometres, and added up sleep from
  several trackers; a stored day summary or workout whose text differs is
  replaced. Days and workouts the export doesn't cover are listed and
  left, and so is the day the export was made on (only part of it is in
  the export), and a day that shows a tracker or a measure the export
  doesn't hold for it — body fat beside a weight, exercise minutes beside
  steps (`other_export` — read from a later export). A night is shown as
  one tracker recorded it, so deep or REM hours another tracker added are
  dropped from a day when the export holds them from that tracker.
  Workouts the phone sent are left alone.
- `copies` — Chrome days, iMessage chat-days and Claude Code sessions an
  earlier version stored under an unversioned ref. Each node kept the text
  that reached it first, so two nodes can show different texts of one
  day. A run re-mints each such day once as a copy named by what it holds
  (`<ref>#r<count>`), from the stored text — the original files aren't
  read — and every node shows the fullest copy. The dry run gives the
  count first: each re-mint is a new episode on every node, and iMessage
  days are read again by the extraction pass (the model cost line). Your
  other nodes re-mint their own copies when the run reaches them at their
  next sync. Two copies holding the same count with different texts can't
  be told apart and stay as each node has them. After an undo, the next
  local read of such a day puts the run's copy back in view when it holds
  as much — on your other nodes too, at their next sync.
- `photo-days` — photo days minted while a photo was stored twice (an
  earlier Mac scan's key next to the phone's) show it twice and keep an
  inflated count, and the day was only minted again once it grew past it.
  A day is re-minted under its next revision, its count put right, only
  when its count is inflated (it shows a photo twice, or its count here
  keeps photos stored here out) and the photos this node holds (each by
  its current name and the names it had before) cover every photo the day
  shows and every place it names — so a day another node minted with more
  photos is listed and left. Two photos shown under one name in the same
  minute count as two. A day that reads otherwise for another reason
  (a place read since, another order) is only counted. A day that already
  reads right but whose count here is off has only its count put right.
  Each of your other nodes checks the corrected day against its own copy
  when it arrives, and again when the day's photos reach it (from another
  node, or from its own Mac scan or the phone): where its
  copy shows a photo the correction lacks, its copy stays in view, and
  where the day is private, the correction is private too.

`chatgpt` and `health` read the export on the node's machine (they refuse
`--url`); the corrected copies reach your other nodes at their next sync.

Every run is a dry run unless `--apply`; it prints counts, episode ids and
a short reason for each, never what a message, an event, a conversation
or a day says (`--json` lists every finding), how many new copies the
extraction pass would read (mail, calendar and health extraction are off
by default; ChatGPT conversations and iMessage days are on), and how many
new episodes the run makes: each is also new to the goal matching, and a
copy that matches one of your goals goes to the cascade pass (one model
call a cycle, for up to 6 matches). Owner only, with the vault unlocked;
one run (or undo) at a time per node. Applying again changes nothing.

`undo` brings the replaced memories back in view on this node and, at
their next sync, on your other nodes — for `copies` and `photo-days`, the
copy that stood before the run (and the photo day's count; the day's next
revision goes past the run's) — and a copy
that arrives later from an undone run stays behind; what a calendar run
added stays (retire it like any memory). It can't be taken back: a later
repair finds the same corrections "repaired before and undone" and leaves
them until the importer reads the originals differently, so `undo` only
says so unless you add `--yes`.

Upgrade every node first. A node older than this release doesn't know
the rule: it keeps each corrected copy as a second memory in view beside
the old one — even where the old one is retired, or private on that node
alone — doesn't re-mint its own copies for `copies`, and as a hub it
doesn't pass an undo between your other nodes. Every node publishes
whether it knows repairs in the node list; the dry run names any node
seen in the last 30 days that doesn't, and `--apply` is refused while
there is one, unless you add `--allow-older-nodes` (for a node you know
is gone). Once it is upgraded, the run reaches it and it settles the
records: the old copies go behind the corrected ones, and private labels
and retirements hold for both.

### `scope` — correct scope labels

```
hearthloom scope ep_abc123 work
hearthloom scope --by-source imessage personal
```

Relabels episodes (work|personal|family|health|private). Labels are
correctable; content is not. Privacy rules outrank later inference.

### `search` — full-text search

```
hearthloom search "consolidation freeze"
```

Hybrid FTS + vector search over the record (FTS-only without a model).

### `source` — virtual sources

```
hearthloom source register gmail-archive ~/.hearthloom/archives --adapter mbox
hearthloom source search gmail-archive "invoice from midjourney"
hearthloom source promote gmail-archive msg-4411
hearthloom source status
hearthloom source sync
```

Data kept at origin, queried at need, promoted (materialized) on
demand with provenance.

### `status` — one screen

```
hearthloom status
hearthloom --url https://my-node.example.com status --json
```

Version, record size, Tend backlog, vault state, thinking state
(active / delegated to peer / paused), and peer sync at a glance.

### `sync` — peer replication

```
hearthloom sync              # status
hearthloom sync --now        # force a cycle
hearthloom sync --now --full # non-incremental pass
```

The hybrid peer sync loop, on demand.

### `task` — tasks from the shell

```
hearthloom task add "call the accountant #today"
hearthloom task add "weekly review #daily @[Jane Smith]"
```

Quick capture to the Desk; `#tags` (`#urgent`, `#today`, `#daily`,
template names) and `@[mentions]` work exactly as they do there.

### `upgrade` — update the host binary

```
hearthloom upgrade --check         # is a newer release available?
hearthloom upgrade                 # download, verify, replace, restart
```

Reads the release channel (the configured registry's well-known doc;
none by default), compares
to the running version, and — when this node can self-update (an
official build for the platform, not a source build or a hosted
instance) — downloads it, verifies its sha256, atomically replaces the
binary, and restarts via launchd. Source-built nodes are told to pull
and rebuild; hosted tenants upgrade by image repin. The ☰ menu badges
"new version available" and `#/upgrade` offers the same one-click flow.

### `vault` — the privacy vault

```
hearthloom vault                      # status
hearthloom vault unlock --minutes 15
hearthloom vault lock
```

While locked (the default), every read surface excludes
`private`-scoped material with zero leakage. The vault is per viewer:
`vault unlock` opens it for the CLI's own key only (so `vault unlock &&
export` works), never for the dashboard or other keys. Unlock is time-boxed and
auto-relocks. Persisted artifacts and cloud-LLM passes exclude private
always, vault open or not.

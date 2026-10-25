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
- The inbox drop folder `~/.hearthloom/inbox/` accepts zips, .mbox
  (≤250MB), .ics, conversations.json, chat.db, Photos.sqlite — dragged
  files ingest automatically.

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

## iMessage / Photos / Chrome / Claude Code (local, TCC)
`POST /v1/ingest/local {"adapter":"imessage"|"photos"|"chrome"|"claude-code"}`
(defaults find the library paths). Re-run any time — only new
messages/photos/visits/sessions land. Photos also feeds the location
index (GPS lines parsed at consolidation).

## Git / ICS / chatlog / Substack
- `hearthloom ingest git <repo-path>` per repo (registered in sources;
  refreshed automatically ≤1/20h).
- `.ics` files → drop folder, or `hearthloom ingest ics <file>`.
- Claude/ChatGPT exports: `conversations.json` → drop folder
  (owner-aware via users.json).
- Substack: `hearthloom ingest substack <publication-url>` (archive
  API; works on custom domains).

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

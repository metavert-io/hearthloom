# The `hearthloom` CLI

Everything goes through the daemon's HTTP API — the same surface the
dashboard uses. The CLI never opens the database itself (design law #4).
If no local daemon is running, one is spawned on demand.

**Global flags** (valid on every command):

| flag | meaning |
|---|---|
| `--url <base>` | Target another node you own (e.g. `--url https://brain.example.com`). Remote nodes are never auto-spawned. |
| `--json` | Machine-readable output where supported (`status`, `key list`). |

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

### `ember` — registry install + developer tools

```
hearthloom ember search presence            # discover (your configured registry)
hearthloom ember info hpresence             # versions, scopes, review status
hearthloom ember install hpresence          # fetch → verify → approve scopes → install
hearthloom ember install hpresence --version 0.1.0 --yes
hearthloom ember install foo --registry https://other-registry.example
hearthloom ember install foo --upgrade-host   # upgrade hearthloom first if the ember needs it
hearthloom ember update                     # check ALL installed vs the registry, update outdated
hearthloom ember update hpresence --check   # report only
hearthloom ember uninstall hpresence
hearthloom ember list                       # installed on the node
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

### `peer` — pairing status

```
hearthloom peer
```

Pairing state, sync cursors, last outcome for the always-on peer.

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

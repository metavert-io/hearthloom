# Writing an ember (a Hearthloom plugin)

Hearthloom's plugins are called **embers** — small companion programs
that glow alongside the hearth. In this guide "ember" and "plugin" are
the same thing; on the wire and on disk the identifiers say `plugins`
(routes like `/v1/plugins`, the `~/.hearthloom/plugins/` directory) and
those are frozen — the brand is embers, the protocol is plugins.

A practical guide for third-party developers. Everything here shipped in
Hearthloom 0.5.11; the architecture rationale lives in
`docs/plugins.md`. The reference plugin is
[hpresence](https://github.com/metavert-io/hpresence) — a complete working
example in ~300 lines with no SDK and one dependency.

## The one idea that explains everything

**A plugin is a separate program that talks to the Hearthloom daemon's
HTTP API on loopback with its own scoped key.** There is no in-process
loading, no ABI, no plugin runtime, and no SDK requirement — if your
language can make HTTP requests, it can be a Hearthloom plugin. The
daemon remains the only process that ever touches the user's data on
disk (that's design law #4, and it's why your plugin gets storage
*endpoints* rather than a directory).

What you can extend:

| surface | how |
|---|---|
| **the record** | ingest episodes (`ingest` scope) — your data becomes part of the user's memory, searchable, graphed, replicated |
| **retrieval** | register a virtual source (`ingest` scope) — your data stays at origin, queryable on demand |
| **notifications** | post to the user's notification bell (`notify` scope) |
| **reading the record** | search/recall/traverse and most GET surfaces (`read` scope) |
| **the dashboard** | one top-nav page, Visualize menu items, hamburger menu items, Desk widgets — declared in your manifest, rendered sandboxed |
| **storage** | per-plugin KV + files, held by the daemon, exported with the user's record, optionally replicated to their always-on node |
| **liveness** | a `launch` entry makes the daemon run your binary under launchd, restart it, and stop it on disable/uninstall |

## Package format

An ember package is a directory (or a zip of one):

```
my-plugin/
  hearthember.json      required — the manifest
  bin/my-plugin         your executable(s), referenced by launch.<platform>
  ui/…                  static pages/assets for your UI contributions
```

Users install it from **☰ → Integrations → embers** (folder path or
zip upload) or with `hearthloom ember install <name>` from the
**ember registry** you configured — either way they see exactly what you
requested before granting anything. Registry-distributed embers are
sha256- and Ed25519-verified against your published release signature
(see your registry's protocol documentation; publish with
`ember-sign`); side-loaded installs are labeled *"unreviewed — you
trust its author with your machine."*

## The manifest (`hearthember.json`)

```json
{
  "protocol": 1,
  "name": "my-plugin",
  "version": "0.1.0",
  "publisher": "you",
  "description": "One honest sentence about what this does and reads.",
  "scopes": ["ingest"],
  "launch": { "darwin": "bin/my-plugin" },
  "sync": "mirror",
  "localOnly": true,
  "ui": {
    "nav":       { "label": "My Page", "page": "index.html" },
    "visualize": [{ "label": "My chart", "page": "chart.html" }],
    "hamburger": [{ "label": "My settings", "page": "settings.html" }],
    "desk":      [{ "label": "my widget", "page": "widget.html", "height": 170 }]
  }
}
```

Field notes:

- `protocol` — always the integer `1` today. Version gates ride this
  and capability strings, never version strings.
- `name` — 1–40 chars of `[a-z0-9-]`; it is your identity, your storage
  namespace, and your URL segment. `self` and `ui` are reserved.
- `scopes` — what you *request*; the user (not you) decides what's
  granted. Plugins may request only `read`, `ingest`, `notify`.
  Infrastructure scopes (`admin`, `peer`, `friend`, `write`, `mcp`) are
  never grantable to plugins.
- `launch` — optional, per-platform relative path to an executable. No
  entry = no sidecar (a pure-UI plugin, or a program the user runs some
  other way). Only `darwin` launches today; cloud nodes run no plugin
  sidecars in v1.
- `sync` — `"local-only"` (default) or `"mirror"`: mirror replicates
  your KV and data files to the user's paired always-on node (see
  Replication).
- `placement` — where your sidecar belongs across a user's nodes:
  `"local"` (reads this machine — never installable on a hosted node),
  `"cloud"` (belongs on the always-on node), or `"any"` (default).
  `localOnly: true` is the legacy alias for `"local"`.
- Unknown fields are ignored (additive wire discipline) — you can carry
  your own metadata.

## Lifecycle: what install actually does

1. The user reviews your requested scopes and approves.
2. The daemon mints an API key with exactly the granted scopes, named
   `plugin:<name>`, and binds it to your registry row.
3. Your package is copied to `~/.hearthloom/plugins/<name>/pkg/`
   (replaced wholesale on upgrade — keep no state there).
4. The key's plaintext is written once to
   `~/.hearthloom/plugins/<name>/key` (mode 0600).
5. If you have a `launch.darwin`, a launchd agent
   (`com.hearthloom.plugin.<name>`) is written and loaded with
   `RunAtLoad` + `KeepAlive` and these environment variables:

```
HEARTHLOOM_URL              http://127.0.0.1:43278
HEARTHLOOM_PLUGIN_KEY_FILE  /Users/…/.hearthloom/plugins/<name>/key
HEARTHLOOM_PLUGIN_NAME      <name>
```

Read the key from the file at startup (and re-read on a 401 — the user
may reinstall). Your stdout/stderr goes to
`plugins/<name>/plugin.log`.

**Disable** unloads your launchd unit and revokes any live UI tokens
(your main key survives). **Uninstall** unloads the unit, revokes your
key, and removes the registry row — but *never* deletes your data
directory; the user is told where it lives. Reinstall/upgrade keeps the
existing key, so your key file stays valid.

## The API you can call

Authenticate every request: `Authorization: Bearer <key>`.

**Plugin keys are default-denied.** Unlike ordinary integration keys,
a plugin-bound key reaches *only* the routes its scopes map to — the
grant screen the user saw is the actual ceiling, enforced in
middleware. Everything else answers 403. The vault sits beneath all of
it: `private`-scoped material is filtered out of every read no matter
what you hold.

Always available to any plugin key (no scope needed):

| route | purpose |
|---|---|
| `GET/PUT/DELETE /v1/plugins/self/kv/{key}` | your KV store (values ≤256KB; strings — JSON-encode yourself) |
| `GET /v1/plugins/self/kv` | list your keys |
| `GET/PUT/DELETE /v1/plugins/self/files/{path}` | your file store (≤32MB/file, clean relative paths, no dotfiles) |
| `GET /v1/plugins/self/files` | recursive listing with size + mtime |
| `GET /v1/bus/stream` | SSE event hints (`operational.changed`, `episode.created`-class topics) — react instead of polling; hints carry no data, your follow-up fetches re-check scope |

With `ingest`:

| route | purpose |
|---|---|
| `POST /v1/ingest` | write episodes (see below) |
| `POST /v1/sources/virtual` | register a virtual source (data stays at origin, queryable) |
| `POST /v1/sources/virtual/{name}/promote` | materialize virtual items on demand |

With `notify`: `POST /v1/notifications` — your origin is forced to
`plugin:<your-key-name>`; you cannot impersonate the system or another
plugin. Links must pass the node's allowlist.

With `read`: most `GET /v1/*` surfaces (search, recall, traverse,
day, stats, timeline, entities…). Denied regardless: auth, keys,
export, users, vault, plugins admin, sync, peering, provisioner,
privacy, integrations, monitoring.

### Writing episodes

```json
POST /v1/ingest
{
  "actor": "plugin:my-plugin",
  "episodes": [{
    "source": "my-plugin",
    "source_ref": "myplugin:2026-07-20",
    "kind": "my_kind",
    "title": "Human-readable one-liner",
    "body": "Markdown. Kept verbatim forever — never discarded.",
    "event_time": { "utc": "2026-07-20T12:00:00Z",
                    "tz_offset_minutes": -240,
                    "uncertainty": "day" },
    "trust": "self",
    "scope": null,
    "entities": [], "edges": []
  }]
}
```

- `(source, source_ref)` is the idempotency key: re-sending the same
  material is a no-op. Use deterministic refs and you can retry freely.
- The daemon **re-stamps your actor** to `plugin:<name>` from your key —
  provenance is receiver-stamped; you can't claim to be anyone else.
- `uncertainty`: `exact | minute | hour | day | month | year | unknown`.
- `scope: null` lets the daemon infer (project > identity > personal);
  the user's privacy rules outrank everything, including you.
- `entities` lets you name people/orgs/places/projects the episode
  mentions; attributes like `("email", "a@b.c")` drive cross-source
  identity resolution.
- Episodes you ingest replicate to the user's peer node through the
  normal pipeline automatically.

## Replication (`"sync": "mirror"`)

If the user runs a paired always-on node, a mirror-sync'd plugin's KV
and data files converge across both nodes on the ~5-minute sync cycle:
KV by last-writer-wins on timestamp, files by fingerprint with
newer-mtime-wins. The receiving node only accepts data for plugins the
user *also installed there* with mirror sync — a peer can never create
storage for a plugin the user didn't choose. Design accordingly: don't
assume you're the only writer of a KV key; prefer per-node keys or
tolerate LWW.

## UI contributions

Your `ui/` files are served at `/plugins/<name>/ui/…` and rendered
inside `<iframe sandbox="allow-scripts">`. That sandbox is the security
model: **your page's JavaScript never runs in the dashboard's origin**,
so it cannot ride the user's login session. Instead, when your page
loads, the shell posts you a message:

```js
addEventListener("message", (ev) => {
  if (ev.data?.type !== "hearthloom") return;
  const { token, base, palette } = ev.data;
  // token: a 12-hour key carrying exactly your granted scopes
  //        (revoked when the user disables/uninstalls you)
  // base:  the daemon origin for fetch()es
  // palette: CSS custom properties (--bg, --ink, --ink-faint,
  //          --accent, --card, --line) — apply them to match the
  //          user's theme, light or dark
});
```

Fetch with `Authorization: Bearer <token>` against `base` — the daemon
answers cross-origin, credential-less requests for exactly this
purpose. See `hpresence/ui/hearth.js` for a copy-paste handshake.

If you render with the house chart library (`<script
src="/assets/chart-svg.js">` works inside the sandbox — it's a
pre-auth asset), style its text classes yourself: the dashboard's
stylesheet never loads in your iframe, and unstyled SVG text is black.

```css
.hl-chart text { fill: var(--ink, #ddd); }
.chart-title-text { font: 600 15px ui-serif, serif; }
.chart-tick { font: 11px ui-monospace, monospace; }
```

Placement:

- `ui.nav` — **one** entry in the top navigation. Use it only if your
  plugin deserves a destination; most don't.
- `ui.visualize[]` — entries in the Visualize dropdown.
- `ui.hamburger[]` — entries in the ☰ menu (settings pages live here).
- `ui.desk[]` — small widgets on the Desk's rail (`height` 80–480px).
  Keep them glanceable; they load on every Desk visit.
- `requires: { host: ">=X.Y.Z" }` — the minimum hearthloom your ember
  needs (it uses an API added in that version). Install is refused on
  older hosts; the user is pointed to upgrade first.
- `config[]` — declarative settings the dashboard renders on your
  Integrations card: `{key, label, type: string|number|boolean|list,
  default?, help?}`. Saved values land in your own `config` KV — just
  read it (and re-read periodically); you never build a settings UI.
- `ui.card[]` — links rendered on your ember's own card in
  Integrations ("View presence data →"). The right home for a
  destination that shouldn't crowd the top nav or Visualize.

Pages are static files; there's no build step or framework requirement.
Relative asset references within your `ui/` dir work.

## Peered setups: one runner, everywhere-data

Many users run two nodes (a laptop + an always-on peer). Your data
already behaves: episodes replicate through the normal pipeline, and
`sync: mirror` carries your KV/files both ways. Your *process* is the
question — two copies of a poller double-ingest and fight over state.
The node handles this for you:

- A mirror-sync'd ember **with a sidecar** gets a **runner lease**: the
  node running your sidecar heartbeats a reserved `__runner` key in
  your (mirrored) KV; the peer sees the fresh lease and holds its copy
  back, and takes over automatically if the runner goes quiet for ~10
  minutes. You don't implement anything — but write your state so a
  handoff mid-day is survivable (idempotent episode refs make this
  free).
- KV keys starting `__` are reserved for the daemon; your writes to
  them are rejected.
- `placement: "local"` embers never run (or install) on hosted nodes;
  `"cloud"` embers install on a laptop but run on the peer — the
  dashboard shows "runs on your always-on node."
- Hosted (cloud) instances only install embers that are cloud-eligible
  AND counter-signed by the registry — enforced by the node at stage
  time, not just hidden in the UI.

## Rules, limits, and honest boundaries

- **You are a guest process.** Hearthloom's scopes protect the user's
  *record*; nothing makes a hostile local binary safe for their
  *machine*. Say plainly in your description what you read and do.
- Never touch `~/.hearthloom` directly — not the db, not even "your"
  directory. Everything goes through the API (your `pkg/` dir is
  replaced on upgrade; your data dir is written by the daemon on your
  behalf).
- Storage caps: 256KB per KV value, 32MB per file. Secrets in plugin
  storage are exported with the user's record — sealed plugin secrets
  are a planned addition; until then, prefer short-lived credentials.
- Wire discipline: the API only ever *adds* fields. Ignore fields you
  don't recognize; feature-detect endpoints (a 404/405 means an older
  node — degrade, don't die).
- Be idle-friendly: the daemon runs on the user's machine. Prefer bus
  hints over tight polling; sleep generously; batch your writes.
- The MCP surface is not extensible (four tools, by invariant), and
  plugins can't add consolidation passes, enforce/verify design laws,
  or reach the vault, keys, export, or peering.

## The fifteen-minute start

1. Copy hpresence's layout; edit `hearthember.json` (your name, your
   scopes — request the *least* you need).
2. Write a loop: read `HEARTHLOOM_PLUGIN_KEY_FILE`, do your thing,
   `POST /v1/ingest` with deterministic `source_ref`s, park state in
   `/v1/plugins/self/kv/*`.
3. Drop an `index.html` in `ui/` with the postMessage handshake.
4. `☰ → Integrations → plugins →` your folder → review → install.
5. Watch `~/.hearthloom/plugins/<name>/plugin.log`.

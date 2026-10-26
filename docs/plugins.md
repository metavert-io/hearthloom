# Plugins ("EMBERS") — the extensibility surface (design 2026-07-16, SHIPPED 2026-07-20)

Branding (Jon 2026-07-20): plugins are **embers** in every user- and
developer-facing surface; the manifest is `hearthember.json`
(hearthplugin.json accepted as fallback). Wire/disk identifiers stay
`plugins` — the Memories/#browse precedent: rename the surface, freeze
the protocol.

Status: **BUILT** (v42 + daemon plugins.rs + the UI hooks below), with
distribution deliberately deferred. What shipped, beyond this design:

- **v42**: `plugins` registry (installed manifests, GRANTED scopes, PAT
  binding, enabled) + `plugin_kv` (daemon-mediated storage; watermarked).
- **Package format**: a dir (or zip) of `hearthember.json` + `bin/<os>`
  + `ui/`. Install is LOCAL-ONLY (Integrations → plugins → review the
  requested scopes → grant): **no central index, no CLI installer, no
  signature verification** — that whole layer is a provisioner's, later,
  against this package format (Jon 2026-07-20). `signature` stays a
  reserved manifest field.
- **Self-storage**: `/v1/plugins/self/kv/*` + `/files/*` → the daemon
  writes under `~/.hearthloom/plugins/<name>/` (law #4 intact). Exported
  (minus each plugin's `key` file — carved out in export.rs::walk).
- **Replication**: `/v1/sync/plugins` peering leg — manifest
  `"sync": "mirror"` replicates a plugin's KV (LWW by updated_at; the
  receiver refuses plugins it doesn't also mirror) and data files
  (fingerprint listing, newer-mtime-wins, per-file fetch). Episodes a
  plugin ingests replicate via the normal pipeline regardless.
- **THE PLUGIN CEILING** (auth_middleware → plugins::plugin_route_allowed):
  plugin-bound keys are DEFAULT-DENY; scopes map to an explicit route
  table (`ingest` → /v1/ingest + virtual-source register/promote;
  `notify` → notifications; `read` → GET minus denied prefixes; the
  self surface + bus SSE always). Plugins may request only
  read/ingest/notify — never admin/peer/friend. /v1/ingest re-stamps
  the actor to `plugin:<name>` (receiver-stamped provenance).
- **UI hooks**: manifest `ui` contributes ONE top-nav entry, Visualize
  items, hamburger items, and Desk widgets; app.js renders them from
  `/v1/plugins/ui`. Pages/widgets are static assets served at
  `/plugins/<name>/ui/*` inside **`<iframe sandbox="allow-scripts">`** —
  plugin JS never runs in the dashboard origin; the shell posts
  `{type:"hearthloom", token, base, palette}` (a 12h `plugin-ui:<name>`
  key carrying exactly the granted scopes; revoked on disable/uninstall).
  Credential-less CORS (`*`, no cookies ever) makes the opaque-origin
  fetches work without weakening session auth.
- **Reference plugin**: [hpresence](https://github.com/metavert-io/hpresence) (its own repo, on purpose — the
  third-party path): the presence sampler, `ingest`-only, mirror-sync'd
  KV, all four UI hooks, zero-SDK hand-rolled HTTP client.

Shipped in 0.5.72 for the first ember that must run on an always-on
node (a Tesla data ember): the daemon supervises ember programs where
there is no launchd (hosted Linux nodes), `launch` keys by target
triple, `publish` (pre-auth public files under /.well-known/), sealed
ember secrets, and the OAuth callback relay. Hosted nodes install only
registry-verified packages (the upload path used to skip that check).

Deferred: a central index with signing and browse (Tier 3); moving Mac
embers off launchd onto the daemon's supervisor (one code path); a
master key provisioned on hosted nodes (until then they seal ember
secrets with a node key file).

The original design follows — still the contract for everything above.

## Why plugins exist

Two reasons, and the second is the bigger one:

1. **Some wanted features don't belong in core** — anything needing its
   own running software or exotic dependencies (screenpipe's app, a
   presence sampler, SQLCipher, device SDKs).
2. **Other people should be able to extend Hearthloom without us.** The
   plugin system is the third-party developer surface — a manifest anyone
   can write, scopes they can request, a key the user mints. Obsidian's
   gravity is its community plugins; that is the bar.

## The core-vs-plugin test (two independent axes)

- **Who builds it?** First-party → compile in by default. Third-party →
  plugin, always (they can't ride our release cycle and shouldn't).
- **What does it need?** Its own running software or exotic/heavy
  dependencies → plugin even if first-party. Plain file/archive parsing →
  compiled-in adapter (the Rewind adapter, and everything like it).

Never plugins: anything that enforces or verifies the design laws, any
trust/audit surface, and anything the rest depends on (substrate,
retrieval, sync, keys, the vault).

## Architecture: sidecar processes, never in-process code

Design law #4 (the daemon is the sole writer) is already the plugin
boundary. A plugin is **a separate process talking to the loopback HTTP
API with a scoped PAT** — exactly what adapters, agents, and the
dashboard already are. No dynamic loading, no ABI, no plugin code inside
the daemon's address space, ever.

What the existing machinery already provides:

| need | existing mechanism |
|---|---|
| capability sandbox (data layer) | v30 scoped PATs; `require_scope` per route |
| untrusted ingestion | quarantine-first; trust tiers on episodes |
| attribution | actor identity (`adapter:…`/`agent:…`) in every mutation |
| instant uninstall of access | key revocation (☰ → API Keys) |
| activity audit | the agent-access card (docs/agent-surface.md) |
| liveness/scheduling | launchd/systemd units the plugin ships itself |

What's genuinely new: the **manifest**, the **install/discover UX**, the
**index**, and **signing**.

## The manifest (`hearthplugin.json`, additive-only wire discipline)

Third-party plugins are permanently version-skewed against core — the
manifest and every plugin-facing API follow the peering rules: additive
fields only, unknown fields ignored, feature-detect per-op, gate on a
`protocol` integer + capability strings, never on version strings.

```json
{
  "protocol": 1,
  "name": "screenpipe-bridge",
  "version": "0.3.1",
  "publisher": "hearthloom",
  "description": "ambient screen context as a virtual source",
  "scopes": ["read", "ingest"],
  "kind": "sidecar",
  "launch": { "darwin": "bin/screenpipe-bridge" },
  "signature": "…"
}
```

- `scopes` is a REQUEST; the user (or provisioner policy) grants at install
  by minting a PAT with exactly those scopes. The offer/grant is MAY; the
  vault + scope_guard remain WILL-ENFORCE — an over-scoped plugin still
  cannot read `private` while the vault is locked, and `/brain` stays
  locked-view always.
- New scope granularity (e.g. `ingest` as narrower-than-`peer` write:
  /v1/ingest + /v1/propose only) ships WITH the first plugin that needs
  it, not before.

## Trust: the two-tier index

- **Central index (via a registry): reviewed + signed.** Publisher-signed
  manifests, our countersignature after review. Signature verification is
  what gates auto-provisioning onto provisioned cloud nodes. Review
  covers: scopes requested vs purpose, network egress claims, license.
- **Self-install: caveat emptor, explicitly.** Any manifest + binary can
  be installed by hand; the UI labels it "unreviewed — you trust its
  author with your machine." No countersignature, no cloud provisioning.

Honest limit, stated everywhere it matters: Hearthloom's scopes protect
**your record**; nothing can make a hostile local binary safe for **your
machine**. That's the OS's job; curation and signing reduce, not remove,
the risk. DIY cloud nodes may run anything — a provisioner's value is
curation + one-click provisioning, not exclusivity.

## Install / discover UX

The Integrations page grows a **Plugins** section: browse the central
index (when configured), install (mint the scoped key, drop the launchd
unit, start it), status chip per plugin (running / stopped / key revoked),
uninstall (revoke key + remove unit; the plugin's own data dir is listed,
never silently deleted). Every plugin action lands in the mutations log.

## Cloud nodes (run by a provisioner)

Provisioning policy, not new enforcement: a provisioned node auto-installs
only countersigned plugins from the central index, pinned by version.
Plugins on cloud nodes get the same scoped keys; `localOnly` plugins
(anything reading the user's machine) simply don't offer themselves there
— the `this_node` adaptation the import cards already do.

## First candidates (validate the model before generalizing)

1. **screenpipe bridge** — registers screenpipe's local store as a
   virtual source (vsearch/vquery), promote-on-demand. Purely opt-in.
2. **presence sampler** — first-party, minimal (frontmost app + idle
   state, no screenshots/audio), `presence_day` aggregates. Built as
   the reference plugin, hpresence.

Both are read-into-quarantine/ingest shapes; neither needs new core
surface beyond (possibly) the `ingest` scope.

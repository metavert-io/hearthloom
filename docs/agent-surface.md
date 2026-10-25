# The agent surface — read-only by construction

Every AI agent, plugin, and peer that touches a Hearthloom record goes
through the same narrow surface, and that surface enforces one posture:
**agents read; humans write.** This document is the contract; its
rules are enforced at read time.

## The four tools (MCP, `http://127.0.0.1:43278/mcp`)

Exactly four, permanently (add detail levels, not tools):

| tool | what it does | writes? |
|---|---|---|
| `search` | hybrid FTS+vector over episodes (`detail: summary\|full`) | no |
| `recall` | reconstruct a day/span/entity with citations | no |
| `traverse` | capped bi-temporal graph walk (`as_of` time travel) | no |
| `propose_memory` | *suggest* a memory | into **quarantine**, never the graph |

`propose_memory` is the only mutation an agent can express, and it lands in
a quarantine queue a human resolves on the Tend page. The RLM (`hearthloom
query` / Ask) follows the same rule at a higher level: it may **suggest**
writes as prefill buttons (```action fences → the Desk), but a human
completes every save. Two-phase, always.

## CAN / MAY / WILL-ENFORCE

Three layers, deliberately kept distinct (see CLAUDE.md "Peering vs
federation" for the full model):

- **CAN** — capability: the operation exists on this build (impersonal,
  version-tied). Peers and plugins feature-detect (404 → degrade), never
  assume.
- **MAY** — willingness: what this node *offers* a given key — scopes on
  PATs, grants on friend keys, per-topic filters on the push bus. Computed
  live, revocable instantly, and only ever a ceiling.
- **WILL-ENFORCE** — the part that actually protects you: `Store::scope_guard`
  filters every read at query time, and **the vault defaults locked** —
  while locked, every server-side read surface (search, recall, dossiers,
  traverse, browse, stats, MCP, Ask, raw SQL via view-shadowing) excludes
  `private` with zero leakage. An offer is a courtesy; the guard is the law.

A leaked event hint, an over-scoped key, a lying peer — each still hits the
read-time guard. There is no second enforcement surface to keep consistent
with the first.

## Keys and scopes

Remote access requires a **PAT** (☰ → API Keys): SHA-256-hashed, scoped,
shown once, revocable to the second. Scopes are narrow (`read`, `peer`,
`friend`, `admin`); the `/brain` manifest surface requires `read` ALWAYS
and serves the vault-**locked** view ALWAYS — no key ever exposes `private`
through it. Loopback needs no key on an un-bootstrapped install; once an
admin user exists, everything authenticates.

Every write that does land (from any actor) carries identity —
`human:… | agent:… | adapter:…` plus session id — into the append-only
`mutations` log with an idempotency key. Provenance is not optional.

## What's watchable today

The Integrations page's **agent access** card aggregates the existing
records: minted keys and their scopes, recent friend-key reads
(`network_accesses`), pending quarantined proposals, and recent Ask runs
(`ask_log`, which traces every RLM run start-to-finish, failures included).

Honest gap: **loopback MCP reads are not per-call logged today.** Reads
can't exfiltrate `private` (the guard), but a local agent's search volume
is currently invisible. If that matters to your threat model, put the
daemon behind `HEARTHLOOM_REQUIRE_KEYS=1` and give each agent its own key.

## LLM spend rules (design law #3)

No agent call triggers cloud LLM work. Tokens are spent at ingest and idle
consolidation only, with two *user-invoked* exceptions (Ask, goal
decomposition). Persisted artifacts (BRAIN.md, digests) and every
cloud-LLM pass exclude `private` always — vault open or not.

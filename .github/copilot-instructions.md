# Hearthloom: review guidance

Hearthloom is a local-first personal knowledge graph: one SQLite file owned by
a single daemon, an HTTP API on loopback, and an embedded dashboard (plain JS,
no build step). `CLAUDE.md` is the full rulebook and `architecture.md`
describes the system; where this summary is shorter, they win.

## Access control (each rule closed a real hole)
- A new route is owner-only for ordinary keys until it is listed in
  `http.rs::KEY_ROUTES` under the narrowest scope. Scoped keys reach only
  `KeyGrants::permits`; friend keys only `friend_route_allowed` plus
  `require_scope(&auth, "friend")`.
- Anything that reveals private material or confers the owner's reach uses
  `require_owner`, never `require_scope("read")`. Key management needs
  `require_key_admin`.
- A write by a key that isn't the owner's takes its actor from `write_actor`,
  never from the request body.
- Never widen `cors_bearer` or exempt an API path from
  `foreign_browser_request`.

## The vault
- Every new read query carries `Store::scope_guard` (or goes through
  `read_query`'s shadow views); no hand-written `!= 'private'`. A locked view
  leaks nothing, not even "N hidden".
- The unlock belongs to one viewer: request work that moves to another thread
  uses `crate::scoped::{spawn_blocking, spawn}`, not plain `tokio::spawn`.
- A locked viewer can move things into the vault, never out. BRAIN.md, digests
  and cloud-LLM passes exclude private data whether the vault is open or not.
- ask_log reads go through `Store::ask_guard`; test `sensitive != 0`, never
  `= 1`.

## Data integrity
- Only the daemon opens the database. Every write carries an actor and lands as
  an event in the `mutations` log.
- Episodes are append-only. A query over episodes or what hangs off them
  carries `superseded_by IS NULL`, or says why not with
  `/* all-episodes: <why> */`.
- Operational tables (tasks, notes, goals, self_model, privacy_rules) journal
  every change through `store::journal_row` / `journal_op`.
- Peer sync merges, never replaces: no `DELETE FROM <table>` when applying a
  peer's rows; deletes travel only as journal tombstones; timestamp legs page
  by `synced_at`. New user-visible state gets a sync leg unless an exclusion is
  written down.

## LLM and Ask
- Every LLM call goes through `llm::call`. No LLM or embedding spend on the
  query path; Ask and goal creation are the user-invoked exceptions.
- Ask acts only through `agents::propose`, and every action is decided by
  `store/agents.rs::decide`. Tool results stay inside `<tool_result>` fences,
  and the system prompt stays byte-stable within an ask.

## Dashboard
- No CDN or remote assets: everything is vendored.
- No `alert`/`confirm`/`prompt`; destructive acts go through `confirmDanger`
  (app.js).
- Escape stored text (the modules' `esc` helpers) before it reaches
  `innerHTML`.
- Route arguments come from the URL, which anyone can craft: validate
  their shape, escape them, and look them up with `Object.hasOwn`, never
  `obj[arg]` alone (`__proto__` is a key).
- Paths built from names or synced rows stay inside their folder; an
  absolute path joined onto a root replaces it.

## Repository hygiene
No real people, contact details, hostnames, tokens or deployment identifiers in
code, tests, fixtures or docs. Tests use invented examples (example.com).

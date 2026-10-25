# Mobile / API client contract

The authoritative, code-grounded contract for [hearthloom-ios](https://github.com/metavert-io/hearthloom-ios) (and any
other API client). Every endpoint/shape below is verified against
`crates/hearthloom-daemon/src/http.rs` and `crates/hearthloom-core` at the
cited lines — trust this over reverse-engineering the dashboard's fetches.

## Client model (do not violate)

The phone is an **authenticated client + on-device adapter host** — never a
peer, never a copy of the substrate. It holds no SQLite file, runs no daemon,
and never touches `/v1/sync/*` (peer-scoped replication of the whole DB).
It authenticates with a **scoped PAT** and, for the webview, an `hl_session`
cookie. Every write carries `actor` + relies on server-side idempotency.

Base URL is per-connection (`https://<your-node>` — a hosted node or a
self-host origin). TLS is terminated at the edge (Fly) for hosted nodes.

---

## Phase 0 works against TODAY's daemon — no server changes required

### 1. Validate a connection
- `GET /health` → `{"service":"hearthloom","status":"ok","version":"x.y.z"}`
  (unauthenticated). Use to confirm a base URL is a Hearthloom node.
- `GET /v1/auth/me` → `{"auth":"required"|"open","user":<row|null>,"killswitch":bool}`
  (http.rs auth_me). `auth:"required"` means a user row exists → you must
  sign in. `killswitch:true` → show the killswitch screen (node is shutting
  down / in grace); export + vault still work but treat as read-mostly.

### 2. Sign in — two flows, one endpoint
`POST /v1/auth/login` (http.rs auth_login, struct `LoginBody` line 1414).
Send `Accept: application/json` (and JSON body) so you get the JSON contract,
not the browser redirect.

**a. Self-host / direct (email + password):**
```
POST /v1/auth/login   Content-Type: application/json   Accept: application/json
{ "email": "you@example.com", "password": "…" }
```
**b. a hosting provisioner (trusted-issuer HMAC):**
```
{ "issuer_token": "<base64url(payloadJSON)>.<base64url(HMAC-SHA256)>" }
```
The issuer token is minted by the provisioner with the shared secret
`HEARTHLOOM_TRUSTED_ISSUER_SECRET` (set on the tenant). Payload JSON =
`{"sub":"<email>","exp":<unix-seconds>}`; HMAC-SHA256 over the raw payload
bytes; both halves base64url **no-pad** (http.rs verify_issuer_token
line ~1481). `sub` must equal the tenant admin's email.

**Success (JSON caller):** `200 {"ok":true}` plus
`Set-Cookie: hl_session=<token>; HttpOnly; SameSite=Lax; Path=/; Max-Age=…`.
**Failure:** `401` with a human message.

### 3. Two credentials, two jobs
- **`hl_session` cookie** → drives the **WKWebView** (the dashboard is built
  around it). After login, capture the `Set-Cookie` and set it on the
  webview's `WKHTTPCookieStore` for the base URL. Sessions expire → on a
  webview `401`/redirect-to-login, silently re-login (you hold the
  credentials/PAT) and refresh the cookie.
- **Device PAT** (`Authorization: Bearer hl_…`) → drives **native API calls**
  (ingest, capture, vault) that must outlive a session. Long-lived, stored in
  Keychain behind FaceID.

### 4. Mint the device PAT (while holding a session)
`POST /v1/keys` (http.rs keys_create, struct `KeyCreateBody` line 1299).
Requires an authenticated session (send the `hl_session` cookie).
```
{ "actor":"human:<device-name>", "name":"iphone:<device>", "scopes":["read","ingest","notify"] }
   // optional: "expires_at":"<RFC3339>"
→ 200 { "key": { …row… }, "secret":"hl_…" }   // secret is shown ONCE — store it now
```
Valid scopes (api_keys.rs line 18): `read write mcp peer friend notify ingest
admin`. **Device PAT = `read,ingest,notify` only.** Never
`write`/`admin`/`peer`/`friend`. (Reads on `/v1/*` accept `read`; the vault
and notifications are covered below.)
- Revoke: `POST /v1/keys/{id}/revoke` (admin/self), or from the app's own
  token `POST /v1/keys/self-revoke`. Keys also appear in the dashboard
  (☰ → API Keys) for out-of-band revocation.
- Alternative pairing (no in-app login): user mints a PAT in the dashboard and
  pastes it. Support this as a fallback.

### 5. Vault unlock (FaceID-gated)
- `GET /v1/vault` → `{"locked":bool,"seconds_remaining":int}` (0 when locked).
- `POST /v1/vault` (struct `VaultBody` line 4913):
  `{ "actor":"human:<device>", "action":"unlock"|"lock", "minutes":<1..60> }`
  (unlock defaults 15, clamped 1–60). Gate the unlock action behind FaceID.
  While locked, every read excludes `scope='private'` server-side — the app
  never needs to filter, and must not cache private data at rest.

### 6. Notifications (use the `notify` scope surface — NOT the bus)
The push bus (`GET /v1/bus/stream`) is **peer-scoped** and off-limits to the
device PAT. Use the notifications queue instead (http.rs lines 131–134):
- `GET /v1/notifications` (notify scope) — poll for the badge/list.
- `POST /v1/notifications/read-all`, `/{id}/read`, `/{id}/dismiss`.
Foreground polling is fine for Phase 3; background push (APNs) is a later,
separate relay.

---

## Ingest contract (native capture + the HealthKit path)

`POST /v1/ingest` (http.rs ingest) accepts an **`IngestBatch`** (ingest.rs
line 72):
```jsonc
{ "actor":"adapter:apple-health" | "human:<device>",
  "session_id": "…",            // optional
  "episodes": [ EpisodeDraft, … ] }
```
`EpisodeDraft` (ingest.rs line 16):
```jsonc
{ "source":"apple-health",          // adapter id — with source_ref, the idempotency key
  "source_ref":"<stable native id>",// re-ingesting the same ref is a no-op UPSERT
  "kind":"health_day",              // episode kind
  "title":"…", "body":"…",          // body kept verbatim, never discarded
  "event_time": { "utc":"2026-07-23T18:04:00Z", "tz_offset_minutes":-300, "uncertainty":"minute" },
  "trust":"self",                   // self | imported | agent | untrusted
  "scope":"health" | null,          // null = infer server-side
  "entities":[ EntityRef… ], "edges":[ EdgeAssertion… ] }
```
`(source, source_ref)` is the idempotency key — the whole reason re-sends and
overlapping export.xml re-ingests dedupe instead of duplicating. A non-peer
ingest key keeps its `actor` as sent (peer keys get rewritten to
`peer:<name>`). For device capture (a task/note/URL), post the relevant
operational endpoint instead (`/v1/tasks`, `/v1/notes`) or an EpisodeDraft
with `source:"inbox"`-style provenance.

---

## Verification pointers (http.rs unless noted)
auth_me • auth_login (LoginBody 1414) • verify_issuer_token (~1481) •
keys_create (KeyCreateBody 1299) • keys_revoke / keys_self_revoke •
vault_set/vault_status (VaultBody 4913) • ingest (IngestBatch ingest.rs 72,
EpisodeDraft ingest.rs 16, EventTime time.rs 58) • notifications 131–134 •
scopes api_keys.rs 18 • require_scope 433.

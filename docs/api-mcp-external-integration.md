# API & MCP — External Integration Roadmap

Status date: 2026-05-08

The Settings → API & MCP panel exposes the local Garden runtime to outside
consumers (Claude / Codex / agents / scripts that want to talk to the
loopback HTTP or MCP surface). This document captures what's shippable in
v1, what was found wanting in a focused pace-test, and what we've chosen
to defer.

## v1 scope (initial release)

The panel surfaces:

- **API base URL** (`http://127.0.0.1:<port>`)
- **MCP URL** (`http://127.0.0.1:<port>/mcp`)
- **Session token** (full-scope `session-all` Bearer, hidden behind a
  Reveal toggle, with Copy)
- **Runtime profile** pill

A single global token is the integration surface for v1. Users who want
to connect an external tool copy the URL + token and go.

The grant-profile / scoped-client-token / audit-log content is **not
shipped in v1**. See [Deferred to v2+](#deferred-to-v2) for why.

## Findings from the pace test (2026-05-08)

A full HTTP+MCP smoke against the loopback (post-CORS, post-routing
fixes) confirmed solid pass on the core surface:

- `/manifest`, `/graphs` listing + summary + navigation, SPARQL
  count via `/graphs/query`, all 70 MCP tools enumerated, `tools/call`
  for representative tools (`quick_orient`, `recall`, `list_graphs`)
  return clean structured content.
- Negative paths return structured errors (`401` missing/invalid
  bearer, `400` missing required param, `404` unknown graph).
- CORS preflight permissive; cross-origin clients work.

Three gaps emerged that block treating this as a publishable third-party
API today. None of them block in-app daily use.

### Gap 1 — Scoped client tokens are Tauri-IPC-only

`create_loopback_client_token`, `list_loopback_client_tokens`, and
`revoke_loopback_client_token` are `#[tauri::command]` functions in
`src-tauri/src/loopback_client_tokens.rs`. There is **no HTTP route**
exposing them. The Settings panel's grant-profile Issue button works
because it goes via `nativeBridge.createLoopbackClientToken(...)` over
the Tauri IPC bridge — that path is invisible to anything outside the
WebView.

Consequence: an external CLI / agent / script cannot rotate or revoke
its own token over HTTP. The whole grant-profile model only works for
in-app callers. The loopback's only HTTP-reachable identity is the
global `session-all` bearer.

### Gap 2 — No HTTP surface for audit log + audit coverage gaps

Two distinct issues rolled into one:

1. `list_loopback_audit_events` is also Tauri-IPC-only. No HTTP route
   exposes the audit log to external clients.
2. The audit log itself only records **CRDT-path writes** and **token
   lifecycle events**. The pace-test confirmed by triggering each:
   - Read traffic (`GET /graphs`, `GET /navigation/{id}`, etc.) →
     **no audit row**.
   - SPARQL queries via `/graphs/query` → **no audit row**.
   - MCP `tools/list` and MCP `tools/call` → **no audit row**.
   - MCP `remember` (which goes through `local-memory-store`, not
     CRDT) → **no audit row**.

A consumer relying on the audit log to reconstruct activity will miss
most of what an agent does, even via the WebView.

### Gap 3 — Response-shape inconsistency

| Surface | Casing |
|---|---|
| Job envelopes (`/graphs`, `/graphs/stats`) | snake_case (`job_id`, `poll_url`) |
| SPARQL (`/graphs/query`, `/graphs/update`, `/api/sparql/*`) | inline `200` result |
| Navigation, time-travel | camelCase (`graphId`, `nextCursor`) |
| `/graphs/{id}/summary` | mixed (`document_count` snake, but a `graphId` would be expected camel) |
| Time travel | flat cloud-shape on the frontend, graph-segmented on the loopback (frontend-side adapter landed in commit `2c67cc81`) |

Plus a missing-route footgun: `/graphs/stats` exists (account-wide,
async) but `/graphs/{id}/stats` 404s, even though `/graphs/{id}/summary`
is right there.

The original SPARQL routes returned **HTTP 202** with
`status:"succeeded"` already inlined. That false asynchronous facade has
since been removed: SPARQL query/update routes now await the admitted engine
operation and return an inline `200` result. Other job-shaped routes retain
their existing envelopes in this slice; their `202`/already-finished behavior
still needs a separate contract sweep.

## Deferred to v2+

In rough priority order, anchored on what each gap unblocks:

| Item | Effort | Unblocks |
|---|---|---|
| HTTP routes for token CRUD (`POST /loopback/client-tokens`, `GET`, `DELETE`) | Medium (Rust + auth scoping) | External callers can rotate/revoke their own tokens |
| HTTP route for audit-events (`GET /loopback/audit-events`) | Small (mirror `list_loopback_audit_events`) | External callers can read their own audit trail |
| Audit hooks on read paths and MCP tool dispatch | Medium (touch every loopback handler + MCP dispatch) | Audit log actually reflects what an agent did |
| Audit hooks on `local-memory-store` writes | Small | `remember` and friends produce audit rows |
| Casing normalization (pick one, sweep) | Medium-large (affects every adapter + every TS consumer) | First integrator doesn't burn a half-hour |
| `/graphs/{id}/stats` route (mirror of summary) | Tiny | Path matches consumer expectations |
| Normalize remaining `202 status:succeeded` envelopes to truthful inline or genuinely async contracts | Small-medium | Naive clients don't poll work that already finished |

The ordering reflects external-integration value, not effort. Token CRUD
is the highest-leverage v2 work — without it, the whole grant-profile
model in the panel is purely cosmetic for non-WebView callers.

## What to do today

Ship v1 with the simplified panel:

- Connection Info card (URLs, port, session token) only.
- A short note that explains what the token is for and that scoped
  tokens / audit / per-integration credentials are coming.
- The grant-profile and audit-log UI is removed from the panel for now.
  The underlying Tauri commands remain (in case we want to surface
  them later or expose them to advanced users via a debug build) but
  they're not user-facing.

For users who need a scoped token in v1, the workaround is "use the
session token" — it's full-scope, it rotates per launch, and the
loopback is bound to `127.0.0.1` so leakage requires local code
already running as the user.

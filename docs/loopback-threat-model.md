# Local Loopback Threat Model

Status: public-alpha readiness draft.

This document describes the threat model for the local-native Tauri loopback
API and MCP endpoint. It is deliberately conservative: the current prototype is
safe enough for private technical preview, but the compatibility session token
is not a final public security posture.

## Security Boundary

The loopback server binds only to `127.0.0.1` on an OS-assigned random port.
Remote network callers are outside the intended boundary and should not be able
to connect directly.

The local user account is inside the boundary. A process already running as the
same OS user can read files that the user can read, including the per-run
manifest under:

```text
<app-data>/profiles/default/loopback.json
```

That means local malware, invasive browser extensions with local-file access, or
other same-user processes can potentially recover the compatibility bearer token
if they can read the profile directory. Owner-only file permissions reduce
accidental exposure to other users on Unix-like systems; they do not defend
against a same-user compromise.

## Protected Assets

- Local profile metadata and identity files.
- Graph catalog records and graph directories.
- Workspace and document Y.Doc state.
- Original uploaded/imported files.
- Oxigraph projection and user RDF stores.
- Semantic indexes and model cache metadata.
- Loopback client token registry.
- Loopback audit log.
- Local job state and job result files.

## Tokens

There are two token families.

The per-run session token is written to `loopback.json` on disk only. It
currently uses `session-all` scope mode for compatibility with the Tauri app
and local developer clients. It grants every known local loopback and MCP
scope, including write/delete/RDF/admin surfaces. The manifest now labels
this explicitly with:

- `tokenAudience: "tauri-runtime-compatibility"`
- `tokenStorage: "plaintext-owner-only-profile-manifest"`
- `securityWarning: ...`

`GET /manifest` over HTTP never echoes the token (or `pid`/`manifestPath`)
back to callers, for any principal, including one authenticating with the
session-all token itself: the handler returns `LoopbackManifestPublic`, a
separate serialization type constructed from the full manifest with those
three fields dropped, never `LoopbackManifest` itself. Before this
redaction, any client holding a scoped token with `loopback.manifest.read` —
which every read-only grant profile includes by default — could recover the
full-privilege session-all bearer in a single unauthenticated-beyond-scope
request; that was the actual privilege boundary this document's "local user
account is inside the boundary" framing failed to account for, since the
disclosure required no filesystem access at all. The remaining disk-read
threat below (same-user process reading `loopback.json` directly) is
unchanged and still the accepted local-boundary risk.

Named client tokens are created through Tauri commands, returned once to the
caller, stored only as SHA-256 hashes, have bounded expiry, and can be revoked.
They should be the public-facing client model for CLI tools, MCP clients, and
third-party local integrations.

Public alpha posture:

- The session token remains an internal compatibility bridge.
- Third-party clients should receive named client tokens, not the session token.
- Mutating named-token profiles must not be default-granted without consent.
- `session-all` must remain non-default-grantable.

Trusted release posture:

- The app should either narrow the session token, keep it in memory only, or
gate all broad third-party access behind explicit user consent.
- Keychain-backed storage can reduce accidental manifest exposure, but it does
not solve same-user process compromise by itself.

## Browser Origins

The loopback server performs origin checks before bearer-token scope checks.
Allowed origins are intentionally narrow:

- no `Origin` header: non-browser clients such as CLI tools and MCP transports;
- `http://127.0.0.1:<port>` and `http://localhost:<port>`: local development
  and browser-based loopback clients;
- `tauri://localhost` and `http://tauri.localhost`: Tauri WebView origins;
- `null`: file/sandboxed WebView contexts that cannot send a normal origin.

The `Origin` check is defense in depth against accidental browser use and DNS
rebinding-style mistakes. It is not the primary authorization mechanism. Bearer
tokens and scopes remain mandatory for all non-public routes.

Public alpha posture:

- Keep `null` only for the Tauri/WebView compatibility path.
- Do not document `null` as a supported browser integration origin.
- Third-party browser integrations should use named tokens with narrow scopes.

## MCP Clients

MCP clients authenticate with the same bearer-token model as REST. `tools/list`
advertises scope metadata, and `tools/call` enforces scopes before dispatch.
The raw `crdt_operation` MCP tool uses operation-kind scope resolution; callers
do not get a bypass around document/workspace/delete/import permissions.

Public alpha posture:

- MCP clients should use named tokens.
- MCP clients should request the minimum grant profile needed for their role.
- Destructive/admin MCP calls need explicit scopes and should appear in audit
  coverage before being described as public-ready.

## Audit Log

The audit log records token lifecycle events and CRDT operation outcomes without
token values, token hashes, or payload values. Audit append failure must never
crash the app; it should be observable and best-effort.

Current gap:

- Some non-CRDT mutating routes still need audit events before trusted release:
  direct RDF load/update, graph lifecycle, job cancellation, salience writes,
  imports, and upload entry points.

## Revocation

Named client-token revocation updates the token registry. Revoked named tokens
stop authorizing future requests. The per-run session token is revoked by
ending the app run and replacing the manifest on next startup.

Current gap:

- There is no consent UI for issuing broad named tokens.
- There is no in-app surface that distinguishes internal app token use from
  third-party client token use strongly enough for non-technical users.

## Threat Scenarios

### Malicious Remote Web Page

Expected defense: cannot connect remotely; browser origin is checked; bearer
token is required; scopes are enforced.

Residual risk: if the page can obtain a valid token through another channel,
origin checks are not sufficient by themselves.

### Same-User Local Process

Expected defense: owner-only manifest and token registry files prevent casual
cross-user reads.

Residual risk: any process already running as the same user can potentially
read `loopback.json` and use the session token. This is the main compatibility
risk before public release.

### Third-Party MCP or CLI Tool

Expected defense: issue a named token with the smallest viable grant profile.

Residual risk: if the tool is handed `loopback.json`, it receives session-all
authority instead of a scoped grant.

### Lost or Stale Named Token

Expected defense: named token hashes are stored, tokens expire, and tokens can
be revoked.

Residual risk: until revocation UI is complete, users may not know which local
clients still have valid credentials.

### Disk Full or Permission Change

Expected defense: token registry writes must fail closed when credentials
cannot be persisted. Audit writes must fail open and report/log the failure
without crashing the runtime.

## Public Alpha Requirements

- This threat model stays linked from `docs/loopback-security.md`.
- The manifest labels the session token as internal compatibility authority.
- Named client tokens remain scoped, expiring, hashed-at-rest, and revocable.
- Mutating grant profiles remain non-default-grantable.
- Route/MCP scope metadata remains covered by parity inventory.
- Audit append failure is non-fatal.
- Negative authorization tests exist for representative REST and MCP surfaces
  before those surfaces are described as public-ready.

## Follow-On Work

- Consent UI for issuing named tokens.
- Clear visual separation between internal app token and third-party client
  tokens.
- Optional keychain-backed session token storage.
- In-memory-only token mode for headless local runtimes.
- Audit coverage for all mutating non-CRDT routes.
- User-facing local model download and network import trust model.

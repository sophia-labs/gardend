# Local Loopback Security

This document describes the current local-native loopback and MCP security model. It is the implementation contract for the Tauri prototype, not a final public security policy. The public-alpha threat model lives in `docs/loopback-threat-model.md`.

## Boundary

The native app starts a loopback server bound to `127.0.0.1` on an OS-assigned random port. It writes the current API URL, MCP URL, OpenAPI URL, bearer token, token scope mode, token scopes, and structured scope descriptors to the local profile manifest at:

```text
<app-data>/profiles/default/loopback.json
```

The manifest token is per app run. Compatibility mode is still `session-all`: the manifest grants every known local scope to one session bearer token so existing local clients continue to work while route and MCP tool enforcement become explicit. `loopback.json` on disk is the only channel that ever carries the raw token, `pid`, and `manifestPath` — it is written owner-only via `write_secret_json` and is the legitimate local bootstrap read for the app's own frontend and CLI tooling. `GET /manifest` over HTTP returns a secret-free projection of the same data (`LoopbackManifestPublic`: no `token`, `pid`, or `manifestPath`) to every caller, including a request presenting the session-all master token itself — the redaction is per-route, not per-principal, so holding `loopback.manifest.read` (which every read-only client token has by default) can never leak the bearer over the wire. The HTTP response still labels the token's risk posture with `tokenAudience`, `tokenStorage`, and `securityWarning`; third-party clients should receive named scoped tokens instead of reading `loopback.json`. Scope descriptors advertise consent defaults separately: read scopes have `defaultGrant: true`, while write/delete scopes have `defaultGrant: false`. The manifest also includes `grantProfiles` (`read-only`, `authoring`, `rdf-admin`, `session-all`) for named client-token issuance in the native shell.

Named loopback client tokens are stored in the local profile at:

```text
<app-data>/profiles/default/loopback-client-tokens.json
```

The registry is written with secret-file permissions and is greenfield local state; no legacy profile migration is required. Token values are returned only at creation time through Tauri commands, while the registry stores SHA-256 token hashes plus bounded expiry timestamps. REST and MCP bearer authentication accept either the per-run session token or an active named client token, then enforce the effective scope list for that token.

Token creation and revocation also append redacted audit events to:

```text
<app-data>/profiles/default/loopback-audit.jsonl
```

The audit log records event metadata, target token IDs, scope counts, grant profiles, and expiry/revocation times, but never records bearer token values or token hashes. The central CRDT enqueue path also writes success/failure audit events for REST and MCP CRDT operations with operation IDs, operation kinds, graph/document IDs, and payload shape only; payload values are redacted. The Local AI Loopback Access card reads the recent audit tail through a Tauri command.

## Request Authentication

All loopback routes require `Authorization: Bearer <token>` except:

- `GET /health`, which is public and used only for local health probes.
- `GET /artifacts/{graph_id}/images/{image_id}` when called with a signed expiring image URL token.

Browser-origin requests are restricted to localhost/Tauri origins. This is a defense-in-depth check against DNS rebinding and accidental cross-origin use; it does not replace bearer-token validation.

Inline image reads support two modes:

- Bearer clients need the `images.read` scope.
- Browser `<img>` rendering can use the object-specific `token` plus `exp` query parameters generated at upload time.

## Scopes

Scopes are defined in `src-tauri/src/loopback_scopes.rs`. They use concrete operation names such as:

- `graphs.read`, `graphs.write`, `graphs.delete`, `graphs.import`, `graphs.export`
- `documents.read`, `documents.write.crdt`, `documents.delete.crdt`
- `workspace.read`, `workspace.write.crdt`, `workspace.delete.crdt`
- `artifacts.read`, `artifacts.write`, `artifacts.ingest`, `artifacts.delete`
- `rdf.query`, `rdf.update`, `rdf.load`, `rdf.dump`
- `search.lexical.read`, `search.semantic.read`
- `semantic.models.read`, `semantic.models.write`, `semantic.index.read`, `semantic.index.write`, `semantic.index.cancel`
- `mcp.tools.read`, `mcp.tools.call`

Routes enforce scopes in their adapter handlers. Compound operations require compound scopes. For example graph export requires both `graphs.export` and `rdf.dump`; document duplicate requires document read/write plus workspace write.

Scope descriptors expose `defaultGrant` for the consent UI. It is not the current session token's effective grant list. Read scopes are default-grantable; write and delete scopes are consent-only even though the compatibility `session-all` token still carries them.

Grant profiles expose common scope bundles for named client tokens:

- `read-only`: every read scope; default-grantable.
- `authoring`: document/workspace/artifact/image/wire/entity writes needed for editing, but no delete/admin scopes.
- `rdf-admin`: RDF query, dump, load, and update.
- `session-all`: every known scope for compatibility; not default-grantable.

The generic CRDT queue entry points authorize by operation kind. For example `document.write` requires `documents.write.crdt`, `workspace.deleteDocument` requires both `documents.delete.crdt` and `workspace.delete.crdt`, `workspace.putArtifact` requires workspace write plus artifact write, and import operations require their import or ingest scopes. This applies to both REST (`POST /api/crdt/operations`) and MCP (`crdt_operation`).

## Contract Visibility

The local surface registry is `parity/local-loopback-surface.json`. It records every local loopback route and MCP tool with status and scopes.

Generated OpenAPI (`parity/local-openapi.json` and `/openapi.json`) emits:

- `x-sophia-required-scopes`
- `x-sophia-scope-mode`

Known scope modes:

- `all`: all listed scopes are required.
- `public`: no bearer token or scope is required.
- `json-rpc-method`: `/mcp` chooses read versus call scope from the JSON-RPC method.
- `bearer-or-signed-url`: bearer clients need the listed scope, while signed URL clients use an object token.
- `operation-kind`: the route chooses required scopes from a typed operation-kind map.

MCP `tools/list` adds `_meta.sophia.local.requiredScopes` and `_meta.sophia.local.scopeMode` to every local tool. Dynamic tools also expose `_meta.sophia.local.requiredScopeMode` (`all` or `operation-kind`); `crdt_operation` includes `_meta.sophia.local.operationScopes` so clients can see the operation-kind map before calling the raw queue tool.

## Drift Checks

The static inventory gate validates the contract chain:

```bash
pnpm parity:openapi
pnpm parity:inventory
```

The inventory checker verifies that:

- `loopback_router.rs` exposes no undeclared local routes.
- Every declared route has valid scopes from `loopback_scopes.rs`.
- Route scopes in `local-loopback-surface.json` match Rust handler enforcement.
- Generated OpenAPI route scope extensions match the registry.
- Every MCP tool has valid scopes in `local-loopback-surface.json`.
- Every MCP tool has catalog schema and Rust dispatch coverage.
- The `crdt_operation` surface scope union matches Rust operation-kind rules.

The live loopback checker additionally verifies live MCP `tools/list` scope metadata against the same registry when the app is running:

```bash
pnpm parity:loopback
```

## Tauri Shell Policy

The Tauri shell has a non-null CSP in `src-tauri/tauri.conf.json`. It allows local loopback/API/image access and Tauri IPC while denying object embedding and external frame ancestors.

The default capability in `src-tauri/capabilities/default.json` allows only event listen/unlisten permissions for the frontend. It does not grant broad `core:default`.

## Import Fetch Policy

Web imports validate outbound URLs before initial fetches and redirects. Localhost, private, link-local, multicast, and reserved network targets are blocked so loopback-capable desktop clients cannot be used as a local network fetch proxy.

## Remaining Hardening

The current `session-all` token is a compatibility bridge. Public-ready hardening still needs:

- Richer consent UI for scoped token issuance.
- Audit records for non-CRDT mutating REST and MCP operations such as direct RDF load/update, graph lifecycle, job cancellation, salience, and import/upload entry points.
- Contract tests for negative authorization cases across REST and MCP.
- A user-facing trust model for network imports and local model downloads.

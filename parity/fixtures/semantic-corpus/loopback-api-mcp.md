# Loopback API and MCP Surface

The native app exposes a localhost API and MCP endpoint while it is running. The
loopback server binds only to 127.0.0.1, uses a per-run bearer token, writes a
manifest to the profile directory, and rejects unexpected browser origins. This
lets developer tools, local agents, and parity harnesses exercise the same graph
operations that the UI uses without embedding cloud credentials into local mode.

## Route Families

- Health, manifest, profile, and capability routes describe the running local profile.
- Graph routes create, list, query, update, and expose job-style envelopes.
- Document routes list, read, delete, expose blocks, and provide block context.
- Navigation routes expose folders, artifacts, and workspace-shaped projections.
- Search routes cover document, block, hybrid, and semantic retrieval.
- RDF and SPARQL routes load, dump, query, and update the graph store.
- MCP tools wrap the same surfaces for agent clients.

The surface can be compatible without being identical internally. Hosted API
calls may enqueue Redis jobs and wait for workers; local calls may enqueue a
typed CRDT operation and wait for the frontend runtime to complete it. Both
should produce bounded waits, error records, status/result shapes, and enough
metadata for a harness to compare behavior.

## Security Notes

The loopback API is not a public network service. The token should rotate on
startup, secrets should never be printed by test harnesses, and the manifest
path should be treated as local profile state. A future hardened build can add
scoped tokens, short-lived grants, and explicit user consent before enabling
external MCP clients.

## Harness Expectations

This fixture should be discoverable with queries for `per-run bearer token`,
`typed CRDT operation`, `job-style envelopes`, `MCP tools`, and `origin
headers`. The document intentionally mentions both REST and MCP because parity
tests need to catch drift between the two adapter layers.

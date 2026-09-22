# Security Policy

Garden is local-first and pre-release. Treat the loopback API, MCP endpoint,
local files, model caches, and imported artifacts as security-sensitive
surfaces even though the app binds only to localhost.

For the current loopback/MCP boundary, token-scope contract, parity-drift
checks, Tauri CSP/capability policy, and threat model, see
[`docs/loopback-security.md`](docs/loopback-security.md) and
[`docs/loopback-threat-model.md`](docs/loopback-threat-model.md).

## Supported Versions

Only the current development branch is supported. No public release channel
exists yet.

## Reporting

**Report security issues privately to:** <vera@sophia-labs.com>

Do not open a public issue for a suspected vulnerability. Email the address
above and allow a reasonable window for triage and a fix before any public
disclosure.

Include:

- the affected route, MCP tool, Tauri command, file path, or model/runtime path;
- reproduction steps from a fresh local profile when possible;
- whether the issue affects read access, writes/deletes, token scope bypass,
  SSRF/network fetch, path traversal, local file disclosure, or persistence;
- logs or parity fixture changes with secrets and local file contents redacted.

## Local Security Expectations

- Loopback REST and MCP require bearer tokens except explicitly documented
  signed local asset URLs.
- Named client tokens should be scoped, revocable, expiring, and audited.
- Web imports must reject localhost, private, link-local, multicast, reserved,
  and redirected private-network targets.
- RDF imports and SPARQL updates must not target reserved projection or profile
  named graphs.
- Multipart uploads should stream into pending files instead of buffering large
  payloads in memory.
- Public contract changes require parity inventory updates and focused tests.

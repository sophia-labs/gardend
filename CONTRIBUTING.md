# Contributing to Garden

Garden is a local-first knowledge-graph engine from Sophia Labs, licensed
**source-available** under the [PolyForm Noncommercial License 1.0.0](LICENSE)
— not open source / OSS. The repo was extracted from the `mnemosyne-platform`
prototype on 2026-05-08.

**Public boundary vs. internal desktop app.** The Tauri desktop app (the
native shell plus the Shrubbery frontend) is Sophia Labs' internal product
surface: its dev/build/CI require a private Shrubbery checkout and a
`SHRUBBERY_READ_TOKEN` credential external contributors do not have (see
`.github/workflows/release.yml`, `native-tauri-prototype.yml`, and
`docs/release-process.md`). `frontend/` is Garden's native-shell adapter
code, not a vendored, self-sufficient copy of the UI.

External contributions target the **public headless boundary**: the
`gardend` engine (same Rust core, built with `--no-default-features
--features headless --example gardend`, no Tauri/webview dependency) and
`sophia-mcp`. See [`docs/headless-engine.md`](docs/headless-engine.md) for
the build/test contract — it needs zero organization credentials. See
[`docs/oss-readiness.md`](docs/oss-readiness.md) for the remaining gates
between today's state and a first public release of that boundary.

Contributions should follow the boundaries expected from that eventual
release, even while gates are still being closed. If your change only
touches internal desktop/Shrubbery integration code, note that explicitly —
it is out of scope for public review of the headless boundary.

## Scope

- Keep CRDT state authoritative. Do not add write paths that mutate manifests,
  RDF, search indexes, or workspace projections as independent sources of truth.
- Put behavior in service modules first, then expose it through Tauri, loopback
  REST, or MCP adapters.
- Prefer typed request/response structs and `AppResult<T>` service errors over
  loose `serde_json::Value` and `Result<T, String>` surfaces.
- Keep local execution local-native. Do not add hosted infrastructure shims just
  to mimic Redis, S3, workers, or cloud auth.
- Update docs and parity fixtures when public contracts, storage layout, scopes,
  or security behavior change.

## Validation

### Public headless gate (no org credentials needed)

For changes scoped to the `src-tauri` engine, run the headless build/test
contract in [`docs/headless-engine.md`](docs/headless-engine.md):

```bash
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml --no-default-features --features headless
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --lib
./src-tauri/build-gardend-headless.sh
```

This is the gate external contributors can run end to end.

### Internal desktop gate (Sophia Labs only — requires Shrubbery credentials)

The full native/desktop validation gate additionally builds and tests the
Shrubbery frontend, which requires the private Shrubbery checkout and
`SHRUBBERY_READ_TOKEN`:

```bash
pnpm --dir frontend install
pnpm frontend:build
pnpm --dir frontend exec vitest run src/crdt src/native --testTimeout=15000
cargo fmt --check --manifest-path src-tauri/Cargo.toml
RUSTFLAGS="-D warnings" cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --lib
pnpm parity:inventory
```

Live loopback parity checks require the native app to be running and are useful
for route, MCP, ingestion, and persistence behavior changes:

```bash
pnpm parity:loopback
pnpm parity:compare --include-mutations --include-mcp-fidelity --markdown
```

Do not broaden the native gate to repo-wide frontend `tsc --noEmit` until the
known generated-client and unrelated component debt is cleared.

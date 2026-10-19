# Contributing to Garden

Garden is a local-first knowledge-graph engine from Sophia Labs, licensed
**source-available** under the [PolyForm Noncommercial License 1.0.0](LICENSE)
— not open source / OSS. The repo was extracted from the `mnemosyne-platform`
prototype on 2026-05-08.

**One repository.** This is the canonical repository for Garden: there is no
private mirror and no export step. The headless engine, the Tauri desktop shell and the
release pipeline live here, and the UI is the public
[Shrubbery](https://github.com/sophia-labs/shrubbery) pinned in
`shrubbery-ui.lock.json`. Nothing here needs an organization credential
(`docs/desktop-from-source.md`, `docs/release-process.md`).

Contributions most often target the **headless engine**: the
`gardend` engine (same Rust core, built with `--no-default-features
--features headless --example gardend`, no Tauri/webview dependency) and
`sophia-mcp`. See [`docs/headless-engine.md`](docs/headless-engine.md) for
the build/test contract — it needs zero organization credentials. See
[`docs/oss-readiness.md`](docs/oss-readiness.md) for the remaining gates
between today's state and a first public release of that boundary.

If your change only touches the desktop shell or its Shrubbery integration,
say so in the pull request.

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

## Run these locally before opening a PR

CI runs the same commands, so a green local run is a green PR. From the repository root:

```bash
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --lib
pnpm parity:inventory    # only if you touched routes, MCP tools, scopes, storage or security
```

The first Rust build is a cold compile of a large dependency tree (tens of minutes). Do not pay it twice: the **build
service** keeps one warm cargo target on your machine, across revisions and worktrees, and builds only what changed.

```bash
scripts/garden-build-service.py start                 # once; `unit launchd|systemd` prints a unit to keep it running
GARDEND_BIN=$(scripts/garden-build-service.py build)  # builds this worktree's gardend, prints the binary's path
scripts/garden-build-service.py test                  # cargo test --lib (headless); prints the log's path
scripts/garden-build-service.py build --rev origin/main
```

It is local only (127.0.0.1) and uses your own cargo; `scripts/garden-build-service.py --help` lists the rest.

Need a real gardend without building one? Every commit on `main` has a prebuilt binary on the
[`main-latest`](https://github.com/sophia-labs/gardend/releases/tag/main-latest) prerelease, named
`gardend-<sha>-<target>`. Shrubbery's `pnpm test:real` fetches it for you.

Tests that load an embedding model use fastembed, which keeps model files in `HF_HOME` when it is set, else in
`FASTEMBED_CACHE_DIR`, else in `.fastembed_cache` under the working directory. Point one of them at a directory you keep
between runs, so the model is fetched once. A few suites are order-dependent and
run isolated; the exact commands are in [`docs/headless-engine.md`](docs/headless-engine.md) and in CI. The suites that
need operator data (`GARDEN_PRESERVATION_*`) are `#[ignore]`d; `-- --include-ignored` runs them if you have the data.

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

### Desktop gate

The full native/desktop validation gate additionally builds and tests the
Shrubbery frontend, from the public checkout named in the lock:

```bash
pnpm frontend:build   # fetches the locked public Shrubbery and builds its bundle
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

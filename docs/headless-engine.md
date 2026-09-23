# Headless Engine — Public Build/Test Contract

This is the **public, credential-free** build/test contract for Garden's
headless engine, `gardend`. It is the supported public artifact (together
with [`sophia-mcp`](https://github.com/sophia-labs/sophia-mcp)) under
Garden's source-available boundary — see the note at the top of the repo
[`README.md`](../README.md) and [`../EXPORT-MANIFEST.md`](../EXPORT-MANIFEST.md).

Everything on this page needs **only** a Rust toolchain and this
repository's `src-tauri/` tree. It does **not** need:

- the private Shrubbery frontend checkout,
- a `SHRUBBERY_READ_TOKEN` (or any other org credential),
- `pnpm` / Node, or
- the Tauri CLI.

Those are only needed for the internal desktop app (see
[`docs/release-process.md`](release-process.md) and the "internal desktop
gate" in [`CONTRIBUTING.md`](../CONTRIBUTING.md)) and are out of scope here.
Do not follow the Shrubbery/frontend contract-suite steps from
`.github/workflows/native-tauri-prototype.yml`'s `headless` job (its `Test
Rust native shell (headless cell features)` and `Build the exact Gardend
integration binary` steps are the token-free parts this doc mirrors; the
rest of that job checks out the private Shrubbery UI and is internal-only).

## What `gardend` is

`gardend` is the same Rust engine that powers the Garden desktop app —
loopback REST/MCP server, storage, CRDT queue, scheduler, Oxigraph RDF
store, semantic index — built with **no Tauri, no webview, no windowing, no
desktop plugins**. See [`headless-gardend.md`](headless-gardend.md) for the
full architecture (why it's a Cargo `[[example]]`, the `--no-default-features`
feature-flag mechanics, and how `sophia-mcp` spawns it). This page is the
narrower "how do I build, test, and run it" contract.

## Prerequisites

- **Rust 1.88+** (matching `src-tauri`'s declared MSRV; `Dockerfile.gardend`
  builds with `rust:1.95-bookworm`).
- A C toolchain (`clang`/`cmake` + `pkg-config`) for Oxigraph's RocksDB
  backend (`oxrocksdb-sys`). On Debian/Ubuntu:
  ```bash
  sudo apt-get install -y clang libclang-dev cmake pkg-config libssl-dev
  ```
  On macOS, Xcode Command Line Tools (`xcode-select --install`) provide
  `clang`; install `cmake` via Homebrew if you don't already have it.
- No Node/pnpm, no Tauri CLI, no GitHub token of any kind.

## Build

From the repo root:

```bash
cd src-tauri
cargo build --release --no-default-features --features headless --example gardend
```

Output: `src-tauri/target/release/examples/gardend`.

Convenience wrapper (same command):

```bash
./src-tauri/build-gardend-headless.sh          # release build
./src-tauri/build-gardend-headless.sh --check  # cargo check only, fast, no codegen
```

A fast compile-only check without the release build:

```bash
cargo check --manifest-path src-tauri/Cargo.toml --no-default-features --features headless
```

## Test

The headless feature set gates test suites the default desktop build never
compiles (Emporium reconcile, memory, SHACL evidence, etc.). This is the
public test contract — the exact commands `.github/workflows/ci.yml` runs
(placed there from `export/github/workflows/ci.yml` on export; see
[`EXPORT-MANIFEST.md`](../EXPORT-MANIFEST.md)), verified against this tree.

Main run:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --lib -- \
  --skip preservation_v2 \
  --skip owned_restore \
  --skip restored_documents_rematerialize_even_when_marker_floor_ran_ahead_of_wall_clock \
  --skip harness_fresh_parity_old_vs_new_net_state_identical \
  --skip semantic_model_state \
  --skip cell_self_heal_tests \
  --skip host_dry_run_apply_and_replay_preserve_the_exact_site_route
```

Three suites are order-dependent — they fail when run in-process with the rest
of the library suite but pass cleanly on their own. Run them isolated:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --lib semantic_model_state -- --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --lib graph_paths::cell_self_heal_tests::cell_self_heal_materializes_a_never_created_graph_on_first_query -- --exact --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --lib emporium::site_projection_tests::headless::host_dry_run_apply_and_replay_preserve_the_exact_site_route -- --exact --test-threads=1
```

Then confirm the exact integration binary CI ships also builds:

```bash
cargo build --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --example gardend
```

**Known exclusions:**

- `preservation_v2`, `owned_restore` — need private `GARDEN_PRESERVATION_*`
  fixture archives not available to public contributors.
- `semantic_model_state::tests::*` (9 tests) — order-dependent: a shared
  profile-env mutex gets poisoned by an unrelated panicking test when run
  in-process with the rest of the suite. Passes in isolation (above); run
  separately with `--test-threads=1`.
- `graph_paths::cell_self_heal_tests::cell_self_heal_materializes_a_never_created_graph_on_first_query`
  — order-dependent: a `OnceLock` gets latched by a co-scheduled test.
  Passes in isolation (above); run separately with `--exact`.
- `emporium::site_projection_tests::headless::host_dry_run_apply_and_replay_preserve_the_exact_site_route`
  — order-dependent: its temporary store can be reopened by a co-scheduled
  test ("lock hold by current process"). Run separately with `--exact`.
- `time_travel_restore_service::tests::restored_documents_rematerialize_even_when_marker_floor_ran_ahead_of_wall_clock`
  and `document_meaningful_object::harness_tests::harness_fresh_parity_old_vs_new_net_state_identical`
  — known pre-existing failures, tracked, not fixed by this contract.

Formatting is **not** currently an enforced gate: `cargo fmt --check` is not
clean on `main` (hundreds of hunks across live worktrees, and a repo-wide
reformat would conflict with them), so CI does not run it.

Optional additional static cross-check (needs `node`, `jq`, and `rg` —
verifies the SPARQL admission/cancellation invariants documented in
[`sparql-admission-and-cancellation.md`](sparql-admission-and-cancellation.md)
haven't silently regressed):

```bash
./scripts/check-sparql-admission.sh
```

## Run

`gardend` needs almost nothing to boot. The one setting you should always
set explicitly outside a throwaway smoke test is the profile directory:

```bash
GARDEN_PROFILE_DIR=/tmp/gardend-profile \
  ./src-tauri/target/release/examples/gardend
```

`GARDEN_PROFILE_DIR` is **required in containers** (there is no OS app-data
directory to fall back to) and is good practice everywhere else — it pins
where the cell's local state (graph catalog, CRDT journals, Oxigraph store,
job registry, the loopback manifest) lives, and lets you throw away a
scratch profile between runs.

Other loopback bind knobs, all optional (defaults shown):

| Var | Default | Meaning |
|-----|---------|---------|
| `GARDEN_LOOPBACK_HOST` | `127.0.0.1` | Bind host. Use `0.0.0.0` in a container/pod. |
| `GARDEN_LOOPBACK_PORT` | `0` (OS-assigned) | Bind port. |
| `GARDEN_LOOPBACK_TOKEN` | random UUID per run | Fixed bearer token, useful for scripting against a known token instead of reading it back out. |
| `RUST_LOG` | `info` | Log filter (`env_logger`; all diagnostic output goes to stderr — stdout is reserved for CaptureEvent NDJSON). |

See [`headless-gardend.md`](headless-gardend.md#cell-runtime-knobs-env) for
the fuller knob list (durable-plane flush tuning, cell-lease/owner-scoping
vars used only by platform-next, observatory testimony). None of those are
needed for a local build/test/smoke loop.

### What `loopback.json` gives you

On boot, `gardend` writes a manifest to `$GARDEN_PROFILE_DIR/loopback.json`
(unlike the desktop app, which nests it under
`<app-data>/profiles/default/loopback.json` — headless mode uses
`GARDEN_PROFILE_DIR` directly as the profile root). It contains, among other
fields:

- `port` — the actual bound port (useful when `GARDEN_LOOPBACK_PORT=0`).
- `apiUrl`, `mcpUrl`, `openapiUrl` — the loopback REST base, the `/mcp`
  endpoint, and the generated OpenAPI document.
- `token` — the bearer token to send as `Authorization: Bearer <token>`.
- `pid`, `startedAt` — process identity for liveness checks.

A minimal smoke loop:

```bash
GARDEN_PROFILE_DIR=$(mktemp -d) \
  ./src-tauri/target/release/examples/gardend &
GARDEN_PID=$!

# wait for the manifest, then read it
until [ -f "$GARDEN_PROFILE_DIR/loopback.json" ]; do sleep 0.2; done
PORT=$(python3 -c "import json; print(json.load(open('$GARDEN_PROFILE_DIR/loopback.json'))['port'])")
TOKEN=$(python3 -c "import json; print(json.load(open('$GARDEN_PROFILE_DIR/loopback.json'))['token'])")

curl -s "http://127.0.0.1:$PORT/health"
curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/openapi.json" | head -c 200

kill "$GARDEN_PID"
```

`/health` and signed inline-image URLs are the only routes that don't
require the bearer token. See
[`loopback-security.md`](loopback-security.md) for the full route/scope
contract and threat model.

## What this contract deliberately excludes

- The Shrubbery frontend contract suite
  (`pnpm --dir "$SHRUBBERY_DIR" test:run` and friends) — that suite validates
  the private, internal-only UI and is not part of the public boundary.
- Desktop packaging, signing, notarization — see
  [`release-process.md`](release-process.md) (internal).
- `parity:*` npm scripts that shell out to the frontend toolchain — those
  need `pnpm`/Node and, for some, a running desktop app; they are part of
  the internal validation gate in `CONTRIBUTING.md`, not this contract.

If you hit a build or test failure that traces back to a file outside
`src-tauri/` (or the `parity/*.json` contract files it `include_str!`s —
see `EXPORT-MANIFEST.md`), that's a genuine gap in this boundary; please
file it rather than reaching for the Shrubbery checkout.

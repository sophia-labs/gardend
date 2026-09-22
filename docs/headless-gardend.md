# Headless `gardend` — the GUI-free garden cell

`gardend` is the **lightweight, GUI-free** build of the garden core. It boots the
full local engine — loopback REST/MCP server, storage, CRDT queue, scheduler,
RDF/oxigraph, semantic index — on Tauri's **MockRuntime**. No webview, no `wry`,
no windowing (`tao`), no desktop plugins (`tauri-plugin-opener` /
`tauri-plugin-log`).

It serves two roles from one binary:

1. **The per-graph cell** for platform-next (one `gardend` process per graph,
   fronted by the gateway; durable plane via `GARDEN_DURABLE_DIR`).
2. **The batteries-included local backend that `sophia-mcp` spawns.** With
   `sophia-mcp --backend local` (the default), `sophia-mcp` launches a `gardend`
   subprocess, reads `<profile>/loopback.json`, polls `/health`, and proxies the
   MCP client (Claude Code, etc.) to the cell's loopback `/mcp`. Bundling
   `gardend` is what makes the local sophia-mcp store "batteries included" — the
   user needs no separate server, just the one headless binary on disk.

## Build

```sh
cargo build --release --no-default-features --features headless --example gardend
```

Convenience equivalents (all run the exact command above):

```sh
# from src-tauri/
./build-gardend-headless.sh          # release build
./build-gardend-headless.sh --check  # cargo check only (fast, no codegen)
cargo gardend                        # cargo alias (.cargo/config.toml)
cargo gardend-check                  # cargo check alias

# from repo root, via pnpm
pnpm --dir frontend build:gardend
```

Output: `src-tauri/target/release/examples/gardend`.

### Why an `[[example]]`, not a `[[bin]]`

`gardend` lives at `src-tauri/examples/gardend.rs` and is declared as an
`[[example]]` (with `required-features = ["headless"]`), not a second `[[bin]]`.
The reason is the **desktop bundler**: `tauri-cli`'s macOS/Windows bundler copies
*every* manifest `[[bin]]` into the app bundle. A second bin that the default
(desktop) build never compiles makes `tauri build` fail at the bundle step with
`failed to copy binary … gardend does not exist`. Examples are invisible to the
bundler, so the desktop `.app`/`.dmg` builds cleanly while `gardend` stays a
real, standalone binary — it just lands under `examples/`. Keeping it an example
also lets it share the garden crate's manifest dir (so `generate_context!()`
finds `tauri.conf.json` and `build.rs`'s capability swap runs) with no
dependency duplication.

### Why `--no-default-features`

The crate's default feature is `desktop`, which turns on `tauri/wry` (the
webview runtime) plus the desktop-only plugins. `--no-default-features` drops
those; `--features headless` selects Tauri's `test` (MockRuntime) backend and
`env_logger`. GUI-only modules — `window_settings` (native title-bar /
NSWindow), the native invoke-handler, onboarding seed — are `#[cfg(feature =
"desktop")]` and are compiled out of the headless build entirely.

`build.rs` swaps the Tauri capability set to `capabilities-headless/*.json`
(no `opener:default` permission) when building headless, so `tauri-build`
validation passes without the desktop plugins.

> The `desktop` and `headless` features are mutually exclusive in practice:
> building with both leaves `gardend`'s `main()` as the desktop stub that
> errors out. Always pass `--no-default-features` for the headless cell.

## What `sophia-mcp` expects

`sophia-mcp` (the stdio MCP proxy) discovers the cell binary by name `gardend`:

- via `SOPHIA_MCP_GARDEN_BIN` (explicit path), or
- on `PATH` (drop `target/release/examples/gardend` somewhere on `PATH`, or
  point the env var at it).

It then runs the cell against a profile dir (`SOPHIA_MCP_PROFILE_DIR`, default
`~/.sophia-mcp/profile`) and waits for `/health`. Use a **fresh profile per
run** — a stale store lock makes the spawn fail.

## Cell runtime knobs (env)

| Var | Meaning |
|-----|---------|
| `GARDEN_PROFILE_DIR` | Profile directory (required in containers). |
| `GARDEN_CELL_GRAPH_ID` | Enables the hosted [single-graph ownership boundary](headless-single-graph-boundary.md). Platform cells set this to their exact graph slug; local `sophia-mcp` headless runtimes leave it unset and remain multi-graph. |
| `GARDEN_DURABLE_DIR` | Durable NFS/EFS snapshot dir; enables hydrate/flush (EFS cell pattern). |
| `GARDEN_FLUSH_INTERVAL_SECONDS` | Dirty-driven durable-flush max-RPO ceiling — forces a flush at least this often while dirty, regardless of debounce (default 30). |
| `GARDEN_FLUSH_DEBOUNCE_SECONDS` | Dirty-driven durable-flush debounce window — flush this long after the last observed write, coalescing bursts (default 5). |
| `GARDEN_LOOPBACK_HOST` | Bind host (default `127.0.0.1`; use `0.0.0.0` in pods). |
| `GARDEN_LOOPBACK_PORT` | Bind port (default `0` = OS-assigned). |
| `GARDEN_LOOPBACK_TOKEN` | Fixed bearer token (default: random per run). |
| `GARDEN_TOKIO_WORKERS` | Worker-thread floor (default ≥6 so `/health` stays schedulable under import load). |
| `SOPHIA_OBSERVATORY_CAPTURE_ENABLED` | Headless cell lifecycle CaptureEvent testimony (default `false`; see [observatory-testimony.md](observatory-testimony.md)). |
| `SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256` | Exact ratified CaptureEvent v0.1 bundle identity; required only when testimony is enabled. |
| `RUST_LOG` | Log filter (default `info`). |

## Verifying the build is GUI-free

```sh
# wry / tao / tauri-runtime-wry must be ABSENT from the headless graph:
cargo tree --no-default-features --features headless -i wry              # errors: no match
cargo tree --no-default-features --features headless -i tauri-runtime-wry # errors: no match

# the desktop graph, by contrast, DOES pull them:
cargo tree --no-default-features --features desktop -i wry
```

Note: on macOS the `tauri` core crate still pulls `objc2-*` type bindings
(including `objc2-web-kit`) as unconditional platform deps even without `wry`.
These are bindings, not a running webview — the MockRuntime never instantiates
one. The headless binary remains free of the actual webview runtime (`wry`),
the windowing layer (`tao`), and the desktop plugins.

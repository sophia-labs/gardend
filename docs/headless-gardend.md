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

## Self-hosting: `gardend serve`

`gardend` with a verb is a friendly way to run your own Sophia on your own
machine. With **no** verb it is the hosted cell, exactly as before (the knobs
below); nothing here changes that path. The step-by-step guide, from a
"Download my gardens" archive to a Sophia reached from anywhere through a
Cloudflare Tunnel with Cloudflare Access as the login, is
[`self-host-cloudflare-tunnel.md`](self-host-cloudflare-tunnel.md).

### `gardend serve`

```sh
gardend serve [--profile <dir>] [--port <n>]
```

Once the API answers it prints, on stderr:

```
Sophia is ready.

  Open Sophia: http://127.0.0.1:7674/#key=sophia-key-…

  profile  /Users/ana/Library/Application Support/Sophia
  ui       /usr/local/lib/gardend/ui
  mcp      http://127.0.0.1:7674/mcp (Authorization: Bearer <key>)
  key      /Users/ana/Library/Application Support/Sophia/selfhost-key.json
```

- **Port:** `--port`, else `GARDEN_LOOPBACK_PORT`, else **7674** (S-O-P-H on a
  phone keypad). A taken port is refused before boot, with a hint to pick
  another.
- **Bind:** 127.0.0.1. Setting `GARDEN_LOOPBACK_HOST` to anything that is not
  loopback prints a warning: the key grants everything and gardend has no TLS.
  Reach Sophia from elsewhere through a tunnel to 127.0.0.1 instead.
- **Profile:** `--profile <dir>` (also `--profile-dir`), else
  `GARDEN_PROFILE_DIR`, else the per-OS default:

  | OS | default profile |
  |---|---|
  | macOS | `~/Library/Application Support/Sophia` |
  | Linux | `$XDG_DATA_HOME/sophia`, else `~/.local/share/sophia` |
  | Windows | `%LOCALAPPDATA%\Sophia` |

  It is created (owner-only) if missing. It is not the desktop app's profile.
  An unpacked "Download my gardens" tar (`graphs/<graph-id>/…`) is a profile.
- **The key in the link** is the profile's **master key** (every scope), the
  loopback token `serve` runs with. It is minted once and kept owner-only in
  `<profile>/selfhost-key.json`, so the link survives restarts; delete that
  file and restart to rotate it. `GARDEN_LOOPBACK_TOKEN`, when set, is used
  instead and not stored. The key rides in the URL fragment, so it never
  reaches a server log; the app stores it and strips it. The link carries the
  master key (not a minted client token) so its owner has every scope. The live
  editor's WebSocket rooms also accept a scoped token that holds the room's write
  scope (`authoring` and `member` do). Give other people and agents scoped
  tokens (below, `--grant member`), never this link.
- **The app:** when `GARDEN_UI_DIR` is unset and a `ui/` folder with an
  `index.html` sits next to the gardend binary (as invoked, or with symlinks
  resolved), `serve` sets `GARDEN_UI_DIR` to it. Without one, Sophia serves its
  API and MCP only and says so.
- **Logs:** quiet by default (`RUST_LOG=warn,gardend=info`); set `RUST_LOG=info`
  for the engine's full log.
- **Already running:** if a gardend already serves the profile, `serve` prints
  its link and exits 0.
- When stderr is not a terminal (a service log), the key is left out of the
  printed link. `gardend url [--profile <dir>]` prints the full link at any time.

Ctrl-C (SIGTERM) stops it after the final flush.

### `gardend token`

```sh
gardend token create --label my-agent --grant member --expires-days 365
gardend token list
gardend token revoke <id>        # the full id, or the 8 characters `list` shows
```

Scoped, expiring client tokens for the profile, sent as
`Authorization: Bearer <token>` to the REST API and `/mcp`. In `token`
commands `--grant` is the grant (`--profile` is an alias): `read-only` (the
default), `authoring`, `member`, `rdf-admin` or `session-all`; the profile
directory is `--profile-dir`, else `GARDEN_PROFILE_DIR`, else the default above.
Give agents and other people **`member`**: the grant Cloudflare Access members
get (below), which is every read, the `authoring` writes and MCP tool calls.
`read-only` and `authoring` cannot call MCP tools (`mcp.tools.call`), and
`authoring` lacks most reads.
`--expires-days` is 1 to 365 (default 30). `create` prints the secret once
(`--json` for scripts); the profile keeps only its SHA-256 hash in
`loopback-client-tokens.json`, and every create and revoke is appended to
`loopback-audit.jsonl` with actor `gardend-cli`.

A running gardend needs no telling: the token store is re-read on every
request, and writes are atomic renames, so a new token works and a revoked one
stops on the next request. These are the tokens the desktop app's token
commands mint, through the same service (`loopback_client_token_service`).

### `gardend service`

```sh
gardend service install [--profile <dir>] [--port <n>] [--print]
gardend service status
gardend service uninstall
```

`install` writes and starts a user service running
`gardend serve --profile <dir> --port <n>` at login, then prints the link:

| OS | service | logs |
|---|---|---|
| macOS | LaunchAgent `~/Library/LaunchAgents/dev.sophia.gardend.plist` (restart after a crash, 60 s to stop) | `~/Library/Logs/Sophia/gardend.log` |
| Linux | `systemd --user` unit `~/.config/systemd/user/gardend.service` (`Restart=on-failure`, `TimeoutStopSec=60`) | the journal: `journalctl --user -u gardend -f` |
| Windows | not yet: `install` says so; run `gardend serve` yourself | |

- The profile and port are written into the service as absolute values. The
  key is not: the service reads it from the profile.
- `install` copies these from your shell into the service when set:
  `GARDEN_UI_DIR`, `GARDEN_CF_ACCESS_TEAM_DOMAIN`, `GARDEN_CF_ACCESS_AUD`,
  `GARDEN_CF_ACCESS_ADMIN_EMAILS`, `GARDEN_ALLOWED_ORIGINS`,
  `GARDEN_PUBLIC_BASE_URL`, `GARDEN_LOOPBACK_HOST`, `GARDEN_NO_UPDATE_CHECK`,
  `RUST_LOG` and `PATH`. Re-run `install` after changing them.
- `--print` shows the file it would write and changes nothing.
- On Linux the unit runs while you are logged in; `install` tells you when
  `sudo loginctl enable-linger $USER` is needed to keep it running after logout.
- `status` exits 0 when the service runs, 3 when it does not. `uninstall` stops
  and removes the service and leaves the profile alone.

### `gardend load`

```sh
gardend load --graph <id> [--profile <dir>] [--format nt|nq|ttl|trig]
             [--into <graph-iri>] [--threads <n>] [--max-memory-mb <n>] <file|->
```

Streams an RDF file into one graph through Oxigraph's bulk loader, for data
the HTTP import cannot take (`rdf_load` and `POST /graphs/{id}/imports/rdf`
hold the whole body in memory and write it in one transaction, at most
500 MiB; they are unchanged). The file is read as a stream, gzip included
(`.gz`); the format comes from the name or `--format`; `-` reads standard
input and needs `--format`.

- **Where quads go.** Triples (N-Triples, Turtle) go to the graph's
  `user:rdf` named graph, or `--into <iri>`. Quads (N-Quads, TriG) go to their
  own named graphs; default-graph quads go to `user:rdf` (or `--into`). A quad
  naming one of the graph's reserved projection graphs (or the profile's)
  refuses the whole load, as the dataset import does.
- **Atomic.** The bulk loader sorts the quads into RocksDB files and ingests
  them once, on commit: a parse error or a refused graph halfway through
  loads nothing.
- **Not while served.** It refuses a profile a gardend is serving (and the
  store's own lock refuses a second process anyway): the store takes one
  writer, and nobody reads a half-loaded graph. Stop gardend, load, start it.
  Loading into a fresh store and swapping it in was the alternative; it would
  have to copy the graph's document projections into the new store first, and
  stopping costs less.
- **The graph must exist** (`create_graph`, or the app); the load touches its
  `graph.json` so the graph shows as updated. Nothing else needs rebuilding:
  the semantic index embeds documents, not RDF, and SPARQL reads the store.
- Progress goes to standard error every 5 s; at the end, the quads per named
  graph. Loading the same file twice adds nothing (quads are a set).
- `--threads` (default up to 4) and `--max-memory-mb` bound the loader.

Never in a hosted cell: it is a CLI verb on a profile folder.

## Cell runtime knobs (env)

| Var | Meaning |
|-----|---------|
| `GARDEN_PROFILE_DIR` | Profile directory (required in containers). |
| `GARDEN_CELL_GRAPH_ID` | Enables the hosted [single-graph ownership boundary](headless-single-graph-boundary.md). Platform cells set this to their exact graph slug; local `sophia-mcp` headless runtimes leave it unset and remain multi-graph. |
| `GARDEN_DURABLE_DIR` | Durable NFS/EFS snapshot dir; enables hydrate/flush (EFS cell pattern). |
| `GARDEN_FLUSH_INTERVAL_SECONDS` | Dirty-driven durable-flush max-RPO ceiling — forces a flush at least this often while dirty, regardless of debounce (default 30). |
| `GARDEN_FLUSH_DEBOUNCE_SECONDS` | Dirty-driven durable-flush debounce window — flush this long after the last observed write, coalescing bursts (default 5). |
| `GARDEN_LOOPBACK_HOST` | Bind host (default `127.0.0.1`; use `0.0.0.0` in pods). |
| `GARDEN_LOOPBACK_PORT` | Bind port (default `0` = OS-assigned; `gardend serve` defaults to 7674). |
| `GARDEN_LOOPBACK_TOKEN` | Fixed bearer token (default: random per run; `gardend serve` uses the profile's key). |
| `GARDEN_TOKIO_WORKERS` | Worker-thread floor (default ≥6 so `/health` stays schedulable under import load). |
| `SOPHIA_OBSERVATORY_CAPTURE_ENABLED` | Headless cell lifecycle CaptureEvent testimony (default `false`; see [observatory-testimony.md](observatory-testimony.md)). |
| `SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256` | Exact ratified CaptureEvent v0.1 bundle identity; required only when testimony is enabled. |
| `RUST_LOG` | Log filter (default `info`). |
| `GARDEN_NO_UPDATE_CHECK` | `1` disables the once-a-day release check (also off when `CI` or `GARDEN_CELL_GRAPH_ID` is set; the container image sets it). |
| `GARDEN_UPDATE_URL` | Release base for `--check-update` / `update` (default `https://github.com/sophia-labs/gardend`). |
| `GARDEN_CACHE_DIR` | Where the update-check stamp lives (default the user cache dir). |

## Self-host knobs (env)

For running your own Sophia behind a Cloudflare Tunnel, with Cloudflare Access as
the login. Every knob is off by default; with none set (hosted cells set none),
gardend behaves exactly as without them. Code: `src-tauri/src/loopback_self_host.rs`
and `loopback_self_host_routes.rs`.

| Var | Meaning |
|-----|---------|
| `GARDEN_UI_DIR` | A static UI build (Shrubbery's self-host flavor) to serve at `/`. Must hold an `index.html`, or gardend refuses to start. Unset: a `ui/` folder next to the gardend binary is used if it holds an `index.html`. |
| `GARDEN_CF_ACCESS_TEAM_DOMAIN` | Your Access team domain, e.g. `myteam.cloudflareaccess.com`. Set together with the AUD. |
| `GARDEN_CF_ACCESS_AUD` | The Access application's AUD tag. |
| `GARDEN_CF_ACCESS_ADMIN_EMAILS` | Optional comma list. Set: those people get the `session-all` grant and everyone else Access admits gets the **member** grant (below). Unset: everyone Access admits gets `session-all`. |
| `GARDEN_ALLOWED_ORIGINS` | Comma list of extra origins accepted on REST, MCP and the rooms (e.g. `https://sophia.example.com`). Unset with Access configured: the origin of `GARDEN_PUBLIC_BASE_URL`. |

What they do:

- **UI.** Paths no API route answers are served from the UI folder. A browser
  navigation (`Sec-Fetch-Mode: navigate` or `Accept: text/html`) to a path with
  no file gets `index.html`; other misses get the usual empty 404, and API routes
  answer exactly as before. `assets/<name>-<hash>.*` is cached `immutable`;
  everything else, `index.html` included, is `no-cache` with an `ETag`.
- **Cloudflare Access login.** A valid `Cf-Access-Jwt-Assertion` authenticates a
  request that carries no valid bearer token: RS256, the team's keys from
  `https://<team>/cdn-cgi/access/certs` matched by `kid` (cached, refetched when an
  unknown `kid` appears), and `iss`, `aud`, `exp` and `nbf` checked. A service
  token (empty `sub`, `common_name` = its Client ID) gets the member grant, or
  `session-all` when no admin list is set. Bearer tokens work exactly as before.
  An Access login is refused from an origin that is not loopback, Tauri or
  allowed, from `Origin: null`, and on `Sec-Fetch-Site: cross-site`. Identity
  comes from the token alone; an `X-User-ID` header is never read for it.
- **The member grant** (non-admins while an admin list is set; the same
  `member` grant profile `gardend token create --grant member` mints): every
  read scope, the `authoring` profile's writes (documents, workspace,
  artifacts, images, wires, entities, web imports) and MCP calls, each tool
  still checking its own scopes. Not: deletes, creating, importing or restoring graphs, loading
  or updating RDF (`graphs.write`, `rdf.load`, `rdf.update`, ...), memory and
  index writes, time-travel writes, service, model and token administration.
- **Origins and CORS.** While any knob is set, CORS answers only the origins
  gardend accepts anyway (loopback, Tauri, the allowed list) instead of mirroring
  every origin.
- **Rooms** (`/hocuspocus/...`). Besides the master token, a scoped client token
  holding the room's write scope (`documents.write.crdt` or
  `workspace.write.crdt`) is accepted (with or without these knobs; a refused
  request still gets the same 401), and under Access the Access login is, with
  its `Origin` checked against the allowed list.
- **`GET /self-host.json`** (public, `no-store`):
  `{"kind":"gardend-self-host","version":"…","auth":"cloudflare-access"|"key","user":{"email":"…"}|null}`.
  `user` is set when a valid Access token for a person came with the request (a
  service token is not a person: `null`).

Set `GARDEN_PUBLIC_BASE_URL` to your public URL under Access: browsers send an
`Origin` on writes, and without an allowed origin gardend refuses them (it logs a
warning at startup).

## Updating

Containers update by pulling a new `ghcr.io/sophia-labs/gardend:<version>`
image. Bare-binary installs run `gardend --check-update` / `gardend update`:
the tarball is verified against the release's `SHA256SUMS`, the new binary's
`--version --json` must match the release manifest, and the replacement is an
atomic rename. When a `ui/` folder sits next to the binary (symlinks of the
binary resolved) and the release archive carries `ui/` beside its binary, the
UI is replaced too: the new `ui/` is unpacked beside the old one and renamed
into place with the binary, and a failed step puts the old ones back. With no
`ui/` folder there, only the binary is replaced. (gardend 0.1.2's updater
replaces only the binary.) `update` refuses while any gardend is serving (stop it first —
SIGTERM runs the forced final durable flush) and refuses a release whose
`storageFormat` differs from the running binary's
(`src-tauri/src/storage_format.rs`).

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

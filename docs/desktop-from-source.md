# Desktop App — Build From Public Source

The Garden desktop app is the same Rust engine as [`gardend`](headless-engine.md)
wrapped in a Tauri shell, with [Shrubbery](https://github.com/sophia-labs/shrubbery)
as its UI. Both repositories are public and source-available under the
[PolyForm Noncommercial License 1.0.0](../LICENSE), so the desktop app builds
end to end with no organization credentials.

## How Shrubbery is tapped in

`shrubbery-ui.lock.json` pins the UI by repository and exact commit:

```json
{
  "repository": "https://github.com/sophia-labs/shrubbery.git",
  "commit": "<40-char sha>",
  "package": "@shrubbery/organism",
  "dist": "apps/organism/dist"
}
```

Tauri's `beforeBuildCommand` / `beforeDevCommand` run
`scripts/shrubbery-frontend.mjs`, which resolves the UI in this order:

1. `SHRUBBERY_DIR`, if set — a local Shrubbery checkout.
2. A sibling `../shrubbery` checkout.
3. Otherwise, a shallow, anonymous fetch of the pinned commit into
   `src-tauri/target/shrubbery-ui/<commit>/`, reused by later runs.

A local checkout must be at the pinned commit. Uncommitted changes on top of
it are fine for `pnpm dev`; a build refuses them unless
`SHRUBBERY_ALLOW_DIRTY=1`.

It then runs `pnpm install --frozen-lockfile`, builds `@shrubbery/organism`,
and copies the result into `frontend/dist`, which Tauri bundles. A checkout at
any commit other than the pinned one is refused, and the packaged UI records
its source in `frontend/dist/shrubbery-build.json`.

To move to a newer UI, set `commit` to a commit on `sophia-labs/shrubbery` and
rebuild.

## Prerequisites

Everything [`headless-engine.md`](headless-engine.md) needs (Rust 1.88+, a C
toolchain, `cmake`), plus:

- **Node 22+** and **pnpm** (`corepack enable`).
- **Tauri CLI:** `cargo install tauri-cli --version '^2.0' --locked`.
- macOS: Xcode Command Line Tools. Linux: the
  [Tauri system dependencies](https://v2.tauri.app/start/prerequisites/).

## Build and install

```bash
pnpm tauri build --bundles app
```

On macOS this produces `src-tauri/target/release/bundle/macos/Garden.app`.
Copy it into `/Applications` and open it. Local builds are ad-hoc signed and
not notarized, so macOS may ask you to confirm the first launch
(right-click → Open).

Other formats: `--bundles dmg` (macOS disk image), `--bundles deb,appimage`
(Linux), `--bundles nsis` (Windows).

## Develop

```bash
pnpm dev
```

This runs the pinned UI's Vite dev server on port 1420 and the Tauri shell
against it. To iterate on the UI, point `SHRUBBERY_DIR` at a Shrubbery
checkout of the pinned commit and edit it; changes hot-reload.

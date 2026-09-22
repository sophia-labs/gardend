# Native Release Process

This document covers two things: (1) the release pipeline that produces
downloadable installers, and (2) the open items that still gate a confident
"v1" public release.

## Release Pipeline

`.github/workflows/release.yml` runs on `v*` tag push (and on
`workflow_dispatch` for manual runs against an existing tag). It builds:

- **macOS universal** `.dmg` and `.app` (arm64 + x86_64 in one bundle),
  ad-hoc signed.
- **Windows** NSIS `.exe` installer, unsigned.

Artifacts upload to a **draft** GitHub release named after the tag. The
release is published manually after smoke-testing the downloaded artifacts.

### Cutting a release

1. Bump version in three places, all to the same value:
   - `package.json` → `version`
   - `src-tauri/Cargo.toml` → `package.version`
   - `src-tauri/tauri.conf.json` → `version`
2. Update `CHANGELOG.md`: move `[Unreleased]` entries under a new
   `## [vX.Y.Z] — YYYY-MM-DD` heading.
3. Commit, tag, push:
   ```bash
   git commit -am "release: vX.Y.Z"
   git tag vX.Y.Z
   git push origin main --tags
   ```
4. The release workflow runs (≈15–25 min). Watch it in the Actions tab.
5. Download both artifacts from the draft release, install each one, and
   confirm the app launches and the golden-path flow works:
   - fresh profile creates or offers the bundled welcome graph;
   - local chat can send one message and survives a WebView remount;
   - daily note creation opens today's note and is idempotent after restart;
   - tag chips render, export to Markdown, and open tag pages;
   - Excalidraw scene artifacts open and preserve graph links;
   - snapshot history opens the full diff timeline for an edited document;
   - Mermaid code blocks show a preview and a parse-error fallback.
6. Edit release notes if needed, then click **Publish release**.

### Frontend source

Shrubbery's `@shrubbery/organism` is the authoritative native frontend. Garden
pins its exact revision in `shrubbery-ui.lock.json`; the Tauri before-build
adapter verifies that revision, builds it, and copies only the generated bundle
into `frontend/dist` for packaging. Garden owns the Rust/Tauri shell and
loopback capability, not a second browser implementation.

`pnpm run frontend:build` automatically fetches the exact locked revision into
Garden's ignored Rust target cache when no matching sibling checkout exists and
the local `gh` client is authenticated. Garden CI materializes the same private
source archive through Shrubbery's organization-scoped composite action, pinned
to the immutable revision in the lock file; it does not carry a cross-repository
credential.
Set `SHRUBBERY_DIR` to validate and package an existing checkout explicitly:

```bash
SHRUBBERY_DIR=/path/to/shrubbery pnpm run frontend:build
```

Release builds reject an explicitly selected dirty or mismatched Shrubbery
checkout. Update the lock file intentionally whenever Garden advances to a
newer authoritative UI revision. Development and preview mode still require a
local checkout because they are editable, long-running processes.

### Install warnings users will see

- macOS: Gatekeeper refuses the first launch because the app is ad-hoc
  signed (`signingIdentity: "-"` in `tauri.conf.json`). Right-click →
  **Open** clears this once. Resolves when we add Developer ID +
  notarization.
- Windows: SmartScreen warns because the installer is unsigned. Resolves
  when we add an EV / OV code-signing certificate.

The default release-notes template already calls these out.

## Open Items Before "v1"

- `package.json` is `private: true` and the Cargo crate has `publish = false`.
  Intentional for now; revisit when we want package-registry distribution.
- macOS Developer ID signing + Apple notarization.
- Windows Authenticode signing.
- Auto-updater (Tauri updater plugin + hosted update manifest).
- SBOM generation and reproducible-build notes.
- A smoke test that creates a graph, writes a document, restarts the app,
  and verifies document, workspace, RDF, search, and job state. Currently
  the only CI gate is `cargo test --lib`.
- Documented model-download behavior, cache locations, and offline
  packaging expectations.
- Local profile migration story (currently "reset/rebuild/import").

## Local Validation Gate

Before tagging, run the same checks CI runs (plus the parity inventory):

```bash
export SHRUBBERY_DIR=/path/to/shrubbery
pnpm --dir "$SHRUBBERY_DIR" install --frozen-lockfile
pnpm --dir "$SHRUBBERY_DIR" --filter @shrubbery/organism test:run
pnpm --dir "$SHRUBBERY_DIR" --filter @shrubbery/organism test:art-direction-browser
pnpm run frontend:build
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml
pnpm parity:inventory
```

Live parity checks require the app to be running:

```bash
pnpm parity:loopback
pnpm parity:compare --include-mutations --include-mcp-fidelity --markdown
```

For hosted mode, merge/deploy the Platform CORS allow-list change first, then
run the preflight checks from the Platform playbook
`docs/hosted-mode-tauri-cors-playbook.md` against these route families from a
Tauri origin:

- `/billing/*`
- `/chat/models`
- one authenticated graph or document route

The broader frontend TypeScript debt is still outside this native slice. Do
not upgrade the release gate to repo-wide `tsc --noEmit` until that debt is
cleared or explicitly scoped.

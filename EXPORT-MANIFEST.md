# Export Manifest — Public Source-Available Boundary

This is the explicit path list for Garden's **engine-scoped public export**:
the headless `gardend` engine plus the minimal supporting docs, notices, and
build tooling a zero-credential external contributor needs. It is licensed
**source-available** under the [PolyForm Noncommercial License 1.0.0](LICENSE)
— never described as OSS/open source (licensing ruling, 2026-09-21).

The desktop app (Tauri shell + Shrubbery frontend) is **not** part of this
boundary. This manifest assumes a **fresh history** publication (a new
initial commit or squashed history), not a filtered export of the existing
`main` history.

Anything not listed under INCLUDE is excluded by default. Where a decision
required judgment rather than a mechanical rule, it is called out and
flagged below.

## INCLUDE

### Root files

- `LICENSE`
- `NOTICE.md`
- `THIRD-PARTY-RUST.md`
- `SECURITY.md`
- `CONTRIBUTING.md`
- `README.md`
- `CODE_OF_CONDUCT.md`
- `Dockerfile.gardend`
- `EXPORT-MANIFEST.md` (this file)

### `src-tauri/` — wholesale, with one flagged exception

The entire `src-tauri/` tree: the crate, its `headless` feature source, unit
test modules, integration tests (`src-tauri/tests/`), and fixtures. This is
the engine. It has no compile-time dependency on `frontend/` — confirmed
during the stripped-tree build check below (`build.rs` is a no-op unless
`--features desktop`; `tauri::generate_context!()`, the only call that would
need `tauri.conf.json`'s `frontendDist`, lives inside the
`#[cfg(feature = "desktop")]` `run()` in `lib.rs` and is never compiled for
`--features headless`).

- **Flagged: `src-tauri/tests/fixtures/vault_import/*.zip`** (6 files,
  174–378 bytes each, one archive containing one URL) — a pre-publication
  privacy/secret scan flagged these pending manual review. I extracted and
  read all six archives by hand: they are synthetic Notion/Obsidian/Roam
  vault-export parser fixtures with placeholder content (`aaaaaaaa…` /
  `bbbbbbbb…` filename stand-ins, "Smith, John" as a CSV-escaping test
  value, a deliberately invalid date "February 30th, 2026" for rollover
  testing, and the one URL is `https://example.com/docs.md`, the IANA
  reserved example domain, in a fixture explicitly named for testing
  "external links are ignored"). **Reasoned call: include**, on the
  evidence that these are synthetic test data, not real user content or
  credentials. This determination should still be confirmed by a
  maintainer privacy/provenance review before final publication; pull them
  if that review finds otherwise. (They are read at runtime via
  `CARGO_MANIFEST_DIR`-relative `std::fs`, in
  `src-tauri/tests/import_vault_ops.rs`, a `tests/`-directory integration
  test — not `--lib`, so they do not affect the build/test contract in
  `docs/headless-engine.md`.)
- `src-tauri/observatory-fixtures/*.ndjson` and
  `src-tauri/src/observatory/fixtures/*` — reviewed clean: the
  phone-number-shaped strings inside are timestamp/run/witness identifier
  fields, not contact data. Included, unflagged.

### `parity/` — wholesale, with one exclusion

The entire `parity/` tree: contract JSON (`local-loopback-surface.json`,
`local-openapi.json`, `surface-contracts.json`, `surface-classification.json`,
`mcp-tool-schema-snapshot.json`, `route-envelope-snapshot.json`,
`performance-budgets.json`, `openapi-route-contracts.mjs`), the check/generate
`.mjs` scripts, and `parity/fixtures/`. `src-tauri` `include_str!`s
`parity/local-loopback-surface.json` and `parity/local-openapi.json` at
compile time (`loopback_scopes.rs`, `runtime_config.rs`,
`cell_graph_boundary.rs`) — this directory is load-bearing for the build, not
optional.

- **Exclude: `parity/generated/nabokovs-goblins-field-notes.pdf`** — a
  17,662-byte generated PDF (about 3,836 characters of extracted text)
  whose provenance and licensing as a generated literary sample have not
  been reviewed for public redistribution. Excluding removes the only file
  in `parity/generated/`, so that subdirectory is empty in the export.

### `scripts/` — four files only

Everything else in `scripts/` is Shrubbery-related, a Sophia-internal
AWS/cloud acceptance harness (CodeBuild, observatory graph provisioning,
choreograph/kg-ultra service resources), or requires the running desktop
app (`parity-fresh.sh`). Included:

- `scripts/gen-third-party-rust.py` — regenerates `THIRD-PARTY-RUST.md` (new,
  this work item).
- `scripts/check-sparql-admission.sh` — static grep-based invariant checker
  over `src-tauri/*.rs` + `parity/*`; no cloud/frontend dependency, no
  network calls. Referenced from `docs/headless-engine.md` as an optional
  additional check.
- `scripts/docling_pdf_to_markdown.py`, `scripts/pymupdf4llm_pdf_to_markdown.py`
  — runtime Python helpers the engine's PDF ingestion path spawns via
  `uv run` (`src-tauri/src/pdf_runtime_processes.rs`). Included for
  functional completeness of the shipped engine; note the Docling path
  additionally needs a `pdf-accurate` `uv`/Python project extra this repo
  does not define (already tracked as a known-deferred surface in
  `docs/oss-readiness.md`), so `pymupdf4llm` is the actually-working path
  today.

### `docs/` — engine/backend subset (18 files)

Included because their content is genuinely about the Rust engine, the
loopback/MCP/RDF/SPARQL/storage/security surface, or (for the two flagged
below) because they are the evidentiary basis for claims made in the
now-included `README.md`/`CONTRIBUTING.md`:

- `docs/headless-engine.md` (new, this work item)
- `docs/headless-gardend.md`
- `docs/headless-single-graph-boundary.md`
- `docs/loopback-security.md`
- `docs/loopback-threat-model.md`
- `docs/mcp-contract.md`
- `docs/api-mcp-parity-roadmap.md`
- `docs/api-mcp-external-integration.md`
- `docs/architecture.md`
- `docs/storage-format.md`
- `docs/local-worklog-architecture.md`
- `docs/observatory-testimony.md`
- `docs/source-sync-offline-contract.md`
- `docs/sparql-admission-and-cancellation.md`
- `docs/kg-ultra-garden-integration.md`
- `docs/local-service-hosting.md`
- **Flagged: `docs/release-process.md`** — its content is entirely about the
  internal desktop packaging/signing pipeline (Shrubbery frontend build,
  macOS/Windows installers), not the engine. Included anyway because
  `README.md` and `CONTRIBUTING.md` (both in this export) cite it as the
  documented evidence for *why* the desktop app needs
  `SHRUBBERY_READ_TOKEN` and is out of the public boundary — excluding it
  would leave those exact fixed claims pointing at a dead link. Reasoned
  call: include for boundary transparency, not because it's engine-scoped.
- **Flagged: `docs/oss-readiness.md`** — the tracker is repo-wide
  (frontend-inclusive) in scope, not engine-only. Included for the same
  reason as `release-process.md`: `README.md` cites it by name as "the
  source-available readiness tracker," and it now correctly frames the
  narrower public boundary in its own text (see the "Two boundaries" note
  added at its top).

## EXCLUDE (with reasons)

- **`frontend/`** (entire directory) — the private-boundary UI, plus
  `frontend/vendor/pi/*.tgz` (vendored third-party package archives — a
  supply-chain/licensing boundary that needs its own upstream-license and
  integrity review before any redistribution) and
  `frontend/public/graphviewss.png` (~3.1MB) + `frontend/public/wirefanout.mp4`
  (~2.1MB) (large tracked media, not reviewed for redistribution rights or
  private UI/user content).
- **`.github/workflows/release.yml`, `.github/workflows/native-tauri-prototype.yml`**
  — both require `secrets.SHRUBBERY_READ_TOKEN`, per spec.
- **`.github/` entirely** (issue templates, PR template) — not requested by
  this work item, and found stale on inspection (the PR template still says
  "Frontend (sibling `../mnemosyne-platform/frontend/`)", predating even the
  `frontend/` vendoring). Flagged as a follow-up finding, not fixed here —
  out of this item's named file set.
- **`scripts/shrubbery-frontend.mjs`, `scripts/resolve-shrubbery-lock.mjs`,
  `shrubbery-ui.lock.json`** — per spec, Shrubbery-coupled.
- **`parity/generated/nabokovs-goblins-field-notes.pdf`** — see above.
- **`CHANGELOG.md`** — whole-repo release history mixing desktop/frontend
  milestones (the Shrubbery-vendoring entry this very round corrected); not
  specific to the engine boundary. Debatable; flagged.
- **`docs/README.md`** — whole-project doc index that itself links to most
  of the excluded docs below; superseded for this export by `README.md`'s
  own "Docs map" section, which is curated to only the included set.
- **`docs/garden-v1-roadmap.md`** — internal, whole-repo (frontend-inclusive)
  v1 push roadmap.
- **`docs/excalidraw-mnemosyne-integration.md`** — frontend canvas-UI feature
  doc (Excalidraw is a frontend dependency).
- **`docs/garden-main-release-convergence-spec.md`** — internal merge/
  convergence planning document, not a contributor-facing contract.
- **`docs/workspace-ydoc-persistence-bug.md`** — frontend (Vite/Yjs module
  singleton) bug investigation notes.
- **`docs/gardend-native-builder-acceptance.md`** — internal build/acceptance
  process notes for Sophia's own ephemeral AWS builder workflow; not a
  contract an external contributor runs. (Checked for leaked infra
  identifiers — none found — this is a scope call, not a secrets call.)
- **`docs/a2-ledger-design.md`, `docs/a2-replay-mitigations.md`,
  `docs/replay-classification/**`** — all three audit and design mitigations
  for **frontend** TypeScript CRDT operation handlers
  (`frontend/src/crdt/*.ts`, `frontend/src/native/native-local-runtime.ts`).
  Their entire subject matter lives in the excluded `frontend/` tree.
  (Cross-references to `docs/replay-classification/` from the *included*
  `docs/local-worklog-architecture.md`, and to `docs/workspace-ydoc-
  persistence-bug.md` / `docs/a2-replay-mitigations.md` from the *included*
  `docs/oss-readiness.md`, are left as-is — accurate citations to real
  material, not false claims, just not everything they cite ships in this
  narrower export.)
- **Everything else under `scripts/`** not listed in INCLUDE — internal
  AWS/cloud acceptance harnesses or requires the running desktop app.

## Sanity check — stripped-tree build

Verified by copying exactly the INCLUDE set above into a scratch directory
(nothing else) and building from a cold `target/`:

```bash
cargo build --release --no-default-features --features headless --example gardend
```

**Result: success.** Exit code 0, finished in 20m32s from a fully cold
build (no shared cache, no incremental state) — well inside the expected
window for a dependency graph this size (RocksDB's C++ sources for
`oxrocksdb-sys`, `candle-core`/`candle-nn`, `tokenizers`, `rusqlite`'s
bundled sqlite3.c, the `turso*` crate family, `oxigraph`, and the
`rudof`/`shacl_*` RDF-validation stack are the heaviest pieces). Zero
compiler errors; 216 warnings, all `unused import`/`dead_code` on
desktop-only Tauri command functions that are legitimately unused under
`--features headless` (they're only wired into `invoke_handler!` inside the
`#[cfg(feature = "desktop")]` build). No path outside the INCLUDE set above
was needed by any `build.rs` or `include_str!` — the manifest as written is
sufficient.

The resulting binary (`target/release/examples/gardend`, a 77MB Mach-O/ELF
executable depending on host) was also smoke-tested directly: booted with
`GARDEN_PROFILE_DIR` set, it wrote `loopback.json` with exactly the fields
[`docs/headless-engine.md`](docs/headless-engine.md) documents (`port`,
`apiUrl`, `mcpUrl`, `openapiUrl`), and `curl`ing `/health` returned
`{"status":"ok","surfacePolicy":"current","surfaceDriftCount":0}`.

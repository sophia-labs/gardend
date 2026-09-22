# Notices

Garden is developed by Sophia Labs. It was extracted from the
`mnemosyne-platform` prototype on 2026-05-08 and now lives in its own
repository at https://github.com/sophia-labs/gardend.

Garden is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE)
— a **source-available** license, not an OSI-approved open source license.
The license permits non-commercial use, modification, and redistribution
under those same terms. Commercial use requires a separate agreement with
the maintainers.

Required Notice (per the PolyForm Noncommercial license):

> Copyright Veronica Chambers (https://github.com/sophia-labs/gardend)

## Third-Party Software

**Public boundary (headless engine): done.** `THIRD-PARTY-RUST.md`, at the
repo root, is the third-party Rust crate license inventory for the
`src-tauri` crate's **headless engine closure** — the exact dependency
closure of `cargo build --release --no-default-features --features headless
--example gardend` (the boundary described in `EXPORT-MANIFEST.md`). It is
packaged into every `gardend` container image at
`/usr/local/share/gardend/THIRD-PARTY-RUST.md` (`Dockerfile.gardend`).
Regenerate it with `scripts/gen-third-party-rust.py` whenever
`src-tauri/Cargo.lock` changes.

**Still open, internal-only:** a dependency license review for the desktop
build (the `--features desktop` Cargo closure, i.e. Tauri/wry/GUI plugins)
and for the `frontend/`/Shrubbery npm dependency tree, plus attributions for
bundled runtime assets, icons, models, OCR/PDF helpers, and installer
artifacts. None of those are part of the public source-available boundary
today (see `EXPORT-MANIFEST.md`), so this remaining review blocks a future
desktop release, not the current headless-engine publication.

#!/usr/bin/env python3
"""Regenerate THIRD-PARTY-RUST.md from `cargo metadata` for the headless
(gardend) feature closure of the src-tauri crate.

This is a fallback tool: prefer `cargo-about` or `cargo-license` if either
is installed (neither was, in the environment this was first authored in).
This script walks the resolved dependency graph (normal + build edges only, dev
edges excluded) starting at the src-tauri `garden` package, for the exact
feature set shipped by Dockerfile.gardend:

    cargo build --release --no-default-features --features headless --example gardend

filtered to the x86_64-unknown-linux-gnu target (the Dockerfile.gardend
runtime platform), so platform-conditional crates that never compile into
the shipped binary (e.g. windows-* crates) are excluded.

Usage (from repo root):
    python3 scripts/gen-third-party-rust.py

Regenerate whenever src-tauri/Cargo.lock changes.
"""
import json
import subprocess
import sys
from collections import deque
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_TAURI = REPO_ROOT / "src-tauri"
OUTPUT_PATH = REPO_ROOT / "THIRD-PARTY-RUST.md"
TARGET = "x86_64-unknown-linux-gnu"


def run_cargo_metadata() -> dict:
    cmd = [
        "cargo",
        "metadata",
        "--format-version=1",
        "--no-default-features",
        "--features",
        "headless",
        "--filter-platform",
        TARGET,
        "--manifest-path",
        str(SRC_TAURI / "Cargo.toml"),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return json.loads(proc.stdout)


def collect_closure(data: dict) -> set:
    nodes = {n["id"]: n for n in data["resolve"]["nodes"]}
    root_id = data["resolve"]["root"]

    visited = set()
    queue = deque([root_id])
    while queue:
        pid = queue.popleft()
        if pid in visited:
            continue
        visited.add(pid)
        node = nodes.get(pid)
        if not node:
            continue
        for dep in node.get("deps", []):
            kinds = dep.get("dep_kinds", [])
            # Include normal (kind None/"normal") and build edges; exclude dev-only edges.
            include = any(dk.get("kind") in (None, "normal", "build") for dk in kinds) or not kinds
            if include:
                queue.append(dep["pkg"])

    visited.discard(root_id)
    return visited


def build_rows(data: dict, ids: set) -> list:
    packages = {p["id"]: p for p in data["packages"]}
    rows = []
    for pid in ids:
        pkg = packages.get(pid)
        if not pkg:
            continue
        name = pkg["name"]
        version = pkg["version"]
        license = pkg.get("license")
        license_file = pkg.get("license_file")
        repository = pkg.get("repository") or ""
        source = pkg.get("source") or ""
        if source.startswith("registry+https://github.com/rust-lang/crates.io-index"):
            src_desc = "crates.io"
        elif source.startswith("git+"):
            src_desc = source
        elif not source:
            src_desc = "path (workspace-local)"
        else:
            src_desc = source

        if license:
            license_desc = license
        elif license_file:
            license_desc = f"see `{license_file}` in crate source (no SPDX expression declared)"
        else:
            license_desc = "UNSPECIFIED (no license field or license_file in Cargo metadata — manual review needed)"

        rows.append(
            {
                "name": name,
                "version": version,
                "license": license_desc,
                "repository": repository,
                "source": src_desc,
            }
        )

    rows.sort(key=lambda r: (r["name"].lower(), r["version"]))
    return rows


def render(rows: list) -> str:
    unspecified = [r for r in rows if r["license"].startswith("UNSPECIFIED")]

    lines = []
    lines.append("# Third-Party Rust Dependencies — Headless Engine Closure")
    lines.append("")
    lines.append(
        "This inventory covers the **`src-tauri` crate built for the headless "
        "engine** — the exact dependency closure of "
        "`cargo build --release --no-default-features --features headless "
        "--example gardend`, filtered to the `x86_64-unknown-linux-gnu` target "
        "used by `Dockerfile.gardend`. It does **not** cover the desktop "
        "(`--features desktop`) build, the `frontend/` npm dependency tree, or "
        "any Shrubbery-owned code — those are outside the public source-available "
        "boundary (see `EXPORT-MANIFEST.md`)."
    )
    lines.append("")
    lines.append(
        f"Generated {len(rows)} third-party crate entries from `cargo metadata` "
        "via `scripts/gen-third-party-rust.py` (neither `cargo-about` nor "
        "`cargo-license` was installed in the generating environment, so this "
        "table was built with a small script over `cargo metadata "
        "--format-version=1 --no-default-features --features headless "
        "--filter-platform x86_64-unknown-linux-gnu`, walking the resolved "
        "dependency graph from the `garden` package over normal + build edges "
        "and excluding dev-dependencies). Prefer running a real `cargo-about` "
        "or `cargo-license` pass instead when either is available, and "
        "regenerate whenever `src-tauri/Cargo.lock` changes."
    )
    lines.append("")
    if unspecified:
        lines.append(
            f"> **{len(unspecified)} crate(s) below have no SPDX `license` field "
            "and no `license_file` in their published Cargo metadata** and are "
            "flagged `UNSPECIFIED`. These need manual review before public "
            "publication — check the crate's repository directly."
        )
        lines.append("")
    lines.append("| Crate | Version | License | Source |")
    lines.append("|---|---|---|---|")
    for r in rows:
        src_cell = r["repository"] or r["source"]
        lines.append(f"| `{r['name']}` | {r['version']} | {r['license']} | {src_cell} |")
    lines.append("")
    return "\n".join(lines) + "\n"


def main() -> int:
    data = run_cargo_metadata()
    ids = collect_closure(data)
    rows = build_rows(data, ids)
    unspecified = [r for r in rows if r["license"].startswith("UNSPECIFIED")]

    OUTPUT_PATH.write_text(render(rows))

    print(f"wrote {OUTPUT_PATH} ({len(rows)} crates, {len(unspecified)} unspecified)", file=sys.stderr)
    for r in unspecified:
        print(f"  UNSPECIFIED: {r['name']} {r['version']}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())

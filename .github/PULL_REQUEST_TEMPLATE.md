<!--
Thanks for opening a PR against Garden. Keep CRDT state authoritative, and route new behavior
through service modules before exposing it via Tauri / loopback REST / MCP.
Run the checks below locally first: CI runs the same ones.
-->

## Summary

<!-- What does this change do, and why? One or two short paragraphs. -->

## Surface impact

- [ ] Loopback HTTP routes (`src-tauri/src/loopback_*`)
- [ ] MCP tools / scopes (`mcp_tool_catalog.json`, `mcp_dispatch_registry.rs`)
- [ ] Storage layout (`docs/storage-format.md`)
- [ ] Security boundary (`docs/loopback-security.md`)
- [ ] Parity inventory / contracts (`parity/`)
- [ ] Tauri command surface or the desktop shell
- [ ] Documentation only

## What I ran

<!-- Paste the outcome, or tick the box. -->

- [ ] `cargo fmt --check --manifest-path src-tauri/Cargo.toml`
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml --no-default-features --features headless --lib` (or `scripts/garden-build-service.py test`)
- [ ] `pnpm parity:openapi:check && pnpm parity:cell-boundary && pnpm parity:security` (if routes, MCP tools, scopes, storage or security changed)
- [ ] Added or updated a real test for the change (no mocks)

## Notes for reviewers

<!-- Anything specific to look at: tricky diffs, follow-up work, known gaps. -->

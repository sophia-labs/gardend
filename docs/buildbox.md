# buildbox: a little build box on your own machine

Cold-compiling gardend takes tens of minutes. Changing one file should take seconds. `scripts/buildbox` keeps one
warm Rust build for the version of Garden you are working on and does the rest of the work around it.

```text
scripts/buildbox build [--release]   build the headless gardend; stdout is the binary's path, nothing else
scripts/buildbox test [filter...]    headless lib tests; with no filter, the whole battery shaped like CI
scripts/buildbox gate                cargo fmt --check, cargo check, the battery, the build
scripts/buildbox status              key, warm directory and size, lock, last log, other keys
scripts/buildbox serve [seconds]     prebuild origin/main every N seconds (default 1800)
scripts/buildbox unit launchd|systemd   print a unit file that keeps `serve` running at login
```

## How it works

- **The key** names the Garden line: a hash of `src-tauri/Cargo.lock` at `git merge-base HEAD origin/main`, plus a hash of
  `rustc -vV`. Same dependencies and the same compiler give the same warm directory, whatever branch or worktree you build
  from. A dependency bump or a new toolchain starts a fresh one; `status` lists the others, and deleting them is safe.
  `BUILDBOX_KEY=name` overrides it.
- **One warm `CARGO_TARGET_DIR`** at `~/.cache/gardend-buildbox/<key>/target` (override the root with `BUILDBOX_HOME`).
  If `sccache` is installed it is used too, in `<key>/sccache`.
- **A stable source directory** `<key>/src`. Your working tree (tracked files and untracked files that are not ignored) is
  copied there with `rsync --checksum`: a file whose content did not change keeps its old modification time, so cargo
  does not see it as touched, and the path never changes, so cargo's fingerprints stay valid. Files you deleted are removed.
  This is why a second `build` after editing one file recompiles one crate, not the tree.
- **A lock** serializes runs (a second request waits for the first). A lock left by a dead process is cleared.
- **Logs** of every run are in `<key>/logs/`; a failure prints the last forty lines and the log's path.

## The gate

`buildbox gate` runs what CI runs: `cargo fmt --check`; `cargo check` of the library and examples; the library tests with
the three order-dependent suites skipped and then run isolated (see [headless-engine.md](headless-engine.md)); the
`gardend` build. On a Mac, 14 process-spawning tests fail with `Bad file descriptor` inside the full parallel run and
pass alone, so `buildbox` skips that group in the main run and runs it on its own afterwards (Linux runs it in the main run,
as CI does). A test that opens a macOS Keychain prompt (the provider-key tests) can wait on a dialog nobody sees: name it in
`BUILDBOX_TEST_SKIP` to leave it out of the main run.

## Keeping it warm

`buildbox serve` fetches `origin/main` and builds it into the same warm directory every half hour, so your next build
starts from a tree that is at most a few commits behind. `buildbox unit launchd` (macOS) and `buildbox unit systemd`
(`systemd --user`) print a unit file and say where to save it and how to load it. The warmer runs at low priority.

## What it does not do

It does not publish anything, listen on a port, or talk to any service except `git fetch origin` in `serve`. It needs
`bash`, `rsync`, `git`, `shasum` and your Rust toolchain. For a ready-made gardend with no Rust at all, use
`scripts/fetch-gardend`.

# Sophia (the app)

A native window onto a Sophia place. Choose the place from the **Place** menu; the choice is remembered.

- **Hosted**: garden.sophia-labs.com, with its own sign-in. Set `SOPHIA_URL` to open another hosted address.
- **This Mac (local, offline)**: a `gardend` the app starts beside itself, serving the Shrubbery UI bundled in the
  app. Data lives in `~/Library/Application Support/com.sophia-labs.sophia/local` (Place → Show Local Data in Finder).
  Nothing leaves the machine.

Links to other sites open in your browser; downloads go to ~/Downloads. **Go** has Back, Forward, Home, Reload and
"Open This Page in Browser". Right-click → Inspect Element opens the Web Inspector.

## Build

```sh
# 1. The local UI: a plain (non-hosted) build of the Shrubbery organism, copied to shell/dist
pnpm --dir <shrubbery> --filter @shrubbery/organism build && cp -R <shrubbery>/apps/organism/dist shell/dist
# 2. The engine for this machine, as a sidecar: shell/src-tauri/binaries/gardend-<target-triple>
#    (a release tarball from github.com/sophia-labs/gardend, or
#     cargo build --release --no-default-features --features headless --example gardend in src-tauri/)
# 3. The app
cd shell/src-tauri && cargo tauri build --bundles app   # → target/release/bundle/macos/Sophia.app
```

The UI and the engine must come from the same moment: a Shrubbery newer than its gardend can fail to sync.

**Mac users:** the app is not notarized. The first time, right-click Sophia.app → Open → Open (or run
`xattr -dr com.apple.quarantine /Applications/Sophia.app`).

Overrides: `SOPHIA_URL` (hosted address), `SOPHIA_GARDEND` (engine binary), `SOPHIA_LOCAL_PROFILE` (local data folder).

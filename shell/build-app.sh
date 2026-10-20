#!/usr/bin/env bash
# build-app.sh: build Sophia.app, self-hosted Sophia in a native window, with everything it runs inside it.
#
#   shell/build-app.sh --gardend <bin> --shrubbery <dir> --vehicle <dir> --choreograph <dir> [--install]
#   shell/build-app.sh --only ui|services --shrubbery <dir> | --vehicle <dir> --choreograph <dir>
#
# What goes in (Contents/Resources unless said):
#   MacOS/gardend      the engine (`--gardend`; `scripts/buildbox app` builds it warm and passes it)
#   ui/                Shrubbery's self-host build (`pnpm build:selfhost` in --shrubbery)
#   services/vehicle/  chat (vehicle's ts/scripts/build-service.mjs in --vehicle)
#   services/choreograph/  Sophia Compute (deploy/build-gardend-service.mjs in --choreograph, this Mac's libSQL only)
#   node/bin/node      the Node.js both run on, 22.19 or later and self-contained (an official build, not Homebrew's):
#                      `--node`, else the `node` on PATH, else the newest from mise or nvm
#   lib/libpdfium.dylib  pdfium for reading scans: `--pdfium`, else GARDEN_PDFIUM_LIB, else pypdfium2's, if found
#
# --install puts the app in ~/Applications (quitting a running Sophia first, so its gardend flushes and stops), keeping
# the one before as ~/.cache/gardend-buildbox/Sophia.prev.app. --only ui|services rebuilds that part into the INSTALLED
# app and re-signs it, without bundling: reload the window (Cmd-R) for a UI, quit and reopen Sophia for services.
#
# The UI and the engine must come from the same moment: a Shrubbery newer than its gardend can fail to sync.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TAURI=$HERE/src-tauri
STAGE=$TAURI/stage
INSTALLED=${SOPHIA_APP_DIR:-$HOME/Applications}/Sophia.app
CACHE=${BUILDBOX_HOME:-$HOME/.cache/gardend-buildbox}
NODE_FLOOR=22.19

say() { printf 'build-app: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

GARDEND= SHRUBBERY= VEHICLE= CHOREOGRAPH= NODE= PDFIUM= INSTALL=0 ONLY=
while (($#)); do
  case $1 in
    --gardend) GARDEND=$2; shift 2 ;;
    --shrubbery) SHRUBBERY=$2; shift 2 ;;
    --vehicle) VEHICLE=$2; shift 2 ;;
    --choreograph) CHOREOGRAPH=$2; shift 2 ;;
    --node) NODE=$2; shift 2 ;;
    --pdfium) PDFIUM=$2; shift 2 ;;
    --install) INSTALL=1; shift ;;
    --only) ONLY=$2; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

stage_ui() {
  [[ -d $SHRUBBERY/apps/organism ]] || die "--shrubbery must be a Shrubbery checkout (no apps/organism in ${SHRUBBERY:-?})"
  say "ui: pnpm build:selfhost in $SHRUBBERY ($(git -C "$SHRUBBERY" rev-parse --short HEAD))"
  (cd "$SHRUBBERY" && pnpm --silent build:selfhost) </dev/null >"$CACHE/app-ui.log" 2>&1 || { tail -n 30 "$CACHE/app-ui.log" >&2; die "ui build failed ($CACHE/app-ui.log)"; }
  rm -rf "$1/ui" && mkdir -p "$1" && cp -R "$SHRUBBERY/apps/organism/dist-selfhost" "$1/ui"
}

stage_services() {
  [[ -f $VEHICLE/ts/scripts/build-service.mjs ]] || die "--vehicle must be a vehicle checkout"
  [[ -f $CHOREOGRAPH/deploy/build-gardend-service.mjs ]] || die "--choreograph must be a Choreograph checkout"
  rm -rf "$1/services" && mkdir -p "$1/services"
  say "services: vehicle $(git -C "$VEHICLE" rev-parse --short HEAD), choreograph $(git -C "$CHOREOGRAPH" rev-parse --short HEAD)"
  (cd "$VEHICLE/ts" && node scripts/build-service.mjs "$1/services/vehicle") </dev/null >"$CACHE/app-vehicle.log" 2>&1 || { tail -n 30 "$CACHE/app-vehicle.log" >&2; die "vehicle build failed"; }
  (cd "$CHOREOGRAPH" && node deploy/build-gardend-service.mjs "$1/services/choreograph" --natives host) </dev/null >"$CACHE/app-choreograph.log" 2>&1 || { tail -n 30 "$CACHE/app-choreograph.log" >&2; die "choreograph build failed"; }
}

# A Node.js that runs on its own: official builds link only the system's libraries; Homebrew's needs its libnode and more.
self_contained() { ! otool -L "$1" 2>/dev/null | tail -n +2 | grep -qvE '^[[:space:]]*(/usr/lib/|/System/)'; }

stage_node() {
  local candidates=() node= c v
  if [[ -n $NODE ]]; then candidates=("$NODE")
  else
    command -v node >/dev/null && candidates+=("$(command -v node)")
    # mise and nvm keep official builds
    for c in $(ls -d "$HOME"/.local/share/mise/installs/node/*/bin/node "$HOME"/.nvm/versions/node/*/bin/node 2>/dev/null | sort -rV); do candidates+=("$c"); done
  fi
  for c in ${candidates[@]+"${candidates[@]}"}; do
    c=$(realpath "$c")
    v=$("$c" -p 'process.versions.node' 2>/dev/null) || continue
    [[ $(printf '%s\n%s\n' "$NODE_FLOOR" "$v" | sort -V | head -1) == "$NODE_FLOOR" ]] || continue
    self_contained "$c" || { say "node: skipping $c (it needs libraries outside the system's)"; continue; }
    node=$c; break
  done
  [[ -n $node ]] || die "no self-contained Node.js $NODE_FLOOR or later (an official build from nodejs.org, mise or nvm); pass --node <bin>"
  rm -rf "$1/node" && mkdir -p "$1/node/bin" && cp "$node" "$1/node/bin/node" && chmod 755 "$1/node/bin/node"
  say "node: $v ($node)"
}

stage_pdfium() {
  local lib=${PDFIUM:-${GARDEN_PDFIUM_LIB:-}}
  if [[ -z $lib ]]; then
    lib=$(ls "$HOME"/.local/share/uv/tools/*/lib/python3*/site-packages/pypdfium2_raw/libpdfium.dylib 2>/dev/null | head -1 || true)
  fi
  rm -rf "$1/lib" && mkdir -p "$1/lib"
  if [[ -n $lib && -f $lib ]]; then cp "$lib" "$1/lib/libpdfium.dylib"; say "pdfium: $lib"
  else say "pdfium: none found (scans will not be read); pass --pdfium <libpdfium.dylib>"; fi
}

resign() { codesign --force --deep --sign - "$1" >/dev/null 2>&1 || say "could not re-sign $1"; }

mkdir -p "$CACHE"

if [[ -n $ONLY ]]; then
  [[ -d $INSTALLED ]] || die "no Sophia.app at $INSTALLED to patch; build it whole first"
  case $ONLY in
    ui) stage_ui "$INSTALLED/Contents/Resources" ;;
    services) stage_services "$INSTALLED/Contents/Resources" ;;
    *) die "--only takes ui or services" ;;
  esac
  resign "$INSTALLED"
  say "patched $ONLY in $INSTALLED"
  exit 0
fi

[[ -x $GARDEND ]] || die "--gardend must be the gardend binary (scripts/buildbox app builds it)"
TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
mkdir -p "$TAURI/binaries"
install -m 755 "$GARDEND" "$TAURI/binaries/gardend-$TRIPLE"
rm -rf "$STAGE" && mkdir -p "$STAGE"
stage_ui "$STAGE"
stage_services "$STAGE"
stage_node "$STAGE"
stage_pdfium "$STAGE"
# The bundler clears extended attributes on every file, so every file must be writable (a Node from mise is not).
chmod -R u+w "$STAGE"

say "bundling Sophia.app"
export CARGO_TARGET_DIR=$CACHE/shell-target
# The build copies the resources beside the binary too; an earlier read-only copy there cannot be overwritten.
chmod -R u+w "$CARGO_TARGET_DIR/release/node" "$CARGO_TARGET_DIR/release/lib" 2>/dev/null || true
(cd "$TAURI" && cargo tauri build --bundles app) </dev/null >"$CACHE/app-bundle.log" 2>&1 || { tail -n 40 "$CACHE/app-bundle.log" >&2; die "bundle failed ($CACHE/app-bundle.log)"; }
APP=$CARGO_TARGET_DIR/release/bundle/macos/Sophia.app
[[ -d $APP ]] || die "no app at $APP"
cat >"$APP/Contents/Resources/BUILD.txt" <<EOF
built $(date -u +%Y-%m-%dT%H:%M:%SZ)
gardend $("$GARDEND" --version 2>/dev/null | head -1) ($(git -C "$HERE" rev-parse --short HEAD))
shrubbery $(git -C "$SHRUBBERY" rev-parse --short HEAD)
vehicle $(git -C "$VEHICLE" rev-parse --short HEAD)
choreograph $(git -C "$CHOREOGRAPH" rev-parse --short HEAD)
node $("$STAGE/node/bin/node" -v)
EOF
resign "$APP"

if ((INSTALL)); then
  if pgrep -xq sophia; then
    say "quitting the running Sophia (its gardend flushes first)"
    osascript -e 'tell application id "com.sophia-labs.sophia" to quit' >/dev/null 2>&1 || true
    for _ in $(seq 1 120); do pgrep -xq sophia || break; sleep 1; done
    pgrep -xq sophia && die "Sophia is still running; quit it and run again"
  fi
  mkdir -p "$(dirname "$INSTALLED")"
  if [[ -d $INSTALLED ]]; then rm -rf "$CACHE/Sophia.prev.app"; mv "$INSTALLED" "$CACHE/Sophia.prev.app"; fi
  ditto "$APP" "$INSTALLED"
  say "installed $INSTALLED (the one before: $CACHE/Sophia.prev.app)"
  echo "$INSTALLED"
else
  echo "$APP"
fi

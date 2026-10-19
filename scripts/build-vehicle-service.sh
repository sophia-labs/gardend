#!/usr/bin/env bash
# Build the `vehicle` service (the self-host chat) that gardend release archives carry as services/vehicle/,
# from the vehicle commit vehicle-service.lock.json names, and check it against the lock.
#
#   scripts/build-vehicle-service.sh <out>            # <out> must not exist
#   scripts/build-vehicle-service.sh --lock <out>     # build from VEHICLE_COMMIT and write the lock from it
#
# VEHICLE_REPO: where to clone sophia-labs/vehicle from (default git@github.com:sophia-labs/vehicle.git; a local
# checkout path works and is not changed: the build happens in a temporary clone). Needs node >= 22.19 and pnpm.
# Then: replace services/vehicle/ in this repository with <out> and commit it together with the regenerated lock.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
lock_file="$here/vehicle-service.lock.json"
write_lock=false
if [[ ${1:-} == --lock ]]; then write_lock=true; shift; fi
out=${1:?usage: build-vehicle-service.sh [--lock] <out>}
[[ -e $out ]] && { echo "build-vehicle-service: $out exists; move it aside first" >&2; exit 2; }
out=$(mkdir -p "$(dirname "$out")" && cd "$(dirname "$out")" && pwd)/$(basename "$out")
repo=${VEHICLE_REPO:-git@github.com:sophia-labs/vehicle.git}
if $write_lock; then
	commit=${VEHICLE_COMMIT:?--lock needs VEHICLE_COMMIT (a vehicle commit on its main)}
else
	commit=$(node -e 'console.log(JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8")).commit)' "$lock_file")
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/vehicle-service-XXXXXX")
trap 'rm -rf "$work"' EXIT
git clone --quiet --no-checkout "$repo" "$work/vehicle"
git -C "$work/vehicle" -c advice.detachedHead=false checkout --quiet "$commit"
test "$(git -C "$work/vehicle" rev-parse HEAD)" = "$commit"
(cd "$work/vehicle/ts" && pnpm install --frozen-lockfile --silent && node scripts/build-service.mjs "$out")
if $write_lock; then
	node -e '
		const fs = require("node:fs");
		const [service, lockFile] = process.argv.slice(1);
		const build = JSON.parse(fs.readFileSync(`${service}/BUILD.json`, "utf8"));
		if (build.dirty !== false) throw new Error("the vehicle tree was dirty");
		const files = Object.fromEntries(Object.entries(build.files).filter(([path]) => path !== "BUILD.json"));
		const lock = {
			repository: "https://github.com/sophia-labs/vehicle",
			commit: build.vehicleCommit,
			build: "ts/scripts/build-service.mjs",
			pi: build.pi,
			node: build.node,
			esbuild: build.esbuild,
			files,
		};
		fs.writeFileSync(lockFile, JSON.stringify(lock, null, 2) + "\n");
	' "$out" "$lock_file"
	echo "build-vehicle-service: wrote $lock_file for vehicle $commit"
fi
node "$here/scripts/check-vehicle-service.mjs" "$out" "$lock_file"

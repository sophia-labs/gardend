#!/usr/bin/env bash
# Build Sophia Compute (the `choreograph` service) that gardend release archives carry as services/choreograph/,
# from the Choreograph commit choreograph-service.lock.json names, and check it against the lock.
#
#   scripts/build-choreograph-service.sh <out>            # <out> must not exist
#   scripts/build-choreograph-service.sh --lock <out>     # build from CHOREOGRAPH_COMMIT and write the lock from it
#
# CHOREOGRAPH_REPO: where to clone sophia-labs/choreograph from (default git@github.com:sophia-labs/choreograph.git;
# a local checkout path works and is not changed: the build happens in a temporary clone). Its public source is
# sophia-labs/choreograph (MIT). Needs node >= 22.19, pnpm and npm (libSQL's bindings for the four release targets come
# from npm at the version libsql pins). <out> holds all four; the repository's services/choreograph/ leaves them out and a release archive gets
# its own target's (scripts/check-choreograph-service.mjs --add-native).
# Then: replace services/choreograph/ in this repository with <out> (minus the natives) and commit it together with the regenerated lock.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
lock_file="$here/choreograph-service.lock.json"
write_lock=false
if [[ ${1:-} == --lock ]]; then write_lock=true; shift; fi
out=${1:?usage: build-choreograph-service.sh [--lock] <out>}
[[ -e $out ]] && { echo "build-choreograph-service: $out exists; move it aside first" >&2; exit 2; }
out=$(mkdir -p "$(dirname "$out")" && cd "$(dirname "$out")" && pwd)/$(basename "$out")
repo=${CHOREOGRAPH_REPO:-git@github.com:sophia-labs/choreograph.git}
if $write_lock; then
	commit=${CHOREOGRAPH_COMMIT:?--lock needs CHOREOGRAPH_COMMIT (a choreograph commit on origin)}
else
	commit=$(node -e 'console.log(JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8")).commit)' "$lock_file")
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/choreograph-service-XXXXXX")
trap 'rm -rf "$work"' EXIT
git clone --quiet --no-checkout "$repo" "$work/choreograph"
git -C "$work/choreograph" -c advice.detachedHead=false checkout --quiet "$commit"
test "$(git -C "$work/choreograph" rev-parse HEAD)" = "$commit"
(
	cd "$work/choreograph"
	pnpm install --frozen-lockfile --silent
	# The orchestrator bundles @sophia-labs/canonical-json from its built dist/ (a workspace package).
	pnpm --filter @sophia-labs/canonical-json build >/dev/null
	node deploy/build-gardend-service.mjs "$out" --natives all
)
if $write_lock; then
	node -e '
		const fs = require("node:fs");
		const crypto = require("node:crypto");
		const [service, lockFile] = process.argv.slice(1);
		const build = JSON.parse(fs.readFileSync(`${service}/BUILD.json`, "utf8"));
		if (build.dirty !== false) throw new Error("the choreograph tree was dirty");
		const triples = {
			"darwin-x64": "x86_64-apple-darwin",
			"darwin-arm64": "aarch64-apple-darwin",
			"linux-x64-gnu": "x86_64-unknown-linux-gnu",
			"linux-arm64-gnu": "aarch64-unknown-linux-gnu",
		};
		const nativesDir = "app/node_modules/@libsql/";
		const files = Object.fromEntries(
			Object.entries(build.files).filter(([path]) => path !== "BUILD.json" && !path.startsWith(nativesDir)),
		);
		const natives = {};
		for (const name of build.natives) {
			const triple = triples[name];
			if (!triple) throw new Error(`no release target for libSQL binding ${name}`);
			const manifest = JSON.parse(fs.readFileSync(`${service}/${nativesDir}${name}/package.json`, "utf8"));
			natives[triple] = {
				package: manifest.name,
				version: manifest.version,
				files: {
					"index.node": build.files[`${nativesDir}${name}/index.node`],
					"package.json": build.files[`${nativesDir}${name}/package.json`],
				},
			};
		}
		const lock = {
			repository:
				"https://github.com/sophia-labs/choreograph (the built folder is what gardend archives carry)",
			commit: build.choreographCommit,
			build: "deploy/build-gardend-service.mjs",
			pi: build.pi,
			libsql: build.libsql,
			node: build.node,
			esbuild: build.esbuild,
			files,
			natives,
		};
		fs.writeFileSync(lockFile, JSON.stringify(lock, null, 2) + "\n");
	' "$out" "$lock_file"
	echo "build-choreograph-service: wrote $lock_file for choreograph $commit"
fi
node "$here/scripts/check-choreograph-service.mjs" "$out" "$lock_file"

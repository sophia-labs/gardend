#!/usr/bin/env node

/**
 * Check a built `services/vehicle/` folder against `vehicle-service.lock.json`.
 *
 * The self-host chat (the agent core's `vehicle` service) rides in every gardend
 * release archive as `services/vehicle/` beside the binary. Its source is the
 * sophia-labs/vehicle repository; what is published is the folder its
 * `ts/scripts/build-service.mjs` builds (one bundled `app/main.mjs`, pi's
 * licence and package.json, photon-node, `LICENSE`, `THIRD-PARTY-NOTICES.md`).
 * The lock names the vehicle commit and the sha256 of every file, so the folder
 * the release carries is exactly the one built from that commit.
 *
 *   node scripts/check-vehicle-service.mjs <service-dir> [<lock>]
 *
 * <lock> defaults to vehicle-service.lock.json at the root of this repository. Exit 0 and one line
 * when every file matches; otherwise every difference and exit 1.
 */

import { createHash } from 'node:crypto'
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')

export function parseVehicleServiceLock(source, sourceName = 'vehicle-service.lock.json') {
  const lock = JSON.parse(source)
  if (!lock || typeof lock !== 'object' || Array.isArray(lock)) throw new Error(`${sourceName} must contain a JSON object`)
  if (typeof lock.commit !== 'string' || !/^[0-9a-f]{40}$/.test(lock.commit)) {
    throw new Error(`${sourceName} commit must be a lowercase 40-character SHA`)
  }
  if (!lock.files || typeof lock.files !== 'object' || Object.keys(lock.files).length === 0) {
    throw new Error(`${sourceName} files must map each file of the service to its sha256`)
  }
  for (const [path, sum] of Object.entries(lock.files)) {
    if (path.startsWith('/') || path.split('/').includes('..') || !/^[0-9a-f]{64}$/.test(String(sum))) {
      throw new Error(`${sourceName}: bad entry ${JSON.stringify(path)}`)
    }
  }
  return lock
}

function filesUnder(dir) {
  const out = []
  for (const name of readdirSync(dir).sort()) {
    const path = join(dir, name)
    if (statSync(path).isDirectory()) out.push(...filesUnder(path))
    else out.push(path)
  }
  return out
}

export function checkVehicleService(serviceDir, lock) {
  const problems = []
  const build = JSON.parse(readFileSync(join(serviceDir, 'BUILD.json'), 'utf8'))
  if (build.vehicleCommit !== lock.commit) problems.push(`BUILD.json names vehicle ${build.vehicleCommit}, the lock ${lock.commit}`)
  if (build.dirty !== false) problems.push('BUILD.json says the vehicle tree had uncommitted changes')
  const found = new Map(
    filesUnder(serviceDir)
      .map(path => relative(serviceDir, path).split('\\').join('/'))
      .filter(path => path !== 'BUILD.json')
      .map(path => [path, createHash('sha256').update(readFileSync(join(serviceDir, path))).digest('hex')]),
  )
  for (const [path, sum] of Object.entries(lock.files)) {
    if (!found.has(path)) problems.push(`missing ${path}`)
    else if (found.get(path) !== sum) problems.push(`${path} differs from the lock`)
  }
  for (const path of found.keys()) if (!(path in lock.files)) problems.push(`${path} is not in the lock`)
  for (const required of ['service.json', 'app/main.mjs', 'LICENSE', 'THIRD-PARTY-NOTICES.md']) {
    if (!(required in lock.files)) problems.push(`the lock has no ${required}`)
  }
  return problems
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [serviceDir, lockArg] = process.argv.slice(2)
  if (!serviceDir) {
    console.error('usage: check-vehicle-service.mjs <service-dir> [<lock>]')
    process.exit(2)
  }
  const lockPath = lockArg
    ?? [join(root, 'vehicle-service.lock.json')].find(existsSync)
  if (!lockPath) {
    console.error('check-vehicle-service: no vehicle-service.lock.json found')
    process.exit(2)
  }
  const lock = parseVehicleServiceLock(readFileSync(lockPath, 'utf8'), lockPath)
  const problems = checkVehicleService(serviceDir, lock)
  if (problems.length) {
    for (const problem of problems) console.error(`check-vehicle-service: ${problem}`)
    process.exit(1)
  }
  console.log(`check-vehicle-service: ${serviceDir} matches vehicle ${lock.commit.slice(0, 12)} (${Object.keys(lock.files).length} files)`)
}

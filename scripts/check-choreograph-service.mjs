#!/usr/bin/env node

/**
 * Check a built `services/choreograph/` folder (Sophia Compute) against
 * `choreograph-service.lock.json`, and put one target's libSQL binding in it.
 *
 * Sophia Compute rides in every gardend release archive as
 * `services/choreograph/` beside the binary, off until the person runs
 * `gardend service enable choreograph`. It is built from the Choreograph commit
 * the lock names (`deploy/build-gardend-service.mjs`): one bundled orchestrator
 * (`app/main.mjs`), its runtime worker, the agent body (`body/body.mjs`, upstream
 * pi), `LICENSE`, `NOTICE`, `THIRD-PARTY-NOTICES.md`. Its one piece of native
 * code is libSQL's prebuilt binding, `app/node_modules/@libsql/<target>/`, one
 * per platform. The public tree carries the folder without them; the lock names
 * each target's binding (npm package, version, sha256 of its two files), and a
 * release archive gets exactly its own target's, fetched from npm and checked
 * against the lock (`--add-native`).
 *
 *   node scripts/check-choreograph-service.mjs <service-dir> [<lock>] [--target <rust triple>]
 *   node scripts/check-choreograph-service.mjs --add-native <rust triple> <service-dir> [<lock>]
 *
 * Without --target, a binding in the folder must be one the lock names, file
 * for file; with it, the folder holds that target's binding and no other.
 * --add-native fetches the target's binding (`npm pack <package>@<version>`),
 * checks its sha256s, places it, then checks the folder with --target.
 * <lock> defaults to export/choreograph-service.lock.json in a garden checkout,
 * or choreograph-service.lock.json at the root of the public tree. Exit 0 and
 * one line when everything matches; otherwise every difference and exit 1.
 */

import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const NATIVES_DIR = 'app/node_modules/@libsql'
const NATIVE_FILES = ['index.node', 'package.json']
const SHA256 = /^[0-9a-f]{64}$/

const sha256 = path => createHash('sha256').update(readFileSync(path)).digest('hex')

export function parseChoreographServiceLock(source, sourceName = 'choreograph-service.lock.json') {
  const lock = JSON.parse(source)
  if (!lock || typeof lock !== 'object' || Array.isArray(lock)) throw new Error(`${sourceName} must contain a JSON object`)
  if (typeof lock.commit !== 'string' || !/^[0-9a-f]{40}$/.test(lock.commit)) {
    throw new Error(`${sourceName} commit must be a lowercase 40-character SHA`)
  }
  if (!lock.files || typeof lock.files !== 'object' || Object.keys(lock.files).length === 0) {
    throw new Error(`${sourceName} files must map each file of the service to its sha256`)
  }
  for (const [path, sum] of Object.entries(lock.files)) {
    if (path.startsWith('/') || path.split('/').includes('..') || !SHA256.test(String(sum))) {
      throw new Error(`${sourceName}: bad entry ${JSON.stringify(path)}`)
    }
    if (path.startsWith(`${NATIVES_DIR}/`)) throw new Error(`${sourceName}: ${path} is a native binding; those go under natives`)
  }
  if (!lock.natives || typeof lock.natives !== 'object' || Object.keys(lock.natives).length === 0) {
    throw new Error(`${sourceName} natives must name each target's libSQL binding`)
  }
  for (const [target, native] of Object.entries(lock.natives)) {
    if (!/^[a-z0-9_]+-[a-z0-9_]+-[a-z0-9_-]+$/.test(target)) throw new Error(`${sourceName}: bad target ${JSON.stringify(target)}`)
    if (typeof native?.package !== 'string' || !/^@libsql\/[a-z0-9-]+$/.test(native.package)) {
      throw new Error(`${sourceName}: ${target} names no @libsql package`)
    }
    if (typeof native.version !== 'string' || !/^\d+\.\d+\.\d+/.test(native.version)) {
      throw new Error(`${sourceName}: ${target} has no version`)
    }
    for (const file of NATIVE_FILES) {
      if (!SHA256.test(String(native.files?.[file]))) throw new Error(`${sourceName}: ${target} has no sha256 for ${file}`)
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

/** Every difference between the folder and the lock (none: the folder is the locked one). */
export function checkChoreographService(serviceDir, lock, target) {
  const problems = []
  if (target !== undefined && !lock.natives[target]) {
    return [`the lock has no libSQL binding for ${target} (it has ${Object.keys(lock.natives).join(', ')})`]
  }
  const build = JSON.parse(readFileSync(join(serviceDir, 'BUILD.json'), 'utf8'))
  if (build.choreographCommit !== lock.commit) {
    problems.push(`BUILD.json names choreograph ${build.choreographCommit}, the lock ${lock.commit}`)
  }
  if (build.dirty !== false) problems.push('BUILD.json says the choreograph tree had uncommitted changes')
  const found = new Map(
    filesUnder(serviceDir)
      .map(path => relative(serviceDir, path).split('\\').join('/'))
      .filter(path => path !== 'BUILD.json')
      .map(path => [path, sha256(join(serviceDir, path))]),
  )
  const byPackage = new Map(Object.entries(lock.natives).map(([triple, native]) => [native.package, { triple, native }]))
  const nativesFound = new Set()
  for (const [path, sum] of found) {
    if (!path.startsWith(`${NATIVES_DIR}/`)) {
      if (!(path in lock.files)) problems.push(`${path} is not in the lock`)
      else if (lock.files[path] !== sum) problems.push(`${path} differs from the lock`)
      continue
    }
    const [, , scope, name, ...rest] = path.split('/')
    const entry = byPackage.get(`${scope}/${name}`)
    const file = rest.join('/')
    if (!entry) problems.push(`${path} is a binding the lock does not name`)
    else if (!NATIVE_FILES.includes(file)) problems.push(`${path} is not part of a binding`)
    else if (entry.native.files[file] !== sum) problems.push(`${path} differs from the lock`)
    else nativesFound.add(entry.triple)
  }
  for (const path of Object.keys(lock.files)) if (!found.has(path)) problems.push(`missing ${path}`)
  for (const required of ['service.json', 'app/main.mjs', 'body/body.mjs', 'LICENSE', 'THIRD-PARTY-NOTICES.md']) {
    if (!(required in lock.files)) problems.push(`the lock has no ${required}`)
  }
  if (target !== undefined) {
    const native = lock.natives[target]
    for (const file of NATIVE_FILES) {
      const path = `${NATIVES_DIR}/${native.package.slice('@libsql/'.length)}/${file}`
      if (!found.has(path)) problems.push(`missing ${path} (the ${target} binding)`)
    }
    for (const other of nativesFound) if (other !== target) problems.push(`the ${other} binding is here too; a ${target} archive carries its own only`)
  }
  return problems
}

/** Fetch one target's binding from npm at the locked version, check it, put it in the folder. */
export function addNative(serviceDir, lock, target) {
  const native = lock.natives[target]
  if (!native) throw new Error(`the lock has no libSQL binding for ${target}`)
  const work = mkdtempSync(join(tmpdir(), 'choreograph-native-'))
  try {
    const tarball = execFileSync('npm', ['pack', `${native.package}@${native.version}`, '--silent', '--pack-destination', work], {
      encoding: 'utf8',
    }).trim().split('\n').pop()
    execFileSync('tar', ['-xzf', join(work, tarball), '-C', work])
    const into = join(serviceDir, NATIVES_DIR, native.package.slice('@libsql/'.length))
    mkdirSync(into, { recursive: true })
    for (const file of NATIVE_FILES) {
      const from = join(work, 'package', file)
      const sum = sha256(from)
      if (sum !== native.files[file]) throw new Error(`${native.package}@${native.version} ${file} from npm is ${sum}, the lock says ${native.files[file]}`)
      copyFileSync(from, join(into, file))
    }
  } finally {
    rmSync(work, { recursive: true, force: true })
  }
}

function lockPathFrom(lockArg) {
  return lockArg
    ?? [join(root, 'export', 'choreograph-service.lock.json'), join(root, 'choreograph-service.lock.json')].find(existsSync)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2)
  const flag = (name) => {
    const at = args.indexOf(name)
    if (at < 0) return undefined
    const value = args[at + 1]
    args.splice(at, 2)
    if (!value || value.startsWith('--')) {
      console.error(`check-choreograph-service: ${name} needs a value`)
      process.exit(2)
    }
    return value
  }
  const addTarget = flag('--add-native')
  const target = flag('--target') ?? addTarget
  const [serviceDir, lockArg] = args
  if (!serviceDir) {
    console.error('usage: check-choreograph-service.mjs <service-dir> [<lock>] [--target <triple>] | --add-native <triple> <service-dir> [<lock>]')
    process.exit(2)
  }
  const lockPath = lockPathFrom(lockArg)
  if (!lockPath) {
    console.error('check-choreograph-service: no choreograph-service.lock.json found')
    process.exit(2)
  }
  const lock = parseChoreographServiceLock(readFileSync(lockPath, 'utf8'), lockPath)
  if (addTarget) addNative(serviceDir, lock, addTarget)
  const problems = checkChoreographService(serviceDir, lock, target)
  if (problems.length) {
    for (const problem of problems) console.error(`check-choreograph-service: ${problem}`)
    process.exit(1)
  }
  console.log(
    `check-choreograph-service: ${serviceDir} matches choreograph ${lock.commit.slice(0, 12)} (${Object.keys(lock.files).length} files${target ? `, the ${target} libSQL binding` : ''})`,
  )
}

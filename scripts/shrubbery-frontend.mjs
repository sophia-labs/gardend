#!/usr/bin/env node

/**
 * Build adapter between Garden's native shell and its authoritative Shrubbery UI.
 *
 * Garden owns the Tauri/Rust runtime and the generated packaging directory.
 * Shrubbery owns every browser-facing source file. The lock file makes that
 * boundary reproducible instead of silently building whichever sibling branch
 * happens to be checked out.
 */

import { spawn, spawnSync } from 'node:child_process'
import {
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  renameSync,
  writeFileSync,
} from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const gardenRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const lock = JSON.parse(readFileSync(resolve(gardenRoot, 'shrubbery-ui.lock.json'), 'utf8'))
const mode = process.argv[2]

if (!['build', 'dev', 'preview'].includes(mode)) {
  console.error('Usage: node scripts/shrubbery-frontend.mjs <build|dev|preview>')
  process.exit(2)
}

function runGit(root, args, { fatal = true } = {}) {
  const result = spawnSync('git', ['-C', root, ...args], {
    encoding: 'utf8',
    env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
  })
  if (fatal && result.status !== 0) {
    process.stderr.write(result.stderr)
    process.exit(result.status ?? 1)
  }
  return result
}

function isShrubberyCheckout(root) {
  return Boolean(root && existsSync(resolve(root, 'apps/organism/package.json')))
}

function gitCheckoutHead(root) {
  if (!isShrubberyCheckout(root)) return null
  // `git -C child rev-parse HEAD` walks upward and would otherwise mistake the
  // caller's Garden checkout for provenance on an action-materialized archive.
  if (!existsSync(resolve(root, '.git'))) return null
  const result = runGit(root, ['rev-parse', 'HEAD'], { fatal: false })
  return result.status === 0 ? result.stdout.trim() : null
}

function archiveCheckoutHead(root) {
  if (!isShrubberyCheckout(root)) return null
  const marker = resolve(root, '.shrubbery-source-revision')
  if (!existsSync(marker)) return null
  const revision = readFileSync(marker, 'utf8').trim()
  return process.env.SHRUBBERY_ARCHIVE_REVISION === revision ? revision : null
}

function checkoutHead(root) {
  return gitCheckoutHead(root) ?? archiveCheckoutHead(root)
}

function materializePinnedShrubbery() {
  const cacheRoot = resolve(
    process.env.SHRUBBERY_CACHE_DIR ||
      resolve(gardenRoot, 'src-tauri', 'target', 'shrubbery-ui'),
    lock.commit,
  )
  if (checkoutHead(cacheRoot) === lock.commit) return cacheRoot

  const temporaryRoot = `${cacheRoot}.tmp-${process.pid}`
  rmSync(temporaryRoot, { recursive: true, force: true })
  rmSync(cacheRoot, { recursive: true, force: true })
  mkdirSync(dirname(cacheRoot), { recursive: true })

  console.log(`Materializing authoritative Shrubbery UI ${lock.commit.slice(0, 12)}…`)
  const ghAuthenticated = spawnSync(
    'gh',
    ['auth', 'status', '--hostname', 'github.com'],
    { stdio: 'ignore' },
  ).status === 0
  const credentialArgs = ghAuthenticated
    ? ['-c', 'credential.helper=!gh auth git-credential']
    : []
  const commands = [
    ['init', '--quiet', temporaryRoot],
    ['-C', temporaryRoot, 'remote', 'add', 'origin', lock.repository],
    [...credentialArgs, '-C', temporaryRoot, 'fetch', '--quiet', '--depth=1', 'origin', lock.commit],
    ['-C', temporaryRoot, '-c', 'advice.detachedHead=false', 'checkout', '--quiet', '--detach', 'FETCH_HEAD'],
  ]
  for (const args of commands) {
    const result = spawnSync('git', args, {
      encoding: 'utf8',
      env: { ...process.env, GIT_TERMINAL_PROMPT: '0' },
    })
    if (result.status !== 0) {
      rmSync(temporaryRoot, { recursive: true, force: true })
      process.stderr.write(result.stderr)
      console.error(
        `Unable to fetch pinned Shrubbery revision ${lock.commit} from ${lock.repository}. ` +
        'Authenticate gh or set SHRUBBERY_DIR to an exact local checkout.',
      )
      process.exit(result.status ?? 1)
    }
  }
  renameSync(temporaryRoot, cacheRoot)
  return cacheRoot
}

const explicitRoot = process.env.SHRUBBERY_DIR
const siblingRoot = resolve(gardenRoot, '..', 'shrubbery')
let shrubberyRoot

if (explicitRoot) {
  if (!isShrubberyCheckout(explicitRoot)) {
    console.error(`SHRUBBERY_DIR is not a Shrubbery checkout: ${explicitRoot}`)
    process.exit(1)
  }
  shrubberyRoot = explicitRoot
} else if (checkoutHead(siblingRoot) === lock.commit) {
  shrubberyRoot = siblingRoot
} else if (mode !== 'build' && isShrubberyCheckout(siblingRoot)) {
  shrubberyRoot = siblingRoot
} else {
  shrubberyRoot = materializePinnedShrubbery()
}

const gitHead = gitCheckoutHead(shrubberyRoot)
const head = gitHead ?? archiveCheckoutHead(shrubberyRoot)
if (!head) {
  console.error(
    `Shrubbery source at ${shrubberyRoot} has neither Git provenance nor a ` +
    'matching immutable workflow archive marker.',
  )
  process.exit(1)
}
if (head !== lock.commit) {
  console.error(
    `Garden pins Shrubbery ${lock.commit}, but ${shrubberyRoot} is at ${head}. ` +
    'Check out the pinned revision or update shrubbery-ui.lock.json intentionally.',
  )
  process.exit(1)
}

const dirty = gitHead
  ? runGit(shrubberyRoot, ['status', '--porcelain']).stdout.trim()
  : ''
if (dirty && mode === 'build' && process.env.SHRUBBERY_ALLOW_DIRTY !== '1') {
  console.error(
    'Refusing to package a dirty Shrubbery checkout. Commit the UI change, or set ' +
    'SHRUBBERY_ALLOW_DIRTY=1 for a deliberately non-reproducible local build.',
  )
  process.exit(1)
}
if (dirty && mode !== 'build') {
  console.warn('Shrubbery has uncommitted changes; the development server will include them.')
}

function runPnpm(args) {
  const child = spawn('pnpm', ['--dir', shrubberyRoot, ...args], {
    stdio: 'inherit',
    env: process.env,
  })
  for (const signal of ['SIGINT', 'SIGTERM']) {
    process.once(signal, () => child.kill(signal))
  }
  return new Promise((resolveExit, reject) => {
    child.once('error', reject)
    child.once('exit', (code, signal) => {
      if (signal) reject(new Error(`pnpm terminated by ${signal}`))
      else if (code !== 0) reject(new Error(`pnpm exited with status ${code}`))
      else resolveExit()
    })
  })
}

if (mode === 'build') {
  await runPnpm(['install', '--frozen-lockfile'])
  await runPnpm(['--filter', lock.package, 'build'])

  const source = resolve(shrubberyRoot, lock.dist)
  const destination = resolve(gardenRoot, 'frontend/dist')
  if (!existsSync(resolve(source, 'index.html'))) {
    throw new Error(`Shrubbery build did not produce ${source}/index.html`)
  }
  rmSync(destination, { recursive: true, force: true })
  cpSync(source, destination, { recursive: true })
  writeFileSync(
    resolve(destination, 'shrubbery-build.json'),
    `${JSON.stringify({ repository: lock.repository, commit: lock.commit }, null, 2)}\n`,
  )
  console.log(`Packaged ${lock.package}@${lock.commit.slice(0, 12)} for Garden`)
} else {
  const command = mode === 'dev' ? 'dev' : 'preview'
  await runPnpm([
    // pnpm forwards a literal standalone `--` to Vite here; pass the script
    // arguments directly so the pinned UI actually binds Garden's dev port.
    '--filter', lock.package, command,
    '--host', '127.0.0.1',
    '--port', '1420',
    '--strictPort',
  ])
}

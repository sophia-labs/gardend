#!/usr/bin/env node

/**
 * Resolve Garden's authoritative Shrubbery checkout coordinates.
 *
 * GitHub Actions cannot interpolate a step output into an action `uses:` ref.
 * Workflows therefore resolve this lock first and pass its outputs to a normal
 * actions/checkout step. The frontend adapter remains the final provenance and
 * dirty-tree enforcement gate.
 */

import { appendFileSync, readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const scriptPath = fileURLToPath(import.meta.url)
const gardenRoot = resolve(dirname(scriptPath), '..')

export function parseShrubberyLock(source, sourceName = 'shrubbery-ui.lock.json') {
  let lock
  try {
    lock = JSON.parse(source)
  } catch (error) {
    throw new Error(`${sourceName} is not valid JSON: ${error.message}`)
  }

  if (!lock || typeof lock !== 'object' || Array.isArray(lock)) {
    throw new Error(`${sourceName} must contain a JSON object`)
  }
  if (typeof lock.repository !== 'string') {
    throw new Error(`${sourceName} repository must be a GitHub HTTPS URL`)
  }

  let repositoryUrl
  try {
    repositoryUrl = new URL(lock.repository)
  } catch {
    throw new Error(`${sourceName} repository must be a GitHub HTTPS URL`)
  }
  const pathParts = repositoryUrl.pathname.replace(/\.git$/, '').split('/').filter(Boolean)
  if (
    repositoryUrl.protocol !== 'https:' ||
    repositoryUrl.hostname !== 'github.com' ||
    repositoryUrl.port !== '' ||
    repositoryUrl.username !== '' ||
    repositoryUrl.password !== '' ||
    repositoryUrl.search !== '' ||
    repositoryUrl.hash !== '' ||
    !pathParts.every(part => /^[A-Za-z0-9_.-]+$/.test(part)) ||
    pathParts.length !== 2
  ) {
    throw new Error(`${sourceName} repository must be a GitHub HTTPS URL`)
  }
  if (typeof lock.commit !== 'string' || !/^[0-9a-f]{40}$/.test(lock.commit)) {
    throw new Error(`${sourceName} commit must be a lowercase 40-character SHA`)
  }
  if (
    typeof lock.package !== 'string'
    || lock.package.length === 0
    || /[\r\n]/.test(lock.package)
  ) {
    throw new Error(`${sourceName} package must be a non-empty string`)
  }
  if (
    typeof lock.dist !== 'string'
    || lock.dist.length === 0
    || /[\r\n\\]/.test(lock.dist)
    || lock.dist.startsWith('/')
    || lock.dist.split('/').some(part => part === '' || part === '.' || part === '..')
  ) {
    throw new Error(`${sourceName} dist must be a safe relative path`)
  }

  return Object.freeze({
    repository: pathParts.join('/'),
    repositoryUrl: lock.repository,
    commit: lock.commit,
    package: lock.package,
    dist: lock.dist,
  })
}

export function readShrubberyLock(lockPath = resolve(gardenRoot, 'shrubbery-ui.lock.json')) {
  return parseShrubberyLock(readFileSync(lockPath, 'utf8'), lockPath)
}

export function githubOutput(lock) {
  return [
    `repository=${lock.repository}`,
    `repository_url=${lock.repositoryUrl}`,
    `commit=${lock.commit}`,
    `package=${lock.package}`,
    `dist=${lock.dist}`,
    '',
  ].join('\n')
}

function main() {
  const args = process.argv.slice(2)
  const outputIndex = args.indexOf('--github-output')
  const outputPath = outputIndex >= 0 ? args[outputIndex + 1] : null
  if (outputIndex >= 0 && !outputPath) {
    throw new Error('Usage: node scripts/resolve-shrubbery-lock.mjs [--github-output PATH]')
  }
  const lock = readShrubberyLock()
  if (outputPath) appendFileSync(outputPath, githubOutput(lock))
  else process.stdout.write(`${JSON.stringify(lock, null, 2)}\n`)
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  try {
    main()
  } catch (error) {
    console.error(error instanceof Error ? error.message : error)
    process.exitCode = 1
  }
}

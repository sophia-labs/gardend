import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { after, describe, it } from 'node:test'
import { fileURLToPath } from 'node:url'

import {
  githubOutput,
  parseShrubberyLock,
  readShrubberyLock,
} from '../resolve-shrubbery-lock.mjs'

const temporaryRoots = []
const gardenRoot = fileURLToPath(new URL('../..', import.meta.url))

after(() => {
  for (const root of temporaryRoots) rmSync(root, { recursive: true, force: true })
})

describe('Shrubbery lock resolver', () => {
  it('resolves the checked-in lock to checkout-safe outputs', () => {
    const source = JSON.parse(readFileSync(join(gardenRoot, 'shrubbery-ui.lock.json'), 'utf8'))
    const lock = readShrubberyLock()
    assert.deepEqual(lock, {
      repository: 'sophia-labs/shrubbery',
      repositoryUrl: source.repository,
      commit: source.commit,
      package: source.package,
      dist: source.dist,
    })
    const output = githubOutput(lock)
    assert.match(output, new RegExp(`^commit=${source.commit}$`, 'm'))
    assert.match(output, new RegExp(`^repository_url=${source.repository}$`, 'm'))
  })

  it('does not resolve to a differently named repository', () => {
    const lock = readShrubberyLock()
    assert.notEqual(lock.repository, 'sophia-labs/shrubbery-private')
  })

  it('rejects mutable refs and non-GitHub repository coordinates', () => {
    const base = {
      repository: 'https://github.com/sophia-labs/shrubbery.git',
      commit: 'a'.repeat(40),
      package: '@shrubbery/organism',
      dist: 'apps/organism/dist',
    }
    assert.throws(
      () => parseShrubberyLock(JSON.stringify({ ...base, commit: 'main' })),
      /40-character SHA/,
    )
    assert.throws(
      () => parseShrubberyLock(JSON.stringify({ ...base, repository: 'https://example.com/x/y.git' })),
      /GitHub HTTPS URL/,
    )
    assert.throws(
      () => parseShrubberyLock(JSON.stringify({ ...base, repository: 'https://github.com/x/y.git?ref=main' })),
      /GitHub HTTPS URL/,
    )
    assert.throws(
      () => parseShrubberyLock(JSON.stringify({ ...base, package: 'safe\ncommit=evil' })),
      /package must be a non-empty string/,
    )
    assert.throws(
      () => parseShrubberyLock(JSON.stringify({ ...base, dist: '../outside' })),
      /safe relative path/,
    )
  })

  it('writes plain GitHub outputs without workflow expressions or shell evaluation', () => {
    const root = mkdtempSync(join(tmpdir(), 'garden-shrubbery-lock-'))
    temporaryRoots.push(root)
    const output = join(root, 'github-output')
    const result = spawnSync(
      process.execPath,
      [fileURLToPath(new URL('../resolve-shrubbery-lock.mjs', import.meta.url)), '--github-output', output],
      { encoding: 'utf8' },
    )
    assert.equal(result.status, 0, result.stderr)
    const payload = readFileSync(output, 'utf8')
    assert.doesNotMatch(payload, /\$\{|`|\$\(/)
    assert.equal(payload.split('\n').filter(Boolean).length, 5)
    assert.equal(readFileSync(new URL('../resolve-shrubbery-lock.mjs', import.meta.url), 'utf8').includes('eval('), false)
  })

  it('keeps the release workflow dependent on lock outputs, not an external action ref', () => {
    const workflow = readFileSync(join(gardenRoot, '.github/workflows/release.yml'), 'utf8')
    assert.doesNotMatch(workflow, /uses:\s*sophia-labs\/shrubbery@/)
    assert.match(workflow, /node scripts\/resolve-shrubbery-lock\.mjs --github-output "\$GITHUB_OUTPUT"/)
    assert.match(workflow, /repository: \$\{\{ steps\.lock\.outputs\.repository \}\}/)
    assert.match(workflow, /ref: \$\{\{ steps\.lock\.outputs\.commit \}\}/)
  })

  it('needs no Shrubbery credential in any workflow', () => {
    for (const name of ['ci.yml', 'release.yml']) {
      const workflow = readFileSync(join(gardenRoot, '.github/workflows', name), 'utf8')
      assert.doesNotMatch(workflow, /SHRUBBERY_READ_TOKEN/)
    }
  })
})

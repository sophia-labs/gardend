#!/usr/bin/env node
/**
 * Live importer for mnemosyne-mcp parity fixtures.
 *
 * Looks up MNEMOSYNE_MCP_ROOT (or falls back to ~/dev/sophia/mnemosyne-mcp), then
 * runs `python3 tests/parity_fixtures.py` and returns the parsed JSON. The fixtures
 * live in upstream so they can be tagged/extended at the source of truth — this
 * importer is intentionally thin.
 *
 * Failure modes are noisy on purpose: if the upstream layout shifts, the parity
 * harness should surface that as a clear error rather than silently dropping the
 * fixture-driven probes.
 */
import os from 'node:os'
import path from 'node:path'
import { execFile } from 'node:child_process'
import { existsSync } from 'node:fs'

export function resolveMcpRoot() {
  const explicit = process.env.MNEMOSYNE_MCP_ROOT
  if (explicit) return path.resolve(explicit)
  return path.resolve(os.homedir(), 'dev', 'sophia', 'mnemosyne-mcp')
}

export function resolveFixturesPath(root = resolveMcpRoot()) {
  return path.join(root, 'tests', 'parity_fixtures.py')
}

function execText(file, args, options = {}) {
  return new Promise((resolve, reject) => {
    execFile(file, args, { maxBuffer: 4 * 1024 * 1024, ...options }, (error, stdout, stderr) => {
      if (error) {
        reject(new Error(`${file} ${args.join(' ')} failed: ${stderr || error.message}`))
      } else {
        resolve(stdout)
      }
    })
  })
}

export async function loadParityFixtures(options = {}) {
  const root = options.root ?? resolveMcpRoot()
  const fixturesPath = resolveFixturesPath(root)
  if (!existsSync(fixturesPath)) {
    throw new Error(
      `mnemosyne-mcp parity fixtures not found at ${fixturesPath}. `
      + `Set MNEMOSYNE_MCP_ROOT or update tests/parity_fixtures.py upstream.`,
    )
  }
  const stdout = await execText('python3', [fixturesPath])
  let parsed
  try {
    parsed = JSON.parse(stdout)
  } catch (error) {
    throw new Error(
      `Failed to parse parity fixtures JSON from ${fixturesPath}: ${error instanceof Error ? error.message : error}`,
    )
  }
  if (!Array.isArray(parsed)) {
    throw new Error(`Parity fixtures must be an array; got ${typeof parsed}`)
  }
  for (const fixture of parsed) {
    validateFixture(fixture, fixturesPath)
  }
  return parsed
}

function validateFixture(fixture, fixturesPath) {
  if (!fixture || typeof fixture !== 'object') {
    throw new Error(`${fixturesPath}: fixture is not an object`)
  }
  for (const required of ['name', 'family', 'seed', 'op', 'expected']) {
    if (!(required in fixture)) {
      throw new Error(`${fixturesPath}: fixture missing required field "${required}" (name=${fixture.name ?? '<unnamed>'})`)
    }
  }
  if (!fixture.op.tool || typeof fixture.op.tool !== 'string') {
    throw new Error(`${fixturesPath}: fixture ${fixture.name} op.tool must be a string`)
  }
  if (!fixture.op.arguments || typeof fixture.op.arguments !== 'object') {
    throw new Error(`${fixturesPath}: fixture ${fixture.name} op.arguments must be an object`)
  }
  if (!Array.isArray(fixture.expected.blocks)) {
    throw new Error(`${fixturesPath}: fixture ${fixture.name} expected.blocks must be an array`)
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  loadParityFixtures()
    .then((fixtures) => {
      console.log(JSON.stringify({ count: fixtures.length, fixtures }, null, 2))
    })
    .catch((error) => {
      console.error(error.message)
      process.exit(1)
    })
}

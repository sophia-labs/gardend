#!/usr/bin/env node

// Real-process acceptance for the hosted graph-meta boundary. This deliberately
// exercises the built gardend binary rather than an Axum router in-process.

import assert from 'node:assert/strict'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { spawn, spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const binary =
  process.env.GARDEND_BIN ??
  path.join(repository, 'src-tauri/target/debug/examples/gardend')
const secret = 'real-cell-lease-secret-0123456789abcdef'
const binding = {
  owner: 'user:owner',
  graphId: 'notes',
  generation: 1,
  lifecycleState: 'active',
  registryRevision: 7,
}

assert.ok(fs.existsSync(binary), `build the headless gardend example first: ${binary}`)

const delay = (milliseconds) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds))

async function waitFor(read, description, timeoutMilliseconds = 20_000) {
  const deadline = Date.now() + timeoutMilliseconds
  let lastError
  while (Date.now() < deadline) {
    try {
      const value = await read()
      if (value !== undefined && value !== false) return value
    } catch (error) {
      lastError = error
    }
    await delay(50)
  }
  throw new Error(`timed out waiting for ${description}`, { cause: lastError })
}

function environment(profile, durable, snapshot = binding) {
  const result = {
    ...process.env,
    GARDEN_PROFILE_DIR: profile,
    GARDEN_DURABLE_DIR: durable,
    GARDEN_SELF_HEAL_GRAPHS: '1',
    GARDEN_LOOPBACK_HOST: '127.0.0.1',
    GARDEN_LOOPBACK_PORT: '0',
    GARDEN_LOOPBACK_TOKEN: 'cell-meta-real-token',
    GARDEN_CELL_ID: 'c-cell-meta-real',
    GARDEN_CELL_GRAPH_ID: binding.graphId,
    GARDEN_CELL_OWNER: binding.owner,
    GARDEN_CELL_GRAPH_GENERATION: String(binding.generation),
    GARDEN_CELL_REGISTRY_REVISION: String(binding.registryRevision),
    GARDEN_CELL_LEASE_SECRET: secret,
    SOPHIA_OBSERVATORY_CAPTURE_ENABLED: 'true',
    SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256:
      '749ef231fdcfea31fbc0ad9dee6647897eb1581f18dd36b9cf37188a2cbdca68',
    GARDEN_CELL_MACHINE_ID: 'cell:c-cell-meta-real',
    GARDEN_CELL_MACHINE_RUN_ID: '01J0000000000000000000000R',
    GARDEN_FLUSH_INTERVAL_SECONDS: '1',
    GARDEN_FLUSH_DEBOUNCE_SECONDS: '1',
    RUST_LOG: 'error',
  }
  if (snapshot !== null) {
    result.GARDEN_CELL_REGISTRY_SNAPSHOT_JSON = JSON.stringify(snapshot)
  } else {
    delete result.GARDEN_CELL_REGISTRY_SNAPSHOT_JSON
  }
  return result
}

function startCell(profile, durable) {
  const child = spawn(binary, [], {
    cwd: path.join(repository, 'src-tauri'),
    env: environment(profile, durable),
    stdio: ['ignore', 'ignore', 'pipe'],
  })
  let stderr = ''
  child.stderr.on('data', (chunk) => {
    stderr += chunk.toString()
    if (process.env.CELL_META_REAL_DEBUG === '1') process.stderr.write(chunk)
  })
  child.completed = new Promise((resolve, reject) => {
    child.once('error', reject)
    child.once('exit', (code, signal) => resolve({ code, signal, stderr }))
  })
  return child
}

async function stopCell(child) {
  if (child.exitCode !== null) return child.completed
  child.kill('SIGINT')
  return Promise.race([
    child.completed,
    delay(20_000).then(() => {
      child.kill('SIGKILL')
      throw new Error('gardend did not stop after SIGINT')
    }),
  ])
}

async function manifest(profile, expectedPid) {
  return waitFor(() => {
    const manifestPath = path.join(profile, 'loopback.json')
    if (!fs.existsSync(manifestPath)) return undefined
    const value = JSON.parse(fs.readFileSync(manifestPath, 'utf8'))
    // A durable hydrate can restore the previous process's loopback manifest
    // before startup publishes the new endpoint. Never connect to that stale
    // port during a replacement-cell readiness check.
    return value.pid === expectedPid ? value : undefined
  }, 'loopback manifest')
}

function lease(role, overrides = {}) {
  const now = Math.floor(Date.now() / 1000)
  const cellId = `c-${crypto
    .createHash('sha256')
    .update(`${binding.owner}\0${binding.graphId}\0${binding.generation}`)
    .digest('hex')
    .slice(0, 40)}`
  const encode = (value) =>
    Buffer.from(JSON.stringify(value)).toString('base64url')
  const header = encode({ alg: 'HS256', typ: 'JWT' })
  const payload = encode({
    iss: 'pn-gateway',
    aud: 'gardend-cell',
    sub: 'user:caller',
    owner: binding.owner,
    graphId: binding.graphId,
    generation: binding.generation,
    cellId,
    role,
    policyRevision: 11,
    registryRevision: binding.registryRevision,
    sessionId: 'cell-meta-real',
    iat: now,
    exp: now + 300,
    ...overrides,
  })
  const signature = crypto
    .createHmac('sha256', secret)
    .update(`${header}.${payload}`)
    .digest('base64url')
  return `${header}.${payload}.${signature}`
}

async function mcp(manifestValue, cellLease, method, params = {}) {
  // The manifest is durably written immediately before the Axum serve task is
  // scheduled. Retry connection refusal so the test synchronizes on actual
  // tool readiness, not merely manifest publication.
  const response = await waitFor(
    async () =>
      fetch(manifestValue.mcpUrl, {
        method: 'POST',
        headers: {
          authorization: 'Bearer cell-meta-real-token',
          'content-type': 'application/json',
          'x-sophia-cell-lease': cellLease,
        },
        body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
      }),
    'gardend MCP readiness',
  )
  return { status: response.status, body: await response.json() }
}

function rejectedSnapshotCase(root, name, snapshot, expected = {}) {
  const profile = path.join(root, `${name}-profile`)
  const durable = path.join(root, `${name}-durable`)
  const result = spawnSync(binary, [], {
    cwd: path.join(repository, 'src-tauri'),
    env: {
      ...environment(profile, durable, snapshot),
      ...expected,
    },
    stdio: 'ignore',
    timeout: 20_000,
  })
  assert.equal(result.status, 6, `${name} should fail registry preflight`)
  assert.equal(fs.existsSync(profile), false, `${name} touched its profile`)
  assert.equal(fs.existsSync(durable), false, `${name} touched durable storage`)
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'garden-cell-meta-real-'))
const durable = path.join(root, 'durable')
const profile = path.join(root, 'profile')
fs.mkdirSync(durable)
let cell

try {
  cell = startCell(profile, durable)
  const connection = await manifest(profile, cell.pid)
  const initialCurrent = await waitFor(() => {
    const current = path.join(durable, 'CURRENT')
    return fs.existsSync(current) ? fs.readFileSync(current, 'utf8') : undefined
  }, 'initial durable snapshot')

  const viewer = lease('viewer')
  const editor = lease('editor')
  const viewerTools = await mcp(connection, viewer, 'tools/list')
  const editorTools = await mcp(connection, editor, 'tools/list')
  const viewerNames = viewerTools.body.result.tools.map(({ name }) => name)
  const editorNames = editorTools.body.result.tools.map(({ name }) => name)
  assert.ok(viewerNames.includes('read_document'))
  assert.equal(viewerNames.includes('create_document'), false)
  assert.ok(editorNames.includes('create_document'))

  const denied = await mcp(connection, viewer, 'tools/call', {
    name: 'create_document',
    arguments: { graphId: 'notes', documentId: 'doc-real-e2e', title: 'denied' },
  })
  assert.equal(denied.body.error.code, -32003)

  const created = await mcp(connection, editor, 'tools/call', {
    name: 'create_document',
    arguments: {
      graphId: 'notes',
      documentId: 'doc-real-e2e',
      title: 'Real editor write',
    },
  })
  assert.equal(created.body.error, undefined)
  const read = await mcp(connection, viewer, 'tools/call', {
    name: 'read_document',
    arguments: { graphId: 'notes', documentId: 'doc-real-e2e' },
  })
  assert.equal(read.body.result.structuredContent.title, 'Real editor write')

  const wrongOwner = await mcp(
    connection,
    lease('viewer', { owner: 'user:other' }),
    'tools/list',
  )
  assert.equal(wrongOwner.status, 401)

  await waitFor(() => {
    const value = fs.readFileSync(path.join(durable, 'CURRENT'), 'utf8')
    return value !== initialCurrent && value
  }, 'post-write durable snapshot')
  await stopCell(cell)
  cell = undefined

  const restartProfile = path.join(root, 'profile-restart')
  cell = startCell(restartProfile, durable)
  const restarted = await manifest(restartProfile, cell.pid)
  const rehydrated = await mcp(restarted, viewer, 'tools/call', {
    name: 'read_document',
    arguments: { graphId: 'notes', documentId: 'doc-real-e2e' },
  })
  assert.equal(rehydrated.body.result.structuredContent.title, 'Real editor write')
  await stopCell(cell)
  cell = undefined

  rejectedSnapshotCase(root, 'stale-generation', binding, {
    GARDEN_CELL_GRAPH_GENERATION: '2',
  })
  rejectedSnapshotCase(root, 'tombstoned', {
    ...binding,
    lifecycleState: 'tombstoned',
  })
  rejectedSnapshotCase(root, 'wrong-owner', {
    ...binding,
    owner: 'user:other',
  })

  console.log(
    `cell graph-meta real process: ok (viewer tools ${viewerNames.length}, editor tools ${editorNames.length}, durable restart verified)`,
  )
} finally {
  if (cell) await stopCell(cell).catch(() => {})
  fs.rmSync(root, { recursive: true, force: true })
}

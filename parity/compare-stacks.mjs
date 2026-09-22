#!/usr/bin/env node
import fs from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { Buffer } from 'node:buffer'
import { spawn, execFile } from 'node:child_process'
import { loadParityFixtures } from './mcp-fixture-import.mjs'

const args = new Set(process.argv.slice(2))
const markdown = args.has('--markdown')
const strict = args.has('--strict')
const includeSemantic = args.has('--include-semantic')
const includeMutations = args.has('--include-mutations')
const includeWriteFidelity =
  args.has('--include-write-fidelity')
  || includeMutations
  || process.env.SOPHIA_COMPARE_WRITE_FIDELITY === 'true'
const includeMcpFidelity =
  args.has('--include-mcp-fidelity')
  || process.env.SOPHIA_COMPARE_MCP_FIDELITY === 'true'
const includeMcpFixtures =
  args.has('--include-mcp-fixtures')
  || process.env.SOPHIA_COMPARE_MCP_FIXTURES === 'true'

function numberArg(name, fallback) {
  const prefix = `${name}=`
  const arg = process.argv.slice(2).find((item) => item.startsWith(prefix))
  if (!arg) return fallback
  const value = Number(arg.slice(prefix.length))
  return Number.isFinite(value) ? value : fallback
}

const legacyPort = Number(process.env.SOPHIA_LEGACY_LOCAL_PORT ?? 18080)
const legacyBase = process.env.SOPHIA_LEGACY_BASE ?? `http://127.0.0.1:${legacyPort}`
const legacyNamespace = process.env.SOPHIA_LEGACY_NAMESPACE ?? 'prod'
const legacyService = process.env.SOPHIA_LEGACY_SERVICE ?? 'mnemosyne-api'
const legacyServicePort = process.env.SOPHIA_LEGACY_SERVICE_PORT ?? '80'
const legacyMcpPort = Number(process.env.SOPHIA_LEGACY_MCP_LOCAL_PORT ?? 18003)
const legacyMcpBase = process.env.SOPHIA_LEGACY_MCP_BASE ?? `http://127.0.0.1:${legacyMcpPort}`
const legacyMcpService = process.env.SOPHIA_LEGACY_MCP_SERVICE ?? 'mnemosyne-mcp'
const legacyMcpServicePort = process.env.SOPHIA_LEGACY_MCP_SERVICE_PORT ?? '8003'
const legacyUserId = process.env.SOPHIA_LEGACY_USER_ID ?? 'vera'
const jobWaitMs = Number(process.env.SOPHIA_COMPARE_JOB_WAIT_MS ?? 20_000)
const jobPollMs = Number(process.env.SOPHIA_COMPARE_JOB_POLL_MS ?? 500)
const autoPortForward = process.env.SOPHIA_LEGACY_PORT_FORWARD !== 'false'
const repeatCount = Math.max(1, Math.floor(numberArg('--repeat', Number(process.env.SOPHIA_COMPARE_REPEAT ?? 1))))

const manifestPath =
  process.env.SOPHIA_LOOPBACK_MANIFEST ??
  path.join(
    os.homedir(),
    'Library/Application Support/dev.sophia.garden/profiles/default/loopback.json',
  )

let spawnedPortForward = null
let spawnedMcpPortForward = null

process.on('exit', () => {
  if (spawnedPortForward) spawnedPortForward.kill('SIGTERM')
  if (spawnedMcpPortForward) spawnedMcpPortForward.kill('SIGTERM')
})
process.on('SIGINT', () => {
  if (spawnedPortForward) spawnedPortForward.kill('SIGTERM')
  if (spawnedMcpPortForward) spawnedMcpPortForward.kill('SIGTERM')
  process.exit(130)
})

function stopLegacyPortForward() {
  if (!spawnedPortForward) return
  spawnedPortForward.kill('SIGTERM')
  spawnedPortForward = null
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

function execFileText(file, fileArgs) {
  return new Promise((resolve, reject) => {
    execFile(file, fileArgs, { maxBuffer: 1024 * 1024 }, (error, stdout, stderr) => {
      if (error) {
        reject(new Error(`${file} ${fileArgs.join(' ')} failed: ${stderr || error.message}`))
      } else {
        resolve(stdout)
      }
    })
  })
}

async function fetchJson(url, init = {}) {
  const response = await fetch(url, init)
  const text = await response.text()
  let body = null
  try {
    body = text ? JSON.parse(text) : null
  } catch {
    body = parseSseEnvelope(text) ?? text
  }
  return {
    ok: response.ok,
    status: response.status,
    url,
    body,
  }
}

function parseSseEnvelope(text) {
  if (typeof text !== 'string' || !text.includes('data:')) return null
  const lines = text.split(/\r?\n/)
  for (let i = lines.length - 1; i >= 0; i -= 1) {
    const line = lines[i]
    if (!line.startsWith('data:')) continue
    const payload = line.slice('data:'.length).trim()
    if (!payload) continue
    try {
      return JSON.parse(payload)
    } catch {
      return null
    }
  }
  return null
}

async function waitForHealth(baseUrl, timeoutMs = 10_000) {
  const started = Date.now()
  let lastError = null
  while (Date.now() - started < timeoutMs) {
    try {
      const response = await fetchJson(`${baseUrl}/health`)
      if (response.ok) return response
      lastError = new Error(`health returned ${response.status}`)
    } catch (error) {
      lastError = error
    }
    await sleep(250)
  }
  throw lastError ?? new Error(`Timed out waiting for ${baseUrl}/health`)
}

async function ensureLegacyPortForward() {
  try {
    return await waitForHealth(legacyBase, 750)
  } catch (error) {
    if (!autoPortForward) throw error
  }

  spawnedPortForward = spawn(
    'kubectl',
    [
      '-n',
      legacyNamespace,
      'port-forward',
      `svc/${legacyService}`,
      `${legacyPort}:${legacyServicePort}`,
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  )

  let output = ''
  spawnedPortForward.stdout.on('data', (chunk) => { output += chunk.toString() })
  spawnedPortForward.stderr.on('data', (chunk) => { output += chunk.toString() })

  try {
    return await waitForHealth(legacyBase, 12_000)
  } catch (error) {
    throw new Error(`Failed to establish legacy port-forward: ${error.message}\n${output}`)
  }
}

async function ensureLegacyMcpPortForward() {
  try {
    return await waitForHealth(legacyMcpBase, 750)
  } catch (error) {
    if (!autoPortForward) throw error
  }

  spawnedMcpPortForward = spawn(
    'kubectl',
    [
      '-n',
      legacyNamespace,
      'port-forward',
      `svc/${legacyMcpService}`,
      `${legacyMcpPort}:${legacyMcpServicePort}`,
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  )

  let output = ''
  spawnedMcpPortForward.stdout.on('data', (chunk) => { output += chunk.toString() })
  spawnedMcpPortForward.stderr.on('data', (chunk) => { output += chunk.toString() })

  try {
    return await waitForHealth(legacyMcpBase, 12_000)
  } catch (error) {
    throw new Error(`Failed to establish legacy MCP port-forward: ${error.message}\n${output}`)
  }
}

async function getLegacyInternalSecret() {
  if (process.env.SOPHIA_LEGACY_INTERNAL_SECRET) {
    return process.env.SOPHIA_LEGACY_INTERNAL_SECRET
  }
  const encoded = await execFileText('kubectl', [
    '-n',
    legacyNamespace,
    'get',
    'secret',
    'internal-service-auth',
    '-o',
    "jsonpath={.data.secret}",
  ])
  return Buffer.from(encoded, 'base64').toString('utf8')
}

function stackRequest(stack, method, route, body) {
  const headers = { ...stack.headers }
  let outBody = body
  if (route === '/mcp' || route.startsWith('/mcp/')) {
    headers['Accept'] = headers['Accept'] ?? 'application/json, text/event-stream'
    if (stack.name === 'legacy-mcp' && body && typeof body === 'object' && body.params?.arguments) {
      outBody = {
        ...body,
        params: { ...body.params, arguments: toLegacyArgs(body.params.arguments) },
      }
    }
  }
  const init = { method, headers }
  if (outBody !== undefined) {
    if (outBody && typeof outBody === 'object' && outBody.__multipart) {
      const form = new FormData()
      for (const [key, value] of Object.entries(outBody.fields ?? {})) {
        if (value !== undefined && value !== null) form.append(key, String(value))
      }
      form.append(
        'file',
        new Blob([outBody.file?.content ?? ''], { type: outBody.file?.mimeType ?? 'application/octet-stream' }),
        outBody.file?.filename ?? 'upload.bin',
      )
      init.body = form
    } else {
      headers['Content-Type'] = 'application/json'
      init.body = JSON.stringify(outBody)
    }
  }
  return fetchJson(`${stack.base}${route}`, init)
}

function mcpToolBody(name, arguments_) {
  return {
    jsonrpc: '2.0',
    id: `tool-${name}-${Date.now()}`,
    method: 'tools/call',
    params: {
      name,
      arguments: arguments_,
    },
  }
}

const CAMEL_TO_SNAKE = {
  graphId: 'graph_id',
  documentId: 'document_id',
  blockId: 'block_id',
  blockIds: 'block_ids',
  parentId: 'parent_id',
  awaitDurable: 'await_durable',
  includeIds: 'include_ids',
  inheritFormat: 'inherit_format',
  afterBlockId: 'after_block_id',
  beforeBlockId: 'before_block_id',
  expectedRevision: 'expected_revision',
  docFilter: 'doc_filter',
  legacyTool: 'legacy_tool',
  legacyExtra: 'legacy_extra',
  attrs: 'attributes',
}

const LEGACY_DROP = new Set(['format'])

function toLegacyArgs(args) {
  const result = {}
  for (const [key, value] of Object.entries(args ?? {})) {
    if (LEGACY_DROP.has(key)) continue
    if (key in CAMEL_TO_SNAKE) {
      result[CAMEL_TO_SNAKE[key]] = value
    } else if (/^[a-z][a-zA-Z0-9]*$/.test(key) && /[A-Z]/.test(key)) {
      result[key.replace(/[A-Z]/g, (c) => '_' + c.toLowerCase())] = value
    } else {
      result[key] = value
    }
  }
  return result
}

function mcpToolBodyLegacy(name, arguments_) {
  return mcpToolBody(name, toLegacyArgs(arguments_))
}

function multipartUploadBody({ filename, content, mimeType = 'application/octet-stream', fields = {} }) {
  return {
    __multipart: true,
    file: { filename, content, mimeType },
    fields,
  }
}

function storedZip(entries) {
  const localParts = []
  const centralParts = []
  let offset = 0
  for (const [name, value] of Object.entries(entries)) {
    const nameBytes = Buffer.from(name)
    const data = Buffer.isBuffer(value) ? value : Buffer.from(String(value))
    const crc = crc32(data)
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0)
    local.writeUInt16LE(20, 4)
    local.writeUInt16LE(0, 6)
    local.writeUInt16LE(0, 8)
    local.writeUInt16LE(0, 10)
    local.writeUInt16LE(0, 12)
    local.writeUInt32LE(crc, 14)
    local.writeUInt32LE(data.length, 18)
    local.writeUInt32LE(data.length, 22)
    local.writeUInt16LE(nameBytes.length, 26)
    local.writeUInt16LE(0, 28)
    localParts.push(local, nameBytes, data)

    const central = Buffer.alloc(46)
    central.writeUInt32LE(0x02014b50, 0)
    central.writeUInt16LE(20, 4)
    central.writeUInt16LE(20, 6)
    central.writeUInt16LE(0, 8)
    central.writeUInt16LE(0, 10)
    central.writeUInt16LE(0, 12)
    central.writeUInt16LE(0, 14)
    central.writeUInt32LE(crc, 16)
    central.writeUInt32LE(data.length, 20)
    central.writeUInt32LE(data.length, 24)
    central.writeUInt16LE(nameBytes.length, 28)
    central.writeUInt16LE(0, 30)
    central.writeUInt16LE(0, 32)
    central.writeUInt16LE(0, 34)
    central.writeUInt16LE(0, 36)
    central.writeUInt32LE(0, 38)
    central.writeUInt32LE(offset, 42)
    centralParts.push(central, nameBytes)
    offset += local.length + nameBytes.length + data.length
  }
  const centralDirectory = Buffer.concat(centralParts)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(0, 4)
  end.writeUInt16LE(0, 6)
  end.writeUInt16LE(Object.keys(entries).length, 8)
  end.writeUInt16LE(Object.keys(entries).length, 10)
  end.writeUInt32LE(centralDirectory.length, 12)
  end.writeUInt32LE(offset, 16)
  end.writeUInt16LE(0, 20)
  return Buffer.concat([...localParts, centralDirectory, end])
}

const CRC32_TABLE = Array.from({ length: 256 }, (_, index) => {
  let value = index
  for (let bit = 0; bit < 8; bit += 1) {
    value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1
  }
  return value >>> 0
})

function crc32(buffer) {
  let crc = 0xffffffff
  for (const byte of buffer) {
    crc = CRC32_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8)
  }
  return (crc ^ 0xffffffff) >>> 0
}

function cloneRequest(request) {
  return {
    ...request,
    body: request.body !== undefined ? JSON.parse(JSON.stringify(request.body)) : undefined,
  }
}

function parseMcpToolResponse(body) {
  if (!body || typeof body !== 'object') return null
  const structured = body.result?.structuredContent
  if (structured && typeof structured === 'object') return structured
  const text = body.result?.content?.[0]?.text
  if (typeof text === 'string') {
    try {
      return JSON.parse(text)
    } catch {
      return null
    }
  }
  return null
}

async function localCrdtTimings(stack, options = {}) {
  const params = new URLSearchParams()
  if (options.limit !== undefined) params.set('limit', String(options.limit))
  if (options.clear) params.set('clear', 'true')
  if (options.kind) params.set('kind', options.kind)
  if (options.documentId) params.set('documentId', options.documentId)
  if (options.operationId) params.set('operationId', options.operationId)
  const suffix = params.toString() ? `?${params.toString()}` : ''
  const response = await stackRequest(stack, 'GET', `/api/local/crdt-timings${suffix}`)
  if (!response.ok) return null
  return response.body
}

function roundMs(value) {
  return Math.round(value * 10) / 10
}

async function timedStackRequest(stack, method, route, body) {
  const started = performance.now()
  const response = await stackRequest(stack, method, route, body)
  return {
    response,
    elapsedMs: roundMs(performance.now() - started),
  }
}

async function timedResolveMaybeJob(stack, response) {
  const started = performance.now()
  const result = await resolveMaybeJob(stack, response)
  return {
    result,
    elapsedMs: roundMs(performance.now() - started),
  }
}

function resultPathFromJob(body) {
  return body?.links?.result ?? body?.result_url ?? body?.resultUrl ?? null
}

function statusPathFromJob(body) {
  return body?.links?.status ?? body?.poll_url ?? body?.pollUrl ?? null
}

function jobStatus(body) {
  return String(body?.status ?? '').toLowerCase()
}

function isTerminalJob(body) {
  return ['succeeded', 'success', 'complete', 'completed', 'failed', 'error', 'cancelled', 'canceled'].includes(jobStatus(body))
}

async function resolveMaybeJob(stack, response) {
  if (response.status !== 202 || !response.body || typeof response.body !== 'object') {
    return { raw: response, resolved: response, job: null }
  }

  const resultPath = resultPathFromJob(response.body)
  const statusPath = statusPathFromJob(response.body)
  const started = Date.now()
  let lastStatus = response
  let lastResult = null

  while (Date.now() - started < jobWaitMs) {
    if (statusPath) {
      lastStatus = await stackRequest(stack, 'GET', statusPath)
    }
    if (resultPath) {
      lastResult = await stackRequest(stack, 'GET', resultPath)
      if (lastResult.ok) {
        return {
          raw: response,
          resolved: lastResult,
          job: {
            status: jobStatus(lastStatus.body) || jobStatus(response.body) || 'result-ready',
            statusCode: lastStatus.status,
            resultCode: lastResult.status,
            resultPath,
          },
        }
      }
    }
    if (isTerminalJob(lastStatus.body) && !resultPath) break
    if (isTerminalJob(lastStatus.body) && lastResult && !lastResult.ok) break
    await sleep(jobPollMs)
  }

  return {
    raw: response,
    resolved: lastResult?.ok ? lastResult : lastStatus,
    job: {
      status: jobStatus(lastStatus.body) || jobStatus(response.body) || 'timeout',
      statusCode: lastStatus.status,
      resultCode: lastResult?.status ?? null,
      resultPath,
      timedOut: Date.now() - started >= jobWaitMs,
    },
  }
}

function objectKeys(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return []
  return Object.keys(value).sort()
}

function firstArray(value) {
  if (Array.isArray(value)) return value
  for (const key of ['documents', 'folders', 'artifacts', 'results', 'wires', 'predicates', 'data', 'result']) {
    if (Array.isArray(value?.[key])) return value[key]
  }
  return null
}

function extractArray(value) {
  if (Array.isArray(value)) return value
  if (Array.isArray(value?.documents)) return value.documents
  if (Array.isArray(value?.results)) return value.results
  if (Array.isArray(value?.data)) return value.data
  if (Array.isArray(value?.result)) return value.result
  return []
}

function countFields(value) {
  if (!value || typeof value !== 'object') return {}
  const fields = {}
  for (const key of [
    'documents',
    'folders',
    'artifacts',
    'blocks',
    'results',
    'wires',
    'predicates',
    'outgoing_wires',
    'incoming_wires',
    'wired_block_ids',
  ]) {
    if (Array.isArray(value[key])) fields[key] = value[key].length
  }
  if (typeof value.count === 'number') fields.count = value.count
  if (typeof value.total_count === 'number') fields.total_count = value.total_count
  if (typeof value.totalCount === 'number') fields.totalCount = value.totalCount
  return fields
}

function shape(response) {
  const value = response?.body
  const array = firstArray(value)
  const kind = Array.isArray(value) ? 'array' : value === null ? 'null' : typeof value
  return {
    status: response?.status ?? null,
    kind,
    keys: objectKeys(value),
    count: Array.isArray(value) ? value.length : null,
    firstKeys: array?.[0] ? objectKeys(array[0]) : [],
    counts: countFields(value),
  }
}

function diffShape(left, right, options = {}) {
  const differences = []
  if (left.kind !== right.kind) differences.push(`kind ${left.kind} != ${right.kind}`)
  if (left.status !== right.status) differences.push(`status ${left.status} != ${right.status}`)
  if (!options.ignoreCollectionCount && left.count !== right.count) differences.push(`count ${left.count} != ${right.count}`)
  const leftKeys = new Set(left.keys)
  const rightKeys = new Set(right.keys)
  const missingRight = [...leftKeys].filter((key) => !rightKeys.has(key))
  const missingLeft = [...rightKeys].filter((key) => !leftKeys.has(key))
  if (missingRight.length) differences.push(`local missing legacy keys: ${missingRight.join(',')}`)
  if (missingLeft.length) differences.push(`local extra keys: ${missingLeft.join(',')}`)
  const leftFirstKeys = new Set(left.firstKeys)
  const rightFirstKeys = new Set(right.firstKeys)
  const missingFirstRight = [...leftFirstKeys].filter((key) => !rightFirstKeys.has(key))
  const missingFirstLeft = [...rightFirstKeys].filter((key) => !leftFirstKeys.has(key))
  if (missingFirstRight.length) differences.push(`local missing item keys: ${missingFirstRight.join(',')}`)
  if (missingFirstLeft.length) differences.push(`local extra item keys: ${missingFirstLeft.join(',')}`)
  const countKeys = new Set([...Object.keys(left.counts), ...Object.keys(right.counts)])
  for (const key of countKeys) {
    if (options.ignoreCollectionCount) continue
    if ((left.counts[key] ?? null) !== (right.counts[key] ?? null)) {
      differences.push(`${key} count ${left.counts[key] ?? null} != ${right.counts[key] ?? null}`)
    }
  }
  return differences
}

function operationCount(openapi) {
  return Object.values(openapi.paths ?? {}).reduce(
    (count, pathItem) => count + Object.keys(pathItem).filter((key) => ['get', 'post', 'put', 'patch', 'delete'].includes(key)).length,
    0,
  )
}

function chooseDocumentId(payload) {
  const row = extractArray(payload)[0]
  return row?.id ?? row?.document_id ?? row?.documentId ?? null
}

function documentIds(payload) {
  return extractArray(payload)
    .map((row) => row?.id ?? row?.document_id ?? row?.documentId ?? null)
    .filter(Boolean)
}

function chooseSharedDocumentIds(legacyPayload, localPayload) {
  const legacyIds = documentIds(legacyPayload)
  const localIds = new Set(documentIds(localPayload))
  const shared = legacyIds.find((id) => localIds.has(id))
  if (shared) return { legacyDocumentId: shared, localDocumentId: shared }
  return {
    legacyDocumentId: legacyIds[0] ?? null,
    localDocumentId: documentIds(localPayload)[0] ?? null,
  }
}

function chooseGraphId(payload) {
  const row = extractArray(payload)[0]
  return row?.graph_id ?? row?.graphId ?? row?.id ?? null
}

function searchTermFrom(payload, fallback = 'hello') {
  const row = extractArray(payload)[0] ?? payload
  const text = String(row?.title ?? row?.content ?? row?.snippet ?? fallback)
  return text.split(/\s+/).find((part) => part.length > 1) ?? fallback
}

function bindingScalar(binding, key) {
  const value = binding?.[key]
  if (value && typeof value === 'object' && 'value' in value) return value.value
  return value ?? null
}

function uriTail(value) {
  if (!value) return null
  const text = String(value)
  if (text.includes('#')) return text.split('#').pop()
  if (text.includes('/')) return decodeURIComponent(text.split('/').pop())
  if (text.includes(':')) return decodeURIComponent(text.split(':').pop())
  return text
}

function normalizeLegacySearchBlocksBody(body, request) {
  const bindings = body?.results?.bindings
  if (!Array.isArray(bindings)) return body
  const query = request.body?.query ?? ''
  const limit = Number(request.body?.limit ?? bindings.length)
  const rows = bindings.slice(0, Number.isFinite(limit) ? limit : bindings.length)
  const results = rows.map((binding, index) => {
    const blockUri = bindingScalar(binding, 'block') ?? bindingScalar(binding, 'block_uri')
    const documentUri = bindingScalar(binding, 'document') ?? bindingScalar(binding, 'doc')
    const text =
      bindingScalar(binding, 'text')
      ?? bindingScalar(binding, 'content')
      ?? bindingScalar(binding, 'snippet')
      ?? bindingScalar(binding, 'blockText')
      ?? ''
    return {
      block_id:
        bindingScalar(binding, 'block_id')
        ?? bindingScalar(binding, 'blockId')
        ?? uriTail(blockUri)
        ?? `legacy-block-${index}`,
      doc_id:
        bindingScalar(binding, 'doc_id')
        ?? bindingScalar(binding, 'document_id')
        ?? bindingScalar(binding, 'documentId')
        ?? uriTail(documentUri)
        ?? null,
      doc_title:
        bindingScalar(binding, 'doc_title')
        ?? bindingScalar(binding, 'docTitle')
        ?? bindingScalar(binding, 'document_title')
        ?? bindingScalar(binding, 'title')
        ?? null,
      text_preview: String(text).slice(0, 180),
      score: Number(bindingScalar(binding, 'score') ?? 1),
      match_source: 'lexical',
    }
  })
  return {
    query,
    results,
    count: results.length,
    lexical_count: results.length,
    semantic_count: 0,
  }
}

function normalizeProbeResponse(name, side, response, request) {
  if (name.startsWith('mcp-')) {
    return {
      ...response,
      body: normalizeMcpToolResponseBody(response.body),
    }
  }
  if (side === 'legacy' && request.route.startsWith('/search/blocks')) {
    return {
      ...response,
      body: normalizeLegacySearchBlocksBody(response.body, request),
    }
  }
  return response
}

function normalizeMcpToolResponseBody(body) {
  const text = body?.result?.content?.[0]?.text
  if (typeof text === 'string') {
    try {
      return JSON.parse(text)
    } catch {
      return { content: text }
    }
  }
  return body
}

function normalizeWhitespace(value) {
  return String(value ?? '').replace(/\s+/g, ' ').trim()
}

function nullableScalar(value) {
  return value === undefined ? null : value
}

function blockText(block) {
  return normalizeWhitespace(
    block?.content
    ?? block?.text
    ?? block?.plain_text
    ?? block?.plainText
    ?? block?.preview
    ?? block?.text_preview
    ?? block?.textPreview
    ?? '',
  )
}

function normalizeBlockForFidelity(block) {
  if (!block || typeof block !== 'object') return null
  return {
    id: block.id ?? block.block_id ?? block.blockId ?? null,
    type: block.type ?? block.block_type ?? block.blockType ?? null,
    text: blockText(block),
    parentId: nullableScalar(block.parentId ?? block.parent_id),
    order: nullableScalar(block.order),
    level: nullableScalar(block.level),
    checked: nullableScalar(block.checked),
    language: nullableScalar(block.language),
    marks: Array.isArray(block.marks) ? block.marks.map((mark) => ({
      type: mark?.type ?? mark?.mark_type ?? mark?.markType ?? null,
      start: nullableScalar(mark?.start),
      end: nullableScalar(mark?.end),
      href: nullableScalar(mark?.href),
      targetDocId: nullableScalar(mark?.targetDocId ?? mark?.target_doc_id),
      targetBlockId: nullableScalar(mark?.targetBlockId ?? mark?.target_block_id),
      label: nullableScalar(mark?.label),
    })) : [],
  }
}

function unwrapMcpEnvelope(body) {
  if (body && typeof body === 'object' && body.result && typeof body.result === 'object') {
    if (body.result.structuredContent && typeof body.result.structuredContent === 'object') {
      return body.result.structuredContent
    }
    const text = body.result.content?.[0]?.text
    if (typeof text === 'string') {
      try { return JSON.parse(text) } catch { return body }
    }
  }
  return body
}

function normalizeDocumentForFidelity(body) {
  const unwrapped = unwrapMcpEnvelope(body)
  const blocks = Array.isArray(unwrapped?.blocks)
    ? unwrapped.blocks
    : Array.isArray(unwrapped?.result?.blocks)
      ? unwrapped.result.blocks
      : []
  return {
    id: unwrapped?.id ?? unwrapped?.document_id ?? unwrapped?.documentId ?? unwrapped?.metadata?.documentId ?? unwrapped?.metadata?.document_id ?? null,
    graphId: unwrapped?.graphId ?? unwrapped?.graph_id ?? null,
    title: normalizeWhitespace(unwrapped?.title ?? unwrapped?.metadata?.title ?? ''),
    revision: nullableScalar(unwrapped?.revision),
    blockCount: blocks.length,
    blocks: blocks.map(normalizeBlockForFidelity).filter(Boolean),
  }
}

function compareExpectedScalar(differences, label, path, actual, expected) {
  if (expected === undefined) return
  if (actual !== expected) differences.push(`${label} ${path} ${JSON.stringify(actual)} != ${JSON.stringify(expected)}`)
}

function compareExpectedText(differences, label, path, actual, expected) {
  if (expected === undefined) return
  const actualText = normalizeWhitespace(actual)
  const expectedText = normalizeWhitespace(expected)
  if (actualText !== expectedText) {
    differences.push(`${label} ${path} ${JSON.stringify(actualText)} != ${JSON.stringify(expectedText)}`)
  }
}

function documentFidelityDifferences(label, body, expected) {
  const differences = []
  const actual = normalizeDocumentForFidelity(body)
  compareExpectedScalar(differences, label, 'id', actual.id, expected.id)
  compareExpectedScalar(differences, label, 'graphId', actual.graphId, expected.graphId)
  compareExpectedText(differences, label, 'title', actual.title, expected.title)
  if (expected.blocks) {
    if (actual.blocks.length < expected.blocks.length) {
      differences.push(`${label} blocks length ${actual.blocks.length} < expected ${expected.blocks.length}`)
    }
    expected.blocks.forEach((expectedBlock, index) => {
      const actualBlock = actual.blocks[index]
      if (!actualBlock) {
        differences.push(`${label} blocks[${index}] missing`)
        return
      }
      compareExpectedScalar(differences, label, `blocks[${index}].type`, actualBlock.type, expectedBlock.type)
      compareExpectedText(differences, label, `blocks[${index}].text`, actualBlock.text, expectedBlock.text)
      if (expectedBlock.textIncludes !== undefined && expectedBlock.textIncludes !== null) {
        const needle = normalizeWhitespace(expectedBlock.textIncludes)
        const haystack = normalizeWhitespace(actualBlock.text)
        if (!haystack.includes(needle)) {
          differences.push(
            `${label} blocks[${index}].text does not contain ${JSON.stringify(needle)} (got ${JSON.stringify(haystack)})`,
          )
        }
      }
      compareExpectedScalar(differences, label, `blocks[${index}].parentId`, actualBlock.parentId, expectedBlock.parentId)
      compareExpectedScalar(differences, label, `blocks[${index}].level`, actualBlock.level, expectedBlock.level)
      compareExpectedScalar(differences, label, `blocks[${index}].checked`, actualBlock.checked, expectedBlock.checked)
      compareExpectedScalar(differences, label, `blocks[${index}].language`, actualBlock.language, expectedBlock.language)
      if (expectedBlock.markTypes) {
        const actualMarkTypes = actualBlock.marks.map((mark) => mark.type).filter(Boolean)
        for (const markType of expectedBlock.markTypes) {
          if (!actualMarkTypes.includes(markType)) {
            differences.push(`${label} blocks[${index}].marks missing ${markType}`)
          }
        }
      }
    })
  }
  return { actual, differences }
}

function searchResults(body) {
  if (Array.isArray(body?.results)) return body.results
  if (Array.isArray(body?.data)) return body.data
  if (Array.isArray(body?.result)) return body.result
  if (Array.isArray(body)) return body
  return []
}

function searchResultDocumentId(result) {
  return result?.doc_id
    ?? result?.document_id
    ?? result?.documentId
    ?? result?.docId
    ?? result?.id
    ?? null
}

function searchResultText(result) {
  return normalizeWhitespace(
    result?.content
    ?? result?.text
    ?? result?.snippet
    ?? result?.text_preview
    ?? result?.textPreview
    ?? result?.doc_title
    ?? result?.docTitle
    ?? result?.title
    ?? '',
  )
}

function searchFidelityDifferences(label, body, expected) {
  const differences = []
  const results = searchResults(body)
  if (results.length < (expected.minResults ?? 1)) {
    differences.push(`${label} search results length ${results.length} < expected ${expected.minResults ?? 1}`)
  }
  if (expected.documentId && !results.some((result) => searchResultDocumentId(result) === expected.documentId)) {
    differences.push(`${label} search results missing document ${expected.documentId}`)
  }
  if (expected.textIncludes) {
    const needle = normalizeWhitespace(expected.textIncludes)
    if (!results.some((result) => searchResultText(result).includes(needle))) {
      differences.push(`${label} search results missing text ${JSON.stringify(needle)}`)
    }
  }
  return {
    actual: results.map((result) => ({
      documentId: searchResultDocumentId(result),
      blockId: result?.block_id ?? result?.blockId ?? null,
      text: searchResultText(result),
      score: nullableScalar(result?.score),
      matchSource: result?.match_source ?? result?.matchSource ?? null,
    })),
    differences,
  }
}

function missingText(value) {
  if (value === undefined || value === null) return ''
  if (typeof value === 'string') return value
  try {
    return JSON.stringify(value)
  } catch {
    return String(value)
  }
}

function responseLooksMissing(response) {
  if ([404, 410].includes(response.status)) return true
  const text = missingText(response.body).toLowerCase()
  return text.includes('not found') || text.includes('doc_not_found') || text.includes('document_not_found')
}

function missingDocumentDifferences(label, response) {
  return responseLooksMissing(response)
    ? []
    : [`${label} deleted document read did not return a missing/not-found response`]
}

function summarizeTimings(timings) {
  const totals = timings.map((timing) => timing.totalMs).sort((left, right) => left - right)
  const median = totals[Math.floor(totals.length / 2)] ?? null
  const latest = timings[timings.length - 1] ?? {}
  return {
    samples: timings.length,
    requestMs: latest.requestMs ?? null,
    resolveMs: latest.resolveMs ?? null,
    totalMs: latest.totalMs ?? null,
    minTotalMs: totals[0] ?? null,
    medianTotalMs: median,
    maxTotalMs: totals[totals.length - 1] ?? null,
  }
}

function timingComparison(legacyTiming, localTiming) {
  const legacyMs = legacyTiming.medianTotalMs ?? legacyTiming.totalMs
  const localMs = localTiming.medianTotalMs ?? localTiming.totalMs
  if (legacyMs === null || localMs === null) {
    return {
      localMinusLegacyMs: null,
      localToLegacyRatio: null,
    }
  }
  return {
    localMinusLegacyMs: roundMs(localMs - legacyMs),
    localToLegacyRatio: legacyMs > 0 ? Math.round((localMs / legacyMs) * 100) / 100 : null,
  }
}

async function probeOnce(name, legacyStack, localStack, legacyRequest, localRequest, options = {}) {
  if (options.localTrace) {
    await localCrdtTimings(localStack, { clear: true })
  }
  // Hocuspocus-style parity: legacy reads see live Y.Doc state via Hocuspocus,
  // while our local reads go through the materialized projection. Probes that
  // verify post-mutation state set flushLocalBeforeRun:true to force the
  // local cold flush so the projection catches up to the in-memory Y.Doc.
  if (options.flushLocalBeforeRun) {
    const graphId = options.flushLocalBeforeRun.graphId ?? null
    const documentId = options.flushLocalBeforeRun.documentId ?? null
    if (graphId) {
      await stackRequest(localStack, 'POST', '/mcp', mcpToolBody('flush_crdt', {
        graphId,
        graph_id: graphId,
        ...(documentId ? { documentId, document_id: documentId } : {}),
      }))
    }
  }
  let resolvedLegacyRequest = legacyRequest
  let resolvedLocalRequest = localRequest
  if (typeof options.rewriteBeforeRun === 'function') {
    resolvedLegacyRequest = await options.rewriteBeforeRun(cloneRequest(legacyRequest), 'legacy')
    resolvedLocalRequest = await options.rewriteBeforeRun(cloneRequest(localRequest), 'local')
  }
  const [rawLegacy, rawLocal] = await Promise.all([
    timedStackRequest(legacyStack, resolvedLegacyRequest.method, resolvedLegacyRequest.route, resolvedLegacyRequest.body),
    timedStackRequest(localStack, resolvedLocalRequest.method, resolvedLocalRequest.route, resolvedLocalRequest.body),
  ])
  const [legacyResolved, localResolved] = await Promise.all([
    timedResolveMaybeJob(legacyStack, rawLegacy.response),
    timedResolveMaybeJob(localStack, rawLocal.response),
  ])
  const legacy = legacyResolved.result
  const local = localResolved.result
  const legacyResponse = normalizeProbeResponse(name, 'legacy', legacy.resolved, resolvedLegacyRequest)
  const localResponse = normalizeProbeResponse(name, 'local', local.resolved, resolvedLocalRequest)
  const legacyTiming = {
    requestMs: rawLegacy.elapsedMs,
    resolveMs: legacyResolved.elapsedMs,
    totalMs: roundMs(rawLegacy.elapsedMs + legacyResolved.elapsedMs),
  }
  const localTiming = {
    requestMs: rawLocal.elapsedMs,
    resolveMs: localResolved.elapsedMs,
    totalMs: roundMs(rawLocal.elapsedMs + localResolved.elapsedMs),
  }
  const traceResponse = options.localTrace
    ? await localCrdtTimings(localStack, {
      limit: 10,
      kind: options.localTrace.kind,
      documentId: options.localTrace.documentId,
    })
    : null
  const localTrace = traceResponse?.traces?.at(-1) ?? null
  const legacyShape = shape(legacyResponse)
  const localShape = shape(localResponse)
  const performance = timingComparison(
    summarizeTimings([legacyTiming]),
    summarizeTimings([localTiming]),
  )
  let fidelity = null
  const valueDifferences = []
  if (options.expectedDocument) {
    const legacyDocument = documentFidelityDifferences('legacy', legacyResponse.body, options.expectedDocument)
    const localDocument = documentFidelityDifferences('local', localResponse.body, options.expectedDocument)
    valueDifferences.push(...legacyDocument.differences, ...localDocument.differences)
    fidelity = {
      kind: 'document',
      expected: options.expectedDocument,
      legacy: legacyDocument.actual,
      local: localDocument.actual,
    }
  }
  if (options.expectedSearch) {
    const legacySearch = searchFidelityDifferences('legacy', legacyResponse.body, options.expectedSearch)
    const localSearch = searchFidelityDifferences('local', localResponse.body, options.expectedSearch)
    valueDifferences.push(...legacySearch.differences, ...localSearch.differences)
    fidelity = {
      kind: 'search',
      expected: options.expectedSearch,
      legacy: legacySearch.actual,
      local: localSearch.actual,
    }
  }
  if (options.expectedMissingDocument) {
    valueDifferences.push(
      ...missingDocumentDifferences('legacy', legacyResponse),
      ...missingDocumentDifferences('local', localResponse),
    )
  }
  const shapeDifferences = options.ignoreShape
    ? []
    : diffShape(legacyShape, localShape, options)
  return {
    name,
    legacy: {
      rawStatus: legacy.raw.status,
      resolvedStatus: legacyResponse.status,
      job: legacy.job,
      shape: legacyShape,
      timing: legacyTiming,
    },
    local: {
      rawStatus: local.raw.status,
      resolvedStatus: localResponse.status,
      job: local.job,
      shape: localShape,
      timing: localTiming,
      timingTrace: localTrace,
    },
    performance,
    differences: [
      ...shapeDifferences,
      ...valueDifferences,
    ],
    fidelity,
    expectations: {
      missingDocument: Boolean(options.expectedMissingDocument),
    },
    bodies: {
      legacy: legacyResponse.body,
      local: localResponse.body,
    },
  }
}

async function probe(name, legacyStack, localStack, legacyRequest, localRequest, options = {}) {
  const samples = Math.max(1, Math.floor(options.repeatCount ?? repeatCount))
  const runs = []
  for (let index = 0; index < samples; index += 1) {
    runs.push(await probeOnce(name, legacyStack, localStack, legacyRequest, localRequest, options))
  }
  const primary = runs[runs.length - 1]
  const legacyTiming = summarizeTimings(runs.map((run) => run.legacy.timing))
  const localTiming = summarizeTimings(runs.map((run) => run.local.timing))
  return {
    ...primary,
    legacy: {
      ...primary.legacy,
      timing: legacyTiming,
    },
    local: {
      ...primary.local,
      timing: localTiming,
    },
    performance: timingComparison(legacyTiming, localTiming),
  }
}

async function cleanupStackDocument(stack, graphId, documentId) {
  if (!graphId || !documentId) return null
  const response = await stackRequest(
    stack,
    'DELETE',
    `/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}`,
  )
  return resolveMaybeJob(stack, response)
}

async function cleanupStackFolder(stack, graphId, folderId) {
  if (!graphId || !folderId) return null
  const response = await stackRequest(
    stack,
    'DELETE',
    `/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(folderId)}`,
  )
  return resolveMaybeJob(stack, response)
}

async function cleanupStackDocuments(pairs) {
  await Promise.all(pairs.map(({ stack, graphId, documentId }) =>
    cleanupStackDocument(stack, graphId, documentId).catch((error) => {
      console.error(`[warn] cleanup ${stack.name}/${documentId}: ${error.message}`)
      return null
    }),
  ))
}

async function cleanupEpubUpload(stack, graphId, primaryDocumentId, title) {
  const navigation = await stackRequest(
    stack,
    'GET',
    `/navigation/${encodeURIComponent(graphId)}`,
  )
  const folders = navigation.body?.folders ?? []
  const documents = navigation.body?.documents ?? []
  const primary = documents.find((document) =>
    (document.id ?? document.documentId ?? document.document_id) === primaryDocumentId)
  const folder = folders.find((candidate) => {
    const label = candidate.label ?? candidate.name ?? candidate.title
    const folderId = candidate.id ?? candidate.folderId ?? candidate.folder_id
    return label === title && (primary?.parentId === folderId || primary?.parent_id === folderId)
  })
  const folderId = folder?.id ?? folder?.folderId ?? folder?.folder_id ?? null
  const documentsToDelete = folderId
    ? documents.filter((document) => (document.parentId ?? document.parent_id) === folderId)
    : documents.filter((document) => (document.id ?? document.documentId ?? document.document_id) === primaryDocumentId)
  await cleanupStackDocuments(documentsToDelete.map((document) => ({
    stack,
    graphId,
    documentId: document.id ?? document.documentId ?? document.document_id,
  })))
  if (folderId) {
    await cleanupStackFolder(stack, graphId, folderId).catch((error) => {
      console.error(`[warn] cleanup ${stack.name}/${folderId}: ${error.message}`)
      return null
    })
  }
}

function scrubProbe(probeResult) {
  const { bodies, ...rest } = probeResult
  if (process.env.SOPHIA_COMPARE_KEEP_BODIES === 'true') {
    return { ...rest, bodies }
  }
  return rest
}

function statusCell(item) {
  const median = item.timing?.medianTotalMs ?? item.timing?.totalMs
  const suffix = item.timing?.samples > 1 ? ` p50 (${item.timing.samples}x)` : ''
  const timing = median === null || median === undefined ? '' : `<br>${median}ms${suffix}`
  return `${item.rawStatus}->${item.resolvedStatus} ${item.shape.kind}${timing}`
}

function performanceCell(item) {
  const delta = item.performance?.localMinusLegacyMs
  const ratio = item.performance?.localToLegacyRatio
  if (delta === null || delta === undefined) return 'n/a'
  const signedDelta = delta > 0 ? `+${delta}` : `${delta}`
  return `${signedDelta}ms / ${ratio ?? 'n/a'}x`
}

function phaseValue(phases, key) {
  const value = phases?.[key]
  return typeof value === 'number' ? `${value}ms` : null
}

function localPhaseCell(item) {
  const phases = item.local?.timingTrace?.phases
  if (!phases) return ''
  const keys = [
    ['queue', 'rustQueueToPollMs'],
    ['js', 'jsTotalMs'],
    ['doc', 'saveDocumentSnapshotMs'],
    ['rdf-doc', 'rustMaterializeDocumentRdfMs'],
    ['ws', 'saveFilesystemWorkspaceMs'],
    ['rdf-ws', 'rustMaterializeWorkspaceRdfMs'],
    ['readback', 'hostedResponseReadbackMs'],
  ]
  return keys
    .map(([label, key]) => {
      const value = phaseValue(phases, key)
      return value ? `${label} ${value}` : null
    })
    .filter(Boolean)
    .join('<br>')
}

function renderMarkdown(report) {
  const lines = []
  lines.push('# Local/Legacy API Parity Probe')
  lines.push('')
  lines.push(`- Legacy: \`${report.stacks.legacy.base}\` graph \`${report.stacks.legacy.graphId ?? 'none'}\` user \`${report.stacks.legacy.userId}\``)
  lines.push(`- Local: \`${report.stacks.local.base}\` graph \`${report.stacks.local.graphId ?? 'none'}\``)
  lines.push(`- OpenAPI: legacy ${report.openapi.legacy.pathCount} paths / ${report.openapi.legacy.operationCount} ops; local ${report.openapi.local.pathCount} paths / ${report.openapi.local.operationCount} ops; shared paths ${report.openapi.sharedPathCount}`)
  lines.push(`- Timing samples: ${report.options.repeatCount}`)
  lines.push(`- Mutations: ${report.options.includeMutations ? 'included' : 'skipped'}`)
  lines.push(`- Write fidelity: ${report.options.includeWriteFidelity ? 'included' : 'skipped'}`)
  lines.push(`- MCP fidelity: ${report.options.includeMcpFidelity ? 'included' : 'skipped'}`)
  lines.push('')
  lines.push('| Probe | Legacy | Local | Perf | Local Phases | Differences |')
  lines.push('| --- | --- | --- | --- | --- | --- |')
  for (const item of report.probes) {
    lines.push(`| ${item.name} | ${statusCell(item.legacy)} | ${statusCell(item.local)} | ${performanceCell(item)} | ${localPhaseCell(item)} | ${item.differences.join('<br>') || 'none'} |`)
  }
  lines.push('')
  return lines.join('\n')
}

await ensureLegacyPortForward()
const legacySecret = await getLegacyInternalSecret()
const manifest = JSON.parse(await fs.readFile(manifestPath, 'utf8'))

const needsMcp = includeMcpFidelity || includeMcpFixtures
if (needsMcp) {
  await ensureLegacyMcpPortForward()
}

const legacyStack = {
  name: 'legacy-k8s',
  base: legacyBase,
  headers: {
    'X-Internal-Service': legacySecret,
    'X-User-ID': legacyUserId,
  },
}
const legacyMcpStack = {
  name: 'legacy-mcp',
  base: legacyMcpBase,
  headers: {
    'X-Internal-Service': legacySecret,
    'X-User-ID': legacyUserId,
  },
}
const localStack = {
  name: 'local-tauri',
  base: manifest.apiUrl,
  headers: {
    Authorization: `Bearer ${manifest.token}`,
  },
}

const legacyOpenapi = await stackRequest(legacyStack, 'GET', '/openapi.json')
const localOpenapi = await stackRequest(localStack, 'GET', '/openapi.json')
const legacyPaths = new Set(Object.keys(legacyOpenapi.body?.paths ?? {}))
const localPaths = new Set(Object.keys(localOpenapi.body?.paths ?? {}))
const sharedPaths = [...localPaths].filter((routePath) => legacyPaths.has(routePath)).sort()

const legacyGraphProbe = await probe(
  'graph-list',
  legacyStack,
  localStack,
  { method: 'GET', route: '/graphs/catalog' },
  { method: 'GET', route: '/graphs/catalog' },
  { ignoreCollectionCount: true },
)
const legacyGraphId = process.env.SOPHIA_LEGACY_GRAPH_ID ?? chooseGraphId(legacyGraphProbe.bodies.legacy)
const localGraphId = process.env.SOPHIA_LOCAL_GRAPH_ID ?? chooseGraphId(legacyGraphProbe.bodies.local)

const probes = [
  await probe('health', legacyStack, localStack, { method: 'GET', route: '/health' }, { method: 'GET', route: '/health' }),
  await probe('openapi', legacyStack, localStack, { method: 'GET', route: '/openapi.json' }, { method: 'GET', route: '/openapi.json' }),
  legacyGraphProbe,
  await probe('graphs', legacyStack, localStack, { method: 'GET', route: '/graphs' }, { method: 'GET', route: '/graphs' }, { ignoreCollectionCount: true }),
]

let legacyDocumentId = null
let localDocumentId = null
if (legacyGraphId && localGraphId) {
  const documentsProbe = await probe(
    'documents-list',
    legacyStack,
    localStack,
    { method: 'GET', route: `/documents/${encodeURIComponent(legacyGraphId)}` },
    { method: 'GET', route: `/documents/${encodeURIComponent(localGraphId)}` },
  )
  probes.push(documentsProbe)
  const chosenDocumentIds = chooseSharedDocumentIds(documentsProbe.bodies.legacy, documentsProbe.bodies.local)
  legacyDocumentId = chosenDocumentIds.legacyDocumentId
  localDocumentId = chosenDocumentIds.localDocumentId

  probes.push(await probe(
    'navigation',
    legacyStack,
    localStack,
    { method: 'GET', route: `/navigation/${encodeURIComponent(legacyGraphId)}` },
    { method: 'GET', route: `/navigation/${encodeURIComponent(localGraphId)}` },
  ))
  probes.push(await probe(
    'navigation-folders',
    legacyStack,
    localStack,
    { method: 'GET', route: `/navigation/${encodeURIComponent(legacyGraphId)}/folders` },
    { method: 'GET', route: `/navigation/${encodeURIComponent(localGraphId)}/folders` },
  ))
  probes.push(await probe(
    'navigation-artifacts',
    legacyStack,
    localStack,
    { method: 'GET', route: `/navigation/${encodeURIComponent(legacyGraphId)}/artifacts` },
    { method: 'GET', route: `/navigation/${encodeURIComponent(localGraphId)}/artifacts` },
  ))

  if (legacyDocumentId && localDocumentId) {
    const legacyDocRoute = `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyDocumentId)}`
    const localDocRoute = `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localDocumentId)}`
    const documentProbe = await probe(
      'document-read',
      legacyStack,
      localStack,
      { method: 'GET', route: legacyDocRoute },
      { method: 'GET', route: localDocRoute },
    )
    probes.push(documentProbe)
    probes.push(await probe(
      'document-blocks',
      legacyStack,
      localStack,
      { method: 'GET', route: `${legacyDocRoute}/blocks` },
      { method: 'GET', route: `${localDocRoute}/blocks` },
    ))
    probes.push(await probe(
      'document-block-context',
      legacyStack,
      localStack,
      { method: 'GET', route: `${legacyDocRoute}/block-context` },
      { method: 'GET', route: `${localDocRoute}/block-context` },
    ))
    probes.push(await probe(
      'wire-predicates',
      legacyStack,
      localStack,
      { method: 'GET', route: `/wires/${encodeURIComponent(legacyGraphId)}/predicates` },
      { method: 'GET', route: `/wires/${encodeURIComponent(localGraphId)}/predicates` },
    ))
    probes.push(await probe(
      'wire-bundle',
      legacyStack,
      localStack,
      { method: 'GET', route: `/wires/${encodeURIComponent(legacyGraphId)}/document/${encodeURIComponent(legacyDocumentId)}/bundle` },
      { method: 'GET', route: `/wires/${encodeURIComponent(localGraphId)}/document/${encodeURIComponent(localDocumentId)}/bundle` },
    ))

    const legacyTerm = searchTermFrom(documentProbe.bodies.legacy, 'hello')
    const localTerm = searchTermFrom(documentProbe.bodies.local, 'hello')
    probes.push(await probe(
      'search-blocks',
      legacyStack,
      localStack,
      { method: 'POST', route: '/search/blocks', body: { graph_id: legacyGraphId, query: legacyTerm, limit: 3 } },
      { method: 'POST', route: '/search/blocks', body: { graph_id: localGraphId, query: localTerm, limit: 3 } },
    ))
    if (includeSemantic) {
      probes.push(await probe(
        'search-hybrid',
        legacyStack,
        localStack,
        { method: 'POST', route: '/search/hybrid', body: { graph_id: legacyGraphId, query: legacyTerm, limit: 3, min_score: 0 } },
        { method: 'POST', route: '/search/hybrid', body: { graph_id: localGraphId, query: localTerm, limit: 3, min_score: 0 } },
      ))
    }
  }

  if (includeMutations || includeWriteFidelity) {
    const mutationDocumentId =
      process.env.SOPHIA_COMPARE_MUTATION_DOC_ID
      ?? `parity-write-smoke-${Date.now()}`
    const mutationToken = `parity-token-${mutationDocumentId}`
    const mutationBody = {
      title: 'Parity Write Smoke',
      parentId: null,
      expectedRevision: 0,
      blocks: [
        {
          id: `${mutationDocumentId}-b1`,
          type: 'heading',
          content: 'Parity Write Smoke',
          parentId: null,
          order: 0,
          level: 2,
          marks: [],
        },
        {
          id: `${mutationDocumentId}-b2`,
          type: 'paragraph',
          content: `Marked bold content survives through both stacks. ${mutationToken}`,
          parentId: null,
          order: 1,
          marks: [
            {
              id: `${mutationDocumentId}-m1`,
              type: 'bold',
              start: 7,
              end: 11,
            },
          ],
        },
        {
          id: `${mutationDocumentId}-b3`,
          type: 'todo',
          content: 'Task item parity',
          parentId: null,
          order: 2,
          checked: true,
          marks: [],
        },
        {
          id: `${mutationDocumentId}-b4`,
          type: 'code',
          content: 'SELECT * WHERE { ?s ?p ?o }',
          parentId: null,
          order: 3,
          language: 'sparql',
          marks: [],
        },
      ],
    }
    const expectedMutationDocument = {
      id: mutationDocumentId,
      title: 'Parity Write Smoke',
      blocks: [
        {
          type: 'heading',
          text: 'Parity Write Smoke',
          parentId: null,
          level: 2,
        },
        {
          type: 'paragraph',
          text: `Marked bold content survives through both stacks. ${mutationToken}`,
          parentId: null,
          markTypes: ['bold'],
        },
        {
          type: 'todo',
          text: 'Task item parity',
          parentId: null,
          checked: true,
        },
        {
          type: 'code',
          text: 'SELECT * WHERE { ?s ?p ?o }',
          parentId: null,
          language: 'sparql',
        },
      ],
    }
    const artifactId =
      process.env.SOPHIA_COMPARE_ARTIFACT_ID
      ?? `parity-artifact-${Date.now()}`
    const artifactBody = {
      label: 'Parity Artifact',
      parentId: null,
      order: Date.now(),
      fileType: 'pdf',
      status: 'ready',
      storageKey: `local://parity/${artifactId}.pdf`,
      originalFilename: `${artifactId}.pdf`,
      mimeType: 'application/pdf',
      sizeBytes: 1234,
    }
    probes.push(await probe(
      'artifact-put',
      legacyStack,
      localStack,
      {
        method: 'PUT',
        route: `/navigation/${encodeURIComponent(legacyGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
        body: artifactBody,
      },
      {
        method: 'PUT',
        route: `/navigation/${encodeURIComponent(localGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
        body: artifactBody,
      },
      {
        repeatCount: 1,
        localTrace: {
          kind: 'workspace.putArtifact',
          documentId: artifactId,
        },
      },
    ))
    probes.push(await probe(
      'artifact-get-after-put',
      legacyStack,
      localStack,
      {
        method: 'GET',
        route: `/navigation/${encodeURIComponent(legacyGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      },
      {
        method: 'GET',
        route: `/navigation/${encodeURIComponent(localGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      },
      { repeatCount: 1 },
    ))
    probes.push(await probe(
      'artifact-list-after-put',
      legacyStack,
      localStack,
      {
        method: 'GET',
        route: `/navigation/${encodeURIComponent(legacyGraphId)}/artifacts`,
      },
      {
        method: 'GET',
        route: `/navigation/${encodeURIComponent(localGraphId)}/artifacts`,
      },
      { repeatCount: 1 },
    ))
    probes.push(await probe(
      'artifact-update',
      legacyStack,
      localStack,
      {
        method: 'PUT',
        route: `/navigation/${encodeURIComponent(legacyGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
        body: { ...artifactBody, label: 'Parity Artifact Updated', status: 'processing' },
      },
      {
        method: 'PUT',
        route: `/navigation/${encodeURIComponent(localGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
        body: { ...artifactBody, label: 'Parity Artifact Updated', status: 'processing' },
      },
      {
        repeatCount: 1,
        localTrace: {
          kind: 'workspace.putArtifact',
          documentId: artifactId,
        },
      },
    ))
    probes.push(await probe(
      'artifact-delete',
      legacyStack,
      localStack,
      {
        method: 'DELETE',
        route: `/navigation/${encodeURIComponent(legacyGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      },
      {
        method: 'DELETE',
        route: `/navigation/${encodeURIComponent(localGraphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      },
      {
        repeatCount: 1,
        localTrace: {
          kind: 'workspace.deleteArtifact',
          documentId: artifactId,
        },
      },
    ))
    probes.push(await probe(
      'artifact-list-after-delete',
      legacyStack,
      localStack,
      {
        method: 'GET',
        route: `/navigation/${encodeURIComponent(legacyGraphId)}/artifacts`,
      },
      {
        method: 'GET',
        route: `/navigation/${encodeURIComponent(localGraphId)}/artifacts`,
      },
      { repeatCount: 1 },
    ))

    const imageFilename = `parity-image-${Date.now()}.png`
    const imageBytes = Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=',
      'base64',
    )
    const imageUploadProbe = await probe(
      'artifact-image-upload',
      legacyStack,
      localStack,
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(legacyGraphId)}/images/upload`,
        body: multipartUploadBody({
          filename: imageFilename,
          mimeType: 'image/png',
          content: imageBytes,
        }),
      },
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(localGraphId)}/images/upload`,
        body: multipartUploadBody({
          filename: imageFilename,
          mimeType: 'image/png',
          content: imageBytes,
        }),
      },
      { repeatCount: 1 },
    )
    probes.push(imageUploadProbe)
    const legacyImageSrc = imageUploadProbe.bodies?.legacy?.src
    const localImageSrc = imageUploadProbe.bodies?.local?.src
    if (legacyImageSrc && localImageSrc) {
      probes.push(await probe(
        'artifact-image-read',
        legacyStack,
        localStack,
        { method: 'GET', route: legacyImageSrc },
        { method: 'GET', route: localImageSrc },
        { repeatCount: 1 },
      ))
    }

    const uploadFilename = `parity-upload-${Date.now()}.md`
    const uploadToken = `upload-token-${Date.now()}`
    const uploadMarkdown = `# Upload Smoke

Paragraph with **bold _italic_** and [link](https://example.test). ${uploadToken}

- [x] done
  - nested

\`\`\`ts
const value = 1
\`\`\``
    const uploadProbe = await probe(
      'artifact-upload',
      legacyStack,
      localStack,
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(legacyGraphId)}/upload`,
        body: multipartUploadBody({
          filename: uploadFilename,
          mimeType: 'text/markdown',
          content: uploadMarkdown,
        }),
      },
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(localGraphId)}/upload`,
        body: multipartUploadBody({
          filename: uploadFilename,
          mimeType: 'text/markdown',
          content: uploadMarkdown,
        }),
      },
      {
        repeatCount: 1,
        localTrace: { kind: 'document.uploadIngest' },
      },
    )
    probes.push(uploadProbe)

    const legacyUploadedDocumentId = uploadProbe.bodies?.legacy?.documentId ?? uploadProbe.bodies?.legacy?.document_id
    const localUploadedDocumentId = uploadProbe.bodies?.local?.documentId ?? uploadProbe.bodies?.local?.document_id
    if (legacyUploadedDocumentId && localUploadedDocumentId) {
      const expectedUploadDocument = {
        title: 'Upload Smoke',
        blocks: [
          { type: 'heading', text: 'Upload Smoke', level: 1 },
          { type: 'paragraph', textIncludes: uploadToken, markTypes: ['bold', 'italic', 'link'] },
          { type: 'todo', checked: true },
          { type: 'bullet' },
          { type: 'code', language: 'ts' },
        ],
      }
      probes.push(await probe(
        'artifact-upload-read',
        legacyStack,
        localStack,
        {
          method: 'GET',
          route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyUploadedDocumentId)}`,
        },
        {
          method: 'GET',
          route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localUploadedDocumentId)}`,
        },
        {
          repeatCount: 1,
          expectedDocument: expectedUploadDocument,
        },
      ))
      probes.push(await probe(
        'artifact-upload-delete',
        legacyStack,
        localStack,
        {
          method: 'DELETE',
          route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyUploadedDocumentId)}`,
        },
        {
          method: 'DELETE',
          route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localUploadedDocumentId)}`,
        },
        { repeatCount: 1 },
      ))
    }

    const htmlTitle = `HTML Upload Smoke ${Date.now()}`
    const htmlFilename = `${htmlTitle.toLowerCase().replace(/[^a-z0-9]+/g, '-')}.html`
    const htmlToken = `html-token-${Date.now()}`
    const uploadHtml = `<!doctype html>
<html>
  <head><title>${htmlTitle}</title><style>.ignored { color: red; }</style></head>
  <body>
    <nav>Navigation should not become imported content.</nav>
    <main>
      <h1>Visible HTML Smoke</h1>
      <p>HTML paragraph with <strong>bold</strong>, <em>italic</em>, and ${htmlToken}.</p>
      <ul><li>Outer item<ul><li>Nested item</li></ul></li></ul>
    </main>
  </body>
</html>`
    const htmlUploadProbe = await probe(
      'artifact-html-upload',
      legacyStack,
      localStack,
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(legacyGraphId)}/upload`,
        body: multipartUploadBody({
          filename: htmlFilename,
          mimeType: 'text/html',
          content: uploadHtml,
        }),
      },
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(localGraphId)}/upload`,
        body: multipartUploadBody({
          filename: htmlFilename,
          mimeType: 'text/html',
          content: uploadHtml,
        }),
      },
      {
        repeatCount: 1,
        localTrace: { kind: 'document.uploadIngest' },
      },
    )
    probes.push(htmlUploadProbe)
    const legacyHtmlDocumentId = htmlUploadProbe.bodies?.legacy?.documentId ?? htmlUploadProbe.bodies?.legacy?.document_id
    const localHtmlDocumentId = htmlUploadProbe.bodies?.local?.documentId ?? htmlUploadProbe.bodies?.local?.document_id
    if (legacyHtmlDocumentId && localHtmlDocumentId) {
      probes.push(await probe(
        'artifact-html-upload-read',
        legacyStack,
        localStack,
        {
          method: 'GET',
          route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyHtmlDocumentId)}`,
        },
        {
          method: 'GET',
          route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localHtmlDocumentId)}`,
        },
        {
          repeatCount: 1,
          expectedDocument: {
            title: htmlTitle,
            blocks: [
              { type: 'heading', text: 'Visible HTML Smoke', level: 1 },
              { type: 'paragraph', textIncludes: htmlToken, markTypes: ['bold', 'italic'] },
              { type: 'bullet' },
              { type: 'bullet' },
            ],
          },
        },
      ))
      probes.push(await probe(
        'artifact-html-upload-delete',
        legacyStack,
        localStack,
        {
          method: 'DELETE',
          route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyHtmlDocumentId)}`,
        },
        {
          method: 'DELETE',
          route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localHtmlDocumentId)}`,
        },
        { repeatCount: 1 },
      ))
    }

    const epubTitle = `EPUB Upload Smoke ${Date.now()}`
    const epubFilename = `${epubTitle.toLowerCase().replace(/[^a-z0-9]+/g, '-')}.epub`
    const chapterText = 'This chapter text is intentionally long enough to avoid stub chapter merging. '.repeat(8)
    const epubBytes = storedZip({
      'META-INF/container.xml': `<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>`,
      'OEBPS/content.opf': `<?xml version="1.0"?>
<package version="3.0" xmlns="http://www.idpf.org/2007/opf">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>${epubTitle}</dc:title></metadata>
  <manifest>
    <item id="chapter-one" href="chapters/chapter-one.xhtml" media-type="application/xhtml+xml"/>
    <item id="chapter-two" href="chapters/chapter-two.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="chapter-one"/><itemref idref="chapter-two"/></spine>
</package>`,
      'OEBPS/chapters/chapter-one.xhtml': `<!doctype html><html><head><title>Chapter One</title></head><body><main><h1>Chapter One</h1><p>${chapterText}</p></main></body></html>`,
      'OEBPS/chapters/chapter-two.xhtml': `<!doctype html><html><head><title>Chapter Two</title></head><body><main><h1>Chapter Two</h1><p>${chapterText}</p></main></body></html>`,
    })
    const epubUploadProbe = await probe(
      'artifact-epub-upload',
      legacyStack,
      localStack,
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(legacyGraphId)}/upload`,
        body: multipartUploadBody({
          filename: epubFilename,
          mimeType: 'application/epub+zip',
          content: epubBytes,
        }),
      },
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(localGraphId)}/upload`,
        body: multipartUploadBody({
          filename: epubFilename,
          mimeType: 'application/epub+zip',
          content: epubBytes,
        }),
      },
      {
        repeatCount: 1,
        localTrace: { kind: 'document.uploadIngest' },
      },
    )
    probes.push(epubUploadProbe)
    const legacyEpubDocumentId = epubUploadProbe.bodies?.legacy?.documentId ?? epubUploadProbe.bodies?.legacy?.document_id
    const localEpubDocumentId = epubUploadProbe.bodies?.local?.documentId ?? epubUploadProbe.bodies?.local?.document_id
    if (legacyEpubDocumentId && localEpubDocumentId) {
      probes.push(await probe(
        'artifact-epub-upload-read',
        legacyStack,
        localStack,
        {
          method: 'GET',
          route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyEpubDocumentId)}`,
        },
        {
          method: 'GET',
          route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localEpubDocumentId)}`,
        },
        {
          repeatCount: 1,
          expectedDocument: {
            title: epubTitle,
            blocks: [
              { type: 'heading', text: epubTitle, level: 1 },
              { type: 'paragraph', text: '2 chapters imported as separate documents.' },
            ],
          },
        },
      ))
      await Promise.all([
        cleanupEpubUpload(legacyStack, legacyGraphId, legacyEpubDocumentId, epubTitle),
        cleanupEpubUpload(localStack, localGraphId, localEpubDocumentId, epubTitle),
      ])
    }

    const batchKey = `parity-batch-${Date.now()}`
    const batchRoot = `${batchKey}-root`
    const batchNested = `${batchRoot}/nested`
    const batchPrepareProbe = await probe(
      'artifact-batch-prepare',
      legacyStack,
      localStack,
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(legacyGraphId)}/batch/prepare`,
        body: {
          clientBatchKey: batchKey,
          folders: [batchNested],
        },
      },
      {
        method: 'POST',
        route: `/artifacts/${encodeURIComponent(localGraphId)}/batch/prepare`,
        body: {
          clientBatchKey: batchKey,
          folders: [batchNested],
        },
      },
      {
        repeatCount: 1,
        localTrace: { kind: 'document.batchPrepare' },
      },
    )
    probes.push(batchPrepareProbe)

    const legacyBatchId = batchPrepareProbe.bodies?.legacy?.batchId
    const localBatchId = batchPrepareProbe.bodies?.local?.batchId
    const legacyFolderMap = batchPrepareProbe.bodies?.legacy?.folderMap ?? {}
    const localFolderMap = batchPrepareProbe.bodies?.local?.folderMap ?? {}
    if (legacyBatchId && localBatchId && legacyFolderMap[batchNested] && localFolderMap[batchNested]) {
      const batchFilename = `${batchKey}.md`
      const batchToken = `batch-token-${Date.now()}`
      const batchMarkdown = `# Batch Upload Smoke

Nested batch content ${batchToken}

- [x] registered

\`\`\`ts
const batch = true
\`\`\``
      const batchUploadProbe = await probe(
        'artifact-batch-upload',
        legacyStack,
        localStack,
        {
          method: 'POST',
          route: `/artifacts/${encodeURIComponent(legacyGraphId)}/upload`,
          body: multipartUploadBody({
            filename: batchFilename,
            mimeType: 'text/markdown',
            content: batchMarkdown,
            fields: { batch_id: legacyBatchId },
          }),
        },
        {
          method: 'POST',
          route: `/artifacts/${encodeURIComponent(localGraphId)}/upload`,
          body: multipartUploadBody({
            filename: batchFilename,
            mimeType: 'text/markdown',
            content: batchMarkdown,
            fields: { batch_id: localBatchId },
          }),
        },
        {
          repeatCount: 1,
          localTrace: { kind: 'document.uploadIngest' },
        },
      )
      probes.push(batchUploadProbe)

      const legacyBatchDocumentId = batchUploadProbe.bodies?.legacy?.documentId ?? batchUploadProbe.bodies?.legacy?.document_id
      const localBatchDocumentId = batchUploadProbe.bodies?.local?.documentId ?? batchUploadProbe.bodies?.local?.document_id
      if (legacyBatchDocumentId && localBatchDocumentId) {
        probes.push(await probe(
          'artifact-batch-register',
          legacyStack,
          localStack,
          {
            method: 'POST',
            route: `/artifacts/${encodeURIComponent(legacyGraphId)}/batch/register`,
            body: {
              batchId: legacyBatchId,
              documents: [{
                documentId: legacyBatchDocumentId,
                title: batchUploadProbe.bodies?.legacy?.title,
                relativePath: `${batchNested}/${batchFilename}`,
                readOnly: batchUploadProbe.bodies?.legacy?.readOnly,
                sourceFile: batchUploadProbe.bodies?.legacy?.sourceFile,
              }],
            },
          },
          {
            method: 'POST',
            route: `/artifacts/${encodeURIComponent(localGraphId)}/batch/register`,
            body: {
              batchId: localBatchId,
              documents: [{
                documentId: localBatchDocumentId,
                title: batchUploadProbe.bodies?.local?.title,
                relativePath: `${batchNested}/${batchFilename}`,
                readOnly: batchUploadProbe.bodies?.local?.readOnly,
                sourceFile: batchUploadProbe.bodies?.local?.sourceFile,
              }],
            },
          },
          {
            repeatCount: 1,
            localTrace: { kind: 'document.batchRegister' },
          },
        ))
        probes.push(await probe(
          'artifact-batch-read',
          legacyStack,
          localStack,
          {
            method: 'GET',
            route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyBatchDocumentId)}`,
          },
          {
            method: 'GET',
            route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localBatchDocumentId)}`,
          },
          {
            repeatCount: 1,
            expectedDocument: {
              title: 'Batch Upload Smoke',
              blocks: [
                { type: 'heading', text: 'Batch Upload Smoke', level: 1 },
                { type: 'paragraph', textIncludes: batchToken },
                { type: 'todo', checked: true },
                { type: 'code', language: 'ts' },
              ],
            },
          },
        ))
        probes.push(await probe(
          'artifact-batch-delete',
          legacyStack,
          localStack,
          {
            method: 'DELETE',
            route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyBatchDocumentId)}`,
          },
          {
            method: 'DELETE',
            route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localBatchDocumentId)}`,
          },
          { repeatCount: 1 },
        ))
      }

      for (const folderPath of [batchNested, batchRoot]) {
        probes.push(await probe(
          `artifact-batch-delete-folder-${folderPath === batchNested ? 'nested' : 'root'}`,
          legacyStack,
          localStack,
          {
            method: 'DELETE',
            route: `/navigation/${encodeURIComponent(legacyGraphId)}/folders/${encodeURIComponent(legacyFolderMap[folderPath])}`,
          },
          {
            method: 'DELETE',
            route: `/navigation/${encodeURIComponent(localGraphId)}/folders/${encodeURIComponent(localFolderMap[folderPath])}`,
          },
          {
            repeatCount: 1,
            localTrace: {
              kind: 'workspace.deleteFolder',
              documentId: localFolderMap[folderPath],
            },
          },
        ))
      }
    }

    probes.push(await probe(
      'document-put',
      legacyStack,
      localStack,
      {
        method: 'PUT',
        route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
        body: mutationBody,
      },
      {
        method: 'PUT',
        route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
        body: mutationBody,
      },
      {
        repeatCount: 1,
        expectedDocument: expectedMutationDocument,
        localTrace: {
          kind: 'document.write',
          documentId: mutationDocumentId,
        },
      },
    ))
    probes.push(await probe(
      'document-read-after-put',
      legacyStack,
      localStack,
      {
        method: 'GET',
        route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
      },
      {
        method: 'GET',
        route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
      },
      {
        repeatCount: 1,
        expectedDocument: expectedMutationDocument,
      },
    ))
    probes.push(await probe(
      'document-blocks-after-put',
      legacyStack,
      localStack,
      {
        method: 'GET',
        route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(mutationDocumentId)}/blocks`,
      },
      {
        method: 'GET',
        route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(mutationDocumentId)}/blocks`,
      },
      {
        repeatCount: 1,
        expectedDocument: {
          blocks: [
            {
              type: 'heading',
              text: 'Parity Write Smoke',
              level: 2,
            },
            {
              type: 'paragraph',
              text: `Marked bold content survives through both stacks. ${mutationToken}`,
            },
            {
              text: 'Task item parity',
            },
            {
              text: 'SELECT * WHERE { ?s ?p ?o }',
            },
          ],
        },
      },
    ))
    probes.push(await probe(
      'document-search-after-put',
      legacyStack,
      localStack,
      {
        method: 'POST',
        route: '/search/blocks?wait_ms=8000',
        body: { graph_id: legacyGraphId, query: mutationToken, limit: 5 },
      },
      {
        method: 'POST',
        route: '/search/blocks?wait_ms=8000',
        body: { graph_id: localGraphId, query: mutationToken, limit: 5 },
      },
      {
        repeatCount: 1,
        expectedSearch: {
          documentId: mutationDocumentId,
          textIncludes: mutationToken,
          minResults: 1,
        },
      },
    ))
    const wireCreateBody = {
      source_block_id: null,
      target_graph_id: legacyGraphId,
      target_document_id: mutationDocumentId,
      target_block_id: null,
      predicate: 'supports',
      bidirectional: false,
    }
    const localWireCreateBody = {
      ...wireCreateBody,
      target_graph_id: localGraphId,
      targetGraphId: localGraphId,
      target_document_id: mutationDocumentId,
      targetDocumentId: mutationDocumentId,
    }
    const wireCreateProbe = await probe(
      'wire-create',
      legacyStack,
      localStack,
      {
        method: 'POST',
        route: `/wires/${encodeURIComponent(legacyGraphId)}/document/${encodeURIComponent(legacyDocumentId)}`,
        body: wireCreateBody,
      },
      {
        method: 'POST',
        route: `/wires/${encodeURIComponent(localGraphId)}/document/${encodeURIComponent(localDocumentId)}`,
        body: localWireCreateBody,
      },
      {
        repeatCount: 1,
        localTrace: {
          kind: 'workspace.createWire',
          documentId: localDocumentId,
        },
      },
    )
    probes.push(wireCreateProbe)
    const legacyWireId = wireCreateProbe.bodies.legacy?.id
    const localWireId = wireCreateProbe.bodies.local?.id
    if (legacyWireId && localWireId) {
      probes.push(await probe(
        'wire-source-bundle-after-create',
        legacyStack,
        localStack,
        {
          method: 'GET',
          route: `/wires/${encodeURIComponent(legacyGraphId)}/document/${encodeURIComponent(legacyDocumentId)}/bundle`,
        },
        {
          method: 'GET',
          route: `/wires/${encodeURIComponent(localGraphId)}/document/${encodeURIComponent(localDocumentId)}/bundle`,
        },
        { repeatCount: 1 },
      ))
      probes.push(await probe(
        'wire-refresh',
        legacyStack,
        localStack,
        {
          method: 'POST',
          route: `/wires/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyWireId)}/refresh`,
        },
        {
          method: 'POST',
          route: `/wires/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localWireId)}/refresh`,
        },
        {
          repeatCount: 1,
          localTrace: {
            kind: 'workspace.refreshWire',
            documentId: localDocumentId,
          },
        },
      ))
      probes.push(await probe(
        'wire-delete',
        legacyStack,
        localStack,
        {
          method: 'DELETE',
          route: `/wires/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(legacyWireId)}`,
        },
        {
          method: 'DELETE',
          route: `/wires/${encodeURIComponent(localGraphId)}/${encodeURIComponent(localWireId)}`,
        },
        {
          repeatCount: 1,
          localTrace: {
            kind: 'workspace.deleteWire',
            documentId: localDocumentId,
          },
        },
      ))
      probes.push(await probe(
        'wire-source-bundle-after-delete',
        legacyStack,
        localStack,
        {
          method: 'GET',
          route: `/wires/${encodeURIComponent(legacyGraphId)}/document/${encodeURIComponent(legacyDocumentId)}/bundle`,
        },
        {
          method: 'GET',
          route: `/wires/${encodeURIComponent(localGraphId)}/document/${encodeURIComponent(localDocumentId)}/bundle`,
        },
        { repeatCount: 1 },
      ))
    }
    probes.push(await probe(
      'document-delete',
      legacyStack,
      localStack,
      {
        method: 'DELETE',
        route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
      },
      {
        method: 'DELETE',
        route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
      },
      {
        repeatCount: 1,
        localTrace: {
          kind: 'workspace.deleteDocument',
          documentId: mutationDocumentId,
        },
      },
    ))
    probes.push(await probe(
      'document-read-after-delete',
      legacyStack,
      localStack,
      {
        method: 'GET',
        route: `/documents/${encodeURIComponent(legacyGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
      },
      {
        method: 'GET',
        route: `/documents/${encodeURIComponent(localGraphId)}/${encodeURIComponent(mutationDocumentId)}`,
      },
      {
        repeatCount: 1,
        expectedMissingDocument: true,
        ignoreShape: true,
      },
    ))
  }

  if (includeMcpFidelity) {
    const mcpDocumentId =
      process.env.SOPHIA_COMPARE_MCP_DOC_ID
      ?? `parity-mcp-write-${Date.now()}`
    const mcpToken = `mcp-token-${mcpDocumentId}`
    const mcpContent = `# MCP Write Smoke\n\nMCP write_document should roundtrip through TipTap XML, Y.Doc, RDF materialization, and block search. ${mcpToken}`
    const mcpWriteArguments = {
      graph_id: legacyGraphId,
      graphId: legacyGraphId,
      document_id: mcpDocumentId,
      documentId: mcpDocumentId,
      content: mcpContent,
      await_durable: true,
      awaitDurable: true,
    }
    const localMcpWriteArguments = {
      ...mcpWriteArguments,
      graph_id: localGraphId,
      graphId: localGraphId,
    }
    probes.push(await probe(
      'mcp-write-document',
      legacyMcpStack,
      localStack,
      { method: 'POST', route: '/mcp', body: mcpToolBody('write_document', mcpWriteArguments) },
      { method: 'POST', route: '/mcp', body: mcpToolBody('write_document', localMcpWriteArguments) },
      { repeatCount: 1 },
    ))
    probes.push(await probe(
      'mcp-read-blocks-after-write',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: legacyGraphId,
          graphId: legacyGraphId,
          document_id: mcpDocumentId,
          documentId: mcpDocumentId,
          limit: 10,
          include_ids: true,
          includeIds: true,
          format: 'text',
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: localGraphId,
          graphId: localGraphId,
          document_id: mcpDocumentId,
          documentId: mcpDocumentId,
          limit: 10,
          include_ids: true,
          includeIds: true,
          format: 'text',
        }),
      },
      {
        repeatCount: 1,
        expectedDocument: {
          blocks: [
            { text: 'MCP Write Smoke' },
            { text: `MCP write_document should roundtrip through TipTap XML, Y.Doc, RDF materialization, and block search. ${mcpToken}` },
          ],
        },
      },
    ))
    probes.push(await probe(
      'mcp-search-blocks-after-write',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('search_blocks', {
          graph_id: legacyGraphId,
          graphId: legacyGraphId,
          query: mcpToken,
          limit: 5,
          mode: 'lexical',
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('search_blocks', {
          graph_id: localGraphId,
          graphId: localGraphId,
          query: mcpToken,
          limit: 5,
          mode: 'lexical',
        }),
      },
      {
        repeatCount: 1,
        expectedSearch: {
          documentId: mcpDocumentId,
          textIncludes: mcpToken,
          minResults: 1,
        },
      },
    ))
    probes.push(await probe(
      'mcp-delete-document',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete_document', {
          graph_id: legacyGraphId,
          graphId: legacyGraphId,
          document_id: mcpDocumentId,
          documentId: mcpDocumentId,
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete_document', {
          graph_id: localGraphId,
          graphId: localGraphId,
          document_id: mcpDocumentId,
          documentId: mcpDocumentId,
        }),
      },
      { repeatCount: 1 },
    ))
    await cleanupStackDocuments([
      { stack: legacyStack, graphId: legacyGraphId, documentId: mcpDocumentId },
      { stack: localStack, graphId: localGraphId, documentId: mcpDocumentId },
    ])

    const blockDocId =
      process.env.SOPHIA_COMPARE_BLOCK_DOC_ID ?? `parity-block-mutations-${Date.now()}`
    const blockToken = `block-token-${blockDocId}`
    const blockSeedContent = `# Block Mutations Probe\n\nThe quick brown fox.\n\nTrailing paragraph for stability. ${blockToken}`
    const blockSeedArgs = (graph) => ({
      graph_id: graph,
      graphId: graph,
      document_id: blockDocId,
      documentId: blockDocId,
      content: blockSeedContent,
      format: 'markdown',
      await_durable: true,
      awaitDurable: true,
    })
    probes.push(await probe(
      'mcp-block-seed',
      legacyMcpStack,
      localStack,
      { method: 'POST', route: '/mcp', body: mcpToolBody('write_document', blockSeedArgs(legacyGraphId)) },
      { method: 'POST', route: '/mcp', body: mcpToolBody('write_document', blockSeedArgs(localGraphId)) },
      { repeatCount: 1 },
    ))
    const insertContent = 'Inserted via insert_blocks parity probe.'
    const insertArgs = (graph) => ({
      graph_id: graph,
      graphId: graph,
      document_id: blockDocId,
      documentId: blockDocId,
      content: insertContent,
      format: 'plain',
      position: 'after',
    })
    probes.push(await probe(
      'mcp-insert-blocks',
      legacyMcpStack,
      localStack,
      { method: 'POST', route: '/mcp', body: mcpToolBody('insert_blocks', insertArgs(legacyGraphId)) },
      { method: 'POST', route: '/mcp', body: mcpToolBody('insert_blocks', insertArgs(localGraphId)) },
      {
        repeatCount: 1,
        localTrace: { kind: 'block.insert', documentId: blockDocId },
      },
    ))
    probes.push(await probe(
      'mcp-read-blocks-after-insert',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: legacyGraphId, graphId: legacyGraphId,
          document_id: blockDocId, documentId: blockDocId,
          limit: 50, include_ids: true, includeIds: true, format: 'text',
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: localGraphId, graphId: localGraphId,
          document_id: blockDocId, documentId: blockDocId,
          limit: 50, include_ids: true, includeIds: true, format: 'text',
        }),
      },
      {
        repeatCount: 1,
        expectedDocument: {
          blocks: [
            { text: 'Block Mutations Probe' },
            { text: 'The quick brown fox.' },
            { text: `Trailing paragraph for stability. ${blockToken}` },
            { text: insertContent },
          ],
        },
      },
    ))
    probes.push(await probe(
      'mcp-edit-block-text',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('edit_block_text', {
          graph_id: legacyGraphId, graphId: legacyGraphId,
          document_id: blockDocId, documentId: blockDocId,
          block_id: '__SECOND_BLOCK__',
          blockId: '__SECOND_BLOCK__',
          operations: [{ type: 'insert', offset: 19, text: 'er' }],
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('edit_block_text', {
          graph_id: localGraphId, graphId: localGraphId,
          document_id: blockDocId, documentId: blockDocId,
          block_id: '__SECOND_BLOCK__',
          blockId: '__SECOND_BLOCK__',
          operations: [{ type: 'insert', offset: 19, text: 'er' }],
        }),
      },
      {
        repeatCount: 1,
        rewriteBeforeRun: async (request, stackKey) => {
          const stack = stackKey === 'legacy' ? legacyMcpStack : localStack
          const blocksBody = mcpToolBody('read_blocks', {
            graph_id: stackKey === 'legacy' ? legacyGraphId : localGraphId,
            graphId: stackKey === 'legacy' ? legacyGraphId : localGraphId,
            document_id: blockDocId,
            documentId: blockDocId,
            limit: 50,
            include_ids: true,
            includeIds: true,
            format: 'text',
          })
          const response = await stackRequest(stack, 'POST', '/mcp', blocksBody)
          const parsed = parseMcpToolResponse(response.body)
          const blocks = parsed?.blocks ?? []
          const second = blocks.find((block) => /quick brown fox/i.test(String(block.content ?? block.text ?? '')))
          const blockId = second?.blockId ?? second?.block_id
          if (!blockId) {
            console.error(`[warn] block-mutations: could not resolve second block on ${stackKey}`)
            request.body.params.arguments.block_id = `__UNRESOLVED_${stackKey}__`
            request.body.params.arguments.blockId = `__UNRESOLVED_${stackKey}__`
            return request
          }
          request.body.params.arguments.block_id = blockId
          request.body.params.arguments.blockId = blockId
          return request
        },
        localTrace: { kind: 'block.editText', documentId: blockDocId },
      },
    ))
    probes.push(await probe(
      'mcp-read-blocks-after-edit',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: legacyGraphId, graphId: legacyGraphId,
          document_id: blockDocId, documentId: blockDocId,
          limit: 50, include_ids: true, includeIds: true, format: 'text',
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: localGraphId, graphId: localGraphId,
          document_id: blockDocId, documentId: blockDocId,
          limit: 50, include_ids: true, includeIds: true, format: 'text',
        }),
      },
      {
        repeatCount: 1,
        expectedDocument: {
          blocks: [
            { text: 'Block Mutations Probe' },
            { textIncludes: 'quick brown foxer' },
          ],
        },
      },
    ))
    probes.push(await probe(
      'mcp-delete-blocks',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete', {
          graph_id: legacyGraphId, graphId: legacyGraphId,
          type: 'blocks',
          document_id: blockDocId, documentId: blockDocId,
          block_id: '__INSERTED_BLOCK__',
          blockId: '__INSERTED_BLOCK__',
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete_blocks', {
          graph_id: localGraphId, graphId: localGraphId,
          document_id: blockDocId, documentId: blockDocId,
          block_id: '__INSERTED_BLOCK__',
          blockId: '__INSERTED_BLOCK__',
        }),
      },
      {
        repeatCount: 1,
        ignoreShape: true,
        rewriteBeforeRun: async (request, stackKey) => {
          const stack = stackKey === 'legacy' ? legacyMcpStack : localStack
          const blocksBody = mcpToolBody('read_blocks', {
            graph_id: stackKey === 'legacy' ? legacyGraphId : localGraphId,
            graphId: stackKey === 'legacy' ? legacyGraphId : localGraphId,
            document_id: blockDocId,
            documentId: blockDocId,
            limit: 50,
            include_ids: true,
            includeIds: true,
            format: 'text',
          })
          const response = await stackRequest(stack, 'POST', '/mcp', blocksBody)
          const parsed = parseMcpToolResponse(response.body)
          const blocks = parsed?.blocks ?? []
          const inserted = blocks.find((block) =>
            String(block.content ?? block.text ?? '').includes(insertContent),
          )
          const blockId = inserted?.blockId ?? inserted?.block_id
          if (!blockId) {
            console.error(`[warn] block-mutations: could not resolve inserted block on ${stackKey}`)
            request.body.params.arguments.block_id = `__UNRESOLVED_${stackKey}__`
            request.body.params.arguments.blockId = `__UNRESOLVED_${stackKey}__`
            return request
          }
          request.body.params.arguments.block_id = blockId
          request.body.params.arguments.blockId = blockId
          return request
        },
        localTrace: { kind: 'block.delete', documentId: blockDocId },
      },
    ))
    probes.push(await probe(
      'mcp-block-cleanup',
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete_document', {
          graph_id: legacyGraphId, graphId: legacyGraphId,
          document_id: blockDocId, documentId: blockDocId,
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete_document', {
          graph_id: localGraphId, graphId: localGraphId,
          document_id: blockDocId, documentId: blockDocId,
        }),
      },
      { repeatCount: 1 },
    ))
    await cleanupStackDocuments([
      { stack: legacyStack, graphId: legacyGraphId, documentId: blockDocId },
      { stack: localStack, graphId: localGraphId, documentId: blockDocId },
    ])
  }
}

let mcpFixtureSummary = null
if (includeMcpFixtures && legacyGraphId && localGraphId) {
  const fixtures = await loadParityFixtures()
  mcpFixtureSummary = { count: fixtures.length, names: fixtures.map((fixture) => fixture.name) }
  for (const fixture of fixtures) {
    const fixtureDocId = `parity-fixture-${fixture.name}-${Date.now()}`
    const seedArgs = (graph) => ({
      graph_id: graph,
      graphId: graph,
      document_id: fixtureDocId,
      documentId: fixtureDocId,
      content: fixture.seed.content,
      format: fixture.seed.format ?? 'markdown',
      await_durable: true,
      awaitDurable: true,
    })
    probes.push(await probe(
      `fixture/${fixture.name}/seed`,
      legacyMcpStack,
      localStack,
      { method: 'POST', route: '/mcp', body: mcpToolBody('write_document', seedArgs(legacyGraphId)) },
      { method: 'POST', route: '/mcp', body: mcpToolBody('write_document', seedArgs(localGraphId)) },
      { repeatCount: 1 },
    ))

    const opTool = fixture.op.tool
    const legacyTool = fixture.op.legacy_tool ?? opTool
    const legacyExtra = fixture.op.legacy_extra ?? {}
    const buildOpArgs = (graph, extras = {}) => {
      const out = {
        graph_id: graph,
        graphId: graph,
        document_id: fixtureDocId,
        documentId: fixtureDocId,
        ...extras,
      }
      for (const [key, value] of Object.entries(fixture.op.arguments)) {
        if (value && typeof value === 'object' && 'resolve' in value) {
          out[key] = `__RESOLVE__${value.resolve}`
          if (key === 'block_id') out.blockId = out[key]
        } else {
          out[key] = value
          if (key === 'block_id') out.blockId = value
        }
      }
      return out
    }

    const opTrace =
      opTool === 'insert_blocks' ? { kind: 'block.insert', documentId: fixtureDocId }
        : opTool === 'edit_block_text' ? { kind: 'block.editText', documentId: fixtureDocId }
        : opTool === 'update_blocks' ? { kind: 'block.update', documentId: fixtureDocId }
        : opTool === 'delete_blocks' ? { kind: 'block.delete', documentId: fixtureDocId }
        : null

    probes.push(await probe(
      `fixture/${fixture.name}/op`,
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody(legacyTool, buildOpArgs(legacyGraphId, legacyExtra)),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody(opTool, buildOpArgs(localGraphId)),
      },
      {
        repeatCount: 1,
        ignoreShape: legacyTool !== opTool,
        rewriteBeforeRun: makeFixtureRewrite(fixture, fixtureDocId, legacyStack, localStack, legacyGraphId, localGraphId),
        ...(opTrace ? { localTrace: opTrace } : {}),
      },
    ))

    const expectedDocument = {
      blocks: fixture.expected.blocks,
      ...(fixture.expected.exactLength ? { exactLength: true } : {}),
    }
    probes.push(await probe(
      `fixture/${fixture.name}/verify`,
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: legacyGraphId, graphId: legacyGraphId,
          document_id: fixtureDocId, documentId: fixtureDocId,
          limit: 50, include_ids: true, includeIds: true, format: 'text',
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('read_blocks', {
          graph_id: localGraphId, graphId: localGraphId,
          document_id: fixtureDocId, documentId: fixtureDocId,
          limit: 50, include_ids: true, includeIds: true, format: 'text',
        }),
      },
      {
        repeatCount: 1,
        expectedDocument,
      },
    ))

    probes.push(await probe(
      `fixture/${fixture.name}/cleanup`,
      legacyMcpStack,
      localStack,
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete_document', {
          graph_id: legacyGraphId, graphId: legacyGraphId,
          document_id: fixtureDocId, documentId: fixtureDocId,
        }),
      },
      {
        method: 'POST',
        route: '/mcp',
        body: mcpToolBody('delete_document', {
          graph_id: localGraphId, graphId: localGraphId,
          document_id: fixtureDocId, documentId: fixtureDocId,
        }),
      },
      { repeatCount: 1 },
    ))
    await cleanupStackDocuments([
      { stack: legacyStack, graphId: legacyGraphId, documentId: fixtureDocId },
      { stack: localStack, graphId: localGraphId, documentId: fixtureDocId },
    ])
  }
}

function makeFixtureRewrite(fixture, fixtureDocId, legacyStack, localStack, legacyGraphId, localGraphId) {
  return async (request, stackKey) => {
    const stack = stackKey === 'legacy' ? legacyMcpStack : localStack
    const graph = stackKey === 'legacy' ? legacyGraphId : localGraphId
    const args = request.body.params.arguments
    const tokens = []
    for (const key of Object.keys(args)) {
      if (typeof args[key] === 'string' && args[key].startsWith('__RESOLVE__')) {
        tokens.push({ key, hint: args[key].slice('__RESOLVE__'.length) })
      }
    }
    if (tokens.length === 0) return request
    const blocksBody = mcpToolBody('read_blocks', {
      graph_id: graph, graphId: graph,
      document_id: fixtureDocId, documentId: fixtureDocId,
      limit: 50, include_ids: true, includeIds: true, format: 'text',
    })
    const response = await stackRequest(stack, 'POST', '/mcp', blocksBody)
    const parsed = parseMcpToolResponse(response.body)
    const blocks = parsed?.blocks ?? []
    for (const { key, hint } of tokens) {
      const resolved = resolveBlockIdHint(blocks, hint)
      if (!resolved) {
        console.error(`[warn] fixture ${fixture.name}: could not resolve ${hint} on ${stackKey}`)
        args[key] = `__UNRESOLVED_${stackKey}__`
        if (key === 'block_id') args.blockId = args[key]
        if (key === 'blockId') args.block_id = args[key]
        continue
      }
      args[key] = resolved
      if (key === 'block_id') args.blockId = resolved
      if (key === 'blockId') args.block_id = resolved
    }
    return request
  }
}

function resolveBlockIdHint(blocks, hint) {
  if (hint.startsWith('match:')) {
    const needle = hint.slice('match:'.length).toLowerCase()
    const found = blocks.find((block) =>
      String(block.content ?? block.text ?? '').toLowerCase().includes(needle),
    )
    return found?.blockId ?? found?.block_id ?? null
  }
  return null
}

const report = {
  ok: true,
  generatedAt: new Date().toISOString(),
  options: {
    repeatCount,
    includeSemantic,
    includeMutations,
    includeWriteFidelity,
    includeMcpFidelity,
    includeMcpFixtures,
  },
  stacks: {
    legacy: {
      base: legacyStack.base,
      namespace: legacyNamespace,
      service: legacyService,
      userId: legacyUserId,
      graphId: legacyGraphId,
      documentId: legacyDocumentId,
      spawnedPortForward: Boolean(spawnedPortForward),
    },
    local: {
      base: localStack.base,
      manifestPath,
      graphId: localGraphId,
      documentId: localDocumentId,
    },
  },
  openapi: {
    legacy: {
      pathCount: legacyPaths.size,
      operationCount: operationCount(legacyOpenapi.body),
    },
    local: {
      pathCount: localPaths.size,
      operationCount: operationCount(localOpenapi.body),
    },
    sharedPathCount: sharedPaths.length,
    sharedPaths,
    localOnlyPaths: [...localPaths].filter((routePath) => !legacyPaths.has(routePath)).sort(),
    legacyOnlyPathCount: [...legacyPaths].filter((routePath) => !localPaths.has(routePath)).length,
  },
  probes: probes.map(scrubProbe),
  mcpFixtures: mcpFixtureSummary,
}

if (strict) {
  const bad = report.probes.filter((item) =>
    (!item.expectations?.missingDocument && (
      item.legacy.resolvedStatus >= 400
      || item.local.resolvedStatus >= 400
    ))
    || item.legacy.job?.timedOut
    || item.local.job?.timedOut
    || item.differences.length > 0
  )
  if (bad.length > 0) {
    console.error(JSON.stringify({ ok: false, failedProbes: bad.map((item) => item.name), report }, null, 2))
    stopLegacyPortForward()
    process.exit(1)
  }
}

if (markdown) {
  console.log(renderMarkdown(report))
} else {
  console.log(JSON.stringify(report, null, 2))
}
stopLegacyPortForward()

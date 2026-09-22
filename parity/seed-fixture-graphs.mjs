#!/usr/bin/env node
import fs from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { Buffer } from 'node:buffer'
import { spawn, execFile } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const args = new Set(process.argv.slice(2))
const localOnly = args.has('--local-only')
const legacyOnly = args.has('--legacy-only')
const markdown = args.has('--markdown')
const skipSemanticRefresh = args.has('--skip-semantic-refresh')
const refreshSemanticIndex = args.has('--refresh-semantic-index') && !skipSemanticRefresh
const seedBlockMutations = args.has('--seed-block-mutations')

const fixtureDir = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures', 'semantic-corpus')
const manifest = JSON.parse(await fs.readFile(path.join(fixtureDir, 'manifest.json'), 'utf8'))
const fixtureGraphId = process.env.SOPHIA_FIXTURE_GRAPH_ID ?? manifest.graphId
const fixtureTitle = process.env.SOPHIA_FIXTURE_TITLE ?? manifest.title
const fixtureDescription = process.env.SOPHIA_FIXTURE_DESCRIPTION ?? manifest.description

const legacyPort = Number(process.env.SOPHIA_LEGACY_LOCAL_PORT ?? 18080)
const legacyBase = process.env.SOPHIA_LEGACY_BASE ?? `http://127.0.0.1:${legacyPort}`
const legacyNamespace = process.env.SOPHIA_LEGACY_NAMESPACE ?? 'prod'
const legacyService = process.env.SOPHIA_LEGACY_SERVICE ?? 'mnemosyne-api'
const legacyServicePort = process.env.SOPHIA_LEGACY_SERVICE_PORT ?? '80'
const legacyUserId = process.env.SOPHIA_LEGACY_USER_ID ?? 'vera'
const autoPortForward = process.env.SOPHIA_LEGACY_PORT_FORWARD !== 'false'
const jobWaitMs = Number(process.env.SOPHIA_SEED_JOB_WAIT_MS ?? 30_000)
const jobPollMs = Number(process.env.SOPHIA_SEED_JOB_POLL_MS ?? 500)
const requestTimeoutMs = Number(process.env.SOPHIA_SEED_REQUEST_TIMEOUT_MS ?? 120_000)

const manifestPath =
  process.env.SOPHIA_LOOPBACK_MANIFEST ??
  path.join(
    os.homedir(),
    'Library/Application Support/dev.sophia.garden/profiles/default/loopback.json',
  )

let spawnedPortForward = null

process.on('exit', () => {
  if (spawnedPortForward) spawnedPortForward.kill('SIGTERM')
})
process.on('SIGINT', () => {
  if (spawnedPortForward) spawnedPortForward.kill('SIGTERM')
  process.exit(130)
})

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
  const controller = new AbortController()
  const timeout = setTimeout(() => controller.abort(), requestTimeoutMs)
  try {
    const response = await fetch(url, { ...init, signal: init.signal ?? controller.signal })
    const text = await response.text()
    let body = null
    try {
      body = text ? JSON.parse(text) : null
    } catch {
      body = text
    }
    return {
      ok: response.ok,
      status: response.status,
      url,
      body,
    }
  } finally {
    clearTimeout(timeout)
  }
}

function stackRequest(stack, method, route, body) {
  const headers = { ...stack.headers }
  const init = { method, headers }
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json'
    init.body = JSON.stringify(body)
  }
  return fetchJson(`${stack.base}${route}`, init)
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
        return { raw: response, resolved: lastResult, job: { status: jobStatus(lastStatus.body) || 'result-ready' } }
      }
    }
    if (isTerminalJob(lastStatus.body) && (!resultPath || lastResult)) break
    await sleep(jobPollMs)
  }

  return {
    raw: response,
    resolved: lastResult?.ok ? lastResult : lastStatus,
    job: {
      status: jobStatus(lastStatus.body) || jobStatus(response.body) || 'timeout',
      timedOut: Date.now() - started >= jobWaitMs,
    },
  }
}

async function refreshLocalSemanticIndex(stack, graphId) {
  const submitted = await stackRequest(stack, 'POST', '/api/semantic/index/refresh/jobs', { graphId })
  if (!submitted.ok) {
    return {
      ok: false,
      status: submitted.status,
      body: submitted.body,
      error: `semantic refresh job submit failed with ${submitted.status}`,
    }
  }

  const jobId = submitted.body?.job_id ?? submitted.body?.jobId
  if (!jobId) {
    return {
      ok: false,
      status: submitted.status,
      body: submitted.body,
      error: 'semantic refresh job submit response did not include a job id',
    }
  }

  const statusPath = `/api/semantic/index/refresh/jobs/${encodeURIComponent(jobId)}`
  const started = Date.now()
  let latest = submitted

  while (Date.now() - started < jobWaitMs) {
    latest = await stackRequest(stack, 'GET', statusPath)
    const status = jobStatus(latest.body)
    if (isTerminalJob(latest.body)) {
      const ok = latest.ok && ['succeeded', 'success', 'complete', 'completed'].includes(status)
      return {
        ok,
        status: latest.status,
        body: latest.body,
        submitted: submitted.body,
        job: { jobId, status },
      }
    }
    await sleep(jobPollMs)
  }

  return {
    ok: false,
    status: latest.status,
    body: latest.body,
    submitted: submitted.body,
    job: {
      jobId,
      status: jobStatus(latest.body) || jobStatus(submitted.body) || 'timeout',
      timedOut: true,
    },
  }
}

function normalizeGraphInfo(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null
  const graphId = String(value.graph_id ?? value.graphId ?? value.id ?? '').trim()
  if (!graphId) return null
  return {
    graphId,
    title: String(value.title ?? value.name ?? graphId),
  }
}

function normalizeGraphList(value) {
  const rows = Array.isArray(value)
    ? value
    : Array.isArray(value?.graphs)
      ? value.graphs
      : Array.isArray(value?.data)
        ? value.data
        : []
  return rows.map(normalizeGraphInfo).filter(Boolean)
}

function block(id, type, content, order, extra = {}) {
  return {
    id,
    type,
    content,
    parentId: null,
    order,
    level: null,
    checked: null,
    language: null,
    marks: [],
    ...extra,
  }
}

function flushParagraph(lines, blocks, docId) {
  if (lines.length === 0) return
  blocks.push(block(`${docId}-b${blocks.length + 1}`, 'paragraph', lines.join(' ').replace(/\s+/g, ' ').trim(), blocks.length))
  lines.length = 0
}

function parseMarkdownToBlocks(docId, markdownText) {
  const lines = markdownText.replace(/\r\n/g, '\n').split('\n')
  const blocks = []
  const paragraph = []
  let codeFence = null
  let codeLines = []

  for (const rawLine of lines) {
    const line = rawLine.trimEnd()
    const fenceMatch = line.match(/^```(\w+)?\s*$/)
    if (fenceMatch) {
      if (codeFence) {
        blocks.push(block(`${docId}-b${blocks.length + 1}`, 'code', codeLines.join('\n'), blocks.length, {
          language: codeFence === true ? null : codeFence,
        }))
        codeFence = null
        codeLines = []
      } else {
        flushParagraph(paragraph, blocks, docId)
        codeFence = fenceMatch[1] || true
      }
      continue
    }

    if (codeFence) {
      codeLines.push(rawLine)
      continue
    }

    if (!line.trim()) {
      flushParagraph(paragraph, blocks, docId)
      continue
    }

    const heading = line.match(/^(#{1,6})\s+(.+)$/)
    if (heading) {
      flushParagraph(paragraph, blocks, docId)
      blocks.push(block(`${docId}-b${blocks.length + 1}`, 'heading', heading[2].trim(), blocks.length, {
        level: heading[1].length,
      }))
      continue
    }

    const todo = line.match(/^[-*]\s+\[( |x|X)]\s+(.+)$/)
    if (todo) {
      flushParagraph(paragraph, blocks, docId)
      blocks.push(block(`${docId}-b${blocks.length + 1}`, 'todo', todo[2].trim(), blocks.length, {
        checked: todo[1].toLowerCase() === 'x',
      }))
      continue
    }

    const bullet = line.match(/^[-*]\s+(.+)$/)
    if (bullet) {
      flushParagraph(paragraph, blocks, docId)
      blocks.push(block(`${docId}-b${blocks.length + 1}`, 'bullet', bullet[1].trim(), blocks.length))
      continue
    }

    const numbered = line.match(/^\d+\.\s+(.+)$/)
    if (numbered) {
      flushParagraph(paragraph, blocks, docId)
      blocks.push(block(`${docId}-b${blocks.length + 1}`, 'numbered', numbered[1].trim(), blocks.length))
      continue
    }

    paragraph.push(line.trim())
  }

  flushParagraph(paragraph, blocks, docId)
  if (codeFence && codeLines.length > 0) {
    blocks.push(block(`${docId}-b${blocks.length + 1}`, 'code', codeLines.join('\n'), blocks.length, {
      language: codeFence === true ? null : codeFence,
    }))
  }
  return blocks
}

async function readCorpus() {
  return Promise.all(manifest.documents.map(async (doc) => {
    const source = await fs.readFile(path.join(fixtureDir, doc.file), 'utf8')
    return {
      id: doc.id,
      title: doc.title,
      file: doc.file,
      source,
      blocks: parseMarkdownToBlocks(doc.id, source),
    }
  }))
}

async function ensureLegacyGraph(stack) {
  const catalog = await stackRequest(stack, 'GET', '/graphs/catalog')
  const existing = normalizeGraphList(catalog.body).find((graph) => graph.graphId === fixtureGraphId)
  if (existing) return { graphId: existing.graphId, created: false }

  const response = await stackRequest(stack, 'POST', '/graphs', {
    graph_id: fixtureGraphId,
    title: fixtureTitle,
    description: fixtureDescription,
  })
  const resolved = await resolveMaybeJob(stack, response)
  if (resolved.resolved.status >= 400) {
    throw new Error(`legacy graph create failed: ${resolved.resolved.status} ${JSON.stringify(resolved.resolved.body)}`)
  }
  return { graphId: fixtureGraphId, created: true }
}

async function seedLegacy(stack, corpus) {
  await ensureLegacyPortForward()
  const graph = await ensureLegacyGraph(stack)
  const documents = []
  for (const doc of corpus) {
    const response = await stackRequest(
      stack,
      'PUT',
      `/documents/${encodeURIComponent(graph.graphId)}/${encodeURIComponent(doc.id)}`,
      {
        title: doc.title,
        parentId: null,
        blocks: doc.blocks,
      },
    )
    const resolved = await resolveMaybeJob(stack, response)
    if (resolved.resolved.status >= 400) {
      throw new Error(`legacy document seed failed for ${doc.id}: ${resolved.resolved.status} ${JSON.stringify(resolved.resolved.body)}`)
    }
    documents.push({ id: doc.id, title: doc.title, status: resolved.resolved.status, blocks: doc.blocks.length })
  }
  return { graphId: graph.graphId, created: graph.created, documents }
}

async function readLoopbackManifest() {
  return JSON.parse(await fs.readFile(manifestPath, 'utf8'))
}

async function ensureLocalGraph(stack) {
  const listed = await stackRequest(stack, 'GET', '/graphs?wait_ms=1')
  const existing = normalizeGraphList(listed.body).find((graph) => graph.graphId === fixtureGraphId)
  if (existing) return { graphId: existing.graphId, created: false }

  const created = await stackRequest(stack, 'POST', '/graphs', {
    graph_id: fixtureGraphId,
    title: fixtureTitle,
    description: fixtureDescription,
  })
  const resolved = await resolveMaybeJob(stack, created)
  if (resolved.resolved.status >= 400) {
    throw new Error(`local graph create failed: ${resolved.resolved.status} ${JSON.stringify(resolved.resolved.body)}`)
  }
  return { graphId: fixtureGraphId, created: true }
}

async function runLocalCrdtOperation(stack, operation) {
  const response = await stackRequest(stack, 'POST', '/api/crdt/operations', operation)
  if (response.status >= 400) {
    throw new Error(`local CRDT operation failed: ${response.status} ${JSON.stringify(response.body)}`)
  }
  return response.body
}

async function seedLocal(stack, corpus) {
  await waitForHealth(stack.base, 10_000)
  const graph = await ensureLocalGraph(stack)
  const documents = []
  for (const doc of corpus) {
    const response = await stackRequest(
      stack,
      'PUT',
      `/documents/${encodeURIComponent(graph.graphId)}/${encodeURIComponent(doc.id)}`,
      {
        title: doc.title,
        parentId: null,
        blocks: doc.blocks,
      },
    )
    if (response.status >= 400) {
      throw new Error(`local document seed failed for ${doc.id}: ${response.status} ${JSON.stringify(response.body)}`)
    }
    documents.push({ id: doc.id, title: doc.title, blocks: doc.blocks.length, status: response.status })
  }

  await runLocalCrdtOperation(stack, {
    kind: 'crdt.flush',
    graphId: graph.graphId,
    documentId: null,
    payload: {},
  })

  let semantic = { skipped: true }
  if (refreshSemanticIndex) {
    try {
      semantic = await refreshLocalSemanticIndex(stack, graph.graphId)
    } catch (error) {
      semantic = { ok: false, error: error.message }
    }
  }

  return { graphId: graph.graphId, created: graph.created, documents, semantic }
}

function renderMarkdown(report) {
  const lines = []
  lines.push('# Semantic Fixture Seed')
  lines.push('')
  lines.push(`- Corpus: \`${manifest.corpusId}\``)
  if (report.legacy) {
    lines.push(`- Legacy graph: \`${report.legacy.graphId}\` (${report.legacy.created ? 'created' : 'reused'}), docs ${report.legacy.documents.length}`)
  }
  if (report.local) {
    lines.push(`- Local graph: \`${report.local.graphId}\` (${report.local.created ? 'created' : 'reused'}), docs ${report.local.documents.length}`)
    const semanticStatus = report.local.semantic?.skipped ? 'skipped' : (report.local.semantic?.status ?? 'n/a')
    lines.push(`- Local semantic refresh: ${report.local.semantic?.ok ? 'ok' : 'not ok'} (${semanticStatus})`)
  }
  lines.push('')
  lines.push('| Document | Blocks |')
  lines.push('| --- | ---: |')
  for (const doc of report.corpus) {
    lines.push(`| ${doc.title} | ${doc.blocks} |`)
  }
  lines.push('')
  return lines.join('\n')
}

if (localOnly && legacyOnly) {
  throw new Error('Use at most one of --local-only or --legacy-only')
}

const corpus = await readCorpus()
const report = {
  ok: true,
  generatedAt: new Date().toISOString(),
  corpus: corpus.map((doc) => ({ id: doc.id, title: doc.title, file: doc.file, blocks: doc.blocks.length })),
  legacy: null,
  local: null,
}

if (!localOnly) {
  const legacySecret = await getLegacyInternalSecret()
  const legacyStack = {
    base: legacyBase,
    headers: {
      'X-Internal-Service': legacySecret,
      'X-User-ID': legacyUserId,
    },
  }
  report.legacy = await seedLegacy(legacyStack, corpus)
}

if (!legacyOnly) {
  const loopback = await readLoopbackManifest()
  const localStack = {
    base: loopback.apiUrl,
    headers: {
      Authorization: `Bearer ${loopback.token}`,
    },
  }
  report.local = await seedLocal(localStack, corpus)
}

if (seedBlockMutations) {
  const blockBaselineDocId = process.env.SOPHIA_BLOCK_BASELINE_DOC_ID ?? 'block-mutated-baseline'
  const blockBaseline = {
    documentId: blockBaselineDocId,
    legacy: null,
    local: null,
  }
  if (report.legacy) {
    const legacySecret = await getLegacyInternalSecret()
    const legacyStack = {
      base: legacyBase,
      headers: {
        'X-Internal-Service': legacySecret,
        'X-User-ID': legacyUserId,
      },
    }
    blockBaseline.legacy = await seedBlockBaseline(legacyStack, fixtureGraphId, blockBaselineDocId, 'legacy')
  }
  if (report.local) {
    const loopback = await readLoopbackManifest()
    const localStack = {
      base: loopback.apiUrl,
      headers: {
        Authorization: `Bearer ${loopback.token}`,
      },
    }
    blockBaseline.local = await seedBlockBaseline(localStack, fixtureGraphId, blockBaselineDocId, 'local')
  }
  report.blockBaseline = blockBaseline
}

if (markdown) {
  console.log(renderMarkdown(report))
} else {
  console.log(JSON.stringify(report, null, 2))
}

async function callMcpTool(stack, name, arguments_) {
  const response = await stackRequest(stack, 'POST', '/mcp', {
    jsonrpc: '2.0',
    id: `seed-${name}-${Date.now()}`,
    method: 'tools/call',
    params: { name, arguments: arguments_ },
  })
  if (response.status >= 400) {
    throw new Error(`MCP ${name} failed: ${response.status} ${JSON.stringify(response.body)}`)
  }
  const text = response.body?.result?.content?.[0]?.text
  return text ? JSON.parse(text) : response.body?.result?.structuredContent ?? response.body?.result
}

async function seedBlockBaseline(stack, graphId, docId, label) {
  await callMcpTool(stack, 'write_document', {
    graph_id: graphId,
    graphId,
    document_id: docId,
    documentId: docId,
    content: `# Block Baseline (${label})\n\nThe quick brown fox.\n\nTrailing paragraph for block baseline.`,
    format: 'markdown',
    await_durable: true,
    awaitDurable: true,
  })
  const blocks = await callMcpTool(stack, 'read_blocks', {
    graph_id: graphId,
    graphId,
    document_id: docId,
    documentId: docId,
    limit: 50,
    include_ids: true,
    includeIds: true,
    format: 'text',
  })
  const second = (blocks?.blocks ?? []).find((block) =>
    /quick brown fox/i.test(String(block.content ?? block.text ?? '')),
  )
  const blockId = second?.blockId ?? second?.block_id
  if (!blockId) throw new Error(`block baseline seed: paragraph block not found on ${label}`)
  await callMcpTool(stack, 'edit_block_text', {
    graph_id: graphId,
    graphId,
    document_id: docId,
    documentId: docId,
    block_id: blockId,
    blockId,
    operations: [{ type: 'insert', offset: 16, text: 'er' }],
  })
  return { documentId: docId, paragraphBlockId: blockId }
}

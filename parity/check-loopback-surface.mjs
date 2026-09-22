#!/usr/bin/env node
import fs from 'node:fs/promises'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { gzipSync } from 'node:zlib'

const here = path.dirname(new URL(import.meta.url).pathname)
const expected = JSON.parse(
  await fs.readFile(path.join(here, 'local-loopback-surface.json'), 'utf8'),
)
const mcpSchemaSnapshot = JSON.parse(
  await fs.readFile(path.join(here, 'mcp-tool-schema-snapshot.json'), 'utf8'),
)
const performanceBudgetConfig = JSON.parse(
  await fs.readFile(path.join(here, 'performance-budgets.json'), 'utf8'),
)

const manifestPath =
  process.env.SOPHIA_LOOPBACK_MANIFEST ??
  path.join(
    os.homedir(),
    'Library/Application Support/dev.sophia.garden/profiles/default/loopback.json',
  )

const manifest = JSON.parse(await fs.readFile(manifestPath, 'utf8'))
const authHeaders = { Authorization: `Bearer ${manifest.token}` }
const requestTimeoutMs = Number(process.env.SOPHIA_PARITY_REQUEST_TIMEOUT_MS ?? 60_000)
const progressEnabled = process.env.SOPHIA_PARITY_PROGRESS === '1'
// Cell profile: headless gardend cells now implement all CRDT op kinds
// (imports, comments, batch, uploadIngest via the parser pool), so most
// sections run under SOPHIA_PARITY_PROFILE=cell. A granular skip set covers
// the remainder: sections that pass desktop-local file paths to MCP tools
// (impossible against a remote cell) or exercise html/epub parser-pool
// support that is not yet tuned. Override with a comma-separated
// SOPHIA_PARITY_CELL_SKIP to widen/narrow the set. Skipped sections are
// reported in the summary.
const CELL_PROFILE = process.env.SOPHIA_PARITY_PROFILE === 'cell'
// Remaining default skips: desktop-local file_path semantics (mcp* tempdir
// tools) and html/epub/pdf upload formats pending parser-pool tuning —
// override per-run with SOPHIA_PARITY_CELL_SKIP. (artifactUpload's fileType
// envelope bug was fixed in upload_ingest_ops.rs; it runs by default now.)
const DEFAULT_CELL_SKIP = 'artifactFormatUpload,artifactPdfAccurate,mcpArtifactAdapters,mcpDocumentEditability,mcpIngestArtifact,artifactRouteImportConvert'
const CELL_SKIP_SECTIONS = new Set(
  (process.env.SOPHIA_PARITY_CELL_SKIP ?? DEFAULT_CELL_SKIP)
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean),
)
const cellSkip = (name) => CELL_PROFILE && CELL_SKIP_SECTIONS.has(name)
const cellSkippedSections = []
function skipForCellProfile(section) {
  cellSkippedSections.push(section)
  return { checked: false, skipped: 'cell-profile' }
}

function methodLabel(init = {}) {
  return init.method ?? 'GET'
}

async function fetchWithTimeout(url, init = {}) {
  if (!Number.isFinite(requestTimeoutMs) || requestTimeoutMs <= 0) {
    return await fetch(url, init)
  }
  if (init.signal) {
    return await fetch(url, init)
  }
  const controller = new AbortController()
  const timeout = setTimeout(() => controller.abort(), requestTimeoutMs)
  try {
    return await fetch(url, { ...init, signal: controller.signal })
  } catch (error) {
    if (error?.name === 'AbortError') {
      throw new Error(`${methodLabel(init)} ${url} timed out after ${requestTimeoutMs}ms`)
    }
    throw error
  } finally {
    clearTimeout(timeout)
  }
}

async function fetchJson(url, init = {}) {
  const { body } = await fetchJsonResponse(url, init)
  return body
}

async function fetchJsonResponse(url, init = {}) {
  const response = await fetchWithTimeout(url, init)
  const text = await response.text()
  let body
  try {
    body = text ? JSON.parse(text) : null
  } catch {
    body = text
  }
  if (!response.ok) {
    throw new Error(`${methodLabel(init)} ${url} -> ${response.status}: ${text}`)
  }
  return { status: response.status, body }
}

async function fetchJsonAnyStatus(url, init = {}) {
  const response = await fetchWithTimeout(url, init)
  const text = await response.text()
  let body
  try {
    body = text ? JSON.parse(text) : null
  } catch {
    body = text
  }
  return { status: response.status, body, ok: response.ok }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

async function fetchJobResultEventually(url, init = {}, attempts = 120, delayMs = 500) {
  let last
  for (let index = 0; index < attempts; index += 1) {
    last = await fetchJsonAnyStatus(url, init)
    if (last.ok) return last.body
    if (last.status !== 404) {
      throw new Error(`${methodLabel(init)} ${url} -> ${last.status}: ${JSON.stringify(last.body)}`)
    }
    await sleep(delayMs)
  }
  throw new Error(`${methodLabel(init)} ${url} did not produce a job result after ${attempts} attempts: ${JSON.stringify(last?.body)}`)
}

const terminalJobStatuses = new Set(['succeeded', 'failed', 'cancelled'])

async function fetchJobStatusEventually(url, init = {}, attempts = 120, delayMs = 500) {
  let last
  for (let index = 0; index < attempts; index += 1) {
    last = await fetchJsonAnyStatus(url, init)
    if (last.ok) {
      const status = String(last.body?.status ?? '')
      if (terminalJobStatuses.has(status)) return last.body
      if (!['queued', 'running'].includes(status)) {
        throw new Error(`${methodLabel(init)} ${url} returned unknown job status: ${JSON.stringify(last.body)}`)
      }
    } else if (last.status !== 404) {
      throw new Error(`${methodLabel(init)} ${url} -> ${last.status}: ${JSON.stringify(last.body)}`)
    }
    await sleep(delayMs)
  }
  throw new Error(`${methodLabel(init)} ${url} did not reach a terminal job status after ${attempts} attempts: ${JSON.stringify(last?.body)}`)
}

async function fetchBytesResponse(url, init = {}) {
  const response = await fetchWithTimeout(url, init)
  const bytes = new Uint8Array(await response.arrayBuffer())
  if (!response.ok) {
    const preview = new TextDecoder().decode(bytes.slice(0, 500))
    throw new Error(`${methodLabel(init)} ${url} -> ${response.status}: ${preview}`)
  }
  return { status: response.status, bytes, headers: response.headers }
}

async function fetchTextResponse(url, init = {}) {
  const response = await fetchWithTimeout(url, init)
  const text = await response.text()
  if (!response.ok) {
    throw new Error(`${methodLabel(init)} ${url} -> ${response.status}: ${text.slice(0, 500)}`)
  }
  return { status: response.status, text, headers: response.headers }
}

async function withLocalHttpFixture(routes, action) {
  const server = http.createServer((request, response) => {
    const handler = routes[request.url ?? '/']
    if (!handler) {
      response.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' })
      response.end('not found')
      return
    }
    handler(request, response)
  })
  await new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => resolve())
  })
  try {
    const address = server.address()
    if (!address || typeof address === 'string') throw new Error('Local fixture did not expose a TCP port')
    return await action(`http://127.0.0.1:${address.port}`)
  } finally {
    await new Promise((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()))
    })
  }
}

function roundMs(value) {
  return Math.round(value * 10) / 10
}

const operationTimings = {}

async function timedStep(name, action) {
  const started = performance.now()
  if (progressEnabled) console.error(`[parity] start ${name}`)
  try {
    return await action()
  } finally {
    operationTimings[name] = roundMs(performance.now() - started)
    if (progressEnabled) console.error(`[parity] done ${name} ${operationTimings[name]}ms`)
  }
}

function median(values) {
  if (values.length === 0) return null
  const sorted = [...values].sort((left, right) => left - right)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 === 0
    ? roundMs(((sorted[middle - 1] ?? 0) + (sorted[middle] ?? 0)) / 2)
    : sorted[middle] ?? null
}

function numberValue(value) {
  const number = Number(value)
  return Number.isFinite(number) ? number : null
}

function assertApprox(label, value, expected, tolerance = 0.01) {
  const actual = numberValue(value)
  if (actual === null || Math.abs(actual - expected) > tolerance) {
    throw new Error(`${label} expected ${expected} +/- ${tolerance}, got ${value}`)
  }
}

function summarizeCrdtTraces(traces) {
  const byKind = {}
  for (const trace of traces) {
    const kind = String(trace.kind ?? 'unknown')
    const totalMs = numberValue(trace.phases?.rustEnqueueToCompleteMs)
    const jsTotalMs = numberValue(trace.phases?.jsTotalMs)
    const existing = byKind[kind] ?? {
      samples: 0,
      totals: [],
      jsTotals: [],
      maxTotalMs: null,
      lastOperationId: null,
    }
    existing.samples += 1
    existing.lastOperationId = trace.operationId ?? existing.lastOperationId
    if (totalMs !== null) {
      existing.totals.push(totalMs)
      existing.maxTotalMs = existing.maxTotalMs === null ? totalMs : Math.max(existing.maxTotalMs, totalMs)
    }
    if (jsTotalMs !== null) existing.jsTotals.push(jsTotalMs)
    byKind[kind] = existing
  }

  return Object.fromEntries(Object.entries(byKind).map(([kind, summary]) => [
    kind,
    {
      samples: summary.samples,
      medianTotalMs: median(summary.totals),
      maxTotalMs: summary.maxTotalMs,
      medianJsTotalMs: median(summary.jsTotals),
      lastOperationId: summary.lastOperationId,
    },
  ]))
}

function compareTimings(leftName, rightName) {
  const leftMs = operationTimings[leftName]
  const rightMs = operationTimings[rightName]
  if (typeof leftMs !== 'number' || typeof rightMs !== 'number') return null
  return {
    left: leftName,
    right: rightName,
    leftMs,
    rightMs,
    deltaMs: roundMs(leftMs - rightMs),
    ratio: rightMs > 0 ? roundMs(leftMs / rightMs) : null,
  }
}

function sortedStringArray(values) {
  return [...new Set((values ?? []).map(String))].sort()
}

function sameStringArray(left, right) {
  if (left.length !== right.length) return false
  return left.every((value, index) => value === right[index])
}

function evaluatePerformanceBudgets() {
  return (performanceBudgetConfig.budgets ?? []).map((budget) => {
    const observedMs = operationTimings[budget.operation]
    if (typeof observedMs !== 'number') {
      return {
        ...budget,
        observedMs: null,
        status: 'not_observed',
      }
    }
    const passed = observedMs <= budget.maxMs
    return {
      ...budget,
      observedMs,
      deltaMs: roundMs(observedMs - budget.maxMs),
      status: passed ? 'within_budget' : budget.enforced ? 'failed' : 'over_budget_unenforced',
    }
  })
}

const health = await fetchJson(`${manifest.apiUrl}/health`)
if (health.status !== 'ok' || !health.redis) {
  throw new Error('/health did not return hosted-shaped status/redis envelope')
}

const loopbackManifest = await fetchJson(`${manifest.apiUrl}/manifest`, { headers: authHeaders })
if (loopbackManifest.openapiUrl !== `${manifest.apiUrl}/openapi.json`) {
  throw new Error('loopback manifest did not advertise the expected openapiUrl')
}
const requiredLoopbackScopes = [
  'loopback.manifest.read',
  'graphs.read',
  'graphs.write',
  'documents.read',
  'documents.write.crdt',
  'documents.delete.crdt',
  'workspace.write.crdt',
  'workspace.delete.crdt',
  'artifacts.ingest',
  'rdf.query',
  'rdf.update',
  'search.semantic.read',
  'semantic.index.cancel',
  'mcp.tools.call',
]
if (loopbackManifest.tokenScopeMode !== 'session-all') {
  throw new Error('loopback manifest did not advertise the current session-all token scope mode')
}
if (loopbackManifest.tokenAudience !== 'tauri-runtime-compatibility') {
  throw new Error('loopback manifest did not label the session token audience')
}
if (loopbackManifest.tokenStorage !== 'plaintext-owner-only-profile-manifest') {
  throw new Error('loopback manifest did not label the session token storage posture')
}
if (
  typeof loopbackManifest.securityWarning !== 'string'
  || !loopbackManifest.securityWarning.includes('session token')
  || !loopbackManifest.securityWarning.includes('every local loopback/MCP scope')
) {
  throw new Error('loopback manifest did not include the expected session-all security warning')
}
if (
  !Array.isArray(loopbackManifest.tokenScopes)
  || !Array.isArray(loopbackManifest.scopeDetails)
  || !Array.isArray(loopbackManifest.grantProfiles)
) {
  throw new Error('loopback manifest did not expose tokenScopes, scopeDetails, and grantProfiles')
}
const tokenScopes = new Set(loopbackManifest.tokenScopes)
const capabilityScopes = new Set(loopbackManifest.capabilities ?? [])
for (const scope of requiredLoopbackScopes) {
  if (!tokenScopes.has(scope) || !capabilityScopes.has(scope)) {
    throw new Error(`loopback manifest did not advertise required scope ${scope}`)
  }
}
const scopeDetails = new Map(loopbackManifest.scopeDetails.map((detail) => [detail.scope, detail]))
for (const scope of requiredLoopbackScopes) {
  const detail = scopeDetails.get(scope)
  const expectedDefaultGrant = detail?.access === 'read'
  if (
    !detail?.category
    || !detail?.access
    || !detail?.description
    || detail.defaultGrant !== expectedDefaultGrant
  ) {
    throw new Error(`loopback manifest scope detail is incomplete for ${scope}`)
  }
}
const grantProfiles = new Map(loopbackManifest.grantProfiles.map((profile) => [profile.id, profile]))
for (const profileId of ['read-only', 'authoring', 'rdf-admin', 'session-all']) {
  if (!grantProfiles.has(profileId)) {
    throw new Error(`loopback manifest did not advertise grant profile ${profileId}`)
  }
}
const readOnlyProfile = grantProfiles.get('read-only')
if (readOnlyProfile.defaultGrant !== true || readOnlyProfile.mutating !== false) {
  throw new Error('read-only grant profile did not advertise safe defaults')
}
for (const scope of readOnlyProfile.scopes ?? []) {
  const detail = scopeDetails.get(scope)
  if (detail?.access !== 'read') {
    throw new Error(`read-only grant profile includes non-read scope ${scope}`)
  }
}
for (const profile of grantProfiles.values()) {
  if (!Array.isArray(profile.scopes) || !profile.label || !profile.description) {
    throw new Error(`grant profile ${profile.id} is incomplete`)
  }
  for (const scope of profile.scopes) {
    if (!tokenScopes.has(scope)) {
      throw new Error(`grant profile ${profile.id} references unknown scope ${scope}`)
    }
  }
  if (profile.mutating && profile.defaultGrant) {
    throw new Error(`mutating grant profile ${profile.id} must not be default-granted`)
  }
}
const crdtTimings = await fetchJson(`${manifest.apiUrl}/api/local/crdt-timings?limit=1&clear=true`, { headers: authHeaders })
if (!Array.isArray(crdtTimings.traces)) {
  throw new Error('local CRDT timing endpoint did not return a traces array')
}
const openapi = await fetchJson(`${manifest.apiUrl}/openapi.json`, { headers: authHeaders })
if (openapi.openapi !== '3.1.0' || !openapi.paths?.['/openapi.json']?.get) {
  throw new Error('GET /openapi.json returned an invalid OpenAPI document')
}
const openapiRouteKeys = new Set(Object.entries(openapi.paths ?? {}).flatMap(([routePath, pathItem]) =>
  Object.keys(pathItem)
    .filter((method) => ['get', 'post', 'put', 'patch', 'delete'].includes(method.toLowerCase()))
    .map((method) => `${method.toUpperCase()} ${routePath}`),
))
const missingOpenapiRoutes = expected.routes
  .filter((route) => String(route.status ?? '').startsWith('implemented'))
  .map((route) => `${route.method.toUpperCase()} ${route.path}`)
  .filter((key) => !openapiRouteKeys.has(key))
if (missingOpenapiRoutes.length > 0) {
  throw new Error(`OpenAPI is missing implemented routes: ${missingOpenapiRoutes.join(', ')}`)
}

const semanticModels = await fetchJson(`${manifest.apiUrl}/api/semantic/models`, { headers: authHeaders })
if (!Array.isArray(semanticModels) || !semanticModels.some((model) => model.modelId && model.dimensions)) {
  throw new Error('GET /api/semantic/models did not return model descriptors')
}
const selectedSemanticModel = semanticModels.find((model) => model.selected) ?? semanticModels[0]
const semanticConfig = await fetchJson(`${manifest.apiUrl}/api/semantic/model/config`, {
  method: 'PUT',
  headers: { ...authHeaders, 'Content-Type': 'application/json' },
  body: JSON.stringify({
    modelId: selectedSemanticModel.modelId,
    batchSize: selectedSemanticModel.effectiveBatchSize ?? selectedSemanticModel.defaultBatchSize ?? 1,
  }),
})
if (semanticConfig?.modelId !== selectedSemanticModel.modelId || typeof semanticConfig?.effectiveBatchSize !== 'number') {
  throw new Error('PUT /api/semantic/model/config did not persist the selected model config')
}
const ingestionApproaches = await fetchJson(`${manifest.apiUrl}/api/artifacts/ingestion/approaches`, { headers: authHeaders })
const doclingRuntime = await fetchJson(`${manifest.apiUrl}/api/artifacts/ingestion/docling/status`, { headers: authHeaders })
let pdfPipeline = await fetchJson(`${manifest.apiUrl}/api/artifacts/ingestion/pdf/pipeline`, { headers: authHeaders })
if (
  doclingRuntime?.runtimeId !== 'docling-python'
  || doclingRuntime?.approachId !== 'pdf.docling-accurate'
  || !['available', 'setup-required', 'unsupported'].includes(doclingRuntime?.status)
  || typeof doclingRuntime?.available !== 'boolean'
  || typeof doclingRuntime?.supported !== 'boolean'
) {
  throw new Error(`GET /api/artifacts/ingestion/docling/status returned an invalid runtime status: ${JSON.stringify(doclingRuntime)}`)
}
const pdfPipelineEngines = Array.isArray(pdfPipeline?.engines) ? pdfPipeline.engines : []
const pdfPipelineEngineIds = new Set(pdfPipelineEngines.map((engine) => engine.engineId))
for (const expectedEngineId of ['pdf.fast-text', 'pdf.docling-accurate', 'pdf.pymupdf4llm', 'pdf.ocrmypdf-tesseract', 'pdf.grobid-paper']) {
  if (!pdfPipelineEngineIds.has(expectedEngineId)) {
    throw new Error(`GET /api/artifacts/ingestion/pdf/pipeline omitted ${expectedEngineId}`)
  }
}
for (const engine of pdfPipelineEngines) {
  if (
    typeof engine.runtimeAvailable !== 'boolean'
    || typeof engine.runtimeStatus !== 'string'
    || !('runtimeReason' in engine)
    || !('fallbackEngineId' in engine)
  ) {
    throw new Error(`GET /api/artifacts/ingestion/pdf/pipeline returned an invalid engine runtime descriptor: ${JSON.stringify(engine)}`)
  }
}
const pymupdfEngine = pdfPipelineEngines.find((engine) => engine.engineId === 'pdf.pymupdf4llm')
if (
  pymupdfEngine?.implemented !== true
  || typeof pymupdfEngine?.available !== 'boolean'
  || !['available', 'setup-required', 'uv-runtime-ready', 'runtime-ready'].some((status) => String(pymupdfEngine?.status ?? pymupdfEngine?.runtimeStatus ?? '').includes(status))
) {
  throw new Error(`GET /api/artifacts/ingestion/pdf/pipeline did not expose PyMuPDF4LLM implemented adapter/runtime semantics: ${JSON.stringify(pymupdfEngine)}`)
}
if (
  pdfPipeline?.schemaVersion !== 1
  || !['auto', ...pdfPipelineEngineIds].includes(pdfPipeline?.preferredEngineId)
  || !['pdf.fast-text', 'pdf.docling-accurate', 'pdf.pymupdf4llm'].includes(pdfPipeline?.effectiveEngineId)
  || pdfPipeline?.doclingRuntimeStatus?.status !== doclingRuntime.status
  || !Array.isArray(pdfPipeline?.preferenceOptions)
) {
  throw new Error(`GET /api/artifacts/ingestion/pdf/pipeline returned an invalid pipeline status: ${JSON.stringify(pdfPipeline)}`)
}
const savedFuturePdfPipeline = await fetchJson(`${manifest.apiUrl}/api/artifacts/ingestion/pdf/pipeline`, {
  method: 'PUT',
  headers: { ...authHeaders, 'Content-Type': 'application/json' },
  body: JSON.stringify({ preferredEngineId: 'pdf.pymupdf4llm' }),
})
const expectedPymupdfEffectiveEngineId = pymupdfEngine?.available ? 'pdf.pymupdf4llm' : 'pdf.fast-text'
if (
  savedFuturePdfPipeline?.preferredEngineId !== 'pdf.pymupdf4llm'
  || savedFuturePdfPipeline?.effectiveEngineId !== expectedPymupdfEffectiveEngineId
) {
  throw new Error(`PUT /api/artifacts/ingestion/pdf/pipeline did not persist PyMuPDF4LLM preference semantics: ${JSON.stringify(savedFuturePdfPipeline)}`)
}
pdfPipeline = await fetchJson(`${manifest.apiUrl}/api/artifacts/ingestion/pdf/pipeline`, {
  method: 'PUT',
  headers: { ...authHeaders, 'Content-Type': 'application/json' },
  body: JSON.stringify({ preferredEngineId: 'auto' }),
})
const pdfFastApproach = Array.isArray(ingestionApproaches)
  ? ingestionApproaches.find((approach) => approach.approachId === 'pdf.fast-text')
  : null
const pdfAccurateApproach = Array.isArray(ingestionApproaches)
  ? ingestionApproaches.find((approach) => approach.approachId === 'pdf.docling-accurate')
  : null
const pdfPymupdfApproach = Array.isArray(ingestionApproaches)
  ? ingestionApproaches.find((approach) => approach.approachId === 'pdf.pymupdf4llm')
  : null
if (!pdfFastApproach || !pdfAccurateApproach || !pdfPymupdfApproach) {
  throw new Error('GET /api/artifacts/ingestion/approaches did not return PDF approach descriptors')
}
if (
  pdfFastApproach.status !== 'available'
  || pdfAccurateApproach.status !== doclingRuntime.status
  || pdfAccurateApproach.selectable !== doclingRuntime.available
  || pdfAccurateApproach.setupRequired !== doclingRuntime.setupRequired
  || pdfPymupdfApproach.status !== pymupdfEngine.status
  || pdfPymupdfApproach.selectable !== pymupdfEngine.available
  || pdfPymupdfApproach.setupRequired !== pymupdfEngine.setupRequired
  || pdfPymupdfApproach.supportsOcr !== false
) {
  throw new Error('PDF ingestion approach descriptors did not expose the expected fast/default, Docling, and PyMuPDF4LLM runtime-gated capabilities')
}

const graphJob = await fetchJsonResponse(`${manifest.apiUrl}/graphs`, { headers: authHeaders })
if (graphJob.status !== 202 || !graphJob.body?.job_id || !graphJob.body?.result_url) {
  throw new Error('GET /graphs did not return a hosted-shaped graph list job envelope')
}
const graphJobResult = await fetchJson(`${manifest.apiUrl}${graphJob.body.result_url}`, { headers: authHeaders })
if (!Array.isArray(graphJobResult)) {
  throw new Error('GET /graphs job result did not return a graph list')
}
const graphs = await fetchJson(`${manifest.apiUrl}/graphs?wait_ms=1`, { headers: authHeaders })
if (!Array.isArray(graphs)) {
  throw new Error('GET /graphs?wait_ms=1 did not return a synchronous graph list')
}

const toolsResponse = await fetchJson(`${manifest.apiUrl}/mcp`, {
  method: 'POST',
  headers: { ...authHeaders, 'Content-Type': 'application/json' },
  body: JSON.stringify({
    jsonrpc: '2.0',
    id: 'tools-list',
    method: 'tools/list',
    params: {},
  }),
})

const actualTools = new Set((toolsResponse.result?.tools ?? []).map((tool) => tool.name))
const actualToolSchemas = new Map((toolsResponse.result?.tools ?? []).map((tool) => [
  tool.name,
  tool.inputSchema ?? {},
]))
const actualToolScopes = new Map((toolsResponse.result?.tools ?? []).map((tool) => [
  tool.name,
  tool._meta?.['sophia.local.requiredScopes'] ?? [],
]))
const actualToolRequiredScopeModes = new Map((toolsResponse.result?.tools ?? []).map((tool) => [
  tool.name,
  tool._meta?.['sophia.local.requiredScopeMode'] ?? 'all',
]))
const actualToolOperationScopes = new Map((toolsResponse.result?.tools ?? []).map((tool) => [
  tool.name,
  tool._meta?.['sophia.local.operationScopes'],
]))
const missingTools = expected.mcpTools
  .filter((tool) => tool.status.startsWith('implemented'))
  .filter((tool) => !actualTools.has(tool.name))
  .map((tool) => tool.name)

if (missingTools.length > 0) {
  throw new Error(`Missing MCP tools: ${missingTools.join(', ')}`)
}

const mcpSchemaDiffs = []
for (const [name, snapshot] of Object.entries(mcpSchemaSnapshot.tools ?? {})) {
  const liveSchema = actualToolSchemas.get(name)
  if (!liveSchema) {
    mcpSchemaDiffs.push(`${name}: missing live schema`)
    continue
  }
  const expectedRequired = sortedStringArray(snapshot.required)
  const actualRequired = sortedStringArray(liveSchema.required)
  if (!sameStringArray(expectedRequired, actualRequired)) {
    mcpSchemaDiffs.push(`${name}: required ${JSON.stringify(actualRequired)} != ${JSON.stringify(expectedRequired)}`)
  }
  const expectedProperties = sortedStringArray(snapshot.properties)
  const actualProperties = sortedStringArray(Object.keys(liveSchema.properties ?? {}))
  if (!sameStringArray(expectedProperties, actualProperties)) {
    mcpSchemaDiffs.push(`${name}: properties ${JSON.stringify(actualProperties)} != ${JSON.stringify(expectedProperties)}`)
  }
}

if (mcpSchemaDiffs.length > 0) {
  throw new Error(`MCP tool schema snapshot drift:\n${mcpSchemaDiffs.join('\n')}`)
}

const mcpToolScopeDiffs = []
for (const tool of expected.mcpTools.filter((candidate) => candidate.status.startsWith('implemented'))) {
  const expectedScopes = sortedStringArray(tool.scopes)
  const actualScopes = sortedStringArray(actualToolScopes.get(tool.name))
  if (!sameStringArray(expectedScopes, actualScopes)) {
    mcpToolScopeDiffs.push(
      `${tool.name}: scopes ${JSON.stringify(actualScopes)} != ${JSON.stringify(expectedScopes)}`,
    )
  }
  const expectedRequiredScopeMode = tool.requiredScopeMode ?? 'all'
  const actualRequiredScopeMode = actualToolRequiredScopeModes.get(tool.name) ?? 'all'
  if (actualRequiredScopeMode !== expectedRequiredScopeMode) {
    mcpToolScopeDiffs.push(
      `${tool.name}: requiredScopeMode ${JSON.stringify(actualRequiredScopeMode)} != ${JSON.stringify(expectedRequiredScopeMode)}`,
    )
  }
  if (expectedRequiredScopeMode === 'operation-kind') {
    const operationScopes = actualToolOperationScopes.get(tool.name)
    if (!operationScopes || typeof operationScopes !== 'object' || Array.isArray(operationScopes)) {
      mcpToolScopeDiffs.push(`${tool.name}: missing operation scope map`)
    } else {
      const operationScopeUnion = sortedStringArray([...new Set(Object.values(operationScopes).flat())])
      if (!sameStringArray(operationScopeUnion, expectedScopes)) {
        mcpToolScopeDiffs.push(
          `${tool.name}: operation scope union ${JSON.stringify(operationScopeUnion)} != ${JSON.stringify(expectedScopes)}`,
        )
      }
    }
  }
}
if (mcpToolScopeDiffs.length > 0) {
  throw new Error(`MCP tool scope metadata drift:\n${mcpToolScopeDiffs.join('\n')}`)
}

async function callTool(name, arguments_ = {}) {
  const response = await fetchJson(`${manifest.apiUrl}/mcp`, {
    method: 'POST',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: `tool-${name}`,
      method: 'tools/call',
      params: { name, arguments: arguments_ },
    }),
  })
  if (response.error) {
    throw new Error(`MCP tool ${name} failed: ${JSON.stringify(response.error)}`)
  }
  const text = response.result?.content?.[0]?.text
  return text ? JSON.parse(text) : response.result?.structuredContent ?? response.result
}

function mcpJobId(envelope) {
  return typeof envelope?.job_id === 'string'
    ? envelope.job_id
    : typeof envelope?.jobId === 'string'
      ? envelope.jobId
      : ''
}

async function callToolJobResult(envelope, attempts = 120, delayMs = 500) {
  const jobId = mcpJobId(envelope)
  if (!jobId) {
    throw new Error(`MCP job envelope missing job_id: ${JSON.stringify(envelope)}`)
  }
  let last
  for (let index = 0; index < attempts; index += 1) {
    last = await callTool('get_job_status', { job_id: jobId })
    const status = String(last?.status ?? '')
    if (status === 'succeeded') {
      return await callTool('get_job_result', { job_id: jobId })
    }
    if (status === 'failed' || status === 'cancelled') {
      throw new Error(`MCP job ${jobId} ended with ${status}: ${JSON.stringify(last)}`)
    }
    if (status !== 'queued' && status !== 'running') {
      throw new Error(`MCP job ${jobId} returned unknown status: ${JSON.stringify(last)}`)
    }
    await sleep(delayMs)
  }
  throw new Error(`MCP job ${jobId} did not complete after ${attempts} attempts: ${JSON.stringify(last)}`)
}

let graphMetadata = { checked: false }
let graphSupportAliases = { checked: false }
let mcpGraphJobs = { checked: false }
const metadataGraphId = `parity-graph-metadata-${Date.now()}`
const metadataGraphTitle = 'Parity Graph Metadata'
const metadataGraphDescription = 'Created by parity:loopback graph metadata smoke.'
const createMetadataGraph = await fetchJsonResponse(`${manifest.apiUrl}/graphs`, {
  method: 'POST',
  headers: { ...authHeaders, 'Content-Type': 'application/json' },
  body: JSON.stringify({
    graphId: metadataGraphId,
    title: metadataGraphTitle,
    description: metadataGraphDescription,
  }),
})
if (createMetadataGraph.status !== 202 || !createMetadataGraph.body?.job_id) {
  throw new Error('POST /graphs did not create a hosted-shaped graph metadata job')
}
const readMetadataGraph = await timedStep('graphMetadataReadMs', () => fetchJson(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(metadataGraphId)}?wait_ms=1`,
  { headers: authHeaders },
))
if (
  readMetadataGraph.graph_id !== metadataGraphId
  || readMetadataGraph.title !== metadataGraphTitle
  || readMetadataGraph.description !== metadataGraphDescription
) {
  throw new Error('GET /graphs/{graph_id}?wait_ms=1 did not return the created graph metadata')
}

const updatedMetadataTitle = 'Parity Graph Metadata Updated'
const updatedMetadataDescription = 'Updated by parity:loopback graph metadata smoke.'
const updateMetadataGraph = await timedStep('graphMetadataUpdateMs', () => fetchJsonResponse(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(metadataGraphId)}`,
  {
    method: 'PUT',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      title: updatedMetadataTitle,
      description: updatedMetadataDescription,
    }),
  },
))
if (updateMetadataGraph.status !== 202 || !updateMetadataGraph.body?.job_id) {
  throw new Error('PUT /graphs/{graph_id} did not return a hosted-shaped graph metadata job')
}
const updatedMetadataGraph = await fetchJson(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(metadataGraphId)}?wait_ms=1`,
  { headers: authHeaders },
)
if (
  updatedMetadataGraph.title !== updatedMetadataTitle
  || updatedMetadataGraph.description !== updatedMetadataDescription
) {
  throw new Error('PUT /graphs/{graph_id} did not persist graph metadata changes')
}

const deleteMetadataGraph = await timedStep('graphMetadataDeleteMs', () => fetchJsonResponse(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(metadataGraphId)}`,
  {
    method: 'DELETE',
    headers: authHeaders,
  },
))
if (deleteMetadataGraph.status !== 202 || !deleteMetadataGraph.body?.job_id) {
  throw new Error('DELETE /graphs/{graph_id} did not return a hosted-shaped graph delete job')
}
let deletedGraphIsMissing = false
try {
  await fetchJson(`${manifest.apiUrl}/graphs/${encodeURIComponent(metadataGraphId)}?wait_ms=1`, {
    headers: authHeaders,
  })
} catch (error) {
  deletedGraphIsMissing = String(error).includes('404')
}
if (!deletedGraphIsMissing) {
  throw new Error('DELETE /graphs/{graph_id} left the graph readable')
}
graphMetadata = {
  checked: true,
  graphId: metadataGraphId,
  createJobId: createMetadataGraph.body.job_id,
  updateJobId: updateMetadataGraph.body.job_id,
  deleteJobId: deleteMetadataGraph.body.job_id,
}

const mcpGraphId = `parity-mcp-graph-${Date.now()}`
const mcpGraphTitle = 'Parity MCP Graph'
const mcpGraphDescription = 'Created through MCP graph adapter parity smoke.'
const mcpCreatedGraph = await timedStep('mcpCreateGraphMs', () => callTool('create_graph', {
  graph_id: mcpGraphId,
  title: mcpGraphTitle,
  description: mcpGraphDescription,
}))
if (
  mcpCreatedGraph.graph_id !== mcpGraphId
  || mcpCreatedGraph.title !== mcpGraphTitle
  || mcpCreatedGraph.description !== mcpGraphDescription
) {
  throw new Error('MCP create_graph returned an invalid graph metadata envelope')
}
const mcpUpdatedGraphTitle = 'Parity MCP Graph Updated'
const mcpUpdatedGraphDescription = 'Updated through MCP graph adapter parity smoke.'
const mcpUpdatedGraph = await timedStep('mcpUpdateGraphMs', () => callTool('update_graph', {
  graph_id: mcpGraphId,
  title: mcpUpdatedGraphTitle,
  description: mcpUpdatedGraphDescription,
}))
if (
  mcpUpdatedGraph.graph_id !== mcpGraphId
  || mcpUpdatedGraph.title !== mcpUpdatedGraphTitle
  || mcpUpdatedGraph.description !== mcpUpdatedGraphDescription
) {
  throw new Error('MCP update_graph did not persist graph metadata changes')
}
const mcpManagedGraphRead = await timedStep('mcpManageGraphReadMs', () => callTool('manage_graph', {
  graph_id: mcpGraphId,
  action: 'read',
}))
if (
  mcpManagedGraphRead.graph_id !== mcpGraphId
  || mcpManagedGraphRead.title !== mcpUpdatedGraphTitle
  || mcpManagedGraphRead.description !== mcpUpdatedGraphDescription
) {
  throw new Error('MCP manage_graph read did not return updated graph metadata')
}
const mcpGraphSourceDocumentId = `parity-mcp-graph-doc-${Date.now()}`
await callTool('create_document', {
  graphId: mcpGraphId,
  documentId: mcpGraphSourceDocumentId,
  title: 'Parity MCP Graph Duplicate Source',
})
await callTool('write_document', {
  graph_id: mcpGraphId,
  document_id: mcpGraphSourceDocumentId,
  content: '# Duplicate source\n\nGraph duplicate carries this block.',
  format: 'markdown',
})
await callTool('flush_crdt', { graphId: mcpGraphId })
const mcpDuplicateGraphId = `${mcpGraphId}-copy`
const mcpDuplicateGraphTitle = 'Parity MCP Graph Copy'
const mcpDuplicatedGraph = await timedStep('mcpDuplicateGraphMs', () => callTool('duplicate_graph', {
  source_graph_id: mcpGraphId,
  new_graph_id: mcpDuplicateGraphId,
  new_title: mcpDuplicateGraphTitle,
}))
if (
  !mcpDuplicatedGraph.success
  || mcpDuplicatedGraph.source_graph_id !== mcpGraphId
  || mcpDuplicatedGraph.new_graph_id !== mcpDuplicateGraphId
  || mcpDuplicatedGraph.title !== mcpDuplicateGraphTitle
  || mcpDuplicatedGraph.document_count < 1
) {
  throw new Error('MCP duplicate_graph returned an invalid duplicate graph envelope')
}
const mcpDuplicateWorkspace = await callTool('get_workspace', { graphId: mcpDuplicateGraphId, depth: 1 })
if (!mcpDuplicateWorkspace.documents?.some((document) =>
  (document.documentId ?? document.document_id ?? document.id) === mcpGraphSourceDocumentId
)) {
  throw new Error('MCP duplicate_graph did not preserve the source document in workspace')
}
const mcpDuplicateDocument = await callTool('read_document', {
  graphId: mcpDuplicateGraphId,
  documentId: mcpGraphSourceDocumentId,
  format: 'markdown',
})
if (!String(mcpDuplicateDocument.content ?? '').includes('Graph duplicate carries this block.')) {
  throw new Error('MCP duplicate_graph did not preserve duplicated document content')
}
const mcpDuplicateDelete = await timedStep('mcpManageGraphDeleteMs', () => callTool('manage_graph', {
  graph_id: mcpDuplicateGraphId,
  action: 'delete',
}))
if (
  mcpDuplicateDelete.status !== 'deleted'
  || mcpDuplicateDelete.graph_id !== mcpDuplicateGraphId
) {
  throw new Error('MCP manage_graph delete did not return a deleted graph envelope')
}
const mcpDeletedDuplicateRead = await fetchJsonAnyStatus(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(mcpDuplicateGraphId)}?wait_ms=1`,
  { headers: authHeaders },
)
if (mcpDeletedDuplicateRead.status !== 404) {
  throw new Error('MCP manage_graph delete left the duplicate graph readable')
}
const mcpGraphQuery = await timedStep('mcpQueryGraphMs', () => callTool('query_graph', {
  graph_id: mcpGraphId,
  query: 'SELECT (COUNT(*) AS ?count) WHERE { ?s ?p ?o . }',
}))
if (
  mcpGraphQuery.graph_id !== mcpGraphId
  || !Array.isArray(mcpGraphQuery.rows)
  || typeof mcpGraphQuery.quadCount !== 'number'
) {
  throw new Error('MCP query_graph returned an invalid SPARQL result envelope')
}
const mcpGraphStats = await timedStep('mcpManageGraphStatsMs', () => callTool('manage_graph', {
  graph_id: mcpGraphId,
  action: 'stats',
}))
if (
  mcpGraphStats.graph_id !== mcpGraphId
  || mcpGraphStats.status !== 'ok'
  || typeof mcpGraphStats.document_count !== 'number'
  || typeof mcpGraphStats.triple_count !== 'number'
) {
  throw new Error('MCP manage_graph stats returned an invalid graph stats envelope')
}
const graphStats = await timedStep('graphStatsMs', () => fetchJson(
  `${manifest.apiUrl}/graphs/stats?wait_ms=1`,
  { headers: authHeaders },
))
if (
  typeof graphStats.total_graphs !== 'number'
  || typeof graphStats.total_triples !== 'number'
  || typeof graphStats.avg_triples !== 'number'
  || graphStats.total_graphs < 1
) {
  throw new Error('GET /graphs/stats?wait_ms=1 returned an invalid graph metadata stats envelope')
}
const graphStatsJob = await fetchJsonResponse(`${manifest.apiUrl}/graphs/stats`, { headers: authHeaders })
if (graphStatsJob.status !== 202 || !graphStatsJob.body?.job_id || !graphStatsJob.body?.result_url) {
  throw new Error('GET /graphs/stats did not return a hosted-shaped graph stats job envelope')
}
const graphStatsJobResult = await fetchJson(`${manifest.apiUrl}${graphStatsJob.body.result_url}`, { headers: authHeaders })
if (typeof graphStatsJobResult.total_graphs !== 'number' || typeof graphStatsJobResult.total_triples !== 'number') {
  throw new Error('GET /graphs/stats job result did not return graph metadata stats')
}
const graphPreflight = await timedStep('documentPreflightMs', () => fetchJson(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(mcpGraphId)}/documents/preflight`,
  { method: 'POST', headers: authHeaders },
))
if (
  graphPreflight.allowed !== true
  || typeof graphPreflight.graph_storage?.used_bytes !== 'number'
  || typeof graphPreflight.graph_storage?.usage_ratio !== 'number'
  || graphPreflight.graph_storage?.limit_reached !== false
) {
  throw new Error('POST /graphs/{graph_id}/documents/preflight returned an invalid storage envelope')
}
const graphPreflightWarning = await fetchJson(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(mcpGraphId)}/documents/preflight?simulate=warning`,
  { method: 'POST', headers: authHeaders },
)
if (
  graphPreflightWarning.allowed !== true
  || graphPreflightWarning.graph_storage?.simulated !== true
  || graphPreflightWarning.graph_storage?.warning_threshold_reached !== true
) {
  throw new Error('POST /graphs/{graph_id}/documents/preflight?simulate=warning did not mirror hosted dev simulation')
}
const graphPreflightBlocked = await fetchJsonAnyStatus(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(mcpGraphId)}/documents/preflight?simulate=blocked`,
  { method: 'POST', headers: authHeaders },
)
if (
  graphPreflightBlocked.status !== 403
  || graphPreflightBlocked.body?.detail?.error_code !== 'GRAPH_STORAGE_LIMIT_REACHED'
  || graphPreflightBlocked.body?.detail?.graph_storage?.limit_reached !== true
) {
  throw new Error('POST /graphs/{graph_id}/documents/preflight?simulate=blocked did not mirror hosted limit semantics')
}
const routeDuplicateGraphId = `${mcpGraphId}-route-copy`
const routeDuplicateTitle = 'Parity Graph Route Copy'
const graphDuplicate = await timedStep('graphDuplicateRouteMs', () => fetchJsonResponse(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(mcpGraphId)}/duplicate`,
  {
    method: 'POST',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      new_graph_id: routeDuplicateGraphId,
      new_title: routeDuplicateTitle,
    }),
  },
))
if (graphDuplicate.status !== 202 || !graphDuplicate.body?.job_id || !graphDuplicate.body?.links?.result) {
  throw new Error('POST /graphs/{graph_id}/duplicate did not return a hosted-shaped duplicate job')
}
const graphDuplicateResult = await fetchJson(
  `${manifest.apiUrl}${graphDuplicate.body.links.result}`,
  { headers: authHeaders },
)
if (
  graphDuplicateResult.source_graph_id !== mcpGraphId
  || graphDuplicateResult.new_graph_id !== routeDuplicateGraphId
  || graphDuplicateResult.title !== routeDuplicateTitle
  || typeof graphDuplicateResult.rdf_quad_count !== 'number'
  || graphDuplicateResult.document_count < 1
) {
  throw new Error('POST /graphs/{graph_id}/duplicate job result did not preserve graph duplicate semantics')
}
const routeDuplicateDocument = await callTool('read_document', {
  graphId: routeDuplicateGraphId,
  documentId: mcpGraphSourceDocumentId,
  format: 'markdown',
})
if (!String(routeDuplicateDocument.content ?? '').includes('Graph duplicate carries this block.')) {
  throw new Error('POST /graphs/{graph_id}/duplicate did not preserve duplicated document content')
}
const graphDuplicateDelete = await fetchJson(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(routeDuplicateGraphId)}`,
  {
    method: 'DELETE',
    headers: authHeaders,
  },
)
if (!graphDuplicateDelete.job_id) {
  throw new Error('cleanup DELETE /graphs/{graph_id} for route duplicate did not return a job id')
}
graphSupportAliases = {
  checked: true,
  graphId: mcpGraphId,
  duplicateGraphId: routeDuplicateGraphId,
  duplicateDocuments: graphDuplicateResult.document_count,
  totalGraphs: graphStats.total_graphs,
  totalTriples: graphStats.total_triples,
  preflightUsedBytes: graphPreflight.graph_storage.used_bytes,
}
const mcpReindex = await timedStep('mcpReindexGraphMs', () => callTool('reindex_graph', {
  graph_id: mcpGraphId,
}))
if (
  mcpReindex.graph_id !== mcpGraphId
  || typeof mcpReindex.total_docs !== 'number'
  || typeof mcpReindex.queued !== 'number'
) {
  throw new Error('MCP reindex_graph returned an invalid reindex envelope')
}
const mcpGraphDelete = await fetchJson(
  `${manifest.apiUrl}/graphs/${encodeURIComponent(mcpGraphId)}`,
  {
    method: 'DELETE',
    headers: authHeaders,
  },
)
if (!mcpGraphDelete.job_id) {
  throw new Error('cleanup DELETE /graphs/{graph_id} for MCP graph did not return a job id')
}
const mcpCancelJob = await timedStep('mcpCancelJobMs', () => callTool('cancel_job', {
  job_id: mcpGraphDelete.job_id,
}))
if (
  mcpCancelJob.job_id !== mcpGraphDelete.job_id
  || typeof mcpCancelJob.cancelled !== 'boolean'
  || !mcpCancelJob.previous_status
) {
  throw new Error('MCP cancel_job returned an invalid cancellation envelope')
}
mcpGraphJobs = {
  checked: true,
  graphId: mcpGraphId,
  duplicateGraphId: mcpDuplicateGraphId,
  duplicateDocuments: mcpDuplicatedGraph.document_count,
  manageStatsDocuments: mcpGraphStats.document_count,
  reindexSkipped: Boolean(mcpReindex.skipped),
  queryRows: mcpGraphQuery.rows.length,
  cancelPreviousStatus: mcpCancelJob.previous_status,
}

function literalCount(value) {
  const match = String(value ?? '').match(/^"([0-9]+)"/)
  return match ? Number(match[1]) : 0
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

function storedTarGz(entries) {
  const parts = []
  for (const [name, value] of Object.entries(entries)) {
    const data = Buffer.isBuffer(value) ? value : Buffer.from(value)
    const header = tarHeader(name, data.length)
    const padding = Buffer.alloc((512 - (data.length % 512)) % 512)
    parts.push(header, data, padding)
  }
  parts.push(Buffer.alloc(1024))
  return gzipSync(Buffer.concat(parts))
}

function tarHeader(name, size) {
  const normalized = name.replaceAll('\\', '/')
  if (normalized.startsWith('/') || normalized.split('/').includes('..')) {
    throw new Error(`unsafe tar path: ${name}`)
  }
  const header = Buffer.alloc(512)
  let fileName = normalized
  let prefix = ''
  if (Buffer.byteLength(fileName) > 100) {
    const slash = normalized.lastIndexOf('/')
    prefix = normalized.slice(0, slash)
    fileName = normalized.slice(slash + 1)
  }
  if (Buffer.byteLength(fileName) > 100 || Buffer.byteLength(prefix) > 155) {
    throw new Error(`tar path too long: ${name}`)
  }
  header.write(fileName, 0, 100)
  writeTarOctal(header, 0o644, 100, 8)
  writeTarOctal(header, 0, 108, 8)
  writeTarOctal(header, 0, 116, 8)
  writeTarOctal(header, size, 124, 12)
  writeTarOctal(header, 0, 136, 12)
  header.fill(0x20, 148, 156)
  header[156] = '0'.charCodeAt(0)
  header.write('ustar\0', 257, 6)
  header.write('00', 263, 2)
  if (prefix) header.write(prefix, 345, 155)
  let checksum = 0
  for (const byte of header) checksum += byte
  const checksumValue = checksum.toString(8).padStart(6, '0')
  header.write(checksumValue, 148, 6)
  header[154] = 0
  header[155] = 0x20
  return header
}

function writeTarOctal(header, value, offset, length) {
  const raw = value.toString(8).padStart(length - 1, '0')
  header.write(raw.slice(-length + 1), offset, length - 1)
  header[offset + length - 1] = 0
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

function simplePdfBytes({ title, lines }) {
  const textOperators = [
    'BT',
    '/F1 12 Tf',
    '72 1900 Td',
    ...lines.flatMap((line, index) => [
      index === 0 ? '' : '0 -28 Td',
      `(${pdfString(line)}) Tj`,
    ]).filter(Boolean),
    'ET',
  ].join('\n')
  const stream = `${textOperators}\n`
  const toUnicode = pdfToUnicodeCMap()
  const objects = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 2000 2000] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>',
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding /ToUnicode 7 0 R >>',
    `<< /Length ${Buffer.byteLength(stream)} >>\nstream\n${stream}endstream`,
    `<< /Title (${pdfString(title)}) >>`,
    `<< /Length ${Buffer.byteLength(toUnicode)} >>\nstream\n${toUnicode}endstream`,
  ]
  let pdf = '%PDF-1.4\n'
  const offsets = [0]
  objects.forEach((body, index) => {
    offsets.push(Buffer.byteLength(pdf))
    pdf += `${index + 1} 0 obj\n${body}\nendobj\n`
  })
  const xrefOffset = Buffer.byteLength(pdf)
  pdf += `xref\n0 ${objects.length + 1}\n`
  pdf += '0000000000 65535 f \n'
  offsets.slice(1).forEach((offset) => {
    pdf += `${String(offset).padStart(10, '0')} 00000 n \n`
  })
  pdf += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R /Info 6 0 R >>\n`
  pdf += `startxref\n${xrefOffset}\n%%EOF\n`
  return Buffer.from(pdf, 'utf8')
}

function pdfString(value) {
  return String(value).replace(/[\\()]/g, '\\$&')
}

function pdfToUnicodeCMap() {
  const mappings = Array.from({ length: 95 }, (_, index) => {
    const code = index + 32
    return `<${code.toString(16).toUpperCase().padStart(2, '0')}> <${code.toString(16).toUpperCase().padStart(4, '0')}>`
  }).join('\n')
  return `/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<00> <FF>
endcodespacerange
95 beginbfchar
${mappings}
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end
`
}

let materialization = { checked: false }
let mcpOrientation = { checked: false }
let mcpWorkspaceManagement = { checked: false }
let mcpWireAdapters = { checked: false }
let readAdapters = { checked: false }
let hostedAliases = { checked: false }
let entityAliases = { checked: false }
let graphAnalytics = { checked: false }
let graphExport = { checked: false }
let graphArchiveImport = { checked: false }
let graphRdfImport = { checked: false }
let graphVaultImport = { checked: false }
let graphWebImports = { checked: false }
let searchMaintenance = { checked: false }
let mcpMemoryAdapters = { checked: false }
let mcpNarrativeSurface = { checked: false }
let mcpDocumentHistory = { checked: false }
let documentHistoryApi = { checked: false }
let navigationFolder = { checked: false }
let navigationJobResult = { checked: false }
let documentExport = { checked: false }
let documentBlobs = { checked: false }
let documentDescription = { checked: false }
let documentDuplicate = { checked: false }
let documentFlush = { checked: false }
let mcpWriteAdapters = { checked: false }
let mcpDocumentEditability = { checked: false }
let mcpArtifactAdapters = { checked: false }
let mcpCommentAdapters = { checked: false }
let mcpValuationAdapters = { checked: false }
let blockMutations = { checked: false }
let wireMutations = { checked: false }
let artifactMutations = { checked: false }
let artifactRouteImportConvert = { checked: false }
let artifactUpload = { checked: false }
let artifactPdfAccurate = { checked: false }
let artifactFormatUpload = { checked: false }
let artifactBatchUpload = { checked: false }
let artifactImageUpload = { checked: false }
{
  const mainGraphId = `parity-loopback-${Date.now()}`
  const mainGraph = await callTool('create_graph', {
    graph_id: mainGraphId,
    title: 'Parity Loopback Live Surface',
    description: 'Fresh graph created by parity:loopback to isolate live route state.',
  })
  const graphId = mainGraph.graph_id ?? mainGraph.graphId ?? mainGraphId
  const seededDocumentId = `parity-loopback-source-${Date.now()}`
  await callTool('create_document', {
    graphId,
    documentId: seededDocumentId,
    title: 'Parity Loopback Source',
  })
  await callTool('write_document', {
    graph_id: graphId,
    document_id: seededDocumentId,
    content: '# Parity Loopback Source\n\nSeed content for graph archive, read adapter, and hosted parity checks.',
    format: 'markdown',
  })
  await callTool('flush_crdt', { graphId })
  let workspace = await callTool('get_workspace', { graphId, depth: 2 })
  const quickOrient = await timedStep('mcpQuickOrientMs', () => callTool('quick_orient', {
    graph_id: graphId,
    recall_limit: 3,
  }))
  if (
    quickOrient.location?.graph_id !== graphId
    || !Array.isArray(quickOrient.recall?.memories)
    || typeof quickOrient.song?.verse_count !== 'number'
    || quickOrient.workspace?.graph_id !== graphId
  ) {
    throw new Error('MCP quick_orient returned an invalid local orientation envelope')
  }
  const contextBundle = await timedStep('mcpContextBundleMs', () => callTool('context_bundle', {
    graph_id: graphId,
    recall_limit: 2,
    important_limit: 2,
    workspace_depth: 2,
  }))
  if (
    contextBundle.location?.graph_id !== graphId
    || contextBundle.workspace?.graph_id !== graphId
    || !Array.isArray(contextBundle.recall?.memories)
    || !Array.isArray(contextBundle.important_blocks?.blocks)
  ) {
    throw new Error('MCP context_bundle returned an invalid local context envelope')
  }
  mcpOrientation = {
    checked: true,
    graphId,
    quickRecall: quickOrient.recall.memories.length,
    contextRecall: contextBundle.recall.memories.length,
    importantBlocks: contextBundle.important_blocks.blocks.length,
    workspaceDocuments: contextBundle.workspace.counts?.documents ?? null,
  }

  const workspaceParentFolderId = `parity-mcp-parent-${Date.now()}`
  const workspaceChildFolderId = `parity-mcp-child-${Date.now()}`
  const workspaceDocumentId = `parity-mcp-workspace-doc-${Date.now()}`
  const workspaceChildRenamed = 'Parity MCP Child Renamed'
  const workspaceDocumentRenamed = 'Parity MCP Document Renamed'
  await callTool('create_folder', {
    graphId,
    folderId: workspaceParentFolderId,
    name: 'Parity MCP Parent',
  })
  await callTool('create_folder', {
    graphId,
    folderId: workspaceChildFolderId,
    name: 'Parity MCP Child',
  })
  const movedFolder = await timedStep('mcpMoveFolderMs', () => callTool('move_folder', {
    graph_id: graphId,
    folder_id: workspaceChildFolderId,
    new_parent_id: workspaceParentFolderId,
  }))
  if (
    movedFolder.folderId !== workspaceChildFolderId
    || movedFolder.parentId !== workspaceParentFolderId
  ) {
    throw new Error('MCP move_folder did not move the child folder under the parent')
  }
  await callTool('create_document', {
    graphId,
    documentId: workspaceDocumentId,
    title: 'Parity MCP Document',
    parentId: workspaceChildFolderId,
  })
  await timedStep('mcpRenameMs', async () => {
    const renamedFolder = await callTool('rename', {
      graph_id: graphId,
      entity_type: 'folder',
      entity_id: workspaceChildFolderId,
      new_name: workspaceChildRenamed,
    })
    if (!renamedFolder.success || renamedFolder.new_name !== workspaceChildRenamed) {
      throw new Error('MCP rename did not return the expected folder rename envelope')
    }
    const renamedDocument = await callTool('rename', {
      graph_id: graphId,
      entity_type: 'document',
      entity_id: workspaceDocumentId,
      new_name: workspaceDocumentRenamed,
    })
    if (!renamedDocument.success || renamedDocument.new_name !== workspaceDocumentRenamed) {
      throw new Error('MCP rename did not return the expected document rename envelope')
    }
  })
  // A2 item B4 deferred projection flush off the request path; folder/document
  // create, move, and rename mutations above may not yet be materialized into the
  // workspace projection. Force a flush before asserting on the returned state.
  await callTool('flush_crdt', { graphId })
  const workspaceAfterManagement = await callTool('get_workspace', { graphId, depth: 2 })
  const managedFolder = (workspaceAfterManagement.folders ?? []).find((folder) => folder.id === workspaceChildFolderId)
  if (
    !managedFolder
    || managedFolder.name !== workspaceChildRenamed
    || managedFolder.parentId !== workspaceParentFolderId
  ) {
    throw new Error('MCP workspace management did not persist moved/renamed folder state')
  }
  const managedDocument = (workspaceAfterManagement.documents ?? [])
    .find((document) => (document.documentId ?? document.document_id ?? document.id) === workspaceDocumentId)
  if (!managedDocument || managedDocument.title !== workspaceDocumentRenamed) {
    throw new Error('MCP workspace management did not persist renamed document state')
  }
  await timedStep('mcpDeleteMs', async () => {
    const deletedDocument = await callTool('delete', {
      graph_id: graphId,
      type: 'documents',
      document_id: workspaceDocumentId,
    })
    if (!deletedDocument.success || deletedDocument.deleted_count !== 1) {
      throw new Error('MCP delete did not delete the managed document')
    }
    const deletedChildFolder = await callTool('delete', {
      graph_id: graphId,
      type: 'folder',
      folder_id: workspaceChildFolderId,
    })
    if (!deletedChildFolder.success) {
      throw new Error('MCP delete did not delete the managed child folder')
    }
    const deletedParentFolder = await callTool('delete', {
      graph_id: graphId,
      type: 'folder',
      folder_id: workspaceParentFolderId,
    })
    if (!deletedParentFolder.success) {
      throw new Error('MCP delete did not delete the managed parent folder')
    }
  })
  mcpWorkspaceManagement = {
    checked: true,
    graphId,
    parentFolderId: workspaceParentFolderId,
    childFolderId: workspaceChildFolderId,
    documentId: workspaceDocumentId,
    renamedFolder: workspaceChildRenamed,
    renamedDocument: workspaceDocumentRenamed,
  }

  const wireDocuments = (workspace.documents ?? [])
    .filter((document) => document.documentId || document.document_id || document.id)
  const temporaryWireDocumentIds = []
  let mcpWireSourceDocumentId = wireDocuments[0]?.documentId ?? wireDocuments[0]?.document_id ?? wireDocuments[0]?.id
  let mcpWireTargetDocumentId = wireDocuments
    .slice(1)
    .map((document) => document.documentId ?? document.document_id ?? document.id)
    .find((documentId) => documentId && documentId !== mcpWireSourceDocumentId)

  if (!mcpWireSourceDocumentId) {
    mcpWireSourceDocumentId = `parity-mcp-wire-source-${Date.now()}`
    temporaryWireDocumentIds.push(mcpWireSourceDocumentId)
    await callTool('create_document', {
      graphId,
      documentId: mcpWireSourceDocumentId,
      title: 'Parity MCP Wire Source',
    })
  }
  if (!mcpWireTargetDocumentId) {
    mcpWireTargetDocumentId = `parity-mcp-wire-target-${Date.now()}`
    temporaryWireDocumentIds.push(mcpWireTargetDocumentId)
    await callTool('create_document', {
      graphId,
      documentId: mcpWireTargetDocumentId,
      title: 'Parity MCP Wire Target',
    })
  }

  const mcpCreatedWires = await timedStep('mcpCreateWiresMs', () => callTool('create_wires', {
    graph_id: graphId,
    source_document_id: mcpWireSourceDocumentId,
    target_document_id: mcpWireTargetDocumentId,
    predicate: 'supports',
    bidirectional: false,
  }))
  const mcpCreatedWire = mcpCreatedWires.wires?.[0]
  if (
    !mcpCreatedWires.success
    || mcpCreatedWires.created_count !== 1
    || !mcpCreatedWire?.id
    || mcpCreatedWire.source_document_id !== mcpWireSourceDocumentId
    || mcpCreatedWire.target_document_id !== mcpWireTargetDocumentId
    || mcpCreatedWire.predicate_label !== 'supports'
  ) {
    throw new Error('MCP create_wires returned an invalid wire creation envelope')
  }
  // A2 item B4 deferred projection flush off the request path; the wire create
  // CRDT mutation may not yet be visible in the projection-backed get_wires and
  // traverse_wires reads. Force a flush first.
  await callTool('flush_crdt', { graphId })
  const mcpSourceWires = await callTool('get_wires', {
    graphId,
    documentId: mcpWireSourceDocumentId,
    direction: 'outgoing',
    predicate: 'supports',
  })
  if (!mcpSourceWires.wires?.some((wire) => wire.id === mcpCreatedWire.id)) {
    throw new Error('MCP create_wires did not persist a source outgoing wire')
  }
  const mcpWireTraversal = await timedStep('mcpTraverseWiresMs', () => callTool('traverse_wires', {
    graph_id: graphId,
    document_id: mcpWireSourceDocumentId,
    max_depth: 1,
    direction: 'outgoing',
    predicate: 'supports',
  }))
  if (
    mcpWireTraversal.document_id !== mcpWireSourceDocumentId
    || !mcpWireTraversal.documents?.some((document) => document.documentId === mcpWireTargetDocumentId)
    || !mcpWireTraversal.paths?.some((path) => path.hops?.some((hop) => hop.wireId === mcpCreatedWire.id))
  ) {
    throw new Error('MCP traverse_wires did not discover the created wire path')
  }
  const mcpDeletedWire = await callTool('delete', {
    graph_id: graphId,
    type: 'wires',
    wire_id: mcpCreatedWire.id,
  })
  if (!mcpDeletedWire.success || mcpDeletedWire.deleted_count !== 1) {
    throw new Error('MCP delete did not delete the created MCP wire')
  }
  // A2 item B4 deferred projection flush off the request path; the wire delete
  // CRDT mutation may not yet be reflected in the projection-backed get_wires
  // read. Force a flush before asserting visibility.
  await callTool('flush_crdt', { graphId })
  const mcpSourceWiresAfterDelete = await callTool('get_wires', {
    graphId,
    documentId: mcpWireSourceDocumentId,
    direction: 'outgoing',
    predicate: 'supports',
  })
  if (mcpSourceWiresAfterDelete.wires?.some((wire) => wire.id === mcpCreatedWire.id)) {
    throw new Error('MCP wire delete left the created wire visible')
  }
  for (const documentId of temporaryWireDocumentIds) {
    await callTool('delete', {
      graph_id: graphId,
      type: 'documents',
      document_id: documentId,
    })
  }
  mcpWireAdapters = {
    checked: true,
    graphId,
    sourceDocumentId: mcpWireSourceDocumentId,
    targetDocumentId: mcpWireTargetDocumentId,
    wireId: mcpCreatedWire.id,
    temporaryDocuments: temporaryWireDocumentIds.length,
  }
  // A2 item B4 deferred projection flush off the request path; CRDT mutations
  // (folder/document/wire creates and deletes above) may not yet be materialized
  // into the RDF projection. Force a flush and refresh the snapshot before
  // asserting on counts.
  await callTool('flush_crdt', { graphId })
  workspace = await callTool('get_workspace', { graphId, depth: 2 })
  const rdfCounts = await fetchJson(`${manifest.apiUrl}/api/sparql/query`, {
    method: 'POST',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      graphId,
      query: `PREFIX doc: <http://mnemosyne.dev/doc#>
PREFIX mnemo: <http://mnemosyne.ai/vocab#>
SELECT ?kind (COUNT(?s) AS ?count) WHERE {
  GRAPH ?projectionGraph {
    { ?s a doc:TipTapDocument . BIND("documents" AS ?kind) }
    UNION { ?s a doc:Folder . BIND("folders" AS ?kind) }
    UNION { ?s a doc:Artifact . BIND("artifacts" AS ?kind) }
    UNION { ?s a mnemo:Wire . BIND("wires" AS ?kind) }
  }
}
GROUP BY ?kind`,
    }),
  })
  const rdfCountByKind = Object.fromEntries(
    (rdfCounts.rows ?? []).map((row) => [
      String(row.kind ?? '').replace(/^"|"$/g, ''),
      literalCount(row.count),
    ]),
  )

  for (const kind of ['documents', 'folders', 'artifacts', 'wires']) {
    const expectedCount = workspace.counts?.[kind] ?? 0
    const actualCount = rdfCountByKind[kind] ?? 0
    if (expectedCount !== actualCount) {
      throw new Error(
        `Workspace RDF materialization mismatch for ${kind}: workspace=${expectedCount}, rdf=${actualCount}`,
      )
    }
  }
  materialization = {
    checked: true,
    graphId,
    workspace: workspace.workspace?.materialization ?? 'unknown',
    counts: workspace.counts,
  }
  const graphSummary = await timedStep('graphAnalyticsSummaryMs', () => fetchJson(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/summary?wait_ms=1`,
    { headers: authHeaders },
  ))
  if (
    graphSummary.graph_id !== graphId
    || graphSummary.document_count !== (workspace.counts?.documents ?? 0)
    || typeof graphSummary.node_count !== 'number'
    || typeof graphSummary.edge_count !== 'number'
  ) {
    throw new Error('GET /graphs/{graph_id}/summary returned an invalid workspace summary')
  }
  const graphProperties = await timedStep('graphAnalyticsPropertiesMs', () => fetchJson(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/properties?wait_ms=1`,
    { headers: authHeaders },
  ))
  if (graphProperties.graph_id !== graphId || !Array.isArray(graphProperties.properties)) {
    throw new Error('GET /graphs/{graph_id}/properties returned an invalid property usage envelope')
  }
  const graphViz = await timedStep('graphAnalyticsVizMs', () => fetchJson(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/viz?limit_edges=25&wait_ms=1`,
    { headers: authHeaders },
  ))
  if (
    graphViz.graph_id !== graphId
    || !Array.isArray(graphViz.nodes)
    || !Array.isArray(graphViz.edges)
    || typeof graphViz.stats?.node_count !== 'number'
    || typeof graphViz.stats?.edge_count !== 'number'
  ) {
    throw new Error('GET /graphs/{graph_id}/viz returned an invalid viz envelope')
  }
  graphAnalytics = {
    checked: true,
    graphId,
    summary: {
      nodeCount: graphSummary.node_count,
      edgeCount: graphSummary.edge_count,
      documentCount: graphSummary.document_count,
    },
    propertyCount: graphProperties.properties.length,
    viz: graphViz.stats,
  }
  const exportGraph = await timedStep('graphExportMs', () => fetchJsonResponse(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/export`,
    {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ include_artifacts: false }),
    },
  ))
  if (exportGraph.status !== 202 || !exportGraph.body?.job_id || !exportGraph.body?.links?.result) {
    throw new Error('POST /graphs/{graph_id}/export did not return a hosted-shaped export job')
  }
  const exportResult = await fetchJson(`${manifest.apiUrl}${exportGraph.body.links.result}`, { headers: authHeaders })
  if (
    exportResult.format !== 'TriG'
    || exportResult.mediaType !== 'application/trig'
    || typeof exportResult.data !== 'string'
    || !exportResult.data.includes('mnemosyne')
    || typeof exportResult.quadCount !== 'number'
  ) {
    throw new Error('POST /graphs/{graph_id}/export job result did not return a valid RDF dump')
  }
  const navigationExportResult = await timedStep('navigationJobResultMs', () => fetchJson(
    `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/job-result/${encodeURIComponent(exportGraph.body.job_id)}`,
    { headers: authHeaders },
  ))
  if (
    navigationExportResult.format !== exportResult.format
    || navigationExportResult.quadCount !== exportResult.quadCount
  ) {
    throw new Error('GET /navigation/{graph_id}/job-result/{job_id} did not return the graph-scoped job result')
  }
  graphExport = {
    checked: true,
    graphId,
    jobId: exportGraph.body.job_id,
    format: exportResult.format,
    quadCount: exportResult.quadCount,
  }
  navigationJobResult = {
    checked: true,
    graphId,
    jobId: exportGraph.body.job_id,
    format: navigationExportResult.format,
  }

  if (!cellSkip('graphArchiveImport')) {
  const graphArchiveRunId = Date.now()
  const archiveImportGraphId = `parity-archive-import-${graphArchiveRunId}`
  const archiveImportTitle = `Parity Archive Import ${graphArchiveRunId}`
  const archiveSentinelTitle = `Graph archive sentinel ${graphArchiveRunId}`
  const sourceDocuments = await fetchJson(`${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}`, {
    headers: authHeaders,
  })
  if (!Array.isArray(sourceDocuments) || sourceDocuments.length === 0) {
    throw new Error('Graph archive import fixture needs at least one source document')
  }
  const sourceDocumentReads = []
  const documentArchiveEntries = {}
  for (const document of sourceDocuments) {
    const documentId = document.id ?? document.documentId ?? document.document_id
    if (!documentId) continue
    const [read, blob] = await Promise.all([
      fetchJson(`${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}`, {
        headers: authHeaders,
      }),
      fetchBytesResponse(`${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/blob`, {
        headers: authHeaders,
      }),
    ])
    sourceDocumentReads.push({ documentId, title: read.title ?? document.title, blocks: read.blocks ?? [] })
    documentArchiveEntries[`crdt/documents/${documentId}.yjs`] = Buffer.from(blob.bytes)
  }
  const archiveSourceRead = sourceDocumentReads.find((document) =>
    (document.blocks ?? []).some((block) => String(block.content ?? '').trim()),
  )
  if (!archiveSourceRead) {
    throw new Error('Graph archive import fixture needs a readable source document')
  }
  const archiveSourceSnippet = String(
    (archiveSourceRead.blocks ?? []).find((block) => String(block.content ?? '').trim())?.content ?? '',
  ).slice(0, 32)
  const workspaceBlob = await fetchBytesResponse(
    `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/workspace/blob`,
    { headers: authHeaders },
  )
  if (workspaceBlob.bytes.length === 0) {
    throw new Error('Graph archive import fixture needs a workspace Y.Doc blob')
  }
  const sourceUserId = 'parity-user'
  const graphArchiveBytes = storedTarGz({
    'manifest.json': JSON.stringify({
      version: 1,
      format: 'mnemosyne-graph-export',
      exported_at: new Date().toISOString(),
      source_user_id: sourceUserId,
      source_graph_id: graphId,
      source_graph_title: 'Parity Source Graph',
      source_graph_description: 'Graph archive import fixture source.',
      includes_artifacts: false,
      document_count: Object.keys(documentArchiveEntries).length,
      rdf_triple_count: 1,
      files: {
        rdf: 'rdf/graph.nq',
        workspace: 'crdt/workspace.yjs',
        documents_dir: 'crdt/documents/',
        artifacts_dir: 'artifacts/',
      },
    }),
    'rdf/graph.nq': `<urn:mnemosyne:user:${sourceUserId}:graph:${graphId}:sentinel> <http://purl.org/dc/terms/title> ${JSON.stringify(archiveSentinelTitle)} <urn:mnemosyne:user:${sourceUserId}:graph:${graphId}> .\n`,
    'crdt/workspace.yjs': Buffer.from(workspaceBlob.bytes),
    ...documentArchiveEntries,
  })
  const graphArchiveForm = new FormData()
  graphArchiveForm.append('file', new Blob([graphArchiveBytes], { type: 'application/gzip' }), `graph-${graphArchiveRunId}.tar.gz`)
  graphArchiveForm.append('new_graph_id', archiveImportGraphId)
  graphArchiveForm.append('new_title', archiveImportTitle)
  const graphArchiveImportResponse = await timedStep('graphArchiveImportMs', () => fetchJsonResponse(
    `${manifest.apiUrl}/graphs/import`,
    {
      method: 'POST',
      headers: authHeaders,
      body: graphArchiveForm,
    },
  ))
  const graphArchiveJobId = graphArchiveImportResponse.body?.jobId ?? graphArchiveImportResponse.body?.job_id
  if (
    graphArchiveImportResponse.status !== 202
    || !graphArchiveJobId
    || !graphArchiveImportResponse.body?.links?.result
    || !['queued', 'running', 'succeeded'].includes(String(graphArchiveImportResponse.body?.status ?? ''))
  ) {
    throw new Error('POST /graphs/import did not return a hosted-shaped graph import job envelope')
  }
  const graphArchiveResult = await fetchJobResultEventually(
    `${manifest.apiUrl}${graphArchiveImportResponse.body.links.result}`,
    { headers: authHeaders },
    240,
    500,
  )
  if (
    graphArchiveResult.graph_id !== archiveImportGraphId
    || graphArchiveResult.title !== archiveImportTitle
    || graphArchiveResult.source_graph_id !== graphId
    || graphArchiveResult.document_count !== Object.keys(documentArchiveEntries).length
    || graphArchiveResult.workspace_imported !== true
    || typeof graphArchiveResult.rdf_triple_count !== 'number'
  ) {
    throw new Error('POST /graphs/import job result did not preserve graph archive metadata')
  }
  const importedGraphRead = await fetchJson(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(archiveImportGraphId)}?wait_ms=1`,
    { headers: authHeaders },
  )
  if (importedGraphRead.graph_id !== archiveImportGraphId || importedGraphRead.title !== archiveImportTitle) {
    throw new Error('POST /graphs/import did not create readable graph metadata')
  }
  const importedWorkspace = await callTool('get_workspace', { graphId: archiveImportGraphId, depth: 0 })
  if ((importedWorkspace.documents ?? []).length !== Object.keys(documentArchiveEntries).length) {
    throw new Error('POST /graphs/import did not materialize imported workspace documents')
  }
  const importedArchiveDocument = await fetchJson(
    `${manifest.apiUrl}/documents/${encodeURIComponent(archiveImportGraphId)}/${encodeURIComponent(archiveSourceRead.documentId)}`,
    { headers: authHeaders },
  )
  if (!(importedArchiveDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(archiveSourceSnippet))) {
    throw new Error('POST /graphs/import did not materialize imported document Y.Doc content')
  }
  const graphArchiveSentinel = await fetchJson(`${manifest.apiUrl}/api/sparql/query`, {
    method: 'POST',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      graphId: archiveImportGraphId,
      query: `SELECT ?title WHERE { GRAPH ?g { ?s <http://purl.org/dc/terms/title> ?title . FILTER(STR(?title) = ${JSON.stringify(archiveSentinelTitle)}) } }`,
    }),
  })
  if (graphArchiveSentinel.rows?.length !== 1) {
    throw new Error('POST /graphs/import did not load the archive RDF payload')
  }
  const invalidGraphArchiveForm = new FormData()
  invalidGraphArchiveForm.append('file', new Blob(['not an archive'], { type: 'application/gzip' }), `invalid-${graphArchiveRunId}.tar.gz`)
  invalidGraphArchiveForm.append('new_graph_id', `parity-archive-invalid-${graphArchiveRunId}`)
  const invalidGraphArchive = await fetchJsonAnyStatus(
    `${manifest.apiUrl}/graphs/import`,
    {
      method: 'POST',
      headers: authHeaders,
      body: invalidGraphArchiveForm,
    },
  )
  if (invalidGraphArchive.status !== 400) {
    throw new Error('POST /graphs/import did not reject non-gzip archives as 400')
  }
  const corruptGraphArchiveForm = new FormData()
  corruptGraphArchiveForm.append('file', new Blob([gzipSync(Buffer.from('not tar'))], { type: 'application/gzip' }), `corrupt-${graphArchiveRunId}.tar.gz`)
  corruptGraphArchiveForm.append('new_graph_id', `parity-archive-corrupt-${graphArchiveRunId}`)
  const corruptGraphArchive = await fetchJsonResponse(
    `${manifest.apiUrl}/graphs/import`,
    {
      method: 'POST',
      headers: authHeaders,
      body: corruptGraphArchiveForm,
    },
  )
  if (
    corruptGraphArchive.status !== 202
    || !corruptGraphArchive.body?.links?.status
  ) {
    throw new Error('POST /graphs/import did not return a hosted-shaped corrupt archive job envelope')
  }
  const corruptGraphArchiveStatus = await fetchJobStatusEventually(
    `${manifest.apiUrl}${corruptGraphArchive.body.links.status}`,
    { headers: authHeaders },
    240,
    500,
  )
  if (
    String(corruptGraphArchiveStatus.status ?? '') !== 'failed'
    || !String(corruptGraphArchiveStatus.detail?.message ?? corruptGraphArchiveStatus.error ?? '').includes('manifest.json')
  ) {
    throw new Error('POST /graphs/import did not preserve worker-style corrupt archive failure semantics')
  }
  await fetchJson(`${manifest.apiUrl}/graphs/${encodeURIComponent(archiveImportGraphId)}`, {
    method: 'DELETE',
    headers: authHeaders,
  })
  graphArchiveImport = {
    checked: true,
    graphId: archiveImportGraphId,
    jobId: graphArchiveJobId,
    documentsImported: graphArchiveResult.document_count,
    rdfTripleCount: graphArchiveResult.rdf_triple_count,
    invalidStatus: invalidGraphArchive.status,
    corruptStatus: corruptGraphArchiveStatus.status,
  }
  } else {
    graphArchiveImport = skipForCellProfile('graphArchiveImport: POST /graphs/import (graph.importArchive)')
  }
  const rdfImportTitle = `RDF Import Smoke ${Date.now()}`
  const rdfImportSubject = `urn:mnemosyne:parity:rdf-import:${Date.now()}`
  const rdfImportFilename = `parity-rdf-import-${Date.now()}.ttl`
  const rdfImportTurtle = `<${rdfImportSubject}> <http://purl.org/dc/terms/title> ${JSON.stringify(rdfImportTitle)} .\n`
  const rdfImportForm = new FormData()
  rdfImportForm.append('file', new Blob([rdfImportTurtle], { type: 'text/turtle' }), rdfImportFilename)
  const rdfImport = await timedStep('graphRdfImportMs', () => fetchJsonResponse(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/imports/rdf`,
    {
      method: 'POST',
      headers: authHeaders,
      body: rdfImportForm,
    },
  ))
  const rdfImportJobId = rdfImport.body?.jobId ?? rdfImport.body?.job_id
  if (
    rdfImport.status !== 202
    || !rdfImportJobId
    || !['queued', 'running', 'succeeded'].includes(String(rdfImport.body?.status ?? ''))
    || !rdfImport.body?.links?.result
  ) {
    throw new Error('POST /graphs/{graph_id}/imports/rdf did not return a hosted-shaped import job envelope')
  }
  const rdfImportResult = await fetchJson(`${manifest.apiUrl}${rdfImport.body.links.result}`, { headers: authHeaders })
  if (
    rdfImportResult.graph_id !== graphId
    || rdfImportResult.filename !== rdfImportFilename
    || rdfImportResult.mime_type !== 'text/turtle'
    || typeof rdfImportResult.quadCount !== 'number'
  ) {
    throw new Error('RDF import job result did not return the expected graph/file/mime envelope')
  }
  const rdfImportQuery = await fetchJson(`${manifest.apiUrl}/api/sparql/query`, {
    method: 'POST',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      graphId,
      query: `SELECT ?title WHERE { GRAPH ?userGraph { <${rdfImportSubject}> <http://purl.org/dc/terms/title> ?title . } }`,
    }),
  })
  const importedTitle = String(rdfImportQuery.rows?.[0]?.title ?? '').replace(/^"|"$/g, '')
  if (importedTitle !== rdfImportTitle) {
    throw new Error('RDF import route did not load the uploaded Turtle triples into the local graph store')
  }
  const invalidRdfImportForm = new FormData()
  invalidRdfImportForm.append('file', new Blob(['not rdf'], { type: 'text/plain' }), 'parity-rdf-import.txt')
  const invalidRdfImport = await fetchJsonAnyStatus(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/imports/rdf`,
    {
      method: 'POST',
      headers: authHeaders,
      body: invalidRdfImportForm,
    },
  )
  if (invalidRdfImport.status !== 400) {
    throw new Error('RDF import route did not reject an undetectable RDF format as 400')
  }
  graphRdfImport = {
    checked: true,
    graphId,
    jobId: rdfImportJobId,
    filename: rdfImportResult.filename,
    mimeType: rdfImportResult.mime_type,
    importedTitle,
    invalidStatus: invalidRdfImport.status,
  }

  async function submitArchiveImport(source, timingName, files, filename, folderName) {
    const archiveForm = new FormData()
    archiveForm.append('file', new Blob([storedZip(files)], { type: 'application/zip' }), filename)
    if (folderName) archiveForm.append('folder_name', folderName)
    const response = await timedStep(timingName, () => fetchJsonResponse(
      `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/imports/${source}`,
      {
        method: 'POST',
        headers: authHeaders,
        body: archiveForm,
      },
    ))
    const jobId = response.body?.jobId ?? response.body?.job_id
    if (
      response.status !== 202
      || !jobId
      || !response.body?.links?.result
      || !['queued', 'running', 'succeeded'].includes(String(response.body?.status ?? ''))
    ) {
      throw new Error(`POST /graphs/{graph_id}/imports/${source} did not return a hosted-shaped job envelope`)
    }
    const result = await fetchJobResultEventually(
      `${manifest.apiUrl}${response.body.links.result}`,
      { headers: authHeaders },
      360,
      500,
    )
    return { jobId, result }
  }

  if (!cellSkip('graphVaultImport')) {
  const archiveRunId = Date.now()
  const obsidianFolder = `Parity Obsidian ${archiveRunId}`
  const obsidianSourceTitle = `Archive Source ${archiveRunId}`
  const obsidianTargetTitle = `Archive Target ${archiveRunId}`
  const obsidianTag = `archive-tag-${archiveRunId}`
  const obsidianLooseTag = `loose-tag-${archiveRunId}`
  const obsidianMissingTitle = `Archive Missing ${archiveRunId}`
  const obsidianImport = await submitArchiveImport('obsidian', 'graphObsidianImportMs', {
    [`Vault/Projects/${obsidianSourceTitle}.md`]: `---\ntitle: ${obsidianSourceTitle}\ntags: [${obsidianTag}]\n---\n\nSee [[${obsidianTargetTitle}|target display]] and #${obsidianLooseTag}.\n\nAlso see [[${obsidianMissingTitle}]].\n`,
    [`Vault/${obsidianTargetTitle}.md`]: `# ${obsidianTargetTitle}\n\nTarget body.\n`,
  }, `obsidian-${archiveRunId}.zip`, obsidianFolder)
  if (
    obsidianImport.result.status !== 'complete'
    || obsidianImport.result.documentsCreated !== 4
    || obsidianImport.result.tagsCreated !== 2
    || obsidianImport.result.unresolvedLinks !== 1
    || obsidianImport.result.wiresCreated < 3
    || !obsidianImport.result.warnings.some((warning) => String(warning).includes(obsidianMissingTitle))
  ) {
    throw new Error('Obsidian archive import did not preserve document/tag/wire/unresolved-link semantics')
  }

  // DIAGNOSTIC: verify obsidian's wires are visible IMMEDIATELY after the
  // import completes, before notion/roam imports run. If they're visible
  // here but missing later, the loss happens between imports. If they're
  // missing here too, obsidian's persistence itself is broken.
  await callTool('flush_crdt', { graphId })
  const obsidianDocsAfterImport = await fetchJson(
    `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}`,
    { headers: authHeaders },
  )
  const obsidianSourceDocAfterImport = obsidianDocsAfterImport.find((d) => d.title === obsidianSourceTitle)
  console.log('[diag] post-obsidian: docs.count=', obsidianDocsAfterImport.length, ' source-doc-found?', Boolean(obsidianSourceDocAfterImport))
  if (obsidianSourceDocAfterImport) {
    const wiresImmediately = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(obsidianSourceDocAfterImport.id ?? obsidianSourceDocAfterImport.documentId)}/outgoing`,
      { headers: authHeaders },
    )
    console.log('[diag] post-obsidian: source-outgoing-wires=', Array.isArray(wiresImmediately) ? wiresImmediately.length : 'NOT-ARRAY')
  } else {
    console.log('[diag] post-obsidian: SOURCE DOC ALREADY MISSING from /documents listing')
  }

  const notionFolder = `Parity Notion ${archiveRunId}`
  const notionMainTitle = `Notion Main ${archiveRunId}`
  const notionChildTitle = `Notion Child ${archiveRunId}`
  const notionDatabaseTitle = `Notion Database ${archiveRunId}`
  const notionChildFile = `${notionChildTitle} bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.md`
  const notionImport = await submitArchiveImport('notion', 'graphNotionImportMs', {
    [`Export/${notionMainTitle} aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.md`]: `# ${notionMainTitle}\n\nSee [${notionChildTitle}](${encodeURIComponent(notionChildFile)}).\n`,
    [`Export/${notionChildFile}`]: `# ${notionChildTitle}\n\nChild body.\n`,
    [`Export/${notionDatabaseTitle} cccccccccccccccccccccccccccccccc.csv`]: 'Name,Status\nAlpha,Done\n',
  }, `notion-${archiveRunId}.zip`, notionFolder)
  if (
    notionImport.result.status !== 'complete'
    || notionImport.result.documentsCreated !== 3
    || notionImport.result.wiresCreated !== 1
    || notionImport.result.unresolvedLinks !== 0
  ) {
    throw new Error('Notion archive import did not preserve page/link/database semantics')
  }

  const roamFolder = `Parity Roam ${archiveRunId}`
  const roamProjectTitle = `Roam Project ${archiveRunId}`
  const roamTag = `roam-tag-${archiveRunId}`
  const roamImport = await submitArchiveImport('roam', 'graphRoamImportMs', {
    [`roam-${archiveRunId}.json`]: JSON.stringify([
      {
        title: 'May 1st, 2026',
        children: [
          {
            string: `{{[[TODO]]}} Work on [[${roamProjectTitle}]] #${roamTag}`,
            children: [{ string: 'Nested note' }],
          },
        ],
      },
      {
        title: roamProjectTitle,
        children: [{ string: 'Status:: active' }],
      },
    ]),
  }, `roam-${archiveRunId}.zip`, roamFolder)
  if (
    roamImport.result.status !== 'complete'
    || roamImport.result.documentsCreated !== 3
    || roamImport.result.tagsCreated !== 1
    || roamImport.result.wiresCreated < 2
    || roamImport.result.unresolvedLinks !== 0
  ) {
    throw new Error('Roam archive import did not preserve page/block/tag/wire semantics')
  }

  const importedDocuments = await fetchJson(`${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}`, {
    headers: authHeaders,
  })
  const documentByTitle = new Map(importedDocuments.map((document) => [document.title, document]))
  const obsidianSourceDoc = documentByTitle.get(obsidianSourceTitle)
  const notionDatabaseDoc = documentByTitle.get(notionDatabaseTitle)
  const roamDailyDoc = (importedDocuments ?? [])
    .filter((document) => (roamImport.result.documentIds ?? []).includes(document.id ?? document.documentId))
    .find((document) => document.title === 'May 1st, 2026')
  if (!obsidianSourceDoc || !notionDatabaseDoc || !roamDailyDoc) {
    throw new Error('Archive import documents were not visible through hosted document summaries')
  }
  const obsidianSourceRead = await fetchJson(
    `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(obsidianSourceDoc.id ?? obsidianSourceDoc.documentId)}`,
    { headers: authHeaders },
  )
  if (!(obsidianSourceRead.blocks ?? []).some((block) => String(block.content ?? '').includes('target display'))) {
    throw new Error('Obsidian import did not materialize readable Markdown content')
  }
  const notionDatabaseRead = await fetchJson(
    `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(notionDatabaseDoc.id ?? notionDatabaseDoc.documentId)}`,
    { headers: authHeaders },
  )
  if (!(notionDatabaseRead.blocks ?? []).some((block) => String(block.content ?? '').includes('Status'))) {
    throw new Error('Notion import did not convert CSV database content to a readable document')
  }
  const roamDailyRead = await fetchJson(
    `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(roamDailyDoc.id ?? roamDailyDoc.documentId)}`,
    { headers: authHeaders },
  )
  if (!(roamDailyRead.blocks ?? []).some((block) => String(block.content ?? '').includes('Work on'))) {
    throw new Error('Roam import did not convert nested blocks to readable content')
  }
  // A2 item B4 deferred projection flush off the request path; vault imports
  // create wires via CRDT, but the wire-projection materialization is deferred.
  // Force a flush before reading the outgoing wire list.
  await callTool('flush_crdt', { graphId })
  const obsidianOutgoing = await fetchJson(
    `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(obsidianSourceDoc.id ?? obsidianSourceDoc.documentId)}/outgoing`,
    { headers: authHeaders },
  )
  if (!Array.isArray(obsidianOutgoing) || obsidianOutgoing.length < 3) {
    throw new Error('Obsidian import did not create expected outgoing wikilink/tag wires')
  }

  const invalidArchiveForm = new FormData()
  invalidArchiveForm.append('file', new Blob(['not zip'], { type: 'application/zip' }), `invalid-${archiveRunId}.zip`)
  const invalidArchive = await fetchJsonResponse(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/imports/obsidian`,
    {
      method: 'POST',
      headers: authHeaders,
      body: invalidArchiveForm,
    },
  )
  const invalidArchiveResult = await fetchJobResultEventually(
    `${manifest.apiUrl}${invalidArchive.body.links.result}`,
    { headers: authHeaders },
    240,
    500,
  )
  if (
    invalidArchive.status !== 202
    || invalidArchiveResult.documentsCreated !== 0
    || !invalidArchiveResult.warnings.some((warning) => String(warning).includes('Invalid ZIP file'))
  ) {
    throw new Error('Archive import did not preserve invalid-ZIP worker-result semantics')
  }
  const invalidExtensionForm = new FormData()
  invalidExtensionForm.append('file', new Blob(['not zip'], { type: 'text/plain' }), `invalid-${archiveRunId}.txt`)
  const invalidExtension = await fetchJsonAnyStatus(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/imports/obsidian`,
    {
      method: 'POST',
      headers: authHeaders,
      body: invalidExtensionForm,
    },
  )
  if (invalidExtension.status !== 400) {
    throw new Error('Archive import route did not reject non-ZIP uploads as 400')
  }

  const archiveImportedDocumentIds = [
    ...(obsidianImport.result.documentIds ?? []),
    ...(notionImport.result.documentIds ?? []),
    ...(roamImport.result.documentIds ?? []),
  ]
  const archiveWireIds = new Set()
  for (const documentId of archiveImportedDocumentIds) {
    for (const direction of ['outgoing', 'incoming']) {
      const wires = await fetchJson(
        `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/${direction}`,
        { headers: authHeaders },
      )
      for (const wire of Array.isArray(wires) ? wires : []) {
        const wireId = wire.id ?? wire.wireId ?? wire.wire_id
        if (wireId) archiveWireIds.add(wireId)
      }
    }
  }
  // A2 item B4 deferred projection flush off the request path; the wires we
  // just enumerated via GET (projection-backed) may not be visible inside the
  // workspace transact that DELETE's handler opens. Force a flush so the
  // delete enqueue sees the same Y.Doc state the GET returned.
  await callTool('flush_crdt', { graphId })
  for (const wireId of archiveWireIds) {
    await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/${encodeURIComponent(wireId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
  }
  const workspaceAfterArchiveImports = await callTool('get_workspace', { graphId, depth: 0 })
  const rootFolders = new Set([obsidianFolder, notionFolder, roamFolder])
  const archiveRootFolderIds = (workspaceAfterArchiveImports.folders ?? [])
    .filter((folder) => rootFolders.has(String(folder.name ?? folder.label ?? '')))
    .map((folder) => folder.id ?? folder.folderId)
    .filter(Boolean)
  for (const folderId of archiveRootFolderIds) {
    await fetchJson(
      `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/folder/${encodeURIComponent(folderId)}?cascade=true`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
  }
  graphVaultImport = {
    checked: true,
    graphId,
    obsidian: {
      jobId: obsidianImport.jobId,
      documentsCreated: obsidianImport.result.documentsCreated,
      tagsCreated: obsidianImport.result.tagsCreated,
      wiresCreated: obsidianImport.result.wiresCreated,
      unresolvedLinks: obsidianImport.result.unresolvedLinks,
    },
    notion: {
      jobId: notionImport.jobId,
      documentsCreated: notionImport.result.documentsCreated,
      wiresCreated: notionImport.result.wiresCreated,
    },
    roam: {
      jobId: roamImport.jobId,
      documentsCreated: roamImport.result.documentsCreated,
      tagsCreated: roamImport.result.tagsCreated,
      wiresCreated: roamImport.result.wiresCreated,
    },
    invalidZipWarnings: invalidArchiveResult.warnings.length,
    invalidExtensionStatus: invalidExtension.status,
    cleanupWires: archiveWireIds.size,
    cleanupFolders: archiveRootFolderIds.length,
  }
  } else {
    graphVaultImport = skipForCellProfile('graphVaultImport: POST /graphs/{graph_id}/imports/{obsidian,notion,roam} (import.vault)')
  }

  const graphWebClipPrivateBlock = await withLocalHttpFixture({
    '/article': (_request, response) => {
      response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
      response.end('<!doctype html><title>Blocked Fixture</title>')
    },
  }, async (baseUrl) => {
    const clipResponse = await timedStep('graphWebClipImportMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/imports/clip`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({ url: `${baseUrl}/article` }),
      },
    ))
    const clipError = String(clipResponse.body?.errors?.[0] ?? '')
    if (
      clipResponse.status !== 200
      || clipResponse.body?.status !== 'error'
      || !clipError.includes('Local/private network URLs are not allowed')
    ) {
      throw new Error(`Web clip import did not reject private/local URLs with hosted ImportResponse semantics: ${JSON.stringify(clipResponse.body)}`)
    }
    return { status: clipResponse.body.status, error: clipError }
  })

  const invalidYoutube = await timedStep('graphYoutubeInvalidImportMs', () => fetchJsonResponse(
    `${manifest.apiUrl}/graphs/${encodeURIComponent(graphId)}/imports/youtube`,
    {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ url: 'not a youtube url' }),
    },
  ))
  if (
    invalidYoutube.status !== 200
    || invalidYoutube.body?.status !== 'error'
    || !String(invalidYoutube.body?.errors?.[0] ?? '').includes('Invalid YouTube URL or video ID')
  ) {
    throw new Error('YouTube import did not preserve deterministic invalid URL ImportResponse semantics')
  }
  graphWebImports = {
    checked: true,
    graphId,
    clipPrivateBlockStatus: graphWebClipPrivateBlock.status,
    clipPrivateBlockError: graphWebClipPrivateBlock.error,
    youtubeInvalidStatus: invalidYoutube.body.status,
  }

  // A2 item B4 deferred projection flush off the request path; imports return
  // before the workspace projection settles. Force a flush and refresh the
  // workspace snapshot before asserting on counts.
  await callTool('flush_crdt', { graphId })
  workspace = await callTool('get_workspace', { graphId, depth: 2 })
  const rematerialize = await timedStep('searchRematerializeMs', () => fetchJson(
    `${manifest.apiUrl}/search/rematerialize`,
    {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ graph_id: graphId, reindex_after: false }),
    },
  ))
  if (
    rematerialize.graph_id !== graphId
    || rematerialize.total_docs !== (workspace.counts?.documents ?? 0)
    || rematerialize.materialized !== rematerialize.total_docs
    || rematerialize.errors !== 0
  ) {
    throw new Error('POST /search/rematerialize returned an invalid rematerialization envelope')
  }
  const semanticModelStatus = await fetchJson(`${manifest.apiUrl}/api/semantic/model/status`, {
    headers: authHeaders,
  })
  const reindex = await timedStep('searchReindexMs', () => fetchJsonAnyStatus(
    `${manifest.apiUrl}/search/reindex`,
    {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ graph_id: graphId }),
    },
  ))
  const semanticSetupRequired = Boolean(semanticModelStatus.setupRequired ?? semanticModelStatus.setup_required)
  if (semanticSetupRequired) {
    if (reindex.status !== 503 || !String(reindex.body?.error ?? '').includes('embedding model is not prepared')) {
      throw new Error('POST /search/reindex did not return the expected setup-required response')
    }
  } else if (
    reindex.status !== 200
    || reindex.body?.graph_id !== graphId
    || reindex.body?.total_docs !== (workspace.counts?.documents ?? 0)
    || typeof reindex.body?.queued !== 'number'
  ) {
    throw new Error('POST /search/reindex returned an invalid reindex envelope')
  }
  searchMaintenance = {
    checked: true,
    graphId,
    totalDocs: rematerialize.total_docs,
    materialized: rematerialize.materialized,
    reindexQueued: rematerialize.reindex_queued,
    reindexStatus: reindex.status,
    reindexSetupRequired: semanticSetupRequired,
    reindexTotalDocs: reindex.body?.total_docs ?? null,
    reindexQueuedDocs: reindex.body?.queued ?? null,
  }

  const memoryGraphId = `parity-memory-${Date.now()}`
  await callTool('create_graph', {
    graph_id: memoryGraphId,
    title: 'Parity Memory Adapters',
  })
  const memoryAlpha = `Parity memory alpha ${Date.now()}`
  const memoryBeta = `Parity memory beta ${Date.now()}`
  const memoryGamma = `Parity memory gamma ${Date.now()}`
  const rememberedAlpha = await timedStep('mcpRememberMs', () => callTool('remember', {
    graphId: memoryGraphId,
    content: memoryAlpha,
  }))
  const rememberedBeta = await callTool('remember', {
    graph_id: memoryGraphId,
    content: memoryBeta,
  })
  const rememberedGamma = await callTool('remember', {
    graphId: memoryGraphId,
    content: memoryGamma,
  })
  if (
    rememberedAlpha.number < 1
    || rememberedBeta.number !== rememberedAlpha.number + 1
    || rememberedGamma.number !== rememberedBeta.number + 1
    || !rememberedAlpha.block_id
  ) {
    throw new Error('remember did not assign sequential memory numbers')
  }

  const recallLatest = await timedStep('mcpRecallMs', () => callTool('recall', {
    graphId: memoryGraphId,
    limit: 2,
  }))
  if (
    recallLatest.count !== 2
    || !(recallLatest.memories ?? []).some((memory) => String(memory.text ?? '').includes(memoryGamma))
  ) {
    throw new Error('recall did not return the latest local memories')
  }
  const recallAlphaQuery = await callTool('recall', {
    graph_id: memoryGraphId,
    query: memoryAlpha,
    limit: 5,
  })
  if (
    recallAlphaQuery.count !== 1
    || !String(recallAlphaQuery.memories?.[0]?.text ?? '').includes(memoryAlpha)
  ) {
    throw new Error('recall query did not search only the local memory queue')
  }

  const caredAlpha = await timedStep('mcpCareMs', () => callTool('care', {
    graphId: memoryGraphId,
    numbers: [rememberedAlpha.number],
  }))
  if (!Array.isArray(caredAlpha.cared) || caredAlpha.cared[0] !== rememberedAlpha.number) {
    throw new Error('care did not update the requested memory number')
  }
  const recallAfterCare = await callTool('recall', {
    graphId: memoryGraphId,
    limit: 1,
  })
  if (!String(recallAfterCare.memories?.[0]?.text ?? '').includes(memoryAlpha)) {
    throw new Error('care did not move the memory into the active recall window')
  }

  const archivedMemories = await timedStep('mcpArchiveMemoriesMs', () => callTool('archive_memories', {
    graphId: memoryGraphId,
    keep: 2,
  }))
  if (
    archivedMemories.kept !== 2
    || archivedMemories.archived !== 1
    || !archivedMemories.archive_doc_id
  ) {
    throw new Error('archive_memories did not archive the oldest memory and keep two active records')
  }
  const recallKeptAlpha = await callTool('recall', {
    graphId: memoryGraphId,
    number: rememberedAlpha.number,
  })
  if (!String(recallKeptAlpha.memories?.[0]?.text ?? '').includes(memoryAlpha)) {
    throw new Error('archive_memories did not preserve the cared memory')
  }
  const recallRemaining = await callTool('recall', {
    graphId: memoryGraphId,
    limit: 10,
  })
  if (recallRemaining.count !== 2) {
    throw new Error('archive_memories left an unexpected live memory count')
  }
  // A2 item B4 deferred projection flush off the request path; archive_memories
  // writes the archive document via CRDT and the RDF projection may not yet be
  // settled. Force a flush before reading the archive document and RDF counts.
  await callTool('flush_crdt', { graphId: memoryGraphId })
  const archiveDocument = await fetchJson(
    `${manifest.apiUrl}/documents/${encodeURIComponent(memoryGraphId)}/${encodeURIComponent(archivedMemories.archive_doc_id)}`,
    { headers: authHeaders },
  )
  if (
    archiveDocument.id !== archivedMemories.archive_doc_id
    || !(archiveDocument.blocks ?? []).some((block) => String(block.content ?? '').includes('Archived 1 memories'))
  ) {
    throw new Error('archive_memories did not create a readable archive document')
  }
  const memoryRdf = await fetchJson(`${manifest.apiUrl}/api/sparql/query`, {
    method: 'POST',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      graphId: memoryGraphId,
      query: `PREFIX mnemo: <https://mnemosyne.local/ns#>
SELECT ?memory ?number WHERE {
  ?memory a mnemo:Memory ;
    mnemo:memoryNumber ?number .
}`,
    }),
  })
  if (!Array.isArray(memoryRdf.rows) || memoryRdf.rows.length !== 2) {
    throw new Error('memory queue did not materialize live memories to RDF')
  }
  mcpMemoryAdapters = {
    checked: true,
    graphId: memoryGraphId,
    firstNumber: rememberedAlpha.number,
    liveCount: recallRemaining.count,
    archived: archivedMemories.archived,
    archiveDocumentId: archivedMemories.archive_doc_id,
    rdfRows: memoryRdf.rows.length,
  }

  const songGraphId = `parity-song-${Date.now()}`
  await callTool('create_graph', {
    graph_id: songGraphId,
    title: 'Parity Song Adapters',
  })
  const initialSong = await timedStep('mcpMusicMs', () => callTool('music', { graphId: songGraphId }))
  if (
    initialSong.source !== 'local-song-store'
    || !Array.isArray(initialSong.verses)
    || initialSong.active_verse_count < 1
  ) {
    throw new Error('music did not return the local Song narrative')
  }
  const codaText = `Parity coda ${Date.now()}`
  const codaResult = await timedStep('mcpSingMs', () => callTool('sing', {
    graphId: songGraphId,
    mode: 'coda',
    verse: codaText,
  }))
  if (codaResult.coda_set !== true || codaResult.ejections_remaining !== 8) {
    throw new Error('sing coda did not install an eight-ejection coda')
  }
  const verseOne = `Parity verse one ${Date.now()}\nline two /`
  const verseTwo = `Parity verse two ${Date.now()}`
  const verseThree = `Parity verse three ${Date.now()}`
  await callTool('sing', { graphId: songGraphId, mode: 'verse', verse: verseOne })
  const counterpointResult = await callTool('sing', {
    graphId: songGraphId,
    mode: 'counterpoint',
    verse_index: 0,
    verse: `Parity counterpoint ${Date.now()}`,
  })
  if (counterpointResult.total_voices !== 2 || counterpointResult.verse_index !== 0) {
    throw new Error('sing counterpoint did not add a second voice to verse 0')
  }
  await callTool('sing', { graphId: songGraphId, mode: 'verse', verse: verseTwo })
  const ejectedVerse = await callTool('sing', { graphId: songGraphId, mode: 'verse', verse: verseThree })
  if (
    ejectedVerse.verse_count !== 3
    || ejectedVerse.ejected !== 1
    || ejectedVerse.coda_ejections_remaining !== 7
    || ejectedVerse.archive_doc_id !== 'geist-past-songs'
  ) {
    throw new Error('sing did not keep a three-verse rolling window with coda ejection accounting')
  }
  const songAfterSing = await callTool('music', { graph_id: songGraphId })
  if (
    songAfterSing.active_verse_count !== 3
    || songAfterSing.coda !== codaText
    || !(songAfterSing.counterpoint_parts ?? []).includes(1)
    || !(songAfterSing.verses ?? []).some((verse) => String(verse).includes(verseThree))
  ) {
    throw new Error('music did not reflect verses, counterpoints, and coda after sing')
  }
  // A2 item B4 deferred projection flush off the request path; sing writes the
  // ejected verse archive document via CRDT and the RDF projection may not yet
  // have settled. Force a flush before reading the archive document and RDF rows.
  await callTool('flush_crdt', { graphId: songGraphId })
  const songArchiveDocument = await fetchJson(
    `${manifest.apiUrl}/documents/${encodeURIComponent(songGraphId)}/geist-past-songs`,
    { headers: authHeaders },
  )
  if (
    songArchiveDocument.id !== 'geist-past-songs'
    || !(songArchiveDocument.blocks ?? []).some((block) => String(block.content ?? '').includes('Archived'))
  ) {
    throw new Error('sing did not materialize the ejected Song verse archive')
  }
  const surfaceResult = await timedStep('mcpSurfaceMs', () => callTool('surface', {
    graphId: songGraphId,
    actions: [
      { document_id: 'geist-song', block_id: 'song-verse-0', action: 'updated song' },
      { documentId: 'missing-document-for-surface', action: 'referenced missing fallback' },
    ],
  }))
  if (
    surfaceResult.type !== 'surface'
    || surfaceResult.actions?.[0]?.title !== 'The Song'
    || surfaceResult.actions?.[0]?.block_id !== 'song-verse-0'
    || surfaceResult.actions?.[1]?.title !== 'missing-document-for-surface'
  ) {
    throw new Error('surface did not resolve local document titles and block references')
  }
  const songRdf = await fetchJson(`${manifest.apiUrl}/api/sparql/query`, {
    method: 'POST',
    headers: { ...authHeaders, 'Content-Type': 'application/json' },
    body: JSON.stringify({
      graphId: songGraphId,
      query: `PREFIX mnemo: <https://mnemosyne.local/ns#>
SELECT ?verse ?voiceCount WHERE {
  ?verse a mnemo:SongVerse ;
    mnemo:voiceCount ?voiceCount .
}`,
    }),
  })
  if (!Array.isArray(songRdf.rows) || songRdf.rows.length !== 3) {
    throw new Error('Song verses did not materialize to RDF')
  }
  mcpNarrativeSurface = {
    checked: true,
    graphId: songGraphId,
    initialActiveVerses: initialSong.active_verse_count,
    activeVerses: songAfterSing.active_verse_count,
    codaRemaining: songAfterSing.coda_ejections_remaining,
    archiveDocumentId: ejectedVerse.archive_doc_id,
    surfaceActions: surfaceResult.actions.length,
    rdfRows: songRdf.rows.length,
  }

  const navigationFolderId = `parity-folder-${Date.now()}`
  const navigationFolderLabel = 'Parity Folder Smoke'
  const folderPut = await timedStep('navigationFolderPutMs', () => fetchJson(
    `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(navigationFolderId)}`,
    {
      method: 'PUT',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        label: navigationFolderLabel,
        parentId: null,
        section: 'documents',
        order: Date.now(),
      }),
    },
  ))
  if (
    folderPut.id !== navigationFolderId
    || folderPut.graphId !== graphId
    || folderPut.label !== navigationFolderLabel
  ) {
    throw new Error('PUT /navigation/{graph_id}/folders/{folder_id} returned an invalid folder envelope')
  }
  // A2 item B4 deferred projection flush off the request path; the folder PUT
  // CRDT mutation may not yet be visible in the projection-backed GET read.
  // Force a flush before asserting on the returned state.
  await callTool('flush_crdt', { graphId })
  const folderRead = await fetchJson(
    `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(navigationFolderId)}`,
    { headers: authHeaders },
  )
  if (folderRead.id !== navigationFolderId || folderRead.label !== navigationFolderLabel) {
    throw new Error('GET /navigation/{graph_id}/folders/{folder_id} did not return the folder just written')
  }
  const folderDelete = await timedStep('navigationFolderDeleteMs', () => fetchJson(
    `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(navigationFolderId)}`,
    {
      method: 'DELETE',
      headers: authHeaders,
    },
  ))
  if (folderDelete.id !== navigationFolderId || folderDelete.status !== 'deleted') {
    throw new Error('DELETE /navigation/{graph_id}/folders/{folder_id} did not delete the folder just written')
  }
  navigationFolder = {
    checked: true,
    graphId,
    folderId: navigationFolderId,
    label: navigationFolderLabel,
  }

  const entityTypes = await timedStep('entityTypesMs', () => fetchJson(
    `${manifest.apiUrl}/entities/entity-types`,
    { headers: authHeaders },
  ))
  const entityTypeById = Object.fromEntries((entityTypes.types ?? []).map((type) => [type.id, type]))
  if (
    entityTypeById.document?.rdf_type !== 'http://mnemosyne.dev/doc#Document'
    || entityTypeById.document?.cascade_policy !== 'automatic'
    || entityTypeById.document?.supports_websocket !== true
    || entityTypeById.folder?.cascade_policy !== 'block'
    || entityTypeById.artifact?.cascade_policy !== 'none'
  ) {
    throw new Error('GET /entities/entity-types did not return the hosted entity registry metadata')
  }
  const invalidEntityType = await fetchJsonAnyStatus(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/philosopher/missing`,
    { headers: authHeaders },
  )
  if (invalidEntityType.status !== 404) {
    throw new Error('GET /entities/{graph_id}/{entity_type}/{entity_id} did not reject unknown entity types')
  }
  const invalidEntityFolderId = `parity-entity-invalid-folder-${Date.now()}`
  const invalidEntityFolder = await fetchJsonAnyStatus(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/folder/${encodeURIComponent(invalidEntityFolderId)}`,
    {
      method: 'PUT',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ section: 'documents' }),
    },
  )
  if (invalidEntityFolder.status !== 422) {
    throw new Error('PUT /entities folder without label did not preserve hosted validation semantics')
  }
  const entityFolderId = `parity-entity-folder-${Date.now()}`
  const entityArtifactId = `parity-entity-artifact-${Date.now()}`
  const entityDocumentId = `parity-entity-document-${Date.now()}`
  const entityDocumentToken = `entity-route-token-${Date.now()}`
  const entityFolder = await timedStep('entityFolderPutMs', () => fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/folder/${encodeURIComponent(entityFolderId)}`,
    {
      method: 'PUT',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        label: 'Parity Entity Folder',
        parentId: null,
        section: 'documents',
        order: Date.now(),
      }),
    },
  ))
  if (
    entityFolder.entityType !== 'folder'
    || entityFolder.id !== entityFolderId
    || entityFolder.graphId !== graphId
    || entityFolder.label !== 'Parity Entity Folder'
  ) {
    throw new Error('PUT /entities/{graph_id}/folder/{entity_id} returned an invalid folder entity')
  }
  // A2 item B4 deferred projection flush off the request path; entity PUT
  // mutations may not yet be materialized into the projection-backed GET reads
  // that follow. Force a flush before asserting on the returned state.
  await callTool('flush_crdt', { graphId })
  const entityFolderRead = await fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/folder/${encodeURIComponent(entityFolderId)}`,
    { headers: authHeaders },
  )
  if (entityFolderRead.id !== entityFolderId || entityFolderRead.label !== entityFolder.label) {
    throw new Error('GET /entities folder did not return the folder just written')
  }
  const entityArtifact = await timedStep('entityArtifactPutMs', () => fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/artifact/${encodeURIComponent(entityArtifactId)}`,
    {
      method: 'PUT',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        label: 'Parity Entity Artifact',
        parentId: entityFolderId,
        order: Date.now(),
        fileType: 'txt',
        status: 'ready',
        originalFilename: 'parity-entity-artifact.txt',
        mimeType: 'text/plain',
        sizeBytes: 24,
      }),
    },
  ))
  if (
    entityArtifact.entityType !== 'artifact'
    || entityArtifact.id !== entityArtifactId
    || entityArtifact.graphId !== graphId
    || entityArtifact.originalFilename !== 'parity-entity-artifact.txt'
    || entityArtifact.status !== 'ready'
  ) {
    throw new Error('PUT /entities/{graph_id}/artifact/{entity_id} returned an invalid artifact entity')
  }
  const entityArtifactRead = await fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/artifact/${encodeURIComponent(entityArtifactId)}`,
    { headers: authHeaders },
  )
  if (entityArtifactRead.id !== entityArtifactId || entityArtifactRead.label !== entityArtifact.label) {
    throw new Error('GET /entities artifact did not return the artifact just written')
  }
  const entityDocument = await timedStep('entityDocumentPutMs', () => fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/document/${encodeURIComponent(entityDocumentId)}`,
    {
      method: 'PUT',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        title: 'Parity Entity Document',
        parentId: entityFolderId,
        expectedRevision: 0,
        blocks: [
          {
            id: `${entityDocumentId}-b1`,
            type: 'paragraph',
            content: `Generic entity route wrote this block. ${entityDocumentToken}`,
            parentId: null,
            order: 0,
            marks: [],
          },
        ],
      }),
    },
  ))
  if (
    entityDocument.entityType !== 'document'
    || entityDocument.id !== entityDocumentId
    || entityDocument.graphId !== graphId
    || entityDocument.parentId !== entityFolderId
    || !(entityDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(entityDocumentToken))
  ) {
    throw new Error('PUT /entities/{graph_id}/document/{entity_id} returned an invalid document entity')
  }
  // B4 deferred projection flush: artifact and document PUTs above didn't
  // each trigger their own flush_crdt; force one before the list reads so
  // every just-written entity is materialized into the projection.
  await callTool('flush_crdt', { graphId })
  const entityDocumentList = await timedStep('entityDocumentListMs', () => fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/document?limit=500&offset=0`,
    { headers: authHeaders },
  ))
  if (
    !Array.isArray(entityDocumentList.data)
    || entityDocumentList.meta?.entity_type !== 'document'
    || entityDocumentList.meta?.limit !== 500
    || !entityDocumentList.data.some((document) => document.id === entityDocumentId)
  ) {
    throw new Error('GET /entities/{graph_id}/document did not return a hosted paginated entity list')
  }
  const entityFolderList = await fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/folder?limit=500&offset=0`,
    { headers: authHeaders },
  )
  const entityArtifactList = await fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/artifact?limit=500&offset=0`,
    { headers: authHeaders },
  )
  if (!entityFolderList.data?.some((folder) => folder.id === entityFolderId)) {
    throw new Error(`GET /entities folder list did not include just-written folder ${entityFolderId} (got ${entityFolderList.data?.length ?? 'no data'} entries)`)
  }
  if (!entityArtifactList.data?.some((artifact) => artifact.id === entityArtifactId)) {
    throw new Error(`GET /entities artifact list did not include just-written artifact ${entityArtifactId} (got ${entityArtifactList.data?.length ?? 'no data'} entries)`)
  }
  const entityDocumentRead = await timedStep('entityDocumentGetMs', () => fetchJson(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/document/${encodeURIComponent(entityDocumentId)}`,
    { headers: authHeaders },
  ))
  if (
    entityDocumentRead.id !== entityDocumentId
    || entityDocumentRead.title !== 'Parity Entity Document'
    || !(entityDocumentRead.blocks ?? []).some((block) => String(block.content ?? '').includes(entityDocumentToken))
  ) {
    throw new Error('GET /entities document did not return the document just written')
  }
  const entityRevisionConflict = await timedStep('entityRevisionConflictMs', () => fetchJsonAnyStatus(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/document/${encodeURIComponent(entityDocumentId)}`,
    {
      method: 'PUT',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        title: 'Parity Entity Document Conflict',
        expectedRevision: 0,
        blocks: [],
      }),
    },
  ))
  if (entityRevisionConflict.status !== 409) {
    throw new Error('PUT /entities document did not preserve expectedRevision conflict semantics')
  }
  const entityBlockedFolderDelete = await timedStep('entityFolderBlockedDeleteMs', () => fetchJsonAnyStatus(
    `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/folder/${encodeURIComponent(entityFolderId)}`,
    {
      method: 'DELETE',
      headers: authHeaders,
    },
  ))
  if (
    entityBlockedFolderDelete.status !== 409
    || !JSON.stringify(entityBlockedFolderDelete.body ?? '').includes('cascade=true')
  ) {
    throw new Error('DELETE /entities folder without cascade did not reject a non-empty folder')
  }
  const entityDeletes = await timedStep('entityDeleteMs', async () => {
    const deletedDocument = await fetchJson(
      `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/document/${encodeURIComponent(entityDocumentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    const deletedArtifact = await fetchJson(
      `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/artifact/${encodeURIComponent(entityArtifactId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    const deletedFolder = await fetchJson(
      `${manifest.apiUrl}/entities/${encodeURIComponent(graphId)}/folder/${encodeURIComponent(entityFolderId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    return { deletedDocument, deletedArtifact, deletedFolder }
  })
  if (
    entityDeletes.deletedDocument.status !== 'deleted'
    || entityDeletes.deletedDocument.entityType !== 'document'
    || entityDeletes.deletedArtifact.status !== 'deleted'
    || entityDeletes.deletedArtifact.entityType !== 'artifact'
    || entityDeletes.deletedFolder.status !== 'deleted'
    || entityDeletes.deletedFolder.entityType !== 'folder'
  ) {
    throw new Error('DELETE /entities did not return hosted entity delete envelopes')
  }
  entityAliases = {
    checked: true,
    graphId,
    documentId: entityDocumentId,
    folderId: entityFolderId,
    artifactId: entityArtifactId,
    documentRevision: entityDocumentRead.revision,
    documentListTotal: entityDocumentList.meta.total,
    folderListTotal: entityFolderList.meta.total,
    artifactListTotal: entityArtifactList.meta.total,
    revisionConflictStatus: entityRevisionConflict.status,
    blockedDeleteStatus: entityBlockedFolderDelete.status,
  }

  const firstDocument = (workspace.documents ?? [])
    .find((document) => document.documentId || document.document_id || document.id)
  if (firstDocument) {
    const documentId = firstDocument.documentId ?? firstDocument.document_id ?? firstDocument.id
    const digest = await callTool('document_digest', { graphId, documentId })
    if (digest.metadata?.documentId !== documentId && digest.metadata?.document_id !== documentId) {
      throw new Error(`document_digest returned wrong document id for ${documentId}`)
    }

    const blocks = await callTool('read_blocks', { graphId, documentId, limit: 1, includeIds: true })
    const firstBlock = blocks.blocks?.[0]
    if (firstBlock?.blockId || firstBlock?.block_id) {
      const blockId = firstBlock.blockId ?? firstBlock.block_id
      const block = await callTool('get_block', { graphId, documentId, blockId, format: 'text' })
      const returnedBlockId = block.block?.blockId ?? block.block?.block_id
      if (returnedBlockId !== blockId) {
        throw new Error(`get_block returned wrong block id: expected ${blockId}, got ${returnedBlockId}`)
      }
      await callTool('query_blocks', {
        graphId,
        documentId,
        blockType: block.block?.blockType ?? block.block?.block_type ?? firstBlock.blockType ?? firstBlock.block_type,
        limit: 1,
      })
    }

    const predicates = await callTool('list_wire_predicates', { graphId })
    if (!Array.isArray(predicates.predicates) || predicates.predicates.length < 1) {
      throw new Error('list_wire_predicates returned no predicates')
    }

    const wires = await callTool('get_wires', { graphId, documentId, direction: 'both' })
    if (!Array.isArray(wires.wires)) {
      throw new Error('get_wires did not return a wires array')
    }

    const hostedDocuments = await fetchJson(`${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}`, {
      headers: authHeaders,
    })
    if (!Array.isArray(hostedDocuments)) {
      throw new Error('GET /documents/{graph_id} did not return an array')
    }
    const hostedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}`,
      { headers: authHeaders },
    )
    if (hostedDocument.id !== documentId || !Array.isArray(hostedDocument.blocks)) {
      throw new Error('GET /documents/{graph_id}/{document_id} returned an invalid document envelope')
    }
    const exportProbeText = hostedDocument.blocks
      .map((block) => String(block.content ?? '').trim())
      .find((content) => content.length >= 12)
    const exportedMarkdown = await timedStep('documentExportMarkdownMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/export?format=markdown`,
      { headers: authHeaders },
    ))
    if (typeof exportedMarkdown.body !== 'string' || exportedMarkdown.body.length < 1) {
      throw new Error('GET /documents/{graph_id}/{document_id}/export?format=markdown returned empty content')
    }
    if (exportProbeText && !exportedMarkdown.body.includes(exportProbeText.slice(0, 24))) {
      throw new Error('GET /documents/{graph_id}/{document_id}/export?format=markdown did not include projected block content')
    }
    const exportedXml = await fetchJsonResponse(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/export?format=xml`,
      { headers: authHeaders },
    )
    if (typeof exportedXml.body !== 'string' || !exportedXml.body.includes('<')) {
      throw new Error('GET /documents/{graph_id}/{document_id}/export?format=xml returned invalid XML content')
    }
    const exportedHtml = await fetchJsonResponse(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/export?format=html&theme=garden`,
      { headers: authHeaders },
    )
    if (typeof exportedHtml.body !== 'string' || !/<html|<!doctype html/i.test(exportedHtml.body)) {
      throw new Error('GET /documents/{graph_id}/{document_id}/export?format=html returned invalid HTML content')
    }
    documentExport = {
      checked: true,
      graphId,
      documentId,
      markdownBytes: Buffer.byteLength(exportedMarkdown.body),
      xmlBytes: Buffer.byteLength(exportedXml.body),
      htmlBytes: Buffer.byteLength(exportedHtml.body),
    }
    const workspaceBlob = await timedStep('workspaceBlobMs', () => fetchBytesResponse(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/workspace/blob`,
      { headers: authHeaders },
    ))
    const documentBlob = await timedStep('documentBlobMs', () => fetchBytesResponse(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/blob`,
      { headers: authHeaders },
    ))
    if (
      workspaceBlob.bytes.length < 1
      || workspaceBlob.headers.get('content-type') !== 'application/octet-stream'
    ) {
      throw new Error('GET /documents/{graph_id}/workspace/blob returned an invalid binary blob')
    }
    if (
      documentBlob.bytes.length < 1
      || documentBlob.headers.get('content-type') !== 'application/octet-stream'
    ) {
      throw new Error('GET /documents/{graph_id}/{document_id}/blob returned an invalid binary blob')
    }
    documentBlobs = {
      checked: true,
      graphId,
      documentId,
      workspaceBytes: workspaceBlob.bytes.length,
      documentBytes: documentBlob.bytes.length,
      workspaceSource: workspaceBlob.headers.get('x-blob-source'),
      documentSource: documentBlob.headers.get('x-blob-source'),
    }
    const duplicateSubmit = await timedStep('documentDuplicateMs', () => fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/duplicate`,
      {
        method: 'POST',
        headers: authHeaders,
      },
    ))
    const duplicateJobId = duplicateSubmit.job_id ?? duplicateSubmit.jobId
    if (!duplicateJobId || !duplicateSubmit.detail || !duplicateSubmit.links) {
      throw new Error('POST /documents/{graph_id}/{document_id}/duplicate returned an invalid job envelope')
    }
    const duplicateResult = await fetchJson(
      `${manifest.apiUrl}/graphs/jobs/${encodeURIComponent(duplicateJobId)}/result`,
      { headers: authHeaders },
    )
    const duplicateDocumentId = duplicateResult.documentId ?? duplicateResult.document_id
    if (
      !duplicateDocumentId
      || (duplicateResult.sourceDocumentId ?? duplicateResult.source_document_id) !== documentId
      || !String(duplicateResult.title ?? '').startsWith('Copy of ')
    ) {
      throw new Error('document duplicate job result was invalid')
    }
    const duplicateDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(duplicateDocumentId)}`,
      { headers: authHeaders },
    )
    if (
      duplicateDocument.id !== duplicateDocumentId
      || !Array.isArray(duplicateDocument.blocks)
      || duplicateDocument.blocks.length < hostedDocument.blocks.length
    ) {
      throw new Error('duplicated document was not readable through the hosted document alias')
    }
    const descriptionText = `Parity description smoke ${duplicateDocumentId}`
    const descriptionPatch = await timedStep('documentDescriptionPatchMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(duplicateDocumentId)}/description`,
      {
        method: 'PATCH',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({ description: descriptionText }),
      },
    ))
    if (descriptionPatch.status !== 204) {
      throw new Error('PATCH /documents/{graph_id}/{doc_id}/description did not return 204')
    }
    // A2 item B4 deferred projection flush off the request path; the description
    // PATCH writes via CRDT and the RDF projection may not yet include the new
    // triple. Force a flush before the SPARQL assertion.
    await callTool('flush_crdt', { graphId })
    const descriptionQuery = await fetchJson(`${manifest.apiUrl}/api/sparql/query`, {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        graphId,
        query: `SELECT ?description WHERE {
  GRAPH ?projectionGraph {
    <urn:mnemosyne:local:document:${duplicateDocumentId}> <http://purl.org/dc/terms/description> ?description .
  }
}`,
      }),
    })
    if (
      !Array.isArray(descriptionQuery.rows)
      || !descriptionQuery.rows.some((row) => String(row.description ?? '').includes(descriptionText))
    ) {
      throw new Error('PATCH /documents/{graph_id}/{doc_id}/description was not materialized to RDF')
    }
    documentDescription = {
      checked: true,
      graphId,
      documentId: duplicateDocumentId,
      rdfRows: descriptionQuery.rows.length,
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(duplicateDocumentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    documentDuplicate = {
      checked: true,
      graphId,
      sourceDocumentId: documentId,
      documentId: duplicateDocumentId,
      jobId: duplicateJobId,
      blockCount: duplicateDocument.blocks.length,
    }
    const flushDocument = await timedStep('documentFlushMs', () => fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/flush?include_materialization=false`,
      {
        method: 'POST',
        headers: authHeaders,
      },
    ))
    if (
      flushDocument.id !== documentId
      || flushDocument.graphId !== graphId
      || flushDocument.status !== 'persisted'
      || flushDocument.includeMaterialization !== false
    ) {
      throw new Error('POST /documents/{graph_id}/{document_id}/flush returned an invalid flush envelope')
    }
    documentFlush = {
      checked: true,
      graphId,
      documentId,
      status: flushDocument.status,
    }
    const writeSmokeDocumentId = `parity-write-smoke-${Date.now()}`
    const writeSmokeToken = `parity-token-${writeSmokeDocumentId}`
    const writeSmoke = await timedStep('documentWriteSmokeMs', () => fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(writeSmokeDocumentId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          title: 'Parity Write Smoke',
          blocks: [
            {
              id: `${writeSmokeDocumentId}-b1`,
              type: 'heading',
              content: 'Parity Write Smoke',
              parentId: null,
              order: 0,
              level: 2,
              marks: [],
            },
            {
              id: `${writeSmokeDocumentId}-b2`,
              type: 'paragraph',
              content: `Marked bold content survives through local CRDT document.write. ${writeSmokeToken}`,
              parentId: null,
              order: 1,
              marks: [
                {
                  id: `${writeSmokeDocumentId}-m1`,
                  type: 'bold',
                  start: 7,
                  end: 11,
                },
              ],
            },
            {
              id: `${writeSmokeDocumentId}-b3`,
              type: 'todo',
              content: 'Task item parity',
              parentId: null,
              order: 2,
              checked: true,
              marks: [],
            },
            {
              id: `${writeSmokeDocumentId}-b4`,
              type: 'code',
              content: 'SELECT * WHERE { ?s ?p ?o }',
              parentId: null,
              order: 3,
              language: 'sparql',
              marks: [],
            },
          ],
          expectedRevision: 0,
          parentId: null,
        }),
      },
    ))
    if (writeSmoke.id !== writeSmokeDocumentId || !Array.isArray(writeSmoke.blocks) || writeSmoke.blocks.length < 4) {
      throw new Error('PUT /documents/{graph_id}/{document_id} returned an invalid document envelope')
    }
    if (
      writeSmoke.blocks[1]?.marks?.[0]?.type !== 'bold'
      || writeSmoke.blocks[2]?.type !== 'todo'
      || writeSmoke.blocks[2]?.checked !== true
      || writeSmoke.blocks[3]?.type !== 'code'
      || writeSmoke.blocks[3]?.language !== 'sparql'
    ) {
      throw new Error('PUT /documents/{graph_id}/{document_id} did not preserve canonical block fields')
    }
    const writeSmokeSearch = await fetchJson(`${manifest.apiUrl}/search/blocks`, {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ graph_id: graphId, query: writeSmokeToken, limit: 5 }),
    })
    if (
      !Array.isArray(writeSmokeSearch.results)
      || !writeSmokeSearch.results.some((result) =>
        (result.doc_id ?? result.documentId ?? result.document_id) === writeSmokeDocumentId
        && String(result.text_preview ?? result.content ?? '').includes(writeSmokeToken),
      )
    ) {
      throw new Error('PUT /documents/{graph_id}/{document_id} content was not visible through /search/blocks')
    }
    const writeSmokeDelete = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(writeSmokeDocumentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    if (writeSmokeDelete.id !== writeSmokeDocumentId || writeSmokeDelete.status !== 'deleted') {
      throw new Error('DELETE /documents/{graph_id}/{document_id} returned an invalid delete envelope')
    }
    const mcpWriteDocumentId = `parity-mcp-write-${Date.now()}`
    const mcpToken = `mcp-token-${mcpWriteDocumentId}`
    const mcpWrite = await timedStep('mcpWriteDocumentMs', () => callTool('write_document', {
      graphId,
      documentId: mcpWriteDocumentId,
      content: `# MCP Write Smoke

MCP write_document should roundtrip through TipTap XML, Y.Doc, RDF materialization, and block search. ${mcpToken}`,
      format: 'markdown',
      awaitDurable: true,
      comments: {},
    }))
    if (
      mcpWrite.success !== true
      || mcpWrite.documentId !== mcpWriteDocumentId
      || !Array.isArray(mcpWrite.blockIds)
      || mcpWrite.blockIds.length < 2
    ) {
      throw new Error('MCP write_document returned an invalid write envelope')
    }
    const mcpBlocks = await callTool('read_blocks', {
      graphId,
      documentId: mcpWriteDocumentId,
      limit: 10,
      includeIds: true,
      format: 'text',
    })
    const mcpBlockTexts = (mcpBlocks.blocks ?? []).map((block) => String(block.content ?? ''))
    if (
      !mcpBlockTexts.includes('MCP Write Smoke')
      || !mcpBlockTexts.some((text) => text.includes(mcpToken))
    ) {
      throw new Error('MCP write_document content was not visible through read_blocks')
    }
    const mcpHostedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(mcpWriteDocumentId)}`,
      { headers: authHeaders },
    )
    if (
      mcpHostedDocument.id !== mcpWriteDocumentId
      || mcpHostedDocument.title !== 'MCP Write Smoke'
      || !Array.isArray(mcpHostedDocument.blocks)
      || mcpHostedDocument.blocks.length < 2
    ) {
      throw new Error('MCP write_document was not visible through hosted-shaped document read')
    }
    const mcpSearch = await callTool('search_blocks', {
      graphId,
      query: mcpToken,
      docFilter: mcpWriteDocumentId,
      mode: 'lexical',
      limit: 5,
    })
    if (
      !Array.isArray(mcpSearch.results)
      || !mcpSearch.results.some((result) =>
        (result.documentId ?? result.document_id) === mcpWriteDocumentId
        && String(result.content ?? '').includes(mcpToken),
      )
    ) {
      throw new Error('MCP write_document content was not visible through search_blocks')
    }
    const mcpHistoryToken = `history-token-${mcpWriteDocumentId}`
    const mcpHistoryRewrite = await callTool('write_document', {
      graphId,
      documentId: mcpWriteDocumentId,
      content: `# MCP Write Smoke

MCP write_document second revision creates a local history snapshot. ${mcpHistoryToken}`,
      format: 'markdown',
      awaitDurable: true,
    })
    if (mcpHistoryRewrite.success !== true || mcpHistoryRewrite.documentId !== mcpWriteDocumentId) {
      throw new Error('second MCP write_document for history did not complete')
    }
    const documentHistory = await timedStep('mcpDocumentHistoryMs', () => callTool('get_document_history', {
      graphId,
      documentId: mcpWriteDocumentId,
      limit: 5,
    }))
    if (
      documentHistory.snapshot_count < 2
      || !Array.isArray(documentHistory.snapshots)
      || !documentHistory.snapshots[0]?.snapshot_id
      || documentHistory.snapshots[0]?.tier_label !== '20 min'
    ) {
      throw new Error('get_document_history did not return local snapshots newest first')
    }
    const oldestSnapshot = documentHistory.snapshots[documentHistory.snapshots.length - 1]
    const firstSnapshotContent = await timedStep('mcpReadSnapshotMs', () => callTool('read_document_at_snapshot', {
      graphId,
      documentId: mcpWriteDocumentId,
      snapshotId: oldestSnapshot.snapshot_id,
    }))
    if (
      firstSnapshotContent.snapshot_id !== oldestSnapshot.snapshot_id
      || !String(firstSnapshotContent.content ?? '').includes(mcpToken)
      || String(firstSnapshotContent.content ?? '').includes(mcpHistoryToken)
    ) {
      throw new Error('read_document_at_snapshot did not return the requested historical content')
    }
    mcpDocumentHistory = {
      checked: true,
      graphId,
      documentId: mcpWriteDocumentId,
      snapshotCount: documentHistory.snapshot_count,
      oldestSnapshotId: oldestSnapshot.snapshot_id,
      latestTier: documentHistory.snapshots[0].tier,
    }
    const historyBase = `${manifest.apiUrl}/v1/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(mcpWriteDocumentId)}/snapshots`
    const apiHistory = await timedStep('documentSnapshotListMs', () => fetchJson(historyBase, { headers: authHeaders }))
    if (
      !Array.isArray(apiHistory.snapshots)
      || apiHistory.snapshots.length < 2
      || apiHistory.snapshots[0]?.snapshot_id !== documentHistory.snapshots[0].snapshot_id
      || apiHistory.snapshots[0]?.doc_id !== mcpWriteDocumentId
      || apiHistory.snapshots[0]?.tier !== '20min'
    ) {
      throw new Error('GET /v1/documents/{graph_id}/{doc_id}/snapshots returned an invalid snapshot list')
    }
    const apiHistoryCount = await timedStep('documentSnapshotCountMs', () => fetchJson(`${historyBase}/count`, { headers: authHeaders }))
    if (typeof apiHistoryCount.count !== 'number' || apiHistoryCount.count < apiHistory.snapshots.length) {
      throw new Error('GET /v1/documents/{graph_id}/{doc_id}/snapshots/count returned an invalid count')
    }
    const apiSnapshotText = await timedStep('documentSnapshotTextMs', () => fetchTextResponse(
      `${historyBase}/${encodeURIComponent(oldestSnapshot.snapshot_id)}/text`,
      { headers: authHeaders },
    ))
    if (
      !apiSnapshotText.text.includes(mcpToken)
      || apiSnapshotText.text.includes(mcpHistoryToken)
    ) {
      throw new Error('GET /v1 document snapshot text did not return the requested historical content')
    }
    const apiSnapshotHtml = await timedStep('documentSnapshotHtmlMs', () => fetchTextResponse(
      `${historyBase}/${encodeURIComponent(oldestSnapshot.snapshot_id)}/html`,
      { headers: authHeaders },
    ))
    if (
      !apiSnapshotHtml.headers.get('content-type')?.includes('text/html')
      || !apiSnapshotHtml.text.includes('data-block-id=')
      || !apiSnapshotHtml.text.includes(mcpToken)
      || apiSnapshotHtml.text.includes(mcpHistoryToken)
    ) {
      throw new Error('GET /v1 document snapshot HTML did not return a rendered historical fragment')
    }
    const manualMutationResult = await timedStep('documentSnapshotManualMutationMs', async () => {
      const manualSave = await fetchJsonResponse(historyBase, {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({ label: 'Parity manual save' }),
      })
      if (
        manualSave.status !== 201
        || manualSave.body?.is_manual !== true
        || manualSave.body?.label !== 'Parity manual save'
        || !manualSave.body?.snapshot_id
      ) {
        throw new Error('POST /v1 document snapshots did not create a manual snapshot')
      }
      const autoDeleteAttempt = await fetchJsonAnyStatus(
        `${historyBase}/${encodeURIComponent(oldestSnapshot.snapshot_id)}`,
        { method: 'DELETE', headers: authHeaders },
      )
      if (autoDeleteAttempt.status !== 403) {
        throw new Error('DELETE /v1 document snapshot allowed deleting an automatic snapshot')
      }
      const manualDelete = await fetchJsonResponse(
        `${historyBase}/${encodeURIComponent(manualSave.body.snapshot_id)}`,
        { method: 'DELETE', headers: authHeaders },
      )
      if (manualDelete.status !== 204) {
        throw new Error('DELETE /v1 document snapshot did not delete a manual snapshot')
      }
      const copied = await fetchJsonResponse(
        `${historyBase}/${encodeURIComponent(oldestSnapshot.snapshot_id)}/copy`,
        {
          method: 'POST',
          headers: { ...authHeaders, 'Content-Type': 'application/json' },
          body: JSON.stringify({ label: 'Copied parity snapshot' }),
        },
      )
      if (
        copied.status !== 201
        || copied.body?.is_manual !== true
        || copied.body?.label !== 'Copied parity snapshot'
        || !copied.body?.snapshot_id
      ) {
        throw new Error('POST /v1 document snapshot copy did not create a manual snapshot')
      }
      const copiedDelete = await fetchJsonResponse(
        `${historyBase}/${encodeURIComponent(copied.body.snapshot_id)}`,
        { method: 'DELETE', headers: authHeaders },
      )
      if (copiedDelete.status !== 204) {
        throw new Error('DELETE /v1 copied document snapshot did not return no-content')
      }
      return { manualSnapshotId: manualSave.body.snapshot_id, copiedSnapshotId: copied.body.snapshot_id }
    })
    documentHistoryApi = {
      checked: true,
      graphId,
      documentId: mcpWriteDocumentId,
      snapshotCount: apiHistory.snapshots.length,
      totalCount: apiHistoryCount.count,
      oldestSnapshotId: oldestSnapshot.snapshot_id,
      manualSnapshotId: manualMutationResult.manualSnapshotId,
      copiedSnapshotId: manualMutationResult.copiedSnapshotId,
    }
    const mcpDelete = await callTool('delete_document', {
      graphId,
      documentId: mcpWriteDocumentId,
    })
    if (mcpDelete.documentId !== mcpWriteDocumentId && mcpDelete.id !== mcpWriteDocumentId) {
      throw new Error('MCP delete_document returned an invalid delete envelope')
    }
    mcpWriteAdapters = {
      checked: true,
      graphId,
      documentId: mcpWriteDocumentId,
      blockCount: mcpWrite.blockIds.length,
      searchHits: mcpSearch.results.length,
    }

    const blockSmokeDocumentId = `parity-block-smoke-${Date.now()}`
    const blockSmokeWrite = await timedStep('blockSeedWriteMs', () => callTool('write_document', {
      graphId,
      documentId: blockSmokeDocumentId,
      content: `# Parity Block Smoke

The quick brown fox.

Trailing paragraph for stability.`,
      format: 'markdown',
      awaitDurable: true,
    }))
    if (!blockSmokeWrite.success || !Array.isArray(blockSmokeWrite.blockIds) || blockSmokeWrite.blockIds.length < 3) {
      throw new Error('block-smoke seed write_document did not return at least 3 blocks')
    }
    const seededBlocks = await callTool('read_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      limit: 50,
      includeIds: true,
      format: 'text',
    })
    const seededBlockList = seededBlocks.blocks ?? []
    if (seededBlockList.length < 3) {
      throw new Error('block-smoke seed read_blocks returned fewer than 3 blocks')
    }
    const paragraphBlock = seededBlockList.find((block) => /quick brown fox/i.test(String(block.content ?? '')))
    if (!paragraphBlock) {
      throw new Error('block-smoke seed could not find paragraph block to edit')
    }
    const paragraphBlockId = paragraphBlock.blockId ?? paragraphBlock.block_id

    const inserted = await timedStep('blockInsertMs', () => callTool('insert_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      content: 'Inserted via insert_blocks parity smoke.',
      format: 'plain',
      blockId: paragraphBlockId,
      position: 'after',
    }))
    if (!inserted.success || !Array.isArray(inserted.block_ids) || inserted.block_ids.length < 1) {
      throw new Error('insert_blocks did not return new block ids')
    }
    const insertedBlockId = inserted.block_ids[0]

    const afterInsert = await callTool('read_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      limit: 50,
      includeIds: true,
      format: 'text',
    })
    if ((afterInsert.blocks ?? []).length !== seededBlockList.length + 1) {
      throw new Error('insert_blocks did not increase the block count by 1')
    }
    if (!(afterInsert.blocks ?? []).some((block) => (block.blockId ?? block.block_id) === insertedBlockId)) {
      throw new Error('insert_blocks new block id not visible in read_blocks projection')
    }

    const updated = await timedStep('blockUpdateMs', () => callTool('update_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      blockId: insertedBlockId,
      attrs: { 'data-parity-marker': 'updated' },
    }))
    if (!updated.success || !Array.isArray(updated.updated) || updated.updated[0] !== insertedBlockId) {
      throw new Error('update_blocks did not report the target as updated')
    }

    const edited = await timedStep('blockEditTextMs', () => callTool('edit_block_text', {
      graphId,
      documentId: blockSmokeDocumentId,
      blockId: paragraphBlockId,
      operations: [
        { type: 'insert', offset: 19, text: 'er' },
      ],
    }))
    if (!edited.success || edited.length_after !== edited.length_before + 2) {
      throw new Error('edit_block_text did not grow the block text by inserted-character count')
    }
    const afterEdit = await callTool('read_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      limit: 50,
      includeIds: true,
      format: 'text',
    })
    const editedParagraph = (afterEdit.blocks ?? []).find(
      (block) => (block.blockId ?? block.block_id) === paragraphBlockId,
    )
    if (!editedParagraph || !/quick brown foxer\./.test(String(editedParagraph.content ?? ''))) {
      throw new Error(
        `edit_block_text did not produce expected substring; got ${JSON.stringify(editedParagraph?.content)}`,
      )
    }

    if (!cellSkip('mcpCommentAdapters')) {
    const commentId = `comment-${Date.now()}`
    const commentSet = await timedStep('mcpEditCommentMs', () => callTool('edit_comment', {
      graph_id: graphId,
      document_id: blockSmokeDocumentId,
      comment_id: commentId,
      action: 'set',
      text: 'Comment anchored by MCP edit_comment.',
      author: 'Parity Harness',
      block_id: paragraphBlockId,
      find: 'quick brown',
    }))
    if (
      !commentSet.success
      || commentSet.commentId !== commentId
      || commentSet.anchored < 1
      || commentSet.comment?.text !== 'Comment anchored by MCP edit_comment.'
    ) {
      throw new Error('edit_comment set did not return a valid anchored comment envelope')
    }
    await callTool('flush_crdt', { graphId, documentId: blockSmokeDocumentId })
    const afterCommentSet = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(blockSmokeDocumentId)}`,
      { headers: authHeaders },
    )
    const commentedBlock = (afterCommentSet.blocks ?? []).find((block) => block.id === paragraphBlockId)
    if (!commentedBlock?.marks?.some((mark) => mark.type === 'comment' && mark.start === 4 && mark.end === 15)) {
      throw new Error('edit_comment set did not materialize the expected comment mark range')
    }
    const commentResolve = await callTool('edit_comment', {
      graph_id: graphId,
      document_id: blockSmokeDocumentId,
      comment_id: commentId,
      action: 'resolve',
      resolved: true,
    })
    if (!commentResolve.success || commentResolve.comment?.resolved !== true) {
      throw new Error('edit_comment resolve did not mark the comment resolved')
    }
    const commentDelete = await callTool('edit_comment', {
      graph_id: graphId,
      document_id: blockSmokeDocumentId,
      comment_id: commentId,
      action: 'delete',
    })
    if (!commentDelete.success || commentDelete.deleted !== true || commentDelete.cleared < 1) {
      throw new Error('edit_comment delete did not remove the comment metadata and mark')
    }
    await callTool('flush_crdt', { graphId, documentId: blockSmokeDocumentId })
    const afterCommentDelete = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(blockSmokeDocumentId)}`,
      { headers: authHeaders },
    )
    const uncommentedBlock = (afterCommentDelete.blocks ?? []).find((block) => block.id === paragraphBlockId)
    if (uncommentedBlock?.marks?.some((mark) => mark.type === 'comment')) {
      throw new Error('edit_comment delete left a comment mark visible in the document projection')
    }
    mcpCommentAdapters = {
      checked: true,
      graphId,
      documentId: blockSmokeDocumentId,
      commentId,
      anchored: commentSet.anchored,
    }
    } else {
      mcpCommentAdapters = skipForCellProfile('mcpCommentAdapters: MCP edit_comment (document.editComment)')
    }

    const valuedBlock = await timedStep('mcpValueMs', () => callTool('value', {
      graph_id: graphId,
      document_id: blockSmokeDocumentId,
      block_id: paragraphBlockId,
      importance: 4,
      valence: 2,
      tags: ['Decision', '#parity'],
    }))
    if (
      (valuedBlock.document_id ?? valuedBlock.documentId) !== blockSmokeDocumentId
      || (valuedBlock.block_id ?? valuedBlock.blockId) !== paragraphBlockId
      || !(valuedBlock.tags ?? []).includes('decision')
      || !(valuedBlock.tags ?? []).includes('parity')
    ) {
      throw new Error('value did not return a valid cumulative block valuation')
    }
    assertApprox('value raw_importance_sum', valuedBlock.raw_importance_sum ?? valuedBlock.rawImportanceSum, 4)
    assertApprox('value cumulative_importance', valuedBlock.cumulative_importance ?? valuedBlock.cumulativeImportance, Math.log2(5))
    assertApprox('value raw_valence_sum', valuedBlock.raw_valence_sum ?? valuedBlock.rawValenceSum, 2)
    assertApprox('value cumulative_valence', valuedBlock.cumulative_valence ?? valuedBlock.cumulativeValence, Math.log2(3))
    if (
      Number(valuedBlock.importance_count ?? valuedBlock.importanceCount ?? 0) !== 1
      || Number(valuedBlock.valence_count ?? valuedBlock.valenceCount ?? 0) !== 1
      || Number(valuedBlock.valuation_count ?? valuedBlock.valuationCount ?? 0) !== 2
    ) {
      throw new Error('value did not track per-field valuation counts')
    }

    const valuesConfig = await timedStep('mcpGetValuesMs', () => callTool('get_values', { graphId }))
    if (
      valuesConfig.source !== 'local-value-store'
      || !valuesConfig.importance_prompt
      || typeof valuesConfig.weights?.importance_weight !== 'number'
      || typeof valuesConfig.weights?.half_life_days !== 'number'
    ) {
      throw new Error('get_values did not return the local valuation config')
    }

    const blockValues = await timedStep('mcpGetBlockValuesMs', () => callTool('get_block_values', {
      graph_id: graphId,
      document_id: blockSmokeDocumentId,
      block_id: paragraphBlockId,
      min_score: 0.1,
      limit: 5,
    }))
    const valuedRow = (blockValues.blocks ?? []).find(
      (block) => (block.block_id ?? block.blockId) === paragraphBlockId,
    )
    if (!valuedRow || Number(valuedRow.composite_score ?? valuedRow.compositeScore ?? 0) <= 0) {
      throw new Error('get_block_values did not return the valued block')
    }
    assertApprox('get_block_values raw_importance_sum', valuedRow.raw_importance_sum ?? valuedRow.rawImportanceSum, 4)
    assertApprox('get_block_values cumulative_importance', valuedRow.cumulative_importance ?? valuedRow.cumulativeImportance, Math.log2(5))
    assertApprox('get_block_values raw_valence_sum', valuedRow.raw_valence_sum ?? valuedRow.rawValenceSum, 2)
    assertApprox('get_block_values cumulative_valence', valuedRow.cumulative_valence ?? valuedRow.cumulativeValence, Math.log2(3))

    const importantBlocks = await timedStep('mcpGetImportantBlocksMs', () => callTool('get_important_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      valence: 'positive',
      limit: 5,
    }))
    const importantRow = (importantBlocks.blocks ?? []).find(
      (block) => (block.block_id ?? block.blockId) === paragraphBlockId,
    )
    if (
      !importantRow
      || !String(importantRow.content ?? importantRow.text ?? '').includes('quick brown foxer')
      || Number(importantRow.score ?? 0) <= 0
    ) {
      throw new Error('get_important_blocks did not return the valued block content')
    }

    const hostedSalienceApplied = await timedStep('salienceApplyValueMs', () => fetchJson(
      `${manifest.apiUrl}/salience/${encodeURIComponent(graphId)}/blocks/value`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          valuations: [
            {
              document_id: blockSmokeDocumentId,
              block_id: insertedBlockId,
              importance: 5,
              valence: -3,
              tags: ['HTTP', '#parity'],
            },
            {
              document_id: '',
              block_id: insertedBlockId,
              importance: 3,
            },
            {
              document_id: blockSmokeDocumentId,
              block_id: insertedBlockId,
              importance: 0,
            },
          ],
        }),
      },
    ))
    const activeForgetRow = (hostedSalienceApplied.results ?? []).at(-1)
    if (
      hostedSalienceApplied.updated_count !== 2
      || hostedSalienceApplied.error_count !== 1
      || (activeForgetRow?.block_id ?? activeForgetRow?.blockId) !== insertedBlockId
    ) {
      throw new Error('POST /salience/{graph_id}/blocks/value did not preserve partial batch semantics')
    }
    assertApprox('active forgetting raw_importance_sum', activeForgetRow.raw_importance_sum ?? activeForgetRow.rawImportanceSum, 1)
    assertApprox('active forgetting cumulative_importance', activeForgetRow.cumulative_importance ?? activeForgetRow.cumulativeImportance, 1)
    assertApprox('active forgetting raw_valence_sum', activeForgetRow.raw_valence_sum ?? activeForgetRow.rawValenceSum, -3)
    assertApprox('active forgetting cumulative_valence', activeForgetRow.cumulative_valence ?? activeForgetRow.cumulativeValence, -2)
    if (
      Number(activeForgetRow.importance_count ?? activeForgetRow.importanceCount ?? 0) !== 2
      || Number(activeForgetRow.valence_count ?? activeForgetRow.valenceCount ?? 0) !== 1
    ) {
      throw new Error('active forgetting did not increment counts without resetting the value record')
    }

    const salienceInsertedValues = await timedStep('salienceValuesMs', () => fetchJson(
      `${manifest.apiUrl}/salience/${encodeURIComponent(graphId)}/blocks/values?document_id=${encodeURIComponent(blockSmokeDocumentId)}&block_id=${encodeURIComponent(insertedBlockId)}&limit=5`,
      { headers: authHeaders },
    ))
    const salienceInsertedRow = (salienceInsertedValues.blocks ?? []).find(
      (block) => (block.block_id ?? block.blockId) === insertedBlockId,
    )
    if (!salienceInsertedRow || salienceInsertedValues.count < 1) {
      throw new Error('GET /salience/{graph_id}/blocks/values did not return the inserted block valuation')
    }
    assertApprox('salience values raw_importance_sum', salienceInsertedRow.raw_importance_sum ?? salienceInsertedRow.rawImportanceSum, 1)
    assertApprox('salience values cumulative_valence', salienceInsertedRow.cumulative_valence ?? salienceInsertedRow.cumulativeValence, -2)

    const userValuedRow = await timedStep('salienceUserValueMs', () => fetchJson(
      `${manifest.apiUrl}/salience/${encodeURIComponent(graphId)}/blocks/user-value`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          document_id: blockSmokeDocumentId,
          block_id: paragraphBlockId,
          importance: 5,
          valence: 4,
        }),
      },
    ))
    if (
      (userValuedRow.block_id ?? userValuedRow.blockId) !== paragraphBlockId
      || Number(userValuedRow.user_importance ?? userValuedRow.userImportance ?? -1) !== 5
      || Number(userValuedRow.user_valence ?? userValuedRow.userValence ?? -1) !== 4
      || Number(userValuedRow.composite_score ?? userValuedRow.compositeScore ?? 0)
        <= Number(valuedRow.composite_score ?? valuedRow.compositeScore ?? 0)
    ) {
      throw new Error('PUT /salience/{graph_id}/blocks/user-value did not apply the user overlay')
    }

    const salienceParagraphValues = await fetchJson(
      `${manifest.apiUrl}/salience/${encodeURIComponent(graphId)}/blocks/values?document_id=${encodeURIComponent(blockSmokeDocumentId)}&block_ids=${encodeURIComponent(paragraphBlockId)}&limit=5`,
      { headers: authHeaders },
    )
    const salienceParagraphRow = (salienceParagraphValues.blocks ?? []).find(
      (block) => (block.block_id ?? block.blockId) === paragraphBlockId,
    )
    if (
      !salienceParagraphRow
      || Number(salienceParagraphRow.user_importance ?? salienceParagraphRow.userImportance ?? -1) !== 5
      || Number(salienceParagraphRow.user_valence ?? salienceParagraphRow.userValence ?? -1) !== 4
    ) {
      throw new Error('GET /salience/{graph_id}/blocks/values did not expose user valuation fields')
    }

    const valuedContextBundle = await callTool('context_bundle', {
      graph_id: graphId,
      important_limit: 3,
      workspace_depth: 1,
    })
    if (
      valuedContextBundle.important_blocks?.source !== 'local-value-store'
      || !(valuedContextBundle.important_blocks?.blocks ?? []).some((block) =>
        (block.block_id ?? block.blockId) === paragraphBlockId
        && String(block.content ?? block.text ?? '').includes('quick brown foxer'),
      )
    ) {
      throw new Error('context_bundle did not prefer valued local important blocks')
    }

    const valuedDigest = await callTool('document_digest', {
      graphId,
      documentId: blockSmokeDocumentId,
      topValued: 3,
    })
    if (
      !(valuedDigest.valuation_summary?.top_valued ?? []).some((block) =>
        (block.block_id ?? block.blockId) === paragraphBlockId
        && String(block.content ?? '').includes('quick brown foxer'),
      )
    ) {
      throw new Error('document_digest did not expose top valued local blocks')
    }

    const historyCountBeforeRevaluate = Number(valuesConfig.history_count ?? valuesConfig.historyCount ?? 0)
    const revaluated = await timedStep('mcpRevaluateMs', () => callTool('revaluate', {
      graphId,
      importancePrompt: 'Parity harness importance prompt.',
      valencePrompt: 'Parity harness valence prompt.',
      weights: 'importance_weight: 0.5\nhalf_life_days: 14',
    }))
    if (
      revaluated.success !== true
      || !(revaluated.updated ?? []).includes('importance_prompt')
      || !(revaluated.updated ?? []).includes('valence_prompt')
      || !(revaluated.updated ?? []).includes('weights')
      || Number(revaluated.config?.history_count ?? revaluated.config?.historyCount ?? 0) !== historyCountBeforeRevaluate + 1
      || Number(revaluated.config?.weights?.importance_weight ?? 0) !== 0.5
      || Number(revaluated.config?.weights?.half_life_days ?? 0) !== 14
    ) {
      throw new Error('revaluate did not archive and update the valuation configuration')
    }

    const patchedSalienceConfig = await timedStep('salienceConfigPatchMs', () => fetchJson(
      `${manifest.apiUrl}/salience/${encodeURIComponent(graphId)}/config`,
      {
        method: 'PATCH',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          valence_weight: 0.25,
          temporal_weight: 0.12,
        }),
      },
    ))
    if (
      patchedSalienceConfig.source !== 'local-value-store'
      || Number(patchedSalienceConfig.weights?.valence_weight ?? 0) !== 0.25
      || Number(patchedSalienceConfig.weights?.temporal_weight ?? 0) !== 0.12
    ) {
      throw new Error('PATCH /salience/{graph_id}/config did not update local salience weights')
    }

    const valuationRdf = await fetchJson(`${manifest.apiUrl}/api/sparql/query`, {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({
        graphId,
        query: `PREFIX mnemo: <https://mnemosyne.local/ns#>
PREFIX doc: <http://mnemosyne.dev/doc#>
SELECT ?value ?rawImportance ?cumulativeImportance ?rawValence ?userImportance WHERE {
  ?value a mnemo:BlockValuation ;
    mnemo:documentId ${JSON.stringify(blockSmokeDocumentId)} ;
    mnemo:blockId ${JSON.stringify(paragraphBlockId)} ;
    doc:rawImportanceSum ?rawImportance ;
    doc:cumulativeImportance ?cumulativeImportance ;
    doc:rawValenceSum ?rawValence ;
    doc:userImportance ?userImportance .
}`,
      }),
    })
    if (!Array.isArray(valuationRdf.rows) || valuationRdf.rows.length < 1) {
      throw new Error('value did not materialize hosted-shaped block valuation triples to RDF')
    }

    mcpValuationAdapters = {
      checked: true,
      graphId,
      documentId: blockSmokeDocumentId,
      blockId: paragraphBlockId,
      rawImportanceSum: valuedRow.raw_importance_sum ?? valuedRow.rawImportanceSum,
      cumulativeImportance: valuedRow.cumulative_importance ?? valuedRow.cumulativeImportance,
      compositeScore: userValuedRow.composite_score ?? userValuedRow.compositeScore,
      activeForgottenRawImportance: activeForgetRow.raw_importance_sum ?? activeForgetRow.rawImportanceSum,
      historyCount: revaluated.config?.history_count ?? revaluated.config?.historyCount,
      importantCount: (importantBlocks.blocks ?? []).length,
      salienceCount: salienceParagraphValues.count,
      rdfRows: valuationRdf.rows.length,
    }

    const deleted = await timedStep('blockDeleteMs', () => callTool('delete_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      blockIds: [insertedBlockId],
    }))
    if (!deleted.success || !Array.isArray(deleted.deleted) || deleted.deleted[0] !== insertedBlockId) {
      throw new Error('delete_blocks did not report the target as deleted')
    }
    const afterDelete = await callTool('read_blocks', {
      graphId,
      documentId: blockSmokeDocumentId,
      limit: 50,
      includeIds: true,
      format: 'text',
    })
    if ((afterDelete.blocks ?? []).some((block) => (block.blockId ?? block.block_id) === insertedBlockId)) {
      throw new Error('delete_blocks left the inserted block visible in read_blocks')
    }

    await callTool('delete_document', { graphId, documentId: blockSmokeDocumentId })

    blockMutations = {
      checked: true,
      graphId,
      documentId: blockSmokeDocumentId,
      seededBlocks: seededBlockList.length,
      insertedBlockId,
      paragraphBlockId,
      finalLengthDelta: edited.length_after - edited.length_before,
    }

    const hostedBlocks = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/blocks`,
      { headers: authHeaders },
    )
    if (!Array.isArray(hostedBlocks.blocks)) {
      throw new Error('GET /documents/{graph_id}/{document_id}/blocks returned an invalid block envelope')
    }
    const hostedContext = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(documentId)}/block-context`,
      { headers: authHeaders },
    )
    if (!Array.isArray(hostedContext.blocks)) {
      throw new Error('GET /documents/{graph_id}/{document_id}/block-context returned an invalid context envelope')
    }

    const hostedNavigation = await fetchJson(`${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}`, {
      headers: authHeaders,
    })
    if (!Array.isArray(hostedNavigation.documents) || !Array.isArray(hostedNavigation.folders)) {
      throw new Error('GET /navigation/{graph_id} returned an invalid navigation envelope')
    }
    const hostedFolders = await fetchJson(`${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders`, {
      headers: authHeaders,
    })
    if (!Array.isArray(hostedFolders)) {
      throw new Error('GET /navigation/{graph_id}/folders did not return an array')
    }
    const hostedArtifacts = await fetchJson(`${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts`, {
      headers: authHeaders,
    })
    if (!Array.isArray(hostedArtifacts)) {
      throw new Error('GET /navigation/{graph_id}/artifacts did not return an array')
    }

    if (!cellSkip('artifactUpload')) {
    const uploadDocumentFilename = `parity-upload-${Date.now()}.md`
    const uploadToken = `upload-token-${Date.now()}`
    const uploadMarkdown = `---
title: Upload Frontmatter
---
# Upload Smoke

Paragraph with **bold _italic_** and [link](https://example.test). ${uploadToken}

- [x] done
  - nested

\`\`\`ts
const value = 1
\`\`\`

[^1]

[^1]: Footnote text`
    const uploadForm = new FormData()
    uploadForm.append(
      'file',
      new Blob([uploadMarkdown], { type: 'text/markdown' }),
      uploadDocumentFilename,
    )
    const uploadResponse = await timedStep('artifactUploadMarkdownMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/upload`,
      {
        method: 'POST',
        headers: { ...authHeaders, Prefer: 'respond-sync' },
        body: uploadForm,
      },
    ))
    const uploaded = uploadResponse.body
    if (
      uploadResponse.status !== 201
      || !uploaded?.documentId
      || uploaded.title !== 'Upload Smoke'
      || uploaded.fileType !== 'md'
      || uploaded.readOnly !== true
      || !uploaded.sourceFile?.storageKey
      || uploaded.sourceFile.originalFilename !== uploadDocumentFilename
    ) {
      throw new Error('POST /artifacts/{graph_id}/upload returned an invalid document upload response')
    }
    // A2 item B4 deferred projection flush — respond-sync guarantees the CRDT op
    // completed, but the /documents projection materializer runs off the request
    // path; flush before reading block content to avoid a stale-projection race.
    await callTool('flush_crdt', { graphId })
    const uploadedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(uploaded.documentId)}`,
      { headers: authHeaders },
    )
    const uploadedBlocks = uploadedDocument.blocks ?? []
    if (
      uploadedDocument.id !== uploaded.documentId
      || uploadedDocument.readOnly !== true
      || !uploadedBlocks.some((block) => block.type === 'heading' && block.content === 'Upload Smoke')
      || !uploadedBlocks.some((block) => String(block.content ?? '').includes(uploadToken))
      || !uploadedBlocks.some((block) => block.type === 'todo' && block.checked === true)
      || !uploadedBlocks.some((block) => block.type === 'code' && block.language === 'ts')
    ) {
      throw new Error('uploaded markdown document did not preserve expected formatting projection')
    }
    const originalResponse = await fetchWithTimeout(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/documents/${encodeURIComponent(uploaded.documentId)}/download-original`,
      { headers: authHeaders },
    )
    if (!originalResponse.ok) {
      throw new Error(`uploaded original download returned ${originalResponse.status}`)
    }
    const originalText = await originalResponse.text()
    if (originalText !== uploadMarkdown) {
      throw new Error('uploaded original download did not return exact source bytes')
    }
    if (!cellSkip('mcpDocumentEditability')) {
    const editableResult = await timedStep('mcpMakeDocumentEditableMs', () => callTool('make_document_editable', {
      graph_id: graphId,
      document_id: uploaded.documentId,
    }))
    if (!editableResult.success || editableResult.readOnly !== false) {
      throw new Error('MCP make_document_editable returned an invalid editable envelope')
    }
    // A2 item B4 deferred projection flush off the request path; make_document_editable
    // clears the readOnly flag via CRDT and the projection may not yet reflect the
    // change. Force a flush before reading the document state.
    await callTool('flush_crdt', { graphId })
    const editableDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(uploaded.documentId)}`,
      { headers: authHeaders },
    )
    if (editableDocument.readOnly !== false) {
      throw new Error('MCP make_document_editable did not clear the hosted readOnly flag')
    }
    mcpDocumentEditability = {
      checked: true,
      graphId,
      documentId: uploaded.documentId,
      readOnly: editableDocument.readOnly,
    }
    } else {
      mcpDocumentEditability = skipForCellProfile('mcpDocumentEditability: MCP make_document_editable on uploaded artifact (document.uploadIngest)')
    }
    if (!cellSkip('mcpArtifactAdapters')) {
    const mcpUploadTempDir = await fs.mkdtemp(path.join(os.tmpdir(), 'mnemosyne-mcp-upload-'))
    try {
      const mcpUploadToken = `mcp-upload-token-${Date.now()}`
      const mcpUploadPath = path.join(mcpUploadTempDir, `mcp-upload-${Date.now()}.md`)
      const mcpUploadMarkdown = `# MCP Upload Smoke

Paragraph uploaded from a local file path. ${mcpUploadToken}`
      await fs.writeFile(mcpUploadPath, mcpUploadMarkdown)
      const mcpUploadJob = await timedStep('mcpUploadArtifactMs', () => callTool('upload_artifact', {
        graph_id: graphId,
        file_path: mcpUploadPath,
        label: 'MCP Upload Artifact',
      }))
      if (!mcpJobId(mcpUploadJob) || mcpUploadJob.asyncWork !== true || !mcpUploadJob.documentId) {
        throw new Error('MCP upload_artifact did not return an async job envelope')
      }
      const mcpUploaded = await callToolJobResult(mcpUploadJob)
      if (
        !mcpUploaded.success
        || mcpUploaded.documentId !== mcpUploadJob.documentId
        || mcpUploaded.artifactId !== mcpUploaded.documentId
        || mcpUploaded.autoIngested !== true
        || mcpUploaded.readOnly !== true
        || mcpUploaded.fileType !== 'md'
        || mcpUploaded.sourceFile?.originalFilename !== path.basename(mcpUploadPath)
      ) {
        throw new Error('MCP upload_artifact returned an invalid auto-ingested document envelope')
      }
      const mcpUploadedDocument = await fetchJson(
        `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(mcpUploaded.documentId)}`,
        { headers: authHeaders },
      )
      if (
        mcpUploadedDocument.readOnly !== true
        || !(mcpUploadedDocument.blocks ?? []).some((block) => block.type === 'heading' && block.content === 'MCP Upload Smoke')
        || !(mcpUploadedDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(mcpUploadToken))
      ) {
        throw new Error('MCP upload_artifact did not create the expected read-only document projection')
      }
      const mcpExistingIngestJob = await callTool('ingest_artifact', {
        graph_id: graphId,
        artifact_id: mcpUploaded.artifactId,
        mode: 'import',
      })
      if (!mcpJobId(mcpExistingIngestJob) || mcpExistingIngestJob.asyncWork !== true) {
        throw new Error('MCP ingest_artifact did not return an async job envelope for an existing upload')
      }
      const mcpExistingIngest = await callToolJobResult(mcpExistingIngestJob)
      if (
        !mcpExistingIngest.success
        || mcpExistingIngest.documentId !== mcpUploaded.documentId
        || mcpExistingIngest.alreadyIngested !== true
        || mcpExistingIngest.readOnly !== false
      ) {
        throw new Error('MCP ingest_artifact did not recognize and import an auto-ingested upload')
      }
      // A2 item B4 deferred projection flush off the request path; ingest_artifact
      // clears readOnly on the document via CRDT and the projection may not yet
      // reflect the change. Force a flush before reading the document state.
      await callTool('flush_crdt', { graphId })
      const mcpImportedUploadedDocument = await fetchJson(
        `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(mcpUploaded.documentId)}`,
        { headers: authHeaders },
      )
      if (mcpImportedUploadedDocument.readOnly !== false) {
        throw new Error('MCP ingest_artifact import mode did not clear readOnly on the auto-ingested upload')
      }
      mcpArtifactAdapters = {
        checked: false,
        graphId,
        uploadedDocumentId: mcpUploaded.documentId,
        uploadedReadOnly: mcpImportedUploadedDocument.readOnly,
      }
    } finally {
      await fs.rm(mcpUploadTempDir, { recursive: true, force: true })
    }
    } else {
      mcpArtifactAdapters = skipForCellProfile('mcpArtifactAdapters: MCP upload_artifact/ingest_artifact (document.uploadIngest)')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(uploaded.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    const asyncUploadFilename = `parity-async-upload-${Date.now()}.md`
    const asyncUploadToken = `async-upload-token-${Date.now()}`
    const asyncUploadMarkdown = `# Async Upload Smoke

Paragraph queued through the local artifact job path. ${asyncUploadToken}`
    const asyncUploadForm = new FormData()
    asyncUploadForm.append(
      'file',
      new Blob([asyncUploadMarkdown], { type: 'text/markdown' }),
      asyncUploadFilename,
    )
    const asyncUploadResponse = await timedStep('artifactUploadAsyncMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/upload`,
      {
        method: 'POST',
        headers: { ...authHeaders, Prefer: 'respond-async' },
        body: asyncUploadForm,
      },
    ))
    const asyncUploadJob = asyncUploadResponse.body
    const asyncUploadJobId = asyncUploadJob?.jobId ?? asyncUploadJob?.job_id
    const asyncUploadDocumentId = asyncUploadJob?.documentId ?? asyncUploadJob?.document_id
    if (
      asyncUploadResponse.status !== 202
      || !asyncUploadJobId
      || !asyncUploadDocumentId
      || !asyncUploadJob?.links?.result
      || !['queued', 'running', 'succeeded'].includes(String(asyncUploadJob?.status ?? ''))
    ) {
      throw new Error('POST /artifacts/{graph_id}/upload with Prefer: respond-async did not return a job envelope')
    }
    const asyncUploaded = await fetchJobResultEventually(
      `${manifest.apiUrl}${asyncUploadJob.links.result}`,
      { headers: authHeaders },
      240,
      500,
    )
    if (
      asyncUploaded.documentId !== asyncUploadDocumentId
      || asyncUploaded.title !== 'Async Upload Smoke'
      || asyncUploaded.fileType !== 'md'
      || asyncUploaded.readOnly !== true
      || asyncUploaded.sourceFile?.originalFilename !== asyncUploadFilename
    ) {
      throw new Error('async artifact upload job result did not preserve document upload semantics')
    }
    const asyncUploadedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(asyncUploadDocumentId)}`,
      { headers: authHeaders },
    )
    if (
      asyncUploadedDocument.readOnly !== true
      || !(asyncUploadedDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(asyncUploadToken))
    ) {
      throw new Error('async artifact upload did not create the expected read-only document projection')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(asyncUploadDocumentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    artifactUpload = {
      checked: true,
      graphId,
      documentId: uploaded.documentId,
      asyncJobId: asyncUploadJobId,
      asyncDocumentId: asyncUploadDocumentId,
      blockCount: uploadedBlocks.length,
    }
    } else {
      artifactUpload = skipForCellProfile('artifactUpload: POST /artifacts/{graph_id}/upload sync+async markdown (document.uploadIngest)')
      // The MCP editability and tempdir upload sub-flows operate on the uploaded
      // artifact, so they cannot run when the upload section itself is skipped.
      mcpDocumentEditability = skipForCellProfile('mcpDocumentEditability: MCP make_document_editable on uploaded artifact (document.uploadIngest)')
      mcpArtifactAdapters = skipForCellProfile('mcpArtifactAdapters: MCP upload_artifact/ingest_artifact (document.uploadIngest)')
    }

    if (!cellSkip('artifactFormatUpload')) {
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
    const htmlForm = new FormData()
    htmlForm.append('file', new Blob([uploadHtml], { type: 'text/html' }), htmlFilename)
    const htmlUploadResponse = await timedStep('artifactUploadHtmlMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/upload`,
      {
        method: 'POST',
        headers: { ...authHeaders, Prefer: 'respond-sync' },
        body: htmlForm,
      },
    ))
    const uploadedHtml = htmlUploadResponse.body
    if (
      htmlUploadResponse.status !== 201
      || !uploadedHtml?.documentId
      || uploadedHtml.title !== htmlTitle
      || uploadedHtml.fileType !== 'html'
      || uploadedHtml.sourceFile?.originalFilename !== htmlFilename
    ) {
      throw new Error('HTML artifact upload returned an invalid document upload response')
    }
    // A2 item B4 deferred projection flush — respond-sync unblocks after the CRDT
    // write, but the HTML-to-blocks projection materializer runs deferred; flush
    // before reading block content so the heading/token assertions are not racing.
    await callTool('flush_crdt', { graphId })
    const htmlDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(uploadedHtml.documentId)}`,
      { headers: authHeaders },
    )
    const htmlBlocks = htmlDocument.blocks ?? []
    if (
      !htmlBlocks.some((block) => block.type === 'heading' && block.content === 'Visible HTML Smoke')
      || !htmlBlocks.some((block) => String(block.content ?? '').includes(htmlToken))
      || htmlBlocks.some((block) => String(block.content ?? '').includes('Navigation should not'))
      || !htmlBlocks.some((block) => block.type === 'bullet' && block.content === 'Nested item' && block.parentId)
    ) {
      throw new Error('uploaded HTML document did not preserve expected main-content formatting projection')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(uploadedHtml.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

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
    const epubForm = new FormData()
    epubForm.append('file', new Blob([epubBytes], { type: 'application/epub+zip' }), epubFilename)
    const epubUploadResponse = await timedStep('artifactUploadEpubMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/upload`,
      {
        method: 'POST',
        headers: { ...authHeaders, Prefer: 'respond-sync' },
        body: epubForm,
      },
    ))
    const uploadedEpub = epubUploadResponse.body
    if (
      epubUploadResponse.status !== 201
      || !uploadedEpub?.documentId
      || uploadedEpub.title !== epubTitle
      || uploadedEpub.fileType !== 'epub'
      || uploadedEpub.sourceFile?.originalFilename !== epubFilename
    ) {
      throw new Error('EPUB artifact upload returned an invalid document upload response')
    }
    // A2 item B4 deferred projection flush — respond-sync completes the CRDT write,
    // but the EPUB chapter-folder navigation projection materializes off the request
    // path; flush before reading /navigation so folder and chapter entries are visible.
    await callTool('flush_crdt', { graphId })
    const epubNavigation = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}`,
      { headers: authHeaders },
    )
    const epubFolderId = `folder-book-${String(uploadedEpub.documentId).slice(0, 8)}`
    const epubFolder = (epubNavigation.folders ?? []).find((folder) => folder.id === epubFolderId)
    const epubDocuments = (epubNavigation.documents ?? []).filter((document) => document.parentId === epubFolderId)
    if (
      epubFolder?.label !== epubTitle
      || !epubDocuments.some((document) => document.id === uploadedEpub.documentId)
      || !epubDocuments.some((document) => document.title === 'Chapter One')
      || !epubDocuments.some((document) => document.title === 'Chapter Two')
    ) {
      throw new Error('EPUB upload did not create the expected book folder and chapter documents')
    }
    const chapterOne = epubDocuments.find((document) => document.title === 'Chapter One')
    const chapterOneDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(chapterOne.id)}`,
      { headers: authHeaders },
    )
    if (!(chapterOneDocument.blocks ?? []).some((block) => block.type === 'heading' && block.content === 'Chapter One')) {
      throw new Error('EPUB chapter document did not preserve chapter heading')
    }
    for (const document of epubDocuments) {
      await fetchJson(
        `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(document.id)}`,
        {
          method: 'DELETE',
          headers: authHeaders,
        },
      )
    }
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(epubFolderId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    const pdfTitle = `PDF Upload Smoke ${Date.now()}`
    const pdfFilename = `${pdfTitle.toLowerCase().replace(/[^a-z0-9]+/g, '-')}.pdf`
    const pdfToken = `pdf-token-${Date.now()}`
    const pdfBytes = simplePdfBytes({
      title: pdfTitle,
      lines: [
        `Page aware PDF text includes ${pdfToken}.`,
        'Second PDF paragraph for extraction.',
      ],
    })
    const pdfForm = new FormData()
    pdfForm.append('file', new Blob([pdfBytes], { type: 'application/pdf' }), pdfFilename)
    const pdfUploadResponse = await timedStep('artifactUploadPdfMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/upload`,
      {
        method: 'POST',
        headers: { ...authHeaders, Prefer: 'respond-sync' },
        body: pdfForm,
      },
    ))
    const uploadedPdf = pdfUploadResponse.body
    if (
      pdfUploadResponse.status !== 201
      || !uploadedPdf?.documentId
      || uploadedPdf.title !== pdfTitle
      || uploadedPdf.fileType !== 'pdf'
      || uploadedPdf.ingestionApproachId !== 'pdf.fast-text'
      || uploadedPdf.sourceFile?.originalFilename !== pdfFilename
    ) {
      throw new Error('PDF artifact upload returned an invalid document upload response')
    }
    // A2 item B4 deferred projection flush — respond-sync blocks until the CRDT op
    // completes, but the PDF fast-text block projection materializes asynchronously;
    // flush before reading /documents so heading and token blocks are present.
    await callTool('flush_crdt', { graphId })
    const pdfDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(uploadedPdf.documentId)}`,
      { headers: authHeaders },
    )
    const pdfBlocks = pdfDocument.blocks ?? []
    if (
      !pdfBlocks.some((block) => block.type === 'heading' && block.content === pdfTitle)
      || !pdfBlocks.some((block) => block.type === 'paragraph' && String(block.content ?? '').includes(pdfToken))
    ) {
      throw new Error(`uploaded PDF document did not preserve expected page-aware text projection: ${JSON.stringify({
        warnings: uploadedPdf.warnings ?? [],
        blocks: pdfBlocks.map((block) => ({ type: block.type, content: block.content })),
      })}`)
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(uploadedPdf.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    artifactFormatUpload = {
      checked: true,
      graphId,
      htmlDocumentId: uploadedHtml.documentId,
      epubDocumentId: uploadedEpub.documentId,
      pdfDocumentId: uploadedPdf.documentId,
      epubChapterCount: epubDocuments.length - 1,
    }
    } else {
      artifactFormatUpload = skipForCellProfile('artifactFormatUpload: HTML/EPUB/PDF uploads (document.uploadIngest)')
    }

    if (!cellSkip('artifactPdfAccurate')) {
    const pdfAccurateTitle = `PDF Accurate Facade ${Date.now()}`
    const pdfAccurateFilename = `${pdfAccurateTitle.toLowerCase().replace(/[^a-z0-9]+/g, '-')}.pdf`
    const pdfAccurateToken = `pdf-accurate-token-${Date.now()}`
    const pdfAccurateBytes = simplePdfBytes({
      title: pdfAccurateTitle,
      lines: [
        `Accurate facade should preserve ${pdfAccurateToken}.`,
        'The local fallback keeps page-aware fast-path extraction explicit.',
      ],
    })
    const pdfAccurateForm = new FormData()
    pdfAccurateForm.append('file', new Blob([pdfAccurateBytes], { type: 'application/pdf' }), pdfAccurateFilename)
    const pdfAccurateResponse = await timedStep('artifactPdfAccurateMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/ingest/pdf-accurate`,
      {
        method: 'POST',
        headers: authHeaders,
        body: pdfAccurateForm,
      },
    ))
    const pdfAccurateJob = pdfAccurateResponse.body
    const pdfAccurateJobId = pdfAccurateJob?.jobId ?? pdfAccurateJob?.job_id
    const pdfAccurateDocumentId = pdfAccurateJob?.documentId ?? pdfAccurateJob?.document_id
    if (
      pdfAccurateResponse.status !== 202
      || !pdfAccurateJobId
      || !pdfAccurateDocumentId
      || !pdfAccurateJob?.links?.result
    ) {
      throw new Error('POST /artifacts/{graph_id}/ingest/pdf-accurate did not return a hosted-shaped job/document envelope')
    }
    const pdfAccurateResult = await fetchJobResultEventually(
      `${manifest.apiUrl}${pdfAccurateJob.links.result}`,
      { headers: authHeaders },
      240,
      500,
    )
    const expectedPdfAccurateApproach = pdfPipeline.effectiveEngineId
    const expectedPdfAccurateFallback = expectedPdfAccurateApproach === 'pdf.fast-text'
    if (
      pdfAccurateResult.documentId !== pdfAccurateDocumentId
      || pdfAccurateResult.requestedApproachId !== 'pdf.docling-accurate'
      || pdfAccurateResult.ingestionApproachId !== expectedPdfAccurateApproach
      || pdfAccurateResult.localFallback !== expectedPdfAccurateFallback
      || pdfAccurateResult.ocrAvailable !== (expectedPdfAccurateApproach === 'pdf.docling-accurate')
      || pdfAccurateResult.pipelinePreferredEngineId !== pdfPipeline.preferredEngineId
      || pdfAccurateResult.pipelineEffectiveEngineId !== expectedPdfAccurateApproach
      || !Array.isArray(pdfAccurateResult.warnings)
    ) {
      throw new Error(`PDF accurate job result did not expose explicit PDF runtime semantics: ${JSON.stringify(pdfAccurateResult)}`)
    }
    const pdfAccurateDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(pdfAccurateDocumentId)}`,
      { headers: authHeaders },
    )
    if (
      pdfAccurateDocument.title !== pdfAccurateTitle
      || pdfAccurateDocument.readOnly !== true
      || !(pdfAccurateDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(pdfAccurateToken))
    ) {
      throw new Error('PDF accurate facade did not create the expected read-only local PDF document')
    }
    const pdfAccurateOriginal = await fetchBytesResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/documents/${encodeURIComponent(pdfAccurateDocumentId)}/download-original`,
      { headers: authHeaders },
    )
    if (pdfAccurateOriginal.bytes[0] !== 0x25 || pdfAccurateOriginal.bytes[1] !== 0x50) {
      throw new Error('PDF accurate facade did not preserve original PDF bytes')
    }
    const invalidPdfAccurateForm = new FormData()
    invalidPdfAccurateForm.append('file', new Blob([pdfAccurateBytes], { type: 'application/pdf' }), 'not-a-pdf.txt')
    const invalidPdfAccurate = await fetchJsonAnyStatus(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/ingest/pdf-accurate`,
      {
        method: 'POST',
        headers: authHeaders,
        body: invalidPdfAccurateForm,
      },
    )
    if (invalidPdfAccurate.status !== 400) {
      throw new Error('PDF accurate facade did not reject non-PDF filenames with a 400')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(pdfAccurateDocumentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    artifactPdfAccurate = {
      checked: true,
      graphId,
      jobId: pdfAccurateJobId,
      documentId: pdfAccurateDocumentId,
      ingestionApproachId: pdfAccurateResult.ingestionApproachId,
      localFallback: pdfAccurateResult.localFallback,
      invalidStatus: invalidPdfAccurate.status,
    }
    } else {
      artifactPdfAccurate = skipForCellProfile('artifactPdfAccurate: POST /artifacts/{graph_id}/ingest/pdf-accurate (document.uploadIngest / document.ingestMarkdownOriginal)')
    }

    if (!cellSkip('artifactBatchUpload')) {
    const batchKey = `parity-batch-${Date.now()}`
    const batchRoot = `${batchKey}-root`
    const batchNested = `${batchRoot}/nested`
    const batchPrepare = await timedStep('artifactBatchPrepareMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/batch/prepare`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          clientBatchKey: batchKey,
          folders: [batchNested],
        }),
      },
    ))
    const prepared = batchPrepare.body
    if (
      batchPrepare.status !== 201
      || !prepared?.batchId
      || !prepared.folderMap?.[batchRoot]
      || !prepared.folderMap?.[batchNested]
    ) {
      throw new Error('POST /artifacts/{graph_id}/batch/prepare returned an invalid batch response')
    }

    const batchFilename = `${batchKey}.md`
    const batchToken = `batch-token-${Date.now()}`
    const batchMarkdown = `# Batch Upload Smoke

Nested batch content ${batchToken}

- [x] registered

\`\`\`ts
const batch = true
\`\`\``
    const batchForm = new FormData()
    batchForm.append('batch_id', prepared.batchId)
    batchForm.append('file', new Blob([batchMarkdown], { type: 'text/markdown' }), batchFilename)
    const batchUploadResponse = await timedStep('artifactBatchUploadMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/upload`,
      {
        method: 'POST',
        headers: { ...authHeaders, Prefer: 'respond-sync' },
        body: batchForm,
      },
    ))
    const batchUploaded = batchUploadResponse.body
    if (
      batchUploadResponse.status !== 201
      || !batchUploaded?.documentId
      || !batchUploaded.sourceFile?.storageKey
      || batchUploaded.sourceFile.originalFilename !== batchFilename
    ) {
      throw new Error('batch upload did not return sourceFile metadata for registration')
    }

    const batchRegister = await timedStep('artifactBatchRegisterMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/batch/register`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          batchId: prepared.batchId,
          documents: [{
            documentId: batchUploaded.documentId,
            title: batchUploaded.title,
            relativePath: `${batchNested}/${batchFilename}`,
            readOnly: batchUploaded.readOnly,
            sourceFile: batchUploaded.sourceFile,
          }],
        }),
      },
    ))
    if (
      batchRegister.status !== 200
      || batchRegister.body?.registered !== 1
      || (batchRegister.body.failed ?? []).length !== 0
    ) {
      throw new Error('POST /artifacts/{graph_id}/batch/register did not register the uploaded document')
    }
    // A2 item B4 deferred projection flush — batch/upload (respond-sync) writes
    // block content and batch/register assigns parentId; both projections
    // materialize off the request path. Flush before reading document/navigation
    // state so parentId and block content are settled.
    await callTool('flush_crdt', { graphId })
    const batchDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(batchUploaded.documentId)}`,
      { headers: authHeaders },
    )
    const batchBlocks = batchDocument.blocks ?? []
    if (
      batchDocument.id !== batchUploaded.documentId
      || batchDocument.parentId !== prepared.folderMap[batchNested]
      || !batchBlocks.some((block) => block.type === 'heading' && block.content === 'Batch Upload Smoke')
      || !batchBlocks.some((block) => String(block.content ?? '').includes(batchToken))
      || !batchBlocks.some((block) => block.type === 'todo' && block.checked === true)
      || !batchBlocks.some((block) => block.type === 'code' && block.language === 'ts')
    ) {
      throw new Error('batch registered document did not preserve expected workspace/document projection')
    }

    const batchFolders = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders`,
      { headers: authHeaders },
    )
    const batchRootFolder = batchFolders.find((folder) => folder.id === prepared.folderMap[batchRoot])
    const batchNestedFolder = batchFolders.find((folder) => folder.id === prepared.folderMap[batchNested])
    if (
      batchRootFolder?.label !== batchRoot
      || batchRootFolder?.parentId !== null
      || batchNestedFolder?.label !== 'nested'
      || batchNestedFolder?.parentId !== prepared.folderMap[batchRoot]
    ) {
      throw new Error('batch prepare did not materialize the expected nested folder structure')
    }

    const batchOriginalResponse = await fetchWithTimeout(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/documents/${encodeURIComponent(batchUploaded.documentId)}/download-original`,
      { headers: authHeaders },
    )
    if (!batchOriginalResponse.ok || await batchOriginalResponse.text() !== batchMarkdown) {
      throw new Error('batch uploaded original download did not return exact source bytes')
    }

    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(batchUploaded.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(prepared.folderMap[batchNested])}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(prepared.folderMap[batchRoot])}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    artifactBatchUpload = {
      checked: true,
      graphId,
      batchId: prepared.batchId,
      documentId: batchUploaded.documentId,
      folderCount: Object.keys(prepared.folderMap).length,
      blockCount: batchBlocks.length,
    }
    } else {
      artifactBatchUpload = skipForCellProfile('artifactBatchUpload: POST /artifacts/{graph_id}/batch/prepare + upload + batch/register (document.batchPrepare/document.batchRegister/document.uploadIngest)')
    }

    const imageFilename = `parity-image-${Date.now()}.png`
    const imageBytes = Buffer.from(
      'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=',
      'base64',
    )
    const imageForm = new FormData()
    imageForm.append('file', new Blob([imageBytes], { type: 'image/png' }), imageFilename)
    const imageUpload = await timedStep('artifactImageUploadMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/images/upload`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'X-User-ID': 'vera' },
        body: imageForm,
      },
    ))
    const uploadedImage = imageUpload.body
    if (
      imageUpload.status !== 201
      || !uploadedImage?.imageId
      || typeof uploadedImage.src !== 'string'
      || !uploadedImage.src.includes(`/artifacts/${encodeURIComponent(graphId)}/images/${uploadedImage.imageId}`)
      || !uploadedImage.src.includes('token=')
      || !uploadedImage.src.includes('exp=')
      || uploadedImage.src.includes('uid=')
      || !uploadedImage.src.includes(`fn=${encodeURIComponent(imageFilename)}`)
    ) {
      throw new Error('POST /artifacts/{graph_id}/images/upload returned an invalid image response')
    }
    // NOTE: prefix-concatenate rather than new URL(path, base) — apiUrl may
    // carry a path prefix behind a gateway (/g/{graph}), which path-absolute
    // resolution would silently drop.
    const imageUidBypass = await fetchWithTimeout(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/images/${encodeURIComponent(uploadedImage.imageId)}?uid=vera&fn=${encodeURIComponent(imageFilename)}`,
    )
    if (imageUidBypass.status !== 401) {
      throw new Error(`GET uploaded image accepted legacy uid bypass with status ${imageUidBypass.status}`)
    }
    const imageResponse = await fetchWithTimeout(
      uploadedImage.src.startsWith('http')
        ? uploadedImage.src
        : `${new URL(manifest.apiUrl).origin}${uploadedImage.src}`,
    )
    if (!imageResponse.ok) {
      throw new Error(`GET uploaded image returned ${imageResponse.status}`)
    }
    const imageContentType = imageResponse.headers.get('content-type') ?? ''
    const imageDownloaded = Buffer.from(await imageResponse.arrayBuffer())
    if (!imageContentType.startsWith('image/png') || !imageDownloaded.equals(imageBytes)) {
      throw new Error('GET uploaded image did not return exact image bytes and content type')
    }
    const navigationAfterImage = await fetchJson(`${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}`, {
      headers: authHeaders,
    })
    if ((navigationAfterImage.artifacts ?? []).some((artifact) => artifact.id === uploadedImage.imageId)) {
      throw new Error('inline image upload unexpectedly created a sidebar artifact entry')
    }
    artifactImageUpload = {
      checked: true,
      graphId,
      imageId: uploadedImage.imageId,
      downloadedBytes: imageDownloaded.length,
    }

    const artifactId = `parity-artifact-${Date.now()}`
    const artifactBytes = `local artifact bytes for ${artifactId}`
    const artifactPayload = {
      label: 'Parity Artifact',
      parentId: null,
      order: Date.now(),
      fileType: 'txt',
      status: 'ready',
      originalFilename: `${artifactId}.txt`,
      mimeType: 'text/plain',
      dataBase64: Buffer.from(artifactBytes, 'utf8').toString('base64'),
    }
    const putArtifact = await timedStep('artifactMetadataPutMs', () => fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify(artifactPayload),
      },
    ))
    if (
      putArtifact.id !== artifactId
      || putArtifact.graphId !== graphId
      || putArtifact.label !== artifactPayload.label
      || putArtifact.fileType !== 'txt'
      || putArtifact.originalFilename !== artifactPayload.originalFilename
      || putArtifact.mimeType !== artifactPayload.mimeType
      || putArtifact.sizeBytes !== Buffer.byteLength(artifactBytes)
      || !String(putArtifact.storageKey ?? '').startsWith(`local://artifacts/${artifactId}/original/`)
    ) {
      throw new Error('PUT /navigation/{graph_id}/artifacts/{artifact_id} returned an invalid Artifact')
    }
    const readArtifact = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      { headers: authHeaders },
    )
    if (readArtifact.id !== artifactId || readArtifact.storageKey !== putArtifact.storageKey) {
      throw new Error('GET /navigation/{graph_id}/artifacts/{artifact_id} did not read back artifact metadata')
    }
    const artifactDownload = await fetchWithTimeout(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(artifactId)}/download`,
      { headers: authHeaders },
    )
    if (!artifactDownload.ok) {
      throw new Error(`GET /artifacts/{graph_id}/{artifact_id}/download -> ${artifactDownload.status}`)
    }
    const downloadedBytes = await artifactDownload.text()
    if (downloadedBytes !== artifactBytes) {
      throw new Error('artifact download did not return the locally persisted bytes')
    }
    if (!String(artifactDownload.headers.get('content-type') ?? '').startsWith('text/plain')) {
      throw new Error('artifact download did not preserve the stored content type')
    }
    if (!cellSkip('mcpIngestArtifact')) {
    const mcpIngestArtifactJob = await timedStep('mcpIngestArtifactMs', () => callTool('ingest_artifact', {
      graph_id: graphId,
      artifact_id: artifactId,
      mode: 'import',
      title: 'Imported Parity Artifact',
    }))
    if (!mcpJobId(mcpIngestArtifactJob) || mcpIngestArtifactJob.asyncWork !== true) {
      throw new Error('MCP ingest_artifact did not return an async job envelope')
    }
    const ingestedArtifact = await callToolJobResult(mcpIngestArtifactJob)
    if (
      !ingestedArtifact.success
      || ingestedArtifact.artifactId !== artifactId
      || !ingestedArtifact.documentId
      || ingestedArtifact.readOnly !== false
      || ingestedArtifact.autoIngested !== false
    ) {
      throw new Error('MCP ingest_artifact returned an invalid artifact import envelope')
    }
    // A2 item B4 deferred projection flush off the request path; ingest_artifact
    // writes the imported document content via CRDT and the projection may not yet
    // include the new document. Force a flush before reading document state.
    await callTool('flush_crdt', { graphId })
    const ingestedArtifactDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(ingestedArtifact.documentId)}`,
      { headers: authHeaders },
    )
    if (
      ingestedArtifactDocument.title !== 'Imported Parity Artifact'
      || ingestedArtifactDocument.readOnly !== false
      || !(ingestedArtifactDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(artifactBytes))
    ) {
      throw new Error('MCP ingest_artifact did not create the expected editable document projection')
    }
    const ingestedArtifactMetadata = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      { headers: authHeaders },
    )
    if (ingestedArtifactMetadata.ingestedDocId !== ingestedArtifact.documentId || ingestedArtifactMetadata.status !== 'ingested') {
      throw new Error('MCP ingest_artifact did not update artifact ingestion metadata')
    }
    mcpArtifactAdapters = {
      ...mcpArtifactAdapters,
      checked: true,
      artifactId,
      ingestedDocumentId: ingestedArtifact.documentId,
    }
    } else {
      skipForCellProfile('mcpIngestArtifact: MCP ingest_artifact on stored artifact (document.uploadIngest)')
    }
    if (!cellSkip('artifactRouteImportConvert')) {
    const routeImportFolderId = `parity-route-import-folder-${Date.now()}`
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(routeImportFolderId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          label: 'Route Import Folder',
          parentId: null,
          section: 'documents',
          order: Date.now(),
        }),
      },
    )
    const routeImportArtifactId = `parity-route-import-artifact-${Date.now()}`
    const routeImportBytes = `Route import artifact bytes ${Date.now()}`
    const routeImportPayload = {
      label: 'route-import-artifact.txt',
      originalFilename: 'route-import-artifact.txt',
      parentId: null,
      fileType: 'txt',
      status: 'ready',
      mimeType: 'text/plain',
      dataBase64: Buffer.from(routeImportBytes, 'utf8').toString('base64'),
    }
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeImportArtifactId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify(routeImportPayload),
      },
    )
    const routeImportResponse = await timedStep('artifactRouteImportMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(routeImportArtifactId)}/import`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json', Prefer: 'respond-sync' },
        body: JSON.stringify({
          title: 'Route Imported Artifact.txt',
          parentId: routeImportFolderId,
          readOnly: false,
          useYdocPath: false,
        }),
      },
    ))
    const routeImported = routeImportResponse.body
    if (
      routeImportResponse.status !== 201
      || !routeImported?.documentId
      || routeImported.title !== 'Route Imported Artifact'
      || routeImported.readOnly !== false
    ) {
      throw new Error('POST /artifacts/{graph_id}/{artifact_id}/import returned invalid editable import envelope')
    }
    // A2 item B4 deferred projection flush — respond-sync completes the CRDT import
    // write, but the document content and parentId projections materialize off the
    // request path; flush before reading /documents to avoid a stale-projection race.
    await callTool('flush_crdt', { graphId })
    const routeImportedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeImported.documentId)}`,
      { headers: authHeaders },
    )
    if (
      routeImportedDocument.title !== 'Route Imported Artifact'
      || routeImportedDocument.parentId !== routeImportFolderId
      || routeImportedDocument.readOnly !== false
      || !(routeImportedDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(routeImportBytes))
    ) {
      throw new Error('artifact import route did not create editable document content with hosted title and parent semantics')
    }
    const routeImportedMetadata = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeImportArtifactId)}`,
      { headers: authHeaders },
    )
    if (routeImportedMetadata.ingestedDocId != null || routeImportedMetadata.status !== 'ready') {
      throw new Error('editable artifact import route did not clear artifact ingestedDocId and preserve status')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeImported.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeImportArtifactId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/folders/${encodeURIComponent(routeImportFolderId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    const routeDefaultImportArtifactId = `parity-route-default-import-artifact-${Date.now()}`
    const routeDefaultImportBytes = `Route default import artifact bytes ${Date.now()}`
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeDefaultImportArtifactId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          label: 'route-default-import.txt',
          originalFilename: 'route-default-import.txt',
          parentId: null,
          fileType: 'txt',
          status: 'ready',
          mimeType: 'text/plain',
          dataBase64: Buffer.from(routeDefaultImportBytes, 'utf8').toString('base64'),
        }),
      },
    )
    const routeDefaultImportResponse = await fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(routeDefaultImportArtifactId)}/import`,
      {
        method: 'POST',
        headers: { ...authHeaders, Prefer: 'respond-sync' },
      },
    )
    const routeDefaultImported = routeDefaultImportResponse.body
    if (
      routeDefaultImportResponse.status !== 201
      || !routeDefaultImported?.documentId
      || routeDefaultImported.title !== 'route-default-import'
      || routeDefaultImported.readOnly !== false
    ) {
      throw new Error('artifact import route did not honor hosted default body and fallback title semantics')
    }
    // A2 item B4 deferred projection flush — respond-sync unblocks after the CRDT
    // write, but the imported document block projection materializes off the request
    // path; flush before reading /documents to avoid asserting on stale block state.
    await callTool('flush_crdt', { graphId })
    const routeDefaultImportedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeDefaultImported.documentId)}`,
      { headers: authHeaders },
    )
    if (
      routeDefaultImportedDocument.readOnly !== false
      || !(routeDefaultImportedDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(routeDefaultImportBytes))
    ) {
      throw new Error('artifact import route default body did not create editable document content')
    }
    const routeDefaultImportedMetadata = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeDefaultImportArtifactId)}`,
      { headers: authHeaders },
    )
    if (routeDefaultImportedMetadata.ingestedDocId != null) {
      throw new Error('artifact import route default body did not clear artifact ingestedDocId')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeDefaultImported.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeDefaultImportArtifactId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    const routeAsyncImportArtifactId = `parity-route-async-import-artifact-${Date.now()}`
    const routeAsyncImportBytes = `Route async import artifact bytes ${Date.now()}`
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeAsyncImportArtifactId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          label: 'route-async-import.txt',
          originalFilename: 'route-async-import.txt',
          parentId: null,
          fileType: 'txt',
          status: 'ready',
          mimeType: 'text/plain',
          dataBase64: Buffer.from(routeAsyncImportBytes, 'utf8').toString('base64'),
        }),
      },
    )
    const routeAsyncImportResponse = await timedStep('artifactRouteImportAsyncMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(routeAsyncImportArtifactId)}/import`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json', Prefer: 'respond-async' },
        body: JSON.stringify({
          title: 'Route Async Imported Artifact.txt',
          readOnly: false,
        }),
      },
    ))
    const routeAsyncImportJob = routeAsyncImportResponse.body
    const routeAsyncImportJobId = routeAsyncImportJob?.jobId ?? routeAsyncImportJob?.job_id
    if (
      routeAsyncImportResponse.status !== 202
      || !routeAsyncImportJobId
      || !routeAsyncImportJob?.links?.result
      || !['queued', 'running', 'succeeded'].includes(String(routeAsyncImportJob?.status ?? ''))
    ) {
      throw new Error('POST /artifacts import with Prefer: respond-async did not return a job envelope')
    }
    const routeAsyncImported = await fetchJobResultEventually(
      `${manifest.apiUrl}${routeAsyncImportJob.links.result}`,
      { headers: authHeaders },
      240,
      500,
    )
    if (
      !routeAsyncImported?.documentId
      || routeAsyncImported.title !== 'Route Async Imported Artifact'
      || routeAsyncImported.readOnly !== false
    ) {
      throw new Error('async artifact import route returned invalid editable import result')
    }
    const routeAsyncImportedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeAsyncImported.documentId)}`,
      { headers: authHeaders },
    )
    if (
      routeAsyncImportedDocument.readOnly !== false
      || !(routeAsyncImportedDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(routeAsyncImportBytes))
    ) {
      throw new Error('async artifact import route did not create editable document content')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeAsyncImported.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeAsyncImportArtifactId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    const routeIngestArtifactId = `parity-route-ingest-artifact-${Date.now()}`
    const routeIngestBytes = `Route read-only ingest artifact bytes ${Date.now()}`
    const routeIngestPayload = {
      label: 'Route Readonly Artifact',
      originalFilename: 'route-ingest-artifact.txt',
      parentId: null,
      fileType: 'txt',
      status: 'ready',
      mimeType: 'text/plain',
      dataBase64: Buffer.from(routeIngestBytes, 'utf8').toString('base64'),
    }
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeIngestArtifactId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify(routeIngestPayload),
      },
    )
    const routeIngestResponse = await timedStep('artifactRouteIngestMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(routeIngestArtifactId)}/import`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json', Prefer: 'respond-sync' },
        body: JSON.stringify({
          title: 'Route Readonly Artifact',
          parentId: routeImportFolderId,
          readOnly: true,
          useYdocPath: true,
        }),
      },
    ))
    const routeIngested = routeIngestResponse.body
    if (
      routeIngestResponse.status !== 201
      || !routeIngested?.documentId
      || routeIngested.title !== 'Route Readonly Artifact'
      || routeIngested.readOnly !== true
    ) {
      throw new Error('POST /artifacts import readOnly=true returned invalid ingest envelope')
    }
    // A2 item B4 deferred projection flush — respond-sync completes the CRDT ingest
    // write (readOnly=true, useYdocPath), but block content and parentId projections
    // materialize off the request path; flush before reading /documents to avoid a race.
    await callTool('flush_crdt', { graphId })
    const routeIngestedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeIngested.documentId)}`,
      { headers: authHeaders },
    )
    if (
      routeIngestedDocument.parentId != null
      || routeIngestedDocument.readOnly !== true
      || !(routeIngestedDocument.blocks ?? []).some((block) => String(block.content ?? '').includes(routeIngestBytes))
    ) {
      throw new Error('artifact import route did not create hidden read-only document content')
    }
    const routeIngestedMetadata = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeIngestArtifactId)}`,
      { headers: authHeaders },
    )
    if (
      routeIngestedMetadata.ingestedDocId !== routeIngested.documentId
      || routeIngestedMetadata.status !== 'ready'
    ) {
      throw new Error('read-only artifact import route did not link artifact ingestedDocId while preserving status')
    }
    // A2 item B4 deferred projection flush off the request path; the convert
    // route returns before the document.readOnly mutation lands in the
    // projection. Opt into synchronous flush so the subsequent /documents read
    // sees the post-convert state.
    const routeConvertResponse = await timedStep('artifactRouteConvertMs', () => fetchJsonResponse(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(routeIngestArtifactId)}/convert`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json', Prefer: 'wait-for-flush' },
        body: JSON.stringify({
          documentId: routeIngested.documentId,
          title: 'Route Converted Artifact',
        }),
      },
    ))
    const routeConverted = routeConvertResponse.body
    if (
      routeConvertResponse.status !== 200
      || routeConverted.documentId !== routeIngested.documentId
      || routeConverted.title !== 'Route Converted Artifact'
    ) {
      throw new Error('POST /artifacts convert returned invalid envelope')
    }
    const routeConvertedDocument = await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeIngested.documentId)}`,
      { headers: authHeaders },
    )
    if (
      routeConvertedDocument.readOnly !== false
      || routeConvertedDocument.title !== 'Route Readonly Artifact'
    ) {
      throw new Error('artifact convert route did not make document editable while preserving its existing title')
    }
    const routeConvertedMetadata = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeIngestArtifactId)}`,
      { headers: authHeaders },
    )
    if (routeConvertedMetadata.ingestedDocId != null || routeConvertedMetadata.status !== 'ready') {
      throw new Error('artifact convert route did not clear ingestedDocId and preserve artifact status')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(routeIngested.documentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeIngestArtifactId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    const routeMissingFileArtifactId = `parity-route-missing-file-artifact-${Date.now()}`
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeMissingFileArtifactId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          label: 'Missing File Artifact',
          originalFilename: 'missing-file-artifact.txt',
          parentId: null,
          fileType: 'txt',
          status: 'ready',
          mimeType: 'text/plain',
        }),
      },
    )
    const routeMissingFileImport = await fetchJsonAnyStatus(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(routeMissingFileArtifactId)}/import`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json', Prefer: 'respond-sync' },
        body: JSON.stringify({}),
      },
    )
    if (routeMissingFileImport.status !== 400) {
      throw new Error('artifact import route did not reject an artifact with no stored file as a 400')
    }
    await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(routeMissingFileArtifactId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )

    artifactRouteImportConvert = {
      checked: true,
      graphId,
      importedArtifactId: routeImportArtifactId,
      importedDocumentId: routeImported.documentId,
      defaultImportedDocumentId: routeDefaultImported.documentId,
      ingestedArtifactId: routeIngestArtifactId,
      ingestedDocumentId: routeIngested.documentId,
      convertedTitle: routeConverted.title,
      missingFileStatus: routeMissingFileImport.status,
    }
    } else {
      artifactRouteImportConvert = skipForCellProfile('artifactRouteImportConvert: POST /artifacts/{graph_id}/{artifact_id}/import + /convert (document.uploadIngest)')
    }
    const afterPutArtifacts = await fetchJson(`${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts`, {
      headers: authHeaders,
    })
    if (!afterPutArtifacts.some((artifact) => artifact.id === artifactId)) {
      throw new Error('created artifact was not visible in artifact list')
    }
    const artifactUpdatePayload = {
      ...artifactPayload,
      storageKey: putArtifact.storageKey,
      sizeBytes: Buffer.byteLength(artifactBytes),
      label: 'Parity Artifact Updated',
      status: 'processing',
    }
    delete artifactUpdatePayload.dataBase64
    const updatedArtifact = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify(artifactUpdatePayload),
      },
    )
    if (updatedArtifact.label !== 'Parity Artifact Updated' || updatedArtifact.status !== 'processing') {
      throw new Error('artifact metadata update did not roundtrip')
    }
    const deletedArtifact = await fetchJson(
      `${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts/${encodeURIComponent(artifactId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    if (deletedArtifact.id !== artifactId || deletedArtifact.status !== 'deleted') {
      throw new Error('DELETE /navigation/{graph_id}/artifacts/{artifact_id} returned an invalid delete response')
    }
    const deletedArtifactDownload = await fetchWithTimeout(
      `${manifest.apiUrl}/artifacts/${encodeURIComponent(graphId)}/${encodeURIComponent(artifactId)}/download`,
      { headers: authHeaders },
    )
    if (deletedArtifactDownload.status !== 404) {
      throw new Error('deleted artifact original bytes remained downloadable')
    }
    const afterDeleteArtifacts = await fetchJson(`${manifest.apiUrl}/navigation/${encodeURIComponent(graphId)}/artifacts`, {
      headers: authHeaders,
    })
    if (afterDeleteArtifacts.some((artifact) => artifact.id === artifactId)) {
      throw new Error('deleted artifact remained visible in artifact list')
    }
    artifactMutations = {
      checked: true,
      graphId,
      artifactId,
      downloadedBytes: Buffer.byteLength(artifactBytes),
    }

    const searchTerm =
      String(firstBlock?.content ?? hostedDocument.title ?? hostedDocuments[0]?.title ?? 'local')
        .split(/\s+/)
        .find((part) => part.length > 1)
      ?? 'local'
    const hostedBlockSearch = await fetchJson(`${manifest.apiUrl}/search/blocks`, {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ graph_id: graphId, query: searchTerm, limit: 3 }),
    })
    if (!Array.isArray(hostedBlockSearch.results) || typeof hostedBlockSearch.count !== 'number') {
      throw new Error('POST /search/blocks returned an invalid search envelope')
    }
    const hostedHybridSearch = await fetchJson(`${manifest.apiUrl}/search/hybrid`, {
      method: 'POST',
      headers: { ...authHeaders, 'Content-Type': 'application/json' },
      body: JSON.stringify({ graph_id: graphId, query: searchTerm, limit: 3, min_score: 0 }),
    })
    if (!Array.isArray(hostedHybridSearch.results) || typeof hostedHybridSearch.count !== 'number') {
      throw new Error('POST /search/hybrid returned an invalid search envelope')
    }

    const hostedPredicates = await fetchJson(`${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/predicates`, {
      headers: authHeaders,
    })
    if (!Array.isArray(hostedPredicates) || hostedPredicates.length < 1) {
      throw new Error('GET /wires/{graph_id}/predicates returned no predicates')
    }
    const hostedOutgoingWires = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/outgoing`,
      { headers: authHeaders },
    )
    const hostedIncomingWires = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/incoming`,
      { headers: authHeaders },
    )
    const hostedWireBundle = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/bundle`,
      { headers: authHeaders },
    )
    const hostedWiredBlocks = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/wired-blocks`,
      { headers: authHeaders },
    )
    const hostedHasIncoming = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/has-incoming`,
      { headers: authHeaders },
    )
    if (!Array.isArray(hostedOutgoingWires) || !Array.isArray(hostedIncomingWires)) {
      throw new Error('wire direction aliases did not return arrays')
    }
    if (!Array.isArray(hostedWireBundle.outgoing_wires) || !Array.isArray(hostedWireBundle.incoming_wires)) {
      throw new Error('wire bundle alias returned an invalid envelope')
    }
    if (!Array.isArray(hostedWiredBlocks.block_ids) || typeof hostedHasIncoming !== 'boolean') {
      throw new Error('wire indicator aliases returned invalid payloads')
    }

    const wireTargetDocumentId = `parity-wire-target-${Date.now()}`
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(wireTargetDocumentId)}`,
      {
        method: 'PUT',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          title: 'Parity Wire Target',
          blocks: [
            {
              id: `${wireTargetDocumentId}-b1`,
              type: 'paragraph',
              content: 'Target document for hosted wire mutation smoke.',
              parentId: null,
              order: 0,
              marks: [],
            },
          ],
          expectedRevision: 0,
          parentId: null,
        }),
      },
    )
    const sourceBlockId = firstBlock?.blockId ?? firstBlock?.block_id ?? null
    const createdWire = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}`,
      {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({
          source_block_id: sourceBlockId,
          target_graph_id: graphId,
          target_document_id: wireTargetDocumentId,
          predicate: 'supports',
          bidirectional: false,
        }),
      },
    )
    if (
      !createdWire.id
      || createdWire.source_document_id !== documentId
      || createdWire.target_document_id !== wireTargetDocumentId
      || createdWire.predicate_label !== 'supports'
    ) {
      throw new Error('POST /wires/{graph_id}/document/{document_id} returned an invalid Wire')
    }
    // A2 item B4 deferred projection flush off the request path; the wire create
    // CRDT mutation and the target document PUT may not yet be materialized into
    // the projection-backed bundle and incoming reads. Force a flush first.
    await callTool('flush_crdt', { graphId })
    const afterWireBundle = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/bundle`,
      { headers: authHeaders },
    )
    if (!afterWireBundle.outgoing_wires?.some((wire) => wire.id === createdWire.id)) {
      throw new Error('created wire was not visible in source outgoing bundle')
    }
    const targetIncoming = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(wireTargetDocumentId)}/incoming`,
      { headers: authHeaders },
    )
    if (!targetIncoming.some((wire) => wire.id === createdWire.id)) {
      throw new Error('created wire was not visible in target incoming wires')
    }
    const refreshedWire = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/${encodeURIComponent(createdWire.id)}/refresh`,
      {
        method: 'POST',
        headers: authHeaders,
      },
    )
    if (refreshedWire.id !== createdWire.id || !refreshedWire.snapshot_at) {
      throw new Error('POST /wires/{graph_id}/{wire_id}/refresh returned an invalid Wire')
    }
    await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/${encodeURIComponent(createdWire.id)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    // A2 item B4 deferred projection flush off the request path; the wire delete
    // CRDT mutation may not yet be reflected in the projection-backed bundle read.
    // Force a flush before asserting visibility.
    await callTool('flush_crdt', { graphId })
    const afterDeleteBundle = await fetchJson(
      `${manifest.apiUrl}/wires/${encodeURIComponent(graphId)}/document/${encodeURIComponent(documentId)}/bundle`,
      { headers: authHeaders },
    )
    if (afterDeleteBundle.outgoing_wires?.some((wire) => wire.id === createdWire.id)) {
      throw new Error('DELETE /wires/{graph_id}/{wire_id} left the wire visible')
    }
    await fetchJson(
      `${manifest.apiUrl}/documents/${encodeURIComponent(graphId)}/${encodeURIComponent(wireTargetDocumentId)}`,
      {
        method: 'DELETE',
        headers: authHeaders,
      },
    )
    wireMutations = {
      checked: true,
      graphId,
      sourceDocumentId: documentId,
      targetDocumentId: wireTargetDocumentId,
      wireId: createdWire.id,
    }

    const semanticIndex = await fetchJson(
      `${manifest.apiUrl}/api/semantic/index/status/${encodeURIComponent(graphId)}`,
      { headers: authHeaders },
    )
    let semanticRouteChecked = false
    if (semanticIndex.exists) {
      const semanticSearch = await fetchJson(`${manifest.apiUrl}/search`, {
        method: 'POST',
        headers: { ...authHeaders, 'Content-Type': 'application/json' },
        body: JSON.stringify({ graph_id: graphId, query: searchTerm, limit: 3, min_score: 0 }),
      })
      if (!Array.isArray(semanticSearch.results) || typeof semanticSearch.total_count !== 'number') {
        throw new Error('POST /search returned an invalid semantic search envelope')
      }
      semanticRouteChecked = true
    }

    readAdapters = {
      checked: true,
      graphId,
      documentId,
      blockCount: blocks.total_blocks ?? blocks.totalBlocks ?? 0,
      predicateCount: predicates.count ?? predicates.predicates.length,
      wireCount: wires.count ?? wires.wires.length,
    }
    hostedAliases = {
      checked: true,
      graphId,
      documentId,
      documents: hostedDocuments.length,
      folders: hostedFolders.length,
      artifacts: hostedArtifacts.length,
      predicates: hostedPredicates.length,
      searchTerm,
      semanticRouteChecked,
    }
  }
}

const finalCrdtTimings = await fetchJson(`${manifest.apiUrl}/api/local/crdt-timings?limit=200`, { headers: authHeaders })
const timingComparisons = [
  compareTimings('artifactUploadPdfMs', 'artifactUploadMarkdownMs'),
  compareTimings('artifactUploadEpubMs', 'artifactUploadHtmlMs'),
  compareTimings('artifactBatchUploadMs', 'artifactUploadMarkdownMs'),
  compareTimings('mcpWriteDocumentMs', 'documentWriteSmokeMs'),
].filter(Boolean)
const performanceBudgets = evaluatePerformanceBudgets()
const enforcedBudgetFailures = performanceBudgets.filter((budget) => budget.status === 'failed')
if (enforcedBudgetFailures.length > 0) {
  throw new Error(`Performance budgets failed: ${enforcedBudgetFailures.map((budget) =>
    `${budget.operation} ${budget.observedMs}ms > ${budget.maxMs}ms`,
  ).join(', ')}`)
}

console.log(
  JSON.stringify(
    {
      ok: true,
      apiUrl: manifest.apiUrl,
      pid: manifest.pid,
      openapi: {
        paths: Object.keys(openapi.paths ?? {}).length,
        operations: openapiRouteKeys.size,
      },
      expectedImplementedTools: expected.mcpTools.filter((tool) =>
        tool.status.startsWith('implemented'),
      ).length,
      actualTools: actualTools.size,
      ...(CELL_PROFILE ? { profile: 'cell', 'skipped (cell profile)': cellSkippedSections } : {}),
      materialization,
      mcpOrientation,
      mcpWorkspaceManagement,
      mcpWireAdapters,
      graphMetadata,
      graphSupportAliases,
      mcpGraphJobs,
      graphAnalytics,
      graphExport,
      graphArchiveImport,
      graphRdfImport,
      graphVaultImport,
      graphWebImports,
      searchMaintenance,
      mcpMemoryAdapters,
      mcpNarrativeSurface,
      mcpDocumentHistory,
      documentHistoryApi,
      navigationFolder,
      navigationJobResult,
      documentExport,
      documentBlobs,
      documentDescription,
      documentDuplicate,
      documentFlush,
      readAdapters,
      hostedAliases,
      entityAliases,
      mcpWriteAdapters,
      mcpDocumentEditability,
      mcpArtifactAdapters,
      mcpCommentAdapters,
      mcpValuationAdapters,
      blockMutations,
      wireMutations,
      artifactMutations,
      artifactRouteImportConvert,
      artifactUpload,
      artifactPdfAccurate,
      artifactFormatUpload,
      artifactBatchUpload,
      artifactImageUpload,
      timings: {
        operationMs: operationTimings,
        comparisons: timingComparisons,
        budgets: performanceBudgets,
        crdtByKind: summarizeCrdtTraces(finalCrdtTimings.traces ?? []),
      },
    },
    null,
    2,
  ),
)

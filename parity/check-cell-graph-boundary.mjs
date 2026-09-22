#!/usr/bin/env node

import crypto from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const read = (relative) => fs.readFileSync(path.join(root, relative), 'utf8')
const readJson = (relative) => JSON.parse(read(relative))

const policy = readJson('src-tauri/src/cell_graph_boundary_policy.json')
const openapi = readJson('parity/local-openapi.json')
const effectRegistryText = read('parity/local-loopback-surface.json')
const surface = JSON.parse(effectRegistryText)
const mcpCatalog = readJson('src-tauri/src/mcp_tool_catalog.json')
const poisoned = readJson(
  'parity/fixtures/cell-graph-boundary-poisoned.json',
)

const fail = (message) => {
  console.error(`cell graph boundary: ${message}`)
  process.exitCode = 1
}

const asSet = (name) => {
  const values = policy[name]
  if (!Array.isArray(values)) {
    fail(`${name} must be an array`)
    return new Set()
  }
  const set = new Set(values)
  if (set.size !== values.length) fail(`${name} contains duplicate entries`)
  return set
}

const restSets = {
  global: asSet('restGlobal'),
  pathGraph: asSet('restPathGraph'),
  bodyGraph: asSet('restBodyGraph'),
  operationGraph: asSet('restOperationGraph'),
  jobGraph: asSet('restJobGraph'),
  profileDenied: asSet('restProfileDenied'),
}
const deniedAnyMethod = asSet('restProfileDeniedAnyMethod')
const mcpSets = {
  global: asSet('mcpGlobal'),
  jobGraph: asSet('mcpJobGraph'),
  operationGuarded: asSet('mcpOperationGuarded'),
  profileDenied: asSet('mcpProfileDenied'),
  graphScoped: asSet('mcpGraphScoped'),
}
const mcpJobCarriers = policy.mcpJobCarriers ?? {}

const assertDisjoint = (sets, family) => {
  const entries = Object.entries(sets)
  for (let index = 0; index < entries.length; index += 1) {
    const [leftName, left] = entries[index]
    for (const [rightName, right] of entries.slice(index + 1)) {
      for (const value of left) {
        if (right.has(value)) {
          fail(`${family} policy overlap: ${leftName}/${rightName}: ${value}`)
        }
      }
    }
  }
}
assertDisjoint(restSets, 'REST')
assertDisjoint(mcpSets, 'MCP')
if (
  [...mcpSets.jobGraph].sort().join('\n') !==
  Object.keys(mcpJobCarriers).sort().join('\n')
) {
  fail('mcpJobCarriers keys must exactly match mcpJobGraph')
}

const surfaceSha256 = (values) =>
  crypto
    .createHash('sha256')
    .update([...values].sort().join('\n') + '\n')
    .digest('hex')

const effectRegistrySha256 = crypto
  .createHash('sha256')
  .update(effectRegistryText)
  .digest('hex')
if (effectRegistrySha256 !== policy.effectRegistrySha256) {
  fail(
    `loopback effect registry changed: policy=${policy.effectRegistrySha256}, actual=${effectRegistrySha256}`,
  )
}

const openapiOperations = new Set()
for (const [route, operations] of Object.entries(openapi.paths ?? {})) {
  for (const method of Object.keys(operations)) {
    openapiOperations.add(`${method.toUpperCase()} ${route}`)
  }
}
const restSurfaceSha256 = surfaceSha256(openapiOperations)
if (restSurfaceSha256 !== policy.restSurfaceSha256) {
  fail(
    `REST surface digest changed: policy=${policy.restSurfaceSha256}, actual=${restSurfaceSha256}`,
  )
}

const restEffects = new Map()
for (const entry of surface.routes ?? []) {
  const key = `${String(entry.method).toUpperCase()} ${entry.path}`
  if (restEffects.has(key)) fail(`duplicate REST effect entry: ${key}`)
  restEffects.set(key, entry)
}
for (const operation of openapiOperations) {
  if (!restEffects.has(operation)) {
    fail(`OpenAPI operation has no authoritative effect entry: ${operation}`)
  }
}
for (const operation of restEffects.keys()) {
  if (!openapiOperations.has(operation)) {
    fail(`REST effect entry is absent from OpenAPI: ${operation}`)
  }
}

const restCounts = {
  global: 0,
  bodyGraph: 0,
  operationGraph: 0,
  jobGraph: 0,
  profileDenied: 0,
  pathGraph: 0,
}
for (const operation of openapiOperations) {
  const route = operation.slice(operation.indexOf(' ') + 1)
  const classes = Object.entries(restSets)
    .filter(([, set]) => set.has(operation))
    .map(([name]) => name)
  if (classes.length !== 1) {
    fail(
      `${operation} must have exactly one policy (found ${
        classes.join(', ') || 'none'
      })`,
    )
    continue
  }
  restCounts[classes[0]] += 1
}
for (const [name, entries] of Object.entries(restSets)) {
  for (const operation of entries) {
    if (!openapiOperations.has(operation)) {
      fail(`${name} classifies a REST operation absent from OpenAPI: ${operation}`)
    }
  }
}

for (const required of [
  'DELETE /graphs/{graph_id}',
  'POST /graphs/{graph_id}/duplicate',
  'GET /api/graphs',
  'GET /graphs',
  'GET /graphs/stats',
  'POST /api/graphs',
  'POST /graphs',
  'POST /graphs/import',
  'GET /api/profile',
]) {
  if (!restSets.profileDenied.has(required)) {
    fail(`required profile/lifecycle denial is missing: ${required}`)
  }
}
for (const required of [
  '/services/{service_id}',
  '/services/{service_id}/{*path}',
]) {
  if (!deniedAnyMethod.has(required)) {
    fail(`opaque service proxy denial is missing: ${required}`)
  }
}
const opaqueServiceSource = read('src-tauri/src/loopback_service_routes.rs')
const opaqueRoutes = new Set(
  [...opaqueServiceSource.matchAll(
    /\.route\(\s*"([^"]+)"\s*,\s*any\(/g,
  )].map((match) => match[1]),
)
for (const route of opaqueRoutes) {
  if (!deniedAnyMethod.has(route)) {
    fail(`opaque any-method service route is not denied: ${route}`)
  }
}
for (const route of deniedAnyMethod) {
  if (!opaqueRoutes.has(route)) {
    fail(`denied any-method route is not registered as opaque service proxy: ${route}`)
  }
}
if (!restSets.operationGraph.has('POST /api/crdt/operations')) {
  fail('CRDT REST ingress must have an operation-aware graph policy')
}

for (const [operation, effect] of restEffects) {
  if (
    (effect.scopes ?? []).some((scope) =>
      ['services.manage', 'services.proxy'].includes(scope),
    )
  ) {
    const route = operation.slice(operation.indexOf(' ') + 1)
    if (
      !restSets.profileDenied.has(operation) &&
      !deniedAnyMethod.has(route)
    ) {
      fail(`service-effect REST operation is not denied: ${operation}`)
    }
  }
}

const mcpEffects = new Map()
for (const entry of surface.mcpTools ?? []) {
  if (mcpEffects.has(entry.name)) {
    fail(`duplicate MCP effect entry: ${entry.name}`)
  }
  mcpEffects.set(entry.name, entry)
}

const graphSelectorKeys = new Set(['graphId', 'graph_id'])
const catalogNames = new Set()
const mcpCounts = {
  global: 0,
  jobGraph: 0,
  operationGuarded: 0,
  profileDenied: 0,
  graphScoped: 0,
}
for (const tool of mcpCatalog.tools ?? []) {
  const name = tool.name
  if (typeof name !== 'string' || name.length === 0) {
    fail('catalog tool is missing a name')
    continue
  }
  if (catalogNames.has(name)) fail(`duplicate MCP tool: ${name}`)
  catalogNames.add(name)
  const effect = mcpEffects.get(name)
  if (!effect) fail(`MCP tool ${name} has no authoritative effect entry`)
  const classes = Object.entries(mcpSets)
    .filter(([, set]) => set.has(name))
    .map(([className]) => className)
  if (classes.length !== 1) {
    fail(`MCP tool ${name} has ${classes.length} policies: ${classes.join(', ')}`)
    continue
  }
  if (classes[0] === 'graphScoped') {
    const properties = tool.inputSchema?.properties ?? {}
    if (![...graphSelectorKeys].some((key) => key in properties)) {
      fail(`explicitly graph-scoped MCP tool ${name} has no graph selector`)
    }
  }
  if (classes[0] === 'jobGraph') {
    const properties = tool.inputSchema?.properties ?? {}
    const carrierKeys = mcpJobCarriers[name] ?? []
    if (
      carrierKeys.length === 0 ||
      carrierKeys.some((key) => !(key in properties))
    ) {
      fail(`MCP job tool ${name} has an invalid resource-carrier policy`)
    }
  }
  mcpCounts[classes[0]] += 1
}
const mcpSurfaceSha256 = surfaceSha256(catalogNames)
if (mcpSurfaceSha256 !== policy.mcpSurfaceSha256) {
  fail(
    `MCP surface digest changed: policy=${policy.mcpSurfaceSha256}, actual=${mcpSurfaceSha256}`,
  )
}
for (const name of catalogNames) {
  if (!mcpEffects.has(name)) fail(`MCP effect registry is missing ${name}`)
}
for (const name of mcpEffects.keys()) {
  if (!catalogNames.has(name)) fail(`MCP effect registry has unknown tool ${name}`)
}
for (const [name, entries] of Object.entries(mcpSets)) {
  for (const toolName of entries) {
    if (!catalogNames.has(toolName)) {
      fail(`${name} classifies an MCP tool absent from the catalog: ${toolName}`)
    }
  }
}

for (const required of [
  'create_graph',
  'duplicate_graph',
  'graph_intuition',
  'list_graphs',
  'manage_graph',
  'upload_artifact',
  'workflow_authoring_session',
  'workflow_run_monitor',
  'workflow_run_start',
]) {
  if (!mcpSets.profileDenied.has(required)) {
    fail(`required MCP profile/effect denial is missing: ${required}`)
  }
}
for (const required of ['crdt_operation', 'delete']) {
  if (!mcpSets.operationGuarded.has(required)) {
    fail(`required operation-aware MCP guard is missing: ${required}`)
  }
}
for (const required of [
  'cancel_job',
  'cancel_restore_operation',
  'get_job_result',
  'get_job_status',
  'get_restore_operation',
]) {
  if (!mcpSets.jobGraph.has(required)) {
    fail(`required MCP resource-carrier guard is missing: ${required}`)
  }
}
for (const [name, effect] of mcpEffects) {
  const scopes = effect.scopes ?? []
  if (
    scopes.some((scope) =>
      ['services.manage', 'services.proxy'].includes(scope),
    ) &&
    !mcpSets.profileDenied.has(name)
  ) {
    fail(`service-effect MCP tool is not denied: ${name}`)
  }
  if (
    scopes.includes('graphs.delete') &&
    !mcpSets.profileDenied.has(name) &&
    !mcpSets.operationGuarded.has(name)
  ) {
    fail(`graph-delete MCP tool lacks operation guard: ${name}`)
  }
  if (
    scopes.includes('graphs.import') &&
    !mcpSets.profileDenied.has(name) &&
    !mcpSets.operationGuarded.has(name)
  ) {
    fail(`graph-import MCP tool lacks operation guard: ${name}`)
  }
}

const bodyGraphSourceFiles = [
  'src-tauri/src/loopback_ai_routes.rs',
  'src-tauri/src/loopback_crdt_routes.rs',
  'src-tauri/src/loopback_graph_routes.rs',
  'src-tauri/src/loopback_graph_job_routes.rs',
  'src-tauri/src/loopback_rdf_routes.rs',
  'src-tauri/src/loopback_semantic_routes.rs',
]
const bodyGraphSource = bodyGraphSourceFiles.map(read).join('\n')
const bodyRouteHandlers = new Map()
for (const match of bodyGraphSource.matchAll(
  /\.route\(\s*"([^"]+)"\s*,\s*post\(\s*([a-zA-Z0-9_]+)\s*\)/g,
)) {
  bodyRouteHandlers.set(`POST ${match[1]}`, match[2])
}
for (const operation of new Set([
  ...restSets.bodyGraph,
  ...restSets.operationGraph,
])) {
  const handler = bodyRouteHandlers.get(operation)
  if (!handler) {
    fail(`cannot resolve body-graph route handler for ${operation}`)
    continue
  }
  const signature = bodyGraphSource.match(
    new RegExp(`async\\s+fn\\s+${handler}\\b[\\s\\S]*?\\{`),
  )?.[0]
  if (!signature?.includes('CellGraphJson')) {
    fail(`${operation} handler ${handler} does not use CellGraphJson`)
  }
}

const rustFunction = (source, name) => {
  const startMatch = source.match(
    new RegExp(
      `(?:pub(?:\\([^)]*\\))?\\s+)?(?:async\\s+)?fn\\s+${name}\\b`,
    ),
  )
  if (!startMatch || startMatch.index === undefined) return undefined
  const start = startMatch.index
  const remainder = source.slice(start + startMatch[0].length)
  const next = remainder.search(
    /\n(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+[a-zA-Z0-9_]+\b/,
  )
  return next < 0
    ? source.slice(start)
    : source.slice(start, start + startMatch[0].length + next)
}

const jobRouteSources = [
  'src-tauri/src/loopback_graph_routes.rs',
  'src-tauri/src/loopback_navigation_routes.rs',
  'src-tauri/src/loopback_restore_routes.rs',
  'src-tauri/src/loopback_semantic_routes.rs',
].map(read)
const jobHandlerSources = [
  'src-tauri/src/loopback_graph_job_routes.rs',
  'src-tauri/src/loopback_navigation_read_routes.rs',
  'src-tauri/src/loopback_restore_routes.rs',
  'src-tauri/src/loopback_semantic_routes.rs',
].map(read)
const jobHandlers = policy.restJobHandlers ?? {}
if (
  JSON.stringify(Object.keys(jobHandlers).sort()) !==
  JSON.stringify([...restSets.jobGraph].sort())
) {
  fail('restJobHandlers must map every and only restJobGraph operation')
}
for (const [operation, handler] of Object.entries(jobHandlers)) {
  const [method, route] = operation.split(' ', 2)
  const routeChunk = jobRouteSources
    .map((source) => {
      const at = source.indexOf(`"${route}"`)
      return at < 0 ? '' : source.slice(at, at + 500)
    })
    .join('\n')
  if (
    !new RegExp(
      `\\.${method.toLowerCase()}\\(\\s*${handler}\\s*\\)|${method.toLowerCase()}\\(\\s*${handler}\\s*\\)`,
    ).test(routeChunk)
  ) {
    fail(`${operation} is not authoritatively bound to ${handler}`)
  }
  const body = jobHandlerSources
    .map((source) => rustFunction(source, handler))
    .find(Boolean)
  if (!body) {
    fail(`cannot resolve job handler body ${handler}`)
  } else if (!/\.cell_graph\s*\.authorize_job_id\(/.test(body)) {
    fail(`${operation} handler ${handler} lacks its own job ownership guard`)
  }
}

const boundarySource = read('src-tauri/src/cell_graph_boundary.rs')
const runtimeSource = read('src-tauri/src/tauri_runtime.rs')
const serverSource = read('src-tauri/src/loopback_server.rs')
const queueSource = read('src-tauri/src/crdt_queue.rs')
const graphPathsSource = read('src-tauri/src/graph_paths.rs')
const schedulerSource = read(
  'src-tauri/src/time_travel_interval_scheduler.rs',
)
const dispatchSource = read('src-tauri/src/mcp_tool_dispatch.rs')
const uploadSource = read('src-tauri/src/artifact_upload_service.rs')
const wireSource = read('src-tauri/src/loopback_wire_routes.rs')

const setupBody = rustFunction(runtimeSource, 'setup_core') ?? ''
for (const anchor of [
  'CellGraphBoundary::from_process_env()',
  'register_managed_state(handle, cell_graph)',
  'recover_local_crdt_operations(handle.clone())',
]) {
  if (!setupBody.includes(anchor)) fail(`setup_core is missing ${anchor}`)
}
if (
  !(
    setupBody.indexOf('CellGraphBoundary::from_process_env()') <
      setupBody.indexOf('register_managed_state(handle, cell_graph)') &&
    setupBody.indexOf('register_managed_state(handle, cell_graph)') <
      setupBody.indexOf('recover_local_crdt_operations(handle.clone())')
  )
) {
  fail('cell owner must be installed before recovery')
}
if (
  serverSource.includes('CellGraphBoundary::from_process_env()') ||
  !serverSource.includes('.state::<Arc<CellGraphBoundary>>()')
) {
  fail('loopback server must reuse the pre-recovery managed boundary')
}

const recoveryBody = rustFunction(queueSource, 'recover_crdt_operations') ?? ''
if (
  !recoveryBody.includes('for operation in &operations') ||
  !recoveryBody.includes('.authorize_crdt_operation(') ||
  recoveryBody.indexOf('.authorize_crdt_operation(') >
    recoveryBody.indexOf('let queue = app.state::<CrdtOperationQueue>()')
) {
  fail('the complete recovered prefix must be owner-validated before enqueue')
}
const enqueueBody =
  rustFunction(queueSource, 'enqueue_crdt_operation_outcome_inner') ?? ''
const enqueueGraphState = enqueueBody.indexOf(
  'coordinator.acquire_hot_write(&input.graph_id)',
)
if (
  !enqueueBody.includes('.authorize_crdt_operation(') ||
  enqueueGraphState < 0 ||
  enqueueBody.indexOf('.authorize_crdt_operation(') > enqueueGraphState
) {
  fail('fresh internal CRDT work must be guarded before graph state')
}
const selfHealBody = rustFunction(graphPathsSource, 'self_heal_missing_graph') ?? ''
const selfHealAuthority =
  rustFunction(graphPathsSource, 'authorize_cell_self_heal') ?? ''
if (
  !selfHealAuthority.includes('.authorize_graph_self_heal(graph_id)') ||
  !selfHealBody.includes('authorize_cell_self_heal(app, graph_id)') ||
  selfHealBody.indexOf('authorize_cell_self_heal(app, graph_id)') >
    selfHealBody.indexOf('create_graph_service_inner(')
) {
  fail('F4c must authorize the process-bound graph before creating storage')
}

const scheduledGraphsBody = rustFunction(schedulerSource, 'scheduled_graphs') ?? ''
for (const anchor of [
  'owner_graph_id()',
  'read_graph_record_no_heal(app, owner_graph_id)',
  'list_graphs(app.clone())',
]) {
  if (!scheduledGraphsBody.includes(anchor)) {
    fail(`scheduler owner confinement is missing ${anchor}`)
  }
}
if (
  scheduledGraphsBody.indexOf('read_graph_record_no_heal(app, owner_graph_id)') >
  scheduledGraphsBody.indexOf('list_graphs(app.clone())')
) {
  fail('cell scheduler must select the owner without enumerating the profile')
}

if (
  !boundarySource.includes('"graphid" | "sourcegraphid" | "targetgraphid" | "scenegraphid"') ||
  !boundarySource.includes('"newgraphid"') ||
  !boundarySource.includes('normalized.ends_with("graphid")') ||
  !boundarySource.includes('serde_json::Value::Array(values)')
) {
  fail('recursive selector guard is missing a required selector/fail-closed branch')
}
if (
  !boundarySource.includes('MCP delete(type=graph)') ||
  !boundarySource.includes('CRDT operation graph.importArchive')
) {
  fail('operation-aware graph lifecycle denials are missing')
}
if (
  dispatchSource.indexOf('mcp_call_required_scopes(name, &arguments)') >
  dispatchSource.indexOf('.scope_mcp_arguments(name, arguments, &state.jobs)')
) {
  fail('MCP route-specific scope auth must precede owner disclosure')
}
if (
  dispatchSource.indexOf('.scope_mcp_arguments(name, arguments, &state.jobs)') >
  dispatchSource.indexOf('(entry.handler)(&ctx, &arguments)')
) {
  fail('MCP boundary must run before handler effects')
}
if (
  !uploadSource.includes('fs::metadata(&path)') ||
  !mcpSets.profileDenied.has('upload_artifact')
) {
  fail('arbitrary host-path upload effect is not fail-closed')
}
const wireCreateBody =
  rustFunction(wireSource, 'loopback_hosted_create_wire') ?? ''
if (
  wireCreateBody.indexOf('authorize_json_graph_references(&input)') < 0 ||
  wireCreateBody.indexOf('authorize_json_graph_references(&input)') >
    wireCreateBody.indexOf('enqueue_crdt_operation_outcome(')
) {
  fail('REST wire payload selectors must be guarded before enqueue')
}

const pathPolicy = boundarySource.indexOf('RestPolicy::PathGraph =>')
const pathEffect = boundarySource.indexOf('authorize_rest_effect(', pathPolicy)
const pathOwner = boundarySource.indexOf('authorize_graph_id(graph_id)', pathPolicy)
if (!(pathPolicy >= 0 && pathEffect > pathPolicy && pathEffect < pathOwner)) {
  fail('REST path scope authorization must precede owner comparison')
}
const extractor = boundarySource.indexOf('async fn from_request')
const extractorEffect = boundarySource.indexOf('authorize_rest_effect(', extractor)
const extractorOwner = boundarySource.indexOf('scope_graph_arguments(', extractor)
if (!(extractor >= 0 && extractorEffect > extractor && extractorEffect < extractorOwner)) {
  fail('REST body scope authorization must precede owner comparison')
}
if (
  !boundarySource.includes('anonymous_signed_image') ||
  !boundarySource.includes('"missing or invalid image access token"')
) {
  fail('anonymous signed-image mismatch must match invalid-token semantics')
}
if (
  !boundarySource.includes('if !loopback_state.cell_graph.is_enabled()') ||
  !boundarySource.includes('Json::<T>::from_request(request, state)')
) {
  fail('desktop/local JSON extraction no-op path is missing')
}

const inspectSelectors = (owner, value, depth = 0) => {
  if (depth > 64) return 'invalid'
  if (Array.isArray(value)) {
    for (const nested of value) {
      const result = inspectSelectors(owner, nested, depth + 1)
      if (result !== 'allow') return result
    }
    return 'allow'
  }
  if (!value || typeof value !== 'object') return 'allow'
  for (const [key, nested] of Object.entries(value)) {
    const normalized = key.replace(/[^a-zA-Z0-9]/g, '').toLowerCase()
    if (normalized === 'newgraphid') {
      if (typeof nested !== 'string' || nested.trim() === '') return 'invalid'
      return 'profile-denied'
    }
    if (
      ['graphid', 'sourcegraphid', 'targetgraphid', 'scenegraphid'].includes(
        normalized,
      )
    ) {
      if (typeof nested !== 'string' || nested.trim() === '') return 'invalid'
      if (nested.trim() !== owner) return 'graph-mismatch'
    } else if (
      normalized.endsWith('graphid') ||
      normalized.endsWith('graphids')
    ) {
      return 'invalid'
    }
    const result = inspectSelectors(owner, nested, depth + 1)
    if (result !== 'allow') return result
  }
  return 'allow'
}

const inspectOperation = (owner, operation) => {
  if (
    typeof operation.graphId !== 'string' ||
    operation.graphId.trim() !== owner
  ) {
    return 'graph-mismatch'
  }
  if (operation.kind?.trim() === 'graph.importArchive') {
    return 'profile-denied'
  }
  return inspectSelectors(owner, operation.payload)
}

let negativeFixtureCount = 0
for (const test of poisoned.selectorCases ?? []) {
  negativeFixtureCount += 1
  const actual = inspectSelectors(poisoned.ownerGraphId, test.value)
  if (actual !== test.result) {
    fail(`poisoned selector fixture "${test.name}": ${actual} != ${test.result}`)
  }
}
for (const test of poisoned.recoveredOperations ?? []) {
  negativeFixtureCount += 1
  const actual = inspectOperation(poisoned.ownerGraphId, test)
  if (actual !== test.result) {
    fail(`poisoned recovery fixture "${test.name}": ${actual} != ${test.result}`)
  }
}
for (const test of poisoned.mcpOperationCases ?? []) {
  negativeFixtureCount += 1
  let actual = inspectSelectors(poisoned.ownerGraphId, test.arguments)
  if (actual === 'allow' && test.tool === 'delete') {
    actual = ['graph', 'graphs'].includes(
      String(test.arguments.type).toLowerCase(),
    )
      ? 'profile-denied'
      : 'allow'
  }
  if (actual === 'allow' && test.tool === 'crdt_operation') {
    actual = inspectOperation(poisoned.ownerGraphId, test.arguments)
  }
  if (actual !== test.result) {
    fail(`poisoned MCP fixture "${test.name}": ${actual} != ${test.result}`)
  }
}

const scheduled = (poisoned.profileGraphs ?? []).filter(
  (graphId) => graphId === poisoned.ownerGraphId,
)
if (
  JSON.stringify(scheduled) !==
  JSON.stringify(poisoned.scheduler?.expectedGraphs)
) {
  fail('poisoned two-graph scheduler fixture escaped owner confinement')
}
negativeFixtureCount += 1

if (
  poisoned.localFile?.result !== 'profile-denied' ||
  !mcpSets.profileDenied.has(poisoned.localFile?.tool)
) {
  fail('poisoned arbitrary-local-file fixture is not denied')
}
negativeFixtureCount += 1
for (const tool of poisoned.serviceSpawnTools ?? []) {
  negativeFixtureCount += 1
  if (!mcpSets.profileDenied.has(tool)) {
    fail(`poisoned service-spawn fixture is not denied: ${tool}`)
  }
  const effects = mcpEffects.get(tool)?.scopes ?? []
  if (
    !effects.some((scope) =>
      ['services.manage', 'services.proxy'].includes(scope),
    )
  ) {
    fail(`service-spawn fixture lacks authoritative service effect: ${tool}`)
  }
}

if (!process.exitCode) {
  console.log(
    `cell graph boundary: ${openapiOperations.size} REST operations ` +
      `(${Object.entries(restCounts)
        .map(([name, count]) => `${name}=${count}`)
        .join(', ')}); ` +
      `${catalogNames.size} MCP tools ` +
      `(${Object.entries(mcpCounts)
        .map(([name, count]) => `${name}=${count}`)
        .join(', ')}); ` +
      `${negativeFixtureCount} poisoned negative fixtures`,
  )
}

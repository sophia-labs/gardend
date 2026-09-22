#!/usr/bin/env node
import fs from 'node:fs/promises'
import path from 'node:path'

const here = path.dirname(new URL(import.meta.url).pathname)
const prototypeRoot = path.resolve(here, '..')
const repoRoot = process.env.MNEMOSYNE_PLATFORM_ROOT
  ? path.resolve(process.env.MNEMOSYNE_PLATFORM_ROOT)
  : prototypeRoot
const mcpRoot = process.env.MNEMOSYNE_MCP_ROOT
  ? path.resolve(process.env.MNEMOSYNE_MCP_ROOT)
  : path.resolve(repoRoot, '../mnemosyne-mcp')

const classificationPath = path.join(here, 'surface-classification.json')
const localSurfacePath = path.join(here, 'local-loopback-surface.json')
const localOpenApiPath = path.join(here, 'local-openapi.json')
const surfaceContractsPath = path.join(here, 'surface-contracts.json')
const mcpSchemaSnapshotPath = path.join(here, 'mcp-tool-schema-snapshot.json')
const routeEnvelopeSnapshotPath = path.join(here, 'route-envelope-snapshot.json')
const rustSourceDir = path.join(prototypeRoot, 'src-tauri/src')
const localMcpCatalogPath = path.join(rustSourceDir, 'mcp_tool_catalog.json')
const rustLoopbackScopesPath = path.join(rustSourceDir, 'loopback_scopes.rs')
const rustLoopbackTokenGrantsPath = path.join(rustSourceDir, 'loopback_token_grants.rs')
const rustMcpDispatchRegistryPath = path.join(rustSourceDir, 'mcp_dispatch_registry.rs')
const rustLoopbackScopeCatalogPaths = [
  'loopback_scope_catalog.rs',
  'loopback_system_scopes.rs',
  'loopback_workspace_scopes.rs',
  'loopback_knowledge_scopes.rs',
].map((fileName) => path.join(rustSourceDir, fileName))

const classification = JSON.parse(await fs.readFile(classificationPath, 'utf8'))
const localSurface = JSON.parse(await fs.readFile(localSurfacePath, 'utf8'))
const localOpenApi = JSON.parse(await fs.readFile(localOpenApiPath, 'utf8'))
const surfaceContracts = JSON.parse(await fs.readFile(surfaceContractsPath, 'utf8'))
const mcpSchemaSnapshot = JSON.parse(await fs.readFile(mcpSchemaSnapshotPath, 'utf8'))
const routeEnvelopeSnapshot = JSON.parse(await fs.readFile(routeEnvelopeSnapshotPath, 'utf8'))
const localMcpCatalog = JSON.parse(await fs.readFile(localMcpCatalogPath, 'utf8'))

const routeMethods = ['get', 'post', 'put', 'patch', 'delete', 'websocket']
const validClassifications = new Set(classification.classificationValues)
const validContractStatuses = new Set(surfaceContracts.statusValues ?? [])
const validSchemaStatuses = new Set(surfaceContracts.schemaStatusValues ?? [])
const validSemanticStatuses = new Set(surfaceContracts.semanticStatusValues ?? [])
const validExposureValues = new Set(surfaceContracts.exposureValues ?? [])
const validEnvelopeKinds = new Set(['any', 'array', 'binary', 'boolean', 'object'])
const validRouteScopeModes = new Set([
  'all',
  'bearer-token',
  'public',
  'json-rpc-method',
  'bearer-or-signed-url',
  'operation-kind',
])
const validMcpRequiredScopeModes = new Set(['all', 'operation-kind'])

async function walk(dir, predicate) {
  const entries = await fs.readdir(dir, { withFileTypes: true })
  const files = []
  for (const entry of entries) {
    const fullPath = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      files.push(...await walk(fullPath, predicate))
    } else if (!predicate || predicate(fullPath)) {
      files.push(fullPath)
    }
  }
  return files
}

async function walkIfExists(dir, predicate) {
  try {
    return await walk(dir, predicate)
  } catch (error) {
    if (error?.code === 'ENOENT') {
      console.warn(`optional inventory source missing, skipping: ${dir}`)
      return []
    }
    throw error
  }
}

function normalizeRoutePath(routePath) {
  if (!routePath || routePath === '/') return '/'
  return routePath.replace(/\/+/g, '/').replace(/\/$/, '') || '/'
}

function joinRoutePath(prefix, suffix) {
  const normalizedPrefix = normalizeRoutePath(prefix || '')
  const normalizedSuffix = suffix === '' ? '' : normalizeRoutePath(suffix || '')
  if (!normalizedSuffix) return normalizedPrefix === '/' ? '/' : normalizedPrefix
  if (normalizedPrefix === '/') return normalizedSuffix
  return normalizeRoutePath(`${normalizedPrefix}/${normalizedSuffix.replace(/^\//, '')}`)
}

function firstStringLiteral(text) {
  const match = text.match(/(['"])((?:\\.|(?!\1)[\s\S])*)\1/)
  return match ? match[2] : null
}

function normalizeMethod(method) {
  return method === 'websocket' ? 'WS' : method.toUpperCase()
}

function routeKey(route) {
  return `${route.method} ${route.path}`
}

function duplicateKeys(keys) {
  const seen = new Set()
  const duplicates = new Set()
  for (const key of keys) {
    if (seen.has(key)) duplicates.add(key)
    seen.add(key)
  }
  return [...duplicates].sort()
}

function routeScopeMode(route) {
  return route.scopeMode ?? 'all'
}

function mcpRequiredScopeMode(tool) {
  return tool.requiredScopeMode ?? 'all'
}

function arraysEqual(left, right) {
  if (!Array.isArray(left) || !Array.isArray(right)) return false
  return left.length === right.length && left.every((value, index) => value === right[index])
}

function parseKnownLoopbackScopes(source) {
  return new Set([...source.matchAll(/scope\(\s*"([^"]+)"/g)].map((match) => match[1]))
}

function parseRustStringConst(source, name) {
  const match = source.match(new RegExp(`\\bconst\\s+${name}\\s*:\\s*&str\\s*=\\s*"([^"]+)"`))
  return match?.[1] ?? null
}

function parseRustStringConsts(source) {
  return new Map(
    [...source.matchAll(/\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*&str\s*=\s*"([^"]+)"/g)]
      .map((match) => [match[1], match[2]]),
  )
}

function parseCrdtOperationScopeUnion(source) {
  const union = []
  const ruleRegex = /crdt_rule\(\s*"[^"]+"\s*,\s*&\[([\s\S]*?)\]\s*,?\s*\)/g
  let match
  while ((match = ruleRegex.exec(source))) {
    const scopes = [...match[1].matchAll(/"([^"]+)"/g)].map((scopeMatch) => scopeMatch[1])
    for (const scope of scopes) {
      if (!union.includes(scope)) union.push(scope)
    }
  }
  return union
}

function parseMcpToolDispatchMap(source) {
  // The dispatch table lives in `mcp_dispatch_registry.rs` as a hand-written
  // `const REGISTRY: &[McpToolEntry] = &[ ... ];` array of `McpToolEntry`
  // structs whose `name: "..."` field is the MCP tool name.
  const registryStart = source.indexOf('const REGISTRY')
  if (registryStart === -1) return new Set()
  const registryBody = source.slice(registryStart)
  const terminator = registryBody.indexOf('];')
  if (terminator === -1) return new Set()
  const body = registryBody.slice(0, terminator)
  return new Set(
    [...body.matchAll(/McpToolEntry\s*\{\s*name:\s*"([^"]+)"/g)].map((match) => match[1]),
  )
}

function parseHostedRoutes(source, filePath) {
  const routers = new Map()
  const routerRegex = /(\w+)\s*=\s*APIRouter\s*\(([\s\S]*?)\)/g
  let routerMatch
  while ((routerMatch = routerRegex.exec(source))) {
    const args = routerMatch[2]
    const prefixMatch = args.match(/prefix\s*=\s*(['"])((?:\\.|(?!\1)[\s\S])*)\1/)
    routers.set(routerMatch[1], prefixMatch ? prefixMatch[2] : '')
  }

  const decoratorRegex = new RegExp(
    `@(\\w+)\\.(${routeMethods.join('|')})\\s*\\(([\\s\\S]*?)\\)`,
    'g',
  )
  const routes = []
  let decoratorMatch
  while ((decoratorMatch = decoratorRegex.exec(source))) {
    const routerName = decoratorMatch[1]
    if (!routers.has(routerName)) continue
    const suffix = firstStringLiteral(decoratorMatch[3])
    if (suffix == null) continue
    const before = source.slice(0, decoratorMatch.index)
    const line = before.split('\n').length
    routes.push({
      method: normalizeMethod(decoratorMatch[2]),
      path: joinRoutePath(routers.get(routerName), suffix),
      file: path.relative(repoRoot, filePath),
      line,
    })
  }

  return routes
}

function parseLocalLoopbackRoutes(source, filePath) {
  // Strip Rust line comments before matching. The route-chain regex captures the
  // handler spec non-greedily up to the next `.route`/`.merge`/`}`; an inter-route
  // `//` comment (as in the emporium router, which comments the ingest route) makes
  // the prior route's capture bleed past the comment and swallow the next route's
  // method call under the WRONG path. Comments are never part of a `.route()` call,
  // and route path literals never contain `//`, so this is a no-op for the existing
  // loopback_* files and only un-breaks comment-separated chains.
  source = source.replace(/\/\/[^\n]*/g, '')
  const routeRegex = /\.route\s*\(\s*(['"])([^'"]+)\1\s*,\s*([\s\S]*?)\)\s*(?=\.(?:route|merge|layer|with_state)|\s*})/g
  const routes = []
  let match
  while ((match = routeRegex.exec(source))) {
    const routePath = normalizeRoutePath(match[2])
    const handlerSpec = match[3]
    const before = source.slice(0, match.index)
    const line = before.split('\n').length
    const methodCallRegex = new RegExp(`\\b(${routeMethods.join('|')})\\s*\\(\\s*([A-Za-z_][A-Za-z0-9_]*)`, 'g')
    let methodMatch
    while ((methodMatch = methodCallRegex.exec(handlerSpec))) {
      routes.push({
        method: normalizeMethod(methodMatch[1]),
        path: routePath,
        handler: methodMatch[2],
        file: path.relative(repoRoot, filePath),
        line,
      })
    }
  }
  return routes
}

function findMatchingBrace(source, openBraceIndex) {
  let depth = 0
  for (let index = openBraceIndex; index < source.length; index += 1) {
    const char = source[index]
    if (char === '{') depth += 1
    if (char === '}') {
      depth -= 1
      if (depth === 0) return index
    }
  }
  return -1
}

function parseLoopbackHandlerBodies(source) {
  const functions = new Map()
  const fnRegex = /(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(loopback_[A-Za-z0-9_]+)\s*\(/g
  let match
  while ((match = fnRegex.exec(source))) {
    const openBraceIndex = source.indexOf('{', match.index)
    if (openBraceIndex === -1) continue
    const closeBraceIndex = findMatchingBrace(source, openBraceIndex)
    if (closeBraceIndex === -1) continue
    functions.set(match[1], source.slice(openBraceIndex, closeBraceIndex + 1))
    fnRegex.lastIndex = closeBraceIndex + 1
  }
  return functions
}

function parseScopesFromHandlerBody(body, stringConsts = new Map()) {
  const scopes = []
  const addScope = (scope) => {
    if (!scopes.includes(scope)) scopes.push(scope)
  }
  const addScopesFromCall = (call) => {
    for (const match of call.matchAll(/"([^"]+)"/g)) addScope(match[1])
    for (const match of call.matchAll(/\b([A-Z][A-Z0-9_]*)\b/g)) {
      const value = stringConsts.get(match[1])
      if (value) addScope(value)
    }
  }
  for (const match of body.matchAll(/require_loopback_scope\s*\(([^)]*)\)/g)) {
    addScopesFromCall(match[1])
  }
  for (const match of body.matchAll(/loopback_token_has_scope\s*\(([^)]*)\)/g)) {
    addScopesFromCall(match[1])
  }
  for (const match of body.matchAll(/require_loopback_scopes\s*\([\s\S]*?&\s*\[([\s\S]*?)\]\s*,?\s*\)/g)) {
    for (const scopeMatch of match[1].matchAll(/"([^"]+)"/g)) {
      addScope(scopeMatch[1])
    }
  }
  return scopes
}

function parseMcpTools(source, filePath) {
  const toolRegex = /(?:^|\n)\s*@\w+\.tool\s*\([\s\S]*?name\s*=\s*(['"])([^'"]+)\1/g
  const tools = []
  let match
  while ((match = toolRegex.exec(source))) {
    const before = source.slice(0, match.index)
    tools.push({
      name: match[2],
      file: path.relative(mcpRoot, filePath),
      line: before.split('\n').length,
    })
  }
  return tools
}

function classifyRoute(route) {
  const key = routeKey(route)
  const explicit = classification.routeClassifications[key]
  if (explicit) {
    return explicit
  }

  const family = classification.routeFamilyClassifications
    .filter((candidate) =>
      candidate.prefix === '/'
      || route.path === candidate.prefix
      || route.path.startsWith(`${candidate.prefix}/`),
    )
    .sort((a, b) => b.prefix.length - a.prefix.length)[0]

  if (family) {
    return family
  }

  return null
}

function assertClassification(value, context) {
  if (!validClassifications.has(value)) {
    throw new Error(`${context} has invalid classification: ${value}`)
  }
}

function assertContractStatus(value, context) {
  if (!validContractStatuses.has(value)) {
    throw new Error(`${context} has invalid contract status: ${value}`)
  }
}

function assertRegistryValue(value, validValues, field, context) {
  if (!validValues.has(value)) {
    throw new Error(`${context} has invalid ${field}: ${value}`)
  }
}

function assertContractShape(contract, context) {
  assertContractStatus(contract.status, context)
  assertRegistryValue(contract.schemaStatus, validSchemaStatuses, 'schemaStatus', context)
  assertRegistryValue(contract.semanticStatus, validSemanticStatuses, 'semanticStatus', context)
  assertRegistryValue(contract.exposure, validExposureValues, 'exposure', context)
  if (contract.status === 'accepted') {
    if (contract.schemaStatus !== 'accepted') {
      throw new Error(`${context} is accepted but schemaStatus is ${contract.schemaStatus}`)
    }
    if (contract.semanticStatus !== 'accepted') {
      throw new Error(`${context} is accepted but semanticStatus is ${contract.semanticStatus}`)
    }
  }
  if (contract.status === 'deferred') {
    if (contract.semanticStatus !== 'deferred') {
      throw new Error(`${context} is deferred but semanticStatus is ${contract.semanticStatus}`)
    }
    if (
      (typeof contract.note !== 'string' || !contract.note.trim())
      && (typeof contract.untestedReason !== 'string' || !contract.untestedReason.trim())
    ) {
      throw new Error(`${context} is deferred and needs note or untestedReason`)
    }
  }
  for (const field of ['ownerAdapter', 'schema']) {
    if (typeof contract[field] !== 'string' || !contract[field].trim()) {
      throw new Error(`${context} is missing ${field}`)
    }
  }
  if (
    (typeof contract.liveProbe !== 'string' || !contract.liveProbe.trim())
    && (typeof contract.untestedReason !== 'string' || !contract.untestedReason.trim())
  ) {
    throw new Error(`${context} needs liveProbe or untestedReason`)
  }
}

function assertRouteScopeShape(route, knownScopes) {
  const context = `local route ${routeKey(route)}`
  const mode = routeScopeMode(route)
  if (!validRouteScopeModes.has(mode)) {
    throw new Error(`${context} has invalid scopeMode: ${mode}`)
  }
  if (!Array.isArray(route.scopes)) {
    throw new Error(`${context} is missing scopes array`)
  }
  const uniqueScopes = new Set(route.scopes)
  if (uniqueScopes.size !== route.scopes.length) {
    throw new Error(`${context} has duplicate scopes`)
  }
  for (const scope of route.scopes) {
    if (typeof scope !== 'string' || !scope.trim()) {
      throw new Error(`${context} has invalid empty scope`)
    }
    if (!knownScopes.has(scope)) {
      throw new Error(`${context} references unknown scope: ${scope}`)
    }
  }
  const bearerOnly = mode === 'bearer-token'
  if ((mode === 'public' || bearerOnly) && route.scopes.length > 0) {
    throw new Error(`${context} uses ${mode} mode but declares granular scopes`)
  }
  if (mode !== 'public' && !bearerOnly && route.scopes.length === 0) {
    throw new Error(`${context} needs at least one required scope`)
  }
}

function assertMcpToolScopeShape(tool, knownScopes) {
  const context = `local MCP tool ${tool.name}`
  const requiredScopeMode = mcpRequiredScopeMode(tool)
  if (!validMcpRequiredScopeModes.has(requiredScopeMode)) {
    throw new Error(`${context} has invalid requiredScopeMode: ${requiredScopeMode}`)
  }
  if (!Array.isArray(tool.scopes)) {
    throw new Error(`${context} is missing scopes array`)
  }
  const uniqueScopes = new Set(tool.scopes)
  if (uniqueScopes.size !== tool.scopes.length) {
    throw new Error(`${context} has duplicate scopes`)
  }
  if (String(tool.status ?? '').startsWith('implemented') && tool.scopes.length === 0) {
    throw new Error(`${context} needs at least one required scope`)
  }
  for (const scope of tool.scopes) {
    if (typeof scope !== 'string' || !scope.trim()) {
      throw new Error(`${context} has invalid empty scope`)
    }
    if (!knownScopes.has(scope)) {
      throw new Error(`${context} references unknown scope: ${scope}`)
    }
  }
  if (requiredScopeMode === 'operation-kind' && tool.scopes.length < 2) {
    throw new Error(`${context} uses operation-kind mode but does not declare a scope union`)
  }
}

function expandGroupedContracts(groups, explicitContracts, itemField, contextLabel) {
  const contracts = new Map()
  const duplicates = []

  function addContract(key, contract, source) {
    if (contracts.has(key)) {
      duplicates.push(`${key} (${contracts.get(key).__source}, ${source})`)
      return
    }
    contracts.set(key, { ...contract, __source: source })
  }

  for (const group of groups ?? []) {
    assertContractShape(group, `${contextLabel} group ${group.name ?? '<unnamed>'}`)
    for (const key of group[itemField] ?? []) {
      addContract(key, group, `${contextLabel} group ${group.name ?? '<unnamed>'}`)
    }
  }

  for (const [key, contract] of Object.entries(explicitContracts ?? {})) {
    assertContractShape(contract, `${contextLabel} contract ${key}`)
    addContract(key, contract, `${contextLabel} contract ${key}`)
  }

  if (duplicates.length > 0) {
    throw new Error(`Duplicate ${contextLabel} contract coverage:\n${duplicates.join('\n')}`)
  }

  return contracts
}

const routeContracts = expandGroupedContracts(
  surfaceContracts.routeContractGroups,
  surfaceContracts.routeContracts,
  'routes',
  'route',
)
const mcpToolContracts = expandGroupedContracts(
  surfaceContracts.mcpToolContractGroups,
  surfaceContracts.mcpToolContracts,
  'tools',
  'MCP tool',
)

function expandRouteEnvelopeSnapshots(groups) {
  const snapshots = new Map()
  const duplicates = []
  for (const group of groups ?? []) {
    if (!validEnvelopeKinds.has(group.kind)) {
      throw new Error(`route envelope group ${group.name ?? '<unnamed>'} has invalid kind: ${group.kind}`)
    }
    if (group.kind === 'object' && !Array.isArray(group.requiredKeys)) {
      throw new Error(`route envelope group ${group.name ?? '<unnamed>'} object snapshot needs requiredKeys`)
    }
    for (const key of group.routes ?? []) {
      if (snapshots.has(key)) {
        duplicates.push(`${key} (${snapshots.get(key).name}, ${group.name ?? '<unnamed>'})`)
        continue
      }
      snapshots.set(key, group)
    }
  }
  if (duplicates.length > 0) {
    throw new Error(`Duplicate route envelope snapshots:\n${duplicates.join('\n')}`)
  }
  return snapshots
}

const routeEnvelopeSnapshots = expandRouteEnvelopeSnapshots(routeEnvelopeSnapshot.routeGroups)
const rustLoopbackScopesSource = await fs.readFile(rustLoopbackScopesPath, 'utf8')
const rustLoopbackTokenGrantsSource = await fs.readFile(rustLoopbackTokenGrantsPath, 'utf8')
const rustLoopbackScopeCatalogSource = (await Promise.all(
  rustLoopbackScopeCatalogPaths.map((filePath) => fs.readFile(filePath, 'utf8')),
)).join('\n')
const knownLoopbackScopes = parseKnownLoopbackScopes(rustLoopbackScopeCatalogSource)
const rustTokenScopeMode = parseRustStringConst(rustLoopbackTokenGrantsSource, 'TOKEN_SCOPE_MODE')
const rustCrdtOperationScopeUnion = parseCrdtOperationScopeUnion(rustLoopbackScopesSource)
const rustMcpDispatchTools = parseMcpToolDispatchMap(
  await fs.readFile(rustMcpDispatchRegistryPath, 'utf8'),
)

const hostedRouteFiles = await walkIfExists(
  path.join(repoRoot, 'app/http/routers'),
  (filePath) => filePath.endsWith('.py'),
)
const hostedRoutes = []
for (const filePath of hostedRouteFiles) {
  hostedRoutes.push(...parseHostedRoutes(await fs.readFile(filePath, 'utf8'), filePath))
}

const loopbackRouteFiles = await walk(
  rustSourceDir,
  (filePath) => {
    const basename = path.basename(filePath)
    if (basename === 'loopback_router.rs' || /loopback_.*_routes\.rs$/.test(basename)) {
      return true
    }
    // The emporium cell routes (`emporium/vocab_routes.rs` hosts the router,
    // `emporium/ingest_routes.rs` the handler) merge into the loopback app via
    // `loopback_emporium_router()` but don't follow the `loopback_*` naming, so
    // they were invisible to the drift check. Scan the emporium `*_routes.rs`
    // modules so the ingest/vocab routes are tracked against the surface JSON.
    return /[\\/]emporium[\\/].*_routes\.rs$/.test(filePath)
  },
)
const localLoopbackRoutes = []
const handlerBodies = new Map()
const rustLoopbackStringConsts = new Map()
for (const filePath of loopbackRouteFiles) {
  const source = await fs.readFile(filePath, 'utf8')
  localLoopbackRoutes.push(...parseLocalLoopbackRoutes(source, filePath))
  for (const [name, body] of parseLoopbackHandlerBodies(source)) {
    handlerBodies.set(name, body)
  }
  for (const [name, value] of parseRustStringConsts(source)) {
    rustLoopbackStringConsts.set(name, value)
  }
}
const localRouteKeys = new Set(localLoopbackRoutes.map(routeKey))
const specialHandlerScopes = new Map([
  ['loopback_health', []],
  // The emporium cell handlers don't use the `loopback_*` naming, so register
  // them here — the same hook `loopback_mcp` uses — for the route-scope drift
  // check. The vocab GET routes are intentionally auth-free (they serve
  // sha-pinned golden bytes). The ingest route IS guarded: `dry_run=true`
  // requires `rdf.query`, apply (`dry_run=false`) requires `rdf.update`
  // (see `emporium/ingest_routes.rs` -> `require_loopback_scope`); declare the
  // union of both here.
  ['list_vocabs', []],
  ['get_vocab_latest', []],
  ['get_vocab_version', []],
  ['get_vocab_class_query', []],
  ['ingest_handler', ['rdf.query', 'rdf.update']],
  ['sweep_handler', ['rdf.update']],
  ['heads_handler', ['rdf.query']],
  ['objects_create_handler', ['rdf.update']],
  ['objects_list_handler', ['rdf.query']],
  ['objects_read_handler', ['rdf.query']],
  ['objects_update_handler', ['rdf.update']],
  ['objects_delete_handler', ['rdf.update']],
  ['get_vocab_openapi_populated', ['rdf.query']],
  ['get_workflow_openapi_populated', ['rdf.query']],
  ['doc_room', []],
  ['workspace_room', []],
  // Service facade handlers delegate authorization to this common helper.
  ['proxy_service_path', ['services.proxy']],
  ['loopback_mcp', ['mcp.tools.read', 'mcp.tools.call']],
  [
    'loopback_crdt_operation',
    [
      'documents.write.crdt',
      'workspace.write.crdt',
      'documents.delete.crdt',
      'workspace.delete.crdt',
      'artifacts.write',
      'artifacts.delete',
      'wires.write',
      'wires.delete',
      'artifacts.ingest',
      'graphs.import',
      'imports.web.write',
    ],
  ],
])
const handlerScopes = new Map([...specialHandlerScopes])
for (const [name, body] of handlerBodies) {
  if (!handlerScopes.has(name)) {
    handlerScopes.set(name, parseScopesFromHandlerBody(body, rustLoopbackStringConsts))
  }
}
for (const [name, body] of handlerBodies) {
  if ((handlerScopes.get(name) ?? []).length > 0) continue
  const delegatedScopes = []
  for (const match of body.matchAll(/\b([A-Za-z_][A-Za-z0-9_]*)\s*\(/g)) {
    if (match[1] === name) continue
    for (const scope of handlerScopes.get(match[1]) ?? []) {
      if (!delegatedScopes.includes(scope)) delegatedScopes.push(scope)
    }
  }
  if (delegatedScopes.length > 0) {
    handlerScopes.set(name, delegatedScopes)
  }
}
const localDeclaredRoutes = (localSurface.routes ?? []).map((route) => ({
  ...route,
  method: route.method.toUpperCase(),
  path: normalizeRoutePath(route.path),
}))
const localDeclaredRouteKeys = new Set(localDeclaredRoutes.map(routeKey))
const localDeclaredRouteByKey = new Map(localDeclaredRoutes.map((route) => [routeKey(route), route]))
const openApiOperations = new Map(Object.entries(localOpenApi.paths ?? {}).flatMap(([routePath, pathItem]) =>
  Object.keys(pathItem)
    .filter((method) => routeMethods.includes(method.toLowerCase()))
    .map((method) => [
      `${method.toUpperCase()} ${normalizeRoutePath(routePath)}`,
      pathItem[method],
    ]),
))
const openApiRouteKeys = new Set(openApiOperations.keys())

const mcpToolFiles = await walkIfExists(
  path.join(mcpRoot, 'src/neem/mcp'),
  (filePath) => filePath.endsWith('.py'),
)
const hostedMcpTools = []
for (const filePath of mcpToolFiles) {
  hostedMcpTools.push(...parseMcpTools(await fs.readFile(filePath, 'utf8'), filePath))
}

const hostedToolNames = [...new Set(hostedMcpTools.map((tool) => tool.name))].sort()
const localMcpToolRows = localSurface.mcpTools ?? []
const localMcpTools = new Set(localMcpToolRows.map((tool) => tool.name))
const localMcpCatalogRows = localMcpCatalog.tools ?? []
const localMcpCatalogTools = new Set(localMcpCatalogRows.map((tool) => tool.name))

const routeRows = hostedRoutes.map((route) => {
  const classInfo = classifyRoute(route)
  return {
    ...route,
    localRoutePresent: localRouteKeys.has(routeKey(route)),
    classification: classInfo?.classification ?? 'unclassified',
    note: classInfo?.note ?? null,
  }
})

const unclassifiedRoutes = routeRows.filter((row) => row.classification === 'unclassified')
for (const row of routeRows) {
  assertClassification(row.classification, `route ${routeKey(row)}`)
}

const toolRows = hostedToolNames.map((name) => ({
  name,
  localToolPresent: localMcpTools.has(name),
  classification: classification.mcpToolClassifications[name] ?? 'unclassified',
}))
const unclassifiedTools = toolRows.filter((row) => row.classification === 'unclassified')
for (const row of toolRows) {
  assertClassification(row.classification, `MCP tool ${row.name}`)
}

const unknownLocalTools = [...localMcpTools]
  .filter((name) => !classification.mcpToolClassifications[name])
  .filter((name) => !classification.localMcpExtensions[name])
  .sort()

const localImplementedHostedToolDrift = localMcpToolRows
  .filter((tool) => String(tool.status ?? '').startsWith('implemented'))
  .map((tool) => tool.name)
  .filter((name) => classification.mcpToolClassifications[name])
  .filter((name) => classification.mcpToolClassifications[name] !== 'implemented')
  .sort()

const implementedHostedRouteKeys = routeRows
  .filter((row) => row.classification === 'implemented')
  .map(routeKey)
const missingRouteContracts = implementedHostedRouteKeys
  .filter((key) => !routeContracts.has(key))
  .sort()
const missingRouteEnvelopeSnapshots = implementedHostedRouteKeys
  .filter((key) => !routeEnvelopeSnapshots.has(key))
  .sort()

const implementedHostedToolNames = toolRows
  .filter((row) => row.classification === 'implemented')
  .map((row) => row.name)
const missingMcpToolContracts = implementedHostedToolNames
  .filter((name) => !mcpToolContracts.has(name))
  .sort()

const localImplementedMcpToolNames = localMcpToolRows
  .filter((tool) => String(tool.status ?? '').startsWith('implemented'))
  .map((tool) => tool.name)
  .sort()
const mcpSchemaSnapshotTools = new Set(Object.keys(mcpSchemaSnapshot.tools ?? {}))
// Hosted-compatible tools keep an independent schema snapshot so parity drift is
// reviewable without trusting either implementation. Garden-native extensions
// have no hosted counterpart; their checked-in mcp_tool_catalog.json plus the
// Rust registry/dispatch checks below are the schema source of truth.
const missingMcpSchemaSnapshots = localImplementedMcpToolNames
  .filter((name) => classification.mcpToolClassifications[name])
  .filter((name) => !mcpSchemaSnapshotTools.has(name))
  .sort()

const staleMcpSchemaSnapshots = [...mcpSchemaSnapshotTools]
  .filter((name) => !localMcpTools.has(name))
  .sort()

const duplicateLocalMcpTools = duplicateKeys(localMcpToolRows.map((tool) => tool.name))
const duplicateLocalMcpCatalogTools = duplicateKeys(localMcpCatalogRows.map((tool) => tool.name))
const missingMcpCatalogTools = [...localMcpTools]
  .filter((name) => !localMcpCatalogTools.has(name))
  .sort()
const staleMcpCatalogTools = [...localMcpCatalogTools]
  .filter((name) => !localMcpTools.has(name))
  .sort()
const missingMcpDispatchTools = [...localMcpTools]
  .filter((name) => !rustMcpDispatchTools.has(name))
  .sort()
const staleMcpDispatchTools = [...rustMcpDispatchTools]
  .filter((name) => !localMcpTools.has(name))
  .sort()
const localMcpToolScopeErrors = []
for (const tool of localMcpToolRows) {
  try {
    assertMcpToolScopeShape(tool, knownLoopbackScopes)
  } catch (error) {
    localMcpToolScopeErrors.push(error.message)
  }
}

const mcpToolScopeDrift = []
const crdtOperationTool = localMcpToolRows.find((tool) => tool.name === 'crdt_operation')
if (!crdtOperationTool) {
  mcpToolScopeDrift.push({
    tool: 'crdt_operation',
    error: 'local surface crdt_operation entry missing',
  })
} else if (!arraysEqual(crdtOperationTool.scopes, rustCrdtOperationScopeUnion)) {
  mcpToolScopeDrift.push({
    tool: 'crdt_operation',
    error: 'local surface scope union differs from Rust CRDT operation rules',
    expectedScopes: rustCrdtOperationScopeUnion,
    localSurfaceScopes: crdtOperationTool.scopes,
  })
}

const staleDeclaredRoutes = [...localDeclaredRouteKeys]
  .filter((key) => {
    const route = localDeclaredRouteByKey.get(key)
    return String(route?.status ?? '').startsWith('implemented')
  })
  .filter((key) => !localRouteKeys.has(key))
  .sort()

const undeclaredLocalRoutes = [...localRouteKeys]
  .filter((key) => !localDeclaredRouteKeys.has(key))
  .sort()

const implementedDeclaredRouteKeys = [...localDeclaredRouteKeys].filter((key) => {
  const route = localDeclaredRouteByKey.get(key)
  return String(route?.status ?? '').startsWith('implemented')
})

const missingOpenApiRoutes = implementedDeclaredRouteKeys
  .filter((key) => !openApiRouteKeys.has(key))
  .sort()

const staleOpenApiRoutes = [...openApiRouteKeys]
  .filter((key) => !localDeclaredRouteKeys.has(key))
  .sort()

const duplicateDeclaredRoutes = duplicateKeys(localDeclaredRoutes.map(routeKey))
const localRouteScopeErrors = []
for (const route of localDeclaredRoutes) {
  try {
    assertRouteScopeShape(route, knownLoopbackScopes)
  } catch (error) {
    localRouteScopeErrors.push(error.message)
  }
}

const openApiScopeDrift = []
for (const key of implementedDeclaredRouteKeys) {
  const route = localDeclaredRouteByKey.get(key)
  const operation = openApiOperations.get(key)
  if (!route || !operation) continue
  const expectedScopes = route.scopes
  const expectedScopeMode = routeScopeMode(route)
  const actualScopes = operation['x-sophia-required-scopes']
  const actualScopeMode = operation['x-sophia-scope-mode']
  if (!arraysEqual(actualScopes, expectedScopes) || actualScopeMode !== expectedScopeMode) {
    openApiScopeDrift.push({
      route: key,
      expectedScopes,
      actualScopes,
      expectedScopeMode,
      actualScopeMode,
    })
  }
}

const tokenScopeModeDrift = []
if (!rustTokenScopeMode) {
  tokenScopeModeDrift.push({ error: 'Rust TOKEN_SCOPE_MODE const missing' })
}
const openApiTokenScopeModeEnum = localOpenApi
  .components
  ?.schemas
  ?.LoopbackManifest
  ?.properties
  ?.tokenScopeMode
  ?.enum
if (!Array.isArray(openApiTokenScopeModeEnum)) {
  tokenScopeModeDrift.push({ error: 'OpenAPI LoopbackManifest tokenScopeMode enum missing' })
} else if (rustTokenScopeMode && !arraysEqual(openApiTokenScopeModeEnum, [rustTokenScopeMode])) {
  tokenScopeModeDrift.push({
    error: 'OpenAPI LoopbackManifest tokenScopeMode enum drift',
    rustTokenScopeMode,
    openApiTokenScopeModeEnum,
  })
}

const routeScopeImplementationDrift = []
for (const route of localLoopbackRoutes) {
  const key = routeKey(route)
  const declared = localDeclaredRouteByKey.get(key)
  if (!declared) continue
  const actualScopes = handlerScopes.get(route.handler)
  if (!actualScopes) {
    routeScopeImplementationDrift.push({
      route: key,
      handler: route.handler,
      error: 'handler scope data missing',
    })
    continue
  }
  if (!arraysEqual(actualScopes, declared.scopes)) {
    routeScopeImplementationDrift.push({
      route: key,
      handler: route.handler,
      expectedScopes: declared.scopes,
      actualScopes,
      scopeMode: routeScopeMode(declared),
    })
  }
}

if (
  unclassifiedRoutes.length
  || unclassifiedTools.length
  || unknownLocalTools.length
  || localImplementedHostedToolDrift.length
  || missingRouteContracts.length
  || missingRouteEnvelopeSnapshots.length
  || missingMcpToolContracts.length
  || missingMcpSchemaSnapshots.length
  || staleMcpSchemaSnapshots.length
  || duplicateLocalMcpTools.length
  || duplicateLocalMcpCatalogTools.length
  || missingMcpCatalogTools.length
  || staleMcpCatalogTools.length
  || missingMcpDispatchTools.length
  || staleMcpDispatchTools.length
  || localMcpToolScopeErrors.length
  || mcpToolScopeDrift.length
  || staleDeclaredRoutes.length
  || undeclaredLocalRoutes.length
  || missingOpenApiRoutes.length
  || staleOpenApiRoutes.length
  || duplicateDeclaredRoutes.length
  || localRouteScopeErrors.length
  || openApiScopeDrift.length
  || tokenScopeModeDrift.length
  || routeScopeImplementationDrift.length
) {
  const details = {
    unclassifiedRoutes: unclassifiedRoutes.map((row) => routeKey(row)),
    unclassifiedTools: unclassifiedTools.map((row) => row.name),
    unknownLocalTools,
    localImplementedHostedToolDrift,
    missingRouteContracts,
    missingRouteEnvelopeSnapshots,
    missingMcpToolContracts,
    missingMcpSchemaSnapshots,
    staleMcpSchemaSnapshots,
    duplicateLocalMcpTools,
    duplicateLocalMcpCatalogTools,
    missingMcpCatalogTools,
    staleMcpCatalogTools,
    missingMcpDispatchTools,
    staleMcpDispatchTools,
    localMcpToolScopeErrors,
    mcpToolScopeDrift,
    staleDeclaredRoutes,
    undeclaredLocalRoutes,
    missingOpenApiRoutes,
    staleOpenApiRoutes,
    duplicateDeclaredRoutes,
    localRouteScopeErrors,
    openApiScopeDrift,
    tokenScopeModeDrift,
    routeScopeImplementationDrift,
  }
  throw new Error(`Surface inventory is incomplete:\n${JSON.stringify(details, null, 2)}`)
}

const countBy = (rows, field) => rows.reduce((counts, row) => {
  counts[row[field]] = (counts[row[field]] ?? 0) + 1
  return counts
}, {})

const localImplementedTools = localMcpToolRows
  .filter((tool) => String(tool.status ?? '').startsWith('implemented'))
  .length

const countContractStatuses = (keys, contracts) => keys.reduce((counts, key) => {
  const status = contracts.get(key)?.status ?? 'missing'
  counts[status] = (counts[status] ?? 0) + 1
  return counts
}, {})

const countContractAxis = (keys, contracts, field) => keys.reduce((counts, key) => {
  const status = contracts.get(key)?.[field] ?? 'missing'
  counts[status] = (counts[status] ?? 0) + 1
  return counts
}, {})

const report = {
  ok: true,
  hostedRoutes: {
    total: routeRows.length,
    websocket: routeRows.filter((row) => row.method === 'WS').length,
    byClassification: countBy(routeRows, 'classification'),
    locallyPresentByPath: routeRows.filter((row) => row.localRoutePresent).length,
    implementedContractStatus: countContractStatuses(implementedHostedRouteKeys, routeContracts),
    implementedSchemaStatus: countContractAxis(implementedHostedRouteKeys, routeContracts, 'schemaStatus'),
    implementedSemanticStatus: countContractAxis(implementedHostedRouteKeys, routeContracts, 'semanticStatus'),
    implementedExposure: countContractAxis(implementedHostedRouteKeys, routeContracts, 'exposure'),
    envelopeSnapshots: routeEnvelopeSnapshots.size,
  },
  localLoopbackRoutes: {
    total: localLoopbackRoutes.length,
    declared: localDeclaredRouteKeys.size,
    scoped: localDeclaredRoutes.filter((route) => Array.isArray(route.scopes)).length,
    implementationScoped: localLoopbackRoutes.filter((route) => handlerScopes.has(route.handler)).length,
    scopeModes: countBy(localDeclaredRoutes.map((route) => ({ scopeMode: routeScopeMode(route) })), 'scopeMode'),
  },
  localOpenApi: {
    paths: Object.keys(localOpenApi.paths ?? {}).length,
    operations: openApiRouteKeys.size,
    tokenScopeMode: rustTokenScopeMode,
  },
  hostedMcpTools: {
    total: hostedToolNames.length,
    byClassification: countBy(toolRows, 'classification'),
    locallyPresentByName: toolRows.filter((row) => row.localToolPresent).length,
    implementedContractStatus: countContractStatuses(implementedHostedToolNames, mcpToolContracts),
    implementedSchemaStatus: countContractAxis(implementedHostedToolNames, mcpToolContracts, 'schemaStatus'),
    implementedSemanticStatus: countContractAxis(implementedHostedToolNames, mcpToolContracts, 'semanticStatus'),
    implementedExposure: countContractAxis(implementedHostedToolNames, mcpToolContracts, 'exposure'),
  },
  localMcpTools: {
    total: localMcpTools.size,
    implemented: localImplementedTools,
    scoped: localMcpToolRows.filter((tool) => Array.isArray(tool.scopes)).length,
    requiredScopeModes: countBy(
      localMcpToolRows.map((tool) => ({ requiredScopeMode: mcpRequiredScopeMode(tool) })),
      'requiredScopeMode',
    ),
    extensions: Object.keys(classification.localMcpExtensions).length,
    schemaSnapshots: mcpSchemaSnapshotTools.size,
    catalogTools: localMcpCatalogTools.size,
    dispatchTools: rustMcpDispatchTools.size,
  },
}

console.log(JSON.stringify(report, null, 2))

#!/usr/bin/env node
import fs from 'node:fs/promises'
import path from 'node:path'
import {
  binaryResponseRoutes,
  documentExportResponseRoutes,
  documentSnapshotHtmlResponseRoutes,
  documentSnapshotTextResponseRoutes,
  extraErrorResponsesByRoute,
  multipartRequestSchemaByRoute,
  queryParametersByRoute,
  requestSchemaByRoute,
  responseSchemaByRoute,
  successStatusByRoute,
} from './openapi-route-contracts.mjs'

const here = path.dirname(new URL(import.meta.url).pathname)
const surfacePath = path.join(here, 'local-loopback-surface.json')
const outputPath = path.join(here, 'local-openapi.json')
const surface = JSON.parse(await fs.readFile(surfacePath, 'utf8'))
const args = new Set(process.argv.slice(2))
const invalidArgs = [...args].filter((arg) => arg !== '--check' && arg !== '--write')
if (invalidArgs.length > 0) {
  throw new Error(`Unknown option(s): ${invalidArgs.join(', ')}`)
}
if (args.has('--check') && args.has('--write')) {
  throw new Error('Use either --check or --write, not both')
}
const checkMode = args.has('--check')

const JSON_OBJECT = { $ref: '#/components/schemas/JsonObject' }

function routeKey(route) {
  return `${route.method.toUpperCase()} ${route.path}`
}

function assertKnownContractRoutes(label, keys) {
  const knownRoutes = new Set(surface.routes.map(routeKey))
  const missing = keys.filter((key) => !knownRoutes.has(key))
  if (missing.length > 0) {
    throw new Error(`${label} references unknown local loopback route(s): ${missing.join(', ')}`)
  }
}

assertKnownContractRoutes('requestSchemaByRoute', Object.keys(requestSchemaByRoute))
assertKnownContractRoutes('responseSchemaByRoute', Object.keys(responseSchemaByRoute))
assertKnownContractRoutes('successStatusByRoute', Object.keys(successStatusByRoute))
assertKnownContractRoutes('binaryResponseRoutes', [...binaryResponseRoutes])
assertKnownContractRoutes('documentExportResponseRoutes', [...documentExportResponseRoutes])
assertKnownContractRoutes('documentSnapshotHtmlResponseRoutes', [...documentSnapshotHtmlResponseRoutes])
assertKnownContractRoutes('documentSnapshotTextResponseRoutes', [...documentSnapshotTextResponseRoutes])
assertKnownContractRoutes('multipartRequestSchemaByRoute', Object.keys(multipartRequestSchemaByRoute))
assertKnownContractRoutes('extraErrorResponsesByRoute', Object.keys(extraErrorResponsesByRoute))
assertKnownContractRoutes('queryParametersByRoute', Object.keys(queryParametersByRoute))

function operationId(route) {
  const stem = route.path
    .replace(/[{}]/g, '')
    .split('/')
    .filter(Boolean)
    .map((part) => part.replace(/[^a-zA-Z0-9]+(.)/g, (_, char) => char.toUpperCase()))
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join('')
  return `${route.method.toLowerCase()}${stem || 'Root'}`
}

function routeTag(route) {
  const parts = route.path.split('/').filter(Boolean)
  const tag = parts[0] === 'api' ? (parts[1] ?? 'api') : (parts[0] ?? 'system')
  return tag.replace(/\.json$/u, '')
}

function routeScopeMode(route) {
  return route.scopeMode ?? 'all'
}

function pathParameters(routePath) {
  return [...routePath.matchAll(/\{([^}]+)\}/g)].map((match) => ({
    name: match[1],
    in: 'path',
    required: true,
    schema: { type: 'string' },
  }))
}

function schemaRef(name) {
  return name ? { $ref: `#/components/schemas/${name}` } : JSON_OBJECT
}

function routeResponseSchema(route) {
  const exact = responseSchemaByRoute[routeKey(route)]
  if (exact) return schemaRef(exact)
  if (route.path.includes('/graphs/jobs/')) return JSON_OBJECT
  if (route.path.startsWith('/documents/')) return JSON_OBJECT
  if (route.path.startsWith('/navigation/')) return JSON_OBJECT
  if (route.path.startsWith('/wires/')) return JSON_OBJECT
  if (route.path.startsWith('/search')) return JSON_OBJECT
  if (route.method.toUpperCase() === 'POST' && route.path === '/mcp') return JSON_OBJECT
  return JSON_OBJECT
}

function routeSuccessContent(route) {
  if (documentSnapshotHtmlResponseRoutes.has(routeKey(route))) {
    return {
      'text/html; charset=utf-8': {
        schema: { type: 'string' },
      },
    }
  }
  if (documentSnapshotTextResponseRoutes.has(routeKey(route))) {
    return {
      'text/plain; charset=utf-8': {
        schema: { type: 'string' },
      },
    }
  }
  if (documentExportResponseRoutes.has(routeKey(route))) {
    return {
      'text/markdown; charset=utf-8': {
        schema: { type: 'string' },
      },
      'application/xml': {
        schema: { type: 'string' },
      },
      'text/html; charset=utf-8': {
        schema: { type: 'string' },
      },
    }
  }
  if (binaryResponseRoutes.has(routeKey(route))) {
    return {
      'application/octet-stream': {
        schema: { type: 'string', format: 'binary' },
      },
    }
  }
  return {
    'application/json': {
      schema: routeResponseSchema(route),
    },
  }
}

function errorResponse(description) {
  return {
    description,
    content: {
      'application/json': { schema: { $ref: '#/components/schemas/ErrorResponse' } },
    },
  }
}

function addOperation(paths, route) {
  const method = route.method.toLowerCase()
  const key = routeKey(route)
  const scopes = Array.isArray(route.scopes) ? route.scopes : []
  const scopeMode = routeScopeMode(route)
  const successStatus = successStatusByRoute[key] ?? 200
  paths[route.path] ??= {}
  const requestSchema = requestSchemaByRoute[key]
  const successResponse = {
    description: 'Successful response',
  }
  if (successStatus !== 204) {
    successResponse.content = routeSuccessContent(route)
  }
  const operation = {
    operationId: operationId(route),
    tags: [routeTag(route)],
    summary: `${route.method.toUpperCase()} ${route.path}`,
    description: `Local native loopback route. Surface status: ${route.status}. Scope mode: ${scopeMode}.`,
    'x-sophia-status': route.status,
    'x-sophia-required-scopes': scopes,
    'x-sophia-scope-mode': scopeMode,
    parameters: pathParameters(route.path),
    responses: {
      [successStatus]: successResponse,
      400: {
        description: 'Rejected request',
        content: {
          'application/json': { schema: { $ref: '#/components/schemas/ErrorResponse' } },
        },
      },
    },
  }

  if (scopeMode === 'bearer-or-signed-url') {
    operation.security = [{ bearerAuth: [] }, { imageUrlToken: [] }]
    operation.responses[403] = {
      description: 'Missing, invalid, or forbidden local loopback bearer token',
      content: {
        'application/json': { schema: { $ref: '#/components/schemas/ErrorResponse' } },
      },
    }
  } else if (route.path !== '/health') {
    operation.security = [{ bearerAuth: [] }]
    operation.responses[403] = {
      description: 'Missing, invalid, or forbidden local loopback bearer token',
      content: {
        'application/json': { schema: { $ref: '#/components/schemas/ErrorResponse' } },
      },
    }
  } else {
    operation.security = []
  }

  if (requestSchema) {
    operation.requestBody = {
      required: true,
      content: {
        'application/json': {
          schema: schemaRef(requestSchema),
        },
      },
    }
  }
  if (multipartRequestSchemaByRoute[key]) {
    operation.requestBody = {
      required: true,
      content: {
        'multipart/form-data': {
          schema: schemaRef(multipartRequestSchemaByRoute[key]),
        },
      },
    }
  }
  for (const [status, description] of Object.entries(extraErrorResponsesByRoute[key] ?? {})) {
    operation.responses[status] = errorResponse(description)
  }
  if (queryParametersByRoute[key]) {
    operation.parameters.push(...queryParametersByRoute[key])
  }

  paths[route.path][method] = operation
}

const components = {
  securitySchemes: {
    bearerAuth: {
      type: 'http',
      scheme: 'bearer',
      bearerFormat: 'loopback-token',
    },
    imageUrlToken: {
      type: 'apiKey',
      in: 'query',
      name: 'token',
      description: 'Signed expiring image URL token paired with the exp query parameter.',
    },
  },
  schemas: {
    CustomCssResponse: {
      type:'object', additionalProperties:false,
      required:['schemaVersion','scope','graphId','graphIncarnation','cssText','contentHashSha256','revision'],
      properties:{schemaVersion:{type:'integer',const:1},scope:{type:'string',const:'graph'},graphId:{type:'string'},graphIncarnation:{type:'string'},cssText:{type:'string'},contentHashSha256:{type:'string',pattern:'^[a-f0-9]{64}$'},revision:{type:'integer',minimum:0,maximum:9007199254740991},replayedOperationId:{type:'string'}},
    },
    CustomCssWriteRequest: {
      type:'object',additionalProperties:false,
      required:['graphIncarnation','expectedContentSha256','expectedRevision','cssText','operationId'],
      properties:{graphIncarnation:{type:'string',minLength:1},expectedContentSha256:{type:'string',pattern:'^[a-f0-9]{64}$'},expectedRevision:{type:'integer',minimum:0,maximum:9007199254740991},cssText:{type:'string',description:'Exact CSS, maximum 65536 UTF-8 bytes.'},operationId:{type:'string',minLength:1}},
    },
    CustomCssResetRequest: {
      type:'object',additionalProperties:false,
      required:['graphIncarnation','expectedContentSha256','expectedRevision','operationId'],
      properties:{graphIncarnation:{type:'string',minLength:1},expectedContentSha256:{type:'string',pattern:'^[a-f0-9]{64}$'},expectedRevision:{type:'integer',minimum:0,maximum:9007199254740991},operationId:{type:'string',minLength:1}},
    },
    ArtifactTextResponse: {
      type: 'object', additionalProperties: false,
      required: ['graphId','graphIncarnation','artifactId','filename','mimeType','text','sizeBytes','contentHashSha256'],
      properties: {
        graphId:{type:'string'},graphIncarnation:{type:'string'},artifactId:{type:'string'},
        filename:{type:'string'},mimeType:{type:'string',enum:['text/html','text/plain']},
        text:{type:'string'},sizeBytes:{type:'integer',minimum:0,maximum:2097152},
        contentHashSha256:{type:'string',pattern:'^[a-f0-9]{64}$'},
      },
    },
    JsonObject: { type: 'object', additionalProperties: true },
    OpenApiDocument: { type: 'object', additionalProperties: true },
    ErrorResponse: {
      type: 'object',
      required: ['error'],
      properties: { error: { type: 'string' } },
      additionalProperties: true,
    },
    HealthResponse: {
      type: 'object',
      required: ['status', 'redis'],
      properties: {
        status: { type: 'string' },
        redis: { type: 'object', additionalProperties: true },
      },
    },
    LoopbackManifest: {
      type: 'object',
      required: [
        'runtimeProfile',
        'bindHost',
        'port',
        'apiUrl',
        'mcpUrl',
        'openapiUrl',
        'token',
        'tokenScopeMode',
        'tokenScopes',
        'scopeDetails',
        'grantProfiles',
      ],
      properties: {
        runtimeProfile: { type: 'string' },
        bindHost: { type: 'string' },
        port: { type: 'integer' },
        apiUrl: { type: 'string', format: 'uri' },
        mcpUrl: { type: 'string', format: 'uri' },
        openapiUrl: { type: 'string', format: 'uri' },
        token: { type: 'string' },
        pid: { type: 'integer' },
        startedAt: { type: 'string' },
        manifestPath: { type: 'string' },
        authHeader: { type: 'string' },
        capabilities: { type: 'array', items: { type: 'string' } },
        tokenScopeMode: { type: 'string', enum: ['session-all'] },
        tokenScopes: { type: 'array', items: { type: 'string' } },
        scopeDetails: { type: 'array', items: { $ref: '#/components/schemas/LoopbackScopeDetail' } },
        grantProfiles: { type: 'array', items: { $ref: '#/components/schemas/LoopbackGrantProfile' } },
      },
    },
    LoopbackManifestPublic: {
      type: 'object',
      description: 'Secret-free projection of LoopbackManifest returned by GET /manifest for every principal, including the Owner/master-token caller. Never carries token, pid, or manifestPath — those remain disk-only (loopback.json), never serialized to HTTP.',
      required: [
        'runtimeProfile',
        'bindHost',
        'port',
        'apiUrl',
        'mcpUrl',
        'openapiUrl',
        'tokenScopeMode',
        'tokenScopes',
        'scopeDetails',
        'grantProfiles',
      ],
      properties: {
        runtimeProfile: { type: 'string' },
        bindHost: { type: 'string' },
        port: { type: 'integer' },
        apiUrl: { type: 'string', format: 'uri' },
        mcpUrl: { type: 'string', format: 'uri' },
        openapiUrl: { type: 'string', format: 'uri' },
        startedAt: { type: 'string' },
        authHeader: { type: 'string' },
        capabilities: { type: 'array', items: { type: 'string' } },
        tokenScopeMode: { type: 'string', enum: ['session-all'] },
        tokenScopes: { type: 'array', items: { type: 'string' } },
        scopeDetails: { type: 'array', items: { $ref: '#/components/schemas/LoopbackScopeDetail' } },
        grantProfiles: { type: 'array', items: { $ref: '#/components/schemas/LoopbackGrantProfile' } },
      },
    },
    LoopbackGrantProfile: {
      type: 'object',
      required: ['id', 'label', 'description', 'scopes', 'defaultGrant', 'mutating'],
      properties: {
        id: { type: 'string' },
        label: { type: 'string' },
        description: { type: 'string' },
        scopes: { type: 'array', items: { type: 'string' } },
        defaultGrant: { type: 'boolean' },
        mutating: { type: 'boolean' },
      },
    },
    LoopbackScopeDetail: {
      type: 'object',
      required: ['scope', 'category', 'access', 'description', 'defaultGrant'],
      properties: {
        scope: { type: 'string' },
        category: { type: 'string' },
        access: { type: 'string', enum: ['read', 'write', 'delete'] },
        description: { type: 'string' },
        defaultGrant: { type: 'boolean' },
      },
    },
    RuntimeCapabilities: {
      type: 'object',
      required: ['runtimeProfile', 'graphOrigin', 'providerId', 'hostedAvailable', 'capabilities'],
      properties: {
        runtimeProfile: { type: 'string' },
        graphOrigin: { type: 'string' },
        providerId: { type: 'string' },
        hostedAvailable: { type: 'boolean' },
        capabilities: { type: 'array', items: { $ref: '#/components/schemas/Capability' } },
      },
    },
    Capability: {
      type: 'object',
      required: ['key', 'status', 'description'],
      properties: {
        key: { type: 'string' },
        status: { type: 'string' },
        description: { type: 'string' },
      },
    },
    ProfileInfo: {
      type: 'object',
      required: ['profileId', 'displayName', 'runtimeProfile', 'profilePath', 'graphsPath'],
      properties: {
        profileId: { type: 'string' },
        displayName: { type: 'string' },
        runtimeProfile: { type: 'string' },
        profilePath: { type: 'string' },
        graphsPath: { type: 'string' },
      },
    },
    CreateGraphRequest: {
      type: 'object',
      required: ['graph_id', 'title'],
      properties: {
        graph_id: { type: 'string' },
        title: { type: 'string' },
        description: { type: ['string', 'null'] },
      },
    },
    GraphDuplicateRequest: {
      type: 'object',
      required: ['new_graph_id'],
      properties: {
        new_graph_id: { type: 'string' },
        new_title: { type: ['string', 'null'] },
      },
      additionalProperties: false,
    },
    GraphMetadataStats: {
      type: 'object',
      required: ['total_graphs', 'total_triples', 'avg_triples'],
      properties: {
        total_graphs: { type: 'integer', minimum: 0 },
        total_triples: { type: 'integer', minimum: 0 },
        avg_triples: { type: 'number', minimum: 0 },
      },
      additionalProperties: true,
    },
    GraphStorageUsage: {
      type: 'object',
      required: ['graph_id', 'tier', 'used_bytes', 'usage_ratio', 'warning_threshold_reached', 'limit_reached'],
      properties: {
        user_id: { type: 'string' },
        graph_id: { type: 'string' },
        tier: { type: 'string' },
        used_bytes: { type: 'integer', minimum: 0 },
        bytes_used: { type: 'integer', minimum: 0 },
        limit_bytes: { type: ['integer', 'null'], minimum: 0 },
        warning_bytes: { type: ['integer', 'null'], minimum: 0 },
        usage_ratio: { type: 'number', minimum: 0 },
        warning_threshold_reached: { type: 'boolean' },
        limit_reached: { type: 'boolean' },
      },
      additionalProperties: true,
    },
    DocumentCreationPreflightResponse: {
      type: 'object',
      required: ['allowed', 'graph_storage'],
      properties: {
        allowed: { type: 'boolean' },
        graph_storage: { $ref: '#/components/schemas/GraphStorageUsage' },
      },
      additionalProperties: true,
    },
    GraphRecord: {
      type: 'object',
      required: ['graph_uri', 'graph_id', 'title', 'status'],
      properties: {
        graph_uri: { type: 'string' },
        graph_id: { type: 'string' },
        title: { type: 'string' },
        description: { type: ['string', 'null'] },
        status: { type: 'string' },
        created_at: { type: ['string', 'null'] },
        updated_at: { type: ['string', 'null'] },
        triple_count: { type: ['integer', 'null'] },
        last_query_at: { type: ['string', 'null'] },
        last_update_at: { type: ['string', 'null'] },
        role: { type: ['string', 'null'] },
        owner_user_id: { type: ['string', 'null'] },
        granted_at: { type: ['string', 'null'] },
      },
      additionalProperties: true,
    },
    GraphRecordList: {
      type: 'array',
      items: { $ref: '#/components/schemas/GraphRecord' },
    },
    WorkspaceCreateDocumentRequest: {
      type: 'object',
      required: ['title'],
      properties: {
        documentId: { type: 'string' },
        title: { type: 'string' },
        parentId: { type: 'string' },
        order: { type: 'number' },
      },
    },
    WorkspaceCreateFolderRequest: {
      type: 'object',
      required: ['name'],
      properties: {
        folderId: { type: 'string' },
        name: { type: 'string' },
        parentId: { type: 'string' },
        section: { type: 'string' },
        order: { type: 'number' },
      },
    },
    WorkspaceMoveDocumentsRequest: {
      type: 'object',
      required: ['documentIds'],
      properties: {
        documentIds: { type: 'array', items: { type: 'string' } },
        parentId: { type: 'string' },
        order: { type: 'number' },
      },
    },
    DocumentRecord: {
      type: 'object',
      required: ['documentId', 'graphId', 'title', 'origin', 'providerId'],
      properties: {
        documentId: { type: 'string' },
        graphId: { type: 'string' },
        title: { type: 'string' },
        body: { type: 'string' },
        origin: { type: 'string' },
        providerId: { type: 'string' },
        localPath: { type: 'string' },
        rdfSubject: { type: 'string' },
        createdAt: { type: 'string' },
        updatedAt: { type: 'string' },
        capabilities: { type: 'array', items: { type: 'string' } },
        schemaVersion: { type: 'integer' },
        tiptapXml: { type: 'string' },
        tiptapJson: {},
        ydocUpdateBase64: { type: 'string' },
        ydocStatePath: { type: 'string' },
        tree: {},
        blocks: { type: 'array', items: { $ref: '#/components/schemas/BlockSnapshot' } },
        rdfTripleCount: { type: 'integer' },
      },
      additionalProperties: true,
    },
    DocumentRecordList: {
      type: 'array',
      items: { $ref: '#/components/schemas/DocumentRecord' },
    },
    BlockSnapshot: {
      type: 'object',
      properties: {
        id: { type: 'string' },
        type: { type: 'string' },
        content: { type: 'string' },
        parentId: { type: 'string' },
        order: { type: 'number' },
        level: { type: 'integer' },
        checked: { type: 'boolean' },
        language: { type: 'string' },
        marks: { type: 'array', items: { type: 'object', additionalProperties: true } },
      },
      additionalProperties: true,
    },
    InlineMark: {
      type: 'object',
      required: ['id', 'type', 'start', 'end'],
      properties: {
        id: { type: 'string' },
        type: { type: 'string' },
        start: { type: 'integer' },
        end: { type: 'integer' },
        href: { type: ['string', 'null'] },
        target_doc_id: { type: ['string', 'null'] },
        label: { type: ['string', 'null'] },
      },
      additionalProperties: true,
    },
    HostedBlock: {
      type: 'object',
      required: ['id', 'type', 'content', 'order'],
      properties: {
        id: { type: 'string' },
        type: { type: 'string', enum: ['paragraph', 'heading', 'bullet', 'numbered', 'todo', 'quote', 'code', 'divider'] },
        content: { type: 'string' },
        parentId: { type: ['string', 'null'] },
        order: { type: 'number' },
        level: { type: ['integer', 'null'] },
        checked: { type: ['boolean', 'null'] },
        language: { type: ['string', 'null'] },
        marks: { type: 'array', items: { $ref: '#/components/schemas/InlineMark' } },
      },
      additionalProperties: true,
    },
    DocumentPutRequest: {
      type: 'object',
      required: ['title'],
      properties: {
        title: { type: 'string' },
        blocks: { type: 'array', items: { $ref: '#/components/schemas/HostedBlock' } },
        expectedRevision: { type: ['integer', 'null'] },
        parentId: { type: ['string', 'null'] },
      },
    },
    DocumentDescriptionRequest: {
      type: 'object',
      required: ['description'],
      properties: {
        description: { type: 'string' },
      },
    },
    ManualSnapshotRequest: {
      type: 'object',
      properties: {
        label: { type: ['string', 'null'] },
      },
      additionalProperties: false,
    },
    SnapshotResponse: {
      type: 'object',
      required: [
        'snapshot_id',
        'graph_id',
        'doc_id',
        'is_manual',
        'tier',
        'tier_label',
        'snapshot_count',
        'chars_added',
        'chars_removed',
        'blocks_added',
        'blocks_removed',
        'blocks_modified',
        'created_at',
      ],
      properties: {
        snapshot_id: { type: 'string' },
        graph_id: { type: 'string' },
        doc_id: { type: 'string' },
        is_manual: { type: 'boolean' },
        tier: { type: 'string', enum: ['20min', '2h', '12h', 'daily', 'weekly'] },
        tier_label: { type: 'string' },
        snapshot_count: { type: 'integer' },
        chars_added: { type: 'integer' },
        chars_removed: { type: 'integer' },
        blocks_added: { type: 'integer' },
        blocks_removed: { type: 'integer' },
        blocks_modified: { type: 'integer' },
        created_at: { type: 'string' },
        label: { type: ['string', 'null'] },
      },
      additionalProperties: false,
    },
    SnapshotListResponse: {
      type: 'object',
      required: ['snapshots'],
      properties: {
        snapshots: { type: 'array', items: { $ref: '#/components/schemas/SnapshotResponse' } },
      },
      additionalProperties: false,
    },
    SnapshotCountResponse: {
      type: 'object',
      required: ['count'],
      properties: {
        count: { type: 'integer' },
      },
      additionalProperties: false,
    },
    EntityTypeInfo: {
      type: 'object',
      required: ['id', 'rdf_type', 'cascade_policy', 'supports_websocket'],
      properties: {
        id: { type: 'string', enum: ['document', 'folder', 'artifact'] },
        rdf_type: { type: 'string' },
        cascade_policy: { type: 'string', enum: ['automatic', 'block', 'none'] },
        supports_websocket: { type: 'boolean' },
      },
      additionalProperties: true,
    },
    EntityTypesResponse: {
      type: 'object',
      required: ['types'],
      properties: {
        types: { type: 'array', items: { $ref: '#/components/schemas/EntityTypeInfo' } },
      },
      additionalProperties: true,
    },
    EntityListMeta: {
      type: 'object',
      required: ['total', 'limit', 'offset', 'entity_type'],
      properties: {
        total: { type: 'integer' },
        limit: { type: 'integer' },
        offset: { type: 'integer' },
        entity_type: { type: 'string', enum: ['document', 'folder', 'artifact'] },
      },
      additionalProperties: true,
    },
    EntityListResponse: {
      type: 'object',
      required: ['data', 'meta'],
      properties: {
        data: {
          type: 'array',
          items: { $ref: '#/components/schemas/EntityResponse' },
        },
        meta: { $ref: '#/components/schemas/EntityListMeta' },
      },
      additionalProperties: true,
    },
    EntityPutRequest: {
      oneOf: [
        { $ref: '#/components/schemas/DocumentPutRequest' },
        { $ref: '#/components/schemas/FolderPutRequest' },
        { $ref: '#/components/schemas/ArtifactPutRequest' },
      ],
    },
    EntityResponse: {
      oneOf: [
        { $ref: '#/components/schemas/HostedDocument' },
        { $ref: '#/components/schemas/Folder' },
        { $ref: '#/components/schemas/Artifact' },
      ],
    },
    EntityDeleteResponse: {
      type: 'object',
      required: ['id', 'graphId', 'entityType', 'status'],
      properties: {
        id: { type: 'string' },
        graphId: { type: 'string' },
        graph_id: { type: 'string' },
        entityType: { type: 'string', enum: ['document', 'folder', 'artifact'] },
        entity_type: { type: 'string', enum: ['document', 'folder', 'artifact'] },
        status: { type: 'string', enum: ['deleted'] },
      },
      additionalProperties: true,
    },
    HostedDocumentSummary: {
      type: 'object',
      required: ['entityType', 'id', 'graphId', 'title', 'revision', 'readOnly'],
      properties: {
        entityType: { type: 'string', enum: ['document'] },
        id: { type: 'string' },
        graphId: { type: 'string' },
        title: { type: 'string' },
        revision: { type: 'integer' },
        snippet: { type: ['string', 'null'] },
        updatedAt: { type: ['string', 'null'] },
        lastAccessedAt: { type: ['string', 'null'] },
        parentId: { type: ['string', 'null'] },
        readOnly: { type: 'boolean' },
      },
      additionalProperties: false,
    },
    HostedDocumentSummaryList: {
      type: 'array',
      items: { $ref: '#/components/schemas/HostedDocumentSummary' },
    },
    BatchPrepareRequest: {
      type: 'object',
      required: ['folders', 'clientBatchKey'],
      properties: {
        folders: { type: 'array', items: { type: 'string' } },
        clientBatchKey: { type: 'string' },
      },
      additionalProperties: true,
    },
    BatchPrepareResponse: {
      type: 'object',
      required: ['batchId', 'folderMap'],
      properties: {
        batchId: { type: 'string' },
        folderMap: { type: 'object', additionalProperties: { type: 'string' } },
      },
      additionalProperties: true,
    },
    BatchRegisterDocument: {
      type: 'object',
      required: ['documentId', 'title', 'relativePath'],
      properties: {
        documentId: { type: 'string' },
        title: { type: 'string' },
        relativePath: { type: 'string' },
        readOnly: { type: 'boolean' },
        sourceFile: { type: ['object', 'null'], additionalProperties: true },
      },
      additionalProperties: true,
    },
    BatchRegisterRequest: {
      type: 'object',
      required: ['batchId', 'documents'],
      properties: {
        batchId: { type: 'string' },
        documents: { type: 'array', items: { $ref: '#/components/schemas/BatchRegisterDocument' } },
      },
      additionalProperties: true,
    },
    BatchRegisterResponse: {
      type: 'object',
      required: ['registered'],
      properties: {
        registered: { type: 'integer' },
        failed: { type: 'array', items: { type: 'string' } },
      },
      additionalProperties: true,
    },
    DocumentUploadRequest: {
      type: 'object',
      required: ['file'],
      properties: {
        file: { type: 'string', format: 'binary' },
        parent_id: { type: ['string', 'null'] },
        batch_id: { type: ['string', 'null'] },
      },
      additionalProperties: true,
    },
    DocumentUploadResponse: {
      type: 'object',
      required: ['documentId', 'title', 'fileType', 'readOnly'],
      properties: {
        documentId: { type: 'string' },
        title: { type: 'string' },
        fileType: { type: 'string' },
        readOnly: { type: 'boolean' },
        sourceFile: {
          type: ['object', 'null'],
          additionalProperties: true,
        },
      },
      additionalProperties: true,
    },
    PdfAccurateUploadRequest: {
      type: 'object',
      required: ['file'],
      properties: {
        file: { type: 'string', format: 'binary' },
        parent_id: { type: ['string', 'null'] },
        parentId: { type: ['string', 'null'] },
      },
      additionalProperties: true,
    },
    PdfAccurateUploadResponse: {
      type: 'object',
      required: ['jobId', 'documentId'],
      properties: {
        jobId: { type: 'string' },
        job_id: { type: 'string' },
        documentId: { type: 'string' },
        document_id: { type: 'string' },
        status: { type: 'string' },
        detail: {},
        trace_id: { type: 'string' },
        links: { $ref: '#/components/schemas/LocalJobLinks' },
      },
      additionalProperties: true,
    },
    ImageUploadRequest: {
      type: 'object',
      required: ['file'],
      properties: {
        file: { type: 'string', format: 'binary' },
      },
      additionalProperties: true,
    },
    ImageUploadResponse: {
      type: 'object',
      required: ['imageId', 'src'],
      properties: {
        imageId: { type: 'string' },
        src: { type: 'string' },
      },
      additionalProperties: true,
    },
    FolderPutRequest: {
      type: 'object',
      required: ['label'],
      properties: {
        label: { type: 'string' },
        parentId: { type: ['string', 'null'] },
        order: { type: 'number' },
        section: { type: 'string', enum: ['documents', 'artifacts'] },
      },
      additionalProperties: true,
    },
    Folder: {
      type: 'object',
      required: ['entityType', 'id', 'graphId', 'label', 'section'],
      properties: {
        entityType: { type: 'string', enum: ['folder'] },
        id: { type: 'string' },
        graphId: { type: 'string' },
        label: { type: 'string' },
        parentId: { type: ['string', 'null'] },
        order: { type: 'number' },
        section: { type: 'string', enum: ['documents', 'artifacts'] },
        createdAt: { type: ['string', 'number', 'null'] },
        updatedAt: { type: ['string', 'number', 'null'] },
      },
      additionalProperties: true,
    },
    FolderDeleteResponse: {
      type: 'object',
      required: ['id', 'graphId', 'status'],
      properties: {
        id: { type: 'string' },
        graphId: { type: 'string' },
        status: { type: 'string', enum: ['deleted'] },
      },
      additionalProperties: true,
    },
    ArtifactPutRequest: {
      type: 'object',
      required: ['label', 'originalFilename'],
      properties: {
        label: { type: 'string' },
        parentId: { type: ['string', 'null'] },
        order: { type: 'number' },
        fileType: { type: 'string' },
        status: { type: 'string', enum: ['uploading', 'processing', 'ready', 'error'] },
        errorMessage: { type: ['string', 'null'] },
        storageKey: { type: ['string', 'null'] },
        originalFilename: { type: 'string' },
        mimeType: { type: ['string', 'null'] },
        sizeBytes: { type: ['integer', 'null'] },
        ingestedDocId: { type: ['string', 'null'] },
      },
      additionalProperties: true,
    },
    Artifact: {
      type: 'object',
      required: ['entityType', 'id', 'graphId', 'label', 'originalFilename', 'status'],
      properties: {
        entityType: { type: 'string', enum: ['artifact'] },
        id: { type: 'string' },
        graphId: { type: 'string' },
        label: { type: 'string' },
        parentId: { type: ['string', 'null'] },
        order: { type: 'number' },
        fileType: { type: 'string' },
        status: { type: 'string' },
        errorMessage: { type: ['string', 'null'] },
        storageKey: { type: ['string', 'null'] },
        originalFilename: { type: 'string' },
        mimeType: { type: ['string', 'null'] },
        sizeBytes: { type: ['integer', 'null'] },
        ingestedDocId: { type: ['string', 'null'] },
        createdAt: { type: ['string', 'number', 'null'] },
        updatedAt: { type: ['string', 'number', 'null'] },
      },
      additionalProperties: true,
    },
    ArtifactDeleteResponse: {
      type: 'object',
      required: ['id', 'graphId', 'status'],
      properties: {
        id: { type: 'string' },
        graphId: { type: 'string' },
        status: { type: 'string', enum: ['deleted'] },
      },
      additionalProperties: true,
    },
    ArtifactImportRequest: {
      type: 'object',
      properties: {
        title: { type: ['string', 'null'] },
        parentId: { type: ['string', 'null'] },
        readOnly: { type: 'boolean' },
        useYdocPath: { type: 'boolean' },
      },
      additionalProperties: true,
    },
    ArtifactImportResponse: {
      type: 'object',
      required: ['documentId', 'title', 'readOnly'],
      properties: {
        documentId: { type: 'string' },
        title: { type: 'string' },
        readOnly: { type: 'boolean' },
      },
      additionalProperties: false,
    },
    ArtifactConvertRequest: {
      type: 'object',
      required: ['documentId'],
      properties: {
        documentId: { type: 'string' },
        title: { type: ['string', 'null'] },
      },
      additionalProperties: true,
    },
    ArtifactConvertResponse: {
      type: 'object',
      required: ['documentId', 'title'],
      properties: {
        documentId: { type: 'string' },
        title: { type: 'string' },
      },
      additionalProperties: true,
    },
    ArtifactList: {
      type: 'array',
      items: { $ref: '#/components/schemas/Artifact' },
    },
    HostedDocument: {
      allOf: [
        { $ref: '#/components/schemas/HostedDocumentSummary' },
        {
          type: 'object',
          required: ['blocks'],
          properties: {
            blocks: { type: 'array', items: { $ref: '#/components/schemas/HostedBlock' } },
            createdAt: { type: ['string', 'null'] },
            createdBy: { type: ['string', 'null'] },
          },
        },
      ],
    },
    DocumentDeleteResponse: {
      type: 'object',
      required: ['id', 'graphId', 'status'],
      properties: {
        id: { type: 'string' },
        graphId: { type: 'string' },
        status: { type: 'string', enum: ['deleted'] },
      },
    },
    BlockSummary: {
      type: 'object',
      required: ['id', 'type', 'text', 'preview'],
      properties: {
        id: { type: 'string' },
        type: { type: 'string' },
        level: { type: ['integer', 'null'] },
        text: { type: 'string' },
        preview: { type: 'string' },
      },
      additionalProperties: true,
    },
    BlocksResponse: {
      type: 'object',
      required: ['blocks'],
      properties: {
        blocks: { type: 'array', items: { $ref: '#/components/schemas/BlockSummary' } },
      },
      additionalProperties: false,
    },
    BlockContextItem: {
      allOf: [
        { $ref: '#/components/schemas/BlockSummary' },
        {
          type: 'object',
          properties: {
            is_target: { type: 'boolean' },
          },
        },
      ],
    },
    BlockContextResponse: {
      type: 'object',
      required: ['doc_id', 'mode', 'title', 'blocks'],
      properties: {
        doc_id: { type: 'string' },
        block_id: { type: ['string', 'null'] },
        mode: { type: 'string', enum: ['block', 'toc', 'document'] },
        title: { type: 'string' },
        blocks: { type: 'array', items: { $ref: '#/components/schemas/BlockContextItem' } },
      },
      additionalProperties: false,
    },
    EnqueueCrdtOperationRequest: {
      type: 'object',
      required: ['kind', 'graphId'],
      properties: {
        kind: { type: 'string' },
        graphId: { type: 'string' },
        documentId: { type: 'string' },
        payload: {},
      },
    },
    CrdtOperationResult: {
      type: 'object',
      required: ['ok'],
      properties: {
        ok: { type: 'boolean' },
        value: {},
        error: { type: 'string' },
      },
    },
    SparqlQueryRequest: {
      type: 'object',
      required: ['graphId', 'query'],
      properties: {
        graphId: { type: 'string' },
        query: { type: 'string' },
        timeoutMs: { type: 'integer', minimum: 1 },
        maxRows: { type: 'integer', minimum: 1 },
      },
    },
    SparqlUpdateRequest: {
      type: 'object',
      required: ['graphId', 'update'],
      properties: {
        graphId: { type: 'string' },
        update: { type: 'string' },
      },
    },
    SparqlQueryResult: {
      type: 'object',
      required: ['resultType', 'variables', 'rows', 'boolean', 'graph', 'quadCount'],
      properties: {
        resultType: { type: 'string' },
        variables: { type: 'array', items: { type: 'string' } },
        rows: { type: 'array', items: { type: 'object', additionalProperties: { type: 'string' } } },
        boolean: { type: ['boolean', 'null'] },
        graph: { type: ['string', 'null'] },
        quadCount: { type: 'integer' },
        warnings: { type: 'array', items: { type: 'string' } },
      },
    },
    RdfLoadRequest: {
      type: 'object',
      required: ['graphId', 'data', 'format'],
      properties: {
        graphId: { type: 'string' },
        data: { type: 'string' },
        format: { type: 'string' },
        baseIri: { type: 'string' },
        targetGraphIri: { type: 'string' },
      },
    },
    RdfDumpRequest: {
      type: 'object',
      required: ['graphId', 'format'],
      properties: {
        graphId: { type: 'string' },
        format: { type: 'string' },
        sourceGraphIri: { type: 'string' },
      },
    },
    RdfImportRequest: {
      type: 'object',
      required: ['file'],
      properties: {
        file: { type: 'string', format: 'binary' },
        format: {
          type: ['string', 'null'],
          description: 'Optional RDF MIME type override such as text/turtle or application/trig.',
        },
      },
      additionalProperties: true,
    },
    VaultImportRequest: {
      type: 'object',
      required: ['file'],
      properties: {
        file: { type: 'string', format: 'binary' },
        folder_name: {
          type: ['string', 'null'],
          description: 'Optional root folder name to nest the imported vault/export under.',
        },
        folderName: {
          type: ['string', 'null'],
          description: 'Camel-case alias for folder_name.',
        },
      },
      additionalProperties: true,
    },
    GraphArchiveImportRequest: {
      type: 'object',
      required: ['file', 'new_graph_id'],
      properties: {
        file: { type: 'string', format: 'binary' },
        new_graph_id: {
          type: 'string',
          description: 'URL-safe identifier for the newly imported graph.',
        },
        newGraphId: {
          type: 'string',
          description: 'Camel-case alias for new_graph_id.',
        },
        new_title: {
          type: ['string', 'null'],
          description: 'Optional title for the imported graph.',
        },
        newTitle: {
          type: ['string', 'null'],
          description: 'Camel-case alias for new_title.',
        },
      },
      additionalProperties: true,
    },
    CellArchiveRestoreRequest: {
      type: 'object',
      required: [
        'file', 'operation_id', 'archive_sha256', 'source_graph_id',
        'source_user_id', 'target_generation', 'plan_digest',
        'expected_document_count', 'expected_rdf_triple_count',
      ],
      properties: {
        file: { type: 'string', format: 'binary' },
        operation_id: { type: 'string', maxLength: 160, pattern: '^[A-Za-z0-9._:-]+$' },
        archive_sha256: { type: 'string', pattern: '^[0-9a-f]{64}$' },
        source_graph_id: { type: 'string' },
        source_user_id: { type: 'string' },
        target_generation: { type: 'integer', minimum: 1 },
        plan_digest: { type: 'string', pattern: '^[0-9a-f]{64}$' },
        expected_document_count: { type: 'integer', minimum: 0 },
        expected_rdf_triple_count: { type: 'integer', minimum: 0 },
      },
      additionalProperties: false,
    },
    RdfDumpResult: {
      type: 'object',
      required: ['format', 'mediaType', 'data', 'quadCount'],
      properties: {
        format: { type: 'string' },
        mediaType: { type: 'string' },
        data: { type: 'string' },
        quadCount: { type: 'integer' },
      },
    },
    MutationResult: {
      type: 'object',
      required: ['ok', 'quadCount'],
      properties: {
        ok: { type: 'boolean' },
        quadCount: { type: 'integer' },
      },
      additionalProperties: true,
    },
    LocalGraphQueryRequest: {
      type: 'object',
      required: ['graph_id', 'sparql'],
      properties: {
        graph_id: { type: 'string' },
        sparql: { type: 'string' },
        result_format: { type: 'string' },
        timeout_ms: { type: 'integer', minimum: 1 },
        max_rows: { type: 'integer', minimum: 1 },
      },
    },
    LocalGraphUpdateRequest: {
      type: 'object',
      required: ['graph_id', 'sparql'],
      properties: {
        graph_id: { type: 'string' },
        sparql: { type: 'string' },
      },
    },
    LocalGraphQuerySubmitResponse: {
      type: 'object',
      required: ['job_id', 'status', 'trace_id', 'poll_url', 'result_url'],
      properties: {
        job_id: { type: 'string' },
        status: { type: 'string' },
        trace_id: { type: 'string' },
        poll_url: { type: 'string' },
        result_url: { type: 'string' },
      },
      additionalProperties: true,
    },
    LocalJobLinks: {
      type: 'object',
      required: ['status'],
      properties: {
        status: { type: 'string' },
        result: { type: ['string', 'null'] },
        websocket: {
          type: ['object', 'null'],
          properties: {
            description: { type: 'string' },
            payload: {},
          },
          additionalProperties: true,
        },
      },
      additionalProperties: true,
    },
    LocalJobSubmitResponse: {
      type: 'object',
      required: ['job_id', 'status', 'trace_id', 'links'],
      properties: {
        job_id: { type: 'string' },
        status: { type: 'string' },
        detail: {},
        progress: {
          anyOf: [
            { $ref: '#/components/schemas/LocalJobProgress' },
            { type: 'null' },
          ],
        },
        trace_id: { type: 'string' },
        links: { $ref: '#/components/schemas/LocalJobLinks' },
      },
      additionalProperties: true,
    },
    ImportJobResponse: {
      type: 'object',
      required: ['jobId', 'status', 'links'],
      properties: {
        jobId: { type: 'string' },
        job_id: { type: 'string' },
        status: { type: 'string' },
        detail: {},
        progress: {
          anyOf: [
            { $ref: '#/components/schemas/LocalJobProgress' },
            { type: 'null' },
          ],
        },
        trace_id: { type: 'string' },
        links: { $ref: '#/components/schemas/LocalJobLinks' },
      },
      additionalProperties: true,
    },
    ImportResponse: {
      type: 'object',
      required: ['status', 'documentsCreated', 'documentIds', 'warnings', 'errors'],
      properties: {
        status: { type: 'string', enum: ['complete', 'error'] },
        documentsCreated: { type: 'integer' },
        documents_created: { type: 'integer' },
        foldersCreated: { type: 'integer' },
        folders_created: { type: 'integer' },
        wiresCreated: { type: 'integer' },
        wires_created: { type: 'integer' },
        tagsCreated: { type: 'integer' },
        tags_created: { type: 'integer' },
        unresolvedLinks: { type: 'integer' },
        unresolved_links: { type: 'integer' },
        documentIds: { type: 'array', items: { type: 'string' } },
        document_ids: { type: 'array', items: { type: 'string' } },
        warnings: { type: 'array', items: { type: 'string' } },
        errors: { type: 'array', items: { type: 'string' } },
      },
      additionalProperties: true,
    },
    ClipUrlRequest: {
      type: 'object',
      required: ['url'],
      properties: {
        url: { type: 'string', format: 'uri' },
        title: { type: ['string', 'null'] },
        folderId: { type: ['string', 'null'] },
        folder_id: { type: ['string', 'null'] },
      },
      additionalProperties: true,
    },
    YouTubeTranscriptRequest: {
      type: 'object',
      required: ['url'],
      properties: {
        url: { type: 'string' },
        title: { type: ['string', 'null'] },
        folderId: { type: ['string', 'null'] },
        folder_id: { type: ['string', 'null'] },
        languages: { type: ['array', 'null'], items: { type: 'string' } },
        mode: { type: 'string', enum: ['readable', 'timestamped'] },
        chunkSeconds: { type: 'integer', minimum: 5, maximum: 600 },
        chunk_seconds: { type: 'integer', minimum: 5, maximum: 600 },
        showRanges: { type: 'boolean' },
        show_ranges: { type: 'boolean' },
      },
      additionalProperties: true,
    },
    LocalJobStatusResponse: {
      type: 'object',
      required: ['job_id', 'status', 'updated_at', 'links'],
      properties: {
        job_id: { type: 'string' },
        status: { type: 'string' },
        updated_at: { type: 'string' },
        user_id: { type: ['string', 'null'] },
        submitted_at: { type: ['string', 'null'] },
        started_at: { type: ['string', 'null'] },
        completed_at: { type: ['string', 'null'] },
        processing_time_ms: { type: ['integer', 'null'] },
        detail: {},
        progress: {
          anyOf: [
            { $ref: '#/components/schemas/LocalJobProgress' },
            { type: 'null' },
          ],
        },
        error: { type: ['string', 'null'] },
        links: { $ref: '#/components/schemas/LocalJobLinks' },
      },
      additionalProperties: true,
    },
    LocalJobRecord: {
      $ref: '#/components/schemas/LocalJobStatusResponse',
    },
    LocalJobProgress: {
      type: 'object',
      required: ['phase', 'message', 'current', 'total', 'percent', 'updated_at'],
      properties: {
        phase: { type: 'string' },
        message: { type: 'string' },
        current: { type: 'integer' },
        total: { type: 'integer' },
        percent: { type: 'number' },
        updated_at: { type: 'string' },
        details: {},
      },
      additionalProperties: true,
    },
    LocalJobCancelResponse: {
      type: 'object',
      required: ['job_id', 'cancelled', 'previous_status', 'message'],
      properties: {
        job_id: { type: 'string' },
        cancelled: { type: 'boolean' },
        previous_status: { type: 'string' },
        message: { type: 'string' },
      },
      additionalProperties: true,
    },
    JsonValue: {},
    SemanticModelConfigRequest: {
      type: 'object',
      required: ['modelId'],
      properties: {
        modelId: { type: 'string' },
        batchSize: { type: 'integer', minimum: 1, maximum: 64 },
      },
      additionalProperties: false,
    },
    SemanticModelStatus: {
      type: 'object',
      required: ['providerId', 'modelId', 'displayName', 'hfRepo', 'dimensions', 'prepared', 'loaded', 'setupRequired'],
      properties: {
        providerId: { type: 'string' },
        modelId: { type: 'string' },
        displayName: { type: 'string' },
        hfRepo: { type: 'string' },
        dimensions: { type: 'integer' },
        maxTokens: { type: 'integer' },
        backend: { type: 'string' },
        runtime: { type: 'string' },
        defaultBatchSize: { type: 'integer' },
        effectiveBatchSize: { type: 'integer' },
        runtimeAvailable: { type: 'boolean' },
        reason: { type: ['string', 'null'] },
        prepared: { type: 'boolean' },
        loaded: { type: 'boolean' },
        setupRequired: { type: 'boolean' },
        configPath: { type: 'string' },
        setupPath: { type: 'string' },
        cachePolicy: { type: 'string' },
        cachePath: { type: 'string' },
        preparedAt: { type: ['string', 'null'] },
      },
    },
    SemanticModelDescriptor: {
      type: 'object',
      required: ['providerId', 'modelId', 'displayName', 'family', 'dimensions', 'maxTokens', 'status', 'selected', 'recommended'],
      properties: {
        providerId: { type: 'string' },
        modelId: { type: 'string' },
        displayName: { type: 'string' },
        family: { type: 'string' },
        dimensions: { type: 'integer' },
        maxTokens: { type: 'integer' },
        backend: { type: 'string' },
        taskPrefixes: { type: 'array', items: { type: 'string' } },
        hfRepo: { type: ['string', 'null'] },
        runtime: { type: 'string' },
        privacy: { type: 'string' },
        cachePolicy: { type: 'string' },
        cachePath: { type: 'string' },
        setupPath: { type: 'string' },
        setupRequired: { type: 'boolean' },
        prepared: { type: 'boolean' },
        loaded: { type: 'boolean' },
        preparedAt: { type: ['string', 'null'] },
        defaultBatchSize: { type: 'integer' },
        effectiveBatchSize: { type: 'integer' },
        speed: { type: 'string' },
        quality: { type: 'string' },
        available: { type: 'boolean' },
        selectable: { type: 'boolean' },
        reason: { type: ['string', 'null'] },
        setupHint: { type: ['string', 'null'] },
        status: { type: 'string' },
        selected: { type: 'boolean' },
        recommended: { type: 'boolean' },
        sizeHint: { type: 'string' },
        strengths: { type: 'array', items: { type: 'string' } },
        limitations: { type: 'array', items: { type: 'string' } },
      },
    },
    SemanticModelDescriptorList: {
      type: 'array',
      items: { $ref: '#/components/schemas/SemanticModelDescriptor' },
    },
    DoclingRuntimeStatus: {
      type: 'object',
      required: ['runtimeId', 'approachId', 'packageName', 'packageRequirement', 'prepared', 'available', 'supported', 'setupRequired', 'status', 'platform'],
      properties: {
        runtimeId: { type: 'string' },
        approachId: { type: 'string' },
        packageName: { type: 'string' },
        packageRequirement: { type: 'string' },
        prepared: { type: 'boolean' },
        available: { type: 'boolean' },
        supported: { type: 'boolean' },
        setupRequired: { type: 'boolean' },
        setupPath: { type: 'string' },
        cachePolicy: { type: 'string' },
        cachePath: { type: 'string' },
        helperPath: { type: 'string' },
        pythonExecutable: { type: ['string', 'null'] },
        doclingVersion: { type: ['string', 'null'] },
        preparedAt: { type: ['string', 'null'] },
        platform: { type: 'string' },
        status: { type: 'string' },
        reason: { type: ['string', 'null'] },
        limitations: { type: 'array', items: { type: 'string' } },
      },
    },
    PdfIngestionPipelineConfigRequest: {
      type: 'object',
      required: ['preferredEngineId'],
      properties: {
        preferredEngineId: { type: 'string' },
      },
    },
    PdfIngestionPreferenceOption: {
      type: 'object',
      required: ['engineId', 'label', 'description'],
      properties: {
        engineId: { type: 'string' },
        label: { type: 'string' },
        description: { type: 'string' },
      },
    },
    PdfIngestionEngineDescriptor: {
      type: 'object',
      required: ['engineId', 'label', 'status', 'available', 'implemented', 'selectable', 'runtimeAvailable', 'runtimeStatus', 'runtimeReason', 'fallbackEngineId', 'runtime', 'role', 'speed', 'fidelity', 'output', 'setupRequired'],
      properties: {
        engineId: { type: 'string' },
        label: { type: 'string' },
        status: { type: 'string' },
        available: { type: 'boolean' },
        implemented: { type: 'boolean' },
        selectable: { type: 'boolean' },
        runtimeAvailable: { type: 'boolean' },
        runtimeStatus: { type: 'string' },
        runtimeReason: { type: ['string', 'null'] },
        fallbackEngineId: { type: ['string', 'null'] },
        runtime: { type: 'string' },
        role: { type: 'string' },
        speed: { type: 'string' },
        fidelity: { type: 'string' },
        output: { type: 'string' },
        setupRequired: { type: 'boolean' },
        supportsOcr: { type: 'boolean' },
        supportsTables: { type: 'boolean' },
        supportsPageAnchors: { type: 'boolean' },
        supportsOriginalView: { type: 'boolean' },
        supportsSourceAnnotations: { type: 'boolean' },
        bestFor: { type: 'array', items: { type: 'string' } },
        capabilities: { type: 'array', items: { type: 'string' } },
        limitations: { type: 'array', items: { type: 'string' } },
        setupHint: { type: ['string', 'null'] },
        reason: { type: ['string', 'null'] },
      },
    },
    PdfIngestionPipelineStatus: {
      type: 'object',
      required: ['schemaVersion', 'preferredEngineId', 'effectiveEngineId', 'effectiveReason', 'configPath', 'platform', 'engines', 'preferenceOptions', 'doclingRuntimeStatus'],
      properties: {
        schemaVersion: { type: 'integer' },
        preferredEngineId: { type: 'string' },
        effectiveEngineId: { type: 'string' },
        effectiveReason: { type: 'string' },
        configPath: { type: 'string' },
        platform: { type: 'string' },
        engines: { type: 'array', items: { $ref: '#/components/schemas/PdfIngestionEngineDescriptor' } },
        preferenceOptions: { type: 'array', items: { $ref: '#/components/schemas/PdfIngestionPreferenceOption' } },
        doclingRuntimeStatus: { $ref: '#/components/schemas/DoclingRuntimeStatus' },
      },
    },
    IngestionApproachDescriptor: {
      type: 'object',
      required: ['approachId', 'label', 'family', 'status', 'runtime', 'fileTypes', 'mimeTypes', 'speed', 'fidelity', 'output', 'selectable'],
      properties: {
        approachId: { type: 'string' },
        label: { type: 'string' },
        family: { type: 'string' },
        status: { type: 'string' },
        runtime: { type: 'string' },
        fileTypes: { type: 'array', items: { type: 'string' } },
        mimeTypes: { type: 'array', items: { type: 'string' } },
        speed: { type: 'string' },
        fidelity: { type: 'string' },
        output: { type: 'string' },
        setupRequired: { type: 'boolean' },
        selectable: { type: 'boolean' },
        defaultFor: { type: 'array', items: { type: 'string' } },
        supportsOcr: { type: 'boolean' },
        supportsTables: { type: 'boolean' },
        supportsPageAnchors: { type: 'boolean' },
        supportsOriginalView: { type: 'boolean' },
        supportsSourceAnnotations: { type: 'boolean' },
        dependencySummary: { type: 'string' },
        bestFor: { type: 'array', items: { type: 'string' } },
        limitations: { type: 'array', items: { type: 'string' } },
      },
    },
    IngestionApproachDescriptorList: {
      type: 'array',
      items: { $ref: '#/components/schemas/IngestionApproachDescriptor' },
    },
    RefreshSemanticIndexRequest: {
      type: 'object',
      required: ['graphId'],
      properties: { graphId: { type: 'string' } },
    },
    SemanticIndexStatus: {
      type: 'object',
      required: ['graphId', 'providerId', 'modelId', 'dimensions', 'exists', 'blockCount', 'documentCount', 'staleDocumentCount'],
      properties: {
        graphId: { type: 'string' },
        providerId: { type: 'string' },
        modelId: { type: 'string' },
        dimensions: { type: 'integer' },
        activeModelId: { type: 'string' },
        activeDimensions: { type: 'integer' },
        compatible: { type: 'boolean' },
        compatibilityReason: { type: 'string' },
        exists: { type: 'boolean' },
        indexPath: { type: 'string' },
        blockCount: { type: 'integer' },
        documentCount: { type: 'integer' },
        staleDocumentCount: { type: 'integer' },
        indexedAt: { type: ['string', 'null'] },
      },
    },
    SemanticSearchRequest: {
      type: 'object',
      required: ['graphId', 'query'],
      properties: {
        graphId: { type: 'string' },
        query: { type: 'string' },
        limit: { type: 'integer' },
      },
    },
    HostedSearchRequest: {
      type: 'object',
      properties: {
        graphId: { type: 'string' },
        graph_id: { type: 'string' },
        query: { type: 'string' },
        limit: { type: 'integer' },
        docFilter: { type: 'string' },
        doc_filter: { type: 'string' },
        minScore: { type: 'number' },
        min_score: { type: 'number' },
      },
    },
    ReindexRequest: {
      type: 'object',
      required: ['graph_id'],
      properties: {
        graph_id: { type: 'string' },
        graphId: { type: 'string' },
      },
    },
    ReindexResponse: {
      type: 'object',
      required: ['graph_id', 'total_docs', 'queued'],
      properties: {
        graph_id: { type: 'string' },
        total_docs: { type: 'integer' },
        queued: { type: 'integer' },
        async: { type: 'boolean' },
        setup_required: { type: 'boolean' },
        job_id: { type: 'string' },
        status: { type: 'string' },
        detail: {},
        progress: {
          anyOf: [
            { $ref: '#/components/schemas/LocalJobProgress' },
            { type: 'null' },
          ],
        },
        trace_id: { type: 'string' },
        links: { $ref: '#/components/schemas/LocalJobLinks' },
      },
      additionalProperties: true,
    },
    SearchHit: {
      type: 'object',
      required: ['block_id', 'doc_id', 'doc_title', 'text_preview', 'score'],
      properties: {
        block_id: { type: 'string' },
        doc_id: { type: 'string' },
        doc_title: { type: 'string' },
        text_preview: { type: 'string' },
        score: { type: 'number' },
      },
    },
    BlockSearchHit: {
      type: 'object',
      required: ['block_id', 'doc_id', 'doc_title', 'text_preview', 'score'],
      properties: {
        block_id: { type: 'string' },
        doc_id: { type: 'string' },
        doc_title: { type: 'string' },
        text_preview: { type: 'string' },
        score: { type: 'number' },
        match_source: { type: 'string' },
      },
    },
    BlockSearchResponse: {
      type: 'object',
      required: ['query', 'results', 'count', 'lexical_count', 'semantic_count'],
      properties: {
        query: { type: 'string' },
        results: { type: 'array', items: { $ref: '#/components/schemas/BlockSearchHit' } },
        count: { type: 'integer' },
        lexical_count: { type: 'integer' },
        semantic_count: { type: 'integer' },
      },
    },
    SemanticSearchResponse: {
      type: 'object',
      required: ['query', 'results', 'total_count', 'model'],
      properties: {
        query: { type: 'string' },
        results: { type: 'array', items: { $ref: '#/components/schemas/SearchHit' } },
        total_count: { type: 'integer' },
        model: { type: 'string' },
      },
    },
    HybridSearchHit: {
      type: 'object',
      required: ['block_id', 'doc_id', 'doc_title', 'text_preview', 'score'],
      properties: {
        block_id: { type: 'string' },
        doc_id: { type: 'string' },
        doc_title: { type: 'string' },
        text_preview: { type: 'string' },
        score: { type: 'number' },
        match_source: { type: 'string' },
      },
    },
    HybridSearchResponse: {
      type: 'object',
      required: ['query', 'results', 'count', 'lexical_count', 'semantic_count'],
      properties: {
        query: { type: 'string' },
        results: { type: 'array', items: { $ref: '#/components/schemas/HybridSearchHit' } },
        count: { type: 'integer' },
        lexical_count: { type: 'integer' },
        semantic_count: { type: 'integer' },
      },
    },
    SemanticSearchResult: {
      type: 'object',
      required: ['graphId', 'query', 'hits'],
      properties: {
        graphId: { type: 'string' },
        query: { type: 'string' },
        providerId: { type: 'string' },
        modelId: { type: 'string' },
        dimensions: { type: 'integer' },
        indexedAt: { type: ['string', 'null'] },
        blockCount: { type: 'integer' },
        hits: { type: 'array', items: { type: 'object', additionalProperties: true } },
      },
    },
    JsonRpcRequest: {
      type: 'object',
      required: ['jsonrpc', 'method'],
      properties: {
        jsonrpc: { type: 'string', enum: ['2.0'] },
        id: {},
        method: { type: 'string' },
        params: {},
      },
    },
    McpInfo: {
      type: 'object',
      properties: {
        protocol: { type: 'string' },
        endpoint: { type: 'string' },
        methods: { type: 'array', items: { type: 'string' } },
      },
      additionalProperties: true,
    },
  },
}

const componentSchemaNames = new Set(Object.keys(components.schemas))

function collectSchemaRefs(value, refs = []) {
  if (Array.isArray(value)) {
    for (const item of value) collectSchemaRefs(item, refs)
    return refs
  }
  if (!value || typeof value !== 'object') return refs
  if (typeof value.$ref === 'string') refs.push(value.$ref)
  for (const item of Object.values(value)) collectSchemaRefs(item, refs)
  return refs
}

function assertRouteSchemaNames(label, names) {
  const missing = names
    .filter(Boolean)
    .filter((name) => !componentSchemaNames.has(name))
  if (missing.length > 0) {
    throw new Error(`${label} references unknown OpenAPI schema(s): ${missing.join(', ')}`)
  }
}

function assertComponentSchemaRefs(label, value) {
  const missing = collectSchemaRefs(value)
    .filter((ref) => ref.startsWith('#/components/schemas/'))
    .map((ref) => ref.slice('#/components/schemas/'.length))
    .filter((name) => !componentSchemaNames.has(name))
  if (missing.length > 0) {
    throw new Error(`${label} contains unresolved OpenAPI schema ref(s): ${missing.join(', ')}`)
  }
}

assertRouteSchemaNames('requestSchemaByRoute', Object.values(requestSchemaByRoute))
assertRouteSchemaNames('responseSchemaByRoute', Object.values(responseSchemaByRoute))
assertRouteSchemaNames('multipartRequestSchemaByRoute', Object.values(multipartRequestSchemaByRoute))
assertComponentSchemaRefs('OpenAPI components', components)

const paths = {}
for (const route of surface.routes) {
  addOperation(paths, route)
}
assertComponentSchemaRefs('OpenAPI paths', paths)

const spec = {
  openapi: '3.1.0',
  info: {
    title: 'Sophia Native Local Loopback API',
    version: '0.1.0',
    description: 'Token-protected localhost API exposed by the native local Tauri runtime. Generated from parity/local-loopback-surface.json.',
  },
  paths,
  components,
}

const renderedSpec = `${JSON.stringify(spec, null, 2)}\n`
const relativeOutputPath = path.relative(process.cwd(), outputPath)

if (checkMode) {
  let currentSpec
  try {
    currentSpec = await fs.readFile(outputPath, 'utf8')
  } catch (error) {
    throw new Error(`Unable to read ${relativeOutputPath}: ${error.message}`)
  }
  if (currentSpec !== renderedSpec) {
    throw new Error(`${relativeOutputPath} is stale; run pnpm parity:openapi`)
  }
  console.log(`checked ${relativeOutputPath}`)
} else {
  await fs.writeFile(outputPath, renderedSpec)
  console.log(`wrote ${relativeOutputPath}`)
}

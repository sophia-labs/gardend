import { IT, small, SMOKE } from './_util.mjs'
const VOC = 'http://mnemosyne.ai/vocab#'
const mk = (id, it, op) => ({ id, version: 1, kind: 'read', suites: SMOKE, params: IT(...it), needs: ['book', 'small'], op })
const sq = (q) => (ctx) => ctx.mcp('sparql_query', { graphId: ctx.graphId, query: q })
export const cases = [
  mk('read.read_document.small', [[10, 2], [60, 5]], (ctx, i) => ctx.mcp('read_document', { graphId: ctx.graphId, documentId: small(ctx, i).id })),
  mk('read.read_document.book_page', [[8, 2], [40, 3]], (ctx, i) => ctx.mcp('read_document', { graphId: ctx.graphId, documentId: ctx.fixture.book.id, maxChars: 65536, offset: (i % 6) * 40000 })),
  mk('read.read_blocks.book', [[8, 2], [40, 3]], (ctx, i) => ctx.mcp('read_blocks', { graphId: ctx.graphId, documentId: ctx.fixture.book.id, offset: (i * 97) % Math.max(1, ctx.fixture.book.blockIds.length - 100), limit: 100 })),
  mk('read.list_documents', [[8, 2], [40, 3]], (ctx) => ctx.mcp('list_documents', { graphId: ctx.graphId, limit: 100 })),
  mk('read.search_documents', [[8, 2], [40, 3]], (ctx, i) => ctx.mcp('search_documents', { graphId: ctx.graphId, query: ctx.lex[(i * 13) % ctx.lex.length], limit: 20 })),
  mk('read.sparql.count', [[8, 2], [40, 3]], sq('SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }')),
  mk('read.sparql.titles', [[8, 2], [40, 3]], sq('PREFIX dct: <http://purl.org/dc/terms/>\nSELECT ?s ?t WHERE { GRAPH ?g { ?s dct:title ?t } } LIMIT 500')),
  mk('read.sparql.wires_2hop', [[8, 2], [40, 3]], sq(`PREFIX v: <${VOC}>\nSELECT ?a ?b ?c WHERE { GRAPH ?g { ?w1 v:sourceDocument ?a ; v:targetDocument ?b . ?w2 v:sourceDocument ?b ; v:targetDocument ?c } } LIMIT 200`)),
  mk('read.get_backlinks', [[8, 2], [40, 3]], (ctx, i) => ctx.mcp('get_backlinks', { graphId: ctx.graphId, documentId: small(ctx, 1 + (i % 40)).id })),
  mk('read.get_workspace', [[8, 2], [40, 3]], (ctx) => ctx.mcp('get_workspace', { graphId: ctx.graphId })),
  mk('read.context_bundle', [[5, 1], [25, 2]], (ctx) => ctx.mcp('context_bundle', { graphId: ctx.graphId, set_home: false })),
  mk('read.document_digest.book', [[8, 2], [40, 3]], (ctx) => ctx.mcp('document_digest', { graphId: ctx.graphId, documentId: ctx.fixture.book.id })),
]

import { IT, SUITES, graphFor } from './_rich.mjs'
import { ntriples } from '../../fixture/parts/rich-gen.mjs'
const seed = (ctx) => ctx.fixture.seed ?? 1
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const ns = (ctx) => `urn:gbench:${ctx.tag}`
const load = (ctx, data, graph) => ctx.mcp('rdf_load', { graphId: ctx.graphId, data, format: 'ntriples', targetGraphIri: graph })
const drop = (ctx, g) => ctx.mcp('sparql_update', { graphId: ctx.graphId, update: `DROP SILENT GRAPH <${g}>` })

export const cases = [
  // Bulk RDF load into a fresh named graph per iteration: 10k vs 100k triples (a ~1 MB and ~10 MB request body). Each graph is dropped in teardown.
  ...[[10_000, [3, 1], [8, 1]], [100_000, [1, 0], [3, 0]]].map(([n, smoke, std]) => mk(`rich.rdf.load.t${n / 1000}k`, [smoke, std], {
    async setup(ctx) { await graphFor(ctx, 'rl' + n / 1000); ctx.S = { nt: ntriples(n, seed(ctx), ns(ctx)), n } },
    op: (ctx, i) => load(ctx, ctx.S.nt, `${ns(ctx)}:g:${i}`),
    async teardown(ctx) { for (let i = 0; i < ctx.params.iterations + ctx.params.warmup; i++) await drop(ctx, `${ns(ctx)}:g:${i}`) } })),
  ...[10_000, 100_000].map((n) => mk(`rich.rdf.dump.t${n / 1000}k`, [[2, 1], [n > 50_000 ? 5 : 10, 1]], {
    async setup(ctx) { await graphFor(ctx, 'rd' + n / 1000); ctx.S = { g: `${ns(ctx)}:dump` }; await load(ctx, ntriples(n, seed(ctx), ns(ctx)), ctx.S.g) },
    op: (ctx) => ctx.mcp('rdf_dump', { graphId: ctx.graphId, format: 'ntriples', sourceGraphIri: ctx.S.g }) })),
  // SPARQL Update: INSERT DATA of 1,000 triples; and a DELETE/INSERT ... WHERE rewriting every integer literal in a 10k-triple graph.
  mk('rich.rdf.update_insert.t1k', [[3, 1], [15, 2]], { async setup(ctx) { await graphFor(ctx, 'rui'); ctx.S = { lines: ntriples(1000 * 30, seed(ctx), ns(ctx)).split('\n') } },
    op: (ctx, i) => ctx.mcp('sparql_update', { graphId: ctx.graphId, update: `INSERT DATA { GRAPH <${ns(ctx)}:u:${i}> {\n${ctx.S.lines.slice(i * 1000, i * 1000 + 1000).join('\n')}\n} }` }) }),
  mk('rich.rdf.update_where.t10k', [[2, 1], [8, 1]], { async setup(ctx) { await graphFor(ctx, 'ruw'); ctx.S = { g: `${ns(ctx)}:w` }; await load(ctx, ntriples(10_000, seed(ctx), ns(ctx)), ctx.S.g) },
    op: (ctx, i) => ctx.mcp('sparql_update', { graphId: ctx.graphId, update: `DELETE { GRAPH <${ctx.S.g}> { ?s <${ns(ctx)}:p:count> ?o } } INSERT { GRAPH <${ctx.S.g}> { ?s <${ns(ctx)}:p:count> ?n } } WHERE { GRAPH <${ctx.S.g}> { ?s <${ns(ctx)}:p:count> ?o } BIND(?o + 1 AS ?n) }` }) }),
]

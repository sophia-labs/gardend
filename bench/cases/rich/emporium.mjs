import { IT, SUITES, graphFor } from './_rich.mjs'
import { ONTOLOGY_CLASSES, gizmoRecords, memoryRecords } from '../../fixture/parts/rich-gen.mjs'

const seed = (ctx) => ctx.fixture.seed ?? 1
const W = (ctx, records, extra = {}) => ctx.mcp('emporium_write', { graphId: ctx.graphId, vocab: 'workspace', records, ...extra })
// A dedicated graph with the Gizmo ontology and n Gizmos (written 100 per call). Returns the subjects.
async function objectGraph(ctx, name, n) {
  await graphFor(ctx, name)
  await ctx.mcp('propose_domain_ontology', { graphId: ctx.graphId, vocab: 'workspace', classes: ONTOLOGY_CLASSES(), rationale: 'gbench' })
  const subjects = []
  for (let s = 0; s < n; s += 100) { const w = await W(ctx, gizmoRecords(Math.min(100, n - s), seed(ctx), s)); subjects.push(...w.results.map((r) => r.subject)) }
  return subjects
}
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const Q = (ctx, criteria, limit = 25) => ctx.mcp('emporium_query', { graphId: ctx.graphId, vocab: 'workspace', class: 'Gizmo', criteria, limit })

export const cases = [
  // Define/redefine a domain ontology: parse, derive SHACL, compile-check, store an immutable version. 5 classes; each call supersedes the last.
  mk('rich.emporium.propose_ontology', [[3, 1], [12, 2]], { async setup(ctx) { await graphFor(ctx, 'eo') },
    op: (ctx, i) => ctx.mcp('propose_domain_ontology', { graphId: ctx.graphId, vocab: 'workspace', classes: ONTOLOGY_CLASSES(3), title: `bench ontology ${i}`, rationale: 'gbench' }) }),
  // Typed-object writes (I1 gate, SHACL, minting, journal) at 10 and 100 records per call, onto a graph that already holds 500 objects.
  ...[10, 100].map((b) => mk(`rich.emporium.write.b${b}`, [[3, 1], [b === 10 ? 20 : 8, 2]], {
    async setup(ctx) { await objectGraph(ctx, 'ew' + b, 500) },
    op: (ctx, i) => W(ctx, gizmoRecords(b, seed(ctx), 10_000 + i * b)) })),
  // Same 10-record write onto an almost empty graph: the difference from write.b10 is the cost of the 500 objects already there.
  mk('rich.emporium.write.b10_empty', [[3, 1], [20, 2]], { async setup(ctx) { await objectGraph(ctx, 'ew0', 0) }, op: (ctx, i) => W(ctx, gizmoRecords(10, seed(ctx), 10_000 + i * 10)) }),
  // Object query faces at 10 vs 500 objects: a contains filter (a scan), an eq filter, and a hydrated page of 25.
  ...[10, 500].flatMap((n) => [
    mk(`rich.emporium.query_contains.n${n}`, [[5, 1], [30, 3]], { async setup(ctx) { await objectGraph(ctx, 'eq' + n, n) }, op: (ctx, i) => Q(ctx, { note: { contains: ['kalo', 'mi', 'tu', 'sel'][i % 4] } }) }),
    mk(`rich.emporium.query_eq.n${n}`, [[5, 1], [30, 3]], { async setup(ctx) { await objectGraph(ctx, 'ee' + n, n) }, op: (ctx, i) => Q(ctx, { weight: { eq: (i * 37) % 1000 } }) }),
  ]),
  mk('rich.emporium.read.n500', [[5, 1], [30, 3]], { async setup(ctx) { ctx.S = { subjects: await objectGraph(ctx, 'er', 500) } },
    op: (ctx, i) => ctx.mcp('emporium_read', { graphId: ctx.graphId, vocab: 'workspace', class: 'Gizmo', address: ctx.S.subjects[(i * 17) % ctx.S.subjects.length] }) }),
  mk('rich.emporium.list.n500', [[5, 1], [30, 3]], { async setup(ctx) { await objectGraph(ctx, 'el', 500) }, op: (ctx) => ctx.mcp('emporium_list', { graphId: ctx.graphId, vocab: 'workspace', class: 'Gizmo', limit: 100 }) }),
  mk('rich.emporium.named_countBy.n500', [[5, 1], [30, 3]], { async setup(ctx) { await objectGraph(ctx, 'en', 500) },
    op: (ctx) => ctx.mcp('sparql_query_named', { graphId: ctx.graphId, vocab: 'workspace', class: 'Gizmo', queryName: 'countBy', params: { attr: 'weight' } }) }),
  // Conformance sweep over the live memory projection: 50 vs 1,000 memory records. It mutates the violation ledger and scans the memory graph.
  ...[50, 1000].map((n) => mk(`rich.emporium.sweep.m${n}`, [[3, 1], [12, 2]], {
    async setup(ctx) { await graphFor(ctx, 'es' + n); for (let s = 0; s < n; s += 100) await ctx.mcp('remember_batch', { graphId: ctx.graphId, records: memoryRecords(Math.min(100, n - s), seed(ctx), s) }) },
    op: (ctx) => ctx.mcp('emporium_sweep', { graphId: ctx.graphId }) })),
  mk('rich.emporium.violations', [[5, 1], [30, 3]], { async setup(ctx) { await graphFor(ctx, 'ev'); await ctx.mcp('remember_batch', { graphId: ctx.graphId, records: memoryRecords(100, seed(ctx)) }); await ctx.mcp('emporium_sweep', { graphId: ctx.graphId }) },
    op: (ctx) => ctx.mcp('emporium_violations', { graphId: ctx.graphId, limit: 100 }) }),
  // Retraction is an event on a subject found by a store-wide scan, so its cost can grow with the store (500 objects + 500 memories).
  mk('rich.emporium.retract.n500', [[3, 1], [25, 3]], { async setup(ctx) { ctx.S = { subjects: await objectGraph(ctx, 'ex', 500) }; for (let s = 0; s < 500; s += 100) await ctx.mcp('remember_batch', { graphId: ctx.graphId, records: memoryRecords(100, seed(ctx), s) }) },
    op: (ctx, i) => ctx.mcp('emporium_retract', { graphId: ctx.graphId, subject: ctx.S.subjects[i], rationale: 'gbench' }) }),
]

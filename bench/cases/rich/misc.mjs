import { IT, SUITES, graphFor, stub } from './_rich.mjs'
import { mulberry32 } from '../../lib/stats.mjs'
import { markdownDoc } from '../../fixture/parts/rich-gen.mjs'

const seed = (ctx) => ctx.fixture.seed ?? 1
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
// A graph of n documents of `blocks` paragraphs each. Returns [{id, blockIds}].
async function docs(ctx, n, blocks) {
  const out = []
  for (let k = 0; k < n; k++) {
    const id = `rd-${ctx.tag}-${k}`
    await ctx.mcp('create_document', { graphId: ctx.graphId, title: `rich doc ${k}`, documentId: id })
    const p = await ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: id, content: markdownDoc(blocks, seed(ctx) + k).split('\n\n').slice(1).join('\n\n'), format: 'markdown' })
    out.push({ id, blockIds: p.block_ids })
  }
  return out
}
// n docs joined by a chain plus random wires (3 per doc).
async function wired(ctx, n) {
  const d = []; for (let k = 0; k < n; k++) { const id = `rw-${ctx.tag}-${k}`; await ctx.mcp('create_document', { graphId: ctx.graphId, title: `wired ${k}`, documentId: id }); d.push(id) }
  const rnd = mulberry32(seed(ctx) ^ 0x71), wires = []
  for (let k = 0; k < n * 3; k++) { const a = k < n - 1 ? k : Math.floor(rnd() * n), b = k < n - 1 ? k + 1 : Math.floor(rnd() * n); wires.push({ sourceDocumentId: d[a], targetDocumentId: d[b], predicate: k % 3 ? 'relatesTo' : 'cites' }) }
  for (let s = 0; s < wires.length; s += 25) await ctx.mcp('create_wires', { graphId: ctx.graphId, wires: wires.slice(s, s + 25) })
  return d
}

export const cases = [
  // Bounded BFS over the wire graph: 300 documents / 900 wires, depth 2 and depth 6, from a few start points.
  ...[2, 6].map((depth) => mk(`rich.wires.traverse.d${depth}`, [[3, 1], [20, 2]], { async setup(ctx) { await graphFor(ctx, 'wt' + depth); ctx.S = { d: await wired(ctx, 300) } },
    op: (ctx, i) => ctx.mcp('traverse_wires', { graphId: ctx.graphId, documentId: ctx.S.d[(i * 37) % 300], maxDepth: depth }) })),
  // Restore point diff between two points with 5 documents edited in between: the graph holds 20 vs 200 documents of 10 blocks.
  ...[20, 200].map((n) => mk(`rich.restore_point.diff.n${n}`, [[3, 1], [10, 2]], {
    async setup(ctx) { await graphFor(ctx, 'rp' + n); const d = await docs(ctx, n, 10); const a = await ctx.mcp('create_restore_point', { graphId: ctx.graphId, label: 'a' }); for (let k = 0; k < 5; k++) await ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: d[k].id, content: 'edited between restore points', format: 'plain' }); const b = await ctx.mcp('create_restore_point', { graphId: ctx.graphId, label: 'b' }); ctx.S = { a: a.restorePointId, b: b.restorePointId } },
    op: (ctx) => ctx.mcp('diff_restore_points', { graphId: ctx.graphId, restorePointId: ctx.S.a, against: ctx.S.b }) })),
  // Valuation: `value` over a batch of 100 blocks (importance/valence), and revaluate (rewrites the scoring config).
  mk('rich.values.value.b100', [[3, 1], [15, 2]], { async setup(ctx) { await graphFor(ctx, 'vv'); const d = await docs(ctx, 10, 10); ctx.S = { items: d.flatMap((x) => x.blockIds.slice(0, 10).map((b) => ({ documentId: x.id, blockId: b }))) } },
    op: (ctx, i) => ctx.mcp('value', { graphId: ctx.graphId, valuations: ctx.S.items.map((x, k) => ({ ...x, importance: (k + i) % 6, valence: ((k * 3 + i) % 11) - 5 })) }) }),
  mk('rich.values.revaluate', [[3, 1], [15, 2]], { async setup(ctx) { await graphFor(ctx, 'vr') }, op: (ctx, i) => ctx.mcp('revaluate', { graphId: ctx.graphId, importance_prompt: `Importance from 0 to 5, variant ${i}.` }) }),
  // The swarm store: six local-profile-only tools (swarm_load/query/save_query/query_named/semantic_search/traverse).
  stub('rich.swarm.load', 'STUB: the installed engine (019463f) predates the swarm store (b334717); it has no swarm_* tools. Design in CASES.md; needs a built engine and an N-Quads + vector package'),
  stub('rich.swarm.query', 'STUB: as rich.swarm.load'),
]

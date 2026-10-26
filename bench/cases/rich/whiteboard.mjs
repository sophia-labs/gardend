import { IT, SUITES, graphFor, parent } from './_rich.mjs'
import { boardBatches, smallEdit, flowBoard } from '../../fixture/parts/rich-gen.mjs'
import { slope } from '../../lib/stats.mjs'

const apply = (ctx, documentId, moves) => ctx.mcp('whiteboard_apply', { graphId: ctx.graphId, documentId, rationale: 'gbench', moves })
const create = async (ctx, title) => (await ctx.mcp('whiteboard_create', { graphId: ctx.graphId, title, ...parent(ctx) })).documentId
// A board of `shapes` shapes, built in 100-shape batches (about 150 moves each). Returns {id, shapeIds}.
async function buildBoard(ctx, title, shapes) {
  const id = await create(ctx, title)
  for (const moves of boardBatches(shapes, ctx.fixture.seed ?? 1)) await apply(ctx, id, moves)
  const r = await ctx.mcp('whiteboard_read', { graphId: ctx.graphId, documentId: id })
  return { id, shapeIds: r.shapes.map((s) => s.id), elementCount: r.elementCount }
}
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const sized = (shapes) => ({ async setup(ctx) { await graphFor(ctx, 'wb'); ctx.S = await buildBoard(ctx, `board ${shapes}`, shapes); ctx.log({ board: shapes, elements: ctx.S.elementCount }) } })

export const cases = [
  mk('rich.whiteboard.create', [[4, 1], [15, 2]], { async setup(ctx) { await graphFor(ctx, 'wbc') }, op: (ctx, i) => ctx.mcp('whiteboard_create', { graphId: ctx.graphId, title: `gbench board ${ctx.tag} ${i}`, ...parent(ctx) }) }),
  // One agent-sized edit (10 moves, explicit positions) on a 50- and a 1,000-shape board: the slope is the per-call cost of board size.
  mk('rich.whiteboard.apply_small.n50', [[5, 1], [25, 3]], { ...sized(50), op: (ctx, i) => apply(ctx, ctx.S.id, smallEdit(ctx.fixture.seed ?? 1, i, ctx.S.shapeIds)) }),
  mk('rich.whiteboard.apply_small.n1000', [[5, 1], [25, 3]], { ...sized(1000), op: (ctx, i) => apply(ctx, ctx.S.id, smallEdit(ctx.fixture.seed ?? 1, i, ctx.S.shapeIds)) }),
  // The default agent path: adds with no position, placed in free space (a scan of the existing shapes) on a 1,000-shape board.
  mk('rich.whiteboard.apply_free.n1000', [[5, 1], [25, 3]], { ...sized(1000), op: (ctx, i) => apply(ctx, ctx.S.id, smallEdit(ctx.fixture.seed ?? 1, i, ctx.S.shapeIds, { free: true })) }),
  // The tool's maximum: about 195 moves (130 shapes, 65 arrows) onto an empty board. A fresh board per iteration, made in setup.
  mk('rich.whiteboard.apply_batch195', [[3, 1], [12, 2]], {
    async setup(ctx) { await graphFor(ctx, 'wbb'); ctx.S = { boards: [] }; for (let k = 0; k < ctx.params.iterations + ctx.params.warmup; k++) ctx.S.boards.push(await create(ctx, `batch ${k}`)) },
    op: (ctx, i) => apply(ctx, ctx.S.boards[i], boardBatches(130, ctx.fixture.seed ?? 1, { perBatch: 130 })[0]) }),
  // Growth: one board, each iteration adds 100 shapes (150 moves), 0 -> 1,500 shapes. The slope is ms of extra cost per 100 shapes already there.
  mk('rich.whiteboard.apply_growth', [[4, 0], [15, 0]], {
    async setup(ctx) { await graphFor(ctx, 'wbg'); ctx.S = { id: await create(ctx, 'growth') } },
    op: (ctx, i) => apply(ctx, ctx.S.id, boardBatches(100, ctx.fixture.seed ?? 1, { offset: i * 100 })[0]),
    finish: (ctx, s) => ({ slope: { msPerHundredShapes: Math.round(slope(s) * 100) / 100 } }) }),
  mk('rich.whiteboard.read.n50', [[5, 1], [30, 3]], { ...sized(50), op: (ctx) => ctx.mcp('whiteboard_read', { graphId: ctx.graphId, documentId: ctx.S.id }) }),
  mk('rich.whiteboard.read.n1000', [[5, 1], [30, 3]], { ...sized(1000), op: (ctx) => ctx.mcp('whiteboard_read', { graphId: ctx.graphId, documentId: ctx.S.id }) }),
  mk('rich.whiteboard.read_elements.n1000', [[5, 1], [30, 3]], { ...sized(1000), op: (ctx) => ctx.mcp('whiteboard_read', { graphId: ctx.graphId, documentId: ctx.S.id, format: 'elements' }) }),
  // flow_seed_board: whole-board seed (replace) of a Mithras Flow board, 20 vs 400 systems (2 requirements each).
  ...[20, 400].map((n) => mk(`rich.whiteboard.flow_seed.n${n}`, [[3, 1], [10, 2]], {
    async setup(ctx) { await graphFor(ctx, 'flow' + n); ctx.S = { json: JSON.stringify(flowBoard(n, ctx.fixture.seed ?? 1)) } },
    op: (ctx) => ctx.mcp('flow_seed_board', { graphId: ctx.graphId, json: ctx.S.json, replace: true }) })),
]

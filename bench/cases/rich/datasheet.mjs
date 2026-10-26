import { IT, SUITES, graphFor } from './_rich.mjs'
import { datasheetEnvelope } from '../../fixture/parts/rich-gen.mjs'
const seed = (ctx) => ctx.fixture.seed ?? 1
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
export const cases = [
  // Import a datasheet sidecar (autonotes.data/2): one assertion node per statement, typed literals, admission checks, a replace-whole named graph.
  ...[20, 400].map((rows) => mk(`rich.datasheet.import.r${rows}`, [[3, 1], [rows > 100 ? 8 : 15, 2]], { async setup(ctx) { await graphFor(ctx, 'dsi' + rows); ctx.S = { env: datasheetEnvelope(rows, seed(ctx)) } },
    op: (ctx) => ctx.mcp('datasheet_import', { graphId: ctx.graphId, envelope: ctx.S.env }) })),
  mk('rich.datasheet.facts.r400', [[5, 1], [30, 3]], { async setup(ctx) { await graphFor(ctx, 'dsf'); ctx.S = { env: datasheetEnvelope(400, seed(ctx)) }; await ctx.mcp('datasheet_import', { graphId: ctx.graphId, envelope: ctx.S.env }) },
    op: (ctx, i) => ctx.mcp('datasheet_facts', { graphId: ctx.graphId, part: 'XBN1000', parameter: ['vin', 'vout', 'iout', 'fsw'][i % 4] }) }),
]

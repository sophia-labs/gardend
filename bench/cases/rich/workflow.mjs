import { IT, SUITES, graphFor, stub, parent } from './_rich.mjs'
import { planMoves, runTurtle, markdownDoc } from '../../fixture/parts/rich-gen.mjs'

const seed = (ctx) => ctx.fixture.seed ?? 1
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const newPlan = async (ctx, title) => (await ctx.mcp('workflow_plan_create', { graphId: ctx.graphId, title, ...parent(ctx) })).documentId
const compose = (ctx, documentId, moves) => ctx.mcp('workflow_plan_compose', { graphId: ctx.graphId, documentId, rationale: 'gbench', moves })
const lastCollection = (phases, parallel = 3) => `c${phases}x0`
// A source document of n paragraphs, a composed plan that targets it, and a recorded run with n items that quote the paragraphs.
async function publishable(ctx, n) {
  await graphFor(ctx, 'wfp' + n)
  const text = markdownDoc(n, seed(ctx)).split('\n\n').slice(1).filter((p) => !p.startsWith('##'))
  const src = `wfsrc-${ctx.tag}-${n}`; await ctx.mcp('create_document', { graphId: ctx.graphId, title: 'wf source', documentId: src, ...parent(ctx) })
  for (let s = 0; s < text.length; s += 100) await ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: src, content: text.slice(s, s + 100).join('\n\n'), format: 'markdown' })
  const plan = await newPlan(ctx, 'publish plan')
  await compose(ctx, plan, planMoves(src, { phases: 2, parallel: 2 }, seed(ctx)))
  const quotes = text.map((p) => p.split(' ').slice(0, 6).join(' '))
  return { src, plan, quotes, collection: lastCollection(2) }
}
const record = (ctx, S, runId, quotes) => {
  const workflow = `urn:sophia:wf:${S.plan}`, runGraph = `${workflow}:run:${runId}`
  return ctx.mcp('workflow_run_record', { graphId: ctx.graphId, documentId: S.plan, runId, turtle: runTurtle({ workflow, runGraph, runId, collection: S.collection, quotes, documentId: S.src }) })
}

export const cases = [
  mk('rich.workflow.plan_create', [[3, 1], [10, 2]], { async setup(ctx) { await graphFor(ctx, 'wfc') }, op: (ctx, i) => newPlan(ctx, `gbench plan ${i}`) }),
  // Compose a 3-phase, 9-step plan (about 16 moves) / the 12-step maximum over 6 phases; a fresh plan document per iteration made in setup.
  ...[[3, 3, 'p3x3'], [6, 2, 'p6x2']].map(([phases, parallel, nm]) => mk(`rich.workflow.plan_compose.${nm}`, [[3, 1], [10, 2]], {
    async setup(ctx) { await graphFor(ctx, 'wfm' + nm); const t = ctx.params.iterations + ctx.params.warmup; ctx.S = { docs: [], src: `wfsrc-${ctx.tag}` }; await ctx.mcp('create_document', { graphId: ctx.graphId, title: 'src', documentId: ctx.S.src }); for (let k = 0; k < t; k++) ctx.S.docs.push(await newPlan(ctx, `c${k}`)) },
    op: (ctx, i) => compose(ctx, ctx.S.docs[i], planMoves(ctx.S.src, { phases, parallel }, seed(ctx))) })),
  mk('rich.workflow.plan_read', [[5, 1], [30, 3]], { async setup(ctx) { await graphFor(ctx, 'wfr'); const src = `wfsrc-${ctx.tag}`; await ctx.mcp('create_document', { graphId: ctx.graphId, title: 'src', documentId: src }); ctx.S = { plan: await newPlan(ctx, 'read'), }; await compose(ctx, ctx.S.plan, planMoves(src, { phases: 3, parallel: 3 }, seed(ctx))) },
    op: (ctx) => ctx.mcp('workflow_plan_read', { graphId: ctx.graphId, documentId: ctx.S.plan }) }),
  // Record a finished run (Turtle parse + one transaction replacing the run graph) with 50 vs 1,000 items.
  ...[50, 1000].map((n) => mk(`rich.workflow.run_record.i${n}`, [[3, 1], [10, 2]], {
    async setup(ctx) { ctx.S = await publishable(ctx, Math.min(n, 300)); ctx.S.q = Array.from({ length: n }, (_, k) => ctx.S.quotes[k % ctx.S.quotes.length]) },
    op: (ctx, i) => record(ctx, ctx.S, 'bench-run', ctx.S.q) })),
  // Publish a run's items as margin comments (anchoring each quote in its document): 50 vs 300 items. Re-publishing updates, not doubles.
  ...[50, 300].map((n) => mk(`rich.workflow.plan_publish.i${n}`, [[2, 1], [8, 1]], {
    async setup(ctx) { ctx.S = await publishable(ctx, n); await record(ctx, ctx.S, 'bench-pub', ctx.S.quotes) },
    op: (ctx) => ctx.mcp('workflow_plan_publish', { graphId: ctx.graphId, documentId: ctx.S.plan, runId: 'bench-pub' }) })),
  stub('rich.workflow.book', 'STUB: workflow_book_* and workflow_run_start need an authored workflow script block and a Choreograph runner; see CASES.md'),
]

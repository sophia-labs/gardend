// Shared helpers for the rich.* cases.
import { newTrace } from '../../lib/mcp.mjs'
export const IT = (smoke, std, conc = 1) => ({ iterations: std[0], warmup: std[1], concurrency: conc, smoke: { iterations: smoke[0], warmup: smoke[1] } })
export const SUITES = ['standard', 'rich']
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const parent = (ctx) => (ctx.fixture.runFolder ? { parentId: ctx.fixture.runFolder } : {})
export { parent }
// A dedicated graph per case family in spawn mode (so sizes are controlled); in attach mode the bench graph and its run folder.
export async function graphFor(ctx, name) {
  if (ctx.mode === 'spawn') {
    const id = `rich-${name}-${ctx.tag}`.toLowerCase().replace(/[^a-z0-9-]/g, '-').slice(0, 60)
    const g = await ctx.http('POST', '/api/graphs', { title: id, graph_id: id })
    if (g.status >= 300 && !/exist|already/i.test(g.text)) throw new Error(`create graph ${id}: ${g.status} ${g.text.slice(0, 120)}`)
    ctx.graphId = id
  }
  return ctx.graphId
}
export async function incarnation(ctx) {
  const r = await ctx.http('GET', '/api/graphs')
  const list = r.json?.graphs ?? r.json ?? []
  const g = list.find((x) => x.graph_id === ctx.graphId)
  if (!g?.graph_incarnation) throw new Error('graph_incarnation not found for ' + ctx.graphId)
  return g.graph_incarnation
}
// Wait for an async job (upload_artifact, ingest_artifact, refresh_semantic_index, ...). Returns the job result.
export async function waitJob(ctx, submitted, { timeoutMs = 600_000, pollMs = 25 } = {}) {
  const id = submitted.job_id ?? submitted.jobId
  if (!id) return submitted // not a job
  const t0 = Date.now()
  for (;;) {
    const s = await ctx.mcp('get_job_status', { job_id: id })
    if (s.status === 'succeeded' || s.status === 'completed') return ctx.mcp('get_job_result', { job_id: id })
    if (s.status === 'failed' || s.status === 'cancelled') { let m = ''; try { await ctx.mcp('get_job_result', { job_id: id }) } catch (e) { m = e.message }; throw new Error(`job ${id} ${s.status}: ${m}`.slice(0, 300)) }
    if (Date.now() - t0 > timeoutMs) throw new Error(`job ${id} timed out`)
    await sleep(pollMs)
  }
}
// A custom run() that times `fn` itself; used by cases that may skip.
export async function timedRun(ctx, params, fn) {
  const calls = []
  for (let i = 0; i < params.iterations + params.warmup; i++) {
    const trace = newTrace(); ctx._trace = trace; const t0 = performance.now(); let ok = true, error = null
    try { await fn(ctx, i) } catch (e) { ok = false; error = String(e.message).slice(0, 200) }
    if (i >= params.warmup) calls.push({ traceId: trace.traceId, ms: performance.now() - t0, ok, error })
  }
  return { calls }
}
export const stub = (id, why, kind = 'rich', suites = SUITES) => ({ id, version: 1, kind, suites, params: IT([1, 0], [1, 0]), needs: [], run: async () => ({ skipped: why }) })
export const SIZES = { small: 50, large: 1000 }
// Per-iteration untimed preparation, then the timed call: prep(ctx,i) is not measured; fn(ctx,i) is.
export async function phasedRun(ctx, params, prep, fn) {
  const calls = []
  for (let i = 0; i < params.iterations + params.warmup; i++) {
    ctx._trace = null; await prep(ctx, i)
    const trace = newTrace(); ctx._trace = trace; const t0 = performance.now(); let ok = true, error = null
    try { await fn(ctx, i) } catch (e) { ok = false; error = String(e.message).slice(0, 200) }
    if (i >= params.warmup) calls.push({ traceId: trace.traceId, ms: performance.now() - t0, ok, error })
  }
  return { calls }
}

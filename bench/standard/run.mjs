// `gbench standard run <bsbm-explore|bsbm-explore-update|sp2b|watdiv> --endpoint <query url> [--update <url>] [--auth ..]
//    [--clients 1|16] [--warmup N] [--runs N] [--timeout s] --label <l> [--scale N] [--store <name>] [--out dir]`
// Writes <out>/run.json (schema sophia.bench.run.v1, kind "standard") + summary.md. Plain SPARQL Protocol over HTTP:
// POST application/sparql-query, Accept application/sparql-results+json (updates: POST application/sparql-update).
// Same timeout everywhere; a failed or timed-out execution is CHARGED at 2x the timeout in every statistic (Lam et al., ESWC 2023);
// the raw count of errors/timeouts is reported next to it. A qualification pass records every distinct query's result count first.
import { readFileSync, writeFileSync, existsSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { createInterface } from 'node:readline'
import { CACHE, readJson, writeJson, mkdirp, log, authHeader, dataDir, envBlock, geomean, sleep } from './lib/common.mjs'
import { withMachine, measured } from './lib/machine.mjs'
import { Endpoint } from './lib/sparql.mjs'
import { readStore } from './store.mjs'
import { summarize } from '../lib/stats.mjs'

export const BENCHES = {
  'bsbm-explore': { data: 'bsbm', caseBase: 'std.bsbm.explore', stream: 'explore' },
  'bsbm-explore-update': { data: 'bsbm', caseBase: 'std.bsbm.explore-update', stream: 'explore-update' },
  sp2b: { data: 'sp2b', caseBase: 'std.sp2b' },
  watdiv: { data: 'watdiv', caseBase: 'std.watdiv' },
}
const DEFAULT_SCALE = { bsbm: 1000, sp2b: 10000000, watdiv: 100 }
const CASE_VERSION = 1

function readStream(file) { return readFileSync(file, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) }

function loadWorkload(bench, dir, o) {
  const spec = BENCHES[bench]
  if (spec.stream) return { type: 'mixes', mixes: readStream(path.join(dir, 'streams', `${spec.stream}.jsonl`)) }
  if (bench === 'sp2b') {
    const qd = path.join(dir, 'queries'), names = ['q1', 'q2', 'q3a', 'q3b', 'q3c', 'q4', 'q5a', 'q5b', 'q6', 'q7', 'q8', 'q9', 'q10', 'q11', 'q12a', 'q12b', 'q12c']
    // q12c is ASK-style in the distribution; all are plain SPARQL 1.0 text
    return { type: 'list', queries: names.filter((n) => existsSync(path.join(qd, `${n}.sparql`))).map((n) => ({ id: n, kind: 'select', text: readFileSync(path.join(qd, `${n}.sparql`), 'utf8') })) }
  }
  const qs = readJson(path.join(dir, 'queries.json')), perTemplate = Number(o['per-template'] ?? 5)
  const queries = []
  for (const t of Object.keys(qs).sort((a, b) => a[0].localeCompare(b[0]) || Number(a.slice(1)) - Number(b.slice(1)))) for (const text of qs[t].slice(0, perTemplate)) queries.push({ id: t, kind: 'select', text })
  return { type: 'list', queries }
}

const exec1 = (ep, q, timeoutMs) => (q.kind === 'update' ? ep.update(q.text, { timeoutMs }) : ep.select(q.text, { timeoutMs }))

// One execution record: charged ms (2x timeout when failed), raw ms, outcome.
async function execOne(ep, q, timeoutMs) {
  const r = await exec1(ep, q, timeoutMs), ok = !!r.ok
  return { id: q.id, ok, timedOut: !!r.timedOut, rows: r.rows ?? null, rawMs: r.ms, chargedMs: ok ? r.ms : 2 * timeoutMs, err: ok ? undefined : r.error }
}

async function runMixes({ ep, mixes, clients, warmup, runs, timeoutMs }) {
  const warm = mixes.filter((m) => m.warmup), meas = mixes.filter((m) => !m.warmup)
  const doPhase = async (list, count, label) => {
    const take = list.slice(0, count), recs = [], mixTimes = []
    let next = 0
    const t0 = performance.now()
    const worker = async () => {
      for (;;) {
        const i = next++; if (i >= take.length) return
        let sum = 0
        for (const q of take[i].queries) { const r = await execOne(ep, q, timeoutMs); recs.push(r); sum += r.chargedMs }
        mixTimes.push(sum)
        if (take.length >= 20 && (i + 1) % Math.max(1, Math.floor(take.length / 5)) === 0) log(`${label}: ${i + 1}/${take.length} mixes`)
      }
    }
    await Promise.all(Array.from({ length: clients }, worker))
    return { recs, mixTimes, wallS: (performance.now() - t0) / 1000, mixes: take.length }
  }
  if (warm.length && warmup > 0) { log(`warm-up: ${Math.min(warmup, warm.length)} mixes`); await doPhase(warm, warmup, 'warm-up') }
  if (meas.length < runs) log(`stream has only ${meas.length} measured mixes (asked ${runs}); using all`)
  return doPhase(meas, runs, 'measured')
}

async function runList({ ep, queries, clients, warmup, runs, timeoutMs }) {
  // each pass executes every query once; `clients` workers share a pass. warmup passes discarded.
  const passes = [], doPass = async () => {
    const recs = new Array(queries.length); let next = 0; const t0 = performance.now()
    await Promise.all(Array.from({ length: clients }, async () => { for (;;) { const i = next++; if (i >= queries.length) return; recs[i] = await execOne(ep, queries[i], timeoutMs) } }))
    return { recs, wallS: (performance.now() - t0) / 1000 }
  }
  for (let i = 0; i < warmup; i++) { log(`warm-up pass ${i + 1}/${warmup}`); await doPass() }
  for (let i = 0; i < runs; i++) { log(`measured pass ${i + 1}/${runs}`); passes.push(await doPass()) }
  return passes
}

async function qualify(ep, workload, timeoutMs) {
  const seen = new Map(), items = workload.type === 'mixes' ? workload.mixes.flatMap((m) => m.queries) : workload.queries
  // BSBM: first occurrence of each template (updates excluded: they are not idempotent); lists: every query
  for (const q of items) {
    if (q.kind === 'update') continue
    const key = workload.type === 'mixes' ? q.id : `${q.id}#${seen.size}`
    if (workload.type === 'mixes' && seen.has(key)) continue
    if (seen.size > 400) break
    const r = await execOne(ep, q, timeoutMs)
    seen.set(key, { id: q.id, rows: r.rows, ok: r.ok, err: r.err, ms: Math.round(r.rawMs) })
  }
  return Object.fromEntries(seen)
}

function caseOf(caseId, samples, errors, timeouts, params, extra = {}) {
  return { caseId, caseVersion: CASE_VERSION, kind: 'standard', params, n: samples.length, errors, timeouts, ms: summarize(samples), samples: samples.map((x) => Math.round(x * 100) / 100), ...extra }
}

export async function runCmd(pos, o) {
  const bench = pos[0], spec = BENCHES[bench]
  if (!spec) throw new Error(`run <${Object.keys(BENCHES).join('|')}> --endpoint ...`)
  if (!o.endpoint) throw new Error('--endpoint <query url> required')
  const scale = Number(o.scale ?? DEFAULT_SCALE[spec.data]), dir = o.data ?? dataDir(spec.data, scale)
  const manifest = readJson(path.join(dir, 'manifest.json')); if (!manifest) throw new Error(`no manifest in ${dir}`)
  const clients = Number(o.clients ?? 1), timeoutS = Number(o.timeout ?? 60), timeoutMs = timeoutS * 1000
  const warmup = Number(o.warmup ?? (spec.stream ? 50 : 1)), runs = Number(o.runs ?? (spec.stream ? 100 : 3))
  const label = String(o.label ?? 'unlabeled').replace(/[^a-z0-9-]/gi, '-').toLowerCase()
  const store = o.store ? readStore(o.store) : null
  const ep = new Endpoint({ query: o.endpoint, update: o.update, headers: authHeader(o), timeoutMs })
  const startedAt = new Date(), runId = `${startedAt.toISOString().replace(/[-:]/g, '').replace(/\.\d+/, '')}-${label}`
  const runDir = mkdirp(o.out ?? path.join(process.env.GBENCH_CACHE ?? path.join(os.homedir(), '.cache', 'sophia-bench'), 'runs', runId))
  const workload = loadWorkload(bench, dir, o)
  if (bench === 'bsbm-explore-update' && !o.update) throw new Error('--update <url> required for bsbm-explore-update')
  const warm = await ep.select('ASK {}'); if (!warm.ok) throw new Error(`endpoint not answering: ${warm.error}`)

  return withMachine(`run-${bench}-${label}`, async ({ awake } = {}) => {
    const notes = [], cases = []
    log(`${bench} scale ${scale} clients ${clients} warmup ${warmup} runs ${runs} timeout ${timeoutS}s -> ${runDir}`)
    log('qualification pass')
    const qualification = await qualify(ep, workload, timeoutMs)
    const qfail = Object.values(qualification).filter((x) => !x.ok)
    if (qfail.length) notes.push(`qualification: ${qfail.length} distinct queries failed (${[...new Set(qfail.map((x) => x.id))].join(',')}): ${qfail[0].err}`)

    const { result, machine } = await measured(async () => {
      if (workload.type === 'mixes') return runMixes({ ep, mixes: workload.mixes, clients, warmup, runs, timeoutMs })
      return runList({ ep, queries: workload.queries, clients, warmup, runs, timeoutMs })
    }, { awake })
    if (machine.health.clean === false) notes.push(`NOT CLEAN, not a judge: ${machine.health.why.join('; ')}`)

    const cid = (s) => `${spec.caseBase}${clients === 1 ? '' : `.c${clients}`}.${s}`
    const params = { clients, warmup, runs, timeoutS, scale, charge: 'errors and timeouts at 2x timeout' }
    let whole
    if (workload.type === 'mixes') {
      const byId = new Map()
      for (const r of result.recs) { if (!byId.has(r.id)) byId.set(r.id, []); byId.get(r.id).push(r) }
      const ids = [...byId.keys()].sort((a, b) => (a[0] === b[0] ? Number(a.slice(1)) - Number(b.slice(1)) : a[0].localeCompare(b[0])))
      for (const id of ids) { const rs = byId.get(id); cases.push(caseOf(cid(id), rs.map((r) => r.chargedMs), rs.filter((r) => !r.ok).length, rs.filter((r) => r.timedOut).length, params, { rawMsP50: summarize(rs.filter((r) => r.ok).map((r) => r.rawMs)).p50 })) }
      const failed = result.recs.filter((r) => !r.ok), extraCharge = result.recs.reduce((a, r) => a + (r.chargedMs - r.rawMs), 0) / 1000
      const qmph = (result.mixes * 3600) / result.wallS, qmphCharged = (result.mixes * 3600) / (result.wallS + extraCharge / clients)
      whole = caseOf(cid('mix'), result.mixTimes, failed.length, failed.filter((r) => r.timedOut).length, params, { metrics: { qmph: Math.round(qmph * 10) / 10, qmphCharged: Math.round(qmphCharged * 10) / 10, mixes: result.mixes, wallS: Math.round(result.wallS * 100) / 100, geomeanMixMs: Math.round(geomean(result.mixTimes) * 100) / 100 } })
    } else {
      const ids = [...new Set(workload.queries.map((q) => q.id))]
      for (const id of ids) {
        const rs = result.flatMap((p) => p.recs.filter((r) => r.id === id))
        cases.push(caseOf(cid(id), rs.map((r) => r.chargedMs), rs.filter((r) => !r.ok).length, rs.filter((r) => r.timedOut).length, params, { rawMsP50: summarize(rs.filter((r) => r.ok).map((r) => r.rawMs)).p50 }))
      }
      const all = result.flatMap((p) => p.recs), perPass = result.map((p) => p.recs.reduce((a, r) => a + r.chargedMs, 0))
      const perQueryMeans = cases.map((c) => c.ms.mean)
      whole = caseOf(cid('all'), perPass, all.filter((r) => !r.ok).length, all.filter((r) => r.timedOut).length, params, { metrics: { geomeanOfQueryMeanMs: Math.round(geomean(perQueryMeans) * 100) / 100, passes: result.length, wallSMean: Math.round((result.reduce((a, p) => a + p.wallS, 0) / result.length) * 100) / 100 } })
    }
    cases.push(whole)

    const finishedAt = new Date(), env = { ...(await envBlock()), contention: undefined, machine, loadAfter: os.loadavg().map((x) => Math.round(x * 100) / 100) }
    const loadRec = o.load ? readJson(o.load) : (store ? readJson(path.join(CACHE, 'stores', `${o.store}.load-${path.basename(dir)}.json`)) : null)
    const run = {
      schema: 'sophia.bench.run.v1', runId, label, protocolVersion: 1, startedAt: startedAt.toISOString(), finishedAt: finishedAt.toISOString(),
      mode: 'attach', suite: 'standard', kind: 'standard', benchmark: bench, tier: String(scale), seed: workload.type === 'mixes' ? manifest.streams?.[spec.stream]?.seed ?? null : null,
      target: {
        binary: null, binarySha256: null, name: store?.name ?? o['target-name'] ?? 'unknown', version: o['target-version'] ?? store?.image ?? 'unknown', gitRev: null,
        imageDigest: store?.digest ?? null, url: o.endpoint, graphId: 'default-graph', spans: false,
        settings: store ? { cpus: store.cpus, memory: store.memory, reasoning: store.reasoning, defaultGraph: store.defaultGraph } : (o.settings ? JSON.parse(o.settings) : null),
      },
      env, dataset: { benchmark: manifest.benchmark, scale: manifest.scale, triples: manifest.dataset?.triples, sha256: manifest.dataset?.sha256, generator: manifest.generator },
      load: loadRec ? { kind: loadRec.kind, loader: loadRec.loader ?? loadRec.method, triplesPerSec: loadRec.triplesPerSec, seconds: loadRec.seconds, storedTriples: loadRec.storedTriples } : null,
      qualification, cases, process: null, notes,
    }
    writeJson(path.join(runDir, 'run.json'), run); writeFileSync(path.join(runDir, 'summary.md'), summaryMd(run))
    console.log(summaryMd(run)); console.log(`run written to ${runDir}`)
    return { run, runDir }
  })
}

export function summaryMd(run) {
  const w = run.cases.find((c) => /\.(mix|all)$/.test(c.caseId)), L = [`# ${run.runId}`, '', `${run.benchmark} scale ${run.tier} on ${run.target.name} (${run.target.version}), clients ${w?.params.clients}, health: ${run.env.machine.health.clean ? 'clean' : 'NOT CLEAN: ' + run.env.machine.health.why.join('; ')}`, '']
  if (w?.metrics) L.push('metrics: ' + JSON.stringify(w.metrics), '')
  if (run.load) L.push(`load: ${run.load.kind} ${run.load.loader ?? ''} ${run.load.triplesPerSec} triples/s in ${run.load.seconds} s`, '')
  L.push('| case | n | err | timeouts | p50 ms | p95 ms | mean ms |', '|---|---|---|---|---|---|---|')
  for (const c of run.cases) L.push(`| ${c.caseId} | ${c.n} | ${c.errors} | ${c.timeouts} | ${c.ms.p50} | ${c.ms.p95} | ${c.ms.mean} |`)
  if (run.notes.length) L.push('', ...run.notes.map((n) => `- ${n}`))
  return L.join('\n') + '\n'
}

// `gbench standard qualdiff <runA> <runB>`: result-count differences between two stores (findings, not speed).
export function qualdiffCmd(pos) {
  const A = readJson(path.join(pos[0], 'run.json')), B = readJson(path.join(pos[1], 'run.json')), diffs = []
  for (const k of Object.keys(A.qualification)) { const a = A.qualification[k], b = B.qualification[k]; if (b && (a.rows !== b.rows || a.ok !== b.ok)) diffs.push({ key: k, a: a.ok ? a.rows : `ERR ${a.err}`, b: b.ok ? b.rows : `ERR ${b.err}` }) }
  console.log(diffs.length ? JSON.stringify(diffs, null, 1) : 'qualification counts identical')
}

import { mkdirSync, writeFileSync, readFileSync, createWriteStream, rmSync, readdirSync, existsSync } from 'node:fs'
import { createGzip } from 'node:zlib'
import { pipeline } from 'node:stream/promises'
import { createReadStream } from 'node:fs'
import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { randomBytes } from 'node:crypto'
import { Client, readKey, newTrace } from './mcp.mjs'
import { Gardend, freePort, binaryInfo, fingerprint, loadavg, sleep, DEFAULT_BIN } from './proc.mjs'
import { ensureFixture, cloneProfile, seedGraph, lexicon, PROTOCOL_VERSION, CACHE_ROOT } from './fixture.mjs'
import { summarize, mulberry32 } from './stats.mjs'
const CASE_VERSION = 1
const CASES_DIR = new URL('../cases/', import.meta.url).pathname
import { pathToFileURL } from 'node:url'
import { takeMeasure, registerDev } from './lock.mjs'
import { loadSpans, serverFacts } from './spans.mjs'
import { summaryMd } from './report.mjs'

const utcStamp = () => new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z')
const r1 = (x) => Math.round(x * 10) / 10

function caseResult({ caseId, kind, caseVersion = CASE_VERSION, params, calls, extra }) {
  const ok = calls.filter((c) => c.ok), samples = ok.map((c) => r1(c.ms))
  const traceIds = ok.map((c) => c.traceId ?? null)
  return { caseId, caseVersion, kind, params, n: samples.length, errors: calls.length - ok.length, ms: summarize(samples), samples, traceIds, server: null, exemplars: [], ...(extra ?? {}), _calls: calls }
}

export async function runBench(o) {
  const lg = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a)
  let release, contention = null
  if (o.measured) { const t = await takeMeasure({ lane: 'core', label: o.label ?? '', log: lg }); release = t.release; contention = t.contention }
  else release = await registerDev({ lane: 'core', label: o.label ?? '', log: lg })
  try { return await runBenchInner({ ...o, _contention: contention }) } finally { release() }
}
async function runBenchInner(o) {
  const log = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a)
  const suite = o.suite ?? 'smoke', tier = o.tier ?? 's', seed = Number(o.seed ?? 1), label = (o.label ?? 'run').replace(/[^a-z0-9-]/gi, '-').toLowerCase()
  const startedAt = new Date(), runId = `${utcStamp()}-${label}`, tag = randomBytes(3).toString('hex')
  const runDir = path.resolve(o.out ?? 'runs', runId); mkdirSync(runDir, { recursive: true })
  const attach = !!o.attach, notes = []
  const only = o.cases ? new RegExp(o.cases) : null
  const want = (id) => !only || only.test(id)
  const results = []
  let gard = null, bin = null, binInfo = null, fixture = null, manifest = null, spanFiles = [], pids = [], client, G, url, cpuU = 0, cpuS = 0, rssPeak = 0, rssStart = 0, rssEnd = 0, sampler = null
  const clone = path.join(CACHE_ROOT, 'runs', runId, 'profile')

  let failure = null
  const fp0 = fingerprint([])
  let startN = 0
  const startGardend = async () => {
    const spansFile = path.join(runDir, `spans-${++startN}.jsonl`); spanFiles.push(spansFile)
    const g = new Gardend({ bin, profile: clone, port: gard?.port ?? await freePort(7790), spansFile, logFile: path.join(runDir, 'gardend.log') })
    const pid = g.start(); pids.push(pid); return g
  }
  const sampleRss = () => { if (!gard?.child || gard.child.exitCode !== null) return; const kb = gard.rssKB(); if (kb) { rssPeak = Math.max(rssPeak, kb); rssEnd = kb; if (!rssStart) rssStart = kb } }

  try {
    if (attach) {
      url = o.attach; client = new Client({ url, key: readKey(o['key-file']) }); G = 'bench'
      if (!(await client.health())) throw new Error(`no gardend at ${url}`)
      notes.push('attach mode: measured a running instance; nothing deleted; boot and span cases not available')
      const folder = `gbench-run-${tag}`
      const g = await client.json('POST', '/api/graphs', { title: 'bench', graph_id: 'bench' }); if (g.status >= 300 && !/exist|conflict|already/i.test(g.text)) log('create graph:', g.status, client.redact(g.text.slice(0, 120)))
      const fr = await client.call('create_folder', { graphId: G, name: `gbench run ${runId}`, folderId: folder }); if (!fr.ok) throw new Error('run folder: ' + fr.error)
      log(`seeding tier ${tier} into graph bench / folder ${folder}`)
      manifest = await seedGraph(client, { graphId: G, tier, seed, prefix: `gb${tag}`, parentFolder: folder, createGraph: false, log })
      manifest.runFolder = folder
      binInfo = { binary: null, binarySha256: null, version: (await client.json('GET', '/health')).json?.status ? 'attached instance' : 'unknown', gitRev: 'unknown' }
    } else {
      bin = o.bin ?? DEFAULT_BIN; binInfo = binaryInfo(bin)
      fixture = await ensureFixture({ bin, tier, seed, log })
      manifest = fixture.manifest; G = manifest.graphId
      log(`cloning fixture ${fixture.dir}`)
      cloneProfile(fixture.profile, clone)
    }
    const lex = lexicon(seed)
    const base = { mode: attach ? 'attach' : 'spawn', graphId: G, tag, lex, fixture: manifest, runDir, shared: {}, rng: mulberry32(seed ^ 0xabcdef), log: (o) => log(JSON.stringify(o)),
      get url() { return client.url }, get key() { return client.key }, get c() { return client } }
    base.proc = attach ? null : {
      start: async () => { gard = await startGardend(); client = gard.client(); return gard },
      stop: async () => { if (gard?.child && gard.child.exitCode === null) { const c = gard.cpu(); cpuU += c.user; cpuS += c.sys; await gard.stop() } },
    }
    const mkCtx = (params, shared) => {
      const ctx = Object.create(base); ctx.params = params; ctx.shared = shared; ctx.graphId = G; ctx._trace = null
      ctx.mcp = async function (tool, args = {}) { const r = await client.call(tool, args, { trace: this._trace ?? newTrace() }); if (!r.ok) throw new Error(`${tool}: ${r.error}`); return r.payload }
      ctx.http = function (m, p, b) { return client.http(m, p, b, { trace: this._trace ?? newTrace() }).then((r) => { let j = null; try { j = JSON.parse(r.text) } catch {} return { ...r, json: j } }) }
      ctx.fork = () => { const c = Object.create(ctx); c._trace = null; c.mcp = async (tool, args = {}) => { const r = await client.call(tool, args, { trace: newTrace() }); if (!r.ok) throw new Error(`${tool}: ${r.error}`); return r.payload }; return c }
      return ctx
    }
    // ---- load case modules
    const mods = []
    const walk = (dir) => { for (const e of readdirSync(dir, { withFileTypes: true })) { const f = path.join(dir, e.name); if (e.isDirectory()) walk(f); else if (e.name.endsWith('.mjs') && !e.name.startsWith('_')) mods.push(f) } }
    walk(CASES_DIR)
    let defs = []
    for (const f of mods.sort()) { const m = await import(pathToFileURL(f).href); for (const c of m.cases ?? []) defs.push(c) }
    const ORDER = ['boot', 'read', 'write', 'burst', 'scale', 'ws', 'rich', 'gui']
    defs = defs.filter((d) => (d.suites ?? []).includes(suite) && want(d.id)).sort((a, b) => ORDER.indexOf(a.kind) - ORDER.indexOf(b.kind))
    const parts = new Set(['book', 'small', 'proc', ...(manifest.parts ?? []), ...Object.keys(manifest)])
    const sharedByFamily = new Map()
    let started = !!gard
    for (const def of defs) {
      const missing = (def.needs ?? []).filter((n) => !(n === 'proc' ? !attach : parts.has(n)))
      if (missing.length) { notes.push(`skipped ${def.id}: needs ${missing.join(',')}`); log(`  skip ${def.id} (needs ${missing.join(',')})`); continue }
      if (def.kind !== 'boot' && !started && !attach) { if (!gard || gard.child.exitCode !== null) { gard = await startGardend(); await gard.waitHealthy(); client = gard.client() } started = true }
      if (def.kind !== 'boot' && !sampler) { sampler = setInterval(sampleRss, 1000); sampleRss() }
      const sk = def.id.split('.')[0], shared = sharedByFamily.get(sk) ?? {}; sharedByFamily.set(sk, shared)
      const params = { ...def.params, ...(def.params?.[suite] ?? {}) }; delete params.smoke; delete params.standard
      const ctx = mkCtx(params, base.shared); ctx.shared = def.kind === 'boot' ? base.shared : shared
      const calls = []; let extra = {}
      try {
        if (def.setup) await def.setup(ctx)
        if (def.run) {
          const out = await def.run(ctx)
          if (out.skipped) { notes.push(`skipped ${def.id}: ${out.skipped}`); log(`  skip ${def.id}: ${out.skipped}`); continue }
          calls.push(...out.calls); if (out.extra) extra.extra = out.extra
        } else {
          const conc = params.concurrency ?? 1, signal = { stop: false }
          const one = async (i, k, warm) => {
            const c = conc > 1 || def.during ? Object.create(ctx) : ctx; c._trace = newTrace(); const t0 = performance.now(); let ok = true, err = null
            try { await def.op(c, i, k) } catch (e) { ok = false; err = client.redact(e.message).slice(0, 200) }
            const ms = performance.now() - t0; if (!warm) calls.push({ traceId: c._trace.traceId, ms, ok, error: err })
          }
          const bg = def.during ? def.during(ctx, signal) : null
          if (def.warmupDelayMs) await sleep(def.warmupDelayMs)
          const t = params.iterations + params.warmup, roundWall = []
          for (let i = 0; i < t; i++) {
            const w0 = performance.now()
            if (conc > 1) await Promise.all(Array.from({ length: conc }, (_, k) => one(i, k, i < params.warmup))); else await one(i, 0, i < params.warmup)
            if (i >= params.warmup && conc > 1) roundWall.push(Math.round((performance.now() - w0) * 10) / 10)
          }
          signal.stop = true; if (bg) await bg
          if (roundWall.length) extra.extra = { roundWallMs: roundWall }
          if (def.finish) { const fo = def.finish(ctx, calls.filter((c) => c.ok).map((c) => c.ms)) ?? {}; if (fo.slope) extra.slope = fo.slope; const rest = { ...fo }; delete rest.slope; if (Object.keys(rest).length) extra.extra = { ...(extra.extra ?? {}), ...rest } }
          if (conc > 1) params.iterations = params.iterations * conc
        }
      } catch (e) { notes.push(`case ${def.id} failed: ${client?.redact ? client.redact(e.message) : e.message}`); log(`  FAIL ${def.id}: ${e.message}`); continue }
      finally { try { if (def.teardown) await def.teardown(ctx) } catch {} }
      results.push(caseResult({ caseId: def.id, kind: def.kind, caseVersion: def.version, params, calls, extra }))
      const s = results.at(-1).ms; log(`  ${def.id}: n ${results.at(-1).n} err ${results.at(-1).errors} p50 ${s.p50} p95 ${s.p95}`)
      try { writeFileSync(path.join(runDir, 'run.partial.json'), JSON.stringify({ runId, label, suite, tier, cases: results.map(({ _calls, ...r }) => r) })) } catch {}
      await sleep(def.kind === 'read' || def.kind === 'boot' ? 300 : 1200) // let background work (projection flush) settle
    }
    if (sampler) { clearInterval(sampler); sampleRss() }
    if (gard?.child && gard.child.exitCode === null) { const c = gard.cpu(); cpuU += c.user; cpuS += c.sys }
  } catch (e) {
    failure = (client?.redact ? client.redact(String(e.stack ?? e.message)) : String(e.message)).slice(0, 2000); log('RUN FAILED:', e.message)
  } finally {
    if (sampler) clearInterval(sampler)
    if (gard && gard.child && gard.child.exitCode === null) { log('stopping our gardend (SIGINT, final flush)'); await gard.stop(); if (gard.stopMs) notes.push(`final flush on SIGINT took ${Math.round(gard.stopMs)} ms`) }
  }

  // ---- join spans
  const hasSpans = !attach && spanFiles.length > 0
  let spanJoin = null
  const allCalls = results.flatMap((r) => r._calls.filter((c) => c.traceId))
  if (hasSpans) {
    const wanted = new Set(allCalls.map((c) => c.traceId))
    const loaded = await loadSpans(spanFiles, wanted)
    let joinedSpans = 0; for (const a of loaded.byTrace.values()) joinedSpans += a.length
    for (const r of results) {
      const calls = r._calls.filter((c) => c.traceId && c.ok)
      const sf = serverFacts(calls, loaded)
      if (sf.server) { r.server = { ...sf.server, callsJoined: sf.joined, callsTotal: calls.length, unjoinedCalls: calls.length - sf.joined }; r.exemplars = sf.exemplars }
    }
    const callsTotal = results.reduce((a, r) => a + r._calls.filter((c) => c.traceId && c.ok).length, 0), callsJoined = results.reduce((a, r) => a + (r.server?.callsJoined ?? 0), 0)
    const top = [...loaded.other.entries()].map(([name, v]) => ({ name, n: v.n, totalMs: Math.round(v.totalMs) })).sort((a, b) => b.totalMs - a.totalMs).slice(0, 10)
    spanJoin = { callsTotal, callsJoined, fraction: callsTotal ? callsJoined / callsTotal : 0, spansTotal: loaded.total, spansJoined: joinedSpans, spansUnjoined: loaded.total - joinedSpans, unjoinedTop: top, badLines: loaded.bad }
    // scale slope
    // gz the spans
    const gz = createGzip(); const out = createWriteStream(path.join(runDir, 'spans.jsonl.gz')); gz.pipe(out)
    for (const f of spanFiles) { for await (const chunk of createReadStream(f)) if (!gz.write(chunk)) await new Promise((r) => gz.once('drain', r)) }
    gz.end(); await new Promise((r) => out.on('close', r))
    for (const f of spanFiles) rmSync(f)
  }
  for (const r of results) delete r._calls

  const finishedAt = new Date()
  const run = {
    schema: 'sophia.bench.run.v1', runId, label, protocolVersion: PROTOCOL_VERSION, startedAt: startedAt.toISOString(), finishedAt: finishedAt.toISOString(),
    mode: attach ? 'attach' : 'spawn', suite, tier, seed,
    target: { binary: binInfo.binary, binarySha256: binInfo.binarySha256, version: binInfo.version, gitRev: binInfo.gitRev, url, graphId: G, spans: hasSpans },
    env: { ...fp0, contention: o._contention ?? { measured: false }, loadAfter: loadavg(), otherGardends: fp0.otherGardends.filter((g) => !pids.includes(g.pid)) },
    fixture: { ...manifest.counts, seedMs: manifest.seedMs },
    cases: results,
    process: { rssStartMB: Math.round(rssStart / 1024), rssPeakMB: Math.round(rssPeak / 1024), rssEndMB: Math.round(rssEnd / 1024), cpuUserS: Math.round(cpuU * 10) / 10, cpuSysS: Math.round(cpuS * 10) / 10 },
    ...(failure ? { failed: true, error: failure } : {}), spanJoin, notes: [...notes, ...(fp0.loadBefore[0] > 6 ? [`machine was busy at start: load1 ${fp0.loadBefore[0]}`] : [])],
  }
  if (fp0.otherGardends.length) run.notes.push(`foreign gardend process(es) present, not ours: ${fp0.otherGardends.map((g) => `pid ${g.pid} port ${g.port}`).join('; ')}`)
  writeFileSync(path.join(runDir, 'run.json'), JSON.stringify(run, null, 1)); rmSync(path.join(runDir, 'run.partial.json'), { force: true })
  writeFileSync(path.join(runDir, 'summary.md'), summaryMd(run))
  // remove this run's profile clone (ours, under the cache)
  if (!attach && clone.startsWith(path.join(CACHE_ROOT, 'runs', runId))) rmSync(path.dirname(clone), { recursive: true, force: true })
  log(`run complete: ${runDir}`)
  return { run, runDir }
}

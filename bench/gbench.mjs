#!/usr/bin/env node
// gbench: benchmark protocol and runner for gardend. See PROTOCOL.md.
import { readFileSync, writeFileSync, mkdirSync, readdirSync, existsSync } from 'node:fs'
import path from 'node:path'
import { runBench } from './lib/runner.mjs'
import { ensureFixture, TIERS, CACHE_ROOT } from './lib/fixture.mjs'
import { fingerprint, DEFAULT_BIN, binaryInfo } from './lib/proc.mjs'
import { loadRun, writeCompare, pooledRun } from './lib/compare.mjs'
import { takeMeasure } from './lib/lock.mjs'
import { holdAwake, powerDuring, roundHealth } from './lib/power.mjs'
import { publish } from './lib/publish.mjs'

function parseArgs(argv) {
  const pos = [], o = {}
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]
    if (a.startsWith('--')) { const k = a.slice(2); const nxt = argv[i + 1]; if (nxt === undefined || nxt.startsWith('--')) o[k] = true; else { o[k] = nxt; i++ } } else pos.push(a)
  }
  return { pos, o }
}
const USAGE = `gbench <cmd>
  fixture --tier s|m|l --seed N [--bin <gardend>] [--force]
  run --bin <gardend> --suite smoke|standard --tier s|m|l --label <l> --out <dir> [--seed N] [--cases <regex>] [--measured]
  run --attach <url> --key-file <path> [--suite ..] [--tier s] [--label ..] [--out ..]      (never against an instance you care about without reading PROTOCOL.md)
  run|ab ... --real-profile <backup profile dir> --graph <id> --suite real   (reads on an APFS clone of real data; never ~/sophia)
  compare <runDirA> <runDirB> [--out <dir>]
  ab --a <bin> --b <bin> --rounds R [--suite ..] [--tier ..] [--label ..] [--out <dir>]
  publish <runDir> --url <url> --key-file <path> [--baseline]
  audit <dir>        reconstruct env.machine from the power log for runs that predate it; re-judge ab sittings
  env`

const { pos, o } = parseArgs(process.argv.slice(2)), cmd = pos[0]
try {
  if (cmd === 'env') { console.log(JSON.stringify({ ...fingerprint([]), defaultBinary: existsSync(DEFAULT_BIN) ? binaryInfo(DEFAULT_BIN) : null }, null, 1)) }
  else if (cmd === 'fixture') {
    const bin = o.bin ?? DEFAULT_BIN, tier = o.tier ?? 's', seed = Number(o.seed ?? 1)
    if (!TIERS[tier]) throw new Error('tier must be s|m|l')
    const r = await ensureFixture({ bin, tier, seed, force: !!o.force })
    console.log(JSON.stringify({ dir: r.dir, built: r.built, counts: r.manifest.counts, seedMs: r.manifest.seedMs, phases: r.manifest.phases, profileMB: r.manifest.profileMB }, null, 1))
  } else if (cmd === 'run') {
    const out = o.out ?? path.join(path.dirname(new URL(import.meta.url).pathname), 'runs')
    const { run, runDir } = await runBench({ ...o, out, measured: !!o.measured })
    console.log(JSON.stringify({ runId: run.runId, runDir, cases: run.cases.length }))
  } else if (cmd === 'compare') {
    const [, a, b] = pos; if (!a || !b) throw new Error('compare <runA> <runB>')
    const A = loadRun(a), B = loadRun(b)
    const { md, judge } = writeCompare(A, B, o.out ?? path.join(path.resolve(b), `compare-vs-${A.runId}`))
    console.log(md); if (!judge.ok) { console.error('gbench: NOT A JUDGE: a run was not on a clean machine'); process.exitCode = 3 }
  } else if (cmd === 'ab') {
    if (!o.a || !o.b) throw new Error('ab --a <bin> --b <bin> --rounds R')
    const rounds = Number(o.rounds ?? 3), out = path.resolve(o.out ?? 'runs'), label = (o.label ?? 'ab').replace(/[^a-z0-9-]/gi, '-').toLowerCase(), ra = [], rb = []
    const stamp = new Date().toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z'), abName = `${stamp}-${label}-ab`, abDir = path.join(out, abName)
    const lg = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a)
    const lock = await takeMeasure({ lane: 'core', label: `${label} ab`, log: lg })   // one lock for the whole sitting
    const awake = holdAwake(lg)                                                         // and one awake hold (power.mjs)
    try {
      for (let i = 0; i < rounds; i++) for (const [name, bin, acc] of [['a', o.a, ra], ['b', o.b, rb]]) {
        // ABAB interleaved; both sides clone the same fixture snapshot (keyed by A's gitRev)
        const { run } = await runBench({ ...o, bin, fixtureBin: o.a, out: path.join(abDir, 'rounds'), label: `${label}-${name}${i + 1}`, suite: o.suite ?? 'smoke', tier: o.tier ?? 's', holdsLock: true, holdsAwake: true, _contention: lock.contention }); acc.push(run)
      }
    } finally { awake.release(); lock.release() }
    const idA = `${stamp}-${label}-a`, idB = `${stamp}-${label}-b`
    const A = pooledRun(ra, { runId: idA, label: `${label}-a`, abSitting: abName, abPartner: idB }), Bp = pooledRun(rb, { runId: idB, label: `${label}-b`, abSitting: abName, abPartner: idA })
    for (const [k, r] of [['a', A], ['b', Bp]]) { mkdirSync(path.join(abDir, k), { recursive: true }); writeFileSync(path.join(abDir, k, 'run.json'), JSON.stringify(r, null, 1)) }
    const { md, judge } = writeCompare(A, Bp, abDir, { pairs: ra.map((r, i) => [r, rb[i]]) }); console.log(md); console.log(`\nwritten to ${abDir} (a/run.json, b/run.json, compare.json, compare.md, rounds/)`)
    if (!judge.ok) { console.error('gbench: NOT A JUDGE: a round ran on an unclean machine (see compare.md)'); process.exitCode = 3 }
  } else if (cmd === 'audit') {
    // Runs recorded before power accounting (2026-10-26) get env.machine reconstructed from `pmset -g log`
    // (foreign CPU stays unmeasured); ab sittings get their pooled rounds and compare.md re-judged.
    const root = path.resolve(pos[1] ?? '.'), found = []
    const walk = (d, depth) => { for (const e of readdirSync(d, { withFileTypes: true })) { const p = path.join(d, e.name); if (e.isDirectory() && depth < 4) walk(p, depth + 1); else if (e.name === 'run.json') found.push(p) } }
    walk(root, 0)
    const byId = new Map(), out = []
    for (const f of found) {
      const r = JSON.parse(readFileSync(f, 'utf8'))
      if (!r.pooledFrom && !r.env?.machine && r.startedAt && r.finishedAt) {
        const during = await powerDuring(Date.parse(r.startedAt), Date.parse(r.finishedAt))
        const machine = { reconstructed: new Date().toISOString(), awakeHeld: false, start: null, end: null, during, foreignCpu: { measured: false } }
        machine.health = roundHealth({ machine })
        r.env = { ...r.env, machine }; r.notes = [...(r.notes ?? []), `machine state reconstructed from the power log by gbench audit: ${machine.health.clean ? 'clean' : `NOT CLEAN, not a judge: ${machine.health.why.join('; ')}`}`]
        writeFileSync(f, JSON.stringify(r, null, 1))
      }
      byId.set(r.runId, { f, r }); out.push({ runId: r.runId, clean: r.env?.machine?.health?.clean ?? null, why: r.env?.machine?.health?.why ?? [] })
    }
    for (const f of found) {   // ab sittings: pooled a/ and b/ carry their rounds' health; compare.md is rewritten with the judge banner
      const r = JSON.parse(readFileSync(f, 'utf8')); if (!r.pooledFrom) continue
      const rs = r.pooledFrom.map((id) => byId.get(id)?.r).filter(Boolean)
      r.rounds = rs.map((x) => ({ runId: x.runId, health: x.env?.machine?.health ?? roundHealth(x.env), foreignCpu: x.env?.machine?.foreignCpu?.foreignPct ?? null })); writeFileSync(f, JSON.stringify(r, null, 1)); byId.set(r.runId, { f, r })
    }
    for (const f of found) {
      const r = byId.get(JSON.parse(readFileSync(f, 'utf8')).runId)?.r; if (!r?.pooledFrom || !r.abPartner || r.label.endsWith('-a')) continue
      const A = byId.get(r.abPartner)?.r; if (!A) continue
      const ra = A.pooledFrom.map((id) => byId.get(id)?.r), rb = r.pooledFrom.map((id) => byId.get(id)?.r)
      const { judge } = writeCompare(A, r, path.dirname(path.dirname(f)), { pairs: ra.map((x, i) => [x, rb[i]]).filter(([x, y]) => x && y) })
      out.push({ sitting: r.abSitting, judge: judge.ok })
    }
    console.log(JSON.stringify(out, null, 1))
  } else if (cmd === 'publish') {
    const dir = pos[1]; if (!dir || !o.url || !o['key-file']) throw new Error('publish <runDir> --url <url> --key-file <path> [--baseline]')
    console.log(JSON.stringify(await publish(path.resolve(dir), { url: o.url, keyFile: o['key-file'], baseline: !!o.baseline })))
  } else { console.log(USAGE); process.exitCode = cmd ? 2 : 0 }
} catch (e) { console.error('gbench:', e.message); process.exitCode = 1 }
process.exit(process.exitCode ?? 0)

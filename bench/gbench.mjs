#!/usr/bin/env node
// gbench: benchmark protocol and runner for gardend. See PROTOCOL.md.
import { readFileSync, writeFileSync, mkdirSync, readdirSync, existsSync } from 'node:fs'
import path from 'node:path'
import { runBench } from './lib/runner.mjs'
import { ensureFixture, TIERS, CACHE_ROOT } from './lib/fixture.mjs'
import { fingerprint, DEFAULT_BIN, binaryInfo } from './lib/proc.mjs'
import { loadRun, writeCompare, pool } from './lib/compare.mjs'
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
  compare <runDirA> <runDirB> [--out <dir>]
  ab --a <bin> --b <bin> --rounds R [--suite ..] [--tier ..] [--label ..] [--out <dir>]
  publish <runDir> --url <url> --key-file <path> [--baseline]
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
    const { md } = writeCompare(A, B, o.out ?? path.join(path.resolve(b), `compare-vs-${A.runId}`))
    console.log(md)
  } else if (cmd === 'ab') {
    if (!o.a || !o.b) throw new Error('ab --a <bin> --b <bin> --rounds R')
    const rounds = Number(o.rounds ?? 3), out = path.resolve(o.out ?? 'runs'), label = o.label ?? 'ab', ra = [], rb = []
    for (let i = 0; i < rounds; i++) for (const [name, bin, acc] of [['a', o.a, ra], ['b', o.b, rb]]) {
      // ABAB interleaved; each run gets its own clone of the same fixture snapshot
      const { run } = await runBench({ ...o, bin, out, label: `${label}-${name}${i + 1}`, measured: true, suite: o.suite ?? 'smoke', tier: o.tier ?? 's' }); acc.push(run)
    }
    const A = pool(ra, `${label}-a-pooled`), Bp = pool(rb, `${label}-b-pooled`)
    const dir = path.join(out, `${label}-ab-${new Date().toISOString().replace(/[-:]/g, '').slice(0, 15)}Z`)
    const { md } = writeCompare(A, Bp, dir); console.log(md); console.log(`\nwritten to ${dir}`)
  } else if (cmd === 'publish') {
    const dir = pos[1]; if (!dir || !o.url || !o['key-file']) throw new Error('publish <runDir> --url <url> --key-file <path> [--baseline]')
    console.log(JSON.stringify(await publish(path.resolve(dir), { url: o.url, keyFile: o['key-file'], baseline: !!o.baseline })))
  } else { console.log(USAGE); process.exitCode = cmd ? 2 : 0 }
} catch (e) { console.error('gbench:', e.message); process.exitCode = 1 }
process.exit(process.exitCode ?? 0)

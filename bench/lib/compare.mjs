import { readFileSync, writeFileSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { bootstrapRatio, verdict, median, p95stat, r2 } from './stats.mjs'

export const loadRun = (dirOrFile) => JSON.parse(readFileSync(dirOrFile.endsWith('.json') ? dirOrFile : path.join(dirOrFile, 'run.json'), 'utf8'))

// Compare cases of B against A (ratio B/A). A and B are run objects; either may be a pooled run (see pool()).
export function compareRuns(A, B, { resamples = 10000, seed = 20261025 } = {}) {
  const byId = new Map(A.cases.map((c) => [c.caseId, c])), rows = []
  for (const cb of B.cases) {
    const ca = byId.get(cb.caseId)
    if (!ca) { rows.push({ caseId: cb.caseId, skipped: 'only in B' }); continue }
    if (ca.caseVersion !== cb.caseVersion) { rows.push({ caseId: cb.caseId, skipped: `caseVersion ${ca.caseVersion} vs ${cb.caseVersion}` }); continue }
    if (!ca.samples.length || !cb.samples.length) { rows.push({ caseId: cb.caseId, skipped: 'no samples' }); continue }
    const m = bootstrapRatio(ca.samples, cb.samples, { stat: median, resamples, seed })
    const p = bootstrapRatio(ca.samples, cb.samples, { stat: p95stat, resamples: Math.min(resamples, 4000), seed })
    const row = { caseId: cb.caseId, nA: ca.n, nB: cb.n, p50A: ca.ms.p50, p50B: cb.ms.p50, median: { ...m, verdict: verdict(m) }, p95: { ...p, verdict: verdict(p) } }
    const fa = ca.server?.fsyncsPerOp?.p50, fb = cb.server?.fsyncsPerOp?.p50
    if (fa != null && fb != null) row.fsyncsPerOpP50 = { a: fa, b: fb, verdict: fa === fb ? 'same' : fb < fa ? 'faster' : 'slower' }
    const ha = ca.server?.leaseHoldMs?.p95, hb = cb.server?.leaseHoldMs?.p95
    if (ha != null && hb != null) row.leaseHoldP95 = { a: ha, b: hb, ratio: ha ? hb / ha : NaN, verdict: ha && Math.abs(hb / ha - 1) >= 0.05 && hb > 1 ? (hb < ha ? 'faster' : 'slower') : 'same' }
    rows.push(row)
  }
  return rows
}
const f = (x, d = 2) => (Number.isFinite(x) ? x.toFixed(d) : '-')
export function compareMd(A, B, rows) {
  const L = [`# Compare ${B.runId} against ${A.runId}`, '', `Ratio is B/A of the median (bootstrap 95% CI, ${10000} resamples, seeded). \`same\` when the CI contains 1.0 or |ratio-1| < 0.05.`, '',
    '| case | p50 A | p50 B | ratio p50 | CI | verdict | ratio p95 | p95 verdict | fsyncs/op A→B | lease hold p95 A→B |', '|---|---:|---:|---:|---|---|---:|---|---|---|']
  for (const r of rows) {
    if (r.skipped) { L.push(`| ${r.caseId} | | | | | skipped: ${r.skipped} | | | | |`); continue }
    L.push(`| ${r.caseId} | ${f(r.p50A, 1)} | ${f(r.p50B, 1)} | ${f(r.median.ratio)} | ${f(r.median.lo)}–${f(r.median.hi)} | **${r.median.verdict}** | ${f(r.p95.ratio)} | ${r.p95.verdict} | ${r.fsyncsPerOpP50 ? `${r.fsyncsPerOpP50.a}→${r.fsyncsPerOpP50.b} (${r.fsyncsPerOpP50.verdict})` : '-'} | ${r.leaseHoldP95 ? `${f(r.leaseHoldP95.a, 1)}→${f(r.leaseHoldP95.b, 1)} (${r.leaseHoldP95.verdict})` : '-'} |`)
  }
  const ok = rows.filter((r) => !r.skipped), ratios = ok.map((r) => r.median.ratio).sort((a, b) => a - b)
  const q = (p) => ratios[Math.min(ratios.length - 1, Math.floor(p * ratios.length))]
  const counts = ok.reduce((a, r) => ({ ...a, [r.median.verdict]: (a[r.median.verdict] ?? 0) + 1 }), {})
  L.push('', `Verdicts: ${JSON.stringify(counts)}. Spread of the median ratios across ${ratios.length} cases: min ${f(ratios[0])}, p10 ${f(q(0.1))}, median ${f(q(0.5))}, p90 ${f(q(0.9))}, max ${f(ratios.at(-1))}.`)
  return L.join('\n')
}
export function writeCompare(A, B, outDir) {
  const rows = compareRuns(A, B), md = compareMd(A, B, rows)
  mkdirSync(outDir, { recursive: true })
  writeFileSync(path.join(outDir, 'compare.json'), JSON.stringify({ a: A.runId, b: B.runId, rows }, null, 1)); writeFileSync(path.join(outDir, 'compare.md'), md)
  return { rows, md }
}
// Pool several runs of the same label into one pseudo-run (samples concatenated per case).
export function pool(runs, runId) {
  const base = runs[0], cases = base.cases.map((c) => {
    const parts = runs.map((r) => r.cases.find((x) => x.caseId === c.caseId && x.caseVersion === c.caseVersion)).filter(Boolean)
    const samples = parts.flatMap((p) => p.samples), s = [...samples].sort((a, b) => a - b), q = (p) => { const pos = (s.length - 1) * p, lo = Math.floor(pos), hi = Math.ceil(pos); return s[lo] + (s[hi] - s[lo]) * (pos - lo) }
    const ms = s.length ? { mean: r2(s.reduce((a, b) => a + b, 0) / s.length), p50: r2(q(0.5)), p95: r2(q(0.95)), p99: r2(q(0.99)), max: r2(s.at(-1)), min: r2(s[0]), mad: 0 } : c.ms
    const fp = parts.map((p) => p.server?.fsyncsPerOp?.p50).filter((v) => v != null), lh = parts.map((p) => p.server?.leaseHoldMs?.p95).filter((v) => v != null)
    return { ...c, samples, n: samples.length, errors: parts.reduce((a, p) => a + p.errors, 0), ms, server: fp.length ? { fsyncsPerOp: { p50: median(fp) }, leaseHoldMs: { p95: median(lh) } } : null }
  })
  return { ...base, runId, cases }
}

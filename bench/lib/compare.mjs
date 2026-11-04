import { readFileSync, writeFileSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { bootstrapRatio, verdict, median, p95stat, r2 } from './stats.mjs'
import { roundHealth } from './power.mjs'

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
    const xa = ca.server?.fullFsyncsPerOp?.p50 ?? fa, xb = cb.server?.fullFsyncsPerOp?.p50 ?? fb
    if (xa != null && xb != null && (xa !== fa || xb !== fb)) row.fullFsyncsPerOpP50 = { a: xa, b: xb }
    const ha = ca.server?.leaseHoldMs?.p95, hb = cb.server?.leaseHoldMs?.p95
    if (ha != null && hb != null) row.leaseHoldP95 = { a: ha, b: hb, ratio: ha ? hb / ha : NaN, verdict: ha && Math.abs(hb / ha - 1) >= 0.05 && hb > 1 ? (hb < ha ? 'faster' : 'slower') : 'same' }
    // Thresholds marked `ab: true` are B/A ratios no single run can judge (report.mjs leaves them `deferred`); here they are judged
    // by this comparison's median ratio: pass when the ratio meets the limit or the comparison's verdict is `same` / `faster`.
    const abSpecs = (cb.thresholds ?? []).filter((t) => t.ab)
    if (abSpecs.length) row.thresholds = abSpecs.map((t) => abVerdict(t, row))
    rows.push(row)
  }
  return rows
}
const abCmp = { '<=': (a, b) => a <= b, '<': (a, b) => a < b, '>=': (a, b) => a >= b, '>': (a, b) => a > b }
export function abVerdict(t, row) {
  const ratio = row.median.ratio, f1 = abCmp[t.op]
  if (!f1 || !Number.isFinite(ratio)) return { name: t.name ?? t.metric, limit: t.limit, op: t.op, ratio: null, status: 'unmeasured', detail: `no usable ratio for op ${t.op}` }
  const met = f1(ratio, t.limit), calm = row.median.verdict === 'same' || row.median.verdict === 'faster'
  return { name: t.name ?? t.metric, limit: t.limit, op: t.op, ratio, verdict: row.median.verdict, status: met || calm ? 'pass' : 'fail',
    detail: `B/A median ratio ${ratio.toFixed(2)} (CI ${row.median.lo?.toFixed?.(2)}-${row.median.hi?.toFixed?.(2)}), verdict ${row.median.verdict}; want ${t.op} ${t.limit} or a \`same\` verdict` }
}
const f = (x, d = 2) => (Number.isFinite(x) ? x.toFixed(d) : '-')
// Is this comparison a judge? Only if every round on both sides ran on a clean machine (power.mjs roundHealth).
export function sittingHealth(run) {
  const rounds = run.rounds ?? [{ runId: run.runId, health: run.env?.machine?.health ?? roundHealth(run.env), foreignCpu: run.env?.machine?.foreignCpu?.foreignPct }]
  return rounds.map((r) => ({ runId: r.runId, clean: r.health?.clean ?? null, why: r.health?.why ?? [], foreignCpu: r.foreignCpu ?? null }))
}
export function judgeOf(A, B) {
  const rounds = [...sittingHealth(A), ...sittingHealth(B)]
  return { ok: rounds.every((r) => r.clean === true), rounds }
}
export function compareMd(A, B, rows, judge = judgeOf(A, B)) {
  const pairs = rows.some((r) => r.pairs)
  const L = [`# Compare ${B.runId} against ${A.runId}`, '']
  if (!judge.ok) L.push('> **NOT A JUDGE.** At least one round did not run on a clean machine (asleep, DarkWake, throttled, on battery, or unrecorded). The numbers below describe that machine, not the binaries.', '')
  L.push('| round | machine | foreign CPU mean / p95 (%) |', '|---|---|---|', ...judge.rounds.map((r) => `| ${r.runId} | ${r.clean === true ? 'clean' : r.clean === false ? `**not clean**: ${r.why.join('; ')}` : `unrecorded: ${r.why.join('; ')}`} | ${r.foreignCpu ? `${r.foreignCpu.mean} / ${r.foreignCpu.p95}` : '-'} |`), '')
  L.push(`Ratio is B/A of the median (bootstrap 95% CI, ${10000} resamples, seeded). \`same\` when the CI contains 1.0 or |ratio-1| < 0.05.${pairs ? ' "Per round" is the median ratio of each interleaved pair (B_i/A_i): a pooled verdict that the pairs disagree on is drift, not a result.' : ''}`, '',
    `| case | p50 A | p50 B | ratio p50 | CI | verdict |${pairs ? ' per round |' : ''} ratio p95 | p95 verdict | fsyncs/op A→B | lease hold p95 A→B |`, `|---|---:|---:|---:|---|---|${pairs ? '---|' : ''}---:|---|---|---|`)
  for (const r of rows) {
    if (r.skipped) { L.push(`| ${r.caseId} | | | | | skipped: ${r.skipped} |${pairs ? ' |' : ''} | | | |`); continue }
    L.push(`| ${r.caseId} | ${f(r.p50A, 1)} | ${f(r.p50B, 1)} | ${f(r.median.ratio)} | ${f(r.median.lo)}–${f(r.median.hi)} | **${r.median.verdict}** |${pairs ? ` ${(r.pairs ?? []).map((x) => f(x)).join(' / ')} |` : ''} ${f(r.p95.ratio)} | ${r.p95.verdict} | ${r.fsyncsPerOpP50 ? `${r.fsyncsPerOpP50.a}→${r.fsyncsPerOpP50.b} (${r.fsyncsPerOpP50.verdict})${r.fullFsyncsPerOpP50 ? `; full ${r.fullFsyncsPerOpP50.a}→${r.fullFsyncsPerOpP50.b}` : ''}` : '-'} | ${r.leaseHoldP95 ? `${f(r.leaseHoldP95.a, 1)}→${f(r.leaseHoldP95.b, 1)} (${r.leaseHoldP95.verdict})` : '-'} |`)
  }
  const abLines = rows.flatMap((r) => (r.thresholds ?? []).map((t) => `- ${t.status.toUpperCase()} ${r.caseId}: ${t.name} -- ${t.detail}`))
  if (abLines.length) L.push('', `## Thresholds judged by this comparison (B/A)${judge.ok ? '' : ' -- NOT A JUDGE, read as direction only'}`, '', ...abLines)
  const ok = rows.filter((r) => !r.skipped), ratios = ok.map((r) => r.median.ratio).sort((a, b) => a - b)
  const q = (p) => ratios[Math.min(ratios.length - 1, Math.floor(p * ratios.length))]
  const counts = ok.reduce((a, r) => ({ ...a, [r.median.verdict]: (a[r.median.verdict] ?? 0) + 1 }), {})
  L.push('', `Verdicts: ${JSON.stringify(counts)}. Spread of the median ratios across ${ratios.length} cases: min ${f(ratios[0])}, p10 ${f(q(0.1))}, median ${f(q(0.5))}, p90 ${f(q(0.9))}, max ${f(ratios.at(-1))}.`)
  return L.join('\n')
}
// pairs: optional [[runA_i, runB_i], ...] (an ab sitting's interleaved rounds) for the per-round ratio column.
export function writeCompare(A, B, outDir, { pairs } = {}) {
  const rows = compareRuns(A, B), judge = judgeOf(A, B)
  if (pairs?.length) {
    const per = pairs.map(([a, b]) => new Map(compareRuns(a, b, { resamples: 1000 }).filter((r) => !r.skipped).map((r) => [r.caseId, r.median.ratio])))
    for (const r of rows) if (!r.skipped) r.pairs = per.map((m) => m.get(r.caseId) ?? NaN)
  }
  const md = compareMd(A, B, rows, judge)
  mkdirSync(outDir, { recursive: true })
  writeFileSync(path.join(outDir, 'compare.json'), JSON.stringify({ a: A.runId, b: B.runId, judge, rows }, null, 1)); writeFileSync(path.join(outDir, 'compare.md'), md)
  return { rows, md, judge }
}
// Pool several runs of the same label into one pseudo-run (samples concatenated per case).
export function pool(runs, runId) {
  const base = runs[0], cases = base.cases.map((c) => {
    const parts = runs.map((r) => r.cases.find((x) => x.caseId === c.caseId && x.caseVersion === c.caseVersion)).filter(Boolean)
    const samples = parts.flatMap((p) => p.samples), s = [...samples].sort((a, b) => a - b), q = (p) => { const pos = (s.length - 1) * p, lo = Math.floor(pos), hi = Math.ceil(pos); return s[lo] + (s[hi] - s[lo]) * (pos - lo) }
    const ms = s.length ? { mean: r2(s.reduce((a, b) => a + b, 0) / s.length), p50: r2(q(0.5)), p95: r2(q(0.95)), p99: r2(q(0.99)), max: r2(s.at(-1)), min: r2(s[0]), mad: 0 } : c.ms
    const fp = parts.map((p) => p.server?.fsyncsPerOp?.p50).filter((v) => v != null), lh = parts.map((p) => p.server?.leaseHoldMs?.p95).filter((v) => v != null), ff = parts.map((p) => p.server?.fullFsyncsPerOp?.p50).filter((v) => v != null)
    return { ...c, samples, n: samples.length, errors: parts.reduce((a, p) => a + p.errors, 0), ms, traceIds: undefined, params: { ...c.params, pooledRounds: parts.length },
      server: fp.length ? { ...parts[0].server, fsyncsPerOp: { ...parts[0].server.fsyncsPerOp, p50: median(fp) }, ...(ff.length ? { fullFsyncsPerOp: { ...parts[0].server.fullFsyncsPerOp, p50: median(ff) } } : {}), leaseHoldMs: { ...parts[0].server.leaseHoldMs, p95: median(lh) } } : null }
  })
  return { ...base, runId, cases }
}

// Make a pooled pseudo-run a publishable run: schema-valid, own target, abSitting/abPartner.
export function pooledRun(runs, { runId, label, abSitting, abPartner }) {
  const p = pool(runs, runId)
  return { ...p, runId, label, abSitting, abPartner, pooledFrom: runs.map((r) => r.runId),
    rounds: runs.map((r) => ({ runId: r.runId, health: r.env?.machine?.health ?? roundHealth(r.env), foreignCpu: r.env?.machine?.foreignCpu?.foreignPct ?? null })),
    startedAt: runs.map((r) => r.startedAt).sort()[0], finishedAt: runs.map((r) => r.finishedAt).sort().at(-1),
    notes: [...(runs[0].notes ?? []), `pooled from ${runs.length} rounds: ${runs.map((r) => r.runId).join(', ')}`],
    process: { rssStartMB: Math.max(...runs.map((r) => r.process.rssStartMB)), rssPeakMB: Math.max(...runs.map((r) => r.process.rssPeakMB)), rssEndMB: runs.at(-1).process.rssEndMB, cpuUserS: runs.reduce((a, r) => a + r.process.cpuUserS, 0), cpuSysS: runs.reduce((a, r) => a + r.process.cpuSysS, 0) },
    env: { ...runs[0].env, loadAfter: runs.at(-1).env.loadAfter } }
}

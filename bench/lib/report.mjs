import { renderTree } from './spans.mjs'

const f = (x, d = 1) => (x == null || Number.isNaN(x) ? '-' : Number(x).toFixed(d).replace(/\.0+$/, ''))
// ---- thresholds ------------------------------------------------------------------------------------------------------
// A case def may carry `thresholds: [{name, metric, op, limit, unit?, ratioTo?, ab?, proposed?, note?}]`.
//   metric: 'p50' | 'p95' | 'p99' | 'mean' | 'max' | 'min' | 'n' | 'errors' | 'extra.<dotted path into the case's extra>'
//   op:     '<=' | '<' | '>=' | '>' | '==' | '!='        limit: a number, or true/false for '=='
//   ratioTo: a caseId of the same run; the value is metric(this case) / metric(that case)
//   ab:     true marks a B/A ratio that only the A/B comparison can judge: reported as 'deferred', never pass or fail here
//   proposed: true marks a limit nobody has ratified yet (printed as such)
// evaluateThresholds returns the same entries with {value, status: 'pass' | 'fail' | 'unmeasured' | 'deferred', detail}.
const metricOf = (r, metric) => {
  if (!r) return undefined
  if (['p50', 'p95', 'p99', 'mean', 'max', 'min'].includes(metric)) return r.n > 0 ? r.ms?.[metric] : undefined
  if (metric === 'n') return r.n
  if (metric === 'errors') return r.errors
  if (metric.startsWith('extra.')) return metric.slice(6).split('.').reduce((o, k) => (o == null ? undefined : o[k]), r.extra)
  return undefined
}
const cmp = { '<=': (a, b) => a <= b, '<': (a, b) => a < b, '>=': (a, b) => a >= b, '>': (a, b) => a > b, '==': (a, b) => a === b, '!=': (a, b) => a !== b }
export function evaluateThresholds(specs, result, all = []) {
  return specs.map((t) => {
    const out = { ...t }
    if (t.ab) return { ...out, value: null, status: 'deferred', detail: 'B/A ratio: judged by the A/B comparison, not by a single run' }
    let value = metricOf(result, t.metric), detail = ''
    if (t.ratioTo) {
      const ref = all.find((c) => c.caseId === t.ratioTo), rv = metricOf(ref, t.metric)
      if (!ref) { detail = `reference ${t.ratioTo} not in this run`; value = undefined }
      else if (typeof value !== 'number' || typeof rv !== 'number' || !rv) { detail = `reference ${t.ratioTo} has no ${t.metric}`; value = undefined }
      else { detail = `${t.metric} ${f(value, 2)} / ${f(rv, 2)} (${t.ratioTo})`; value = value / rv }
    }
    const f1 = cmp[t.op]
    if (!f1) return { ...out, value: null, status: 'unmeasured', detail: `unknown op ${t.op}` }
    if (value === undefined || value === null || (typeof value === 'number' && Number.isNaN(value))) return { ...out, value: null, status: 'unmeasured', detail: detail || `${t.metric} not recorded` }
    return { ...out, value, status: f1(value, t.limit) ? 'pass' : 'fail', detail }
  })
}
export function thresholdSummary(cases) {
  const c = { pass: 0, fail: 0, unmeasured: 0, deferred: 0 }
  for (const r of cases) for (const t of r.thresholds ?? []) c[t.status] = (c[t.status] ?? 0) + 1
  return c
}
export function thresholdLines(cases) {
  const L = []
  for (const r of cases) for (const t of r.thresholds ?? []) {
    const v = t.value == null ? '-' : typeof t.value === 'number' ? f(t.value, 2) : String(t.value)
    const tag = t.status.toUpperCase()
    L.push(`- ${tag} ${r.caseId}: ${t.name ?? t.metric} = ${v}${t.unit && typeof t.value === 'number' ? ' ' + t.unit : ''} (want ${t.metric}${t.ratioTo ? ' ratio' : ''} ${t.op} ${t.limit}${t.unit ? ' ' + t.unit : ''}${t.proposed ? ', proposed' : ''})${t.detail ? ` [${t.detail}]` : ''}${t.note ? ` -- ${t.note}` : ''}`)
  }
  return L
}

export function headlineTable(cases) {
  const rows = ['| case | n | err | p50 ms | p95 ms | max ms | fsyncs/op p50 | lease hold p95 ms | queue wait p95 ms |', '|---|---:|---:|---:|---:|---:|---:|---:|---:|']
  for (const c of cases) {
    const s = c.server
    rows.push(`| ${c.caseId} | ${c.n} | ${c.errors} | ${f(c.ms.p50)} | ${f(c.ms.p95)} | ${f(c.ms.max)} | ${s ? f(s.fsyncsPerOp.p50, 0) : '-'} | ${s ? f(s.leaseHoldMs.p95) : '-'} | ${s ? f(s.queueWaitMs.p95) : '-'} |`)
  }
  return rows.join('\n')
}

export function summaryMd(run) {
  const L = []
  L.push(`# Run ${run.runId}`, '')
  L.push(`- label \`${run.label}\`, mode ${run.mode}, suite ${run.suite}, tier ${run.tier}, seed ${run.seed}, protocol v${run.protocolVersion}`)
  L.push(`- started ${run.startedAt}, finished ${run.finishedAt}`)
  L.push(`- target: ${run.target.version}, sha256 ${String(run.target.binarySha256 ?? '').slice(0, 12)}, spans ${run.target.spans}`)
  L.push(`- env: ${run.env.cpu}, ${run.env.ncpu} cpu, ${run.env.memGB} GB, power ${run.env.power}, disk free ${run.env.diskFreeGB} GB, node ${run.env.node}`)
  L.push(`- load before ${run.env.loadBefore.join(' ')}; after ${run.env.loadAfter.join(' ')}`)
  const m = run.env.machine
  if (m) L.push(`- machine: ${m.health?.clean ? 'clean' : `**NOT CLEAN** (${(m.health?.why ?? []).join('; ')})`}; awake held ${m.awakeHeld}; foreign CPU mean ${m.foreignCpu?.foreignPct?.mean ?? '-'}% p95 ${m.foreignCpu?.foreignPct?.p95 ?? '-'}% of ${m.foreignCpu?.cores ?? '?'}00% (top: ${(m.foreignCpu?.top ?? []).slice(0, 4).map((t) => `${t.comm} ${t.meanPct}%`).join(', ') || '-'})`)
  else L.push('- machine: not recorded (predates power accounting)')
  L.push(`- other gardends (not ours): ${run.env.otherGardends.map((g) => `pid ${g.pid}${g.port ? ` :${g.port}` : ''}`).join(', ') || 'none'}`)
  L.push(`- fixture: ${JSON.stringify(run.fixture)}`)
  L.push(`- process: ${JSON.stringify(run.process)}`)
  if (run.spanJoin) L.push(`- span join: ${run.spanJoin.callsJoined}/${run.spanJoin.callsTotal} calls joined (${(run.spanJoin.fraction * 100).toFixed(1)}%); ${run.spanJoin.spansTotal} spans read, ${run.spanJoin.spansJoined} in call traces, ${run.spanJoin.spansUnjoined} unjoined (background)`)
  L.push('', '## Headline', '', headlineTable(run.cases), '')
  const slow = [...run.cases].filter((c) => c.kind !== 'boot').sort((a, b) => b.ms.p50 - a.ms.p50).slice(0, 5)
  L.push('## Slowest five by p50: where the time goes (self time, summed over the case\'s calls)', '')
  for (const c of slow) {
    L.push(`### ${c.caseId} (p50 ${f(c.ms.p50)} ms, p95 ${f(c.ms.p95)} ms)`)
    if (c.server) L.push('', c.server.topSelfTime.slice(0, 6).map((t) => `- ${t.name}: ${f(t.selfMsTotal / Math.max(1, c.n), 1)} ms/op self (n ${t.n})`).join('\n'))
    if (c.exemplars?.[0]) L.push('', 'slowest call tree:', '```', renderTree(c.exemplars[0], 16), '```')
    L.push('')
  }
  { const tl = thresholdLines(run.cases); if (tl.length) { const sm = thresholdSummary(run.cases); L.push(`## Thresholds (${sm.pass} pass, ${sm.fail} fail, ${sm.unmeasured} unmeasured, ${sm.deferred} deferred to A/B)`, '', ...tl, '') } }
  if (run.spanJoin?.unjoinedTop?.length) L.push('## Unjoined (background) spans, by total ms', '', run.spanJoin.unjoinedTop.map((t) => `- ${t.name}: n ${t.n}, ${f(t.totalMs, 0)} ms`).join('\n'), '')
  if (run.notes?.length) L.push('## Notes', '', run.notes.map((n) => `- ${n}`).join('\n'), '')
  return L.join('\n')
}

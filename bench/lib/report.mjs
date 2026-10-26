import { renderTree } from './spans.mjs'

const f = (x, d = 1) => (x == null || Number.isNaN(x) ? '-' : Number(x).toFixed(d).replace(/\.0+$/, ''))
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
  if (run.spanJoin?.unjoinedTop?.length) L.push('## Unjoined (background) spans, by total ms', '', run.spanJoin.unjoinedTop.map((t) => `- ${t.name}: n ${t.n}, ${f(t.totalMs, 0)} ms`).join('\n'), '')
  if (run.notes?.length) L.push('## Notes', '', run.notes.map((n) => `- ${n}`).join('\n'), '')
  return L.join('\n')
}

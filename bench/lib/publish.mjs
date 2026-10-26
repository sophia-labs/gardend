// Emit the SCHEMA §4 ledger triples for a run, load them into graph `perf`, and write the summary document.
import { readFileSync, existsSync, writeFileSync } from 'node:fs'
import path from 'node:path'
import { Client, readKey } from './mcp.mjs'
import { summaryMd } from './report.mjs'

const B = 'https://sophia-labs.com/ns/bench#', XSD = 'http://www.w3.org/2001/XMLSchema#', RDF = 'http://www.w3.org/1999/02/22-rdf-syntax-ns#', RDFS = 'http://www.w3.org/2000/01/rdf-schema#'
const esc = (s) => String(s).replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/\n/g, '\\n').replace(/\r/g, '\\r').replace(/\t/g, '\\t')
const iriEsc = (s) => String(s).replace(/[<>"{}|^`\\\s]/g, (c) => '%' + c.charCodeAt(0).toString(16).toUpperCase().padStart(2, '0'))
const dec = (x) => (Number.isFinite(x) ? `"${Number(x.toFixed ? x.toFixed(4) : x).toString()}"^^<${XSD}decimal>` : null)
const int = (x) => (Number.isFinite(x) ? `"${Math.round(x)}"^^<${XSD}integer>` : null)

export function runTriples(run, { baseline = false } = {}) {
  const T = []
  const add = (s, p, o) => { if (o != null) T.push(`<${s}> <${p}> ${o} .`) }
  const lit = (x) => `"${esc(x)}"`
  const runIri = `urn:sophia:bench:run:${iriEsc(run.runId)}`
  add(runIri, RDF + 'type', `<${B}Run>`)
  add(runIri, B + 'runId', lit(run.runId)); add(runIri, B + 'label', lit(run.label))
  add(runIri, B + 'startedAt', `"${run.startedAt}"^^<${XSD}dateTime>`)
  add(runIri, B + 'mode', lit(run.mode)); add(runIri, B + 'suite', lit(run.suite)); add(runIri, B + 'tier', lit(run.tier))
  add(runIri, B + 'seed', int(run.seed)); add(runIri, B + 'protocolVersion', int(run.protocolVersion))
  add(runIri, B + 'gitRev', lit(run.target.gitRev)); add(runIri, B + 'version', lit(run.target.version))
  if (run.target.binarySha256) add(runIri, B + 'binarySha256', lit(run.target.binarySha256))
  add(runIri, B + 'host', lit(run.env.host)); add(runIri, B + 'loadBefore1m', dec(run.env.loadBefore[0]))
  if (run.abSitting) add(runIri, B + 'abSitting', lit(run.abSitting)); if (run.abPartner) add(runIri, B + 'abPartner', lit(run.abPartner))
  add(runIri, B + 'rssPeakMB', int(run.process.rssPeakMB)); add(runIri, B + 'isBaseline', `"${baseline}"^^<${XSD}boolean>`)
  for (const c of run.cases) {
    const caseIri = `urn:sophia:bench:case:${iriEsc(c.caseId)}@v${c.caseVersion}`, crIri = `${runIri}/case/${iriEsc(c.caseId)}`
    add(caseIri, RDF + 'type', `<${B}Case>`); add(caseIri, B + 'caseId', lit(c.caseId)); add(caseIri, B + 'caseVersion', int(c.caseVersion)); add(caseIri, B + 'kind', lit(c.kind)); add(caseIri, RDFS + 'label', lit(c.caseId))
    add(crIri, RDF + 'type', `<${B}CaseResult>`); add(crIri, B + 'run', `<${runIri}>`); add(crIri, B + 'case', `<${caseIri}>`)
    add(crIri, B + 'n', int(c.n)); add(crIri, B + 'errors', int(c.errors))
    add(crIri, B + 'meanMs', dec(c.ms.mean)); add(crIri, B + 'p50Ms', dec(c.ms.p50)); add(crIri, B + 'p95Ms', dec(c.ms.p95)); add(crIri, B + 'p99Ms', dec(c.ms.p99)); add(crIri, B + 'maxMs', dec(c.ms.max))
    if (c.server) {
      add(crIri, B + 'fsyncsPerOpP50', dec(c.server.fsyncsPerOp.p50)); add(crIri, B + 'leaseWaitP95Ms', dec(c.server.leaseWaitMs.p95))
      add(crIri, B + 'leaseHoldP95Ms', dec(c.server.leaseHoldMs.p95)); add(crIri, B + 'queueWaitP95Ms', dec(c.server.queueWaitMs.p95))
    }
    if (c.extra?.slope) add(crIri, B + 'slopeMsPer100', dec(c.extra.slope.msPerHundredDocs))
    c.exemplars?.forEach((ex, k) => {
      const exIri = `${crIri}/exemplar/${k + 1}`
      add(exIri, RDF + 'type', `<${B}Exemplar>`); add(exIri, B + 'caseResult', `<${crIri}>`); add(exIri, B + 'rank', int(k + 1)); add(exIri, B + 'traceId', lit(ex.traceId)); add(exIri, B + 'wallMs', dec(ex.wallMs))
      for (const s of ex.spans) {
        const sIri = `${exIri}/span/${s.spanId}`
        add(sIri, RDF + 'type', `<${B}Span>`); add(sIri, B + 'exemplar', `<${exIri}>`); add(sIri, B + 'spanId', lit(s.spanId)); add(sIri, B + 'parentSpanId', lit(s.parentSpanId)); add(sIri, B + 'name', lit(s.name))
        add(sIri, B + 'startOffsetMs', dec(s.startOffsetMs)); add(sIri, B + 'durationMs', dec(s.durationMs)); add(sIri, B + 'fsyncs', int(s.fsyncs)); add(sIri, B + 'attrsJson', lit(JSON.stringify(s.attrs)))
      }
    })
  }
  return T
}

export async function publish(runDir, { url, keyFile, baseline = false, log = console.error }) {
  const run = JSON.parse(readFileSync(path.join(runDir, 'run.json'), 'utf8'))
  const c = new Client({ url, key: readKey(keyFile) })
  const g = await c.json('POST', '/api/graphs', { title: 'perf', graph_id: 'perf' })
  if (g.status >= 300 && !/exist|already|conflict/i.test(g.text)) throw new Error(`create graph perf: ${g.status} ${c.redact(g.text.slice(0, 200))}`)
  const triples = runTriples(run, { baseline })
  let loaded = 0
  for (let i = 0; i < triples.length; i += 4000) {
    const r = await c.call('rdf_load', { graphId: 'perf', format: 'ntriples', targetGraphIri: 'urn:sophia:bench:ledger', data: triples.slice(i, i + 4000).join('\n') + '\n' })
    if (!r.ok) throw new Error('rdf_load failed: ' + r.error)
    loaded += Math.min(4000, triples.length - i)
  }
  // folder + summary doc + protocol doc
  const fr = await c.call('create_folder', { graphId: 'perf', name: 'Runs', folderId: 'perf-runs' }); if (!fr.ok && !/exist|already/i.test(fr.error ?? '')) log('folder:', fr.error)
  const docId = `perf-run-${run.runId}`
  const cd = await c.call('create_document', { graphId: 'perf', title: `Run ${run.runId}`, documentId: docId, parentId: 'perf-runs', ifExists: 'keep' })
  if (!cd.ok) throw new Error('create run doc: ' + cd.error)
  const wd = await c.call('write_document', { graphId: 'perf', documentId: docId, content: summaryMd(run), format: 'markdown' })
  if (!wd.ok) throw new Error('write run doc: ' + wd.error)
  const protoFile = new URL('../PROTOCOL.md', import.meta.url).pathname
  if (existsSync(protoFile)) {
    const pd = await c.call('create_document', { graphId: 'perf', title: 'gbench protocol', documentId: 'perf-gbench-protocol', ifExists: 'keep' })
    if (pd.ok) await c.call('write_document', { graphId: 'perf', documentId: 'perf-gbench-protocol', content: readFileSync(protoFile, 'utf8'), format: 'markdown' })
  }
  return { triples: loaded, runDoc: docId, baseline }
}

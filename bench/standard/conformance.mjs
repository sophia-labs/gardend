// `gbench standard conformance --endpoint <query url> [--update <url>] [--gsp <url>] [--auth ..] [--label l] [--only <regex>] [--ignore file]`
// W3C SPARQL 1.1 (w3c/rdf-tests sparql/sparql11): Query evaluation + syntax, Update evaluation + syntax, Protocol, Graph Store Protocol,
// CSV/TSV results, run through plain HTTP against one endpoint. Isolation: before each test the store is emptied (DROP ALL, falling back
// to GSP deletes), the test's data is loaded as default/named graphs (GSP PUT, else INSERT DATA), then the operation runs and results are
// compared as RDF terms: solution multisets with blank-node isomorphism (ordered when the query has ORDER BY), graphs by isomorphism.
//
// Data and manifest IRIs are rebased to http://www.w3.org/2009/sparql/docs/tests/data-sparql11/ (the base the expected results use),
// every data file is parsed here and sent as N-Triples, and queries/updates get a BASE line, so the server never resolves file IRIs.
//
// Not run, counted as "skipped" with a reason: entailment regimes (no reasoning), federated SERVICE tests (need peer endpoints),
// service-description (not part of the protocol surface we publish), direct graph identification GSP tests (needs --gsp-direct).
// rdf-test-suite (npm 2.9.0) was evaluated: it drives an in-process JS engine object, not an HTTP endpoint, so it would need a
// per-test store-isolation adapter anyway; this runner is that adapter plus the comparison, ~400 lines, with no engine assumptions.
import { readFileSync, existsSync, writeFileSync } from 'node:fs'
import { Readable } from 'node:stream'
import path from 'node:path'
import { Parser as N3Parser, Writer as N3Writer, Store as N3Store, DataFactory } from 'n3'
import { isomorphic } from 'rdf-isomorphic'
import { RdfXmlParser } from 'rdfxml-streaming-parser'
import { SparqlJsonParser } from 'sparqljson-parse'
import { SparqlXmlParser } from 'sparqlxml-parse'
import { SRC, exec, log, authHeader, mkdirp, writeJson, readJson, CACHE } from './lib/common.mjs'
import { withMachine } from './lib/machine.mjs'

const { namedNode, blankNode, literal, quad, defaultGraph } = DataFactory
const RT = path.join(SRC, 'rdf-tests'), S11 = path.join(RT, 'sparql', 'sparql11')
const BASE = 'http://www.w3.org/2009/sparql/docs/tests/data-sparql11/'
const NS = {
  mf: 'http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#', qt: 'http://www.w3.org/2001/sw/DataAccess/tests/test-query#',
  ut: 'http://www.w3.org/2009/sparql/tests/test-update#', rdf: 'http://www.w3.org/1999/02/22-rdf-syntax-ns#', rdfs: 'http://www.w3.org/2000/01/rdf-schema#',
  ht: 'http://www.w3.org/2011/http#', cnt: 'http://www.w3.org/2011/content#', rs: 'http://www.w3.org/2001/sw/DataAccess/tests/result-set#',
  hts: 'http://www.w3.org/2011/http-statusCodes#',
}
const P = (ns, l) => NS[ns] + l
const toPath = (iri) => (iri.startsWith(BASE) ? path.join(S11, iri.slice(BASE.length).split('#')[0]) : null)
const toIri = (file) => BASE + path.relative(S11, file).split(path.sep).join('/')

export async function ensureTests() {
  if (!existsSync(S11)) {
    mkdirp(SRC)
    await exec('git', ['clone', '-q', '--depth', '1', '--filter=blob:none', '--sparse', 'https://github.com/w3c/rdf-tests.git', RT])
    await exec('git', ['-C', RT, 'sparse-checkout', 'set', 'sparql'])
  }
  return (await exec('git', ['-C', RT, 'rev-parse', 'HEAD'])).out.trim()
}

// ---------- manifests ----------
function parseTtl(file) {
  const iri = toIri(file), store = new N3Store()
  store.addQuads(new N3Parser({ baseIRI: iri, format: 'text/turtle' }).parse(readFileSync(file, 'utf8')))
  return { iri, store }
}
const one = (st, s, p) => st.getObjects(s, p, null)[0] ?? null
function list(st, head) { const out = []; let n = head; while (n && n.value !== P('rdf', 'nil')) { out.push(one(st, n, P('rdf', 'first'))); n = one(st, n, P('rdf', 'rest')) } return out }

function collect(manifestFile, group = null, seen = new Set()) {
  if (seen.has(manifestFile)) return []
  seen.add(manifestFile)
  const { iri, store } = parseTtl(manifestFile), man = namedNode(iri), tests = []
  const dir = path.basename(path.dirname(manifestFile)), g = group ?? dir
  for (const inc of store.getObjects(man, P('mf', 'include'), null)) for (const i of list(store, inc)) {
    const f = toPath(i.value); if (f && existsSync(f)) tests.push(...collect(f, path.dirname(f) === S11 ? path.basename(f, '.ttl') : path.basename(path.dirname(f)), seen))
  }
  for (const ent of store.getObjects(man, P('mf', 'entries'), null)) for (const t of list(store, ent)) tests.push(describe(store, t, g, manifestFile))
  return tests
}
function describe(st, t, group, manifestFile) {
  const type = one(st, t, P('rdf', 'type'))?.value.split('#')[1], t0 = { id: t.value, name: one(st, t, P('mf', 'name'))?.value ?? t.value.split('#')[1], type, group, st, node: t }
  t0.requires = st.getObjects(t, P('mf', 'requires'), null).map((x) => x.value.split('#')[1])
  return t0
}

// ---------- data ----------
async function parseRdfFile(file, iri) {
  const text = readFileSync(file, 'utf8'), ext = path.extname(file)
  if (ext === '.rdf') {
    return new Promise((res, rej) => { const q = [], p = new RdfXmlParser({ baseIRI: iri }); p.on('data', (x) => q.push(x)).on('error', rej).on('end', () => res(q)); p.write(text); p.end() })
  }
  return new N3Parser({ baseIRI: iri, format: ext === '.nt' ? 'application/n-triples' : ext === '.trig' ? 'application/trig' : 'text/turtle' }).parse(text)
}
function toNT(quads) { const w = new N3Writer({ format: 'N-Triples' }); w.addQuads(quads.map((q) => quad(q.subject, q.predicate, q.object))); let out; w.end((e, r) => (out = r)); return out }

export class Client {
  constructor({ query, update, gsp, headers }) { this.query = query; this.update = update; this.gsp = gsp; this.headers = headers }
  async http(method, url, { headers = {}, body, accept } = {}) {
    try {
      const r = await fetch(url, { method, headers: { ...(accept ? { accept } : {}), ...headers, ...this.headers }, body, redirect: 'manual', signal: AbortSignal.timeout(60000) })
      const buf = Buffer.from(await r.arrayBuffer())
      return { status: r.status, headers: r.headers, text: buf.toString('utf8'), buf }
    } catch (e) { return { status: 0, headers: new Headers(), text: '', error: String(e.cause ?? e) } }
  }
  runQuery(q, accept) { return this.http('POST', this.query, { headers: { 'content-type': 'application/sparql-query' }, body: q, accept }) }
  runUpdate(u) { return this.http('POST', this.update, { headers: { 'content-type': 'application/sparql-update' }, body: u }) }
  async clear() {
    const r = await this.runUpdate('DROP ALL')
    if (r.status >= 200 && r.status < 300) return
    // fallback: delete the default graph and every named graph through GSP
    if (!this.gsp) throw new Error(`cannot clear store: DROP ALL -> ${r.status} ${r.text.slice(0, 100)}`)
    const gs = await this.runQuery('SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }', 'application/sparql-results+json')
    await this.http('DELETE', `${this.gsp}?default`)
    for (const b of JSON.parse(gs.text).results.bindings) await this.http('DELETE', `${this.gsp}?graph=${encodeURIComponent(b.g.value)}`)
  }
  async load(graph, nt) {
    if (!nt.trim()) return
    if (this.gsp) {
      const r = await this.http('PUT', `${this.gsp}?${graph ? 'graph=' + encodeURIComponent(graph) : 'default'}`, { headers: { 'content-type': 'application/n-triples' }, body: nt })
      if (r.status >= 300) throw new Error(`GSP load -> ${r.status} ${r.text.slice(0, 100)}`)
    } else {
      const body = graph ? `INSERT DATA { GRAPH <${graph}> {\n${nt}\n} }` : `INSERT DATA {\n${nt}\n}`
      const r = await this.runUpdate(body); if (r.status >= 300) throw new Error(`INSERT DATA load -> ${r.status} ${r.text.slice(0, 100)}`)
    }
  }
}

// ---------- comparison ----------
const parseJsonRes = new SparqlJsonParser()
async function parseSolutions(text, contentType) {
  if (/json/.test(contentType)) { const j = JSON.parse(text); return j.boolean !== undefined ? { boolean: j.boolean } : { vars: j.head?.vars ?? [], rows: parseJsonRes.parseJsonResults(j) } }
  if (/xml/.test(contentType) || text.trimStart().startsWith('<')) {
    if (/<boolean>/.test(text)) return { boolean: /<boolean>\s*true/.test(text) }
    const rows = await new Promise((res, rej) => { const out = []; new SparqlXmlParser().parseXmlResultsStream(Readable.from([text])).on('data', (b) => out.push(b)).on('error', rej).on('end', () => res(out)) })
    const vars = [...text.matchAll(/<variable\s+name=["']([^"']+)/g)].map((m) => m[1])
    return { vars, rows }
  }
  throw new Error(`unsupported results content-type ${contentType}`)
}
// expected result set encoded as RDF (rs:ResultSet)
function rsToSolutions(quads) {
  const st = new N3Store(quads), set = st.getSubjects(P('rdf', 'type'), P('rs', 'ResultSet'), null)[0]
  if (!set) return null
  const b = st.getObjects(set, P('rs', 'boolean'), null)[0]; if (b) return { boolean: b.value === 'true' }
  const vars = st.getObjects(set, P('rs', 'resultVariable'), null).map((x) => x.value)
  const rows = st.getObjects(set, P('rs', 'solution'), null).map((sol) => { const row = {}; for (const bd of st.getObjects(sol, P('rs', 'binding'), null)) row[one(st, bd, P('rs', 'variable')).value] = one(st, bd, P('rs', 'value')); return row })
  return { vars, rows }
}
const rowKey = (row, vars) => vars.map((v) => (row[v] ? termKey(row[v]) : '-')).join('\t')
const termKey = (t) => (t.termType === 'BlankNode' ? '_:*' : t.termType === 'NamedNode' ? `<${t.value}>` : t.termType === 'Literal' ? `"${t.value}"^^<${t.datatype?.value}>@${t.language ?? ''}` : t.value)
function solutionsGraph(sol) {
  const quads = [], rs = (l) => namedNode(NS.rs + l), root = blankNode('root'); let i = 0
  for (const row of sol.rows) {
    const s = blankNode(`sol${i++}`); quads.push(quad(root, rs('solution'), s))
    for (const [v, t] of Object.entries(row)) { const b = blankNode(`b${i++}`); quads.push(quad(s, rs('binding'), b), quad(b, rs('variable'), literal(v)), quad(b, rs('value'), t.termType === 'BlankNode' ? blankNode('x' + t.value) : t)) }
  }
  return quads
}
function sameSolutions(exp, act, ordered) {
  if (exp.boolean !== undefined || act.boolean !== undefined) return exp.boolean === act.boolean ? null : `ASK expected ${exp.boolean}, got ${act.boolean}`
  if (exp.rows.length !== act.rows.length) return `row count: expected ${exp.rows.length}, got ${act.rows.length}`
  const vars = [...new Set([...exp.rows.flatMap(Object.keys), ...act.rows.flatMap(Object.keys)])].sort()
  if (ordered) {
    const map = new Map(), rev = new Map()
    for (let i = 0; i < exp.rows.length; i++) for (const v of vars) {
      const e = exp.rows[i][v], a = act.rows[i][v]
      if (!e && !a) continue
      if (!e || !a) return `row ${i} var ${v}: expected ${e ? termKey(e) : 'unbound'}, got ${a ? termKey(a) : 'unbound'}`
      if (e.termType === 'BlankNode' && a.termType === 'BlankNode') { if ((map.get(e.value) ?? a.value) !== a.value || (rev.get(a.value) ?? e.value) !== e.value) return `row ${i}: blank node mapping conflict`; map.set(e.value, a.value); rev.set(a.value, e.value) }
      else if (!e.equals(a)) return `row ${i} var ${v}: expected ${termKey(e)}, got ${termKey(a)}`
    }
    return null
  }
  // unordered: cheap multiset pre-check (bnodes wildcarded), then isomorphism for exactness with blank nodes
  const ca = new Map(); for (const r of exp.rows) { const k = rowKey(r, vars); ca.set(k, (ca.get(k) ?? 0) + 1) }
  for (const r of act.rows) { const k = rowKey(r, vars); const n = ca.get(k); if (!n) return `unexpected row ${k}`; ca.set(k, n - 1) }
  return isomorphic(solutionsGraph(exp), solutionsGraph(act)) ? null : 'solutions differ (blank node structure)'
}
const graphsEqual = (a, b) => isomorphic(a.map((q) => quad(q.subject, q.predicate, q.object)), b.map((q) => quad(q.subject, q.predicate, q.object)))

async function expectedSolutions(file, iri) {
  const ext = path.extname(file)
  if (ext === '.srx') return parseSolutions(readFileSync(file, 'utf8'), 'xml')
  if (ext === '.srj') return parseSolutions(readFileSync(file, 'utf8'), 'json')
  const quads = await parseRdfFile(file, iri), sol = rsToSolutions(quads)
  return sol ?? { graph: quads }
}
const canonCsv = (s) => s.replace(/\r\n/g, '\n').trimEnd()

// ---------- test execution ----------
async function loadDataset(cl, dataIris, graphIris) {
  for (const iri of dataIris) { const f = toPath(iri); await cl.load(null, toNT(await parseRdfFile(f, iri))) }
  for (const [iri, name] of graphIris) { const f = toPath(iri); await cl.load(name ?? iri, toNT(await parseRdfFile(f, iri))) }
}
function fromGraphs(queryText, base) {
  const out = []
  for (const m of queryText.matchAll(/\bFROM\s+(?:NAMED\s+)?<([^>]+)>/gi)) { try { const iri = new URL(m[1], base).href; if (toPath(iri) && existsSync(toPath(iri))) out.push(iri) } catch { /* ignore */ } }
  return [...new Set(out)]
}
const withBase = (text, base) => (/^\s*(#[^\n]*\n\s*)*BASE\s*</im.test(text.split('\n').slice(0, 30).join('\n')) ? text : `BASE <${base}>\n${text}`)

async function runTest(cl, t, opts) {
  const st = t.st, node = t.node, action = one(st, node, P('mf', 'action')), result = one(st, node, P('mf', 'result'))
  const type = t.type
  if (/Entailment/.test(type) || t.group === 'entailment') return { skip: 'entailment regimes (no reasoning)' }
  if (t.group === 'service' || type === 'ServiceDescriptionTest' || t.group === 'service-description') return { skip: t.group === 'service' ? 'federated SERVICE (needs peer endpoints)' : 'service description not covered' }
  if (t.requires.includes('DirectGraphIdentification') && !opts.gspDirect) return { skip: 'GSP direct graph identification (pass --gsp-direct)' }
  if (t.requires.includes('POSTGraphCreation') && !opts.gspPost) { /* run it anyway; location templating is implemented */ }

  if (/^(Positive|Negative)(Update)?Syntax/.test(type)) {
    const f = toPath(action.value), text = readFileSync(f, 'utf8'), isUpdate = /Update/.test(type), neg = /^Negative/.test(type)
    await cl.clear()
    const r = isUpdate ? await cl.runUpdate(withBase(text, toIri(f))) : await cl.runQuery(withBase(text, toIri(f)), 'application/sparql-results+json')
    if (neg) return r.status >= 400 && r.status < 500 ? { pass: true } : { fail: `negative syntax test accepted: HTTP ${r.status}` }
    return r.status !== 400 && r.status !== 0 ? { pass: true } : { fail: `positive syntax test rejected: HTTP ${r.status} ${r.text.slice(0, 160)}` }
  }
  if (type === 'ProtocolTest' || type === 'GraphStoreProtocolTest') return runHttpTest(cl, t, opts)

  if (type === 'QueryEvaluationTest' || type === 'CSVResultFormatTest') {
    const q = one(st, action, P('qt', 'query')).value, qf = toPath(q), text = readFileSync(qf, 'utf8')
    const data = st.getObjects(action, P('qt', 'data'), null).map((x) => x.value), graphData = st.getObjects(action, P('qt', 'graphData'), null).map((x) => [x.value, null])
    for (const g of fromGraphs(text, q)) if (!graphData.some(([i]) => i === g) && !data.includes(g)) graphData.push([g, null])
    await cl.clear(); await loadDataset(cl, data, graphData)
    const ordered = /\bORDER\s+BY\b/i.test(text), resFile = toPath(result.value), ext = path.extname(resFile)
    if (type === 'CSVResultFormatTest') {
      const accept = ext === '.tsv' ? 'text/tab-separated-values' : 'text/csv', r = await cl.runQuery(withBase(text, q), accept)
      if (r.status !== 200) return { fail: `HTTP ${r.status} ${r.text.slice(0, 160)}` }
      return canonCsv(r.text) === canonCsv(readFileSync(resFile, 'utf8')) ? { pass: true } : { fail: `${accept} body differs`, actual: r.text.slice(0, 300) }
    }
    const exp = await expectedSolutions(resFile, result.value)
    const isGraph = !!exp.graph
    const r = await cl.runQuery(withBase(text, q), isGraph ? 'application/n-triples' : 'application/sparql-results+json')
    if (r.status !== 200) return { fail: `HTTP ${r.status} ${r.text.slice(0, 200)}` }
    if (isGraph) {
      const act = new N3Parser({ format: 'application/n-triples' }).parse(r.text)
      return graphsEqual(exp.graph, act) ? { pass: true } : { fail: `graphs differ: expected ${exp.graph.length} triples, got ${act.length}` }
    }
    const act = await parseSolutions(r.text, r.headers.get('content-type') ?? 'json'), why = sameSolutions(exp, act, ordered)
    return why ? { fail: why } : { pass: true }
  }

  if (type === 'UpdateEvaluationTest') {
    const req = one(st, action, P('ut', 'request')).value, rf = toPath(req), text = readFileSync(rf, 'utf8')
    const data = st.getObjects(action, P('ut', 'data'), null).map((x) => x.value)
    const gd = st.getObjects(action, P('ut', 'graphData'), null).map((g) => [one(st, g, P('ut', 'graph')).value, one(st, g, P('rdfs', 'label'))?.value])
    await cl.clear(); await loadDataset(cl, data, gd)
    const r = await cl.runUpdate(withBase(text, req))
    if (r.status >= 300 || r.status < 200) return { fail: `update HTTP ${r.status} ${r.text.slice(0, 200)}` }
    const eData = st.getObjects(result, P('ut', 'data'), null).map((x) => x.value)
    const eGd = st.getObjects(result, P('ut', 'graphData'), null).map((g) => [one(st, g, P('ut', 'graph')).value, one(st, g, P('rdfs', 'label'))?.value])
    const dump = async (graph) => {
      const q = graph ? `CONSTRUCT { ?s ?p ?o } WHERE { GRAPH <${graph}> { ?s ?p ?o } }` : 'CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }'
      const x = await cl.runQuery(q, 'application/n-triples'); if (x.status !== 200) throw new Error(`dump -> ${x.status}`); return new N3Parser({ format: 'application/n-triples' }).parse(x.text)
    }
    const expDef = (await Promise.all(eData.map((i) => parseRdfFile(toPath(i), i)))).flat()
    const actDef = await dump(null)
    if (!graphsEqual(expDef, actDef)) return { fail: `default graph differs: expected ${expDef.length} triples, got ${actDef.length}` }
    const names = new Set(eGd.map(([i, n]) => n ?? i))
    for (const [i, n] of eGd) { const e = await parseRdfFile(toPath(i), i), a = await dump(n ?? i); if (!graphsEqual(e, a)) return { fail: `graph <${n ?? i}> differs: expected ${e.length}, got ${a.length}` } }
    const gl = await cl.runQuery('SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }', 'application/sparql-results+json')
    const have = JSON.parse(gl.text).results.bindings.map((b) => b.g.value).filter((g) => !names.has(g))
    return have.length ? { fail: `unexpected extra graph(s): ${have.slice(0, 3).join(', ')}` } : { pass: true }
  }
  return { skip: `test type ${type} not implemented` }
}

// ---- HTTP-described tests (Protocol, Graph Store Protocol) ----
const STATUS = { OK: 200, Created: 201, Accepted: 202, NoContent: 204, SeeOther: 303, BadRequest: 400, Unauthorized: 401, NotFound: 404, MethodNotAllowed: 405, NotAcceptable: 406, Conflict: 409, UnsupportedMediaType: 415 }
function statusOk(st, expected, code) {
  if (!expected.length) return true
  return expected.some((e) => { const l = e.value.split('#')[1], m = l.match(/^StatusCode(\d)xx$/); return m ? Math.floor(code / 100) === +m[1] : STATUS[l] === code })
}
async function runHttpTest(cl, t, opts) {
  const st = t.st, action = one(st, t.node, P('mf', 'action'))
  await cl.clear()
  for (const g of st.getObjects(t.node, P('ut', 'graphData'), null)) {
    const gi = one(st, g, P('ut', 'graph')).value, label = one(st, g, P('rdfs', 'label'))?.value
    await cl.load(label ?? gi, toNT(await parseRdfFile(toPath(gi), gi)))
  }
  const reqs = list(st, one(st, action, P('ht', 'requests'))), vars = {}
  const sub = (s) => s.replace(/\$LOCATION\$/g, vars.LOCATION ?? '$LOCATION$')
  for (const [n, r] of reqs.entries()) {
    const method = one(st, r, P('ht', 'methodName')).value, ap = sub(one(st, r, P('ht', 'absolutePath'))?.value ?? '')
    const headers = {}; const hl = one(st, r, P('ht', 'headers')); if (hl) for (const h of list(st, hl)) headers[one(st, h, P('ht', 'fieldName')).value.toLowerCase()] = one(st, h, P('ht', 'fieldValue')).value
    const bodyNode = one(st, r, P('ht', 'body')); let body
    if (bodyNode) {
      const chars = sub(one(st, bodyNode, P('cnt', 'chars')).value), enc = one(st, bodyNode, P('cnt', 'characterEncoding'))?.value ?? 'UTF-8'
      body = /UTF-16/i.test(enc) ? Buffer.from(chars, 'utf16le') : Buffer.from(chars, 'utf8')
    }
    let url
    if (/^https?:/.test(ap)) url = ap
    else if (ap.startsWith('/gsp')) { if (!cl.gsp) return { skip: 'no --gsp endpoint' }; let rest = ap.slice(4); if (rest.startsWith('/') && !rest.startsWith('/?') && opts.gspDirect) url = new URL(rest, cl.gsp).href; else url = cl.gsp + (rest.startsWith('/?') ? rest.slice(1) : rest) }
    else {
      const rest = ap.replace(/^\/sparql\/?/, ''), ct = headers['content-type'] ?? '', bodyText = body?.toString('utf8') ?? ''
      const isUpdate = /sparql-update/.test(ct) || /^update=/.test(bodyText) || /[?&]update=/.test(rest)
      const base = isUpdate ? cl.update : cl.query
      if (!base) return { skip: isUpdate ? 'no --update endpoint' : 'no query endpoint' }
      url = base + (rest.startsWith('?') ? (base.includes('?') ? '&' + rest.slice(1) : rest) : rest)
    }
    const resp = one(st, r, P('ht', 'resp'))
    if (!headers.accept && resp) headers.accept = 'application/sparql-results+json,application/sparql-results+xml;q=0.9,text/turtle;q=0.8,application/n-triples;q=0.7,application/rdf+xml;q=0.6,text/plain;q=0.1'
    const x = await cl.http(method, url, { headers, body })
    if (x.error) return { fail: `request ${n + 1}: ${x.error}` }
    if (!resp) continue
    const expStatus = st.getObjects(resp, P('mf', 'expectedStatus'), null)
    if (!statusOk(st, expStatus, x.status)) return { fail: `request ${n + 1} ${method} ${ap.slice(0, 60)}: HTTP ${x.status}, expected ${expStatus.map((e) => e.value.split('#')[1]).join('|')} ${x.text.slice(0, 120)}` }
    const loc = one(st, resp, P('mf', 'expectedLocation'))
    if (loc) { const l = x.headers.get('location'); if (!l) return { fail: `request ${n + 1}: no Location header` }; vars.LOCATION = new URL(l, cl.gsp ?? url).href }
    const eb = one(st, resp, P('mf', 'expectedBoolean'))
    if (eb) { try { const sol = await parseSolutions(x.text, x.headers.get('content-type') ?? ''); if (sol.boolean !== (eb.value === 'true')) return { fail: `request ${n + 1}: ASK expected ${eb.value}, got ${sol.boolean}` } } catch (e) { return { fail: `request ${n + 1}: unparseable ASK result (${e.message}) content-type ${x.headers.get('content-type')}` } } }
    const ef = one(st, resp, P('mf', 'expectedFormat'))?.value, ct = x.headers.get('content-type') ?? ''
    if (ef === 'tabular' && !/sparql-results|csv|tab-separated/.test(ct)) return { fail: `request ${n + 1}: expected tabular results, content-type ${ct}` }
    if (ef === 'RDF' && !/turtle|n-triples|rdf\+xml|n-quads|trig|ld\+json/.test(ct)) return { fail: `request ${n + 1}: expected RDF, content-type ${ct}` }
    if (ef === 'boolean' && !/sparql-results/.test(ct)) return { fail: `request ${n + 1}: expected boolean results, content-type ${ct}` }
    const rb = one(st, resp, P('ht', 'body'))
    if (rb) {
      const exp = new N3Parser({ format: 'text/turtle' }).parse(one(st, rb, P('cnt', 'chars')).value)
      let act; try { act = new N3Parser({ format: /n-triples/.test(ct) ? 'application/n-triples' : 'text/turtle' }).parse(x.text) } catch (e) { return { fail: `request ${n + 1}: cannot parse response as ${ct}: ${e.message}` } }
      if (!graphsEqual(exp, act)) return { fail: `request ${n + 1}: response graph differs (expected ${exp.length} triples, got ${act.length})` }
    }
  }
  return { pass: true }
}

// ---------- driver ----------
const GROUPS = {
  query: ['aggregates', 'bind', 'bindings', 'cast', 'construct', 'exists', 'functions', 'grouping', 'negation', 'project-expression', 'property-path', 'subquery', 'syntax-query', 'syntax-fed', 'entailment', 'service', 'service-description'],
  results: ['csv-tsv-res', 'json-res'],
  update: ['add', 'basic-update', 'clear', 'copy', 'delete', 'delete-data', 'delete-insert', 'delete-where', 'drop', 'move', 'syntax-update-1', 'syntax-update-2', 'update-silent'],
  protocol: ['protocol'], 'graph-store': ['graph-store-protocol', 'http-rdf-update'],
}
const groupOf = (dir) => Object.entries(GROUPS).find(([, v]) => v.includes(dir))?.[0] ?? 'other'

export async function conformanceCmd(pos, o) {
  const rev = await ensureTests()
  const tests = collect(path.join(S11, 'manifest-all.ttl'))
  const only = o.only ? new RegExp(o.only) : null
  const ignore = o.ignore ? readJson(o.ignore, {}) : {}
  const cl = new Client({ query: o.endpoint, update: o.update, gsp: o.gsp, headers: authHeader(o) })
  if (!o.endpoint) throw new Error('--endpoint <query url> required (and --update, --gsp for those manifests)')
  const label = String(o.label ?? 'conformance').replace(/[^a-z0-9-]/gi, '-').toLowerCase()
  return withMachine(`conformance-${label}`, async () => {
    const results = [], t0 = Date.now()
    for (const [i, t] of tests.entries()) {
      if (only && !only.test(t.id)) continue
      let r
      try { r = await runTest(cl, t, { gspDirect: !!o['gsp-direct'] }) } catch (e) { r = { fail: `harness/exception: ${String(e.message ?? e).slice(0, 200)}` } }
      const status = r.pass ? 'pass' : r.skip ? 'skip' : 'fail'
      results.push({ id: t.id, name: t.name, type: t.type, dir: t.group, group: groupOf(t.group), status, reason: r.fail ?? r.skip, ...(r.actual ? { actual: r.actual } : {}) })
      if ((i + 1) % 200 === 0) log(`${i + 1}/${tests.length} tests`)
    }
    const agg = (rows) => { const pass = rows.filter((r) => r.status === 'pass').length, fail = rows.filter((r) => r.status === 'fail').length, skip = rows.filter((r) => r.status === 'skip').length; return { pass, fail, skip, run: pass + fail, passRate: pass + fail ? Math.round((pass / (pass + fail)) * 10000) / 100 : null } }
    const by = (key) => Object.fromEntries([...new Set(results.map((r) => r[key]))].sort().map((k) => [k, agg(results.filter((r) => r[key] === k))]))
    const failures = results.filter((r) => r.status === 'fail'), newFailures = failures.filter((r) => !(r.id in ignore))
    const report = { schema: 'sophia.bench.conformance.v1', label, finishedAt: new Date().toISOString(), seconds: Math.round((Date.now() - t0) / 1000), rdfTests: { repo: 'https://github.com/w3c/rdf-tests', rev, dir: 'sparql/sparql11' }, endpoints: { query: o.endpoint, update: o.update ?? null, gsp: o.gsp ?? null }, target: { name: o['target-name'] ?? 'unknown', version: o['target-version'] ?? 'unknown' }, total: agg(results), groups: by('group'), manifests: by('dir'), ignoreFile: o.ignore ?? null, failuresNotInIgnore: newFailures.length, results }
    const out = o.out ?? path.join(CACHE, 'conformance', `${label}.json`); mkdirp(path.dirname(out)); writeJson(out, report)
    if (o['write-ignore']) { const ig = {}; for (const f of failures) ig[f.id] = f.reason; writeJson(o['write-ignore'], ig) }
    console.log(`SPARQL 1.1 conformance (${label}): ${report.total.pass}/${report.total.run} run tests pass = ${report.total.passRate}% (${report.total.skip} skipped)`)
    for (const [k, v] of Object.entries(report.groups)) console.log(`  ${k.padEnd(12)} ${v.pass}/${v.run} = ${v.passRate}%  (skip ${v.skip})`)
    console.log(`report: ${out}${o.ignore ? `; ${newFailures.length} failures are not in the ignore list` : ''}`)
    return report
  })
}

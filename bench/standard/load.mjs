// `gbench standard load --endpoint <query url> --gsp <url> [--auth ..] --data <dir> [--store <name> --external]`
// Loads dataset.nt[.gz] into the store's DEFAULT graph.
//   GSP kind:      chunks of --chunk lines (default 250000); first chunk PUT ?default (replaces), the rest POST ?default.
//   external kind: the store's own bulk loader (Oxigraph `load`, tdb2.tdbloader, ...), see store.mjs. Recorded as such.
// Metric: triples/s over the whole phase (including the final ASK count check). Result goes to stdout and --out / the store's load.json.
import { createReadStream, writeFileSync } from 'node:fs'
import { createGunzip } from 'node:zlib'
import { createInterface } from 'node:readline'
import path from 'node:path'
import { readJson, writeJson, log, authHeader, dataDir, exec } from './lib/common.mjs'
import { withMachine, measured } from './lib/machine.mjs'
import { Endpoint } from './lib/sparql.mjs'
import { externalLoad, stateFile, readStore } from './store.mjs'

export async function countTriples(ep) {
  const j = await fetch(ep.queryUrl, { method: 'POST', headers: { 'content-type': 'application/sparql-query', accept: 'application/sparql-results+json', ...ep.headers }, body: 'SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }' }).then((x) => x.json())
  return Number(j.results.bindings[0].n.value)
}

async function gspLoad({ gsp, headers, file, chunk }) {
  const input = file.endsWith('.gz') ? createReadStream(file).pipe(createGunzip()) : createReadStream(file)
  const rl = createInterface({ input, crlfDelay: Infinity })
  let buf = [], n = 0, chunks = 0, first = true, bytes = 0
  const flush = async () => {
    if (!buf.length) return
    const body = buf.join('\n') + '\n'; bytes += body.length
    const r = await fetch(`${gsp}${gsp.includes('?') ? '&' : '?'}default`, { method: first ? 'PUT' : 'POST', headers: { 'content-type': 'application/n-triples', ...headers }, body })
    if (!r.ok) throw new Error(`GSP ${first ? 'PUT' : 'POST'} chunk ${chunks} -> HTTP ${r.status}: ${(await r.text()).slice(0, 300)}`)
    await r.arrayBuffer()
    first = false; chunks++; n += buf.length; buf = []
    if (chunks % 10 === 0) log(`loaded ${n} lines in ${chunks} chunks`)
  }
  for await (const line of rl) { if (!line || line[0] === '#') continue; buf.push(line); if (buf.length >= chunk) await flush() }
  await flush()
  return { kind: 'gsp', chunks, lines: n, bytes, method: 'PUT ?default then POST ?default, application/n-triples' }
}

export async function loadCmd(pos, o) {
  const bench = o.bench ?? 'bsbm', scale = o.scale
  const dir = o.data ?? dataDir(bench, scale)
  const manifest = readJson(path.join(dir, 'manifest.json'))
  if (!manifest) throw new Error(`no manifest.json in ${dir}; run \`standard data\` first`)
  const file = path.join(dir, manifest.dataset.name)
  const headers = authHeader(o)
  const ep = new Endpoint({ query: o.endpoint, headers })
  return withMachine(`load-${path.basename(dir)}`, async ({ awake } = {}) => {
    const t0 = Date.now()
    const { result, machine } = await measured(async () => {
      if (o.external) return externalLoad(o.store, [file], o)
      if (!o.gsp) throw new Error('--gsp <url> (or --external --store <name>) required')
      return gspLoad({ gsp: o.gsp, headers, file, chunk: Number(o.chunk ?? 250000) })
    }, { awake })
    const secs = (Date.now() - t0) / 1000
    const expected = manifest.dataset.triples
    let stored = null
    try { stored = await countTriples(ep) } catch (e) { stored = `count failed: ${e.message}` }
    const out = { dataset: path.basename(dir), file: manifest.dataset.name, sha256: manifest.dataset.sha256, expectedTriples: expected, storedTriples: stored, seconds: Math.round(secs * 10) / 10, triplesPerSec: Math.round(expected / secs), ...result, machine, finishedAt: new Date().toISOString() }
    const outFile = o.out ?? (o.store ? path.join(path.dirname(stateFile(o.store)), `${o.store}.load-${path.basename(dir)}.json`) : null)
    if (outFile) writeJson(outFile, out)
    console.log(JSON.stringify({ ...out, machine: machine.health }, null, 1))
    return out
  })
}

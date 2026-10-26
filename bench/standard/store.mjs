// `gbench standard store <oxigraph|fuseki|virtuoso|qlever|stardog|graphdb|rdfox> up|down|status|load`
//
// Fairness settings, identical for every store (override with flags; all recorded in the state file and in run.json):
//   --cpus 8 --memory 12g   container limits (Docker Desktop's VM has 18 GiB; stores get the same cap)
//   --timeout 60            per-query timeout, enforced by the CLIENT (run.mjs) so it is identical everywhere
//   reasoning/inference OFF everywhere; data goes in the store's DEFAULT graph (what BSBM/SP2B/WatDiv assume);
//   warm-up is the harness's (run.mjs --warmup); one store at a time; no server-side row caps (Virtuoso raised).
// Bulk loads use the store's own loader where it has one (kind=external), else GSP (kind=gsp).
import { existsSync, writeFileSync, readFileSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { CACHE, exec, log, readJson, writeJson, mkdirp, sleep } from './lib/common.mjs'
import { withMachine } from './lib/machine.mjs'

export const STATE_DIR = path.join(CACHE, 'stores')
const here = path.dirname(new URL(import.meta.url).pathname)
export const stateFile = (name) => path.join(STATE_DIR, `${name}.json`)
export const readStore = (name) => readJson(stateFile(name))

const PORTS = { oxigraph: 7878, fuseki: 3030, virtuoso: 8890, qlever: 7001, graphdb: 7200, stardog: 5820, rdfox: 12110 }
const cname = (n) => `sbench-${n}`

// Recipes. `image` is resolved to a digest at `up`. `load` returns the docker argv for an external bulk load.
export const RECIPES = {
  oxigraph: {
    status: 'publishable',
    image: process.env.OXIGRAPH_IMAGE ?? 'ghcr.io/oxigraph/oxigraph:v0.5.9', // the version gardend embeds (src-tauri/Cargo.toml)
    endpoints: (p) => ({ query: `http://127.0.0.1:${p}/query`, update: `http://127.0.0.1:${p}/update`, gsp: `http://127.0.0.1:${p}/store` }),
    notes: 'Exclusive default graph (no --union-default-graph). RocksDB uses the page cache: the container memory limit is the cache budget.',
    serve: (o) => ['--location', '/data', 'serve', '--bind', '0.0.0.0:7878'],
    load: (o, files) => ['--location', '/data', 'load', ...files.flatMap((f) => ['--file', f])],
    ready: (p) => `http://127.0.0.1:${p}/query?query=ASK%7B%7D`,
  },
  fuseki: {
    status: 'publishable (untested)',
    dockerfile: path.join(here, 'docker', 'Dockerfile.fuseki'), image: 'sophia-bench-fuseki:5.2.0',
    endpoints: (p) => ({ query: `http://127.0.0.1:${p}/ds/sparql`, update: `http://127.0.0.1:${p}/ds/update`, gsp: `http://127.0.0.1:${p}/ds/data` }),
    notes: 'TDB2, exclusive default graph (no tdb2:unionDefaultGraph), JVM heap = 60% of container memory, rest is OS cache. Load: tdb2.tdbloader (external).',
  },
  virtuoso: {
    status: 'publishable, open-source edition only (untested)',
    image: 'openlink/virtuoso-opensource-7:7.2.14',
    endpoints: (p) => ({ query: `http://127.0.0.1:${p}/sparql`, update: `http://127.0.0.1:${p}/sparql-auth`, gsp: `http://127.0.0.1:${p}/sparql-graph-crud-auth` }),
    notes: 'NumberOfBuffers/MaxDirtyBuffers sized to the memory limit; ResultSetMaxRows=1e9 and MaxQueryExecutionTime lifted so no silent truncation; no-FROM queries run over ALL graphs (union), so load into the default graph IRI and say so. Load: ld_dir + rdf_loader_run (external). Updates and GSP need digest auth (dba).',
  },
  qlever: {
    status: 'publishable, read mixes only (untested)',
    image: 'docker.io/adfreiburg/qlever:latest',
    endpoints: (p) => ({ query: `http://127.0.0.1:${p}/` }),
    notes: 'Read-mostly: run bsbm-explore / sp2b / watdiv only; no GSP, no full Update. Index with qlever-index (external).',
  },
  stardog: { status: 'INTERNAL ONLY: needs a license key and written permission to publish comparisons', image: 'stardog/stardog:latest', stub: true,
    endpoints: (p) => ({ query: `http://127.0.0.1:${p}/bench/query`, update: `http://127.0.0.1:${p}/bench/update`, gsp: `http://127.0.0.1:${p}/bench` }),
    notes: 'Mount a dir with stardog-license-key.bin at /var/opt/stardog. Create db with reasoning off, query.all.graphs=false (default graph only).' },
  graphdb: { status: 'INTERNAL ONLY: Free licence Art. 15.3 forbids publishing results without written permission', image: 'ontotext/graphdb:11.0.0', stub: true,
    endpoints: (p) => ({ query: `http://127.0.0.1:${p}/repositories/bench`, update: `http://127.0.0.1:${p}/repositories/bench/statements`, gsp: `http://127.0.0.1:${p}/repositories/bench/rdf-graphs/service` }),
    notes: 'Create repository with ruleset "empty" (no inference). Needs graphdb.license mounted at /opt/graphdb/home/conf.' },
  rdfox: { status: 'INTERNAL ONLY: evaluation licence s.2.2(v)/2.3 needs 30 days notice to OST before publishing', image: 'oxfordsemantic/rdfox:latest', stub: true,
    endpoints: (p) => ({ query: `http://127.0.0.1:${p}/datastores/bench/sparql`, update: `http://127.0.0.1:${p}/datastores/bench/sparql`, gsp: `http://127.0.0.1:${p}/datastores/bench/content` }),
    notes: 'Needs RDFox.lic. Datastore without Datalog rules.' },
}

async function digestOf(image) {
  const r = await exec('docker', ['image', 'inspect', image, '--format', '{{index .RepoDigests 0}}|{{.Id}}'], { allowFail: true })
  return r.code === 0 ? r.out.trim() : null
}
async function waitReady(url, secs = 120) {
  for (let i = 0; i < secs; i++) { try { const r = await fetch(url); if (r.status < 500) return true } catch { /* not yet */ } await sleep(1000) }
  return false
}

export async function storeCmd(pos, o) {
  const name = pos[0], action = pos[1], rec = RECIPES[name]
  if (!rec) throw new Error(`unknown store ${name}; known: ${Object.keys(RECIPES).join(', ')}`)
  mkdirp(STATE_DIR)
  const cpus = o.cpus ?? '8', memory = o.memory ?? '12g', port = Number(o.port ?? PORTS[name]), volume = `${cname(name)}-data`
  if (action === 'status') { console.log(JSON.stringify({ recipe: { status: rec.status, notes: rec.notes }, state: readStore(name) }, null, 1)); return }
  if (rec.stub && action !== 'down') { console.log(`${name}: ${rec.status}\nstub recipe, not wired for \`up\`.\nimage ${rec.image}\nendpoints ${JSON.stringify(rec.endpoints(port))}\n${rec.notes}`); return }
  if (action === 'up') return withMachine(`store-${name}-up`, async () => {
    if (rec.dockerfile) { log(`building ${rec.image}`); await exec('docker', ['build', '-t', rec.image, '-f', rec.dockerfile, path.dirname(rec.dockerfile)]) }
    else await exec('docker', ['pull', rec.image])
    const digest = await digestOf(rec.image)
    await exec('docker', ['rm', '-f', cname(name)], { allowFail: true })
    await exec('docker', ['volume', 'create', volume])
    const args = ['run', '-d', '--name', cname(name), '--cpus', cpus, '--memory', memory, '-p', `127.0.0.1:${port}:${rec.serve ? 7878 : port}`, '-v', `${volume}:/data`, rec.image, ...(rec.serve ? rec.serve(o) : [])]
    await exec('docker', args)
    const ok = await waitReady(rec.ready ? rec.ready(port) : `http://127.0.0.1:${port}/`)
    if (!ok) throw new Error(`${name} did not become ready`)
    const ver = (await exec('docker', ['logs', cname(name)], { allowFail: true })).out.split('\n')[0]
    const state = { name, image: rec.image, digest, container: cname(name), volume, port, cpus, memory, endpoints: rec.endpoints(port), upAt: new Date().toISOString(), defaultGraph: 'exclusive (store default graph)', reasoning: 'off', notes: rec.notes, status: rec.status, firstLog: ver }
    writeJson(stateFile(name), state); console.log(JSON.stringify(state, null, 1))
  }, { docker: true, quitDockerAtEnd: false })
  if (action === 'down') return withMachine(`store-${name}-down`, async () => {
    // only the container we started, by name
    await exec('docker', ['rm', '-f', cname(name)], { allowFail: true })
    if (o.purge) await exec('docker', ['volume', 'rm', volume], { allowFail: true })
    console.log(`${cname(name)} removed${o.purge ? `; volume ${volume} purged` : `; volume ${volume} kept`}`)
  }, { docker: true, quitDockerAtEnd: false })
  throw new Error('store <name> up|down|status')
}

// External bulk load (Oxigraph `load`): stop the server (RocksDB lock), run the loader, start again.
export async function externalLoad(name, dataFiles, o) {
  const rec = RECIPES[name], st = readStore(name)
  if (!rec.load) throw new Error(`${name} has no external loader recipe; use GSP`)
  if (!st) throw new Error(`${name} is not up (no state file)`)
  const t0 = Date.now()
  await exec('docker', ['stop', st.container])
  const mounts = [...new Set(dataFiles.map((f) => path.dirname(f)))]
  const mountArgs = mounts.flatMap((d, i) => ['-v', `${d}:/in${i}:ro`])
  const inFiles = dataFiles.map((f) => `/in${mounts.indexOf(path.dirname(f))}/${path.basename(f)}`)
  const args = ['run', '--rm', '--cpus', st.cpus, '--memory', st.memory, '-v', `${st.volume}:/data`, ...mountArgs, st.image, ...rec.load(o, inFiles)]
  const r = await exec('docker', args)
  const loadMs = Date.now() - t0
  await exec('docker', ['start', st.container])
  await waitReady(rec.ready(st.port))
  return { kind: 'external', loader: `${name} load`, loadMs, log: (r.err + r.out).slice(-600) }
}

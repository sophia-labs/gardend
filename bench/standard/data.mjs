// `gbench standard data <bsbm|sp2b|watdiv> --scale N`
//   bsbm  --scale <products>   (1000 = smoke, 100000 = ~35M triples)   -> dataset.nt[.gz], td_data/, streams/{explore,explore-update}.jsonl
//   sp2b  --scale <triples>    (10000000)                              -> dataset.nt[.gz], queries/*.sparql
//   watdiv --scale <SF>        (100 = ~10M triples)                    -> dataset.nt[.gz], queries/<T>.txt (20 basic templates x --queries-per-template)
// Everything is generated once inside Docker (image sophia-bench-gen:1 from docker/Dockerfile.gen) and cached under
// ~/.cache/sophia-bench/standard/<benchmark>-<scale>/ with a manifest.json (versions, seeds, sha256).
import { existsSync, readdirSync, statSync, readFileSync, writeFileSync, rmSync, createWriteStream, copyFileSync, mkdirSync } from 'node:fs'
import http from 'node:http'
import path from 'node:path'
import { CACHE, SRC, exec, log, mkdirp, dataDir, sha256File, writeJson, readJson, sleep } from './lib/common.mjs'
import { withMachine } from './lib/machine.mjs'

const here = path.dirname(new URL(import.meta.url).pathname)
export const GEN_IMAGE = 'sophia-bench-gen:1'
const GEN_SRC = {
  bsbm: { repo: 'https://github.com/Tpt/bsbm-tools', note: 'Tpt/bsbm-tools (clone of bsbmtools 0.2, sourceforge)' },
  sp2b: { url: 'https://dbis.informatik.uni-freiburg.de/content/projects/SP2B/docs/sp2b-v1_01-full.tar.gz', sha256: '6640506cf2e2d142ea3435b1b8cc3999879349ebcd1a3cdc601db831f5cb3d73' },
  watdiv: { url: 'https://dsg.uwaterloo.ca/watdiv/watdiv_v06.tar', sha256: 'fb8d930b74b3fbc8f948101bfaf658a90d2f74002f1fefda465c45ffd33a71d2' },
}

async function ensureSources() {
  mkdirp(SRC)
  if (!existsSync(path.join(SRC, 'bsbm-tools'))) await exec('git', ['clone', '-q', GEN_SRC.bsbm.repo, path.join(SRC, 'bsbm-tools')])
  for (const [k, f] of [['sp2b', 'sp2b-v1_01-full.tar.gz'], ['watdiv', 'watdiv_v06.tar']]) {
    const p = path.join(SRC, f)
    if (!existsSync(p)) await exec('curl', ['-fsSL', '-o', p, GEN_SRC[k].url])
    const h = await sha256File(p); if (h !== GEN_SRC[k].sha256) throw new Error(`${f}: sha256 ${h} != pinned ${GEN_SRC[k].sha256}`)
  }
  mkdirSync(path.join(SRC, 'gen-entry'), { recursive: true })
  copyFileSync(path.join(here, 'docker', 'gen-entry.sh'), path.join(SRC, 'gen-entry.sh'))
  copyFileSync(path.join(here, 'docker', 'Dockerfile.gen'), path.join(SRC, 'Dockerfile.gen'))
}
async function ensureImage() {
  const have = await exec('docker', ['image', 'inspect', GEN_IMAGE], { allowFail: true })
  if (have.code === 0) return
  await ensureSources()
  log(`building ${GEN_IMAGE} (Java 17, g++, Boost; BSBM compiled from source)`)
  await exec('docker', ['build', '-t', GEN_IMAGE, '-f', path.join(SRC, 'Dockerfile.gen'), SRC], { stdio: ['ignore', 'inherit', 'inherit'] })
}
const gen = (outDir, args, extra = []) => exec('docker', ['run', '--rm', '-v', `${outDir}:/out`, ...extra, GEN_IMAGE, ...args], { stdio: ['ignore', 'inherit', 'inherit'] })

async function lines(file) {
  const gz = file.endsWith('.gz')
  const r = await exec('sh', ['-c', gz ? `gzip -dc "${file}" | wc -l` : `wc -l < "${file}"`])
  return Number(r.out.trim())
}
async function gzipFile(f) { await exec('sh', ['-c', `gzip -3 -f "${f}"`]); return f + '.gz' }

// ---- BSBM query stream recorder: run the OFFICIAL Java test driver against a recording HTTP server ----
// The driver generates the parameterised queries (its own seeded RNG over td_data/); we log what it sends and answer with
// an empty result. The harness then replays the identical text to every store. This keeps parameter semantics exactly BSBM's.
async function recordStream({ outDir, usecase, mixes, warmup, seed, label }) {
  const reqs = []
  const srv = http.createServer((req, res) => {
    const chunks = []
    req.on('data', (c) => chunks.push(c))
    req.on('end', () => {
      const u = new URL(req.url, 'http://x'); let text
      if (req.method === 'GET') text = u.searchParams.get('query')
      else { const b = Buffer.concat(chunks).toString('utf8'); text = (req.headers['content-type'] ?? '').includes('urlencoded') ? new URLSearchParams(b).get('update') : b }
      reqs.push({ kind: u.pathname.startsWith('/update') ? 'update' : 'select', text })
      res.writeHead(200, { 'content-type': 'application/sparql-results+xml' })
      res.end('<?xml version="1.0"?><sparql xmlns="http://www.w3.org/2005/sparql-results#"><head/><results/></sparql>')
    })
  })
  await new Promise((r) => srv.listen(0, '0.0.0.0', r))
  const port = srv.address().port
  const mixCount = mixes + warmup
  const args = ['bsbm-driver', String(port), usecase, String(mixCount), String(seed)]
  try {
    await exec('docker', ['run', '--rm', '-v', `${outDir}:/out`, '--entrypoint', 'bash', GEN_IMAGE, '-c',
      `cd /opt/bsbm && ./testdriver -idir /out/td_data -ucf usecases/${usecase}/sparql.txt -runs ${mixes} -w ${warmup} -seed ${seed} -o /tmp/r.xml ` +
      `${usecase === 'explore' ? '' : `-u http://host.docker.internal:${port}/update -udataset /out/dataset_update.nt `}http://host.docker.internal:${port}/sparql`], { stdio: ['ignore', 'pipe', 'pipe'] })
  } finally { srv.close() }
  return reqs
}

// Template fingerprints: build one regex per template query so every recorded text is classified by content, not position.
function templateRegexes(dir, ids) {
  const out = {}
  for (const id of ids) {
    const t = readFileSync(path.join(dir, `query${id}.txt`), 'utf8')
    const esc = t.trim().split(/%[A-Za-z0-9]+%/).map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&').replace(/\s+/g, '\\s+'))
    out[id] = new RegExp('^\\s*' + esc.join('[\\s\\S]+?') + '\\s*$')
  }
  return out
}

export async function buildBsbmStreams(outDir, o) {
  const src = path.join(SRC, 'bsbm-tools')
  const mixes = Number(o.mixes ?? 550), warmup = Number(o.warmup ?? 50), seed = Number(o.seed ?? 1)
  const exploreIds = readFileSync(path.join(src, 'queries/explore/querymix.txt'), 'utf8').trim().split(/\s+/).map(Number)
  const updIds = readFileSync(path.join(src, 'queries/update/querymix.txt'), 'utf8').trim().split(/\s+/).map(Number)
  const exRe = templateRegexes(path.join(src, 'queries/explore'), [...new Set(exploreIds)])
  const upRe = templateRegexes(path.join(src, 'queries/update'), [...new Set(updIds)])
  mkdirp(path.join(outDir, 'streams'))
  const result = {}
  for (const [name, usecase] of [['explore', 'explore'], ['explore-update', 'exploreAndUpdate']]) {
    const reqs = await recordStream({ outDir: name === 'explore' ? outDir : path.join(outDir, 'upd'), usecase, mixes, warmup, seed })
    // the official driver issues the update queries only in measured mixes; warm-up mixes are explore-only
    const exploreOnly = exploreIds.map((n) => ['q', n]), full = name === 'explore' ? exploreOnly : [...updIds.map((n) => ['u', n]), ...exploreOnly]
    const total = warmup * exploreOnly.length + mixes * full.length
    if (reqs.length !== total) throw new Error(`${name}: recorded ${reqs.length} requests, expected ${total}`)
    const lines = []; let at = 0
    for (let m = 0; m < mixes + warmup; m++) {
      const expect = m < warmup ? exploreOnly : full
      const queries = reqs.slice(at, at + expect.length).map((r, i) => {
        const [k, n] = expect[i], re = k === 'q' ? exRe[n] : upRe[n]
        if (!re.test(r.text)) throw new Error(`${name}: mix ${m} position ${i}: text does not match template ${k}${n}: ${r.text.slice(0, 120)}`)
        return { id: `${k}${n}`, kind: r.kind, text: r.text }
      })
      at += expect.length
      lines.push(JSON.stringify({ mix: m, warmup: m < warmup, queries }))
    }
    writeFileSync(path.join(outDir, 'streams', `${name}.jsonl`), lines.join('\n') + '\n')
    result[name] = { mixes, warmup, queriesPerMix: full.length, seed, file: `streams/${name}.jsonl` }
    log(`stream ${name}: ${mixes + warmup} mixes x ${per} queries (seed ${seed})`)
  }
  return result
}

export function parseWatdivQueries(dir) {
  const out = {}
  for (const f of readdirSync(dir).filter((x) => /^[LSFC]\d+\.txt$/.test(x))) {
    const body = readFileSync(path.join(dir, f), 'utf8')
    out[f.replace('.txt', '')] = body.split(/\n\s*\n/).map((s) => s.trim()).filter((s) => /^(SELECT|PREFIX|ASK)/i.test(s))
  }
  return out
}

export async function dataCmd(pos, o) {
  const bench = pos[0]; if (!['bsbm', 'sp2b', 'watdiv'].includes(bench)) throw new Error('data <bsbm|sp2b|watdiv> --scale N')
  const scale = Number(o.scale ?? { bsbm: 1000, sp2b: 10000000, watdiv: 100 }[bench])
  const dir = mkdirp(dataDir(bench, scale)), man = path.join(dir, 'manifest.json')
  if (existsSync(man) && !o.force) { console.log(`cached: ${dir}`); console.log(readFileSync(man, 'utf8')); return }
  return withMachine(`data-${bench}-${scale}`, async () => {
    await ensureImage()
    const t0 = Date.now(), gv = (await exec('docker', ['run', '--rm', GEN_IMAGE, 'version'])).out.trim().replace(/\n/g, '; ')
    const manifest = { benchmark: bench, scale, createdAt: new Date().toISOString(), host: 'macOS docker linux/amd64', toolchain: gv, genImage: GEN_IMAGE }
    if (bench === 'bsbm') {
      await gen(dir, ['bsbm', String(scale)])
      manifest.generator = { name: 'BSBM tools 0.2 (Tpt/bsbm-tools)', gitSha: (await exec('git', ['-C', path.join(SRC, 'bsbm-tools'), 'rev-parse', 'HEAD'])).out.trim(), flags: `-fc -pc ${scale} -s nt -ud (forward-chained types, N-Triples, update dataset)`, seed: 'built-in generator seed 53223436' }
      manifest.streams = await buildBsbmStreams(dir, o)
    } else if (bench === 'sp2b') {
      await gen(dir, ['sp2b', String(scale)])
      // generator emits Turtle (prefixes): convert to N-Triples with Oxigraph so every benchmark ships .nt
      await exec('docker', ['pull', 'ghcr.io/oxigraph/oxigraph:v0.5.9'])
      await exec('docker', ['run', '--rm', '-v', `${dir}:/d`, 'ghcr.io/oxigraph/oxigraph:v0.5.9', 'convert', '--from-file', '/d/dataset.n3', '--from-format', 'text/turtle', '--to-file', '/d/dataset.nt'], { stdio: ['ignore', 'inherit', 'inherit'] })
      rmSync(path.join(dir, 'dataset.n3'))
      manifest.generator = { name: 'SP2Bench v1.01 sp2b_gen (patched for modern g++, -O2)', source: GEN_SRC.sp2b.url, tarballSha256: GEN_SRC.sp2b.sha256, flags: `-t ${scale}`, seed: 'none (deterministic)' }
    } else {
      await gen(dir, ['watdiv', String(scale), String(o['queries-per-template'] ?? 20)])
      manifest.generator = { name: 'WatDiv v0.6', source: GEN_SRC.watdiv.url, tarballSha256: GEN_SRC.watdiv.sha256, flags: `-d wsdbm-data-model.txt ${scale}; -q <T> ${o['queries-per-template'] ?? 20} 1`, seed: 'none (built-in)' }
      const qs = parseWatdivQueries(path.join(dir, 'queries'))
      writeJson(path.join(dir, 'queries.json'), qs); manifest.templates = Object.fromEntries(Object.entries(qs).map(([k, v]) => [k, v.length]))
    }
    for (const nt of [path.join(dir, 'dataset.nt'), ...(bench === 'bsbm' ? [path.join(dir, 'upd', 'dataset.nt')] : [])]) if (statSync(nt).size > 512 * 2 ** 20 && !o['no-gzip']) await gzipFile(nt)
    manifest.files = []
    for (const f of [...readdirSync(dir), ...(bench === 'bsbm' ? readdirSync(path.join(dir, 'upd')).map((x) => path.join('upd', x)) : [])]) {
      const p = path.join(dir, f), st = statSync(p)
      if (st.isFile() && /^(upd\/)?dataset/.test(f)) manifest.files.push({ name: f, bytes: st.size, sha256: await sha256File(p), ...(/^(upd\/)?dataset\.nt/.test(f) ? { triples: await lines(p) } : {}) })
    }
    manifest.dataset = manifest.files.find((x) => x.name.startsWith('dataset.nt'))
    manifest.datasetUpdateMix = manifest.files.find((x) => x.name.startsWith('upd/dataset.nt')) ?? null // load THIS one before bsbm-explore-update (official -ud split)
    manifest.genSeconds = Math.round((Date.now() - t0) / 1000)
    writeJson(man, manifest); console.log(JSON.stringify(manifest, null, 1))
  }, { docker: true })
}

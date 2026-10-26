// Deterministic synthetic corpus (seeded PRNG, invented vocabulary: no real or private text), seeded through gardend's own API.
import { mkdirSync, existsSync, writeFileSync, readFileSync, renameSync, rmSync, readdirSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { pathToFileURL } from 'node:url'
import { execFileSync } from 'node:child_process'
import os from 'node:os'
import path from 'node:path'
import { mulberry32 } from './stats.mjs'
import { Gardend, freePort, binaryInfo, dirSizeMB } from './proc.mjs'

export const PROTOCOL_VERSION = 1
export const TIERS = {
  s: { docs: 40, blocksPerDoc: 10, bookBlocks: 300, folders: 3, comments: 10, wires: 20 },
  m: { docs: 400, blocksPerDoc: 30, bookBlocks: 2500, folders: 12, comments: 200, wires: 300 },
  l: { docs: 2000, blocksPerDoc: 30, bookBlocks: 5000, folders: 40, comments: 1000, wires: 1500 },
}
export const CACHE_ROOT = process.env.GBENCH_CACHE ?? path.join(os.homedir(), '.cache', 'sophia-bench')

// A made-up lexicon: syllable soup, so no real text can leak in.
export function lexicon(seed, n = 400) {
  const rnd = mulberry32(seed ^ 0x9e3779b9), syl = ['ka', 'lo', 'mi', 'ren', 'tu', 'va', 'sel', 'or', 'dri', 'pa', 'nu', 'xe', 'bar', 'quin', 'lum', 'tos', 'fey', 'gro']
  const words = new Set()
  while (words.size < n) { let w = ''; for (let k = 1 + Math.floor(rnd() * 3); k > 0; k--) w += syl[Math.floor(rnd() * syl.length)]; words.add(w) }
  return [...words]
}
export function makeText(rnd, lex, nWords) { const out = []; for (let i = 0; i < nWords; i++) out.push(lex[Math.floor(rnd() * lex.length)]); return out.join(' ') }
export function paragraphs(rnd, lex, n, { words = 45, headings = true } = {}) {
  const out = []
  for (let i = 0; i < n; i++) out.push(headings && i % 12 === 0 ? `## ${makeText(rnd, lex, 3)}` : `${makeText(rnd, lex, words + Math.floor(rnd() * 25))}.`)
  return out.join('\n\n')
}
export const pad = (n, w = 4) => String(n).padStart(w, '0')

// Seed a graph. `client` is an MCP/HTTP Client. opts: {graphId, prefix, parentFolder, tier, seed, log}
export async function seedGraph(client, { graphId, tier, seed, prefix = 'bench', parentFolder = null, createGraph = true, log = () => {} }) {
  const T = TIERS[tier]; if (!T) throw new Error(`unknown tier ${tier}`)
  const rnd = mulberry32(seed), lex = lexicon(seed)
  const t0 = performance.now(), phases = {}
  const must = (r, what) => { if (!r.ok) throw new Error(`seed ${what} failed: ${r.error}`); return r.payload }
  const phase = async (name, fn) => { const s = performance.now(); await fn(); phases[name] = Math.round(performance.now() - s); log(`  ${name}: ${phases[name]} ms`) }
  if (createGraph) {
    const g = await client.json('POST', '/api/graphs', { title: graphId, graph_id: graphId })
    if (g.status >= 300 && !/exist/i.test(g.text)) throw new Error(`create graph: ${g.status} ${g.text.slice(0, 200)}`)
  }
  const folders = [], docs = []
  let bookBlockIds = [], blocks = 0, comments = 0, wires = 0
  await phase('folders', async () => {
    for (let i = 0; i < T.folders; i++) {
      const id = `${prefix}-folder-${pad(i, 2)}`
      must(await client.call('create_folder', { graphId, name: `Shelf ${makeText(rnd, lex, 2)}`, folderId: id, ...(parentFolder ? { parentId: parentFolder } : {}) }), 'folder')
      folders.push(id)
    }
  })
  await phase('documents', async () => {
    for (let i = 0; i < T.docs; i++) {
      const id = `${prefix}-d${pad(i)}`
      must(await client.call('create_document', { graphId, title: `Note ${pad(i)} ${makeText(rnd, lex, 3)}`, documentId: id, parentId: folders.length ? folders[i % folders.length] : parentFolder }), 'document')
      const nb = Math.max(3, T.blocksPerDoc + Math.floor((rnd() - 0.5) * T.blocksPerDoc * 0.4))
      const p = must(await client.call('insert_blocks', { graphId, documentId: id, content: paragraphs(rnd, lex, nb), format: 'markdown' }), 'doc blocks')
      docs.push({ id, blockIds: p.block_ids.slice(0, 8), blockCount: p.block_count }); blocks += p.block_count
      if ((i + 1) % 50 === 0) log(`    docs ${i + 1}/${T.docs} (${Math.round(performance.now() - t0)} ms)`)
    }
  })
  const bookId = `${prefix}-book`
  await phase('book', async () => {
    must(await client.call('create_document', { graphId, title: `The Book ${makeText(rnd, lex, 2)}`, documentId: bookId, parentId: folders[0] ?? parentFolder }), 'book')
    for (let done = 0; done < T.bookBlocks; done += 100) {
      const n = Math.min(100, T.bookBlocks - done)
      const p = must(await client.call('insert_blocks', { graphId, documentId: bookId, content: paragraphs(rnd, lex, n, { words: 60 }), format: 'markdown' }), 'book chunk')
      bookBlockIds.push(...p.block_ids)
    }
    blocks += bookBlockIds.length
  })
  await phase('comments', async () => {
    for (let i = 0; i < T.comments; i++) {
      const d = docs[Math.floor(rnd() * docs.length)]
      must(await client.call('edit_comment', { graphId, documentId: d.id, comment_id: `${prefix}-c${pad(i)}`, action: 'set', text: makeText(rnd, lex, 8), author: 'gbench', blockId: d.blockIds[0] }), 'comment'); comments++
    }
  })
  await phase('wires', async () => {
    const all = []
    for (let i = 0; i < T.wires; i++) {
      const a = i < docs.length - 1 && i < T.wires * 0.6 ? i : Math.floor(rnd() * docs.length) // a chain first (so 2-hop paths exist), then random
      const b = i < docs.length - 1 && i < T.wires * 0.6 ? i + 1 : Math.floor(rnd() * docs.length)
      all.push({ sourceDocumentId: docs[a].id, targetDocumentId: docs[b].id, predicate: i % 3 ? 'relatesTo' : 'cites' })
    }
    for (let i = 0; i < all.length; i += 25) { must(await client.call('create_wires', { graphId, wires: all.slice(i, i + 25) }), 'wires'); wires += Math.min(25, all.length - i) }
  })
  return {
    graphId, prefix, tier, seed, folders, docs, book: { id: bookId, blockIds: bookBlockIds },
    counts: { documents: docs.length + 1, blocks, bookBlocks: bookBlockIds.length, comments, wires, folders: folders.length },
    seedMs: Math.round(performance.now() - t0), phases,
  }
}

// Fixture parts (SCHEMA §5): bench/fixture/parts/*.mjs, each `export const name; export async function seed(ctx) -> ids`. ids are merged
// into the manifest (ctx.fixture in cases). ctx: { mcp(tool,args), http(method,path,body), graphId, rng, tier, seed, prefix, folder, log, fixture }.
const PARTS_DIR = new URL('../fixture/parts/', import.meta.url).pathname
export const partFiles = () => (!process.env.GBENCH_NO_PARTS && existsSync(PARTS_DIR) ? readdirSync(PARTS_DIR).filter((f) => f.endsWith('.mjs') && !f.startsWith('_')).sort() : [])
export const partsHash = () => { const files = partFiles(); return files.length ? createHash('sha256').update(files.map((f) => f + readFileSync(PARTS_DIR + f, 'utf8')).join('\0')).digest('hex').slice(0, 6) : '' }
export async function seedParts(client, manifest, { graphId, tier, seed, prefix, folder = null, log = () => {} }) {
  const ctx = { graphId, tier, seed, prefix, folder, log, rng: mulberry32(seed ^ 0x5bd1e995), fixture: manifest,
    mcp: async (tool, args) => { const r = await client.call(tool, args); if (!r.ok) throw new Error(`${tool}: ${r.error}`); return r.payload }, http: (m, p, b) => client.json(m, p, b) }
  const timings = {}
  for (const f of partFiles()) {
    const mod = await import(pathToFileURL(PARTS_DIR + f).href), t0 = performance.now()
    if (typeof mod.seed !== 'function') { log(`  part ${f}: no seed(), skipped`); continue }
    log(`  part ${f}`)
    const ids = await mod.seed(ctx); Object.assign(manifest, ids ?? {}); (manifest.parts ??= []).push(mod.name ?? f)
    timings[f] = Math.round(performance.now() - t0)
  }
  manifest.partTimings = timings
}
export const fixtureKey = (tier, seed, gitRev) => `${tier}-${seed}-p${PROTOCOL_VERSION}-${gitRev}${partsHash() ? '-' + partsHash() : ''}`
export const fixtureDir = (tier, seed, gitRev) => path.join(CACHE_ROOT, 'fixtures', fixtureKey(tier, seed, gitRev))

// Build (or reuse) the cached fixture profile for a binary. Returns {dir, profile, manifest, built}.
export async function ensureFixture({ bin, tier, seed, log = console.error, force = false }) {
  const info = binaryInfo(bin), dir = fixtureDir(tier, seed, info.gitRev)
  if (!force && existsSync(path.join(dir, 'manifest.json')) && existsSync(path.join(dir, 'profile'))) {
    return { dir, profile: path.join(dir, 'profile'), manifest: JSON.parse(readFileSync(path.join(dir, 'manifest.json'), 'utf8')), built: false }
  }
  const tmp = `${dir}.partial`
  rmSync(tmp, { recursive: true, force: true }); mkdirSync(path.join(tmp, 'profile'), { recursive: true })
  const port = await freePort(7790), g = new Gardend({ bin, profile: path.join(tmp, 'profile'), port, logFile: path.join(tmp, 'seed.log') })
  const graphId = `bench-${tier}-${seed}`
  log(`seeding fixture ${fixtureKey(tier, seed, info.gitRev)} on :${port} (pid recorded)`)
  g.start()
  try {
    await g.waitHealthy()
    const manifest = await seedGraph(g.client({ timeoutMs: 600_000 }), { graphId, tier, seed, prefix: 'doc-bench', log })
    await seedParts(g.client({ timeoutMs: 600_000 }), manifest, { graphId, tier, seed, prefix: 'doc-bench', log })
    manifest.gitRev = info.gitRev; manifest.protocolVersion = PROTOCOL_VERSION
    await g.stop()
    manifest.profileMB = dirSizeMB(path.join(tmp, 'profile')); manifest.stopMs = Math.round(g.stopMs)
    writeFileSync(path.join(tmp, 'manifest.json'), JSON.stringify(manifest))
    rmSync(dir, { recursive: true, force: true }); renameSync(tmp, dir)
    log(`fixture ready: ${manifest.counts.documents} docs, ${manifest.counts.blocks} blocks, seed ${manifest.seedMs} ms, ${manifest.profileMB} MB`)
    return { dir, profile: path.join(dir, 'profile'), manifest, built: true }
  } catch (e) { await g.stop(); throw e }
}

// APFS clone (copy-on-write) of a profile.
export function cloneProfile(src, dst) { mkdirSync(path.dirname(dst), { recursive: true }); execFileSync('cp', ['-c', '-R', src, dst]) }

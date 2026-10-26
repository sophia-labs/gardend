// Deterministic synthetic generators for the rich.* cases. Everything derives from a seed and the made-up syllable lexicon in
// lib/fixture.mjs, so no real or private text can appear. Pure functions: no I/O, no engine calls.
import { mulberry32 } from '../../lib/stats.mjs'
import { lexicon, makeText } from '../../lib/fixture.mjs'

export { makeTextPdf } from './rich-pdf.mjs'
const words = (seed, n, lex) => makeText(mulberry32(seed), lex, n)
export const lex = (seed) => lexicon(seed, 300)

// ---- whiteboard -------------------------------------------------------------------------------------------------------
// Batches of moves (each <= 200, the tool's cap) that draw `shapes` shapes on a grid, joined by arrows (about one arrow per two shapes).
export function boardBatches(shapes, seed, { perBatch = 100, offset = 0 } = {}) {
  const rnd = mulberry32(seed ^ 0xb0a4d), L = lex(seed), kinds = ['box', 'note', 'ellipse', 'diamond', 'box', 'text'], colors = ['yellow', 'blue', 'green', 'red', 'violet', 'grey']
  const out = []
  for (let b = 0; b * perBatch < shapes; b++) {
    const moves = [], n = Math.min(perBatch, shapes - b * perBatch)
    for (let k = 0; k < n; k++) {
      const g = offset + b * perBatch + k
      moves.push({ op: 'add', ref: `s${k}`, shape: kinds[g % kinds.length], text: makeText(rnd, L, 2 + (g % 3)), at: { x: (g % 40) * 220, y: Math.floor(g / 40) * 140 }, ...(g % 6 === 0 ? {} : { color: colors[g % colors.length] }) })
    }
    for (let k = 0; k + 1 < n; k += 2) moves.push({ op: 'connect', from: `s${k}`, to: `s${k + 1}`, label: k % 4 === 0 ? 'cites' : undefined, style: k % 8 === 0 ? 'dashed' : 'solid' })
    out.push(moves.map((m) => JSON.parse(JSON.stringify(m)))) // drop undefined
  }
  return out
}
// One agent-sized edit (about 10 moves): `free` adds with no position (the default agent path: placed in free space), two connects among them,
// then updates and relative moves on `existingIds`.
export function smallEdit(seed, i, existingIds, { free = false } = {}) {
  const rnd = mulberry32(seed ^ (i * 7919 + 3)), L = lex(seed), moves = []
  for (let k = 0; k < 4; k++) moves.push({ op: 'add', ref: `n${k}`, shape: k % 2 ? 'note' : 'box', text: makeText(rnd, L, 3), ...(free ? {} : { at: { x: 9000 + (i % 20) * 200, y: k * 120 } }) })
  moves.push({ op: 'connect', from: 'n0', to: 'n1', label: 'next' }, { op: 'connect', from: 'n2', to: 'n3' })
  if (existingIds.length) {
    const pick = (j) => existingIds[Math.floor(rnd() * existingIds.length)]
    moves.push({ op: 'update', id: pick(0), text: makeText(rnd, L, 3) }, { op: 'update', id: pick(1), color: 'green' }, { op: 'move', id: pick(2), by: { dx: 10, dy: 10 } }, { op: 'connect', from: 'n0', to: pick(3) })
  }
  return moves
}

// ---- Flow board (flow_seed_board) -------------------------------------------------------------------------------------
export function flowBoard(systems, seed) {
  const rnd = mulberry32(seed ^ 0xf10), L = lex(seed), sys = [{ id: 'sys-root', parent_id: null, name: 'Root', description: '', display_mode: 'outline', color: '#2a2a2a', x: 0, y: 0, width: 4000, height: 4000 }], reqs = []
  for (let i = 0; i < systems; i++) {
    sys.push({ id: `sys-${i}`, parent_id: i > 8 ? `sys-${Math.floor(i / 4)}` : 'sys-root', name: makeText(rnd, L, 2), description: makeText(rnd, L, 6), display_mode: 'solid', color: '#111', x: (i % 20) * 60, y: Math.floor(i / 20) * 60, width: null, height: null })
    for (let r = 0; r < 2; r++) reqs.push({ id: `req-${i}-${r}`, system_id: `sys-${i}`, text: makeText(rnd, L, 8), source: 'spec', status: r ? 'open' : 'done', sort_order: r, start_date: '2026-08-24', due_date: '2026-09-02', duration_days: 5 })
  }
  return { schemaVersion: '1', systems: sys, requirements: reqs }
}

// ---- Emporium: a small domain ontology and typed objects ---------------------------------------------------------------
export const ONTOLOGY_CLASSES = (extra = 0) => {
  const base = [
    { name: 'Gizmo', label: 'Gizmo', plural: 'Gizmos', fields: [{ key: 'title', label: 'Title', datatype: 'string', required: true }, { key: 'weight', label: 'Weight', datatype: 'integer' }, { key: 'note', label: 'Note', datatype: 'string' }, { key: 'madeOn', label: 'Made on', datatype: 'dateTime' }, { key: 'tag', label: 'Tag', datatype: 'string', multi: true }] },
    { name: 'Widget', label: 'Widget', plural: 'Widgets', fields: [{ key: 'name', label: 'Name', datatype: 'string', required: true }, { key: 'size', label: 'Size', datatype: 'integer' }] },
  ]
  for (let c = 0; c < extra; c++) base.push({ name: `Kind${c}`, label: `Kind ${c}`, plural: `Kinds ${c}`, fields: [{ key: 'title', label: 'Title', datatype: 'string', required: true }, { key: 'n', label: 'N', datatype: 'integer' }] })
  return base
}
export function gizmoRecords(n, seed, start = 0) {
  const rnd = mulberry32(seed ^ 0x6122), L = lex(seed), out = []
  for (let i = start; i < start + n; i++) out.push({ kind: 'Gizmo', clientRef: `gz${i}`, title: `${makeText(rnd, L, 2)} ${i}`, weight: Math.floor(rnd() * 1000), note: makeText(rnd, L, 6), madeOn: new Date(Date.UTC(2026, 0, 1) + i * 3600e3).toISOString(), tag: [L[i % 20], L[(i * 7) % 20]] })
  return out
}
// Memory records for remember_batch (and the emporium sweep, which scans the memory projection).
export function memoryRecords(n, seed, start = 0) {
  const rnd = mulberry32(seed ^ 0x3e30), L = lex(seed)
  return Array.from({ length: n }, (_, k) => ({ content: `${makeText(rnd, L, 14)} (${start + k})`, kind: 'SemanticMemory', clientRef: `mem${start + k}`, sourceRefs: [{ sourceKind: 'ConversationTurn', label: `bench source ${(start + k) % 50}` }] }))
}

// ---- workflows --------------------------------------------------------------------------------------------------------
// A plan of `phases` phases x `parallel` steps (<= 6 phases, <= 12 steps in all), grammar graph-plan-v1.
export function planMoves(documentId, { phases = 3, parallel = 3 } = {}, seed = 1) {
  const L = lex(seed), mv = [{ op: 'set_title', title: `Bench plan ${words(seed, 2, L)}` }, { op: 'upsert_source', id: 'book', label: 'The bench book', documentId }]
  let prev = null
  for (let p = 1; p <= phases; p++) {
    mv.push({ op: 'upsert_phase', id: `p${p}`, title: `Phase ${p}`, order: p })
    const produced = []
    for (let s = 0; s < parallel; s++) {
      const id = `st${p}x${s}`, out = `c${p}x${s}`; produced.push(out)
      mv.push({ op: 'upsert_step', id, phase: `p${p}`, label: `the ${words(seed + p * 31 + s, 1, L)}`, agentType: 'extractor', instructions: `Read the material and list ${words(seed + s, 3, L)}.`, mode: prev ? 'perItem' : 'once', ...(prev ? { over: prev[s % prev.length] } : { reads: 'book' }), produces: out, itemSchema: { type: 'object', required: ['quote'], properties: { quote: { type: 'string' }, note: { type: 'string' }, documentId: { type: 'string' } } } })
    }
    prev = produced
  }
  mv.push({ op: 'upsert_destination', id: 'margin', label: 'The margin', receives: prev[0], documentId })
  return mv
}
// The run graph (Turtle) Choreograph writes: one GraphPlanRun and `items` wf:Items in `collection`. Each item names the quote to anchor.
export function runTurtle({ workflow, runGraph, runId, collection, quotes, documentId }) {
  const lit = (s) => JSON.stringify(s), run = `<${runGraph}#run>`
  const t = [`@prefix wf: <http://mnemosyne.dev/workflow#> .`, `${run} a wf:GraphPlanRun ; wf:runId ${lit(runId)} ; wf:status "completed" ; wf:definition <${workflow}:v1> ; wf:definitionDigest "sha256:bench" ; wf:workflow <${workflow}> .`]
  quotes.forEach((q, n) => t.push(`<${runGraph}#item-${n + 1}> a wf:Item ; wf:partOfRun ${run} ; wf:inCollection <${workflow}#${collection}> ; wf:position ${n} ; wf:json ${lit(JSON.stringify({ documentId, quote: q, note: `bench finding ${n}` }))} .`))
  return t.join('\n')
}

// ---- artifacts --------------------------------------------------------------------------------------------------------
export function markdownDoc(paragraphs, seed) {
  const rnd = mulberry32(seed ^ 0xd0c), L = lex(seed), out = ['# Bench artifact ' + words(seed, 2, L)]
  for (let i = 0; i < paragraphs; i++) out.push(i % 25 === 0 ? `## ${makeText(rnd, L, 3)}` : `${makeText(rnd, L, 40)}.`)
  return out.join('\n\n')
}
export function plainText(kib, seed) {
  const rnd = mulberry32(seed ^ 0x7e7), L = lex(seed); let s = ''
  while (s.length < kib * 1024) s += makeText(rnd, L, 12) + '\n'
  return s.slice(0, kib * 1024)
}

// ---- datasheets: an autonotes.data/2 envelope with `rows` specification rows ---------------------------------------------
export function datasheetEnvelope(rows, seed, part = 'XBN1000') {
  const rnd = mulberry32(seed ^ 0xda7a), L = lex(seed), r1 = (x) => Math.round(x * 100) / 100
  const spec = Array.from({ length: rows }, (_, i) => {
    const typ = r1(1 + rnd() * 100)
    return { section: i % 2 ? 'Electrical Characteristics' : 'Recommended Operating Conditions', page: 4 + (i % 12), group: '', symbol: `S${i}`, parameter: `${makeText(rnd, L, 2)} ${i}`, conditions: i % 3 ? `TJ = ${25 + (i % 5) * 10} C` : '', min: r1(typ * 0.8), typ, max: r1(typ * 1.3), unit: ['V', 'A', 'mV', 'nA', 'kHz'][i % 5], extractor: 'docling', confidence: ['high', 'medium', 'low'][i % 3], flags: [] }
  })
  const params = ['vin', 'vout', 'iout', 'fsw'].map((key, i) => ({ key, min: i % 2 ? null : 1.8, typ: null, max: 6.5 + i, unit: ['V', 'V', 'A', 'kHz'][i], doc_min: null, doc_max: 6.5 + i, doc_confidence: 'high', readings: [{ source: 'page1', extractor: 'docling', page: 1, min: null, max: 6.5 + i }, { source: 'roc', extractor: 'pdfplumber', page: 4, min: null, max: 6.5 + i }], disagreement: null, flags: [], page: 4, confidence: 'high', contested: false }))
  const sha = [...Array(64)].map(() => '0123456789abcdef'[Math.floor(rnd() * 16)]).join('')
  return { schema: 'autonotes.data/2', document: { file: `${part.toLowerCase()}.pdf`, original_name: `${part} datasheet.pdf`, sha256: sha, pages: 20, doc_type: 'datasheet', category: 'Power/Buck', title: `${part} synthetic converter`, parts: [part], revision: 'A', ids: {}, product_type: 'dc-dc', classified_by: 'text', manufacturer: 'Synthetic Semiconductor', manufacturer_by: 'filename', topology_class: 'buck', topology_by: 'text', steps_down: true, converts_voltage: true, correction_problems: [] },
    extraction: { generated: '2026-10-25', readers: { registers: null, device_info: 'docling', prose: 'docling', tables: 'docling' }, tools: { 'datasheet-parser': 'bench', docling: '2.31.0' }, docling_rows: rows, pdfplumber_sections: 2, unit_repairs: 0, text_disagreement_pages: [] },
    params, spec_rows: spec, registers: [], flags: [] }
}

// ---- RDF --------------------------------------------------------------------------------------------------------------
// N-Triples: about `n` triples over n/6 subjects with typed and plain literals and links (so SPARQL joins have something to chew on).
export function ntriples(n, seed, ns = 'urn:gbench:rdf') {
  const rnd = mulberry32(seed ^ 0x2df), L = lex(seed), lines = [], subj = Math.max(1, Math.floor(n / 6)), P = (s) => `<${ns}:p:${s}>`
  for (let i = 0; lines.length < n; i++) {
    const s = `<${ns}:e:${i % subj}>`
    switch (i % 6) {
      case 0: lines.push(`${s} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <${ns}:C:${i % 7}> .`); break
      case 1: lines.push(`${s} ${P('label')} "${makeText(rnd, L, 3)}" .`); break
      case 2: lines.push(`${s} ${P('count')} "${Math.floor(rnd() * 10000)}"^^<http://www.w3.org/2001/XMLSchema#integer> .`); break
      case 3: lines.push(`${s} ${P('knows')} <${ns}:e:${Math.floor(rnd() * subj)}> .`); break
      case 4: lines.push(`${s} ${P('note')} "${makeText(rnd, L, 8)}"@en .`); break
      default: lines.push(`${s} ${P('at')} "2026-01-${String(1 + (i % 28)).padStart(2, '0')}T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime> .`)
    }
  }
  return lines.join('\n') + '\n'
}

// ---- parts descriptor (for the runner's fixture-parts hook, whose exact shape was still open when this was written) -------------
export const part = { id: 'rich', provides: ['board', 'objects', 'pdf', 'rdf'], note: 'The rich.* cases seed their own data in setup(); these generators are the shared source.' }

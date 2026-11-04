// Client for the ledger room (/hocuspocus/ledger/{graph}/{vocab}), for the rich.ledger.* cases.
//
// The ledger room speaks the y-sync message tags (ARCHITECTURE.md sections 3 and 4) with ledger payloads:
//   message  = varuint type (0 sync, 1 awareness) ...
//   sync     = varuint type 0, varuint sub (0 SyncStep1, 1 SyncStep2, 2 Update), varUint8Array payload
//   Step1    = ledger-vv/1     [u8 1][u32 n]{[u16 len][origin utf8][u64 seq]}*                (little-endian)
//   Step2/Update = ledger-bundle/1 [u8 1]{[u16 len][origin][u32 nbytes][raw frames]}*
//   awareness = varuint type 1, varUint8Array(y-protocols awareness update)
// A raw frame is the crate's frame v1 (garden-ledger frame.rs), byte for byte:
//   [u32 body_len][u16 hdr_len][header JCS][u32 hdr_crc over hdr_len||header][payload][u32 payload_crc]
// with the event id = blake3(header bytes). crc32c is the Castagnoli polynomial. All integers little-endian.
//
// The bench is the relay a browser is: relay(A, B) carries every frame one cell's room holds into the other's.
// No yjs is needed; only @noble/hashes (blake3). `available` says whether it loaded.
let B3 = null
try { B3 = (await import('@noble/hashes/blake3')).blake3 } catch { B3 = null }
export const available = !!B3

const enc8 = new TextEncoder(), dec8 = new TextDecoder()
const CHUNK = 8 * 1024 * 1024 // wire payloads stay under the 16 MiB ws cap; the server uses the same 8 MiB
const MAX_JSON_INT = 2 ** 53

// ---------- bytes ----------
const cat = (parts) => { let n = 0; for (const p of parts) n += p.length; const o = new Uint8Array(n); let k = 0; for (const p of parts) { o.set(p, k); k += p.length } return o }
const u16 = (n) => { const b = new Uint8Array(2); new DataView(b.buffer).setUint16(0, n, true); return b }
const u32 = (n) => { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, n >>> 0, true); return b }
const u64 = (n) => { const b = new Uint8Array(8); new DataView(b.buffer).setBigUint64(0, BigInt(n), true); return b }
const toHex = (b) => { let s = ''; for (const x of b) s += x.toString(16).padStart(2, '0'); return s }

// lib0-compatible varuint and varUint8Array / varString (so no lib0 dependency)
function writeVarUint(out, n) { while (n > 0x7f) { out.push(0x80 | (n & 0x7f)); n = Math.floor(n / 128) } out.push(n & 0x7f) }
function readVarUint(buf, pos) { let n = 0, mul = 1, i = pos.p; for (;;) { if (i >= buf.length) throw new Error('varuint past end'); const b = buf[i++]; n += (b & 0x7f) * mul; if (b < 0x80) break; mul *= 128 } pos.p = i; return n }
function varBytes(bytes) { const h = []; writeVarUint(h, bytes.length); return cat([Uint8Array.from(h), bytes]) }
function readVarBytes(buf, pos) { const n = readVarUint(buf, pos); if (pos.p + n > buf.length) throw new Error('var bytes past end'); const s = buf.subarray(pos.p, pos.p + n); pos.p += n; return s }

// ---------- crc32c (Castagnoli, reflected poly 0x82F63B78) ----------
const CRC_T = (() => { const t = new Uint32Array(256); for (let i = 0; i < 256; i++) { let c = i; for (let k = 0; k < 8; k++) c = c & 1 ? (c >>> 1) ^ 0x82f63b78 : c >>> 1; t[i] = c >>> 0 } return t })()
export function crc32c(bytes, crc = 0) { let c = ~crc >>> 0; for (let i = 0; i < bytes.length; i++) c = CRC_T[(c ^ bytes[i]) & 0xff] ^ (c >>> 8); return ~c >>> 0 }

// ---------- RFC 8785 canonical JSON: sorted keys (UTF-16 code units), JSON.stringify for scalars ----------
export function jcs(v) {
  if (v === null || typeof v === 'boolean' || typeof v === 'string') return JSON.stringify(v)
  if (typeof v === 'number') {
    if (!Number.isFinite(v)) throw new Error('jcs: non-finite number')
    if (Number.isInteger(v) && Math.abs(v) > MAX_JSON_INT) throw new Error(`jcs: integer ${v} above 2^53`)
    return JSON.stringify(v)
  }
  if (Array.isArray(v)) return '[' + v.map(jcs).join(',') + ']'
  if (typeof v === 'object') {
    const keys = Object.keys(v).filter((k) => v[k] !== undefined).sort() // default sort compares UTF-16 code units
    return '{' + keys.map((k) => JSON.stringify(k) + ':' + jcs(v[k])).join(',') + '}'
  }
  throw new Error(`jcs: unsupported ${typeof v}`)
}

// ---------- frames ----------
export const idOf = (headerBytes) => { if (!B3) throw new Error('@noble/hashes not installed (pnpm install in bench/)'); return B3(headerBytes) }
const payloadBytes = (p) => p instanceof Uint8Array ? p : enc8.encode(typeof p === 'string' ? p : JSON.stringify(p ?? []))

// Encode one event. `h` is {origin, seq, hlc:{ms,count}, witness, vocab, subjects, mode, txn?}; the payload hash and length are filled here.
// Returns {bytes, id (hex), origin, seq}. A commit frame has an empty payload and a txn.
export function encodeFrame(h, payload) {
  const pl = payloadBytes(payload)
  const header = { v: 1, origin: h.origin, seq: h.seq, hlc: { ms: h.hlc.ms, count: h.hlc.count }, witness: h.witness, vocab: h.vocab,
    subjects: (h.subjects ?? []).map((s) => ({ subject: s.subject, class: s.class, parents: s.parents ?? [] })), mode: h.mode,
    payload_hash: toHex(idOf(pl)), payload_len: pl.length }
  if (h.txn != null) header.txn = h.txn
  const hb = enc8.encode(jcs(header))
  if (hb.length > 0xffff) throw new Error(`header ${hb.length} bytes exceeds 65535`)
  const hl = u16(hb.length), hcrc = u32(crc32c(cat([hl, hb])))
  const bodyLen = 2 + hb.length + 4 + pl.length + 4
  const bytes = cat([u32(bodyLen), hl, hb, hcrc, pl, u32(crc32c(pl))])
  return { bytes, id: toHex(idOf(hb)), origin: h.origin, seq: h.seq }
}

// Split a run of raw frames into [{origin, seq, id, bytes}], checking structure and both crcs. Throws on damage.
export function parseFrames(raw) {
  const out = [], dv = new DataView(raw.buffer, raw.byteOffset, raw.byteLength)
  let p = 0
  while (p < raw.length) {
    if (raw.length - p < 4) throw new Error('truncated frame length')
    const bodyLen = dv.getUint32(p, true), end = p + 4 + bodyLen
    if (bodyLen < 10 || end > raw.length) throw new Error('truncated frame body')
    const hl = dv.getUint16(p + 4, true), hs = p + 6, he = hs + hl
    if (he + 4 > end) throw new Error('header overruns frame')
    const hb = raw.subarray(hs, he)
    if (dv.getUint32(he, true) !== crc32c(raw.subarray(p + 4, he))) throw new Error('header crc mismatch')
    const ps = he + 4, pe = end - 4
    if (pe < ps) throw new Error('payload overruns frame')
    if (dv.getUint32(pe, true) !== crc32c(raw.subarray(ps, pe))) throw new Error('payload crc mismatch')
    const h = JSON.parse(dec8.decode(hb))
    out.push({ origin: h.origin, seq: h.seq, id: toHex(idOf(hb)), bytes: raw.slice(p, end) })
    p = end
  }
  return out
}

// ---------- vv and bundle codecs ----------
// vv: Map<origin, seq> (seq may be a number or bigint)
export function encodeVV(vv) {
  const es = [...vv].filter(([, s]) => BigInt(s) > 0n), parts = [Uint8Array.of(1), u32(es.length)]
  for (const [o, s] of es) { const ob = enc8.encode(o); parts.push(u16(ob.length), ob, u64(s)) }
  return cat(parts)
}
export function decodeVV(b) {
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength), vv = new Map()
  if (b.length < 5 || b[0] !== 1) throw new Error('bad ledger-vv/1')
  const n = dv.getUint32(1, true); let p = 5
  for (let i = 0; i < n; i++) {
    const l = dv.getUint16(p, true); p += 2
    const o = dec8.decode(b.subarray(p, p + l)); p += l
    vv.set(o, Number(dv.getBigUint64(p, true))); p += 8
  }
  if (p !== b.length) throw new Error('trailing bytes in ledger-vv/1')
  return vv
}
// bundle: [{origin, bytes}] (bytes = raw frames)
export function encodeBundle(entries) {
  const parts = [Uint8Array.of(1)]
  for (const e of entries) { const ob = enc8.encode(e.origin); parts.push(u16(ob.length), ob, u32(e.bytes.length), e.bytes) }
  return cat(parts)
}
export function decodeBundle(b) {
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength), out = []
  if (b.length < 1 || b[0] !== 1) throw new Error('bad ledger-bundle/1')
  let p = 1
  while (p < b.length) {
    const l = dv.getUint16(p, true); p += 2
    const origin = dec8.decode(b.subarray(p, p + l)); p += l
    const n = dv.getUint32(p, true); p += 4
    if (p + n > b.length) throw new Error('bundle entry overruns payload')
    out.push({ origin, bytes: b.subarray(p, p + n) }); p += n
  }
  return out
}

// ---------- the client ----------
const syncMsg = (sub, payload) => { const h = []; writeVarUint(h, 0); writeVarUint(h, sub); return cat([Uint8Array.from(h), varBytes(payload)]) }
const lastOf = (c, o) => c.store.get(o)?.length ?? 0

// connect(url, key, graph, vocab, origin) -> client. `url` is the cell's http base. `origin` is this client's writer id
// ([A-Za-z0-9._-]{1,128}); default is random. The client keeps every frame it has seen in memory: store: Map<origin, frame[]> (index seq-1).
export function connect(url, key, graph, vocab, origin = `bench-${Math.random().toString(36).slice(2, 10)}`) {
  if (!available) throw new Error('@noble/hashes not installed (pnpm install in bench/)')
  if (!/^[A-Za-z0-9._-]{1,128}$/.test(origin)) throw new Error(`bad origin ${origin}`)
  const ws = new WebSocket(url.replace(/^http/, 'ws') + `/hocuspocus/ledger/${graph}/${vocab}`, [`bearer.${key}`]); ws.binaryType = 'arraybuffer'
  const o = { ws, origin, store: new Map(), errors: [], isSynced: false, closed: false, graph, vocab, hlc: { ms: 0, count: 0 },
    clientId: Math.floor(Math.random() * 0xffffffff) + 1, awClock: 0, _frameListeners: new Set(), _awListeners: new Set() }
  let resolve, reject; const p = new Promise((res, rej) => { resolve = res; reject = rej }); p.catch(() => {})
  const sendRaw = (bytes) => { if (ws.readyState === 1) ws.send(bytes) }
  o._send = sendRaw
  const fail = (msg) => { o.errors.push(msg) }

  // frames this client lacks are sent in 8 MiB bundles, one Update (or Step2) message per chunk
  const sendBundles = (sub, frames) => {
    let cur = [], size = 0
    const flush = () => { if (cur.length) { sendRaw(syncMsg(sub, encodeBundle(groupByOrigin(cur, o.store)))); cur = []; size = 0 } }
    for (const f of frames) { if (size + f.bytes.length > CHUNK && cur.length) flush(); cur.push(f); size += f.bytes.length }
    flush()
  }
  o._sendBundles = sendBundles
  // add frames in order; returns the ones that were new. A different id at a held seq is a fork, a seq beyond last+1 a gap: both recorded.
  o.ingest = (frames) => {
    const added = []
    for (const f of frames) {
      let s = o.store.get(f.origin); if (!s) { s = []; o.store.set(f.origin, s) }
      if (f.seq <= s.length) { if (s[f.seq - 1].id !== f.id) fail(`fork ${f.origin}#${f.seq}`); continue }
      if (f.seq !== s.length + 1) { fail(`gap ${f.origin}: have ${s.length}, got ${f.seq}`); continue }
      s.push(f); added.push(f)
    }
    if (added.length) for (const cb of o._frameListeners) cb(added)
    return added
  }
  // ingest frames that came from somewhere other than this client's server (a relay, append) and ship the new ones to the server
  o.ingestLocal = (frames) => { const added = o.ingest(frames); if (added.length) sendBundles(2, added); return added }

  ws.onerror = (e) => { o.error = String(e.message ?? 'ws error'); reject(new Error(`ledger room ${graph}/${vocab}: ${o.error}`)) }
  ws.onclose = (e) => { o.closed = true; o.closeCode = e.code; o.closeReason = e.reason; reject(new Error(`ledger room ${graph}/${vocab} closed (${e.code}${e.reason ? ' ' + e.reason : ''})`)) }
  ws.onopen = () => sendRaw(syncMsg(0, encodeVV(vvOf(o))))
  ws.onmessage = (ev) => {
    try {
      const buf = new Uint8Array(ev.data), pos = { p: 0 }, type = readVarUint(buf, pos)
      if (type === 0) {
        const sub = readVarUint(buf, pos), payload = readVarBytes(buf, pos)
        if (sub === 0) { // the server wants what it lacks: answer with every frame we hold beyond its vv (with its tip repeated, like Ledger::diff)
          const theirs = decodeVV(payload), out = []
          for (const [org, frames] of o.store) { const t = theirs.get(org) ?? 0; if (frames.length >= t && frames.length > 0) for (let i = Math.max(t - 1, 0); i < frames.length; i++) out.push(frames[i]) }
          if (out.length) sendBundles(1, out); else sendRaw(syncMsg(1, encodeBundle([])))
        } else if (sub === 1 || sub === 2) {
          const frames = []
          for (const e of decodeBundle(payload)) for (const f of parseFrames(e.bytes)) { if (f.origin !== e.origin) fail(`bundle entry ${e.origin} holds a frame of ${f.origin}`); else frames.push(f) }
          o.ingest(frames)
          if (sub === 1 && !o.isSynced) { o.isSynced = true; resolve() }
        } else fail(`unknown sync sub-tag ${sub}`)
      } else if (type === 1) {
        for (const s of decodeAwareness(readVarBytes(buf, pos))) if (s.clientId !== o.clientId && s.state !== null) for (const cb of o._awListeners) cb([s])
      }
    } catch (e) { fail(`decode: ${String(e.message).slice(0, 160)}`) }
  }
  o.synced = (timeoutMs = 60_000) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error(`ledger room ${graph}/${vocab} not synced in ${timeoutMs} ms`)), timeoutMs))])
  o.close = () => { try { sendRaw(awarenessMsg(o, null)) } catch {} try { ws.close() } catch {} }
  return o
}

// One bundle entry per origin. Each entry repeats the frame just before its first (the tip the receiver already holds): the crate's apply
// refuses a chunk for a non-empty origin that repeats none of its frames (NoOverlap), so a continuation without the overlap is an error.
function groupByOrigin(frames, store) {
  const m = new Map()
  for (const f of frames) {
    let a = m.get(f.origin)
    if (!a) { a = []; m.set(f.origin, a); const prev = f.seq > 1 ? store?.get(f.origin)?.[f.seq - 2] : null; if (prev) a.push(prev.bytes) }
    a.push(f.bytes)
  }
  return [...m].map(([origin, bs]) => ({ origin, bytes: cat(bs) }))
}
export const vvOf = (c) => new Map([...c.store].filter(([, s]) => s.length > 0).map(([k, s]) => [k, s.length]))
export const lastSeq = (c, origin) => lastOf(c, origin)
export const frameCount = (c) => { let n = 0; for (const s of c.store.values()) n += s.length; return n }

// append(client, draft) -> {origin, seq, id}. Builds the next frame of the client's own origin (its own seq counter and hlc) and sends it
// as an Update. draft = {witness, vocab?, subjects, mode ('assert' by default), payload (Uint8Array | string | JSON value), txn?}. mode 'commit' sends an empty payload.
export function append(client, draft) {
  const seq = lastOf(client, client.origin) + 1, now = Date.now(), h = client.hlc
  if (now > h.ms) { h.ms = now; h.count = 0 } else h.count += 1
  const mode = draft.mode ?? 'assert'
  if (mode === 'commit' && draft.txn == null) throw new Error('a commit frame needs a txn')
  const f = encodeFrame({ origin: client.origin, seq, hlc: { ms: h.ms, count: h.count }, witness: draft.witness ?? 'urn:gbench:witness', vocab: draft.vocab ?? client.vocab,
    subjects: draft.subjects ?? [], mode, txn: draft.txn }, mode === 'commit' ? new Uint8Array(0) : draft.payload ?? [])
  client.ingestLocal([f])
  return { origin: f.origin, seq: f.seq, id: f.id }
}
// Append `n` generated frames in one pass; mk(i) returns the draft. Waits for the socket buffer between chunks.
export async function appendMany(client, n, mk, { chunk = 500, drainMs = 120_000 } = {}) {
  for (let i = 0; i < n; i += chunk) {
    const seq0 = lastOf(client, client.origin) + 1, frames = []
    for (let j = i; j < Math.min(n, i + chunk); j++) {
      const d = mk(j), now = Date.now(), h = client.hlc
      if (now > h.ms) { h.ms = now; h.count = 0 } else h.count += 1
      frames.push(encodeFrame({ origin: client.origin, seq: seq0 + frames.length, hlc: { ms: h.ms, count: h.count }, witness: d.witness ?? 'urn:gbench:witness', vocab: d.vocab ?? client.vocab, subjects: d.subjects ?? [], mode: d.mode ?? 'assert', txn: d.txn }, d.payload ?? []))
    }
    client.ingestLocal(frames)
    await drain(client, drainMs)
  }
}
export async function drain(client, drainMs = 120_000) {
  const ws = client.ws, t0 = performance.now()
  if (typeof ws.bufferedAmount !== 'number') throw new Error('ws.bufferedAmount unavailable: no backpressure possible')
  while (ws.readyState === 1 && ws.bufferedAmount > 0) { if (performance.now() - t0 > drainMs) throw new Error('ledger room send buffer did not drain'); await new Promise((r) => setTimeout(r, 5)) }
}

// relay(A, B) -> off(). The in-process browser: every frame A holds or later receives that B lacks goes into B (and out B's socket to its cell),
// and the same the other way. Each side's SyncStep1 is answered from its store, which the relay keeps merged, so a reconnect also converges.
// Frames are deduped by (origin, seq) and compared by id, so the two directions cannot loop.
export function relay(A, B) {
  const carry = (from, to) => {
    const catchUp = []
    for (const [org, frames] of from.store) { const have = lastOf(to, org); if (frames.length > have) catchUp.push(...frames.slice(have)) }
    if (catchUp.length) to.ingestLocal(catchUp)
    const h = (added) => to.ingestLocal(added)
    from._frameListeners.add(h)
    return () => from._frameListeners.delete(h)
  }
  const offs = [carry(A, B), carry(B, A)]
  return () => { for (const f of offs) f() }
}

// ---------- awareness (y-protocols encoding, no yjs) ----------
// update = varuint n, then n x {varuint clientId, varuint clock, varString json(state)}; a state of null removes the client.
function encodeAwareness(clientId, clock, state) {
  const j = enc8.encode(JSON.stringify(state)), h = []
  writeVarUint(h, 1); writeVarUint(h, clientId); writeVarUint(h, clock)
  return cat([Uint8Array.from(h), varBytes(j)])
}
function decodeAwareness(b) {
  const pos = { p: 0 }, n = readVarUint(b, pos), out = []
  for (let i = 0; i < n; i++) {
    const clientId = readVarUint(b, pos); readVarUint(b, pos)
    out.push({ clientId, state: JSON.parse(dec8.decode(readVarBytes(b, pos))) })
  }
  return out
}
function awarenessMsg(client, state) { const h = []; writeVarUint(h, 1); return cat([Uint8Array.from(h), varBytes(encodeAwareness(client.clientId, ++client.awClock, state))]) }
// send(client, state) publishes `state` as this client's awareness state on the room. onAwareness(client, cb) calls cb([{clientId, state}]) for each remote state.
export function send(client, state) { if (client.ws.readyState === 1) client.ws.send(awarenessMsg(client, state)) }
export function onAwareness(client, cb) { client._awListeners.add(cb); return () => client._awListeners.delete(cb) }

// ---------- the version route ----------
// version(api, graph, vocab) -> {origins: {origin: seq}, durable: {origin: seq}, local}; `api` is the ctx or ctx.twin (anything with http(method, path)).
// A 404 means the binary has no ledger: returns null.
export async function version(api, graph, vocab) {
  const r = await api.http('GET', `/graphs/${graph}/ledger/${vocab}/version`)
  if (r.status === 404) return null
  if (r.status >= 300) throw new Error(`ledger version ${graph}/${vocab}: ${r.status} ${String(r.text).slice(0, 120)}`)
  return r.json ?? JSON.parse(r.text)
}
export const sumOf = (m) => Object.values(m ?? {}).reduce((a, b) => a + b, 0)
// Poll the version route until pred(v) holds; returns ms from the call. Throws after timeoutMs.
export async function waitVersion(api, graph, vocab, pred, { timeoutMs = 120_000, pollMs = 2 } = {}) {
  const t0 = performance.now()
  for (;;) {
    const v = await version(api, graph, vocab)
    if (v && pred(v)) return performance.now() - t0
    if (performance.now() - t0 > timeoutMs) throw new Error(`ledger ${graph}/${vocab}: condition not met in ${timeoutMs} ms (last ${JSON.stringify(v)?.slice(0, 160)})`)
    await new Promise((r) => setTimeout(r, pollMs))
  }
}

// Known-answer checks for the codec (crc32c("123456789") = 0xe3069283; blake3("") = af1349b9...). Throws on the first miss.
export function selfTest() {
  if (crc32c(enc8.encode('123456789')) !== 0xe3069283) throw new Error('crc32c known answer failed')
  if (toHex(idOf(new Uint8Array(0))).slice(0, 8) !== 'af1349b9') throw new Error('blake3 known answer failed')
  const f = encodeFrame({ origin: 'o', seq: 1, hlc: { ms: 1, count: 0 }, witness: 'w', vocab: 'v', subjects: [], mode: 'assert' }, '[]')
  const [g] = parseFrames(f.bytes); if (g.id !== f.id || g.seq !== 1) throw new Error('frame roundtrip failed')
  const vv = decodeVV(encodeVV(new Map([['a', 3], ['b', 7]]))); if (vv.get('b') !== 7) throw new Error('vv roundtrip failed')
  const bd = decodeBundle(encodeBundle([{ origin: 'a', bytes: f.bytes }])); if (bd[0].origin !== 'a' || bd[0].bytes.length !== f.bytes.length) throw new Error('bundle roundtrip failed')
}

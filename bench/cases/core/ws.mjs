// y-websocket cases on the hocuspocus room (/hocuspocus/docs/{graph}/{doc}). Needs yjs + y-protocols (pnpm install in bench/).
import { SMOKE } from './_util.mjs'

async function yjs() {
  try {
    const Y = await import('yjs'), sync = await import('y-protocols/sync'), enc = await import('lib0/encoding'), dec = await import('lib0/decoding')
    return { Y, sync, enc, dec }
  } catch (e) { return null }
}
const wsUrl = (ctx, docId) => ctx.url.replace(/^http/, 'ws') + `/hocuspocus/docs/${ctx.graphId}/${docId}`

function connect(ctx, L, docId, { doc } = {}) {
  const { Y, sync, enc, dec } = L, ydoc = doc ?? new Y.Doc(), t0 = performance.now()
  const ws = new WebSocket(wsUrl(ctx, docId), [`bearer.${ctx.key}`]); ws.binaryType = 'arraybuffer'
  const o = { ws, ydoc, t0, firstFrameMs: null, synced: false, onRemoteUpdate: null }
  o.opened = new Promise((res, rej) => { ws.onopen = () => res(); ws.onerror = () => rej(new Error('ws error')) })
  o.firstFrame = new Promise((res) => { o._first = res })
  o.synced_p = new Promise((res) => { o._synced = res })
  ws.onmessage = (ev) => {
    if (o.firstFrameMs === null) { o.firstFrameMs = performance.now() - t0; o._first(o.firstFrameMs) }
    const d = dec.createDecoder(new Uint8Array(ev.data)), type = dec.readVarUint(d)
    if (type !== 0) return // awareness etc.
    const e = enc.createEncoder(); enc.writeVarUint(e, 0)
    const st = sync.readSyncMessage(d, e, ydoc, o)
    if (enc.length(e) > 1) ws.send(enc.toUint8Array(e))
    if (st === 1 || st === 2) { if (!o.synced) { o.synced = true; o._synced() } }
  }
  ydoc.on('update', (u, origin) => { if (origin === o) { o.onRemoteUpdate?.() } else if (ws.readyState === 1) { const e = enc.createEncoder(); enc.writeVarUint(e, 0); sync.writeUpdate(e, u); ws.send(enc.toUint8Array(e)) } })
  o.opened.then(() => { const e = enc.createEncoder(); enc.writeVarUint(e, 0); sync.writeSyncStep1(e, ydoc); ws.send(enc.toUint8Array(e)) }).catch(() => {})
  return o
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

export const cases = [
  { id: 'ws.open.book', version: 1, kind: 'ws', suites: SMOKE, params: { iterations: 20, warmup: 2, concurrency: 1, smoke: { iterations: 4, warmup: 1 }, def: 'new WebSocket to the first frame from the server' }, needs: ['book'],
    async run(ctx) {
      const L = await yjs(); if (!L) return { skipped: 'yjs not installed (pnpm install in bench/)' }
      const n = ctx.params.iterations, w = ctx.params.warmup, calls = []
      for (let i = 0; i < n + w; i++) {
        const c = connect(ctx, L, ctx.fixture.book.id); let ok = true
        try { await Promise.race([c.firstFrame, sleep(30_000).then(() => { throw new Error('timeout') })]) } catch { ok = false }
        if (i >= w) calls.push({ ms: c.firstFrameMs ?? 30_000, ok, traceId: null })
        c.ws.close(); await sleep(150)
      }
      return { calls }
    } },
  { id: 'ws.typing.book', version: 1, kind: 'ws', suites: SMOKE, params: { iterations: 600, warmup: 0, concurrency: 1, hz: 20, smoke: { iterations: 100 }, def: 'keystroke update at 20/s on client A; latency = time until client B holds it' }, needs: ['book'],
    async run(ctx) {
      const L = await yjs(); if (!L) return { skipped: 'yjs not installed (pnpm install in bench/)' }
      const { Y } = L, n = ctx.params.iterations, hz = ctx.params.hz
      const A = connect(ctx, L, ctx.fixture.book.id), B = connect(ctx, L, ctx.fixture.book.id)
      await Promise.race([Promise.all([A.synced_p, B.synced_p]), sleep(60_000).then(() => { throw new Error('sync timeout') })])
      await sleep(500)
      const frag = A.ydoc.getXmlFragment('content')
      const para = frag.toArray().find((e) => e.nodeName && e.toArray().some((c) => c instanceof Y.XmlText))
      if (!para) { A.ws.close(); B.ws.close(); return { skipped: 'no paragraph with XmlText found in the content fragment' } }
      const text = para.toArray().find((c) => c instanceof Y.XmlText)
      const pending = [], calls = []
      B.onRemoteUpdate = () => { const have = Y.getState(B.ydoc.store, A.ydoc.clientID); const now = performance.now(); while (pending.length && pending[0].clock <= have) { const p = pending.shift(); calls.push({ ms: now - p.t, ok: true, traceId: null }) } }
      const period = 1000 / hz, start = performance.now()
      for (let i = 0; i < n; i++) {
        const target = start + i * period; const d = target - performance.now(); if (d > 0) await sleep(d)
        const t = performance.now()
        A.ydoc.transact(() => text.insert(0, 'k'))
        pending.push({ clock: Y.getState(A.ydoc.store, A.ydoc.clientID), t })
      }
      const dead = performance.now() + 5000; while (pending.length && performance.now() < dead) await sleep(20)
      const lost = pending.length
      A.ws.close(); B.ws.close(); await sleep(300)
      return { calls: [...calls, ...pending.map(() => ({ ms: 5000, ok: false, traceId: null }))], extra: { sent: n, lost, hz } }
    } },
]

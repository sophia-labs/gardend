// y-websocket client for the emporium log room (/hocuspocus/emporium-log/{graph}/{vocab}), for the sync.* and awareness.* cases.
// Ported from the W2 two-cell relay. Needs yjs, y-protocols and lib0 (pnpm install in bench/); `available` says whether they loaded.
// The bench is the relay a browser is today: relayState / relayUpdates carry one cell's room into the other's.
let L = null
try { L = { Y: await import('yjs'), sync: await import('y-protocols/sync'), aw: await import('y-protocols/awareness'), enc: await import('lib0/encoding'), dec: await import('lib0/decoding') } } catch { L = null }
export const available = !!L
const RELAY = 'ylog-relay'

// connect(url, key, graph, vocab) -> client {ydoc, ws, awareness, synced(), close()}. `url` is the cell's http base (http://127.0.0.1:port).
export function connect(url, key, graph, vocab) {
  if (!L) throw new Error('yjs / y-protocols not installed (pnpm install in bench/)')
  const { Y, sync, aw, enc, dec } = L
  const ydoc = new Y.Doc(), awareness = new aw.Awareness(ydoc), listeners = new Set()
  const ws = new WebSocket(url.replace(/^http/, 'ws') + `/hocuspocus/emporium-log/${graph}/${vocab}`, [`bearer.${key}`]); ws.binaryType = 'arraybuffer'
  const o = { ydoc, ws, awareness, _listeners: listeners, isSynced: false, error: null }
  let resolve, reject; const p = new Promise((res, rej) => { resolve = res; reject = rej })
  const send = (e) => { if (ws.readyState === 1) ws.send(enc.toUint8Array(e)) }
  ws.onerror = (e) => { o.error = String(e.message ?? 'ws error'); reject(new Error(`log room ${graph}/${vocab}: ${o.error}`)) }
  ws.onclose = (e) => { o.closed = true; reject(new Error(`log room ${graph}/${vocab} closed (${e.code})`)) }
  p.catch(() => {})
  ws.onmessage = (ev) => {
    const d = dec.createDecoder(new Uint8Array(ev.data)), type = dec.readVarUint(d)
    if (type === 0) {
      const e = enc.createEncoder(); enc.writeVarUint(e, 0)
      const st = sync.readSyncMessage(d, e, ydoc, o)
      if (enc.length(e) > 1) send(e)
      if (st === 1 || st === 2) { if (!o.isSynced) { o.isSynced = true; resolve() } }
    } else if (type === 1) aw.applyAwarenessUpdate(awareness, dec.readVarUint8Array(d), 'remote')
  }
  ydoc.on('update', (u, origin) => { if (origin !== o) { const e = enc.createEncoder(); enc.writeVarUint(e, 0); sync.writeUpdate(e, u); send(e) } })
  awareness.on('update', ({ added, updated }, origin) => {
    if (origin !== 'remote') return
    const states = [...added, ...updated].map((id) => ({ clientId: id, state: awareness.getStates().get(id) })).filter((s) => s.state)
    for (const cb of listeners) cb(states)
  })
  ws.onopen = () => { const e = enc.createEncoder(); enc.writeVarUint(e, 0); sync.writeSyncStep1(e, ydoc); send(e) }
  o.synced = (timeoutMs = 60_000) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error(`log room ${graph}/${vocab} not synced in ${timeoutMs} ms`)), timeoutMs))])
  o.close = () => { try { aw.removeAwarenessStates(awareness, [ydoc.clientID], 'close') } catch {} try { awareness.destroy() } catch {} try { ws.close() } catch {} }
  return o
}

// The whole state of `from` as one update into `to` (to's own websocket forwards it to its cell).
export function relayState(from, to) { L.Y.applyUpdate(to.ydoc, L.Y.encodeStateAsUpdate(from.ydoc), RELAY) }
// Forward every later update of `from` into `to`. Returns the unsubscribe.
export function relayUpdates(from, to) {
  const h = (u, origin) => { if (origin !== RELAY) L.Y.applyUpdate(to.ydoc, u, RELAY) }
  from.ydoc.on('update', h); return () => from.ydoc.off('update', h)
}

// Awareness, which core/ws.mjs drops (type 1 frames). send(client, state) publishes `state` as this client's local awareness state;
// onAwareness(client, cb) calls cb([{clientId, state}]) for every remote add/update. Returns the unsubscribe.
export function send(client, state) {
  const { aw, enc } = L, e = enc.createEncoder()
  client.awareness.setLocalState(state)
  enc.writeVarUint(e, 1); enc.writeVarUint8Array(e, aw.encodeAwarenessUpdate(client.awareness, [client.ydoc.clientID]))
  if (client.ws.readyState === 1) client.ws.send(enc.toUint8Array(e))
}
export function onAwareness(client, cb) { client._listeners.add(cb); return () => client._listeners.delete(cb) }
export const encodeAwareness = (client, ids = [client.ydoc.clientID]) => L.aw.encodeAwarenessUpdate(client.awareness, ids)
export const decodeAwareness = (client, bytes, origin = 'remote') => L.aw.applyAwarenessUpdate(client.awareness, bytes, origin)

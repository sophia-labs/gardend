// SPARQL Protocol client: plain HTTP, no store-specific behaviour. Counts SELECT rows without materialising JSON.
const dec = new TextDecoder()

// Streaming counter over application/sparql-results+json: depth tracking outside strings.
// root{=1, results{=2, bindings[=3, each binding{=4; a '}' returning depth to 3 is one row. "boolean" is picked up for ASK.
export class JsonResultCounter {
  constructor() { this.depth = 0; this.inStr = false; this.esc = false; this.rows = 0; this.tail = ''; this.bool = null; this.bytes = 0 }
  push(buf) {
    this.bytes += buf.length
    for (let i = 0; i < buf.length; i++) {
      const c = buf[i]
      if (this.inStr) { if (this.esc) this.esc = false; else if (c === 0x5c) this.esc = true; else if (c === 0x22) this.inStr = false; continue }
      if (c === 0x22) this.inStr = true
      else if (c === 0x7b || c === 0x5b) this.depth++
      else if (c === 0x7d || c === 0x5d) { this.depth--; if (c === 0x7d && this.depth === 3) this.rows++ }
    }
    if (this.bool === null) { this.tail = (this.tail + dec.decode(buf.subarray(Math.max(0, buf.length - 64)))).slice(-160); const m = this.tail.match(/"boolean"\s*:\s*(true|false)/); if (m) this.bool = m[1] === 'true' }
  }
}

export class Endpoint {
  constructor({ query, update, headers = {}, timeoutMs = 60000 }) { this.queryUrl = query; this.updateUrl = update ?? null; this.headers = headers; this.timeoutMs = timeoutMs }

  // Returns { ok, ms, rows, status, bytes, error, timedOut, bool }. ms is client wall time to last byte.
  async select(q, { accept = 'application/sparql-results+json', timeoutMs = this.timeoutMs, params = '' } = {}) {
    const t0 = performance.now(), ac = new AbortController(), timer = setTimeout(() => ac.abort(), timeoutMs)
    try {
      const url = params ? `${this.queryUrl}${this.queryUrl.includes('?') ? '&' : '?'}${params}` : this.queryUrl
      const r = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/sparql-query', accept, ...this.headers }, body: q, signal: ac.signal })
      if (!r.ok) { const body = (await r.text()).slice(0, 300); return { ok: false, ms: performance.now() - t0, status: r.status, error: `HTTP ${r.status}: ${body}` } }
      const ctr = new JsonResultCounter(), chunks = []
      const json = accept.includes('json')
      for await (const ch of r.body) { if (json) ctr.push(ch); else chunks.push(ch) }
      const ms = performance.now() - t0
      if (!json) return { ok: true, ms, status: r.status, bytes: chunks.reduce((a, c) => a + c.length, 0), body: Buffer.concat(chunks).toString('utf8') }
      return { ok: true, ms, status: r.status, rows: ctr.bool !== null ? (ctr.bool ? 1 : 0) : ctr.rows, bool: ctr.bool, bytes: ctr.bytes }
    } catch (e) {
      const timedOut = ac.signal.aborted
      return { ok: false, ms: performance.now() - t0, timedOut, error: timedOut ? 'timeout' : String(e.cause ?? e).slice(0, 300) }
    } finally { clearTimeout(timer) }
  }

  // SPARQL Update via POST application/sparql-update.
  async update(u, { timeoutMs = this.timeoutMs } = {}) {
    if (!this.updateUrl) return { ok: false, ms: 0, error: 'no --update endpoint' }
    const t0 = performance.now(), ac = new AbortController(), timer = setTimeout(() => ac.abort(), timeoutMs)
    try {
      const r = await fetch(this.updateUrl, { method: 'POST', headers: { 'content-type': 'application/sparql-update', ...this.headers }, body: u, signal: ac.signal })
      const body = await r.text()
      const ms = performance.now() - t0
      return r.ok ? { ok: true, ms, status: r.status } : { ok: false, ms, status: r.status, error: `HTTP ${r.status}: ${body.slice(0, 300)}` }
    } catch (e) {
      const timedOut = ac.signal.aborted
      return { ok: false, ms: performance.now() - t0, timedOut, error: timedOut ? 'timeout' : String(e.cause ?? e).slice(0, 300) }
    } finally { clearTimeout(timer) }
  }
}

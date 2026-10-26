// Minimal MCP-over-HTTP client for gardend. Every call carries a fresh W3C traceparent so server spans join to it.
import { randomBytes } from 'node:crypto'
import { readFileSync } from 'node:fs'

export const hex = (n) => randomBytes(n).toString('hex')
export const newTrace = () => ({ traceId: hex(16), spanId: hex(8) })
export const readKey = (file) => JSON.parse(readFileSync(file, 'utf8')).key

export class Client {
  constructor({ url, key, timeoutMs = 600_000 }) { this.url = url.replace(/\/$/, ''); this.key = key; this.timeoutMs = timeoutMs; this.id = 0 }
  redact(s) { return this.key ? String(s).replaceAll(this.key, '<key>') : String(s) }
  headers(trace) {
    return { 'content-type': 'application/json', accept: 'application/json, text/event-stream', authorization: `Bearer ${this.key}`,
      ...(trace ? { traceparent: `00-${trace.traceId}-${trace.spanId}-01` } : {}) }
  }
  // Raw HTTP request; returns {status, text, ms, traceId}
  async http(method, path, body, { trace = newTrace() } = {}) {
    const t0 = performance.now()
    const res = await fetch(this.url + path, { method, headers: this.headers(trace), body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(this.timeoutMs) })
    const text = await res.text()
    return { status: res.status, text, ms: performance.now() - t0, traceId: trace.traceId }
  }
  async json(method, path, body) {
    const r = await this.http(method, path, body)
    let j = null; try { j = JSON.parse(r.text) } catch { /* not json */ }
    return { ...r, json: j }
  }
  // tools/call. Returns {ok, ms, traceId, payload, error}. ms is client wall time to full body.
  async call(tool, args = {}, { trace = newTrace() } = {}) {
    const t0 = performance.now()
    let status = 0, text = ''
    try {
      const res = await fetch(this.url + '/mcp', { method: 'POST', headers: this.headers(trace), signal: AbortSignal.timeout(this.timeoutMs),
        body: JSON.stringify({ jsonrpc: '2.0', id: ++this.id, method: 'tools/call', params: { name: tool, arguments: args } }) })
      status = res.status; text = await res.text()
    } catch (e) { return { ok: false, ms: performance.now() - t0, traceId: trace.traceId, error: this.redact(e.message), payload: null } }
    const ms = performance.now() - t0
    let body
    try { body = text.trimStart().startsWith('{') ? JSON.parse(text) : JSON.parse(text.split('\n').find((l) => l.startsWith('data:')).slice(5)) }
    catch { return { ok: false, ms, traceId: trace.traceId, error: `http ${status}: ${this.redact(text.slice(0, 200))}`, payload: null } }
    const result = body.result ?? {}
    const isError = !!(result.isError || body.error || status >= 400)
    let payload = result.structuredContent ?? null
    if (payload == null && result.content?.[0]?.text) { try { payload = JSON.parse(result.content[0].text) } catch { payload = result.content[0].text } }
    return { ok: !isError, ms, traceId: trace.traceId, payload, bytes: text.length, error: isError ? this.redact(JSON.stringify(body.error ?? payload).slice(0, 300)) : null }
  }
  async health() { try { const r = await fetch(this.url + '/health', { signal: AbortSignal.timeout(2000) }); return r.ok } catch { return false } }
}

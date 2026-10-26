// Shared helpers for `gbench standard`. Node ESM, no dependencies beyond gbench's own lib/.
import { createHash } from 'node:crypto'
import { createReadStream, mkdirSync, existsSync, readFileSync, writeFileSync, statSync } from 'node:fs'
import { spawn } from 'node:child_process'
import os from 'node:os'
import path from 'node:path'

export const CACHE = process.env.GBENCH_STD_CACHE ?? path.join(os.homedir(), '.cache', 'sophia-bench', 'standard')
export const SRC = path.join(CACHE, 'src')
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
export const log = (...a) => console.error(`[std ${new Date().toISOString().slice(11, 19)}]`, ...a)
export const mkdirp = (d) => (mkdirSync(d, { recursive: true }), d)
export const dataDir = (bench, scale) => path.join(CACHE, `${bench}-${scale}`)
export const readJson = (f, dflt = null) => { try { return JSON.parse(readFileSync(f, 'utf8')) } catch { return dflt } }
export const writeJson = (f, o) => writeFileSync(f, JSON.stringify(o, null, 1))

export function parseArgs(argv) {
  const pos = [], o = {}
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]
    if (a.startsWith('--')) { const k = a.slice(2); const nxt = argv[i + 1]; if (nxt === undefined || nxt.startsWith('--')) o[k] = true; else { o[k] = nxt; i++ } } else pos.push(a)
  }
  return { pos, o }
}

export function sha256File(f) {
  return new Promise((res, rej) => { const h = createHash('sha256'); createReadStream(f).on('data', (c) => h.update(c)).on('end', () => res(h.digest('hex'))).on('error', rej) })
}
export const sha256Str = (s) => createHash('sha256').update(s).digest('hex')

// Run a command, collect output; reject on non-zero unless allowFail.
export function exec(cmd, args, { allowFail = false, input, stdio, env, cwd } = {}) {
  return new Promise((res, rej) => {
    const c = spawn(cmd, args, { stdio: stdio ?? ['pipe', 'pipe', 'pipe'], env: env ?? process.env, cwd })
    let out = '', err = ''
    c.stdout?.on('data', (d) => (out += d)); c.stderr?.on('data', (d) => (err += d))
    c.on('error', (e) => (allowFail ? res({ code: -1, out, err: String(e) }) : rej(e)))
    c.on('close', (code) => (code === 0 || allowFail ? res({ code, out, err }) : rej(new Error(`${cmd} ${args.join(' ')} -> ${code}: ${err.slice(-800)}`))))
    if (input != null) c.stdin.end(input); else c.stdin?.end()
  })
}

// Basic or bearer auth from `--auth user:pass` or `--auth Bearer:<token>` or `--auth-file path` (never printed).
export function authHeader(o) {
  let a = o.auth
  if (o['auth-file']) a = readFileSync(o['auth-file'], 'utf8').trim()
  if (!a) return {}
  if (a.startsWith('Bearer:')) return { authorization: `Bearer ${a.slice(7)}` }
  return { authorization: `Basic ${Buffer.from(a).toString('base64')}` }
}

export const geomean = (xs) => (xs.length ? Math.exp(xs.reduce((a, x) => a + Math.log(Math.max(x, 1e-9)), 0) / xs.length) : 0)

// Environment block for run.json: gbench's fingerprint plus the container-store note.
export async function envBlock() {
  const { fingerprint } = await import('../../lib/proc.mjs')
  return fingerprint([])
}

// Spawn / stop the gardend we start, and fingerprint the machine.
import { spawn, execFileSync } from 'node:child_process'
import { createWriteStream, existsSync, readFileSync, statSync, readdirSync } from 'node:fs'
import path from 'node:path'
import { createHash } from 'node:crypto'
import os from 'node:os'
import net from 'node:net'
import { Client, readKey } from './mcp.mjs'

const parseCpu = (s) => { const m = String(s ?? '').match(/^(?:(\d+):)?(\d+):(\d+\.\d+)$/); return m ? Number(m[1] ?? 0) * 3600 + Number(m[2]) * 60 + Number(m[3]) : 0 }
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
export const DEFAULT_BIN = '/Users/vera/Applications/Sophia.app/Contents/MacOS/gardend'
const sh = (cmd, args) => { try { return execFileSync(cmd, args, { encoding: 'utf8', timeout: 10000 }).trim() } catch { return '' } }

export async function freePort(from = 7790) {
  for (let p = from; p < from + 200; p++) {
    const free = await new Promise((res) => { const s = net.createServer(); s.once('error', () => res(false)); s.listen(p, '127.0.0.1', () => s.close(() => res(true))) })
    if (free) return p
  }
  throw new Error('no free port')
}
export const sha256File = (f) => createHash('sha256').update(readFileSync(f)).digest('hex')
export function binaryInfo(bin) {
  const version = sh(bin, ['--version']).split('\n')[0]
  let gitRev = (version.match(/\(([0-9a-f]{7,40})\)\s*$/) ?? [])[1]
  if (!gitRev) { // installed app keeps BUILD.txt in Resources; version line there names the rev
    for (const f of [bin.replace(/MacOS\/gardend$/, 'Resources/BUILD.txt'), bin + '.BUILD.txt']) {
      if (existsSync(f)) { const m = readFileSync(f, 'utf8').match(/gardend [^\n]*\(([0-9a-f]{7,40})\)/); if (m) { gitRev = m[1]; break } }
    }
  }
  // A binary built elsewhere and parked in a cache (not in a git checkout) names its rev in a BUILD.txt beside it: one line `gitRev=<sha>` (optional `binary=<file name>`).
  // Accepted: `<bin>.BUILD.txt`; or `BUILD.txt` in the binary's directory when its `binary=` line names this file, or when this file is the only binary there
  // (a shared bins/ directory never lends one binary's rev to another).
  if (!gitRev) {
    const rev = (f) => { try { const t = readFileSync(f, 'utf8'); return { rev: (t.match(/^gitRev=([0-9a-f]{7,40})\s*$/m) ?? [])[1], binary: (t.match(/^binary=(\S+)\s*$/m) ?? [])[1] } } catch { return {} } }
    const own = existsSync(bin + '.BUILD.txt') ? rev(bin + '.BUILD.txt') : {}
    if (own.rev) gitRev = own.rev
    else {
      const dir = path.dirname(bin), shared = path.join(dir, 'BUILD.txt')
      if (existsSync(shared)) {
        const r = rev(shared)
        const others = readdirSync(dir).filter((n) => n !== 'BUILD.txt' && !n.endsWith('.BUILD.txt') && statSync(path.join(dir, n)).isFile())
        if (r.rev && (r.binary ? r.binary === path.basename(bin) : others.length === 1 && others[0] === path.basename(bin))) gitRev = r.rev
      }
    }
  }
  const full = existsSync(bin.replace(/MacOS\/gardend$/, 'Resources/BUILD.txt')) ? readFileSync(bin.replace(/MacOS\/gardend$/, 'Resources/BUILD.txt'), 'utf8').match(/gardend [^\n]*/)?.[0].replace(/^gardend /, '') : null
  return { binary: bin, binarySha256: sha256File(bin), version: full ?? version, gitRev: gitRev ?? 'unknown' }
}

// A gardend child we own. Only this PID is ever signalled.
export class Gardend {
  constructor({ bin, profile, port, spansFile, logFile, env = {} }) { Object.assign(this, { bin, profile, port, spansFile, logFile, env }); this.child = null; this.exited = null }
  get url() { return `http://127.0.0.1:${this.port}` }
  start() {
    const log = createWriteStream(this.logFile, { flags: 'a' })
    this.child = spawn(this.bin, ['serve', '--profile', this.profile, '--port', String(this.port)], { env: { ...process.env, ...(this.spansFile ? { GARDEN_OTEL_FILE: this.spansFile } : {}), ...this.env }, stdio: ['ignore', 'pipe', 'pipe'] })
    // The banner prints the key link; scrub it before it reaches disk.
    const scrub = (b) => b.toString().replace(/#key=\S+/g, '#key=<redacted>')
    this.child.stdout.on('data', (b) => log.write(scrub(b))); this.child.stderr.on('data', (b) => log.write(scrub(b)))
    this.exited = new Promise((res) => this.child.once('exit', (code, sig) => { log.end(); res({ code, sig }) }))
    this.t0 = performance.now()
    return this.child.pid
  }
  async waitHealthy(timeoutMs = 120_000) {
    const c = new Client({ url: this.url, key: '' }); const t0 = performance.now()
    while (performance.now() - t0 < timeoutMs) {
      if (await c.health()) return performance.now() - this.t0
      if (this.child.exitCode !== null) throw new Error('gardend exited during startup')
      await sleep(20)
    }
    throw new Error('gardend not healthy in time')
  }
  key() { return readKey(`${this.profile}/selfhost-key.json`) }
  client(opts = {}) { return new Client({ url: this.url, key: this.key(), ...opts }) }
  rssKB() { return Number(sh('ps', ['-o', 'rss=', '-p', String(this.child.pid)])) || 0 }
  cpu() { const [u, st] = sh('ps', ['-o', 'utime=,stime=', '-p', String(this.child.pid)]).trim().split(/\s+/); return { user: parseCpu(u), sys: parseCpu(st) } }
  // SIGINT then wait; the final flush can take ~20 s. Escalates only on this PID.
  async stop(graceMs = 120_000) {
    if (!this.child || this.child.exitCode !== null) return this.exited
    const t0 = performance.now()
    this.child.kill('SIGINT')
    const r = await Promise.race([this.exited, sleep(graceMs).then(() => null)])
    if (r === null) { this.child.kill('SIGTERM'); await Promise.race([this.exited, sleep(30_000)]) }
    this.stopMs = performance.now() - t0
    return this.exited
  }
}

export function loadavg() { return os.loadavg().map((x) => Math.round(x * 100) / 100) }
// Every gardend not started by this run (pid in `ours`). Looks at args to find --port when present.
export function otherGardends(ours = []) {
  const out = []
  for (const line of sh('ps', ['-axo', 'pid=,args=']).split('\n')) {
    const m = line.trim().match(/^(\d+)\s+(\S*\/gardend|gardend)(\s.*)?$/)
    if (!m) continue
    const pid = Number(m[1]); if (ours.includes(pid) || pid === process.pid) continue
    const listen = sh('lsof', ['-nP', '-a', '-p', String(pid), '-iTCP', '-sTCP:LISTEN', '-Fn']).split('\n').filter((l) => l.startsWith('n')).map((l) => l.replace(/^n.*:/, '')).filter((p, i, a) => a.indexOf(p) === i && Number(p) < 10000)
    out.push({ pid, port: listen.length ? Number(listen[0]) : null, ports: listen.map(Number) })
  }
  return out
}
export function fingerprint(ours = []) {
  const cpus = os.cpus()
  const batt = sh('pmset', ['-g', 'batt'])
  const df = sh('df', ['-k', os.homedir()]).split('\n')[1]?.split(/\s+/)
  return {
    host: os.hostname(), os: `${sh('sw_vers', ['-productName'])} ${sh('sw_vers', ['-productVersion'])}`.trim() || os.release(), cpu: cpus[0]?.model ?? 'unknown', ncpu: cpus.length,
    memGB: Math.round(os.totalmem() / 2 ** 30), power: /AC Power/.test(batt) ? 'AC' : /Battery Power/.test(batt) ? 'battery' : 'unknown',
    diskFreeGB: df ? Math.round(Number(df[3]) / 1048576) : null, loadBefore: loadavg(), otherGardends: otherGardends(ours), node: process.version,
  }
}
export function dirSizeMB(dir) { const o = sh('du', ['-sk', dir]); return o ? Math.round(Number(o.split('\t')[0]) / 1024) : 0 }

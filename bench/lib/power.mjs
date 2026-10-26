// What the machine was doing while we measured: power state and foreign CPU.
//
// Why this exists (wave 1, 2026-10-25): the "70 s queue stall" in a write_document sample was the Mac entering
// Maintenance Sleep at 23:55:16 for 74 s, mid-request. From 19:42 that night the Mac was in Deep Idle — sleeping
// 35-75 s of every ~2.5 min and running the rest in DarkWake — and every round of the sitting straddled sleeps
// (B1 slept ~29 of its 34 minutes). Node's performance.now() counts sleep; gardend's Instant (busy/idle/held) does
// not, so a span whose wall time far exceeds busy+idle is one the machine slept through. Claude Code's own
// `caffeinate -i -t 300` lapses between turns, so background sittings were unguarded.
//
// So a sitting (1) holds the machine awake, (2) samples foreign CPU while it runs, and (3) after each round reads
// the power log for that window. A round that slept, ran in DarkWake, or ran under a CPU speed limit is not clean,
// and a comparison over unclean rounds is not a judge.
import { spawn, execFile } from 'node:child_process'
import { promisify } from 'node:util'
import os from 'node:os'
import path from 'node:path'

const run = promisify(execFile)
const mac = process.platform === 'darwin'

// Hold an idle/system/disk sleep assertion for as long as this process lives (-w ties it to our pid).
// Display sleep is allowed: the display going dark does not put the CPU into DarkWake while the assertion holds.
// It cannot stop clamshell sleep with no external display; that shows up in the power log as 'Clamshell Sleep'.
export function holdAwake(log = () => {}) {
  if (!mac) return { held: false, release() {} }
  const c = spawn('caffeinate', ['-i', '-m', '-s', '-w', String(process.pid)], { stdio: 'ignore' })
  c.on('error', (e) => log(`caffeinate failed: ${e.message}`))
  log(`holding the machine awake (caffeinate pid ${c.pid})`)
  let done = false
  const release = () => { if (!done) { done = true; if (c.exitCode === null) c.kill() } } // our own child, by handle
  process.once('exit', release)
  return { held: true, pid: c.pid, release }
}

const n = (s) => Number(s.replace(/[^0-9.]/g, ''))
async function pmsetLines(args) { try { return (await run('pmset', args, { maxBuffer: 64 << 20 })).stdout.split('\n') } catch { return [] } }

// Power source and CPU limits right now (cheap).
export async function powerNow() {
  if (!mac) return null
  const batt = (await pmsetLines(['-g', 'batt'])).join('\n'), therm = (await pmsetLines(['-g', 'therm'])).join('\n')
  const lim = (k) => { const m = therm.match(new RegExp(`${k}\\s*=\\s*(\\d+)`)); return m ? Number(m[1]) : null }
  return { source: /'AC Power'/.test(batt) ? 'ac' : /'Battery Power'/.test(batt) ? 'battery' : 'unknown', cpuSpeedLimit: lim('CPU_Speed_Limit'), cpuSchedulerLimit: lim('CPU_Scheduler_Limit') }
}

// Parse `pmset -g log` for [fromMs, toMs]: sleeps and dark wakes (each line carries its own duration), full wakes,
// clamshell and thermal events. ~5 s of CPU on a week of log: call it between rounds, never while measuring.
export async function powerDuring(fromMs, toMs) {
  if (!mac) return { checked: false, clean: true }
  const lines = await pmsetLines(['-g', 'log'])
  const day0 = new Date(fromMs - 86_400_000).toISOString().slice(0, 10)
  const ev = []
  for (const l of lines) {
    const m = l.match(/^(\d{4}-\d\d-\d\d) (\d\d:\d\d:\d\d) ([+-]\d{4}) (\S+)\s+(.*)$/); if (!m || m[1] < day0) continue
    const [, d, t, tz, kind, rest] = m
    if (!/^(Sleep|DarkWake|Wake|ThermalEvent)$/.test(kind)) continue
    const at = Date.parse(`${d}T${t}${tz.slice(0, 3)}:${tz.slice(3)}`), secs = (rest.match(/(\d+) secs?\s*$/) ?? [])[1]
    ev.push({ at, kind, rest: rest.trim(), secs: secs != null ? n(secs) : null })
  }
  const overlap = (a, b) => Math.max(0, Math.min(b, toMs) - Math.max(a, fromMs))
  const sleeps = [], darkWakes = [], fullWakes = [], thermal = []
  for (const e of ev) {
    const end = e.secs != null ? e.at + e.secs * 1000 : e.at
    if (e.kind === 'Sleep' && /Entering Sleep state/.test(e.rest)) {
      const o = overlap(e.at, end); if (o > 0 || (e.at >= fromMs && e.at <= toMs)) sleeps.push({ at: new Date(e.at).toISOString(), secs: e.secs, overlapMs: o, reason: (e.rest.match(/due to '([^']+)'/) ?? [])[1] ?? '?' })
    } else if (e.kind === 'DarkWake' || (e.kind === 'Sleep' && /Entering DarkWake/.test(e.rest))) {
      const o = overlap(e.at, end); if (o > 0) darkWakes.push({ at: new Date(e.at).toISOString(), secs: e.secs, overlapMs: o })
    } else if (e.kind === 'Wake' && /FullWake/.test(e.rest) && e.at >= fromMs && e.at <= toMs) fullWakes.push({ at: new Date(e.at).toISOString(), why: (e.rest.match(/due to (.+?) Using/) ?? [])[1] ?? '?' })
    else if (e.kind === 'ThermalEvent' && e.at >= fromMs && e.at <= toMs) thermal.push({ at: new Date(e.at).toISOString(), what: e.rest.slice(0, 80) })
  }
  const asleepMs = sleeps.reduce((a, s) => a + s.overlapMs, 0), darkWakeMs = darkWakes.reduce((a, s) => a + s.overlapMs, 0)
  const reasons = []
  if (sleeps.length) reasons.push(`slept ${sleeps.length}× (${Math.round(asleepMs / 1000)} s: ${[...new Set(sleeps.map((s) => s.reason))].join(', ')})`)
  if (darkWakeMs > 0) reasons.push(`ran ${Math.round(darkWakeMs / 1000)} s in DarkWake`)
  if (thermal.length) reasons.push(`${thermal.length} thermal event(s)`)
  return { checked: lines.length > 0, clean: lines.length > 0 && !reasons.length, reasons, asleepMs, darkWakeMs, sleeps, darkWakes: darkWakes.length, fullWakes, thermal }
}

// Sample everyone else's CPU every `everyMs` while a round runs. `ours()` returns pids to exclude (our gardend(s)).
// ps %cpu on macOS is a decaying recent average: good enough to say "the machine was busy, and with what".
export function foreignCpuSampler({ everyMs = 5000, ours = () => [] } = {}) {
  const samples = [], byComm = new Map(), self = new Set([process.pid]), cores = os.cpus().length
  let timer = null, busy = false
  const tick = async () => {
    if (busy) return; busy = true
    try {
      const { stdout } = await run('ps', ['-Ao', 'pid=,ppid=,pcpu=,comm='], { maxBuffer: 8 << 20 })
      const rows = []
      for (const l of stdout.split('\n')) { const m = l.trim().match(/^(\d+)\s+(\d+)\s+([\d.]+)\s+(.*)$/); if (m) rows.push({ pid: +m[1], ppid: +m[2], pct: +m[3], comm: m[4] }) }
      // ours = this process, the given pids, and every descendant of either (gardend, a browser the bench drives, ps, caffeinate)
      const kids = new Map(); for (const r of rows) { if (!kids.has(r.ppid)) kids.set(r.ppid, []); kids.get(r.ppid).push(r.pid) }
      const skip = new Set(), stack = [...self, ...ours()]
      while (stack.length) { const p = stack.pop(); if (skip.has(p)) continue; skip.add(p); for (const c of kids.get(p) ?? []) stack.push(c) }
      let tot = 0, oursPct = 0; const seen = new Map()
      for (const { pid, pct, comm } of rows) {
        if (!pct) continue
        if (skip.has(pid)) { oursPct += pct; continue }
        tot += pct; const c = path.basename(comm); seen.set(c, (seen.get(c) ?? 0) + pct)
      }
      samples.push({ t: Date.now(), foreignPct: Math.round(tot), oursPct: Math.round(oursPct) })
      for (const [c, p] of seen) byComm.set(c, (byComm.get(c) ?? 0) + p)
    } catch { /* a missed sample is not a measurement */ } finally { busy = false }
  }
  return {
    start() { tick(); timer = setInterval(tick, everyMs); timer.unref?.() },
    stop() {
      if (timer) clearInterval(timer); timer = null
      const f = samples.map((s) => s.foreignPct).sort((a, b) => a - b), k = samples.length
      const q = (p) => (k ? f[Math.min(k - 1, Math.floor(p * k))] : null)
      const top = [...byComm.entries()].map(([comm, sum]) => ({ comm, meanPct: Math.round(sum / Math.max(1, k)) })).filter((x) => x.meanPct >= 2).sort((a, b) => b.meanPct - a.meanPct).slice(0, 8)
      return { measured: k > 0, everyMs, samples: k, cores, foreignPct: { mean: k ? Math.round(f.reduce((a, b) => a + b, 0) / k) : null, p50: q(0.5), p95: q(0.95), max: k ? f[k - 1] : null }, top }
    },
  }
}

// Compilers, linkers and virtual machines: their presence in a round is a contamination, not background noise.
export const HEAVY = /^(rustc|cargo|ld|ld64|clang|cc1|cc|swift-frontend|java|qemu|com\.docker\.|Docker|com\.apple\.Virtualization|vfkit|node-gyp)/
// A round is a judge-worthy measurement only if the machine stayed awake, out of DarkWake, unthrottled, on AC, and
// with no build or VM beside it.
export function roundHealth(env) {
  const p = env?.machine, why = []
  if (!p) return { clean: null, why: ['machine state not recorded (run predates gbench power accounting, 2026-10-26)'] }
  if (p.during && !p.during.checked) why.push('power log unreadable')
  if (p.during?.reasons?.length) why.push(...p.during.reasons)
  // A build or a VM beside a round contaminates it even when the power state is perfect (10-26: rustc at 169% inside a GUI
  // round). Sustained foreign load over ~3 cores does too.
  const fc = p.foreignCpu
  if (fc?.measured) {
    const heavy = (fc.top ?? []).filter((t) => HEAVY.test(t.comm) && t.meanPct >= 25)
    if (heavy.length) why.push(`heavy foreign process(es): ${heavy.map((t) => `${t.comm} ${t.meanPct}%`).join(', ')}`)
    if (fc.foreignPct?.mean > 300) why.push(`busy machine: foreign CPU mean ${fc.foreignPct.mean}%`)
  }
  for (const k of ['start', 'end']) { const s = p[k]; if (s && s.cpuSpeedLimit != null && s.cpuSpeedLimit < 100) why.push(`CPU speed limit ${s.cpuSpeedLimit}% at ${k}`); if (s && s.source === 'battery') why.push(`on battery at ${k}`) }
  return { clean: why.length === 0, why }
}

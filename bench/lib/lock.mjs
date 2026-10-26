// The machine lock (SCHEMA §6). measure/ is taken with an atomic mkdir; dev work registers in dev/ and yields to it.
import { mkdirSync, writeFileSync, readFileSync, readdirSync, rmSync, existsSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'

export const LOCK_ROOT = path.join(process.env.GBENCH_CACHE ?? path.join(os.homedir(), '.cache', 'sophia-bench'), 'locks')
const MEASURE = path.join(LOCK_ROOT, 'measure'), DEV = path.join(LOCK_ROOT, 'dev')
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
export const alive = (pid) => { try { process.kill(pid, 0); return true } catch (e) { return e.code === 'EPERM' } }
const readJson = (f) => { try { return JSON.parse(readFileSync(f, 'utf8')) } catch { return null } }

export function devEntries() {
  if (!existsSync(DEV)) return []
  const out = []
  for (const f of readdirSync(DEV)) {
    const p = path.join(DEV, f), j = readJson(p)
    if (j && alive(j.pid)) out.push(j); else rmSync(p, { force: true }) // stale: anyone may remove it
  }
  return out
}
export function measureOwner() {
  const j = readJson(path.join(MEASURE, 'owner.json'))
  if (existsSync(MEASURE) && (!j || !alive(j.pid))) { rmSync(MEASURE, { recursive: true, force: true }); return null } // stale
  return j
}

// Dev/smoke work: wait while a measured run holds the machine, then register. Returns release().
export async function registerDev({ lane = 'core', label = '', log = () => {} } = {}) {
  mkdirSync(DEV, { recursive: true })
  while (measureOwner()) { log(`yielding to measured run ${JSON.stringify(measureOwner())}`); await sleep(15_000) }
  const f = path.join(DEV, `${lane}-${process.pid}.json`)
  writeFileSync(f, JSON.stringify({ pid: process.pid, lane, label, startedAt: new Date().toISOString() }))
  let done = false
  const release = () => { if (!done) { done = true; rmSync(f, { force: true }) } }
  process.once('exit', release)
  return release
}

// Measured run: take measure/, wait up to 10 min for dev/ to drain. Returns {release, contention}.
export async function takeMeasure({ lane = 'core', label = '', log = () => {}, waitMaxMs = 600_000 } = {}) {
  mkdirSync(LOCK_ROOT, { recursive: true })
  for (;;) {
    try { mkdirSync(MEASURE); break } catch (e) { if (e.code !== 'EEXIST') throw e }
    if (!measureOwner()) continue
    log(`another measured run holds the machine: ${JSON.stringify(measureOwner())}; waiting`); await sleep(15_000)
  }
  writeFileSync(path.join(MEASURE, 'owner.json'), JSON.stringify({ pid: process.pid, lane, label, startedAt: new Date().toISOString() }))
  let done = false
  const release = () => { if (!done) { done = true; rmSync(MEASURE, { recursive: true, force: true }) } }
  process.once('exit', release); for (const s of ['SIGINT', 'SIGTERM']) process.once(s, () => { release(); process.exit(130) })
  const t0 = Date.now(), seen = new Map()
  for (;;) {
    const d = devEntries().filter((e) => e.pid !== process.pid)
    for (const e of d) seen.set(e.pid, e)
    if (!d.length) break
    if (Date.now() - t0 > waitMaxMs) { log('dev entries still live after 10 min; proceeding with contention recorded'); break }
    log(`waiting for dev work to finish: ${d.map((e) => `${e.lane}:${e.pid}`).join(', ')}`); await sleep(10_000)
  }
  return { release, contention: { devEntriesSeen: [...seen.values()], waitedMin: Math.round(((Date.now() - t0) / 60000) * 10) / 10 } }
}

// Machine discipline for Docker work and measured runs: take the gbench measure lock for the whole duration,
// hold the Mac awake, optionally start Docker Desktop (and quit it again if we started it).
import { takeMeasure } from '../../lib/lock.mjs'
import { holdAwake, powerNow, powerDuring, foreignCpuSampler, roundHealth } from '../../lib/power.mjs'
import { exec, sleep, log } from './common.mjs'

export async function dockerUp() { const r = await exec('docker', ['version', '--format', '{{.Server.Version}}'], { allowFail: true }); return r.code === 0 && /^\d/.test(r.out.trim()) }

export async function ensureDocker() {
  if (await dockerUp()) return false
  log('starting Docker Desktop (we hold the measure lock)')
  await exec('open', ['-a', 'Docker'])
  for (let i = 0; i < 60; i++) { await sleep(5000); if (await dockerUp()) { log('Docker is up'); return true } }
  throw new Error('Docker did not come up in 5 minutes')
}
export async function quitDocker() { log('quitting Docker Desktop'); await exec('osascript', ['-e', 'quit app "Docker"'], { allowFail: true }) }

// Re-entrant: GBENCH_STD_SITTING=1 means an outer `sitting` already holds the lock.
export async function withMachine(label, fn, { docker = false, quitDockerAtEnd = true } = {}) {
  if (process.env.GBENCH_STD_SITTING) { if (docker) await ensureDocker(); return fn({ nested: true }) }
  const { release, contention } = await takeMeasure({ lane: 'std-harness', label, log })
  const awake = holdAwake(log)
  let started = false
  try {
    if (docker) started = await ensureDocker()
    return await fn({ contention, awake })
  } finally {
    if (started && quitDockerAtEnd) await quitDocker()
    awake.release(); release()
  }
}

// Wrap a measured phase: power + foreign CPU accounting. Returns {result, machine}.
export async function measured(fn, { awake } = {}) {
  const power0 = await powerNow(), sampler = foreignCpuSampler({ everyMs: 5000 })
  const t0 = Date.now(); sampler.start()
  let result
  try { result = await fn() } finally { var foreignCpu = sampler.stop() }
  const t1 = Date.now(), power1 = await powerNow(), during = await powerDuring(t0, t1)
  const machine = { awakeHeld: process.env.GBENCH_STD_SITTING ? 'by the sitting' : !!awake?.held, start: power0, end: power1, during, foreignCpu }
  machine.health = roundHealth({ machine })
  return { result, machine }
}

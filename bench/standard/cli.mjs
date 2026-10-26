#!/usr/bin/env node
// gbench standard: BSBM / SP2Bench / WatDiv over the SPARQL Protocol, W3C SPARQL 1.1 conformance, reference-store recipes.
// Standalone entry point (`node bench/standard/cli.mjs <cmd>`); wired into `gbench standard ...` once the measure lock is free.
import { spawn } from 'node:child_process'
import { parseArgs } from './lib/common.mjs'
import { takeMeasure } from '../lib/lock.mjs'
import { holdAwake } from '../lib/power.mjs'
import { ensureDocker, quitDocker } from './lib/machine.mjs'
import { log } from './lib/common.mjs'

const USAGE = `gbench standard <cmd>
  data <bsbm|sp2b|watdiv> --scale N [--force] [--mixes 550 --warmup 50 --seed 1] [--queries-per-template 20]
  store <oxigraph|fuseki|virtuoso|qlever|stardog|graphdb|rdfox> up|down|status [--cpus 8 --memory 12g --port N] [--purge]
  load --endpoint <query url> (--gsp <url> | --external --store <name>) --bench bsbm --scale N [--auth ..] [--chunk 250000]
  run <bsbm-explore|bsbm-explore-update|sp2b|watdiv> --endpoint <query url> [--update <url>] [--auth ..] [--clients 1|16]
      [--warmup N] [--runs N] [--timeout s] --label <l> [--scale N] [--store <name>] [--target-name n --target-version v] [--out dir]
  qualdiff <runDirA> <runDirB>
  conformance --endpoint <query url> [--update <url>] [--gsp <url>] [--auth ..] [--label l] [--only re] [--ignore f.json] [--write-ignore f.json] [--gsp-direct]
  sitting [--docker] -- <command...>     take the measure lock, hold the Mac awake (and Docker up) for the whole command; quits Docker if we started it`

const { pos, o } = parseArgs(process.argv.slice(2).filter((a) => a !== '--'))
const raw = process.argv.slice(2), cmd = pos[0]

async function sitting() {
  const i = raw.indexOf('--'); if (i < 0) throw new Error('sitting [--docker] -- <command...>')
  const flags = raw.slice(0, i), argv = raw.slice(i + 1)
  const { release } = await takeMeasure({ lane: 'std-harness', label: `sitting ${argv.slice(0, 4).join(' ')}`, log })
  const awake = holdAwake(log)
  let started = false
  try {
    if (flags.includes('--docker')) started = await ensureDocker()
    const code = await new Promise((res) => { const c = spawn(argv[0], argv.slice(1), { stdio: 'inherit', env: { ...process.env, GBENCH_STD_SITTING: '1' } }); c.on('close', res) })
    process.exitCode = code
  } finally { if (started) await quitDocker(); awake.release(); release() }
}

try {
  if (cmd === 'sitting') await sitting()
  else if (cmd === 'data') await (await import('./data.mjs')).dataCmd(pos.slice(1), o)
  else if (cmd === 'store') await (await import('./store.mjs')).storeCmd(pos.slice(1), o)
  else if (cmd === 'load') await (await import('./load.mjs')).loadCmd(pos.slice(1), o)
  else if (cmd === 'run') await (await import('./run.mjs')).runCmd(pos.slice(1), o)
  else if (cmd === 'qualdiff') (await import('./run.mjs')).qualdiffCmd(pos.slice(1))
  else if (cmd === 'conformance') await (await import('./conformance.mjs')).conformanceCmd(pos.slice(1), o)
  else { console.error(USAGE); process.exitCode = cmd ? 2 : 0 }
} catch (e) { console.error(`error: ${e.message}`); if (process.env.GBENCH_DEBUG) console.error(e.stack); process.exitCode = 1 }

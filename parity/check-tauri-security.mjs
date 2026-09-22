#!/usr/bin/env node
import fs from 'node:fs/promises'
import path from 'node:path'

const here = path.dirname(new URL(import.meta.url).pathname)
const prototypeRoot = path.resolve(here, '..')
const tauriRoot = path.join(prototypeRoot, 'src-tauri')
const tauriConfigPath = path.join(tauriRoot, 'tauri.conf.json')
const defaultCapabilityPath = path.join(tauriRoot, 'capabilities/default.json')

const tauriConfig = JSON.parse(await fs.readFile(tauriConfigPath, 'utf8'))
const defaultCapability = JSON.parse(await fs.readFile(defaultCapabilityPath, 'utf8'))

const csp = tauriConfig.app?.security?.csp
const permissions = defaultCapability.permissions ?? []
const errors = []

// Single-process profile-dir lock: the lock module must exist and the
// runtime must actually acquire it. Without this, two Tauri instances on the
// same profile dir would corrupt Y.Docs / Oxigraph stores silently.
const profileLockPath = path.join(tauriRoot, 'src/profile_lock.rs')
const tauriRuntimePath = path.join(tauriRoot, 'src/tauri_runtime.rs')
try {
  await fs.access(profileLockPath)
} catch {
  errors.push('src-tauri/src/profile_lock.rs is missing — profile-dir lock module must exist')
}
try {
  const runtimeSource = await fs.readFile(tauriRuntimePath, 'utf8')
  if (!runtimeSource.includes('acquire_profile_lock')) {
    errors.push('tauri_runtime.rs must call acquire_profile_lock during setup_native_app')
  }
} catch (error) {
  errors.push(`failed to read tauri_runtime.rs: ${error.message}`)
}

function requireCspDirective(fragment) {
  if (typeof csp !== 'string' || !csp.includes(fragment)) {
    errors.push(`CSP must include ${fragment}`)
  }
}

if (typeof csp !== 'string' || csp.trim() === '') {
  errors.push('Tauri CSP must be a non-empty string')
} else {
  requireCspDirective("default-src 'self'")
  requireCspDirective("script-src 'self'")
  requireCspDirective("object-src 'none'")
  requireCspDirective("base-uri 'self'")
  requireCspDirective("frame-ancestors 'none'")
}

if (permissions.includes('core:default')) {
  errors.push('default capability must not grant core:default')
}

const unexpectedCorePermissions = permissions.filter((permission) => (
  permission.startsWith('core:')
  && permission !== 'core:event:allow-listen'
  && permission !== 'core:event:allow-unlisten'
))
if (unexpectedCorePermissions.length > 0) {
  errors.push(`default capability grants unexpected core permissions: ${unexpectedCorePermissions.join(', ')}`)
}

if (errors.length > 0) {
  throw new Error(`Tauri security baseline failed:\n${errors.join('\n')}`)
}

console.log(JSON.stringify({
  ok: true,
  csp: 'non-empty',
  defaultCapabilityPermissions: permissions.length,
  profileLockPresent: true,
  profileLockAcquired: true,
}, null, 2))

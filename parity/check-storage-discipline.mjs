#!/usr/bin/env node
import fs from 'node:fs/promises'
import path from 'node:path'

const here = path.dirname(new URL(import.meta.url).pathname)
const rustSourceDir = path.resolve(here, '../src-tauri/src')

async function walk(dir) {
  const entries = await fs.readdir(dir, { withFileTypes: true })
  const files = []
  for (const entry of entries) {
    const fullPath = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      files.push(...await walk(fullPath))
    } else if (entry.name.endsWith('.rs')) {
      files.push(fullPath)
    }
  }
  return files
}

function findMatchingBrace(source, openBraceIndex) {
  let depth = 0
  let state = 'code'
  let blockCommentDepth = 0
  let rawStringEnd = null
  for (let index = openBraceIndex; index < source.length; index += 1) {
    const char = source[index]

    if (state === 'line-comment') {
      if (char === '\n') state = 'code'
      continue
    }
    if (state === 'block-comment') {
      if (source.startsWith('/*', index)) {
        blockCommentDepth += 1
        index += 1
      } else if (source.startsWith('*/', index)) {
        blockCommentDepth -= 1
        index += 1
        if (blockCommentDepth === 0) state = 'code'
      }
      continue
    }
    if (state === 'string') {
      if (char === '\\') index += 1
      else if (char === '"') state = 'code'
      continue
    }
    if (state === 'raw-string') {
      if (source.startsWith(rawStringEnd, index)) {
        index += rawStringEnd.length - 1
        state = 'code'
        rawStringEnd = null
      }
      continue
    }

    if (source.startsWith('//', index)) {
      state = 'line-comment'
      index += 1
      continue
    }
    if (source.startsWith('/*', index)) {
      state = 'block-comment'
      blockCommentDepth = 1
      index += 1
      continue
    }
    const rawString = source.slice(index).match(/^(?:b)?r(#{0,16})"/)
    if (rawString) {
      state = 'raw-string'
      rawStringEnd = `"${rawString[1]}`
      index += rawString[0].length - 1
      continue
    }
    if (char === '"') {
      state = 'string'
      continue
    }
    if (char === "'") {
      const charLiteral = source.slice(index).match(/^'(?:\\.|[^'\\])'/s)
      if (charLiteral) {
        index += charLiteral[0].length - 1
        continue
      }
    }
    if (char === '{') depth += 1
    if (char === '}') {
      depth -= 1
      if (depth === 0) return index
    }
  }
  return -1
}

function testModuleRanges(source) {
  const ranges = []
  const testModuleRegex = /#\s*\[\s*cfg\s*\(([\s\S]*?)\)\s*\]\s*mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{/g
  let match
  while ((match = testModuleRegex.exec(source))) {
    if (!/\btest\b/.test(match[1])) continue
    const openBraceIndex = source.indexOf('{', match.index)
    const closeBraceIndex = findMatchingBrace(source, openBraceIndex)
    if (openBraceIndex !== -1 && closeBraceIndex !== -1) {
      ranges.push([openBraceIndex, closeBraceIndex])
      testModuleRegex.lastIndex = closeBraceIndex + 1
    }
  }
  return ranges
}

function externalTestModuleNames(source) {
  const names = []
  const moduleRegex = /#\s*\[\s*cfg\s*\(([\s\S]*?)\)\s*\]\s*mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;/g
  let match
  while ((match = moduleRegex.exec(source))) {
    if (/\btest\b/.test(match[1])) names.push(match[2])
  }
  return names
}

function externalModuleCandidates(declaringFile, moduleName) {
  const directory = path.dirname(declaringFile)
  const basename = path.basename(declaringFile, '.rs')
  const moduleDirectory = ['lib', 'main', 'mod'].includes(basename)
    ? directory
    : path.join(directory, basename)
  return [
    path.join(moduleDirectory, `${moduleName}.rs`),
    path.join(moduleDirectory, moduleName, 'mod.rs'),
  ]
}

function lineForIndex(source, index) {
  return source.slice(0, index).split('\n').length
}

function inRanges(index, ranges) {
  return ranges.some(([start, end]) => index >= start && index <= end)
}

const prohibitedPatterns = [
  { name: 'fs::write', pattern: /\b(?:std::)?fs::write\s*\(/g },
  { name: 'tokio::fs::write', pattern: /\btokio::fs::write\s*\(/g },
  { name: 'multipart field.bytes().await', pattern: /\bfield\.bytes\s*\(\s*\)\s*\.await/g },
  { name: 'read pending upload into memory', pattern: /\bread_bytes\s*\(\s*&pending_original_path\s*\)/g },
  { name: 'queued upload dataBase64 handoff', pattern: /"dataBase64"\s*:\s*BASE64_STANDARD\.encode\s*\(/g },
  // Atomicity discipline: sync_parent_dir returns Result; swallowing it via
  // `let _ = ...` reintroduces the silent-fsync-error class we just removed.
  // Production callers must propagate via `?` (or destructure explicitly).
  { name: 'let _ = sync_parent_dir', pattern: /let\s+_\s*=\s*sync_parent_dir\s*\(/g },
]

const expectedProductionExceptions = new Map([
  ['artifact_ingest_payloads.rs::queued upload dataBase64 handoff', {
    count: 1,
    reason: 'stored artifact re-ingest is still parser-bound; new upload/PDF paths must use pendingOriginalPath handoff',
  }],
])

const violations = []
let checkedFiles = 0
let testOnlyDirectWrites = 0
const productionExceptions = new Map()

const rustFiles = await walk(rustSourceDir)
const rustSources = new Map(await Promise.all(rustFiles.map(async (filePath) => [
  filePath,
  await fs.readFile(filePath, 'utf8'),
])))
const rustFileSet = new Set(rustFiles)
const externalTestModuleFiles = new Set()
for (const [filePath, source] of rustSources) {
  for (const moduleName of externalTestModuleNames(source)) {
    for (const candidate of externalModuleCandidates(filePath, moduleName)) {
      if (rustFileSet.has(candidate)) externalTestModuleFiles.add(candidate)
    }
  }
}

for (const filePath of rustFiles) {
  checkedFiles += 1
  const source = rustSources.get(filePath)
  const testRanges = externalTestModuleFiles.has(filePath)
    ? [[0, source.length]]
    : testModuleRanges(source)
  const relativeFile = path.relative(rustSourceDir, filePath)
  for (const { name, pattern } of prohibitedPatterns) {
    pattern.lastIndex = 0
    let match
    while ((match = pattern.exec(source))) {
      if (inRanges(match.index, testRanges)) {
        testOnlyDirectWrites += 1
        continue
      }
      const exceptionKey = `${relativeFile}::${name}`
      if (expectedProductionExceptions.has(exceptionKey)) {
        productionExceptions.set(exceptionKey, (productionExceptions.get(exceptionKey) ?? 0) + 1)
        continue
      }
      violations.push({
        file: relativeFile,
        line: lineForIndex(source, match.index),
        call: name,
      })
    }
  }
}

for (const [key, expected] of expectedProductionExceptions.entries()) {
  const actual = productionExceptions.get(key) ?? 0
  if (actual !== expected.count) {
    violations.push({
      file: key.split('::')[0],
      line: null,
      call: key.split('::').slice(1).join('::'),
      expectedExceptionCount: expected.count,
      actualExceptionCount: actual,
      reason: expected.reason,
    })
  }
}

if (violations.length > 0) {
  throw new Error(`Production code must use storage/upload helpers and pending-file handoff instead of direct fs writes, buffered multipart reads, or queued upload base64 copies:\n${JSON.stringify(violations, null, 2)}`)
}

console.log(JSON.stringify({
  ok: true,
  checkedFiles,
  prohibitedProductionWrites: 0,
  allowedProductionExceptions: [...productionExceptions.entries()].map(([key, count]) => ({
    key,
    count,
    reason: expectedProductionExceptions.get(key)?.reason,
  })),
  testOnlyDirectWrites,
}, null, 2))

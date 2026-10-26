import { writeFileSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { IT, SUITES, graphFor, incarnation, waitJob, timedRun, stub } from './_rich.mjs'
import { markdownDoc, plainText, makeTextPdf } from '../../fixture/parts/rich-gen.mjs'

const seed = (ctx) => ctx.fixture.seed ?? 1
const mk = (id, it, extra) => ({ id, version: 1, kind: 'rich', suites: SUITES, params: IT(...it), needs: [], ...extra })
const dir = (ctx) => { const d = path.join(ctx.runDir, 'rich-assets'); mkdirSync(d, { recursive: true }); return d }
const create = (ctx, id, kib, op = id) => ctx.mcp('create_artifact_text', { graph_id: ctx.graphId, graph_incarnation: ctx.S.inc, artifact_id: id, operation_id: `op-${op}`, filename: `${id}.txt`, mime_type: 'text/plain', text: plainText(kib, seed(ctx)) })

export const cases = [
  // Text artifacts (original-byte records with revisions): create, write (compare-and-swap on the content hash), read, at 10 KiB and 1 MiB.
  ...[10, 1024].flatMap((kib) => [
    mk(`rich.artifact.text_create.k${kib}`, [[3, 1], [kib > 100 ? 8 : 20, 2]], { async setup(ctx) { await graphFor(ctx, 'atc' + kib); ctx.S = { inc: await incarnation(ctx) } }, op: (ctx, i) => create(ctx, `art-${ctx.tag}-${i}`, kib) }),
    mk(`rich.artifact.text_write.k${kib}`, [[3, 1], [kib > 100 ? 8 : 20, 2]], {
      async setup(ctx) { await graphFor(ctx, 'atw' + kib); ctx.S = { inc: await incarnation(ctx), id: `art-${ctx.tag}-w` }; ctx.S.hash = (await create(ctx, ctx.S.id, kib, 'w0')).contentHashSha256 },
      op: async (ctx, i) => { const r = await ctx.mcp('write_artifact_text', { graph_id: ctx.graphId, graph_incarnation: ctx.S.inc, artifact_id: ctx.S.id, operation_id: `op-w${i + 1}`, expected_content_sha256: ctx.S.hash, text: plainText(kib, seed(ctx) + i + 1) }); ctx.S.hash = r.contentHashSha256 } }),
    mk(`rich.artifact.text_read.k${kib}`, [[5, 1], [30, 3]], { async setup(ctx) { await graphFor(ctx, 'atr' + kib); ctx.S = { inc: await incarnation(ctx), id: `art-${ctx.tag}-r` }; await create(ctx, ctx.S.id, kib, 'r0') },
      op: (ctx) => ctx.mcp('read_artifact', { graphId: ctx.graphId, artifact_id: ctx.S.id }) }),
  ]),
  // Upload a markdown file: copy, async job, parse into blocks, workspace entry, room write, projections. 100 vs 2,000 paragraphs. Time = submit + job wait.
  ...[100, 2000].map((p) => mk(`rich.artifact.upload_md.p${p}`, [[2, 1], [p > 500 ? 5 : 12, 1]], {
    async setup(ctx) { await graphFor(ctx, 'aum' + p); ctx.S = { files: [] }; const d = dir(ctx); for (let k = 0; k < ctx.params.iterations + ctx.params.warmup; k++) { const f = path.join(d, `up-${p}-${k}.md`); writeFileSync(f, markdownDoc(p, seed(ctx) + k)); ctx.S.files.push(f) } },
    op: async (ctx, i) => waitJob(ctx, await ctx.mcp('upload_artifact', { graphId: ctx.graphId, file_path: ctx.S.files[i], label: `up ${i}` })) })),
  // Re-ingest an uploaded markdown artifact (update the auto-ingested document), 300 paragraphs.
  mk('rich.artifact.reingest_md.p300', [[2, 1], [8, 1]], {
    async setup(ctx) { await graphFor(ctx, 'ari'); const f = path.join(dir(ctx), 'reingest.md'); writeFileSync(f, markdownDoc(300, seed(ctx))); const r = await waitJob(ctx, await ctx.mcp('upload_artifact', { graphId: ctx.graphId, file_path: f })); ctx.S = { id: r.artifactId ?? r.documentId } },
    op: async (ctx) => waitJob(ctx, await ctx.mcp('ingest_artifact', { graphId: ctx.graphId, artifact_id: ctx.S.id })) }),
  // PDF ingest with a real text layer (hand-written PDF, 5 vs 50 pages). The headless cell has no parser pool unless GARDEN_PARSER_URL is set:
  // the case probes with one upload and skips with that reason if the job fails for it.
  ...[5, 50].map((pg) => ({ id: `rich.artifact.ingest_pdf.p${pg}`, version: 1, kind: 'rich', suites: SUITES, params: IT([1, 0], [pg > 20 ? 3 : 6, 1]), needs: [],
    async run(ctx) {
      await graphFor(ctx, 'apdf' + pg); const d = dir(ctx), files = []
      for (let k = 0; k < ctx.params.iterations + ctx.params.warmup + 1; k++) { const f = path.join(d, `t-${pg}-${k}.pdf`); writeFileSync(f, makeTextPdf({ pages: pg, seed: seed(ctx) + k })); files.push(f) }
      try { await waitJob(ctx, await ctx.mcp('upload_artifact', { graphId: ctx.graphId, file_path: files[files.length - 1] })) }
      catch (e) { if (/parser pool|GARDEN_PARSER_URL/i.test(e.message)) return { skipped: 'PDF ingest needs the parser pool (set GARDEN_PARSER_URL); this headless gardend has none' }; throw e }
      return timedRun(ctx, ctx.params, async (c, i) => waitJob(c, await c.mcp('upload_artifact', { graphId: c.graphId, file_path: files[i] })))
    } })),
  stub('rich.artifact.scan_text_layer.p50', 'STUB: needs a PDF original in the graph (PDF upload needs GARDEN_PARSER_URL) and GARDEN_PDFIUM_LIB for read_scan_text; OCR needs raster pages, which would need a dependency'),
  stub('rich.artifact.read_scan_text.ocr', 'STUB: OCR needs raster pages (no dependency-free writer) plus pdfium and tesseract; the PDF writer in fixture/parts/rich-pdf.mjs makes text-layer pages only'),
]

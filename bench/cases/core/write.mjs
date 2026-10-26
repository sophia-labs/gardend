import { IT, small, SMOKE, paragraphs } from './_util.mjs'
import { mulberry32 } from '../../lib/stats.mjs'
const mk = (id, it, op, extra = {}) => ({ id, version: 1, kind: 'write', suites: SMOKE, params: IT(...it), needs: ['book', 'small'], op, ...extra })
const parent = (ctx) => (ctx.fixture.runFolder ? { parentId: ctx.fixture.runFolder } : {})
export const cases = [
  mk('write.insert_blocks.small', [[10, 2], [60, 5]], (ctx, i) => ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: small(ctx, i).id, content: `gbench block ${ctx.tag} ${i}`, format: 'plain' })),
  mk('write.insert_blocks.book', [[8, 2], [40, 3]], (ctx, i) => ctx.mcp('insert_blocks', { graphId: ctx.graphId, documentId: ctx.fixture.book.id, content: `gbench block ${ctx.tag} ${i}`, format: 'plain', index: Math.floor(ctx.fixture.book.blockIds.length / 2) })),
  mk('write.edit_block_text.small', [[10, 2], [60, 5]], (ctx, i) => { const d = small(ctx, i); return ctx.mcp('edit_block_text', { graphId: ctx.graphId, documentId: d.id, blockId: d.blockIds[1], operations: [{ type: 'insert', offset: 0, text: 'x' }] }) }),
  mk('write.edit_block_text.book', [[10, 2], [60, 5]], (ctx, i) => ctx.mcp('edit_block_text', { graphId: ctx.graphId, documentId: ctx.fixture.book.id, blockId: ctx.fixture.book.blockIds[(i * 131) % ctx.fixture.book.blockIds.length], operations: [{ type: 'insert', offset: 0, text: 'x' }] })),
  mk('write.edit_comment.small', [[8, 2], [40, 3]], (ctx, i) => { const d = small(ctx, i); return ctx.mcp('edit_comment', { graphId: ctx.graphId, documentId: d.id, comment_id: `gb-${ctx.tag}-c${i}`, action: 'set', text: `gbench comment ${i}`, author: 'gbench', blockId: d.blockIds[0] }) }),
  mk('write.create_document', [[6, 1], [30, 2]], (ctx, i) => ctx.mcp('create_document', { graphId: ctx.graphId, title: `gbench doc ${ctx.tag} ${i}`, documentId: `gb-${ctx.tag}-cd${i}`, ...parent(ctx) }), { needs: [] }),
  mk('write.write_document.medium', [[6, 1], [30, 2]], (ctx, i) => ctx.mcp('write_document', { graphId: ctx.graphId, documentId: `gb-${ctx.tag}-wd`, content: paragraphs(mulberry32(i + 7), ctx.lex, 30), format: 'markdown' }),
    { needs: [], setup: (ctx) => ctx.mcp('create_document', { graphId: ctx.graphId, title: 'gbench write target', documentId: `gb-${ctx.tag}-wd`, ...parent(ctx) }) }),
  mk('write.create_wires', [[8, 2], [40, 3]], (ctx, i) => ctx.mcp('create_wires', { graphId: ctx.graphId, sourceDocumentId: small(ctx, i).id, targetDocumentId: small(ctx, i * 3 + 5).id, predicate: 'relatesTo' })),
  mk('write.create_folder', [[6, 1], [30, 2]], (ctx, i) => ctx.mcp('create_folder', { graphId: ctx.graphId, name: `gbench shelf ${ctx.tag} ${i}`, folderId: `gb-${ctx.tag}-f${i}`, ...parent(ctx) }), { needs: [] }),
  mk('write.create_restore_point', [[2, 0], [10, 1]], (ctx, i) => ctx.mcp('create_restore_point', { graphId: ctx.graphId, label: `gbench ${ctx.tag} ${i}` }), { needs: [] }),
]

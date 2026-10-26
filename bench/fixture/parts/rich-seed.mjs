// Fixture part: a 50-shape whiteboard and 20 typed objects (a Gizmo/Widget ontology) in the bench graph, so other lanes (gui.board.*,
// gui.object.*) have ids to open. The rich.* cases build their own sized data in setup() and do not depend on this.
import { boardBatches, ONTOLOGY_CLASSES, gizmoRecords } from './rich-gen.mjs'
export const name = 'rich'
export async function seed(ctx) {
  const b = await ctx.mcp('whiteboard_create', { graphId: ctx.graphId, title: `Bench board ${ctx.prefix}`, ...(ctx.folder ? { parentId: ctx.folder } : {}) })
  for (const moves of boardBatches(50, ctx.seed)) await ctx.mcp('whiteboard_apply', { graphId: ctx.graphId, documentId: b.documentId, rationale: 'bench seed', moves })
  await ctx.mcp('propose_domain_ontology', { graphId: ctx.graphId, vocab: 'workspace', classes: ONTOLOGY_CLASSES(), rationale: 'bench seed' })
  const w = await ctx.mcp('emporium_write', { graphId: ctx.graphId, vocab: 'workspace', records: gizmoRecords(20, ctx.seed) })
  return { board: b.documentId, boardShapes: 50, objects: (w.results ?? []).map((r) => r.subject), objectVocab: 'workspace', objectClass: 'Gizmo' }
}

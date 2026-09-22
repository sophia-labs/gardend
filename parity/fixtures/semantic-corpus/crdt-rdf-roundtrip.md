# CRDT RDF Roundtrip Notes

The document editing path treats the Y.Doc as the operational authority. A
local document should not be considered complete just because a JSON manifest or
RDF record exists. The expected shape is a Y.XmlFragment named `content`, a
TipTap JSON projection, a TipTap XML projection, a DocumentTree snapshot, flat
blocks, body text, and RDF triples derived from the same source state.

## Materialization Contract

- The active editor writes collaborative operations into the document Y.Doc.
- A managed local channel serializes transactions and debounces persistence.
- The materializer projects the same Y.Doc into TipTap XML for inspection.
- The DocumentTree projection preserves block IDs, text, marks, and structured attrs.
- RDF materialization owns content nodes under document-local subjects.
- Workspace materialization owns containment, folder placement, wires, and source-file metadata.

The important parity point is that hosted and local stacks can disagree about
storage but should not disagree about observable document semantics. Hosted mode
may persist Redis envelopes, S3 snapshots, worker job results, and Hocuspocus
state. Local mode may persist filesystem update blobs and sidecar snapshots. In
both cases a read adapter should return the same document title, block order,
parent relationships, inline marks, and RDF content subjects.

## Failure Modes

When the local runtime skips the Y.Doc layer, subtle gaps appear. The editor may
open stale text even though semantic search sees new blocks. RDF may contain
fresh triples while navigation still displays an old title. A direct manifest
write can also bypass future merge logic, making hosted sync harder because
there is no operation history to reconcile.

## Test Queries

Use phrases like `Y.XmlFragment content authority`, `DocumentTree projection`,
`RDF content subjects`, and `block order parent relationships` to verify that
semantic search returns this document. Exact keyword search should also find
`Hocuspocus`, `TipTap XML`, and `managed local channel`.

## Fixture Tasks

- [ ] Create a deterministic document ID for roundtrip tests.
- [ ] Verify blocks contain stable `data-block-id` attributes.
- [ ] Confirm semantic search indexes each heading and paragraph block.
- [ ] Compare hosted and local block-context responses for heading-heavy input.

```sparql
SELECT ?block ?text WHERE {
  ?block <http://mnemosyne.dev/doc#textContent> ?text .
  FILTER(CONTAINS(LCASE(STR(?text)), "documenttree"))
}
LIMIT 10
```

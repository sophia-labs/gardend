# rich.* cases: gardend as a headless engine for its fancier data types

Written by la Tasadora against SCHEMA §5/§7. Every case is self-contained: `setup()` (untimed) builds its own sized data, in a
dedicated graph in spawn mode (so the size is the variable, not whatever else the fixture holds) and in the bench graph in
attach mode. `op` is the one expensive call. Data is synthetic and deterministic from `ctx.fixture.seed`
(`bench/fixture/parts/rich-gen.mjs`; syllable-soup words, no real text).

Suites: `standard` and `rich`. Run only these with `--suite rich`. Cost class is a guess before the baseline: S < 50 ms,
M 50 ms-1 s, L 1-10 s, XL > 10 s. "Iter" is measured+warmup for the standard suite (smoke override in `params.smoke`).

Scaling pairs are listed together; compare the two ids to read the slope. Cases with a `slope` in the result:
`rich.whiteboard.apply_growth` (`msPerHundredShapes`).

## whiteboards (`whiteboard_*`, `flow_seed_board`)

A board is a Y.Doc of Excalidraw elements stored as JSON strings; every apply re-derives the outline, blocks and RDF lane.
Boards of N shapes have about 2.6N elements (bound text, arrows).

| id | measures | fixture | iter | cost |
|---|---|---|---|---|
| `rich.whiteboard.create` | whiteboard_create (new document + room) | none | 15+2 | L |
| `rich.whiteboard.apply_small.n50` | one agent-sized edit, 10 moves, explicit positions, on a 50-shape board | board 50 | 25+3 | M |
| `rich.whiteboard.apply_small.n1000` | the same edit on a 1,000-shape board; ratio to n50 is the board-size tax | board 1000 | 25+3 | L |
| `rich.whiteboard.apply_free.n1000` | the default agent path: adds with no position (free-space search over all shapes) on 1,000 shapes | board 1000 | 25+3 | L |
| `rich.whiteboard.apply_batch195` | the tool's maximum, 195 moves (130 shapes, 65 arrows), onto an empty board; fresh board per iteration | boards made in setup | 12+2 | L |
| `rich.whiteboard.apply_growth` | one board, +100 shapes per iteration to 1,500; **slope** in ms per 100 shapes already on the board | one empty board | 15+0 | L |
| `rich.whiteboard.read.n50` / `.n1000` | whiteboard_read outline (live room), 50 vs 1,000 shapes | boards | 30+3 | S / M |
| `rich.whiteboard.read_elements.n1000` | whiteboard_read format=elements (the whole element JSON) | board 1000 | 30+3 | M |
| `rich.whiteboard.flow_seed.n20` / `.n400` | flow_seed_board replace=true of a Mithras Flow board (20 vs 400 systems, 2 requirements each) | flow JSON | 10+2 | M |

## Emporium typed objects (`propose_domain_ontology`, `emporium_*`, `sparql_query_named`)

Fixture: a 2-class ontology (Gizmo: string, integer, dateTime, multi-valued string; Widget) and Gizmo objects, written 100 per call.

| id | measures | fixture | iter | cost |
|---|---|---|---|---|
| `rich.emporium.propose_ontology` | derive SHACL, compile-check, store a new immutable version (5 classes) | empty graph | 12+2 | M |
| `rich.emporium.write.b10` / `.b100` | emporium_write of 10 / 100 records onto a graph already holding 500 objects (I1 gate, SHACL, minting, journal) | 500 Gizmos | 20+2 / 8+2 | M |
| `rich.emporium.write.b10_empty` | the same 10-record write onto an empty graph; the difference from b10 is the per-object tax of the 500 present | 0 objects | 20+2 | M |
| `rich.emporium.query_contains.n10` / `.n500` | hydrated object query, `contains` filter (scan), limit 25 | 10 / 500 Gizmos | 30+3 | S / S-M |
| `rich.emporium.query_eq.n10` / `.n500` | the same with an `eq` filter | same | 30+3 | S |
| `rich.emporium.read.n500`, `list.n500` | emporium_read one object; emporium_list a page of 100 | 500 Gizmos | 30+3 | S |
| `rich.emporium.named_countBy.n500` | sparql_query_named countBy(weight) | 500 Gizmos | 30+3 | S |
| `rich.emporium.sweep.m50` / `.m1000` | §8.1 conformance sweep over the memory projection, 50 vs 1,000 memories | memories | 12+2 | S |
| `rich.emporium.violations` | read the violation ledger (after a sweep over 100 memories) | memories | 30+3 | S |
| `rich.emporium.retract.n500` | retraction event on a subject found by a store-wide scan (500 objects + 500 memories); one fresh subject per iteration | both | 25+3 | S |

Not included: `currentHeads` (Gizmo declares no lineage/isCurrent pair, so the tool refuses it; it applies to memory-like classes).

## workflows (`workflow_plan_*`, `workflow_run_record`)

| id | measures | fixture | iter | cost |
|---|---|---|---|---|
| `rich.workflow.plan_create` | workflow_plan_create (document + RDF draft + report) | none | 10+2 | L |
| `rich.workflow.plan_compose.p3x3` / `.p6x2` | compose a 3-phase/9-step plan (about 16 moves); 6-phase/12-step; fresh plan document per iteration | plans made in setup | 10+2 | M |
| `rich.workflow.plan_read` | read a composed plan back (SHACL report included) | plan | 30+3 | S |
| `rich.workflow.run_record.i50` / `.i1000` | record a finished run: Turtle parse plus one transaction replacing the run graph; 50 vs 1,000 items | source doc + plan | 10+2 | M |
| `rich.workflow.plan_publish.i50` / `.i300` | publish items as margin comments, anchoring each quote in a 50 / 300-paragraph document; re-publishing updates | source doc, plan, recorded run | 8+1 | M-L |
| `rich.workflow.book` | STUB: `workflow_book_*`, `workflow_run_start` need an authored script block and a Choreograph runner | | | |

## artifacts and datasheets

| id | measures | fixture | iter | cost |
|---|---|---|---|---|
| `rich.artifact.text_create.k10` / `.k1024` | create_artifact_text, 10 KiB vs 1 MiB | none | 20+2 / 8+2 | L |
| `rich.artifact.text_write.k10` / `.k1024` | write_artifact_text (CAS on content hash, new revision) | one artifact | 20+2 / 8+2 | L |
| `rich.artifact.text_read.k10` / `.k1024` | read_artifact | one artifact | 30+3 | S |
| `rich.artifact.upload_md.p100` / `.p2000` | upload_artifact of a markdown file and the async job to completion (parse, blocks, workspace entry, room, projections); time = submit + 25 ms polling | files written to runDir in setup | 12+1 / 5+1 | L / XL |
| `rich.artifact.reingest_md.p300` | ingest_artifact on the auto-ingested upload, job to completion | one upload | 8+1 | L |
| `rich.artifact.ingest_pdf.p5` / `.p50` | upload of a hand-written text-layer PDF (5 vs 50 pages), job to completion. **Skips** when the cell has no parser pool (no `GARDEN_PARSER_URL`), which is the case for `gardend serve` as installed | `makeTextPdf` | 6+1 / 3+1 | XL |
| `rich.artifact.scan_text_layer.p50`, `rich.artifact.read_scan_text.ocr` | STUBS: need a PDF original in the graph (needs the parser pool), `GARDEN_PDFIUM_LIB`, and for OCR raster pages (no dependency-free writer) plus tesseract | | | |
| `rich.datasheet.import.r20` / `.r400` | datasheet_import of an autonotes.data/2 envelope with 20 vs 400 spec rows (assertion nodes, typed literals, replace-whole graph) | synthetic envelope | 15+2 / 8+2 | L |
| `rich.datasheet.facts.r400` | datasheet_facts by part and parameter after a 400-row import | imported sheet | 30+3 | S-M |

## semantic and memory

The default model is `fastembed/qdrant/bge-small-en-v1.5-onnx-q` (a few tens of MB). The 1.8 GB `nomic-embed-text-v2-moe` is never
selected by these cases. Set `GBENCH_SKIP_MODEL=1` to skip every case that prepares a model (the machine disk was 95% full).

| id | measures | fixture | iter | cost |
|---|---|---|---|---|
| `rich.semantic.prepare_model` | prepare_semantic_model job (loads, and the first time may fetch, the model) | none | 1+0 | L |
| `rich.semantic.index_cold.b60` / `.b300` | refresh_semantic_index from nothing over 60 vs 300 paragraphs; one build each | corpus | 1+0 | L / XL |
| `rich.semantic.index_refresh.b300` | incremental refresh after adding 5 paragraphs (untimed) to a 300-block index | indexed corpus | 6+1 | L |
| `rich.semantic.search.b300` | semantic_search over the 300-block index, 4 queries | indexed corpus | 30+3 | S-M |
| `rich.semantic.reason` | STUB: semantic_reason needs a head/tail IRI plus a relation from a scaffold we do not generate | | | |
| `rich.memory.remember` | one `remember` | none | 30+3 | M |
| `rich.memory.remember_batch.b100` | remember_batch of 100 | none | 12+2 | M-L |
| `rich.memory.recall.m50` / `.m1000` | recall by substring over 50 vs 1,000 memories | memories | 30+3 | S |
| `rich.memory.semantic_recall.m50` / `.m1000` | memory_semantic_recall (embeds the query, cosine over memories) | memories + model | 20+2 | S-M |

## RDF, wires, restore points, values

| id | measures | fixture | iter | cost |
|---|---|---|---|---|
| `rich.rdf.load.t10k` / `.t100k` | rdf_load N-Triples (about 1 MB / 10 MB body) into a fresh named graph per iteration; dropped in teardown | generated N-Triples | 8+1 / 3+0 | M / XL |
| `rich.rdf.dump.t10k` / `.t100k` | rdf_dump of one named graph | loaded graph | 10+1 / 5+1 | M / L |
| `rich.rdf.update_insert.t1k` | sparql_update INSERT DATA of 1,000 triples | lines | 15+2 | M |
| `rich.rdf.update_where.t10k` | DELETE/INSERT WHERE rewriting every integer in a 10k-triple graph | loaded graph | 8+1 | M-L |
| `rich.wires.traverse.d2` / `.d6` | traverse_wires BFS depth 2 / 6 over 300 documents and 900 wires | wired graph | 20+2 | S-M |
| `rich.restore_point.diff.n20` / `.n200` | diff_restore_points between two points with 5 documents edited between, graph of 20 vs 200 documents | two restore points | 10+2 | M / L |
| `rich.values.value.b100` | `value` over 100 blocks | 10 docs | 15+2 | M |
| `rich.values.revaluate` | revaluate (rewrite scoring config, archive the old one) | none | 15+2 | S |

## swarm store (local profiles only)

`rich.swarm.load`, `rich.swarm.query`: STUBS. The installed engine (019463f) predates the swarm store (commit b334717) and has no
`swarm_*` tools. When an engine with them is the target the cases should: write an N-Quads file plus a vector package
(manifest `{schema_version:1, rdf, rdf_sha256, vectors:[{file, sha256, dimensions, rows}]}`, little-endian float32 rows), time
`swarm_load` of it, then `swarm_query` / `swarm_traverse` with `exclude_graphs` under 8-way concurrency (the point of the store is that reads
never touch the CRDT queue). See `docs/swarm-store-design.md`.

## The fixture part

`bench/fixture/parts/rich-seed.mjs` (name `rich`) adds to `ctx.fixture`: `board` (a 50-shape whiteboard document id), `boardShapes`,
`objects` (20 Gizmo subject IRIs), `objectVocab` (`workspace`), `objectClass` (`Gizmo`). It exists for other lanes (gui.board.*,
gui.object.*); the rich.* cases do not read it. `rich-gen.mjs` (generators) and `rich-pdf.mjs` (PDF writer) have no `seed()` and are skipped by the loader.

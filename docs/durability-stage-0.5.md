# Stage 0.5: acknowledge authority, coalesce document projections

The decision is in [DESIGN.md](/Users/vera/dev/sophia/plans/2026-10-22-durability/DESIGN.md:5): about one second of power-loss exposure is acceptable; corrupt or half-applied authority is not. This stage keeps the existing forced, atomic room-state write. Group commit and the one-second flush cadence belong to later stages.

The [round 3 queue addendum](/Users/vera/dev/sophia/plans/2026-10-22-stress/REPORT-round3.md:135) reports live queue wait p50 103 ms, p95 10,217 ms, maximum 14,570 ms. The executor polls one operation at a time (`executor.rs`, `drain_queue`); insert/text-edit/comment completion previously awaited its full record/RDF/history projection. Releasing the hot-write lease did not release that FIFO. [Round 2](/Users/vera/dev/sophia/plans/2026-10-22-stress/REPORT-round2.md:51) and [round 3](/Users/vera/dev/sophia/plans/2026-10-22-stress/REPORT-round3.md:64) identify the expensive save/reconcile tail.

`block.insert`, `block.update`, `block.editText` and `document.editComment` now persist mutation and replay receipt together inside the authoritative room, publish the pending debounce, and return. Update and comment now also use the existing room operation ledger. Journal completion follows through the existing executor responder path. A missing journal completion remains safe to replay.

Document debounces keep the existing 600 ms delay, trace links, superseding generations, graph generation fence, process background lease and retry ceiling. Their projections run outside the mutation FIFO on blocking workers, holding a shared graph lifecycle lease and the existing room projection gate. Different documents can project concurrently. Oxigraph handles its store transactions, and `mutate_graph_record` serializes the shared graph manifest updates with its existing file lock. The tombstone/evicted-room checks and projection-only save's protection against overwriting newer authority remain in force. Workspace debounces and explicit `crdt.flush` operations retain their existing queue path.

A replay of a partially saved record repairs that record's captured Y.Doc state, rather than mixing its content with the newer authority sidecar. Its completed room epoch advances only after the record, RDF, history and tail marker all succeed.

## MCP read rules

All eight consistency barriers are at the real MCP dispatch boundary. The underlying synchronous helpers remain usable by callers that already own persistence ordering. Each barrier captures a committed room epoch, completes the projection through at least that epoch and waits at most 30 seconds. It holds no hot-write lease. Timeout/error is returned instead of a stale success. A timed-out observer leaves its blocking projection owning the locks until the tail completes.

| Tool | Existing data source | Rule after acknowledged writes |
|---|---|---|
| `read_document` | Record, with authority sidecar hydrated for comments | Fence this document before reading |
| `read_blocks` | Active live projection, otherwise record | Fence this document before choosing either path |
| `get_block` | Same live/record helper | Fence this document |
| `query_blocks` | Same live/record helper | Fence this document |
| `document_digest` | Record metadata, live/record blocks, RDF valuations | Fence this document |
| `search_blocks` | Cold records for lexical results; independently refreshed embeddings for semantic results | Fence all document projections before searching records |
| `sparql_query` | Oxigraph | Fence all document projections; arbitrary SPARQL cannot reliably infer a document scope |
| `get_document_history` | History index/payloads | Fence this document's entire tail; retain the existing deleted-history policy |

The watermark is the existing **completed room projection epoch**, not a per-write document revision or merely the record rename. A fresh room conservatively starts dirty if it hydrates a nonempty sidecar. Graph-wide barriers walk cold sidecars one at a time; a clean room opened only for repair is evicted under the registry lock if no concurrent user holds it. Live/concurrently used/dirty rooms remain. This avoids retaining all decoded document histories, but a cold graph-wide read can still be expensive.

Semantic-only discovery still requires the existing independent embedding refresh (`semantic_search_service.rs:semantic_search`); this stage does not make the embedding index synchronous or give it a revision watermark. Lexical search and the lexical part of default hybrid search see the newly projected blocks. Semantic index freshness needs its own follow-up.

## Recovery and history

No projection queue entry is required for a completed authority write. Room hydration starts dirty; connect-time configure/schedule, the MCP read barrier, and recovered/explicit sidecar-hydrating flushes all use the existing serialized projection repair. Semantic equality repairs an interrupted RDF/history tail under the same record revision; changed content creates the next projection revision. Graph-wide recovered flushes still hydrate one room at a time.

History is now **per projection**, so a burst may produce one record revision and one automatic snapshot instead of one of each per write. An intervening fenced read may require an earlier projection, so exact counts depend on those observation boundaries. Tier names (`20min`, `2h`, `12h`, `daily`, `weekly`), four automatic slots per tier, promotion/collapse windows, aggregate counts and manual snapshot exemptions are unchanged.

`document.write` remains synchronous: it additionally owns optimistic revision preconditions/rebase, workspace membership/placement, creation and same-ID tombstone recreation. Detaching that transaction safely requires a separate revision/metadata design. `block.delete` and bulk `document.editComments` also retain their existing synchronous tails.

## Validation handoff

No Cargo command, build, test execution, server or network access was used in this lane. Standalone `rustfmt` parsed/formatted the changed Rust files; `git diff --check` checks whitespace. The following tests need the house combined validation.

New/expanded real-room/store/queue tests in `crdt_engine/otel_tests.rs`:

- `ten_inserts_reply_on_durable_authority_and_share_one_projection`: holds the real projection gate; every reply's persisted sidecar contains its block and ledger receipt; ten apply spans are much shorter than the deliberately blocked projection, with authority fsyncs; one save/history snapshot; all ten blocks in record and RDF.
- `each_mcp_read_fences_its_acknowledged_write`: each of the eight real dispatch entries stays pending behind the projection gate, permits another hot-write acquisition, and observes its own preceding insert after release. Search uses lexical mode.
- `update_and_comment_replay_receipts_preserve_later_writes`: queues two successive edits, replays the first server operation ID, and verifies the durable authority and receipt preserve the later edit.
- `replied_write_with_dropped_projection_is_repaired_on_reopen`: replies, captures and drops the projection, cancels its debounce, opens fresh app/queue/registry state with no pending journal operation, and repairs record/RDF from authority.
- `document_projection_releases_hot_write_but_keeps_the_deletion_fence`: now requires the writer to return while projection is blocked; the background projector owns the deletion fence.
- `quick_edits_coalesce_without_changing_queue_order`: the final text still reflects FIFO edits, with one projection revision/history entry for the burst.

Existing tests that continue to pin safety:

- Persistence: `older_document_materialization_cannot_clear_newer_projection` (expanded to check replay retains the record's captured state), `fresh_registry_hydration_does_not_create_a_document_revision`, `failed_history_tail_is_required_and_repaired_once_by_semantic_replay`, `document_tail_failure_is_retryable_after_hot_commit_and_replays_same_revision`, `projection_only_save_refusals_are_terminal_after_hot_commit`.
- Lifecycle/tombstones: `queued_document_delete_evicts_live_room_and_cancels_zombie_flush`, `failed_delete_cannot_resurrect_and_same_id_recreation_clears_only_after_workspace_commit`, `graph_deletion_evicts_room_and_cancels_pending_automatic_flush`, and executor permanent-error/retry-ceiling tests.
- Minimal edits: `markdown_round_trip_of_freud_chapter_is_zero_ops`, `one_paragraph_edit_broadcasts_a_small_update`, `unflushed_person_edit_survives_expected_revision_write`, `write_without_expected_revision_keeps_an_unsaved_live_edit`, `comment_marks_survive_markdown_rewrite`.
- History: `failed_history_store_commit_keeps_payloads_referenced_by_old_index`, `concurrent_automatic_manual_copy_and_delete_preserve_one_history_index`, retained-deleted-history tests and the existing tier/collapse suite.
- Durability: `concurrent_flushes_serialize_and_publish_complete_snapshots`, `scheduler_write_landing_during_flush_remains_pending_for_next_round`, `kill_and_restore_after_debounced_not_yet_flushed_write_bounds_loss_to_rpo`, and `forced_flush_during_import_never_composes_a_torn_ledger_and_stale_rdf_combination`.

Legitimate expectation changes in `persistence_tests.rs`: comments explicitly fence before inspecting projected revisions/history; automatic document projection tests now expect no FIFO entry and progress with the drainer blocked; a completed block authority now has no pending journal operation and is repaired by reopening. The executor's comment-recovery test explicitly fences before inspecting its record revision. Its non-flush cold-tail retry-ceiling test now uses the still-synchronous bulk comment operation; a single comment's projection failure no longer keeps that write pending. Per-write completion no longer implies its own history entry.

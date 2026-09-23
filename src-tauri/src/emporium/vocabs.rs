//! Emporium vocabulary contracts — the canonical `wf:` shape of
//! workflow-knowledge, shipped as **bytes** and never re-serialized.
//!
//! Phase 1 of the gen-2 port (re-home emporium into gardend). The vocab is the
//! single source of truth for the shape of workflow-knowledge; the platform's
//! Python emporium serves `canonical_json()` verbatim with `ETag = sha`, where
//! `sha = sha256(canonical_json())`. The golden file on disk (`workflow.golden
//! .json`) is byte-identical to that canonical serialization, so we embed it
//! via `include_str!` and pin `sha256(EMBEDDED_BYTES)` against the published
//! sha. Re-serializing here would risk a byte (and therefore sha) drift, so we
//! deliberately do NOT round-trip the JSON for the served body.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// The canonical workflow contract, embedded verbatim. This is the exact byte
/// sequence the platform serves and the sha is computed over.
pub(crate) const WORKFLOW_GOLDEN_JSON: &str = include_str!("vocabs/workflow.golden.json");

/// The published sha256 of the workflow contract's canonical JSON. Pinned here
/// (full digest) and asserted against the embedded bytes by the sha-pin test;
/// recompute after any edit to the golden.
pub(crate) const WORKFLOW_GOLDEN_SHA: &str =
    "fa103c3b89fa1167bae2de9c0c3d5437c09d0d1e3adc4967d32154a2e82d4d05";

/// The canonical `sophia-memory-core` contract, embedded verbatim. Same
/// discipline as the workflow golden: shipped as bytes, never re-serialized, and
/// the sha is pinned over exactly these bytes.
pub(crate) const MEMORY_CORE_GOLDEN_JSON: &str =
    include_str!("vocabs/sophia-memory-core.golden.json");

/// The pinned sha256 of the memory-core golden's on-disk bytes — the load-bearing
/// drift gate (asserted by `embedded_memory_sha_is_pinned`). Recompute with
/// `shasum -a 256 src/emporium/vocabs/sophia-memory-core.golden.json` after any
/// edit to the golden.
pub(crate) const MEMORY_CORE_GOLDEN_SHA: &str =
    "bbe37c7178f1fa01975a391382c68ba5de881118104f9fce8e8b5379fb68b9cc";

/// The `emporium-graph` SHACL-retrofit contract (EA-2b), embedded verbatim. This
/// is NOT a served emporium pack — it is the AUTHORED minimal mirror of the
/// code-defined graph-metadata span (`rdf_record_materializer::graph_desired_triples`),
/// consumed ONLY by `vocab_to_shacl` to derive the Graph conformance-oracle
/// shapes. Embedded + sha-pinned with the same discipline as the served goldens
/// so a drift between the contract and its bytes fails CI.
pub(crate) const GRAPH_GOLDEN_JSON: &str = include_str!("vocabs/emporium-graph.golden.json");

/// The pinned sha256 of the `emporium-graph` golden's on-disk bytes — the drift
/// gate (asserted by `embedded_graph_sha_is_pinned`). Recompute with
/// `shasum -a 256 src/emporium/vocabs/emporium-graph.golden.json` after any edit.
pub(crate) const GRAPH_GOLDEN_SHA: &str =
    "bb105960a71fedc137cfc0a1560c6b2f4e4907441458fd767b8008c0242dbfe2";

/// The `emporium-salience` SHACL-retrofit contract (EA-2b), embedded verbatim.
/// Like the graph contract this is NOT a served pack — it is the AUTHORED mirror
/// of the code-defined salience span (`salience_rdf_materializer::salience_value_triples`),
/// the single `mnemo:BlockValuation` class with its dual-namespace (`mnemo:`/`mdoc:`)
/// predicate fan-out. Consumed ONLY by `vocab_to_shacl`.
pub(crate) const SALIENCE_GOLDEN_JSON: &str = include_str!("vocabs/emporium-salience.golden.json");

/// The pinned sha256 of the `emporium-salience` golden's on-disk bytes — the
/// drift gate (asserted by `embedded_salience_sha_is_pinned`). Recompute with
/// `shasum -a 256 src/emporium/vocabs/emporium-salience.golden.json` after any edit.
pub(crate) const SALIENCE_GOLDEN_SHA: &str =
    "febdae8e9f22cbe238970cb8b616c4dfd8d7be4380c97f31e8e9d4bbca7e9502";

/// The `emporium-semantic` SHACL-retrofit contract for SEMMA+ semantic projection
/// records. This is NOT a served pack — it is the AUTHORED mirror of the
/// `:projection:semantic` scaffold/relation testimony, consumed ONLY by
/// `vocab_to_shacl`.
pub(crate) const SEMANTIC_GOLDEN_JSON: &str = include_str!("vocabs/emporium-semantic.golden.json");

/// Pinned sha256 — recompute with
/// `shasum -a 256 src/emporium/vocabs/emporium-semantic.golden.json`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SEMANTIC_GOLDEN_SHA: &str =
    "0c54a2847f132d11f51a8f4680c11a43b7f3c204510efa4779c89609ee05cc99";

// ── EA-2b+ structural-fork retrofit contracts (Wires / Song / Document / Workspace) ──
// The four REMAINING reconcile kinds, each carrying a STRUCTURAL feature the flat
// Graph/Salience emitter did not need. Like the graph/salience contracts these are
// NOT served packs — they are AUTHORED mirrors of code-defined spans, consumed ONLY
// by `vocab_to_shacl` to derive the conformance-oracle shapes. Embedded + sha-pinned
// with the same discipline; DELIBERATELY ABSENT from `VOCAB_REGISTRY` (proven by
// `structural_retrofit_contracts_are_not_served_packs`).

/// The `emporium-wires` retrofit contract (single class wire:Wire, flat ~24
/// predicates fully DERIVED; the structural subtlety is the no-referential-integrity
/// note — endpoints are IRIs that may dangle).
pub(crate) const WIRES_GOLDEN_JSON: &str = include_str!("vocabs/emporium-wires.golden.json");
/// Pinned sha256 — recompute with `shasum -a 256 src/emporium/vocabs/emporium-wires.golden.json`.
pub(crate) const WIRES_GOLDEN_SHA: &str =
    "bdebca3cd0def257b5a3e99aa13c033c031442165aa3d93262731fcd015042ad";

/// The `emporium-song` retrofit contract (MULTI-CLASS union Song/SongVerse/SongCoda,
/// flat per-class shapes DERIVED; the cross-class invariant verseIndex==position is
/// SHACL-inexpressible in rudof 0.2.12 and stays Lean-proven + oracle-checked).
pub(crate) const SONG_GOLDEN_JSON: &str = include_str!("vocabs/emporium-song.golden.json");
/// Pinned sha256 — recompute with `shasum -a 256 src/emporium/vocabs/emporium-song.golden.json`.
pub(crate) const SONG_GOLDEN_SHA: &str =
    "23cf268696ab19799cfa5e07bc7918759fe95ca7f9e7d68e2651e2d117817736";

/// The `emporium-document` retrofit contract (the recursive node-tree; per-node-type
/// flat shapes DERIVED; the recursive mdoc:childNode structure (sh:node) is
/// SHACL-inexpressible in rudof 0.2.12 and stays reconcile-guaranteed + oracle-checked).
pub(crate) const DOCUMENT_GOLDEN_JSON: &str = include_str!("vocabs/emporium-document.golden.json");
/// Pinned sha256 — recompute with `shasum -a 256 src/emporium/vocabs/emporium-document.golden.json`.
pub(crate) const DOCUMENT_GOLDEN_SHA: &str =
    "d866b574fa15b5d34f48081db5aeb11edfe70ff5aa2dad4bf4f87f15311843cd";

/// The `emporium-workspace` retrofit contract (the 3 mdoc-namespaced entity classes
/// of the 10-class union, flat shapes DERIVED; the once-at-seed rdfs:subClassOf
/// ontology block is SHACL-inexpressible in the instance-shape model and stays a
/// hand-authored idempotent-seed complement + oracle-checked).
pub(crate) const WORKSPACE_GOLDEN_JSON: &str =
    include_str!("vocabs/emporium-workspace.golden.json");
/// Pinned sha256 — recompute with `shasum -a 256 src/emporium/vocabs/emporium-workspace.golden.json`.
pub(crate) const WORKSPACE_GOLDEN_SHA: &str =
    "cf6c525d43c312cf8d13b0bc6b003e36105603e9034ebbb4ae37ee1a84de12ca";

/// The `emporium-bookmark` EXAMPLE product vocab (EA-3), embedded verbatim. Unlike
/// the EA-2b retrofit contracts this IS a served pack — it is the minimal real
/// organism that proves the GENERIC publication path (register → serve → generic-
/// ingest → B4/B5-materialize → SHACL-validate → query-back). One `Bookmark` class
/// with a Template `subject_rule` and `write_target: "projection:bookmark"`.
pub(crate) const BOOKMARK_GOLDEN_JSON: &str = include_str!("vocabs/emporium-bookmark.golden.json");

/// The pinned sha256 of the `emporium-bookmark` golden's on-disk bytes — the drift
/// gate (asserted by `embedded_bookmark_sha_is_pinned`). Recompute with
/// `shasum -a 256 src/emporium/vocabs/emporium-bookmark.golden.json` after any edit.
pub(crate) const BOOKMARK_GOLDEN_SHA: &str =
    "9f169bb46eeb5f847c36107f2fdeb5c1bc0d05f4e42e8e4504cc7070dbd27733";

/// Read-only PDF-source model. Native document projection is its only writer;
/// the served pack intentionally omits top-level write_target.
pub(crate) const PDF_SOURCE_GOLDEN_JSON: &str = include_str!("vocabs/garden-pdf-source.golden.json");
pub(crate) const PDF_SOURCE_GOLDEN_SHA: &str =
    "1c68bf40562e97fa8a944f39e5e3687e5a4e72303d991733b88de01282ec4b9b";

/// Ludus source/editorial current-state objects and append-only source events.
/// Event immutability is supplied by the activated source ledger, not generic upsert.
pub(crate) const LUDUS_CORE_GOLDEN_JSON: &str = include_str!("vocabs/ludus-core.golden.json");
pub(crate) const LUDUS_CORE_GOLDEN_SHA: &str =
    "ec1835ee0c0066096827237bb5d4d3833c6e7e813cd5ca191ee35a36fadd61a3";

/// The `koch-morse` domain pack. Koch is not an application-shaped exception:
/// its course, lessons, Morse characters, learners, completed sessions, and
/// receive/send attempts are graph-local Meaningful Objects materialized by the
/// generic Emporium spine into `:projection:koch-morse`. The contract also names
/// the Shrubbery faces compatible with the course and learner objects.
pub(crate) const KOCH_MORSE_GOLDEN_JSON: &str = include_str!("vocabs/koch-morse.golden.json");

/// Pinned sha256 of the exact `koch-morse` golden bytes.
pub(crate) const KOCH_MORSE_GOLDEN_SHA: &str =
    "6206dbe24768aaefe599c6058960053ac7b20906de9e948f92ac202a24235792";

/// The `emporium-chamber` BUILT-IN pack (EA-3 §4, the hyperbaric knowledge
/// chamber), embedded verbatim. A SERVED pack whose single `DomainOntology` class
/// stores the agent's RUNTIME-PROPOSED domain ontologies as born-RDF in
/// `:projection:chamber`. The agent's proposed ontologies ride the SAME generic
/// publication spine the bookmark example proves; the chamber's net-new piece is
/// resolving those in-graph ontologies back to validate the agent's INSTANCES
/// against its OWN derived shapes (`chamber_ontology.rs`).
pub(crate) const CHAMBER_GOLDEN_JSON: &str = include_str!("vocabs/emporium-chamber.golden.json");

/// The pinned sha256 of the `emporium-chamber` golden's on-disk bytes — the drift
/// gate (asserted by `embedded_chamber_sha_is_pinned`). Recompute with
/// `shasum -a 256 src/emporium/vocabs/emporium-chamber.golden.json` after any edit.
pub(crate) const CHAMBER_GOLDEN_SHA: &str =
    "dd38db10625a1f910e66b3748541a72085d9343f06afe9f115549c66ebdb43c2";

/// The `sophia-api` API-publication pack (EA-3 Seq 9 / UC-2), embedded verbatim. A
/// SERVED pack — the source vocab for the OpenAPI FACE: its five `api:` classes
/// (Operation/Parameter/Response/WorkflowBinding/Server) declare a published API as
/// born-RDF triples, and `vocab_to_openapi` (the peer of `vocab_to_shacl`) projects
/// them into a valid OpenAPI 3.x document, served via content negotiation. The
/// `api:WorkflowBinding` carries the I/O JSON Schemas as STRING LITERALS (the CA-1
/// invocation-binding boundary) passed VERBATIM into the OpenAPI components/schemas.
pub(crate) const SOPHIA_API_GOLDEN_JSON: &str = include_str!("vocabs/sophia-api.golden.json");

/// The pinned sha256 of the `sophia-api` golden's on-disk bytes — the drift gate
/// (asserted by `embedded_sophia_api_sha_is_pinned`). Recompute with
/// `shasum -a 256 src/emporium/vocabs/sophia-api.golden.json` after any edit.
pub(crate) const SOPHIA_API_GOLDEN_SHA: &str =
    "ef18a9a65e9f7cf3baf47bb6153471d386b88e74f02747e8ae84bb82d45ebd95";

/// The `sophia-agent-core` AGENT-ONTOLOGY pack (CA-1 / EA-3), embedded verbatim. A
/// SERVED pack — the agent-as-subject vocab (spec `plans/ca1-agent-ontology-v1-20260623.md`):
/// the 5 FROZEN classes (Agent/Session/Turn/Capability/Tool, verbatim from
/// choreograph `agent-ontology.ts`) plus the witness/voice/voicing/driver/faculty
/// glue. A COMPOSITE Meaningful-Object vocab — it `imports` `mem:` and references the
/// `mnemo:` Song/Valuation projections BY RELATION (`agt:bindsRecordClass`), so
/// build-time A ⊂ runtime B. It carries the spec §3 SHACL invariants VERBATIM in
/// `raw_shacl_shapes` (the membrane-scoped/cardinality `sh:select` shapes the
/// structural emitter cannot derive — the contract-level `sh:sparql` passthrough the
/// oxigraph evaluator was built for). A VOCAB-PUBLICATION pack: no `write_target`, so
/// `agt:` instances ride the user:rdf applier path while the membrane/leaf testimony
/// is written by the existing L0 memory/song materializers under `mem:`/`mnemo:`.
pub(crate) const AGENT_CORE_GOLDEN_JSON: &str =
    include_str!("vocabs/sophia-agent-core.golden.json");

/// The pinned sha256 of the `sophia-agent-core` golden's on-disk bytes — the drift
/// gate (asserted by `embedded_agent_core_sha_is_pinned`). Recompute with
/// `shasum -a 256 src/emporium/vocabs/sophia-agent-core.golden.json` after any edit.
pub(crate) const AGENT_CORE_GOLDEN_SHA: &str =
    "e9dd3eaa68b831047ee1772f52ff7d8a13eb05dbd09f9f9006a8ca4c7ae43ad8";

/// The WS-7 workflow-agent binding compatibility publication pack. Canonical
/// ownership now lives in `workflow` (`wf:boundToAgent`) and `sophia-agent-core`
/// (`agt:realizedBy` as the derived inverse); this served pack remains a
/// publication-only compatibility face with no write_target.
pub(crate) const WF_AGENT_BINDING_GOLDEN_JSON: &str =
    include_str!("vocabs/wf-agent-binding.golden.json");

/// Pinned sha256 for the WS-7 binding pack.
pub(crate) const WF_AGENT_BINDING_GOLDEN_SHA: &str =
    "a541f62f795fd7a4a8f26b6f2f1bc2f09d5390f592f56eca04434377e717ea7d";

/// The WS-7 run Session/Turn projection companion. This is the write pack that
/// routes run-fold materialization into `:projection:session` through the generic
/// simple-projection spine.
pub(crate) const WF_AGENT_SESSION_PROJECTION_GOLDEN_JSON: &str =
    include_str!("vocabs/wf-agent-session-projection.golden.json");

/// Pinned sha256 for the WS-7 Session/Turn projection pack.
pub(crate) const WF_AGENT_SESSION_PROJECTION_GOLDEN_SHA: &str =
    "99be391a6467016c4793ebcd86c1e6818aef3504046e51cca72c1bbf990e159f";

/// The §WS3 live runtime/world-state projection companion (EA-3 relevel). This is
/// the write pack that reconciles the MUTABLE agent-session state (session head,
/// conversation messages, codex comments, control approvals, the active prompt
/// binding) into `:projection:agent-world` through the generic simple-projection
/// spine. Distinct lane from `wf-agent-session-projection` (run-journal anatomy):
/// the two never clobber each other's class-spans.
pub(crate) const AGENT_WORLD_RUNTIME_GOLDEN_JSON: &str =
    include_str!("vocabs/wf-agent-world-runtime.golden.json");

/// Pinned sha256 for the §WS3 world-runtime projection pack.
pub(crate) const AGENT_WORLD_RUNTIME_GOLDEN_SHA: &str =
    "dd8f8d0c29f97b8ed6095902889e7f3ddcb5a2bcf4ef581e5ccbf359c62663ac";

/// The KG-ULTRA intuition pack. Served projection vocab: one `kgultra:Intuition`
/// event plus ranked `kgultra:IntuitionCandidate` and
/// `kgultra:AnswerCandidate` children, materialized into `:projection:kg-ultra`
/// through the generic simple-projection spine.
pub(crate) const KG_ULTRA_INTUITION_GOLDEN_JSON: &str =
    include_str!("vocabs/kg-ultra-intuition.golden.json");

/// Pinned sha256 for the KG-ULTRA intuition projection pack.
pub(crate) const KG_ULTRA_INTUITION_GOLDEN_SHA: &str =
    "36896d79e28f734e6ee3615df277c1b5fbdac7b37acc06015fbfe4f4311c1d76";

/// The LongMemEval labeled-memory calibration pack. Served projection vocab:
/// frame-first evidence-backed labels (case packets, spans, cues, observations,
/// frames, derivations, adjudications, and backprojection checks) materialized
/// into `:projection:lme-labeled-memory`.
pub(crate) const LME_LABELED_MEMORY_GOLDEN_JSON: &str =
    include_str!("vocabs/lme-labeled-memory.golden.json");

/// Pinned sha256 for the LongMemEval labeled-memory projection pack.
pub(crate) const LME_LABELED_MEMORY_GOLDEN_SHA: &str =
    "3cf9d48984f410422c535e766a672a5a4d9895b8541f789aa92e6bc453a35535";

/// The stable `wfui:` namespace used by workflow-book retained decisions and
/// navigation-route records. Kept as an exported constant so the runtime page
/// renderer and the served vocabulary contract cannot drift.
pub(crate) const WORKFLOW_UI_NS: &str = "https://sophia-labs.com/ns/workflow-ui#";

/// The Workflow Book UI pack. Served publication vocab: retained
/// PageTurnDecision Meaningful Objects, NavigationRoute route handles,
/// EvidenceArtifact sidecars, and AuthorizationFlag inspection gates for the
/// choose-your-own-adventure agent workflow surface.
pub(crate) const WORKFLOW_UI_GOLDEN_JSON: &str = include_str!("vocabs/workflow-ui.golden.json");

/// Pinned sha256 for the Workflow Book UI publication pack.
pub(crate) const WORKFLOW_UI_GOLDEN_SHA: &str =
    "db3f7913b3a062a73023c12f983baad3a304a938fb8f7f53fb143b917d4611b8";

/// The `nomos` omphalos constitution retrofit contract. This is NOT a served
/// pack — it validates the deployment-level constitution authored into the
/// omphalos store. It stays embedded + sha-pinned like the other internal
/// retrofit contracts, and deliberately stays absent from [`VOCAB_REGISTRY`].
pub(crate) const NOMOS_GOLDEN_JSON: &str = include_str!("vocabs/nomos.golden.json");

/// Pinned sha256 for the `nomos` constitution contract.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const NOMOS_GOLDEN_SHA: &str =
    "8fe96b4ff59ff793789a2f861053b1cced2308a2ceeb02c991f1100b4ba0dded";

/// The `lex-scotus-core` domain-ontology pack (N1, the `crawford` campaign —
/// `plans/crawford-campaign-20260706.md`). A SERVED pack, COMPILED-IN by
/// deliberate route decision (not chamber-published): the landing brief
/// (`crawford/ontology/N1-landing-brief.md`) chose the rebuild-anyway moment
/// to ship it embedded so it gets full `emporium_vocab` / `/emporium/vocabs`
/// catalog visibility, which a chamber-proposed pack does not. Eight classes
/// (Case/Justice/Opinion/LegalQuestion/DoctrineHead/Holding/ReasoningStep/
/// DoctrinalTest) over `:projection:lex`; `DoctrineHead` is the one Contested-
/// reconciliation, lineage-shaped class (Roberts→Crawford supersession, the
/// Marks-rule contested-sibling fixture for Williams v. Illinois).
pub(crate) const LEX_SCOTUS_CORE_GOLDEN_JSON: &str =
    include_str!("vocabs/lex-scotus-core.golden.json");

/// The pinned sha256 of the `lex-scotus-core` golden's on-disk bytes — the
/// drift gate (asserted by `embedded_lex_scotus_core_sha_is_pinned`). Recompute
/// with `shasum -a 256 src/emporium/vocabs/lex-scotus-core.golden.json` after
/// any edit to the golden.
pub(crate) const LEX_SCOTUS_CORE_GOLDEN_SHA: &str =
    "9fa122b7443ca58dd8c4715e8f7cd442acddd0ed3fcc0ccbbeb951022fbd23dc";

/// The lightweight operational-machine ontology. This is a SERVED internal-
/// substrate pack, not a fourth public ontology pillar: it gives Observatory,
/// deployment tooling, and graph testimony a shared distinction between a
/// versioned definition, stable logical machine, concrete run, port, and binding.
pub(crate) const MACHINE_CORE_GOLDEN_JSON: &str =
    include_str!("vocabs/sophia-machine-core.golden.json");

/// Pinned sha256 of the machine-core golden's exact on-disk bytes.
pub(crate) const MACHINE_CORE_GOLDEN_SHA: &str =
    "689007d7e1aec46eb8454285399892f9f545891d0107498dabe0074ec3d9285f";

/// The `emporium-observatory` PUBLISH-ONLY pack (Observatory Analysis Cell
/// spec `plans/observatory-analysis-cell-spec-20260715.md` SS A.8), embedded
/// verbatim. A SERVED pack for vocab + derived SHACL + catalog/discovery
/// faces ONLY: every class declares `store_mode: "virtual"`, so the generic
/// Emporium ingest write path refuses every one of them
/// (`class_dispatch::resolve` resolves `DispatchRoute::VirtualSkip`) — the
/// real write path for `:projection:obs:raw`/`:projection:obs:rollups` is
/// the observatory analysis cell's own direct-on-store apply (A2/A3),
/// entirely outside this pack (spec SS A.7). Reuses the `mach:` namespace and
/// `MachineRun`'s `urn:sophia:machine-run:{environmentId}:{runId}` subject
/// convention VERBATIM from `sophia-machine-core.golden.json` to preserve
/// the machine-core join.
pub(crate) const OBSERVATORY_GOLDEN_JSON: &str =
    include_str!("vocabs/emporium-observatory.golden.json");

/// The pinned sha256 of the `emporium-observatory` golden's on-disk bytes —
/// the drift gate (asserted by `embedded_observatory_sha_is_pinned`).
/// Recompute with
/// `shasum -a 256 src/emporium/vocabs/emporium-observatory.golden.json`
/// after any edit to the golden.
pub(crate) const OBSERVATORY_GOLDEN_SHA: &str =
    "aa7c1c0141afdb4edfef9b22280ed272e00e99d926bbac049f27e7eff20defa9";

/// Domain Kit current-state manifest projection. The authored JSON manifest is
/// durable source; this served pack is Garden's sole write authority for the
/// regenerable `:projection:domain-manifest` graph.
pub(crate) const DOMAIN_MANIFEST_GOLDEN_JSON: &str =
    include_str!("vocabs/sophia-domain-manifest.golden.json");
pub(crate) const DOMAIN_MANIFEST_GOLDEN_SHA: &str =
    "fe9f2c637678b0efcb932967c814696c9b9aef8027c46c78f707ddabf4822961";

/// Domain Kit append-only verdict projection. Typed host effects enter this
/// pack through Emporium; sandboxes never write the reserved graph directly.
pub(crate) const DOMAIN_VERDICT_GOLDEN_JSON: &str =
    include_str!("vocabs/sophia-domain-verdict.golden.json");
pub(crate) const DOMAIN_VERDICT_GOLDEN_SHA: &str =
    "ffc92c7a93b61340261688a2b27be0571a74084cdae3564ac374aafedf93e4c3";

/// Domain Kit graph-native dashboard projection. Nucleus validates the layout
/// document and Emporium owns its reserved materialization boundary.
pub(crate) const DOMAIN_DASHBOARD_GOLDEN_JSON: &str =
    include_str!("vocabs/sophia-domain-dashboard.golden.json");
pub(crate) const DOMAIN_DASHBOARD_GOLDEN_SHA: &str =
    "5d81e27c566f22f54d633f75460a453d827e6fa8562c3f3b40ad59654b0268cd";

/// Registered graph-defined site projection. Planter and host effect workers
/// submit typed records; Garden alone owns `:projection:site`.
pub(crate) const SHRUBBERY_SITE_GOLDEN_JSON: &str =
    include_str!("vocabs/shrubbery-site.golden.json");
pub(crate) const SHRUBBERY_SITE_GOLDEN_SHA: &str =
    "22a1159852508fd6099dbea5a6ea082d3ae32d25a4862a769627b738d96b72c8";

/// The Mithras Flow playground board vocabulary (unit G1). One class per Flow
/// table (13); geometry columns (x, y, width, height, waypoints — the 14
/// fields interfaces.md §C names) are deliberately absent, they live in the
/// board Y.Doc's scene root, never in `:projection:flow`. See
/// `contracts/vocabulary-map.md` (Mithras Flow build) for the full map and
/// the six RTRIP-11 vocabulary corrections.
pub(crate) const FLOW_GOLDEN_JSON: &str = include_str!("vocabs/flow.golden.json");
pub(crate) const FLOW_GOLDEN_SHA: &str =
    "121842b0ee5dd4a59fd7735c0a512e0cf7cf1f059301f7cfd1daf282bf8c4bfb";

/// The TABLE-DRIVEN vocab registry (EA-3 / Seq 0). One row per SERVED pack:
/// `(name, golden JSON bytes, pinned sha256)`. The two registration projections
/// — the thin `Vec<VocabContract>` for serving and the parsed
/// `BTreeMap<String, VocabularyContract>` for [`crate::emporium::contract::get_vocabulary`]
/// — both build OFF this single table, so registering a new served pack is one row
/// here + one sha-pin test (no per-vocab boilerplate functions).
///
/// The EA-2b retrofit contracts (`emporium-graph`/`emporium-salience`) are
/// DELIBERATELY ABSENT — they are SHACL-shape sources, not served packs (proven by
/// `retrofit_contracts_are_not_served_packs`). The `name` MUST equal the golden's
/// `"name"` field (asserted by `registry_names_match_golden_names`).
pub(crate) const VOCAB_REGISTRY: &[(&str, &str, &str)] = &[
    ("ludus-core", LUDUS_CORE_GOLDEN_JSON, LUDUS_CORE_GOLDEN_SHA),
    ("garden-pdf-source", PDF_SOURCE_GOLDEN_JSON, PDF_SOURCE_GOLDEN_SHA),
    ("workflow", WORKFLOW_GOLDEN_JSON, WORKFLOW_GOLDEN_SHA),
    (
        "sophia-memory-core",
        MEMORY_CORE_GOLDEN_JSON,
        MEMORY_CORE_GOLDEN_SHA,
    ),
    (
        "emporium-bookmark",
        BOOKMARK_GOLDEN_JSON,
        BOOKMARK_GOLDEN_SHA,
    ),
    ("koch-morse", KOCH_MORSE_GOLDEN_JSON, KOCH_MORSE_GOLDEN_SHA),
    ("emporium-chamber", CHAMBER_GOLDEN_JSON, CHAMBER_GOLDEN_SHA),
    ("sophia-api", SOPHIA_API_GOLDEN_JSON, SOPHIA_API_GOLDEN_SHA),
    (
        "sophia-agent-core",
        AGENT_CORE_GOLDEN_JSON,
        AGENT_CORE_GOLDEN_SHA,
    ),
    (
        "wf-agent-binding",
        WF_AGENT_BINDING_GOLDEN_JSON,
        WF_AGENT_BINDING_GOLDEN_SHA,
    ),
    (
        "wf-agent-session-projection",
        WF_AGENT_SESSION_PROJECTION_GOLDEN_JSON,
        WF_AGENT_SESSION_PROJECTION_GOLDEN_SHA,
    ),
    (
        "wf-agent-world-runtime",
        AGENT_WORLD_RUNTIME_GOLDEN_JSON,
        AGENT_WORLD_RUNTIME_GOLDEN_SHA,
    ),
    (
        "kg-ultra-intuition",
        KG_ULTRA_INTUITION_GOLDEN_JSON,
        KG_ULTRA_INTUITION_GOLDEN_SHA,
    ),
    (
        "lme-labeled-memory",
        LME_LABELED_MEMORY_GOLDEN_JSON,
        LME_LABELED_MEMORY_GOLDEN_SHA,
    ),
    (
        "workflow-ui",
        WORKFLOW_UI_GOLDEN_JSON,
        WORKFLOW_UI_GOLDEN_SHA,
    ),
    (
        "lex-scotus-core",
        LEX_SCOTUS_CORE_GOLDEN_JSON,
        LEX_SCOTUS_CORE_GOLDEN_SHA,
    ),
    (
        "sophia-machine-core",
        MACHINE_CORE_GOLDEN_JSON,
        MACHINE_CORE_GOLDEN_SHA,
    ),
    (
        "emporium-observatory",
        OBSERVATORY_GOLDEN_JSON,
        OBSERVATORY_GOLDEN_SHA,
    ),
    (
        "sophia-domain-manifest",
        DOMAIN_MANIFEST_GOLDEN_JSON,
        DOMAIN_MANIFEST_GOLDEN_SHA,
    ),
    (
        "sophia-domain-verdict",
        DOMAIN_VERDICT_GOLDEN_JSON,
        DOMAIN_VERDICT_GOLDEN_SHA,
    ),
    (
        "sophia-domain-dashboard",
        DOMAIN_DASHBOARD_GOLDEN_JSON,
        DOMAIN_DASHBOARD_GOLDEN_SHA,
    ),
    (
        "shrubbery-site",
        SHRUBBERY_SITE_GOLDEN_JSON,
        SHRUBBERY_SITE_GOLDEN_SHA,
    ),
    ("flow", FLOW_GOLDEN_JSON, FLOW_GOLDEN_SHA),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryStatus {
    CanonicalPublic,
    DomainPack,
    CompatibilityAlias,
    InternalSubstrate,
    ExampleFixture,
}

impl RegistryStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CanonicalPublic => "canonical-public",
            Self::DomainPack => "domain-pack",
            Self::CompatibilityAlias => "compatibility-alias",
            Self::InternalSubstrate => "internal-substrate",
            Self::ExampleFixture => "example-fixture",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VocabJurisdiction {
    pub(crate) name: &'static str,
    pub(crate) public_jurisdiction: &'static str,
    pub(crate) canonical_ontology: &'static str,
    pub(crate) canonical_pack: &'static str,
    pub(crate) status: RegistryStatus,
    pub(crate) slug_aliases: &'static [&'static str],
    pub(crate) compatibility_aliases: &'static [&'static str],
}

/// Phase-1 jurisdiction manifest for the unified Agent / Memory / Workflow world.
/// Served compatibility packs remain resolvable, but the catalog labels them as
/// aliases or internal fixtures instead of peer ontology pillars.
pub(crate) const VOCAB_JURISDICTIONS: &[VocabJurisdiction] = &[
    VocabJurisdiction {
        name: "ludus-core",
        public_jurisdiction: "ludus",
        canonical_ontology: "ludus-core",
        canonical_pack: "ludus-core",
        status: RegistryStatus::DomainPack,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "garden-pdf-source",
        public_jurisdiction: "pdf-source",
        canonical_ontology: "garden-pdf-source",
        canonical_pack: "garden-pdf-source",
        status: RegistryStatus::DomainPack,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "workflow",
        public_jurisdiction: "workflow",
        canonical_ontology: "workflow",
        canonical_pack: "workflow",
        status: RegistryStatus::CanonicalPublic,
        slug_aliases: &[],
        compatibility_aliases: &["workflow-ui", "wf-agent-binding", "sophia-api"],
    },
    VocabJurisdiction {
        name: "sophia-memory-core",
        public_jurisdiction: "memory",
        canonical_ontology: "memory",
        canonical_pack: "sophia-memory-core",
        status: RegistryStatus::CanonicalPublic,
        slug_aliases: &["memory"],
        compatibility_aliases: &["lme-labeled-memory", "kg-ultra-intuition"],
    },
    VocabJurisdiction {
        name: "sophia-agent-core",
        public_jurisdiction: "agent",
        canonical_ontology: "agent",
        canonical_pack: "sophia-agent-core",
        status: RegistryStatus::CanonicalPublic,
        slug_aliases: &["agent"],
        compatibility_aliases: &["wf-agent-session-projection", "wf-agent-world-runtime"],
    },
    VocabJurisdiction {
        name: "workflow-ui",
        public_jurisdiction: "workflow",
        canonical_ontology: "workflow",
        canonical_pack: "workflow",
        status: RegistryStatus::CompatibilityAlias,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "wf-agent-binding",
        public_jurisdiction: "workflow",
        canonical_ontology: "workflow",
        canonical_pack: "workflow",
        status: RegistryStatus::CompatibilityAlias,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "sophia-api",
        public_jurisdiction: "workflow",
        canonical_ontology: "workflow",
        canonical_pack: "workflow",
        status: RegistryStatus::CompatibilityAlias,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "wf-agent-session-projection",
        public_jurisdiction: "agent",
        canonical_ontology: "agent",
        canonical_pack: "sophia-agent-core",
        status: RegistryStatus::CompatibilityAlias,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "wf-agent-world-runtime",
        public_jurisdiction: "agent",
        canonical_ontology: "agent",
        canonical_pack: "sophia-agent-core",
        status: RegistryStatus::CompatibilityAlias,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "lme-labeled-memory",
        public_jurisdiction: "memory",
        canonical_ontology: "memory",
        canonical_pack: "sophia-memory-core",
        status: RegistryStatus::CompatibilityAlias,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "kg-ultra-intuition",
        public_jurisdiction: "memory",
        canonical_ontology: "memory",
        canonical_pack: "sophia-memory-core",
        status: RegistryStatus::CompatibilityAlias,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "emporium-chamber",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "emporium-chamber",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "emporium-bookmark",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "emporium-bookmark",
        status: RegistryStatus::ExampleFixture,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    // Product/domain packs are first-class published vocabularies without
    // pretending to be a fourth platform ontology pillar. Their own domain is
    // their jurisdiction; Emporium still gives them the registry, ingest,
    // materialization, SHACL, query, and face-publication path.
    VocabJurisdiction {
        name: "koch-morse",
        public_jurisdiction: "koch",
        canonical_ontology: "koch-morse",
        canonical_pack: "koch-morse",
        status: RegistryStatus::DomainPack,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    // `lex-scotus-core` is a genuinely new domain (US Supreme Court doctrine),
    // not a compatibility alias of workflow/memory/agent. It is deliberately
    // NOT registered as a fourth `canonical-public` pillar here (that is a
    // bigger jurisdiction-manifest decision than N1's landing brief scoped) —
    // `internal-substrate` is the closest existing category to "a served,
    // first-class content pack outside the three ratified pillars", mirroring
    // `emporium-bookmark`'s own non-pillar status. Flagged for Vera: whether
    // `lex` deserves its own public jurisdiction is an open follow-up, not
    // resolved by this registration.
    VocabJurisdiction {
        name: "lex-scotus-core",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "lex-scotus-core",
        status: RegistryStatus::ExampleFixture,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "sophia-machine-core",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "sophia-machine-core",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &["machine"],
        compatibility_aliases: &[],
    },
    // `emporium-observatory` (Observatory Analysis Cell spec SS A.8): a
    // PUBLISH-ONLY pack — every class is store_mode=virtual, so it can never
    // become a second write path for :projection:obs:*. Classified
    // `internal-substrate` like `sophia-machine-core`, not a fourth public
    // ontology pillar.
    VocabJurisdiction {
        name: "emporium-observatory",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "emporium-observatory",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &["observatory"],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "sophia-domain-manifest",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "sophia-domain-manifest",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "sophia-domain-verdict",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "sophia-domain-verdict",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "sophia-domain-dashboard",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "sophia-domain-dashboard",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    VocabJurisdiction {
        name: "shrubbery-site",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "shrubbery-site",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
    // Mithras Flow playground board vocabulary (unit G1) — a third-party app's
    // schema faithfully described, not a fourth ontology pillar. Mirrors
    // shrubbery-site's own internal-substrate/non-pillar classification.
    VocabJurisdiction {
        name: "flow",
        public_jurisdiction: "internal-substrate",
        canonical_ontology: "internal-substrate",
        canonical_pack: "flow",
        status: RegistryStatus::InternalSubstrate,
        slug_aliases: &[],
        compatibility_aliases: &[],
    },
];

pub(crate) fn registry_metadata(name: &str) -> Option<VocabJurisdiction> {
    VOCAB_JURISDICTIONS
        .iter()
        .find(|entry| entry.name == name)
        .copied()
}

pub(crate) fn canonical_registry_name(name: &str) -> &str {
    VOCAB_JURISDICTIONS
        .iter()
        .find(|entry| entry.slug_aliases.contains(&name))
        .map(|entry| entry.name)
        .unwrap_or(name)
}

/// Metadata projected out of a vocabulary contract for the catalog/list view
/// and for header derivation. Mirrors the platform's `/emporium/vocabs` row:
/// `{name, version, namespace, sha, title}`.
#[derive(Debug, Clone)]
pub(crate) struct VocabContract {
    /// Vocabulary slug (e.g. `workflow`); the `{name}` path segment.
    pub(crate) name: &'static str,
    /// Semantic version string (e.g. `1.0.0`).
    pub(crate) version: &'static str,
    /// Primary namespace URI (the namespace bound to `primary_prefix`).
    pub(crate) namespace: &'static str,
    /// Human title.
    pub(crate) title: &'static str,
    /// sha256 of the canonical JSON bytes — the ETag and the commons pin.
    pub(crate) sha: &'static str,
    pub(crate) public_jurisdiction: &'static str,
    pub(crate) canonical_ontology: &'static str,
    pub(crate) canonical_pack: &'static str,
    pub(crate) registry_status: &'static str,
    pub(crate) slug_aliases: &'static [&'static str],
    pub(crate) compatibility_aliases: &'static [&'static str],
    /// The canonical JSON body, byte-exact. Served verbatim for `?format=json`.
    pub(crate) json: &'static str,
}

/// Shape we deserialize from the golden bytes purely to derive metadata
/// (name/version/title/namespace). We never re-serialize this back into the
/// served body — the served body is always `self.json` verbatim.
#[derive(Debug, Deserialize)]
struct WorkflowContractMeta {
    name: String,
    version: String,
    title: String,
    primary_prefix: String,
    namespaces: std::collections::BTreeMap<String, String>,
}

/// The workflow contract as a thin `VocabContract`, resolved by name off the
/// table-driven registry. Cheap to call repeatedly (`all_contracts` is cached).
pub(crate) fn workflow_contract() -> VocabContract {
    find_contract("workflow", "latest").expect("workflow is a registered vocab")
}

/// The memory-core contract as a thin `VocabContract`, resolved by name off the
/// table-driven registry. Cheap to call repeatedly.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn memory_core_contract() -> VocabContract {
    find_contract("sophia-memory-core", "latest").expect("sophia-memory-core is a registered vocab")
}

/// Build a thin [`VocabContract`] (the serving/catalog projection) from one
/// [`VOCAB_REGISTRY`] row. Parses ONLY the metadata (name/version/title/namespace)
/// off the golden bytes — never re-serializes the served body, which stays the
/// embedded `json` verbatim. The parse cannot fail for the embedded goldens
/// (covered by `all_registered_contracts_parse`), so the strings are leaked to
/// `'static` to keep [`VocabContract`] `Copy`-cheap, mirroring the former
/// per-vocab `*_meta()` resolvers.
fn contract_from_registry_row(
    name: &'static str,
    json: &'static str,
    sha: &'static str,
) -> VocabContract {
    let parsed: WorkflowContractMeta = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("embedded golden '{name}' must be valid JSON: {e}"));
    let namespace = parsed
        .namespaces
        .get(&parsed.primary_prefix)
        .cloned()
        .unwrap_or_else(|| panic!("golden '{name}' primary_prefix must be a declared namespace"));
    let metadata = registry_metadata(name)
        .unwrap_or_else(|| panic!("registry row '{name}' must declare ontology jurisdiction"));
    VocabContract {
        name,
        version: Box::leak(parsed.version.into_boxed_str()),
        namespace: Box::leak(namespace.into_boxed_str()),
        title: Box::leak(parsed.title.into_boxed_str()),
        sha,
        public_jurisdiction: metadata.public_jurisdiction,
        canonical_ontology: metadata.canonical_ontology,
        canonical_pack: metadata.canonical_pack,
        registry_status: metadata.status.as_str(),
        slug_aliases: metadata.slug_aliases,
        compatibility_aliases: metadata.compatibility_aliases,
        json,
    }
}

/// Every registered vocabulary contract (latest version of each), projected off
/// the table-driven [`VOCAB_REGISTRY`] and cached once. Ships the workflow pack,
/// the `sophia-memory-core` memory pack, and the `emporium-bookmark` example pack;
/// both surfaces (HTTP `/emporium/vocab/*` and MCP `emporium_vocab`) loop over this
/// list, so adding a registry ROW lights up both with no further wiring.
pub(crate) fn all_contracts() -> Vec<VocabContract> {
    static CACHED: OnceLock<Vec<VocabContract>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            VOCAB_REGISTRY
                .iter()
                .map(|(name, json, sha)| contract_from_registry_row(name, json, sha))
                .collect()
        })
        .clone()
}

/// Look up a contract by name + version selector. `"latest"` resolves to the
/// newest version (trivially the only version in Phase 1). Returns `None` for
/// an unknown name or a version mismatch.
pub(crate) fn find_contract(name: &str, version_or_latest: &str) -> Option<VocabContract> {
    let name = canonical_registry_name(name);
    let contract = all_contracts()
        .into_iter()
        .find(|contract| contract.name == name)?;
    if version_or_latest == "latest" || version_or_latest == contract.version {
        Some(contract)
    } else {
        None
    }
}

/// Compute the sha256 hex digest of arbitrary bytes. Used by the sha-pin test
/// to prove the embedded bytes match the published sha.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_workflow_metadata_parses() {
        let contract = workflow_contract();
        assert_eq!(contract.name, "workflow");
        assert_eq!(contract.version, "1.0.0");
        assert_eq!(contract.title, "Mnemosyne Workflow Vocabulary");
        assert_eq!(contract.namespace, "http://mnemosyne.dev/workflow#");
        // Served body is the embedded bytes verbatim — never re-serialized.
        // (Byte-identity with the published canonical JSON is what matters and
        // is proven by `embedded_workflow_sha_is_pinned`.)
        assert_eq!(contract.json, WORKFLOW_GOLDEN_JSON);
    }

    // The make-or-break invariant: the bytes we embed must hash to the sha the
    // platform publishes as the ETag and the workflow-commons pin. A drift here
    // means a cross-stack cache/identity break, so this is the load-bearing
    // test for Phase 1.
    #[test]
    fn embedded_workflow_sha_is_pinned() {
        let computed = sha256_hex(WORKFLOW_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, WORKFLOW_GOLDEN_SHA,
            "embedded workflow golden bytes drifted from the pinned sha"
        );
        // Cross-check that the contract surfaces the same sha.
        assert_eq!(workflow_contract().sha, WORKFLOW_GOLDEN_SHA);
    }

    #[test]
    fn embedded_memory_metadata_parses() {
        let contract = memory_core_contract();
        assert_eq!(contract.name, "sophia-memory-core");
        assert_eq!(contract.version, "1.3.0");
        assert_eq!(contract.title, "Sophia Memory Core Vocabulary");
        assert_eq!(contract.namespace, "http://mnemosyne.dev/memory#");
        assert_eq!(contract.json, MEMORY_CORE_GOLDEN_JSON);
    }

    // The load-bearing drift gate for the memory pack — twin of
    // `embedded_workflow_sha_is_pinned`. A byte drift in the golden fails CI here.
    #[test]
    fn embedded_memory_sha_is_pinned() {
        let computed = sha256_hex(MEMORY_CORE_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, MEMORY_CORE_GOLDEN_SHA,
            "embedded memory-core golden bytes drifted from the pinned sha"
        );
        assert_eq!(memory_core_contract().sha, MEMORY_CORE_GOLDEN_SHA);
    }

    // The EA-2b retrofit contracts are NOT served packs (absent from
    // `all_contracts`), but they ARE embedded + sha-pinned so a drift between the
    // golden bytes and the pin fails CI exactly like the served goldens.
    #[test]
    fn embedded_graph_sha_is_pinned() {
        let computed = sha256_hex(GRAPH_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, GRAPH_GOLDEN_SHA,
            "embedded emporium-graph golden bytes drifted from the pinned sha"
        );
    }

    #[test]
    fn embedded_salience_sha_is_pinned() {
        let computed = sha256_hex(SALIENCE_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, SALIENCE_GOLDEN_SHA,
            "embedded emporium-salience golden bytes drifted from the pinned sha"
        );
    }

    // ── EA-2b+ structural-fork retrofit sha-pins (Wires / Song / Document / Workspace) ──
    #[test]
    fn embedded_wires_sha_is_pinned() {
        assert_eq!(
            sha256_hex(WIRES_GOLDEN_JSON.as_bytes()),
            WIRES_GOLDEN_SHA,
            "embedded emporium-wires golden bytes drifted from the pinned sha"
        );
    }

    #[test]
    fn embedded_song_sha_is_pinned() {
        assert_eq!(
            sha256_hex(SONG_GOLDEN_JSON.as_bytes()),
            SONG_GOLDEN_SHA,
            "embedded emporium-song golden bytes drifted from the pinned sha"
        );
    }

    #[test]
    fn embedded_document_sha_is_pinned() {
        assert_eq!(
            sha256_hex(DOCUMENT_GOLDEN_JSON.as_bytes()),
            DOCUMENT_GOLDEN_SHA,
            "embedded emporium-document golden bytes drifted from the pinned sha"
        );
    }

    // The drift gate for the Mithras Flow playground pack (unit G1) — a byte
    // drift in the flow golden fails CI here, exactly like every other pack.
    #[test]
    fn embedded_flow_sha_is_pinned() {
        let computed = sha256_hex(FLOW_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, FLOW_GOLDEN_SHA,
            "embedded flow golden bytes drifted from the pinned sha"
        );
        assert_eq!(find_contract("flow", "latest").unwrap().sha, FLOW_GOLDEN_SHA);
    }

    #[test]
    fn embedded_workspace_sha_is_pinned() {
        assert_eq!(
            sha256_hex(WORKSPACE_GOLDEN_JSON.as_bytes()),
            WORKSPACE_GOLDEN_SHA,
            "embedded emporium-workspace golden bytes drifted from the pinned sha"
        );
    }

    /// The four structural-fork retrofit contracts must stay OUT of the served
    /// catalog — they are SHACL-shape sources, not published emporium packs (twin of
    /// `retrofit_contracts_are_not_served_packs` for graph/salience).
    #[test]
    fn structural_retrofit_contracts_are_not_served_packs() {
        let names: Vec<&str> = all_contracts().iter().map(|c| c.name).collect();
        for absent in [
            "emporium-wires",
            "emporium-song",
            "emporium-document",
            "emporium-workspace",
        ] {
            assert!(
                !names.contains(&absent),
                "{absent} must NOT be a served pack (it is a SHACL-shape source only)"
            );
        }
    }

    // The load-bearing drift gate for the EA-3 example pack — a byte drift in the
    // bookmark golden fails CI here, exactly like the workflow/memory packs.
    #[test]
    fn embedded_bookmark_sha_is_pinned() {
        let computed = sha256_hex(BOOKMARK_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, BOOKMARK_GOLDEN_SHA,
            "embedded emporium-bookmark golden bytes drifted from the pinned sha"
        );
        assert_eq!(
            find_contract("emporium-bookmark", "latest").unwrap().sha,
            BOOKMARK_GOLDEN_SHA
        );
    }

    #[test]
    fn embedded_koch_morse_sha_is_pinned() {
        let computed = sha256_hex(KOCH_MORSE_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, KOCH_MORSE_GOLDEN_SHA,
            "embedded koch-morse golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("koch-morse", "latest").unwrap();
        assert_eq!(contract.sha, KOCH_MORSE_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/koch#");
    }

    // The drift gate for the EA-3 §4 chamber pack — a byte drift in the chamber
    // golden fails CI here.
    #[test]
    fn embedded_chamber_sha_is_pinned() {
        let computed = sha256_hex(CHAMBER_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, CHAMBER_GOLDEN_SHA,
            "embedded emporium-chamber golden bytes drifted from the pinned sha"
        );
        assert_eq!(
            find_contract("emporium-chamber", "latest").unwrap().sha,
            CHAMBER_GOLDEN_SHA
        );
    }

    // The drift gate for the EA-3 Seq 9 API-publication pack — a byte drift in the
    // sophia-api golden fails CI here, exactly like the bookmark/chamber packs.
    #[test]
    fn embedded_sophia_api_sha_is_pinned() {
        let computed = sha256_hex(SOPHIA_API_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, SOPHIA_API_GOLDEN_SHA,
            "embedded sophia-api golden bytes drifted from the pinned sha"
        );
        assert_eq!(
            find_contract("sophia-api", "latest").unwrap().sha,
            SOPHIA_API_GOLDEN_SHA
        );
    }

    /// Every [`VOCAB_REGISTRY`] sha row matches its golden bytes — the table-driven
    /// drift gate (a new row that forgets to pin a sha fails here).
    #[test]
    fn every_registry_row_sha_is_pinned() {
        for (name, json, sha) in VOCAB_REGISTRY {
            assert_eq!(
                &sha256_hex(json.as_bytes()),
                sha,
                "registry row '{name}' golden bytes drifted from the pinned sha"
            );
        }
    }

    /// S6 VERSION-BUMP RAIL — the `(pack, version, sha)` discipline table.
    ///
    /// One PINNED row per SERVED pack, coupling its golden `version` field to its
    /// pinned sha in ONE place. The DISCIPLINE this enforces, written down:
    ///
    /// > **content change ⇒ version bump + sha re-pin + this table row updated.**
    ///
    /// A golden edit changes its sha256 → the sha assertion below fails → the editor
    /// MUST touch this table's row. Because the version sits RIGHT NEXT TO the sha in
    /// the same row, re-pinning a sha WITHOUT bumping the version is conspicuous in
    /// review ("sha moved, version didn't — was that intended?"). The rail is also
    /// asserted EXHAUSTIVE against [`VOCAB_REGISTRY`], so registering a new served
    /// pack forces a new conscious row here too. (This is the rail the S4 memory-pack
    /// bump and the S6 chamber bump both exercised.)
    #[test]
    fn served_pack_version_and_sha_rail() {
        // (pack_name, golden version, pinned sha). Edit CONSCIOUSLY per the discipline.
        const RAIL: &[(&str, &str, &str)] = &[
            ("ludus-core", "1.0.0", LUDUS_CORE_GOLDEN_SHA),
            ("garden-pdf-source", "1.0.0", PDF_SOURCE_GOLDEN_SHA),
            ("workflow", "1.0.0", WORKFLOW_GOLDEN_SHA),
            ("sophia-memory-core", "1.3.0", MEMORY_CORE_GOLDEN_SHA),
            ("emporium-bookmark", "1.0.0", BOOKMARK_GOLDEN_SHA),
            ("koch-morse", "1.0.0", KOCH_MORSE_GOLDEN_SHA),
            ("emporium-chamber", "1.1.0", CHAMBER_GOLDEN_SHA),
            ("sophia-api", "1.0.0", SOPHIA_API_GOLDEN_SHA),
            ("sophia-agent-core", "1.3.0", AGENT_CORE_GOLDEN_SHA),
            ("wf-agent-binding", "1.0.0", WF_AGENT_BINDING_GOLDEN_SHA),
            (
                "wf-agent-session-projection",
                "1.1.0",
                WF_AGENT_SESSION_PROJECTION_GOLDEN_SHA,
            ),
            (
                "wf-agent-world-runtime",
                "1.0.0",
                AGENT_WORLD_RUNTIME_GOLDEN_SHA,
            ),
            ("kg-ultra-intuition", "1.1.0", KG_ULTRA_INTUITION_GOLDEN_SHA),
            ("lme-labeled-memory", "0.1.0", LME_LABELED_MEMORY_GOLDEN_SHA),
            ("workflow-ui", "1.0.0", WORKFLOW_UI_GOLDEN_SHA),
            ("lex-scotus-core", "1.0.0", LEX_SCOTUS_CORE_GOLDEN_SHA),
            ("sophia-machine-core", "0.1.0", MACHINE_CORE_GOLDEN_SHA),
            ("emporium-observatory", "0.2.0", OBSERVATORY_GOLDEN_SHA),
            (
                "sophia-domain-manifest",
                "1.0.0",
                DOMAIN_MANIFEST_GOLDEN_SHA,
            ),
            ("sophia-domain-verdict", "1.0.0", DOMAIN_VERDICT_GOLDEN_SHA),
            (
                "sophia-domain-dashboard",
                "1.0.0",
                DOMAIN_DASHBOARD_GOLDEN_SHA,
            ),
            ("shrubbery-site", "0.2.1", SHRUBBERY_SITE_GOLDEN_SHA),
            ("flow", "0.1.0", FLOW_GOLDEN_SHA),
        ];

        // (1) The rail covers EXACTLY the served registry — none missing, none extra.
        let rail_names: std::collections::BTreeSet<&str> =
            RAIL.iter().map(|(n, _, _)| *n).collect();
        let registry_names: std::collections::BTreeSet<&str> =
            VOCAB_REGISTRY.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(
            rail_names, registry_names,
            "the version-bump rail must list EXACTLY the served packs (a new served \
             pack needs a new rail row; a removed one needs its row deleted)"
        );

        // (2) Each row's version AND sha match the LIVE golden.
        for (name, version, sha) in RAIL {
            let contract = find_contract(name, "latest")
                .unwrap_or_else(|| panic!("served pack '{name}' resolves"));
            assert_eq!(
                contract.version, *version,
                "pack '{name}': golden version drifted from the pinned rail version — \
                 bump the rail row consciously"
            );
            assert_eq!(
                contract.sha, *sha,
                "pack '{name}': golden sha drifted from the pinned rail sha"
            );
            // Belt-and-suspenders: the live bytes must hash to the pinned rail sha, so
            // the rail is anchored to reality even if a sha const were edited by hand.
            let (_n, json, _s) = VOCAB_REGISTRY
                .iter()
                .find(|(n, _, _)| n == name)
                .expect("registry row for a rail pack");
            assert_eq!(
                &sha256_hex(json.as_bytes()),
                *sha,
                "pack '{name}': live golden bytes do not hash to the pinned rail sha"
            );
        }
    }

    /// The registry `name` MUST equal the golden's own `"name"` field, so a by-name
    /// lookup resolves the row whose bytes it serves (a mismatch would serve one
    /// pack's bytes under another pack's name).
    #[test]
    fn registry_names_match_golden_names() {
        for (name, json, _sha) in VOCAB_REGISTRY {
            let parsed: WorkflowContractMeta =
                serde_json::from_str(json).expect("registry golden parses");
            assert_eq!(
                &parsed.name.as_str(),
                name,
                "registry name '{name}' != golden name '{}'",
                parsed.name
            );
        }
    }

    /// Every registered golden's metadata projection parses (the `all_contracts`
    /// build cannot panic).
    #[test]
    fn all_registered_contracts_parse() {
        let contracts = all_contracts();
        assert_eq!(contracts.len(), VOCAB_REGISTRY.len());
        for c in &contracts {
            assert!(!c.namespace.is_empty(), "{} has a namespace", c.name);
            assert!(!c.version.is_empty(), "{} has a version", c.name);
        }
    }

    #[test]
    fn every_served_pack_declares_ontology_jurisdiction() {
        for (name, _json, _sha) in VOCAB_REGISTRY {
            let metadata = registry_metadata(name)
                .unwrap_or_else(|| panic!("{name} declares jurisdiction metadata"));
            assert!(
                matches!(
                    metadata.public_jurisdiction,
                    "agent"
                        | "memory"
                        | "workflow"
                        | "koch"
                        | "ludus"
                        | "pdf-source"
                        | "internal-substrate"
                ),
                "{name} has an allowed public jurisdiction"
            );
        }
    }

    #[test]
    fn registry_declares_exactly_three_public_canonical_ontologies() {
        let canonical = all_contracts()
            .into_iter()
            .filter(|contract| contract.registry_status == "canonical-public")
            .map(|contract| contract.canonical_ontology)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            canonical,
            ["agent", "memory", "workflow"].into_iter().collect(),
            "only Agent, Memory, and Workflow are public canonical ontology pillars"
        );
    }

    #[test]
    fn compatibility_aliases_point_at_canonical_ontologies() {
        let contracts = all_contracts();
        let canonical_packs = contracts
            .iter()
            .filter(|contract| contract.registry_status == "canonical-public")
            .map(|contract| contract.name)
            .collect::<std::collections::BTreeSet<_>>();
        let served_names = contracts
            .iter()
            .map(|contract| contract.name)
            .collect::<std::collections::BTreeSet<_>>();

        for contract in &contracts {
            if contract.registry_status == "compatibility-alias" {
                assert!(
                    canonical_packs.contains(contract.canonical_pack),
                    "{} points at canonical pack {}",
                    contract.name,
                    contract.canonical_pack
                );
                assert!(
                    matches!(
                        contract.public_jurisdiction,
                        "agent" | "memory" | "workflow"
                    ),
                    "{} aliases a public ontology jurisdiction",
                    contract.name
                );
            }
            if contract.registry_status == "canonical-public" {
                for alias in contract.compatibility_aliases {
                    assert!(
                        served_names.contains(alias),
                        "{} declares served compatibility alias {}",
                        contract.name,
                        alias
                    );
                }
            }
        }
    }

    #[test]
    fn internal_and_fixture_packs_are_not_public_ontology_peers() {
        for name in [
            "emporium-bookmark",
            "emporium-chamber",
            "sophia-machine-core",
            "emporium-observatory",
            "sophia-domain-manifest",
            "sophia-domain-verdict",
            "sophia-domain-dashboard",
            "shrubbery-site",
            "flow",
        ] {
            let contract = find_contract(name, "latest").expect("served internal/example pack");
            assert_ne!(contract.registry_status, "canonical-public");
            assert_eq!(contract.public_jurisdiction, "internal-substrate");
        }
    }

    #[test]
    fn koch_is_a_first_class_domain_pack_not_a_shell_or_ontology_pillar() {
        let contract = find_contract("koch-morse", "latest").expect("Koch domain pack served");
        assert_eq!(contract.registry_status, "domain-pack");
        assert_eq!(contract.public_jurisdiction, "koch");
        assert_eq!(contract.canonical_ontology, "koch-morse");
        assert_eq!(contract.canonical_pack, "koch-morse");
    }

    // The retrofit contracts must stay OUT of the served catalog — they are a
    // shapes source, not a published emporium pack.
    #[test]
    fn retrofit_contracts_are_not_served_packs() {
        let names: Vec<&str> = all_contracts().iter().map(|c| c.name).collect();
        assert!(!names.contains(&"emporium-graph"));
        assert!(!names.contains(&"emporium-salience"));
    }

    #[test]
    fn find_contract_resolves_latest_and_exact_version() {
        assert!(find_contract("workflow", "latest").is_some());
        assert!(find_contract("workflow", "1.0.0").is_some());
        assert!(find_contract("workflow", "9.9.9").is_none());
        assert!(find_contract("nope", "latest").is_none());
        // The memory pack resolves over the same generic surface.
        assert!(find_contract("sophia-memory-core", "latest").is_some());
        assert!(find_contract("sophia-memory-core", "1.3.0").is_some());
        assert!(find_contract("sophia-memory-core", "1.2.0").is_none());
        assert!(find_contract("sophia-memory-core", "1.0.0").is_none());
        assert!(find_contract("sophia-memory-core", "0.0.1").is_none());
        assert_eq!(
            find_contract("agent", "latest").map(|contract| contract.name),
            Some("sophia-agent-core")
        );
        assert_eq!(
            find_contract("memory", "latest").map(|contract| contract.name),
            Some("sophia-memory-core")
        );
    }

    #[test]
    fn all_contracts_lists_every_registered_pack_in_order() {
        let names: Vec<&str> = all_contracts().iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "ludus-core",
                "garden-pdf-source",
                "workflow",
                "sophia-memory-core",
                "emporium-bookmark",
                "koch-morse",
                "emporium-chamber",
                "sophia-api",
                "sophia-agent-core",
                "wf-agent-binding",
                "wf-agent-session-projection",
                "wf-agent-world-runtime",
                "kg-ultra-intuition",
                "lme-labeled-memory",
                "workflow-ui",
                "lex-scotus-core",
                "sophia-machine-core",
                "emporium-observatory",
                "sophia-domain-manifest",
                "sophia-domain-verdict",
                "sophia-domain-dashboard",
                "shrubbery-site",
                "flow"
            ]
        );
    }

    // The drift gate for the CA-1 agent-ontology pack — a byte drift in the
    // sophia-agent-core golden fails CI here, exactly like the api/chamber packs.
    #[test]
    fn embedded_agent_core_sha_is_pinned() {
        let computed = sha256_hex(AGENT_CORE_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, AGENT_CORE_GOLDEN_SHA,
            "embedded sophia-agent-core golden bytes drifted from the pinned sha"
        );
        assert_eq!(
            find_contract("sophia-agent-core", "latest").unwrap().sha,
            AGENT_CORE_GOLDEN_SHA
        );
    }

    #[test]
    fn embedded_wf_agent_binding_sha_is_pinned() {
        let computed = sha256_hex(WF_AGENT_BINDING_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, WF_AGENT_BINDING_GOLDEN_SHA,
            "embedded wf-agent-binding golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("wf-agent-binding", "latest").unwrap();
        assert_eq!(contract.sha, WF_AGENT_BINDING_GOLDEN_SHA);
        assert_eq!(
            contract.namespace,
            "http://mnemosyne.dev/workflow-agent-binding#"
        );
    }

    #[test]
    fn embedded_wf_agent_session_projection_sha_is_pinned() {
        let computed = sha256_hex(WF_AGENT_SESSION_PROJECTION_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, WF_AGENT_SESSION_PROJECTION_GOLDEN_SHA,
            "embedded wf-agent-session-projection golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("wf-agent-session-projection", "latest").unwrap();
        assert_eq!(contract.sha, WF_AGENT_SESSION_PROJECTION_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/agent#");
    }

    #[test]
    fn embedded_agent_world_runtime_sha_is_pinned() {
        let computed = sha256_hex(AGENT_WORLD_RUNTIME_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, AGENT_WORLD_RUNTIME_GOLDEN_SHA,
            "embedded wf-agent-world-runtime golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("wf-agent-world-runtime", "latest").unwrap();
        assert_eq!(contract.sha, AGENT_WORLD_RUNTIME_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/agent#");
        assert_eq!(contract.version, "1.0.0");
    }

    #[test]
    fn embedded_kg_ultra_intuition_sha_is_pinned() {
        let computed = sha256_hex(KG_ULTRA_INTUITION_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, KG_ULTRA_INTUITION_GOLDEN_SHA,
            "embedded kg-ultra-intuition golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("kg-ultra-intuition", "latest").unwrap();
        assert_eq!(contract.sha, KG_ULTRA_INTUITION_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/kg-ultra#");
    }

    #[test]
    fn embedded_lme_labeled_memory_sha_is_pinned() {
        let computed = sha256_hex(LME_LABELED_MEMORY_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, LME_LABELED_MEMORY_GOLDEN_SHA,
            "embedded lme-labeled-memory golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("lme-labeled-memory", "latest").unwrap();
        assert_eq!(contract.sha, LME_LABELED_MEMORY_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/longmemeval#");
    }

    #[test]
    fn embedded_workflow_ui_sha_is_pinned() {
        let computed = sha256_hex(WORKFLOW_UI_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, WORKFLOW_UI_GOLDEN_SHA,
            "embedded workflow-ui golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("workflow-ui", "latest").unwrap();
        assert_eq!(contract.sha, WORKFLOW_UI_GOLDEN_SHA);
        assert_eq!(contract.namespace, WORKFLOW_UI_NS);
    }

    #[test]
    fn embedded_nomos_sha_is_pinned() {
        let computed = sha256_hex(NOMOS_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, NOMOS_GOLDEN_SHA,
            "embedded nomos golden bytes drifted from the pinned sha"
        );
    }

    #[test]
    fn embedded_semantic_sha_is_pinned() {
        let computed = sha256_hex(SEMANTIC_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, SEMANTIC_GOLDEN_SHA,
            "embedded emporium-semantic golden bytes drifted from the pinned sha"
        );
    }

    // The drift gate for the N1 `lex-scotus-core` domain-ontology pack — a byte
    // drift in the golden fails CI here, exactly like the other served packs.
    #[test]
    fn embedded_lex_scotus_core_sha_is_pinned() {
        let computed = sha256_hex(LEX_SCOTUS_CORE_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, LEX_SCOTUS_CORE_GOLDEN_SHA,
            "embedded lex-scotus-core golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("lex-scotus-core", "latest").unwrap();
        assert_eq!(contract.sha, LEX_SCOTUS_CORE_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/lex#");
    }

    #[test]
    fn embedded_machine_core_sha_is_pinned() {
        let computed = sha256_hex(MACHINE_CORE_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, MACHINE_CORE_GOLDEN_SHA,
            "embedded sophia-machine-core golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("sophia-machine-core", "latest").unwrap();
        assert_eq!(contract.sha, MACHINE_CORE_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/machine#");
        assert_eq!(contract.registry_status, "internal-substrate");
    }

    // The drift gate for the Observatory Analysis Cell publish-only pack — a
    // byte drift in the golden fails CI here, exactly like the other served
    // packs.
    #[test]
    fn embedded_observatory_sha_is_pinned() {
        let computed = sha256_hex(OBSERVATORY_GOLDEN_JSON.as_bytes());
        assert_eq!(
            computed, OBSERVATORY_GOLDEN_SHA,
            "embedded emporium-observatory golden bytes drifted from the pinned sha"
        );
        let contract = find_contract("emporium-observatory", "latest").unwrap();
        assert_eq!(contract.sha, OBSERVATORY_GOLDEN_SHA);
        assert_eq!(contract.namespace, "http://mnemosyne.dev/observatory#");
        assert_eq!(contract.registry_status, "internal-substrate");
        // The `observatory` slug alias resolves to the same pack (registry
        // round-trip: name -> sha -> contract -> alias -> same contract).
        assert_eq!(
            find_contract("observatory", "latest").map(|c| c.sha),
            Some(OBSERVATORY_GOLDEN_SHA)
        );
    }

    #[test]
    fn embedded_domain_kit_projection_shas_are_pinned() {
        for (name, bytes, pinned, target) in [
            (
                "sophia-domain-manifest",
                DOMAIN_MANIFEST_GOLDEN_JSON,
                DOMAIN_MANIFEST_GOLDEN_SHA,
                "projection:domain-manifest",
            ),
            (
                "sophia-domain-verdict",
                DOMAIN_VERDICT_GOLDEN_JSON,
                DOMAIN_VERDICT_GOLDEN_SHA,
                "projection:domain-verdict",
            ),
            (
                "sophia-domain-dashboard",
                DOMAIN_DASHBOARD_GOLDEN_JSON,
                DOMAIN_DASHBOARD_GOLDEN_SHA,
                "projection:domain-dashboard",
            ),
        ] {
            assert_eq!(sha256_hex(bytes.as_bytes()), pinned, "{name} bytes drifted");
            let contract = crate::emporium::contract::get_vocabulary(name)
                .unwrap_or_else(|| panic!("{name} resolves"));
            assert_eq!(contract.write_target.as_deref(), Some(target));
        }
    }

    #[test]
    fn embedded_shrubbery_site_sha_is_pinned() {
        assert_eq!(
            sha256_hex(SHRUBBERY_SITE_GOLDEN_JSON.as_bytes()),
            SHRUBBERY_SITE_GOLDEN_SHA,
            "embedded shrubbery-site golden bytes drifted"
        );
        let contract = crate::emporium::contract::get_vocabulary("shrubbery-site")
            .expect("shrubbery-site resolves");
        assert_eq!(contract.write_target.as_deref(), Some("projection:site"));
        assert_eq!(contract.classes.len(), 8);
        let served = find_contract("shrubbery-site", "latest").expect("site pack is served");
        assert_eq!(served.sha, SHRUBBERY_SITE_GOLDEN_SHA);
        assert_eq!(served.registry_status, "internal-substrate");
    }

    /// ACCEPTANCE PROOF (Observatory Analysis Cell spec SS A.8): every class
    /// in `emporium-observatory` is `store_mode: "virtual"`, so
    /// `class_dispatch::resolve` — the SAME read `write_generic_lane`
    /// consults (`write.rs`: "class '{kind}' resolves to {route:?} ... a
    /// virtual/derived class is never written") and `plan_generic_compute`
    /// consults before minting anything — resolves every one of them to
    /// `DispatchRoute::VirtualSkip`. This is an executable, no-mock proof
    /// that an `emporium_write` against ANY class in this pack fails loudly
    /// (a `BadRequest`, not a silent no-op) before a single triple is
    /// planned or touches the store: the pack is publish-only, never a
    /// second write path for `:projection:obs:*`.
    #[test]
    fn every_observatory_class_is_a_virtual_skip_not_a_write_path() {
        use crate::emporium::class_dispatch::{self, DispatchRoute};
        use crate::emporium::contract::get_vocabulary;

        let contract = get_vocabulary("emporium-observatory")
            .expect("emporium-observatory registered and parses");
        assert_eq!(contract.classes.len(), 11, "the 11 Observatory classes");

        for class_name in contract.classes.keys() {
            let dispatch = class_dispatch::resolve(contract, class_name)
                .unwrap_or_else(|error| panic!("{class_name}: {error}"));
            assert_eq!(
                dispatch.route,
                DispatchRoute::VirtualSkip,
                "{class_name}: emporium-observatory classes must all be \
                 store_mode=virtual so emporium_write can never target them \
                 — got {:?}",
                dispatch.route
            );
            // The EXACT guard `write_generic_lane` runs per record
            // (`write.rs`, just before `resolve_subject`/`gather_and_plan`):
            // a non-SimpleProjection route is the loud rejection.
            assert_ne!(
                dispatch.route,
                DispatchRoute::SimpleProjection,
                "{class_name}: must NOT be directly writable via \
                 emporium_write's generic lane"
            );
        }
    }

    /// The derived SHACL for `emporium-observatory` COMPILES (EA-3 SS4 publish
    /// gate) — the load-bearing structural check that every predicate CURIE
    /// expands against the pack's declared namespaces and the shapes graph
    /// is well-formed Turtle the real rudof engine can load.
    #[test]
    fn observatory_pack_derived_shacl_compiles() {
        use crate::emporium::contract::get_vocabulary;
        use crate::emporium::shacl_validator::compile_check_contract;

        let contract = get_vocabulary("emporium-observatory")
            .expect("emporium-observatory registered and parses");
        let shapes_ttl = compile_check_contract(contract).expect("derived SHACL shapes compile");
        assert!(
            shapes_ttl.contains("http://mnemosyne.dev/observatory#CaptureEvent"),
            "derived shapes target the CaptureEvent class: {shapes_ttl}"
        );
    }

    #[test]
    fn internal_contracts_are_not_served_packs() {
        let names: Vec<&str> = all_contracts().iter().map(|c| c.name).collect();
        assert!(
            !names.contains(&"nomos"),
            "nomos must NOT be a served pack (it validates omphalos only)"
        );
        assert!(
            !names.contains(&"emporium-semantic"),
            "emporium-semantic must NOT be a served pack (it validates :projection:semantic only)"
        );
    }
}

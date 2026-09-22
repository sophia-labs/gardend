//! Wire schemas for the ingest endpoint — accept + validate SHAPE only.
//!
//! Port of `app/schemas/emporium.py` (the request side). These structs validate
//! the thin client's artifacts (parsed workflow, judgment, campaign) as the
//! typed inbox. They do NOT interpret the workflow definition's semantics — the
//! per-class mint that reads these is HELD (its real input is the DSL, designed
//! in parallel). For now the route accepts, validates the shape, surveys the
//! live graph, and ACKs.
//!
//! `serde(deny_unknown_fields)` is deliberately NOT set on the NESTED artifact
//! structs: the platform's pydantic silently drops undeclared fields, and
//! choreograph may carry extra telemetry we don't model yet. We keep that lenient
//! posture for the bodies. It IS set on the TOP-LEVEL [`IngestRequest`] alone, to
//! close the snake_case `dry_run` foot-gun (a camelCase `dryRun` typo would
//! otherwise be silently dropped, defaulting `dry_run` to false and running the
//! destructive apply path).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct PhaseIn {
    pub(crate) order: i64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeIn {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) phase: Option<String>,
    pub(crate) phase_index: i64,
    #[serde(default)]
    pub(crate) agent_type: Option<String>,
    #[serde(default)]
    pub(crate) prompt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentTelemetryIn {
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) phase_index: i64,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) tokens: i64,
    #[serde(default)]
    pub(crate) tool_calls: i64,
    #[serde(default)]
    pub(crate) duration_ms: i64,
    #[serde(default)]
    pub(crate) queued_at: Option<i64>,
    #[serde(default)]
    pub(crate) started_at: Option<i64>,
    #[serde(default)]
    pub(crate) cached: bool,
    #[serde(default)]
    pub(crate) agent_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunIn {
    pub(crate) run_id: String,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) start_time_ms: Option<i64>,
    #[serde(default)]
    pub(crate) end_time_ms: Option<i64>,
    #[serde(default)]
    pub(crate) end_time_iso: Option<String>,
    #[serde(default)]
    pub(crate) total_tokens: i64,
    #[serde(default)]
    pub(crate) agent_count: i64,
    #[serde(default)]
    pub(crate) duration_ms: i64,
    #[serde(default)]
    pub(crate) record_path: Option<String>,
    #[serde(default)]
    pub(crate) phases: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) agents: Vec<AgentTelemetryIn>,
    #[serde(default)]
    pub(crate) result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ParsedWorkflow {
    /// "script-only" | "run-record".
    pub(crate) kind: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) when_to_use: Option<String>,
    pub(crate) script: String,
    pub(crate) script_sha256: String,
    #[serde(default)]
    pub(crate) phases: Vec<PhaseIn>,
    #[serde(default)]
    pub(crate) nodes: Vec<NodeIn>,
    #[serde(default)]
    pub(crate) edges: Vec<Vec<String>>,
    #[serde(default)]
    pub(crate) duplicate_labels: Vec<String>,
    #[serde(default)]
    pub(crate) run: Option<RunIn>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct NewArchetype {
    pub(crate) slug: String,
    pub(crate) title: String,
    pub(crate) role: String,
    pub(crate) template: String,
    #[serde(default)]
    pub(crate) design_notes: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JudgmentInput {
    #[serde(default)]
    pub(crate) short_id: Option<String>,
    #[serde(default)]
    pub(crate) preamble: String,
    #[serde(default)]
    pub(crate) node_archetypes: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) new_archetypes: Vec<NewArchetype>,
    #[serde(default)]
    pub(crate) rationale: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CandidateIn {
    pub(crate) idx: i64,
    pub(crate) text: String,
    pub(crate) sha256: String,
    pub(crate) val_score: f64,
    #[serde(default)]
    pub(crate) holdout_score: Option<f64>,
    pub(crate) generation: i64,
    pub(crate) is_seed: bool,
    pub(crate) is_frontier: bool,
    pub(crate) is_best: bool,
    #[serde(default)]
    pub(crate) parent_idx: Option<i64>,
    #[serde(default)]
    pub(crate) per_task: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CampaignRecord {
    pub(crate) campaign_id: String,
    pub(crate) kind: String,
    pub(crate) archetype_doc_id: String,
    pub(crate) archetype_name: String,
    #[serde(default)]
    pub(crate) objective: Option<String>,
    #[serde(default)]
    pub(crate) task_model: Option<String>,
    #[serde(default)]
    pub(crate) reflection_model: Option<String>,
    #[serde(default)]
    pub(crate) candidates: Vec<CandidateIn>,
    #[serde(flatten)]
    pub(crate) extra: std::collections::BTreeMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Memory payload (sophia-memory-core, vocab "memory") — the typed inbox for the
// mem: family. camelCase wire; lenient nested posture (NO deny_unknown_fields)
// for telemetry passthrough, with an explicit memory-only unknown-field scan in
// validate() so a `sourceRef`-vs-`sourceRefs` typo cannot silently strip
// provenance (the I1 gate).
// ---------------------------------------------------------------------------

/// The v1 closed enums, asserted against the golden by the enum-divergence test.
/// These are the producer-side membership checks (the contract engine validates
/// datatype, not enum value — §10 standing risk).
pub(crate) const MEM_SCOPES: &[&str] = &[
    "user",
    "agent",
    "graph",
    "thread",
    "task",
    "organization",
    "environment",
    "global",
];
pub(crate) const MEM_CONTENT_ORIENTATIONS: &[&str] = &[
    "knowledge",
    "execution",
    "affective",
    "safety",
    "structural",
    "policy",
];
pub(crate) const MEM_VISIBILITIES: &[&str] = &["private", "shared", "system", "read_only"];
/// The full status enum (lifecycle outputs included). Producers may emit only the
/// [`MEM_PRODUCER_STATUSES`] subset; superseded/archived/… are lifecycle-op
/// outputs the planner mints, not inbound shapes.
pub(crate) const MEM_STATUSES: &[&str] = &[
    "draft",
    "active",
    "stale",
    "superseded",
    "contradicted",
    "archived",
    "deleted",
    "quarantined",
];
/// The statuses a producer is allowed to emit (the others are planner outputs).
pub(crate) const MEM_PRODUCER_STATUSES: &[&str] = &["draft", "active"];
/// The 16-family `mem:kind` enum (a closed DIMENSION, not subclasses).
pub(crate) const MEM_KINDS: &[&str] = &[
    "ClaimMemory",
    "ProfileMemory",
    "PreferenceMemory",
    "AffectiveMemory",
    "EventMemory",
    "EpisodeMemory",
    "SummaryMemory",
    "ProcedureMemory",
    "TaskStateMemory",
    "TrajectoryMemory",
    "FailureMemory",
    "AffordanceMemory",
    "WorldStateMemory",
    "SafetyMemory",
    "PolicyMemory",
    "StructuralMemory",
];
/// The closed `mem:sourceKind` enum (8 source kinds).
pub(crate) const MEM_SOURCE_KINDS: &[&str] = &[
    "ConversationTurn",
    "DocumentBlock",
    "ToolCall",
    "ToolResult",
    "ArtifactReference",
    "ExternalEvent",
    "PlatformEvent",
    "CodeChangeEvent",
];

/// One provenance anchor (`mem:SourceReference`) on an inbound memory record.
/// `serde(default)` everywhere keeps the nested posture lenient.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceRefIn {
    pub(crate) source_kind: String,
    #[serde(default)]
    pub(crate) source_label: Option<String>,
    /// CRDT block id (`<docUri>#<blockId>` when paired with `document_id`).
    #[serde(default)]
    pub(crate) block_id: Option<String>,
    #[serde(default)]
    pub(crate) document_id: Option<String>,
    #[serde(default)]
    pub(crate) external_id: Option<String>,
    #[serde(default)]
    pub(crate) external_uri: Option<String>,
    #[serde(default)]
    pub(crate) observed_at: Option<i64>,
    #[serde(default)]
    pub(crate) trust_tier: Option<String>,
}

/// One reified evidence edge (`mem:EvidenceLink`) on an inbound memory record.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvidenceIn {
    /// `supports|contradicts|reconciles|validates`.
    pub(crate) relation: String,
    /// The supported/contradicted target (record / Claim / SourceReference IRI).
    #[serde(default)]
    pub(crate) target_ref: Option<String>,
    #[serde(default)]
    pub(crate) confidence: Option<f64>,
    #[serde(default)]
    pub(crate) evidence_strength: Option<f64>,
    #[serde(default)]
    pub(crate) trust_tier: Option<String>,
}

/// One inbound typed memory record (`mem:MemoryRecord` + its provenance/evidence).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryRecordIn {
    /// Caller-supplied correlation id (echoed in failures / the queue). Not part
    /// of the content-hash subject.
    #[serde(default)]
    pub(crate) client_ref: Option<String>,
    pub(crate) scope: String,
    pub(crate) kind: String,
    pub(crate) content_orientation: String,
    pub(crate) visibility: String,
    pub(crate) status: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) source_refs: Vec<SourceRefIn>,
    #[serde(default)]
    pub(crate) evidence: Vec<EvidenceIn>,
    #[serde(default)]
    pub(crate) observed_at: Option<i64>,
    #[serde(default)]
    pub(crate) valid_from: Option<i64>,
    #[serde(default)]
    pub(crate) is_current: Option<bool>,
    #[serde(default)]
    pub(crate) confidence: Option<f64>,
    #[serde(default)]
    pub(crate) valence: Option<f64>,
    #[serde(default)]
    pub(crate) agent_id: Option<String>,
    /// The OBSERVER (witness) this memory is attributed to — the agt:Agent IRI the
    /// wire supplies (`agent-<hex>` or a full IRI). Per-observer routing (Variant
    /// B) keys on this; it is folded into the content-hash subject (Variant A,
    /// CONDITIONALLY — empty ⇒ today's IRI byte-for-byte) and serialized as the
    /// canonical `mem:observedBy` predicate. Empty/absent ⇒ the shared commons.
    /// Distinct from `agent_id` (the legacy `mem:agentId` alias, scope sugar).
    #[serde(default)]
    pub(crate) observer_agent_id: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) supersedes_ref: Option<String>,
    #[serde(default)]
    pub(crate) contradicts_ref: Option<String>,
}

/// The set of known camelCase keys on a `MemoryRecordIn` body — used by the
/// memory-only unknown-field scan (a `sourceRef` typo for `sourceRefs` would land
/// outside this set and be flagged, never silently dropping provenance).
const MEMORY_RECORD_KNOWN_KEYS: &[&str] = &[
    "clientRef",
    "scope",
    "kind",
    "contentOrientation",
    "visibility",
    "status",
    "content",
    "sourceRefs",
    "evidence",
    "observedAt",
    "validFrom",
    "isCurrent",
    "confidence",
    "valence",
    "agentId",
    "observerAgentId",
    "tags",
    "supersedesRef",
    "contradictsRef",
];

/// One inbound GENERIC record (EA-3): a flat object whose `kind` names a class of
/// the request's vocab and whose other keys are the predicate VALUES + a `localId`
/// the subject_rule template binds. The body is kept as raw JSON (`serde_json::Map`)
/// because the predicate set is vocab-defined, not known at compile time; the
/// generic planner reads it against the registered contract's `ClassSpec`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct GenericRecordIn {
    /// The contract class name this record instantiates (e.g. `"Bookmark"`).
    pub(crate) kind: String,
    /// The remaining fields: `localId` (bound by the subject_rule template) + one
    /// entry per predicate CURIE/local-name the class declares. Kept flat + raw so
    /// the generic planner can map them against the contract without a per-vocab
    /// struct. Lenient by construction (unknown keys land here; the planner's
    /// frozen-vocab guard rejects predicates outside the class).
    #[serde(flatten)]
    pub(crate) fields: serde_json::Map<String, serde_json::Value>,
}

/// The discriminated ingest payload (`kind`: "workflow" | "campaign" | "memory" |
/// "generic"). The `generic` arm (EA-3) is the vocab-agnostic ingest lane: a batch
/// of [`GenericRecordIn`] minted against ANY registered vocab via the subject-rule
/// grammar + `render_class_triples`, materialized into the vocab's `write_target`
/// projection sink.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum IngestPayload {
    Workflow {
        parsed: ParsedWorkflow,
        #[serde(default)]
        judgment: Option<JudgmentInput>,
        #[serde(default)]
        journal_document_id: Option<String>,
    },
    Campaign {
        campaign: CampaignRecord,
    },
    Memory {
        records: Vec<MemoryRecordIn>,
    },
    Generic {
        records: Vec<GenericRecordIn>,
    },
}

/// The top-level ingest request body.
///
/// `deny_unknown_fields` is set HERE (and ONLY here — the nested structs keep the
/// lenient `serde(default)` posture so undeclared telemetry is silently dropped).
/// The top level is the snake_case `dry_run` foot-gun seam: a producer that sends
/// camelCase `dryRun` (or any other top-level typo) is now REJECTED instead of
/// silently defaulting `dry_run` to `false` and running the destructive apply path.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IngestRequest {
    #[serde(default = "default_vocab")]
    pub(crate) vocab: String,
    #[serde(default)]
    pub(crate) dry_run: bool,
    /// GENERIC-vocab apply semantics. `false` (the default): SUBJECT-SCOPED
    /// UPSERT — the batch reconciles only the subjects it contains; class
    /// siblings are untouched. `true`: the batch is the ENTIRE desired state of
    /// every class it mentions and absent siblings are DELETED (whole-class
    /// replace). Destructive, so it must be asked for by name. Ignored by the
    /// workflow/memory kinds (their appliers have their own scoping).
    #[serde(default)]
    pub(crate) replace_class: bool,
    pub(crate) payload: IngestPayload,
}

fn default_vocab() -> String {
    "workflow".to_string()
}

impl IngestRequest {
    /// Shape-level validation (the typed inbox gate). DSL-semantic validation
    /// (archetype refs against the live graph, etc.) is HELD with the mint.
    pub(crate) fn validate(&self) -> Result<(), String> {
        use crate::emporium::contract::get_vocabulary;

        // The Generic arm (EA-3) opens the gate to ANY REGISTERED vocab whose
        // contract declares a `projection:*` write_target — the in-graph projection
        // sink the generic materializer writes. The legacy wf/campaign/mem families
        // keep their own (narrower) whitelist below; the generic lane is opt-in via
        // the `generic` payload discriminant, so it cannot widen the wf/mem gates.
        if let IngestPayload::Generic { records } = &self.payload {
            // EMBEDDED vocab: the strict, store-free shape gate (a 400 before any
            // read). CHAMBER vocab (EA-3 §4): the agent's RUNTIME-proposed ontology
            // is NOT in the embedded registry — it lives in the chamber graph, which
            // `validate()` (a method on the request, no store handle) cannot read.
            // For an unregistered vocab we therefore defer the contract gate to the
            // store-backed spine (`gather_and_plan` resolves the in-graph ontology
            // and loud-fails on a genuinely-unknown vocab) and only run the
            // vocab-INDEPENDENT shape check (non-empty batch + every record naming a
            // kind + a non-empty localId) — which needs no contract.
            if let Some(contract) = get_vocabulary(&self.vocab) {
                let target = contract.write_target.as_deref().unwrap_or_default();
                if !target.starts_with("projection:") {
                    return Err(format!(
                        "generic payload: vocab '{}' has no 'projection:*' write_target \
                         (got '{target}') — the generic lane materializes into a projection sink",
                        self.vocab
                    ));
                }
                return Self::validate_generic(contract, records);
            }
            return Self::validate_generic_shape_only(records);
        }

        // The guard lift: accept the workflow/campaign family AND the mem* family.
        // (Historically this rejected everything but "workflow" — campaign was
        // unreachable via validate even though the planner handled it; the lift
        // also re-enables campaign. Memory is the additive family.)
        let is_mem = self.vocab.starts_with("mem");
        if !matches!(self.vocab.as_str(), "workflow" | "campaign") && !is_mem {
            return Err(format!(
                "unsupported vocab '{}': only 'workflow', 'campaign', a 'mem*' family, \
                 or a registered projection vocab (via a 'generic' payload) are served",
                self.vocab
            ));
        }
        // Payload/vocab agreement: the memory payload requires a mem* vocab and
        // vice versa, so a wf vocab cannot smuggle memory records past the wf path.
        match (&self.payload, is_mem) {
            (IngestPayload::Memory { .. }, false) => {
                return Err(format!(
                    "memory payload requires a 'mem*' vocab, got '{}'",
                    self.vocab
                ));
            }
            (IngestPayload::Workflow { .. } | IngestPayload::Campaign { .. }, true) => {
                return Err(format!(
                    "vocab '{}' is a memory family but the payload is not 'memory'",
                    self.vocab
                ));
            }
            _ => {}
        }
        match &self.payload {
            IngestPayload::Memory { records } => {
                Self::validate_memory(records)?;
            }
            IngestPayload::Workflow { parsed, .. } => {
                if parsed.name.trim().is_empty() {
                    return Err("workflow payload: parsed.name is empty".to_string());
                }
                if parsed.script_sha256.trim().is_empty() {
                    return Err("workflow payload: parsed.scriptSha256 is empty".to_string());
                }
                if !matches!(parsed.kind.as_str(), "script-only" | "run-record") {
                    return Err(format!("workflow payload: unknown kind '{}'", parsed.kind));
                }
            }
            IngestPayload::Campaign { campaign } => {
                if campaign.campaign_id.trim().is_empty() {
                    return Err("campaign payload: campaignId is empty".to_string());
                }
                if campaign.archetype_doc_id.trim().is_empty() {
                    return Err("campaign payload: archetypeDocId is empty".to_string());
                }
            }
            // Handled by the early-return generic branch above.
            IngestPayload::Generic { .. } => unreachable!("generic payload handled above"),
        }
        Ok(())
    }

    /// The GENERIC ingest SHAPE gate (EA-3): a 400 BEFORE any plan/apply. Checks
    /// only what is vocab-independent — a non-empty batch, every record naming a
    /// class the contract declares, and a non-empty `localId` (the subject_rule's
    /// binding). Predicate-level validity (required predicates present, no rogue
    /// predicates, datatype) is enforced DOWNSTREAM by the planner's
    /// `render_class_triples` frozen-vocab guard + the SHACL gate — this is the
    /// cheap pre-plan shape check, not a re-implementation of those.
    /// The vocab-INDEPENDENT half of [`Self::validate_generic`] (EA-3 §4 chamber
    /// lane): a non-empty batch where every record carries a non-empty `localId`.
    /// The `kind`/predicate validity is deferred to the store-backed spine, which
    /// resolves the agent's RUNTIME-proposed ontology from the chamber graph and
    /// loud-fails there if the vocab is genuinely unknown. This is the gate for a
    /// vocab not in the embedded registry (the request method has no store handle).
    pub(crate) fn validate_generic_shape_only(records: &[GenericRecordIn]) -> Result<(), String> {
        if records.is_empty() {
            return Err("generic payload: records is empty".to_string());
        }
        for (i, r) in records.iter().enumerate() {
            let at = |what: &str| format!("generic record [{i}]: {what}");
            if r.kind.trim().is_empty() {
                return Err(at("missing or empty 'kind'"));
            }
            let local_id = r
                .fields
                .get("localId")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if local_id.trim().is_empty() {
                return Err(at(
                    "missing or empty 'localId' (the subject_rule template binding)",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_generic(
        contract: &crate::emporium::contract::VocabularyContract,
        records: &[GenericRecordIn],
    ) -> Result<(), String> {
        if records.is_empty() {
            return Err("generic payload: records is empty".to_string());
        }
        for (i, r) in records.iter().enumerate() {
            let at = |what: &str| format!("generic record [{i}]: {what}");
            if !contract.classes.contains_key(&r.kind) {
                return Err(at(&format!(
                    "kind '{}' is not a class of vocab '{}'",
                    r.kind, contract.name
                )));
            }
            let local_id = r
                .fields
                .get("localId")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if local_id.trim().is_empty() {
                return Err(at(
                    "missing or empty 'localId' (the subject_rule template binding)",
                ));
            }
        }
        Ok(())
    }

    /// The LOUD memory shape gate (I1 + enum membership). A failure here is a
    /// 400 BEFORE any plan/apply: it is a malformed request, never a silent drop.
    pub(crate) fn validate_memory(records: &[MemoryRecordIn]) -> Result<(), String> {
        if records.is_empty() {
            return Err("memory payload: records is empty".to_string());
        }
        for (i, r) in records.iter().enumerate() {
            let at = |what: &str| format!("memory record [{i}]: {what}");
            if r.content.trim().is_empty() {
                return Err(at("content is empty"));
            }
            if !MEM_SCOPES.contains(&r.scope.as_str()) {
                return Err(at(&format!("scope '{}' not in the v1 enum", r.scope)));
            }
            if !MEM_KINDS.contains(&r.kind.as_str()) {
                return Err(at(&format!("kind '{}' not in the v1 enum", r.kind)));
            }
            if !MEM_CONTENT_ORIENTATIONS.contains(&r.content_orientation.as_str()) {
                return Err(at(&format!(
                    "contentOrientation '{}' not in the v1 enum",
                    r.content_orientation
                )));
            }
            if !MEM_VISIBILITIES.contains(&r.visibility.as_str()) {
                return Err(at(&format!(
                    "visibility '{}' not in the v1 enum",
                    r.visibility
                )));
            }
            if !MEM_STATUSES.contains(&r.status.as_str()) {
                return Err(at(&format!("status '{}' not in the v1 enum", r.status)));
            }
            if !MEM_PRODUCER_STATUSES.contains(&r.status.as_str()) {
                return Err(at(&format!(
                    "status '{}' is a lifecycle output; producers may emit only {:?}",
                    r.status, MEM_PRODUCER_STATUSES
                )));
            }
            // The I1 provenance gate — the first loud line of defense. A
            // `sourceRef`-vs-`sourceRefs` typo lands here as an empty list.
            if r.source_refs.is_empty() {
                return Err(at("sourceRefs is empty — I1 NO MEMORY WITHOUT PROVENANCE \
                     (check for a sourceRef vs sourceRefs typo)"));
            }
            for (j, sr) in r.source_refs.iter().enumerate() {
                if !MEM_SOURCE_KINDS.contains(&sr.source_kind.as_str()) {
                    return Err(at(&format!(
                        "sourceRefs[{j}].sourceKind '{}' not in the v1 enum",
                        sr.source_kind
                    )));
                }
            }
        }
        Ok(())
    }

    /// Memory-only unknown-field scan over the RAW inbound JSON. Callers that hold
    /// the raw body (the route / `remember`) run this BEFORE deserialize-driven
    /// drop so a misspelled provenance key (`sourceRef`) is flagged loudly instead
    /// of being silently stripped. `raw` is the `payload.records` array value.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn scan_unknown_memory_keys(records_raw: &serde_json::Value) -> Result<(), String> {
        let Some(arr) = records_raw.as_array() else {
            return Ok(());
        };
        for (i, rec) in arr.iter().enumerate() {
            let Some(obj) = rec.as_object() else { continue };
            let unknown: Vec<&str> = obj
                .keys()
                .map(String::as_str)
                .filter(|k| !MEMORY_RECORD_KNOWN_KEYS.contains(k))
                .collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "memory record [{i}]: unknown field(s) {unknown:?} — refusing to \
                     silently drop (a sourceRef vs sourceRefs typo would strip provenance)"
                ));
            }
        }
        Ok(())
    }

    /// The workflow/campaign/memory name for the ACK + survey scoping.
    pub(crate) fn subject_name(&self) -> String {
        match &self.payload {
            IngestPayload::Workflow { parsed, .. } => parsed.name.clone(),
            IngestPayload::Campaign { campaign } => campaign.archetype_name.clone(),
            IngestPayload::Memory { records } => records
                .first()
                .and_then(|r| r.client_ref.clone())
                .unwrap_or_else(|| "memory-batch".to_string()),
            IngestPayload::Generic { records } => records
                .first()
                .and_then(|r| r.fields.get("localId"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| "generic-batch".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared TS↔Rust wire golden, embedded at compile time. This is a
    /// BYTE-IDENTICAL copy of choreograph's
    /// `src/test/fixtures/ingest_request.golden.json` (kept in lockstep until a
    /// shared cross-repo fixture mechanism lands). The outer file wraps the actual
    /// `IngestRequest` under `request` so it can carry the keep-in-sync header
    /// `_comment` without that comment becoming an unknown top-level field (the
    /// `IngestRequest` now `deny_unknown_fields`).
    const INGEST_REQUEST_GOLDEN_JSON: &str = include_str!("fixtures/ingest_request.golden.json");

    fn minimal_workflow_json() -> serde_json::Value {
        serde_json::json!({
            "vocab": "workflow",
            "dry_run": true,
            "payload": {
                "kind": "workflow",
                "parsed": {
                    "kind": "run-record",
                    "name": "demo",
                    "description": "d",
                    "script": "export const meta = {}\n",
                    "scriptSha256": "abc",
                    "phases": [{"order": 1, "title": "Go"}],
                    "nodes": [{"label": "a", "phaseIndex": 1, "prompt": "p1"}],
                    "edges": [["a", "b"]]
                },
                "judgment": {"shortId": "wf-demo", "preamble": "Demo.", "nodeArchetypes": {}}
            }
        })
    }

    #[test]
    fn ingest_request_deserializes_workflow_payload() {
        let req: IngestRequest = serde_json::from_value(minimal_workflow_json()).unwrap();
        assert!(req.dry_run);
        assert_eq!(req.subject_name(), "demo");
        req.validate().expect("minimal workflow payload validates");
        match req.payload {
            IngestPayload::Workflow {
                parsed, judgment, ..
            } => {
                assert_eq!(parsed.name, "demo");
                assert_eq!(parsed.phases.len(), 1);
                assert_eq!(parsed.nodes[0].phase_index, 1);
                assert_eq!(judgment.unwrap().short_id.as_deref(), Some("wf-demo"));
            }
            _ => panic!("expected workflow payload"),
        }
    }

    #[test]
    fn ingest_request_deserializes_campaign_payload() {
        let json = serde_json::json!({
            "payload": {
                "kind": "campaign",
                "campaign": {
                    "campaignId": "camp-1",
                    "kind": "gepa",
                    "archetypeDocId": "agent-alpha",
                    "archetypeName": "alpha",
                    "candidates": []
                }
            }
        });
        let req: IngestRequest = serde_json::from_value(json).unwrap();
        // vocab defaults to "workflow"; dry_run defaults false.
        assert_eq!(req.vocab, "workflow");
        assert!(!req.dry_run);
        req.validate().expect("campaign payload validates");
        assert_eq!(req.subject_name(), "alpha");
    }

    #[test]
    fn validate_rejects_unknown_vocab() {
        let mut json = minimal_workflow_json();
        json["vocab"] = serde_json::json!("biblio");
        let req: IngestRequest = serde_json::from_value(json).unwrap();
        assert!(req.validate().is_err());
    }

    // ── memory payload harness (pure-unit slice of Caveat-3) ──

    fn minimal_memory_json() -> serde_json::Value {
        serde_json::json!({
            "vocab": "memory",
            "dry_run": false,
            "payload": {
                "kind": "memory",
                "records": [{
                    "clientRef": "remember-1-0",
                    "scope": "agent",
                    "kind": "ClaimMemory",
                    "contentOrientation": "knowledge",
                    "visibility": "private",
                    "status": "active",
                    "content": "vera prefers fish CLI over zsh",
                    "sourceRefs": [
                        {"sourceKind": "DocumentBlock", "blockId": "abc123", "documentId": "doc-shell"}
                    ],
                    "observedAt": 1718700000000_i64,
                    "validFrom": 1718700000000_i64,
                    "isCurrent": true,
                    "confidence": 0.92
                }]
            }
        })
    }

    #[test]
    fn ingest_request_deserializes_memory_payload() {
        let req: IngestRequest = serde_json::from_value(minimal_memory_json()).unwrap();
        assert_eq!(req.vocab, "memory");
        assert_eq!(req.subject_name(), "remember-1-0");
        req.validate().expect("minimal memory payload validates");
        match &req.payload {
            IngestPayload::Memory { records } => {
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].source_refs.len(), 1);
                assert_eq!(
                    records[0].source_refs[0].block_id.as_deref(),
                    Some("abc123")
                );
            }
            _ => panic!("expected memory payload"),
        }
    }

    #[test]
    fn memory_validate_rejects_provenanceless_write() {
        let mut json = minimal_memory_json();
        json["payload"]["records"][0]["sourceRefs"] = serde_json::json!([]);
        let req: IngestRequest = serde_json::from_value(json).unwrap();
        let err = req
            .validate()
            .expect_err("I1 gate must reject empty sourceRefs");
        assert!(err.contains("PROVENANCE"), "{err}");
    }

    #[test]
    fn memory_validate_rejects_bad_enum() {
        let mut json = minimal_memory_json();
        json["payload"]["records"][0]["scope"] = serde_json::json!("galaxy");
        let req: IngestRequest = serde_json::from_value(json).unwrap();
        assert!(req.validate().is_err());
    }

    #[test]
    fn memory_validate_rejects_lifecycle_status_from_producer() {
        let mut json = minimal_memory_json();
        json["payload"]["records"][0]["status"] = serde_json::json!("superseded");
        let req: IngestRequest = serde_json::from_value(json).unwrap();
        assert!(req.validate().is_err());
    }

    #[test]
    fn memory_vocab_payload_mismatch_is_rejected() {
        // A wf vocab carrying a memory payload (or vice versa) is loud-rejected so
        // memory cannot be smuggled through the wf path.
        let mut json = minimal_memory_json();
        json["vocab"] = serde_json::json!("workflow");
        let req: IngestRequest = serde_json::from_value(json).unwrap();
        assert!(req.validate().is_err());
    }

    #[test]
    fn scan_unknown_memory_keys_flags_sourceref_typo() {
        let records = serde_json::json!([{
            "scope": "agent", "kind": "ClaimMemory", "contentOrientation": "knowledge",
            "visibility": "private", "status": "active", "content": "x",
            "sourceRef": [{"sourceKind": "ConversationTurn"}]  // typo: singular
        }]);
        let err = IngestRequest::scan_unknown_memory_keys(&records)
            .expect_err("a sourceRef typo must be flagged, not silently dropped");
        assert!(err.contains("sourceRef"), "{err}");
        // The correct spelling passes the scan.
        let ok = serde_json::json!([{
            "scope": "agent", "kind": "ClaimMemory", "contentOrientation": "knowledge",
            "visibility": "private", "status": "active", "content": "x",
            "sourceRefs": [{"sourceKind": "ConversationTurn"}]
        }]);
        assert!(IngestRequest::scan_unknown_memory_keys(&ok).is_ok());
    }

    /// Enum-divergence: the producer-side enum `const` slices MUST match the
    /// membership the golden contract declares (no one-sided enum addition).
    #[test]
    fn memory_enums_match_the_golden() {
        use crate::emporium::contract::memory_core_vocabulary;
        let c = memory_core_vocabulary();
        // The golden encodes enum membership in the predicate `source` strings
        // (e.g. "enum: knowledge|execution|..."). Extract + compare for the two
        // closed dimensions the planner mints subject identity from.
        let source_of = |class: &str, pred: &str| -> String {
            c.classes[class].predicates[pred]
                .source
                .clone()
                .unwrap_or_default()
        };
        let orient_src = source_of("MemoryRecord", "mem:contentOrientation");
        for v in MEM_CONTENT_ORIENTATIONS {
            assert!(
                orient_src.contains(v),
                "contentOrientation enum '{v}' absent from golden source '{orient_src}'"
            );
        }
        let sk_src = source_of("SourceReference", "mem:sourceKind");
        for v in MEM_SOURCE_KINDS {
            assert!(
                sk_src.contains(v),
                "sourceKind enum '{v}' absent from golden source '{sk_src}'"
            );
        }
        let scope_src = source_of("MemoryRecord", "mem:scope");
        for v in MEM_SCOPES {
            assert!(
                scope_src.contains(v),
                "scope enum '{v}' absent from golden source '{scope_src}'"
            );
        }
    }

    #[test]
    fn validate_rejects_empty_name() {
        let mut json = minimal_workflow_json();
        json["payload"]["parsed"]["name"] = serde_json::json!("");
        let req: IngestRequest = serde_json::from_value(json).unwrap();
        assert!(req.validate().is_err());
    }

    /// The shared golden survives a serde round-trip into the typed inbox, and the
    /// key wire fields land where the Rust consumer reads them. A rename/drop on
    /// the RUST side (e.g. `run_id` → something else, or dropping `start_time_ms`)
    /// fails THIS test; choreograph has a mirror test over the byte-identical copy
    /// that fails on a TS-side rename. Together they pin the wire from both ends.
    #[test]
    fn shared_golden_deserializes_into_ingest_request() {
        // The golden wraps the request under `request` alongside the keep-in-sync
        // `_comment` header; pull the request out before deserializing.
        let outer: serde_json::Value =
            serde_json::from_str(INGEST_REQUEST_GOLDEN_JSON).expect("golden is valid JSON");
        assert!(
            outer["_comment"]
                .as_str()
                .unwrap_or_default()
                .contains("BYTE-IDENTICAL"),
            "golden must carry the cross-repo keep-in-sync header comment"
        );
        let request_value = outer["request"].clone();

        let req: IngestRequest = serde_json::from_value(request_value)
            .expect("golden request deserializes into IngestRequest");

        // Top-level discriminator + flags survive.
        assert_eq!(req.vocab, "workflow");
        assert!(!req.dry_run);
        req.validate().expect("golden request validates");

        match &req.payload {
            // payload "kind" discriminator → Workflow.
            IngestPayload::Workflow { parsed, .. } => {
                // parsed "kind" discriminator → run-record.
                assert_eq!(parsed.kind, "run-record");
                assert_eq!(parsed.name, "graph-survey-synthesize");
                assert_eq!(parsed.script_sha256, "deadbeefcafef00d");

                let run = parsed.run.as_ref().expect("run-record carries a run");
                assert_eq!(run.run_id, "wfr-abc123");
                // The rename trap: TS startTimeMs/endTimeMs → Rust start_time_ms/
                // end_time_ms. If either Rust field were renamed, these are None.
                assert_eq!(run.start_time_ms, Some(1_700_000_000_000));
                assert_eq!(run.end_time_ms, Some(1_700_000_005_000));

                assert_eq!(run.agents.len(), 2);
                let first = &run.agents[0];
                assert_eq!(first.label, "map:d1");
                assert_eq!(first.started_at, Some(1_700_000_000_200));
                let second = &run.agents[1];
                assert_eq!(second.label, "synthesize");
                assert_eq!(second.started_at, Some(1_700_000_001_100));
            }
            IngestPayload::Campaign { .. }
            | IngestPayload::Memory { .. }
            | IngestPayload::Generic { .. } => {
                panic!("expected workflow payload")
            }
        }
    }

    /// `deny_unknown_fields` on the TOP-LEVEL `IngestRequest` closes the snake_case
    /// `dry_run` foot-gun: a producer that sends the camelCase `dryRun` (or any
    /// other unmodeled top-level key) is now REJECTED at deserialize, instead of
    /// the typo being silently dropped and `dry_run` defaulting to `false` (which
    /// would silently run the destructive apply path).
    #[test]
    fn top_level_unknown_field_is_rejected() {
        let mut json = minimal_workflow_json();
        // The exact foot-gun: camelCase `dryRun` instead of snake_case `dry_run`.
        json["dryRun"] = serde_json::json!(true);
        let result: Result<IngestRequest, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "a top-level `dryRun` typo must be rejected by deny_unknown_fields"
        );

        // Sanity: the lenient nested posture is UNCHANGED — an unknown field on a
        // nested struct (here on `parsed`) is still silently dropped, not rejected.
        let mut json = minimal_workflow_json();
        json["payload"]["parsed"]["someFutureTelemetry"] = serde_json::json!("ok");
        let nested: IngestRequest = serde_json::from_value(json)
            .expect("unknown NESTED fields stay lenient (serde default posture)");
        assert_eq!(nested.subject_name(), "demo");
    }
}

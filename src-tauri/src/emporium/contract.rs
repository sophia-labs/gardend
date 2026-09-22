//! The `VocabularyContract` model — the typed, parsed form of the `wf:` golden
//! contract that the ingest planner, applier, and assert suite read.
//!
//! This is a faithful port of the platform's `app/services/emporium/contract.py`
//! `VocabularyContract` (and its component specs). It deserializes from a
//! **clone** of the embedded golden JSON ([`vocabs::WORKFLOW_GOLDEN_JSON`]) via a
//! `OnceLock`; it never re-serializes the served body. The served bytes stay
//! `vocabs::workflow_contract().json` verbatim, and the sha-pin test
//! (`vocabs::embedded_workflow_sha_is_pinned`) remains the load-bearing
//! invariant — re-serializing here would risk a byte/sha drift.
//!
//! Only the read-surface (`expand`, `primary_namespace`, `known_predicate_uris`)
//! is ported; the canonical-serialization path the platform uses to *produce*
//! the sha is deliberately not ported (we embed the bytes, we don't mint them).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::emporium::vocabs::{
    DOCUMENT_GOLDEN_JSON, GRAPH_GOLDEN_JSON, MEMORY_CORE_GOLDEN_JSON, NOMOS_GOLDEN_JSON,
    SALIENCE_GOLDEN_JSON, SEMANTIC_GOLDEN_JSON, SONG_GOLDEN_JSON, WIRES_GOLDEN_JSON,
    WORKFLOW_GOLDEN_JSON, WORKSPACE_GOLDEN_JSON,
};

/// Literal datatypes a predicate may carry. `uri` means an object property;
/// `dateTime` is camelCase (matches the golden token exactly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[allow(non_camel_case_types)]
pub(crate) enum Datatype {
    string,
    uri,
    integer,
    long,
    /// IEEE single-precision. DISTINCT from `double`: SHACL `sh:datatype` is an
    /// EXACT IRI match (no xsd numeric hierarchy — a `^^xsd:float` literal does
    /// NOT satisfy `sh:datatype xsd:double`, proven by the validator probe), and
    /// the salience materializer's `push_float_triple` emits `^^xsd:float`, so the
    /// retrofit contracts need this variant to derive a shape that matches the
    /// real projection. The served `wf:`/memory goldens never use it.
    float,
    double,
    boolean,
    dateTime,
}

/// One predicate on one class, keyed by CURIE in [`ClassSpec::predicates`].
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PredicateSpec {
    pub(crate) datatype: Datatype,
    #[serde(default)]
    pub(crate) required: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) multi: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) comment: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) face_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    CurrentState,
    EventLog,
    Derived,
}

impl SourceKind {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "current-state" => Some(Self::CurrentState),
            "event-log" => Some(Self::EventLog),
            "derived" => Some(Self::Derived),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityKind {
    UrnTemplate,
    ContentHash,
    LogicalId,
    DocUri,
    EventId,
    ResolveByQuery,
}

impl IdentityKind {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "urn-template" => Some(Self::UrnTemplate),
            "content-hash" => Some(Self::ContentHash),
            "logical-id" => Some(Self::LogicalId),
            "doc-uri" => Some(Self::DocUri),
            "event-id" => Some(Self::EventId),
            "resolve-by-query" => Some(Self::ResolveByQuery),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreMode {
    Materialize,
    Virtual,
}

impl StoreMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "materialize" => Some(Self::Materialize),
            "virtual" => Some(Self::Virtual),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Enforcement {
    Halt,
    FlagAndAccept,
    Warning,
    StoredV2,
    Off,
}

impl Enforcement {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "halt" => Some(Self::Halt),
            "flag-and-accept" => Some(Self::FlagAndAccept),
            "warning" => Some(Self::Warning),
            "stored-v2" => Some(Self::StoredV2),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReconciliationStrategy {
    ProducerDirected,
    Contested,
    CausalLww,
    EvidenceWeighted,
    CodeBacked,
}

impl ReconciliationStrategy {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "producerDirected" => Some(Self::ProducerDirected),
            "contested" => Some(Self::Contested),
            "causalLww" => Some(Self::CausalLww),
            "evidenceWeighted" => Some(Self::EvidenceWeighted),
            "codeBacked" => Some(Self::CodeBacked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchMode {
    CurrentStateMaterialize,
    DerivedMaterialize,
    DerivedVirtual,
    EventLogFold,
}

impl DispatchMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "current-state-materialize" => Some(Self::CurrentStateMaterialize),
            "derived-materialize" => Some(Self::DerivedMaterialize),
            "derived-virtual" => Some(Self::DerivedVirtual),
            "event-log-fold" => Some(Self::EventLogFold),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MaterializationSignature<'a> {
    pub(crate) source_kind: SourceKind,
    pub(crate) identity_kind: IdentityKind,
    pub(crate) store_mode: StoreMode,
    pub(crate) store_target: &'a str,
    pub(crate) enforcement: Enforcement,
    pub(crate) reconciliation_strategy: ReconciliationStrategy,
    pub(crate) dispatch_mode: DispatchMode,
}

/// One vocabulary class: its `rdf:type`(s), subject convention, predicates.
/// `rdf_types` may be empty (`Protocol`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClassSpec {
    #[serde(default)]
    pub(crate) rdf_types: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) subject_rule: Option<String>,
    #[serde(default)]
    pub(crate) predicates: BTreeMap<String, PredicateSpec>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) comment: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) source_kind: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) identity_kind: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) identity_fields: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) store_target: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) store_mode: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) enforcement: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) reconciliation_strategy: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) dispatch_mode: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) projection_faces: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) public_jurisdiction: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) derived_from_query: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) faces_oracle: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) authoring_grain: Option<serde_json::Value>,
}

/// Label → URI/doc-id slug rule (must match any reverse parse downstream).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SlugRule {
    pub(crate) pattern: String,
    pub(crate) replacement: String,
    pub(crate) strip: String,
    #[serde(default)]
    pub(crate) lowercase: bool,
}

/// Workspace folder convention. Retained for fidelity with the golden shape
/// (the DSL-agnostic slice does not consume folders yet).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct FolderRule {
    pub(crate) folder_id: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) parent: Option<String>,
    #[serde(default)]
    pub(crate) only_if: Option<String>,
}

/// CRDT wire convention between documents. `predicate_uri` is set only for
/// custom (non-builtin) predicates; builtin wires resolve by short name.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct WireRule {
    pub(crate) from_kind: String,
    pub(crate) to_kind: String,
    pub(crate) source: String,
    #[serde(default)]
    pub(crate) predicate_uri: Option<String>,
    #[serde(default)]
    pub(crate) comment: Option<String>,
}

/// A typed data block this vocabulary's documents may carry. Retained for
/// fidelity (not consumed by the DSL-agnostic slice).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct BlockTypeSpec {
    pub(crate) fence_language: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) schema_hint: Option<String>,
}

/// The parsed vocabulary contract — the single source of truth for one RDF
/// vocabulary's namespaces, classes, predicates, and minting rules.
///
/// Fields the DSL-agnostic slice does not yet read (folders, uri_rules, …) are
/// retained so the golden deserializes losslessly and later phases can consume
/// them without re-touching the parse.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VocabularyContract {
    pub(crate) name: String,
    pub(crate) version: String,
    #[allow(dead_code)]
    pub(crate) title: String,
    #[allow(dead_code)]
    pub(crate) description: String,
    pub(crate) namespaces: BTreeMap<String, String>,
    pub(crate) primary_prefix: String,
    #[serde(default)]
    pub(crate) slug_rule: Option<SlugRule>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) uri_rules: BTreeMap<String, String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) doc_id_rules: BTreeMap<String, String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) folders: BTreeMap<String, FolderRule>,
    #[serde(default)]
    pub(crate) classes: BTreeMap<String, ClassSpec>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) wires: BTreeMap<String, WireRule>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) block_types: BTreeMap<String, BlockTypeSpec>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) comanaged_forbidden: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) protocol_subject: Option<String>,
    /// Projection-sink selector for generic projection vocabularies. `None` means
    /// the vocabulary has no generic projection sink of its own; workflow/memory
    /// routing still happens from the request plan and class signatures, not from
    /// this field alone.
    #[serde(default)]
    pub(crate) write_target: Option<String>,
    /// Other vocabularies this contract `owl:imports` (composite Meaningful-Object
    /// vocab). Each entry is a namespace URI (e.g. `http://mnemosyne.dev/memory#`).
    /// The `sophia-agent-core` pack imports `mem:` — build-time A ⊂ runtime B: the
    /// `agt:` contract is the closed declared set A; B is the live membrane graphs
    /// PLUS the imported `mem:`/`mnemo:` records its faculties reference by relation.
    /// STORED-only fidelity in v1 (no engine consumes it yet); kept so the composite
    /// import is first-class on the typed contract.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) imports: Vec<String>,
    /// One-directional, un-ranged property aliases this contract declares
    /// (`<curie> rdfs:subPropertyOf <super>`). Keyed by the L0 predicate CURIE
    /// (e.g. `mem:observedBy`); the value carries the `subPropertyOf` super-property
    /// + a documenting comment. Kept as raw JSON so the alias shape (deliberately
    /// minimal — NO `rdfs:range`, so an alias never re-types stored L0 data by
    /// entailment) is not pinned in Rust. STORED-only fidelity in v1.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) property_aliases: BTreeMap<String, serde_json::Value>,
    /// RAW SHACL shapes (Turtle) the contract carries VERBATIM — the contract-level
    /// `sh:sparql`/`sh:select` passthrough the [`crate::emporium::shacl_sparql`]
    /// evaluator was built for. `vocab_to_shacl` DERIVES structural shapes from the
    /// classes/predicates; this field carries the shapes that CANNOT be derived
    /// (the membrane-scoped, cross-record, cardinality `sh:select` invariants — the
    /// spec §3 agent-ontology shapes). The next step appends these to the derived
    /// shapes so the merged graph drives BOTH faces (rudof structural + the oxigraph
    /// `sh:select` evaluator). `None` ⇒ a contract with only derivable structure.
    #[serde(default)]
    pub(crate) raw_shacl_shapes: Option<String>,
    /// Legal `[from, to]` status transitions. STORED-only in v1 (the lifecycle
    /// guard that consumes them is v2); kept so the memory golden deserializes
    /// losslessly and the field is available to the v2 guard without re-touching
    /// the parse. The `wf:` golden omits it (defaulted).
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) status_transitions: Vec<Vec<String>>,
    /// Declared conflict-resolution Policy instances. STORED-only in v1 (the
    /// `resolve_conflict` executor is v2). Kept as raw JSON values so the v2
    /// resolver can read its own shape without pinning it here.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) conflict_policies: Vec<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) comment: Option<String>,
}

impl VocabularyContract {
    /// Expand a CURIE against the declared namespaces. Full URIs (`://`) and
    /// `urn:` pass through; otherwise split on the first colon and resolve the
    /// prefix. Mirrors `contract.py::_expand_checked`/`expand`.
    pub(crate) fn expand(&self, curie: &str) -> Result<String, String> {
        if curie.contains("://") || curie.starts_with("urn:") {
            return Ok(curie.to_string());
        }
        let (prefix, local) = match curie.split_once(':') {
            Some((p, l)) => (p, l),
            None => ("", ""),
        };
        if local.is_empty() {
            return Err(format!("unknown CURIE prefix in expand(): '{curie}'"));
        }
        match self.namespaces.get(prefix) {
            Some(ns) => Ok(format!("{ns}{local}")),
            None => Err(format!("unknown CURIE prefix in expand(): '{curie}'")),
        }
    }

    /// The namespace URI bound to `primary_prefix`.
    pub(crate) fn primary_namespace(&self) -> &str {
        self.namespaces
            .get(&self.primary_prefix)
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// Every predicate URI this contract can write, expanded. The frozen-vocab
    /// guard input (a minted predicate outside this set is drift).
    #[allow(dead_code)]
    pub(crate) fn known_predicate_uris(&self) -> std::collections::BTreeSet<String> {
        let mut uris = std::collections::BTreeSet::new();
        for cls in self.classes.values() {
            for curie in cls.predicates.keys() {
                if let Ok(uri) = self.expand(curie) {
                    uris.insert(uri);
                }
            }
        }
        uris
    }

    pub(crate) fn materialization_signature(
        &self,
        class_name: &str,
    ) -> Result<MaterializationSignature<'_>, String> {
        let class = self
            .classes
            .get(class_name)
            .ok_or_else(|| format!("{} has no class {class_name}", self.name))?;
        let source_kind = class
            .source_kind
            .as_deref()
            .ok_or_else(|| format!("{}.{} missing source_kind", self.name, class_name))
            .and_then(|value| {
                SourceKind::parse(value).ok_or_else(|| {
                    format!(
                        "{}.{} has invalid source_kind {value:?}",
                        self.name, class_name
                    )
                })
            })?;
        let identity_kind = class
            .identity_kind
            .as_deref()
            .ok_or_else(|| format!("{}.{} missing identity_kind", self.name, class_name))
            .and_then(|value| {
                IdentityKind::parse(value).ok_or_else(|| {
                    format!(
                        "{}.{} has invalid identity_kind {value:?}",
                        self.name, class_name
                    )
                })
            })?;
        let store_mode = class
            .store_mode
            .as_deref()
            .ok_or_else(|| format!("{}.{} missing store_mode", self.name, class_name))
            .and_then(|value| {
                StoreMode::parse(value).ok_or_else(|| {
                    format!(
                        "{}.{} has invalid store_mode {value:?}",
                        self.name, class_name
                    )
                })
            })?;
        let store_target = class
            .store_target
            .as_deref()
            .ok_or_else(|| format!("{}.{} missing store_target", self.name, class_name))?;
        let enforcement = class
            .enforcement
            .as_deref()
            .ok_or_else(|| format!("{}.{} missing enforcement", self.name, class_name))
            .and_then(|value| {
                Enforcement::parse(value).ok_or_else(|| {
                    format!(
                        "{}.{} has invalid enforcement {value:?}",
                        self.name, class_name
                    )
                })
            })?;
        let reconciliation_strategy = class
            .reconciliation_strategy
            .as_deref()
            .ok_or_else(|| {
                format!(
                    "{}.{} missing reconciliation_strategy",
                    self.name, class_name
                )
            })
            .and_then(|value| {
                ReconciliationStrategy::parse(value).ok_or_else(|| {
                    format!(
                        "{}.{} has invalid reconciliation_strategy {value:?}",
                        self.name, class_name
                    )
                })
            })?;
        let dispatch_mode = class
            .dispatch_mode
            .as_deref()
            .ok_or_else(|| format!("{}.{} missing dispatch_mode", self.name, class_name))
            .and_then(|value| {
                DispatchMode::parse(value).ok_or_else(|| {
                    format!(
                        "{}.{} has invalid dispatch_mode {value:?}",
                        self.name, class_name
                    )
                })
            })?;

        if store_mode == StoreMode::Materialize && store_target == "none" {
            return Err(format!(
                "{}.{} materializes but declares store_target=none",
                self.name, class_name
            ));
        }
        if store_mode == StoreMode::Materialize
            && source_kind == SourceKind::Derived
            && dispatch_mode != DispatchMode::DerivedMaterialize
        {
            return Err(format!(
                "{}.{} materializes a derived class but dispatch_mode is not derived-materialize",
                self.name, class_name
            ));
        }
        if store_mode == StoreMode::Virtual {
            if dispatch_mode != DispatchMode::DerivedVirtual {
                return Err(format!(
                    "{}.{} is virtual but dispatch_mode is not derived-virtual",
                    self.name, class_name
                ));
            }
            if source_kind != SourceKind::Derived {
                return Err(format!(
                    "{}.{} is virtual but source_kind is not derived",
                    self.name, class_name
                ));
            }
            if identity_kind != IdentityKind::ResolveByQuery {
                return Err(format!(
                    "{}.{} is virtual but identity_kind is not resolve-by-query",
                    self.name, class_name
                ));
            }
            if class.derived_from_query.as_deref().unwrap_or("").is_empty() {
                return Err(format!(
                    "{}.{} is virtual but missing derived_from_query",
                    self.name, class_name
                ));
            }
        }
        if class.projection_faces.len() > 1
            && class.faces_oracle.as_deref().unwrap_or("").is_empty()
        {
            return Err(format!(
                "{}.{} has multiple projection_faces but no faces_oracle",
                self.name, class_name
            ));
        }

        Ok(MaterializationSignature {
            source_kind,
            identity_kind,
            store_mode,
            store_target,
            enforcement,
            reconciliation_strategy,
            dispatch_mode,
        })
    }
}

/// The parsed workflow contract, deserialized **once** from a clone of the
/// embedded golden bytes. The served body is never produced from this — the
/// router still serves `vocabs::WORKFLOW_GOLDEN_JSON` verbatim.
pub(crate) fn workflow_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(WORKFLOW_GOLDEN_JSON)
            .expect("embedded workflow golden contract must deserialize into VocabularyContract")
    })
}

/// The parsed `sophia-memory-core` contract, deserialized **once** from a clone
/// of the embedded golden bytes ([`vocabs::MEMORY_CORE_GOLDEN_JSON`]). Parallel
/// to [`workflow_vocabulary`]; the served body is never produced from this (the
/// router serves the embedded bytes verbatim). This is the typed form the memory
/// planner ([`crate::emporium::planner::plan_memory_compute`]) mints against.
pub(crate) fn memory_core_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(MEMORY_CORE_GOLDEN_JSON).expect(
            "embedded sophia-memory-core golden contract must deserialize into VocabularyContract",
        )
    })
}

/// The parsed `emporium-graph` SHACL-retrofit contract (EA-2b), deserialized
/// once from the embedded golden bytes. NOT a served pack — the AUTHORED mirror
/// of the code-defined graph-metadata span, consumed by `vocab_to_shacl` to
/// derive the Graph conformance-oracle shapes. Parallel to
/// [`memory_core_vocabulary`].
pub(crate) fn graph_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(GRAPH_GOLDEN_JSON)
            .expect("embedded emporium-graph golden contract must deserialize")
    })
}

/// The parsed `emporium-salience` SHACL-retrofit contract (EA-2b), deserialized
/// once from the embedded golden bytes. NOT a served pack — the AUTHORED mirror
/// of the code-defined `salience_value_triples` span (the single
/// `mnemo:BlockValuation` class), consumed by `vocab_to_shacl`.
pub(crate) fn salience_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(SALIENCE_GOLDEN_JSON)
            .expect("embedded emporium-salience golden contract must deserialize")
    })
}

/// The parsed `emporium-semantic` SHACL-retrofit contract. NOT a served pack —
/// the AUTHORED validation mirror for SEMMA+ `:projection:semantic` records.
pub(crate) fn semantic_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(SEMANTIC_GOLDEN_JSON)
            .expect("embedded emporium-semantic golden contract must deserialize")
    })
}

/// The parsed `emporium-wires` retrofit contract (EA-2b+ structural fork). NOT a
/// served pack — the AUTHORED mirror of the code-defined `wire_subject_triples`
/// span (single `wire:Wire` class; endpoints are IRIs the shapes intentionally do
/// NOT enforce referential integrity over). Consumed ONLY by `vocab_to_shacl`.
pub(crate) fn wires_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(WIRES_GOLDEN_JSON)
            .expect("embedded emporium-wires golden contract must deserialize")
    })
}

/// The parsed `emporium-song` retrofit contract (EA-2b+ structural fork). NOT a
/// served pack — the AUTHORED mirror of the code-defined `song_value_triples` span
/// (the MULTI-CLASS union Song/SongVerse/SongCoda; flat per-class shapes, with the
/// cross-class verseIndex==position invariant SHACL-inexpressible/Lean-proven).
pub(crate) fn song_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(SONG_GOLDEN_JSON)
            .expect("embedded emporium-song golden contract must deserialize")
    })
}

/// The parsed `emporium-document` retrofit contract (EA-2b+ structural fork). NOT a
/// served pack — the AUTHORED mirror of the code-defined `document_tree_triples`
/// span (the recursive node-tree; per-node-type flat shapes, with the recursive
/// `mdoc:childNode` structure SHACL-inexpressible/reconcile-guaranteed).
pub(crate) fn document_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(DOCUMENT_GOLDEN_JSON)
            .expect("embedded emporium-document golden contract must deserialize")
    })
}

/// The parsed `emporium-workspace` retrofit contract (EA-2b+ structural fork). NOT a
/// served pack — the AUTHORED mirror of the code-defined `workspace_entity_triples`
/// span (the 3 mdoc-namespaced entity classes of the 10-class union; flat shapes,
/// with the once-at-seed `rdfs:subClassOf` ontology SHACL-inexpressible/seed-complement).
pub(crate) fn workspace_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(WORKSPACE_GOLDEN_JSON)
            .expect("embedded emporium-workspace golden contract must deserialize")
    })
}

/// The parsed `nomos` omphalos constitution contract. NOT a served pack — the
/// AUTHORED validation contract for the deployment constitution in omphalos.
pub(crate) fn nomos_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(NOMOS_GOLDEN_JSON)
            .expect("embedded nomos golden contract must deserialize")
    })
}

/// Resolve a SERVED vocab's parsed [`VocabularyContract`] BY NAME off the
/// table-driven [`crate::emporium::vocabs::VOCAB_REGISTRY`] (EA-3 / Seq 0). This is
/// the by-name resolver the GENERIC publication path needs: the ingest gate, the
/// generic planner, and the conneg Turtle/SHACL face all hand it a vocab name and
/// receive the typed contract to mint/validate/derive-shapes against — without a
/// per-vocab hardcoded selector (the bottleneck the spine's hardcoded
/// `memory_core_vocabulary()` call was).
///
/// The map is built once (every registered golden parsed into a
/// `BTreeMap<String, VocabularyContract>`) and the contracts live in the `OnceLock`
/// for the process lifetime, so a `&'static` reference into it is sound. Returns
/// `None` for an unregistered name. The parse cannot fail for the embedded goldens
/// (covered by `every_served_vocab_resolves_and_parses`).
pub(crate) fn get_vocabulary(name: &str) -> Option<&'static VocabularyContract> {
    static PARSED: OnceLock<BTreeMap<String, VocabularyContract>> = OnceLock::new();
    let map = PARSED.get_or_init(|| {
        let mut m = BTreeMap::new();
        for (vocab_name, json, _sha) in crate::emporium::vocabs::VOCAB_REGISTRY.iter() {
            let contract: VocabularyContract = serde_json::from_str(json).unwrap_or_else(|e| {
                panic!(
                    "embedded golden '{vocab_name}' must deserialize into VocabularyContract: {e}"
                )
            });
            m.insert((*vocab_name).to_string(), contract);
        }
        m
    });
    let name = crate::emporium::vocabs::canonical_registry_name(name);
    map.get(name)
}

/// WF-4 validation contract for agent-witnessed memory writes. Memory-core remains
/// the structural source for mem: records; the agt raw witness/voice invariants are
/// appended so agent membranes are agt-validated too.
pub(crate) fn agent_memory_validation_vocabulary() -> &'static VocabularyContract {
    static CONTRACT: OnceLock<VocabularyContract> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        let mut merged = memory_core_vocabulary().clone();
        merged.name = "sophia-memory-core+agent-core".to_string();
        merged.title = "Sophia Memory Core + Agent Witness Validation".to_string();
        merged.description =
            "Memory structural validation plus agent witness/voice SHACL invariants for WF-4."
                .to_string();
        let agent_core = get_vocabulary("sophia-agent-core")
            .expect("sophia-agent-core registered before WF-4 merge");
        if !merged
            .imports
            .iter()
            .any(|i| i == "http://mnemosyne.dev/agent#")
        {
            merged
                .imports
                .push("http://mnemosyne.dev/agent#".to_string());
        }
        let mut raw = String::new();
        if let Some(existing) = merged.raw_shacl_shapes.as_deref() {
            raw.push_str(existing);
            if !raw.ends_with('\n') {
                raw.push('\n');
            }
        }
        if let Some(agent_raw) = agent_core.raw_shacl_shapes.as_deref() {
            raw.push_str(agent_raw);
            if !raw.ends_with('\n') {
                raw.push('\n');
            }
        }
        if !raw.is_empty() {
            merged.raw_shacl_shapes = Some(raw);
        }
        merged
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_vocabulary_parses_canonical_workflow_and_page_classes() {
        let c = workflow_vocabulary();
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "ActionArgument",
                "AgentNode",
                "AgentRun",
                "Archetype",
                "AuthoringSession",
                "AuthorizationFlag",
                "CompletenessGap",
                "CompositionEvent",
                "Contract",
                "Draft",
                "DraftWarning",
                "EvidenceArtifact",
                "LiveRecommendedAction",
                "NavigationRoute",
                "Operation",
                "PageTurnDecision",
                "PageView",
                "Parameter",
                "Phase",
                "Protocol",
                "RawSparqlQuery",
                "RecommendedAction",
                "Response",
                "RouteAction",
                "Run",
                "RunStatistics",
                "Server",
                "Variant",
                "Workflow",
                "WorkflowBinding",
            ]
        );
        assert_eq!(c.name, "workflow");
        assert_eq!(c.version, "1.0.0");
        assert_eq!(c.primary_prefix, "wf");
    }

    #[test]
    fn workflow_declares_live_recommendation_as_virtual_mode_two() {
        let c = workflow_vocabulary();
        let live = &c.classes["LiveRecommendedAction"];
        let signature = c
            .materialization_signature("LiveRecommendedAction")
            .expect("virtual recommendation signature is valid");
        assert_eq!(signature.source_kind, SourceKind::Derived);
        assert_eq!(signature.identity_kind, IdentityKind::ResolveByQuery);
        assert_eq!(signature.store_mode, StoreMode::Virtual);
        assert_eq!(signature.store_target, "none");
        assert_eq!(signature.dispatch_mode, DispatchMode::DerivedVirtual);
        assert_eq!(
            live.derived_from_query.as_deref(),
            Some(
                "workflow_book_open.intentRecommendation(current page choices, navigation routes, retained PageView recommendation rows)"
            )
        );
        assert_eq!(live.rdf_types, vec!["wf:LiveRecommendedAction"]);
    }

    #[test]
    fn workflow_declares_authoring_grain_and_virtual_authoring_views() {
        let c = workflow_vocabulary();

        for class_name in ["AuthoringSession", "CompositionEvent"] {
            let class = &c.classes[class_name];
            let signature = c
                .materialization_signature(class_name)
                .unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(signature.source_kind, SourceKind::EventLog);
            assert_eq!(signature.identity_kind, IdentityKind::EventId);
            assert_eq!(signature.store_mode, StoreMode::Materialize);
            assert_eq!(signature.store_target, "user:rdf");
            assert_eq!(signature.dispatch_mode, DispatchMode::EventLogFold);
            assert_eq!(
                class
                    .authoring_grain
                    .as_ref()
                    .and_then(|grain| grain.get("folds_definition_subject"))
                    .and_then(serde_json::Value::as_str),
                Some("wf:Workflow")
            );
            let agent_binding = &class.predicates["wf:boundToAgent"];
            assert_eq!(agent_binding.datatype, Datatype::uri);
            assert!(!agent_binding.required);
        }
        let composition = &c.classes["CompositionEvent"];
        for predicate_name in ["wf:insertTriple", "wf:deleteTriple"] {
            let predicate = &composition.predicates[predicate_name];
            assert_eq!(predicate.datatype, Datatype::string);
            assert!(predicate.multi);
            assert!(!predicate.required);
        }

        for class_name in ["Draft", "CompletenessGap", "DraftWarning", "RunStatistics"] {
            let class = &c.classes[class_name];
            let signature = c
                .materialization_signature(class_name)
                .unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(signature.source_kind, SourceKind::Derived);
            assert_eq!(signature.identity_kind, IdentityKind::ResolveByQuery);
            assert_eq!(signature.store_mode, StoreMode::Virtual);
            assert_eq!(signature.store_target, "none");
            assert_eq!(signature.dispatch_mode, DispatchMode::DerivedVirtual);
            assert!(class.derived_from_query.as_deref().unwrap_or("").len() > 8);
        }

        for class_name in ["Workflow", "Phase", "AgentNode"] {
            let seeded_from = &c.classes[class_name].predicates["wf:seededFrom"];
            assert_eq!(seeded_from.datatype, Datatype::uri);
            assert!(seeded_from.multi);
            assert!(!seeded_from.required);
        }

        let run_statistics = &c.classes["RunStatistics"];
        let definition_subject = &run_statistics.predicates["wf:definitionSubject"];
        assert_eq!(definition_subject.datatype, Datatype::uri);
        assert!(definition_subject.required);
        let branch_frequency = &run_statistics.predicates["wf:branchFrequency"];
        assert_eq!(branch_frequency.datatype, Datatype::string);
        assert!(branch_frequency.multi);
        let median_run_tokens = &run_statistics.predicates["wf:medianRunTokens"];
        assert_eq!(median_run_tokens.datatype, Datatype::integer);
        assert!(!median_run_tokens.multi);
        let latest_run_status = &run_statistics.predicates["wf:latestRunStatus"];
        assert_eq!(latest_run_status.datatype, Datatype::string);
        assert!(!latest_run_status.multi);
        let node_reliability = &run_statistics.predicates["wf:nodeReliability"];
        assert_eq!(node_reliability.datatype, Datatype::string);
        assert!(node_reliability.multi);
        let node_reliability_status = &run_statistics.predicates["wf:nodeReliabilityStatus"];
        assert_eq!(node_reliability_status.datatype, Datatype::string);
        assert!(!node_reliability_status.multi);
    }

    #[test]
    fn every_served_class_has_a_valid_materialization_signature() {
        for (name, _json, _sha) in crate::emporium::vocabs::VOCAB_REGISTRY.iter() {
            let contract = get_vocabulary(name).unwrap_or_else(|| panic!("{name} resolves"));
            for class_name in contract.classes.keys() {
                contract
                    .materialization_signature(class_name)
                    .unwrap_or_else(|error| panic!("{error}"));
            }
        }
    }

    #[test]
    fn internal_semantic_contract_has_derived_materialization_signatures() {
        let c = semantic_vocabulary();
        assert_eq!(c.name, "emporium-semantic");
        for class_name in c.classes.keys() {
            let signature = c
                .materialization_signature(class_name)
                .unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(signature.source_kind, SourceKind::Derived);
            assert_eq!(signature.store_mode, StoreMode::Materialize);
            assert_eq!(signature.store_target, "projection:semantic");
            assert_eq!(signature.enforcement, Enforcement::Halt);
            assert_eq!(
                signature.reconciliation_strategy,
                ReconciliationStrategy::CodeBacked
            );
            assert_eq!(signature.dispatch_mode, DispatchMode::DerivedMaterialize);
        }
    }

    #[test]
    fn internal_nomos_contract_has_omphalos_materialization_signatures() {
        let c = nomos_vocabulary();
        assert_eq!(c.name, "nomos");
        for class_name in c.classes.keys() {
            let signature = c
                .materialization_signature(class_name)
                .unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(signature.source_kind, SourceKind::CurrentState);
            assert_eq!(signature.store_mode, StoreMode::Materialize);
            assert_eq!(signature.store_target, "omphalos:default");
            assert_eq!(signature.enforcement, Enforcement::Halt);
            assert_eq!(
                signature.reconciliation_strategy,
                ReconciliationStrategy::CodeBacked
            );
            assert_eq!(
                signature.dispatch_mode,
                DispatchMode::CurrentStateMaterialize
            );
        }
        assert_eq!(
            c.materialization_signature("World")
                .expect("World signature")
                .identity_kind,
            IdentityKind::LogicalId
        );
    }

    #[test]
    fn workflow_page_view_names_its_faces_oracle() {
        let c = workflow_vocabulary();
        let page_view = &c.classes["PageView"];
        assert_eq!(
            page_view.faces_oracle.as_deref(),
            Some("workflow_book_page_view_faces_agree")
        );
        c.materialization_signature("PageView")
            .expect("PageView multi-face signature is valid");
    }

    #[test]
    fn memory_core_vocabulary_parses_five_classes() {
        let c = memory_core_vocabulary();
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "Claim",
                "EvidenceLink",
                "MemoryRecord",
                "Policy",
                "SourceReference",
            ]
        );
        assert_eq!(c.name, "sophia-memory-core");
        assert_eq!(c.version, "1.3.0");
        assert_eq!(c.primary_prefix, "mem");
        assert_eq!(c.primary_namespace(), "http://mnemosyne.dev/memory#");
        // Memory declares its authoritative projection sink.
        assert_eq!(c.write_target.as_deref(), Some("projection:memory"));
        // STORED-only lifecycle data deserializes (8 transitions, 1 policy).
        assert_eq!(c.status_transitions.len(), 8);
        assert_eq!(c.conflict_policies.len(), 1);
        // I1 provenance gate is a required predicate on MemoryRecord.
        assert!(c.classes["MemoryRecord"].predicates["mem:derivedFrom"].required);
        // The wf golden still has no write_target (defaults None).
        assert!(workflow_vocabulary().write_target.is_none());
    }

    #[test]
    fn agent_memory_validation_contract_merges_memory_and_agent_shapes() {
        let c = agent_memory_validation_vocabulary();
        assert_eq!(c.name, "sophia-memory-core+agent-core");
        assert!(c.classes.contains_key("MemoryRecord"));
        assert_eq!(c.write_target.as_deref(), Some("projection:memory"));
        assert!(c.imports.iter().any(|i| i == "http://mnemosyne.dev/agent#"));

        let shapes = c
            .raw_shacl_shapes
            .as_deref()
            .expect("agent raw shapes appended");
        let constraints = crate::emporium::shacl_sparql::extract_sparql_constraints(shapes)
            .expect("merged raw shapes extract");
        assert!(
            constraints
                .iter()
                .any(|c| c.shape == "http://mnemosyne.dev/agent#I1_MembraneWitnessShape"),
            "WF-4 merged contract exposes the agt I1 membrane witness shape"
        );
    }

    // ── EA-3 by-name resolver + the example bookmark pack ──

    #[test]
    fn every_served_vocab_resolves_and_parses() {
        // Every name in the served registry resolves to a parsed contract whose
        // own name agrees (the by-name resolver is consistent with the table).
        for (name, _json, _sha) in crate::emporium::vocabs::VOCAB_REGISTRY.iter() {
            let c = get_vocabulary(name).unwrap_or_else(|| panic!("{name} resolves"));
            assert_eq!(&c.name.as_str(), name);
        }
        assert_eq!(
            get_vocabulary("agent").map(|contract| contract.name.as_str()),
            Some("sophia-agent-core")
        );
        assert_eq!(
            get_vocabulary("memory").map(|contract| contract.name.as_str()),
            Some("sophia-memory-core")
        );
        // An unregistered name resolves to None.
        assert!(get_vocabulary("not-a-real-vocab").is_none());
    }

    // ── CA-1 sophia-agent-core agent-ontology pack (PROVE: register + serve + A⊂B) ──

    /// The pack loads via the REAL parse path (`get_vocabulary`), registers + is
    /// served, declares the 5 FROZEN classes verbatim + the witness/voice/voicing/
    /// driver/faculty glue, carries the composite import of `mem:`, the one-directional
    /// un-ranged alias, and the per-class write conventions. No mock — the same
    /// `OnceLock` parse every served pack rides.
    #[test]
    fn agent_core_pack_registers_and_parses_natively() {
        // (1) parses via the real by-name resolver …
        let c = get_vocabulary("sophia-agent-core").expect("sophia-agent-core registered");
        assert_eq!(c.name, "sophia-agent-core");
        assert_eq!(c.version, "1.1.0");
        assert_eq!(c.primary_prefix, "agt");
        assert_eq!(c.primary_namespace(), "http://mnemosyne.dev/agent#");
        // (2) … and is SERVED (in the thin catalog + by-name find) ……
        assert!(crate::emporium::vocabs::find_contract("sophia-agent-core", "latest").is_some());
        assert!(crate::emporium::vocabs::all_contracts()
            .iter()
            .any(|v| v.name == "sophia-agent-core"));

        // The 5 FROZEN classes are present verbatim (BTreeMap order).
        for frozen in ["Agent", "Capability", "Session", "Tool", "Turn"] {
            assert!(
                c.classes.contains_key(frozen),
                "frozen class {frozen} present"
            );
        }
        // §WS1 relevel (B1): the publication pack now also carries the agt:Run
        // class (Agent ⊃ Session ⊃ Run ⊃ Turn), the Turn nests under a Run
        // (agt:inRun, required), and the Session pins the inference model.
        let run = &c.classes["Run"];
        assert_eq!(run.rdf_types, vec!["agt:Run"]);
        assert!(run.predicates["agt:realizedBy"].required);
        assert!(run.predicates["agt:ofSession"].required);
        // Publication face mirrors the user:rdf serializer (inferenceProvider/Model).
        assert_eq!(
            run.predicates["agt:inferenceProvider"].datatype,
            Datatype::string
        );
        assert!(c.classes["Turn"].predicates["agt:inRun"].required);
        assert_eq!(
            c.classes["Session"].predicates["agt:model"].datatype,
            Datatype::string
        );
        // The net-new witness/voice/voicing/driver/faculty glue is present too.
        for glue in [
            "Voice",
            "Driver",
            "Voicing",
            "Witness",
            "Faculty",
            "ReadOperation",
            "PoolingMode",
            "FacultyPooling",
        ] {
            assert!(c.classes.contains_key(glue), "glue class {glue} present");
        }

        // Agent is dual-typed agt:Agent + prov:Agent (the witness); agentId required.
        let agent = &c.classes["Agent"];
        assert_eq!(agent.rdf_types, vec!["agt:Agent", "prov:Agent"]);
        assert!(agent.predicates["agt:agentId"].required);
        // Voice carries the leaf join-key + borrow edge, both required (I2/I4a).
        let voice = &c.classes["Voice"];
        assert!(voice.predicates["agt:voiceId"].required);
        assert!(voice.predicates["agt:voiceOf"].required);
        // ownsMembrane lives ONLY on Agent (the secession hinge) — NOT on Voice.
        assert!(agent.predicates.contains_key("agt:ownsMembrane"));
        assert!(!voice.predicates.contains_key("agt:ownsMembrane"));
        // Faculty's bindsRecordClass is a uri (rdfs:Class), MULTI (closure check
        // is well-defined — must-fix #7).
        let faculty = &c.classes["Faculty"];
        assert_eq!(
            faculty.predicates["agt:bindsRecordClass"].datatype,
            Datatype::uri
        );
        assert!(faculty.predicates["agt:bindsRecordClass"].multi);
        // agt:realizedBy is the derived inverse reading of Workflow's canonical
        // wf:boundToAgent edge. Agent core owns the predicate declaration, while
        // the workflow-session projection can still require it for its product.
        assert_eq!(
            c.classes["Session"].predicates["agt:realizedBy"].datatype,
            Datatype::uri
        );
        assert!(!c.classes["Session"].predicates["agt:realizedBy"].required);
        assert_eq!(
            c.classes["Turn"].predicates["agt:realizedBy"].datatype,
            Datatype::uri
        );
        assert!(!c.classes["Turn"].predicates["agt:realizedBy"].required);

        // The composite import of mem: is first-class on the typed contract.
        assert!(c
            .imports
            .iter()
            .any(|i| i == "http://mnemosyne.dev/memory#"));
        // The one-directional, un-ranged alias is declared and reads UP to agt:witnessedBy.
        let alias = c
            .property_aliases
            .get("mem:observedBy")
            .expect("observedBy alias declared");
        assert_eq!(
            alias.get("subPropertyOf").and_then(|v| v.as_str()),
            Some("agt:witnessedBy")
        );
        // A vocab-PUBLICATION pack: no write_target (agt: instances ride user:rdf;
        // membrane/leaf testimony stays the L0 mem:/mnemo: materializers' job).
        assert!(c.write_target.is_none());
        // The enum/glue classes carry NO rdf_types (registry-only — no structural
        // shape; the teeth are the §3 sh:select shapes).
        assert!(c.classes["Voicing"].rdf_types.is_empty());
        assert!(c.classes["Witness"].rdf_types.is_empty());
    }

    /// PROVE (no-mock): the pack carries the spec §3 SHACL shapes VERBATIM on the
    /// contract (the `raw_shacl_shapes` passthrough), and they REALLY parse +
    /// extract through the live oxigraph SHACL-SPARQL evaluator — the membrane-
    /// scoped `sh:select` invariants (I1/I1b/I2/I2b/I3-cardinality/I4b/identity)
    /// are recovered as runnable constraints, not inert text.
    #[test]
    fn agent_core_raw_shapes_parse_and_extract_through_the_real_evaluator() {
        let c = get_vocabulary("sophia-agent-core").expect("registered");
        let shapes = c
            .raw_shacl_shapes
            .as_deref()
            .expect("sophia-agent-core carries raw §3 SHACL shapes");
        // The shapes are real Turtle that loads into a real oxigraph Store, and the
        // sh:select constraints are extracted (no mock — the same path the validator
        // merge uses).
        let constraints = crate::emporium::shacl_sparql::extract_sparql_constraints(shapes)
            .expect("the §3 sh:sparql shapes parse + extract through the real evaluator");
        // The membrane/cardinality/identity sh:select invariants are present.
        let shape_iris: std::collections::BTreeSet<&str> =
            constraints.iter().map(|c| c.shape.as_str()).collect();
        for inv in [
            "http://mnemosyne.dev/agent#I1_MembraneWitnessShape",
            "http://mnemosyne.dev/agent#I1b_MembraneOwnerShape",
            "http://mnemosyne.dev/agent#I2_VoiceContainmentShape",
            "http://mnemosyne.dev/agent#I2b_VoiceNeedsWitnessShape",
            "http://mnemosyne.dev/agent#I3_CardinalityShape",
            "http://mnemosyne.dev/agent#I4b_SharedObserverShape",
            "http://mnemosyne.dev/agent#IdentityUnificationShape",
        ] {
            assert!(
                shape_iris.contains(inv),
                "§3 sh:select shape {inv} extracted"
            );
        }
        // The advisories carry sh:Warning severity (honest about the salience gap +
        // the monovocal-elision convention) — proving severity round-trips.
        assert!(
            constraints.iter().any(|c| c.severity == "Warning"),
            "the §3 advisories (I1-Valuation / I3d) extract as sh:Warning"
        );
    }

    /// PROVE A⊂B closure (no-mock): build the spec §2 worked instance (a Parliament
    /// agent, two voices, a witnessed+leaf-tagged membrane record, and the contrast
    /// commons record) as REAL triples in a REAL oxigraph Store, then confirm EVERY
    /// primary-namespace (`agt:`) class AND predicate the contract declares is
    /// PROJECTABLE from that live membrane — the EA-3 publication invariant.
    #[test]
    fn agent_core_declared_terms_are_projectable_from_a_live_membrane() {
        use oxigraph::sparql::{QueryResults, SparqlEvaluator};
        use oxigraph::store::Store;

        // The real evaluator path (this oxigraph builds queries via SparqlEvaluator,
        // not Store::query) — an ASK over the loaded store, no mock.
        let ask = |store: &Store, query: &str| -> bool {
            let prepared = SparqlEvaluator::new()
                .parse_query(query)
                .expect("ASK parses");
            match prepared.on_store(store).execute().expect("ASK executes") {
                QueryResults::Boolean(b) => b,
                _ => panic!("ASK must return a boolean"),
            }
        };

        let c = get_vocabulary("sophia-agent-core").expect("registered");
        let agt = c.primary_namespace(); // http://mnemosyne.dev/agent#

        // The §2 worked instance (Parliament, two voices, mixed pooling) materialized
        // as a real RDF graph — every agt: class + predicate the contract declares is
        // asserted on a live subject so the closure SPARQL can project it back.
        let agent = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b";
        let membrane =
            "urn:mnemosyne:local:graph:lab:projection:memory:agent:agent-1a2b3c4d5e6f7a8b";
        let ttl = format!(
            r#"@prefix agt: <{agt}> .
@prefix mem: <http://mnemosyne.dev/memory#> .
@prefix mnemo: <https://mnemosyne.local/ns#> .
@prefix prov: <http://www.w3.org/ns/prov#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix wf: <http://mnemosyne.dev/workflow#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

<{agent}> a agt:Agent, prov:Agent ;
    agt:agentId "agent-1a2b3c4d5e6f7a8b" ;
    rdfs:label "the archivist-critic" ;
    agt:model "deepseek-v4-flash" ;
    agt:voicing agt:Parliament ;
    agt:ownsMembrane <{membrane}> ;
    agt:hasFaculty agt:Song, agt:Memory, agt:Valuation ;
    agt:hasVoice <{agent}#voice:archivist>, <{agent}#voice:critic> .

<{agent}#voice:archivist> a agt:Voice, agt:Witness ; agt:voiceOf <{agent}> ; agt:voiceId "archivist" .
<{agent}#voice:critic>    a agt:Voice, agt:Witness ; agt:voiceOf <{agent}> ; agt:voiceId "critic" .

<urn:sophia:driver:vera> a agt:Driver, prov:Agent ; agt:driverId "vera" .

<urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s1> a agt:Session ;
    agt:sessionId "s1" ; agt:ofAgent <{agent}> ; agt:ownerUserId "u1" ;
    agt:turnCount 3 ; agt:graphId "lab" ;
    agt:model "deepseek-v4-flash" ;
    agt:inferenceProvider "deepseek" ; agt:inferenceModel "deepseek-v4-flash" ;
    agt:systemPromptDocument <urn:mnemosyne:doc:sys-1> ;
    agt:systemPromptDocumentId "sys-1" ; agt:systemPromptSnapshot "snap-1" ;
    agt:systemPromptDigest "digest-1" ; agt:systemPromptBinding "binding-1" ;
    agt:systemPromptIdentityAlgorithm "agent-identity-v1-registered-name" ;
    agt:systemPromptDirty false ; agt:systemPromptRevision 2 ;
    agt:systemPromptChangeId "chg-1" ; agt:systemPromptCurrentSnapshot "snap-2" ;
    agt:systemPromptCurrentDigest "digest-2" ; agt:systemPromptCompatibilitySeeded true ;
    agt:systemPromptBlock <urn:mnemosyne:block:b1> ;
    agt:realizedBy <urn:sophia:wf-run:wfr-fold> .

<urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:turn:t1> a agt:Turn ;
    agt:inSession <urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s1> ;
    agt:inRun <urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s1:run:wfr-fold> ;
    agt:ordinal 1 ; agt:role "assistant" ;
    agt:usedTool <urn:sophia:tool:remember> ;
    agt:turnTime "2026-06-23T00:00:00Z"^^xsd:dateTime ;
    agt:recallRef <urn:mnemosyne:recall:1> ; agt:songRef <urn:mnemosyne:song:1> ;
    agt:drivenBy <urn:sophia:driver:vera> ;
    agt:realizedBy <urn:sophia:wf-run:wfr-fold:agent:node-1> .

<urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s1:run:wfr-fold> a agt:Run ;
    agt:ofAgent <{agent}> ;
    agt:ofSession <urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s1> ;
    agt:realizedBy <urn:sophia:wf-run:wfr-fold> ;
    agt:runId "wfr-fold" ; agt:ownerUserId "u1" ; agt:graphId "lab" ;
    agt:turnCount 3 ;
    agt:inferenceProvider "deepseek" ; agt:inferenceModel "deepseek-v4-flash" ;
    agt:supersedesLegacyRunSession <urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:wfr-fold> .

<urn:sophia:wf-run:wfr-fold> a wf:Run ;
    wf:boundToAgent <urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:s1:run:wfr-fold> .
<urn:sophia:wf-run:wfr-fold:agent:node-1> a wf:AgentRun ;
    wf:boundToAgent <urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:turn:t1> .

<urn:sophia:tool:remember> a agt:Tool ; agt:ofAgent <{agent}> ;
    agt:toolName "remember" ; agt:toolStatus "used" .
<urn:sophia:cap:write> a agt:Capability ; agt:ofAgent <{agent}> ; agt:toolAccess "write" .

agt:Memory a agt:Faculty ;
    agt:bindsRecordClass mem:MemoryRecord ;
    agt:membranePattern "urn:mnemosyne:local:graph:{{graphId}}:projection:memory:agent:{{id}}" ;
    agt:witnessScoped true .

agt:Attunement a agt:ReadOperation ; agt:readsAcross agt:Song, agt:Memory, agt:Valuation .

[] a agt:FacultyPooling ; agt:poolsFaculty agt:Memory ;
    agt:poolingVoice <{agent}#voice:archivist> ; agt:poolingMode agt:Pooled .
"#
        );

        let store = Store::new().expect("oxigraph store");
        store
            .load_from_slice(oxigraph::io::RdfFormat::Turtle, ttl.as_bytes())
            .expect("the §2 worked instance is real, valid Turtle");

        // CLOSURE over CLASSES: every class with a primary-namespace rdf_type the
        // contract declares MUST have at least one instance projectable from the
        // live graph (A ⊂ B — A's class is realized in B).
        for (class_name, class) in &c.classes {
            let targets: Vec<String> = class
                .rdf_types
                .iter()
                .filter_map(|t| c.expand(t).ok())
                .filter(|u| u.starts_with(agt))
                .collect();
            if targets.is_empty() {
                continue; // registry-only glue (Voicing/Witness/…): no instance to project.
            }
            for type_uri in targets {
                let present = ask(&store, &format!("ASK {{ ?s a <{type_uri}> }}"));
                assert!(
                    present,
                    "class {class_name} ({type_uri}) is projectable from the membrane"
                );
            }
        }

        // CLOSURE over PREDICATES: every declared predicate URI the contract can
        // write MUST be projectable from the live graph (every term in A appears in B).
        for uri in c.known_predicate_uris() {
            // Only assert closure over the contract's OWN agt: terms (the imported
            // mem:/prov:/rdfs: predicates are B's, referenced by relation, not A's
            // closed set to round-trip here).
            if !uri.starts_with(agt) {
                continue;
            }
            let present = ask(&store, &format!("ASK {{ ?s <{uri}> ?o }}"));
            assert!(
                present,
                "predicate {uri} is projectable from the live membrane (A ⊂ B)"
            );
        }
    }

    /// TEETH (no-mock): the §3 shapes carried on the contract REALLY catch a
    /// malformed instance through the live oxigraph `sh:select` evaluator. A record
    /// inside a per-observer membrane graph WITHOUT a `mem:observedBy` witness MUST
    /// trip I1 (the firm-boundary invariant); a legal commons record (un-segmented
    /// graph, no witness) MUST pass (the must-fix #1 regression guard).
    #[test]
    fn agent_core_shapes_catch_a_witnessless_membrane_record_but_pass_the_commons() {
        let c = get_vocabulary("sophia-agent-core").expect("registered");
        let shapes = c.raw_shacl_shapes.as_deref().expect("raw shapes present");

        // (a) MALFORMED: a MemoryRecord inside a membrane graph with NO mem:observedBy.
        let membrane = "urn:mnemosyne:local:graph:lab:projection:memory:agent:agent-deadbeef";
        let bad = "<urn:rec:bad> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> \
                   <http://mnemosyne.dev/memory#MemoryRecord> .\n";
        let violations =
            crate::emporium::shacl_sparql::evaluate_sparql_constraints(shapes, bad, membrane)
                .expect("evaluator runs");
        assert!(
            violations.iter().any(|v| v.shape.as_deref()
                == Some("http://mnemosyne.dev/agent#I1_MembraneWitnessShape")),
            "I1 MUST catch a witnessless record inside a per-observer membrane (teeth). got: {:?}",
            violations
                .iter()
                .map(|v| v.shape.clone())
                .collect::<Vec<_>>()
        );

        // (b) LEGAL COMMONS: the SAME witnessless record in the UN-segmented commons
        // graph MUST pass I1 (observedBy is optional at L0 — must-fix #1 guard).
        let commons = "urn:mnemosyne:local:graph:lab:projection:memory";
        let commons_violations =
            crate::emporium::shacl_sparql::evaluate_sparql_constraints(shapes, bad, commons)
                .expect("evaluator runs");
        assert!(
            !commons_violations.iter().any(|v| v.shape.as_deref()
                == Some("http://mnemosyne.dev/agent#I1_MembraneWitnessShape")),
            "a commons record (un-segmented graph, no witness) MUST NOT trip I1"
        );
    }

    #[test]
    fn bookmark_vocabulary_parses_one_class_with_a_template_subject_rule() {
        let c = get_vocabulary("emporium-bookmark").expect("bookmark registered");
        assert_eq!(c.name, "emporium-bookmark");
        assert_eq!(c.version, "1.0.0");
        assert_eq!(c.primary_prefix, "bm");
        assert_eq!(c.primary_namespace(), "http://mnemosyne.dev/bookmark#");
        // Bookmark declares its generic projection sink.
        assert_eq!(c.write_target.as_deref(), Some("projection:bookmark"));
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["Bookmark"]);
        let bm = &c.classes["Bookmark"];
        assert_eq!(bm.rdf_types, vec!["bm:Bookmark"]);
        // url + title are required; note/tag/createdAt optional; tag is multi.
        assert!(bm.predicates["bm:url"].required);
        assert!(bm.predicates["bm:title"].required);
        assert!(!bm.predicates["bm:note"].required);
        assert!(bm.predicates["bm:tag"].multi);
        // The Template subject_rule is present (B5 parses it elsewhere).
        assert_eq!(
            bm.subject_rule.as_deref(),
            Some("{graph_subject}:projection:bookmark:bookmark:{localId}")
        );
    }

    #[test]
    fn wf_agent_binding_pack_registers_without_a_write_target() {
        let c = get_vocabulary("wf-agent-binding").expect("wf-agent-binding registered");
        assert_eq!(c.name, "wf-agent-binding");
        assert_eq!(c.primary_prefix, "wfagt");
        assert_eq!(
            c.primary_namespace(),
            "http://mnemosyne.dev/workflow-agent-binding#"
        );
        assert!(
            c.write_target.is_none(),
            "WF-2 binding pack is publication-only"
        );
        assert!(c
            .imports
            .iter()
            .any(|i| i == "http://mnemosyne.dev/workflow#"));
        assert!(c.classes["RunBinding"]
            .predicates
            .contains_key("wf:boundToAgent"));
        assert!(c.classes["AgentRunBinding"]
            .predicates
            .contains_key("wf:boundToAgent"));
        assert!(c.classes["SessionRealization"]
            .predicates
            .contains_key("agt:realizedBy"));

        let shapes = c
            .raw_shacl_shapes
            .as_deref()
            .expect("binding pack carries raw shapes");
        let constraints = crate::emporium::shacl_sparql::extract_sparql_constraints(shapes)
            .expect("binding raw shapes parse through the evaluator");
        let shape_iris: std::collections::BTreeSet<&str> =
            constraints.iter().map(|c| c.shape.as_str()).collect();
        assert!(shape_iris
            .contains("http://mnemosyne.dev/workflow-agent-binding#RunBoundToSessionShape"));
        assert!(shape_iris
            .contains("http://mnemosyne.dev/workflow-agent-binding#AgentRunBoundToTurnShape"));
    }

    #[test]
    fn workflow_core_owns_agent_binding_predicate_and_shapes() {
        let c = workflow_vocabulary();
        assert!(c
            .namespaces
            .get("agt")
            .is_some_and(|ns| ns == "http://mnemosyne.dev/agent#"));

        let run_binding = &c.classes["Run"].predicates["wf:boundToAgent"];
        assert_eq!(run_binding.datatype, Datatype::uri);
        assert!(!run_binding.required);
        let agent_run_binding = &c.classes["AgentRun"].predicates["wf:boundToAgent"];
        assert_eq!(agent_run_binding.datatype, Datatype::uri);
        assert!(!agent_run_binding.required);
        assert!(c
            .known_predicate_uris()
            .contains("http://mnemosyne.dev/workflow#boundToAgent"));

        let shapes = c
            .raw_shacl_shapes
            .as_deref()
            .expect("workflow core carries binding raw shapes");
        let constraints = crate::emporium::shacl_sparql::extract_sparql_constraints(shapes)
            .expect("workflow raw shapes parse through the evaluator");
        let shape_iris: std::collections::BTreeSet<&str> =
            constraints.iter().map(|c| c.shape.as_str()).collect();
        assert!(shape_iris.contains("http://mnemosyne.dev/workflow#RunBoundToSessionShape"));
        assert!(shape_iris.contains("http://mnemosyne.dev/workflow#AgentRunBoundToTurnShape"));
    }

    #[test]
    fn wf_agent_session_projection_pack_registers_with_session_write_target() {
        let c = get_vocabulary("wf-agent-session-projection")
            .expect("wf-agent-session-projection registered");
        assert_eq!(c.name, "wf-agent-session-projection");
        assert_eq!(c.primary_prefix, "agt");
        assert_eq!(c.write_target.as_deref(), Some("projection:session"));
        for class in [
            "Agent",
            "Voice",
            "Session",
            "Run",
            "Turn",
            "Tool",
            "WorkflowRunAnchor",
            "WorkflowAgentRunAnchor",
        ] {
            assert!(c.classes.contains_key(class), "{class} class present");
        }
        assert!(c.classes["WorkflowRunAnchor"]
            .rdf_types
            .contains(&"wf:Run".to_string()));
        assert!(c.classes["WorkflowAgentRunAnchor"]
            .rdf_types
            .contains(&"wf:AgentRun".to_string()));
        assert!(c.classes["Turn"].predicates["agt:turnGrain"].required);
        // §WS1 relevel (B2): the breaker is fixed — the run-grain Session no longer
        // carries (much less requires) agt:realizedBy; that edge moved to agt:Run.
        assert!(!c.classes["Session"]
            .predicates
            .contains_key("agt:realizedBy"));
        assert!(c.classes["Session"].predicates["agt:ownerUserId"].required);
        assert!(c.classes["Run"].predicates["agt:realizedBy"].required);
        assert!(c.classes["Run"].predicates["agt:ofSession"].required);
        // The gated write face uses the generic-record field names agt:provider /
        // agt:model (NOT the serializer's agt:inferenceProvider/Model), so the
        // no-silent-drop planner guard accepts the real generic records.
        assert_eq!(
            c.classes["Run"].predicates["agt:provider"].datatype,
            Datatype::string
        );
        assert_eq!(
            c.classes["Run"].predicates["agt:model"].datatype,
            Datatype::string
        );
        // The node-grain Turn now nests under its Run (agt:inRun, required).
        assert!(c.classes["Turn"].predicates["agt:inRun"].required);
        assert!(c.classes["Turn"].predicates["agt:realizedBy"].required);
    }

    #[test]
    fn workflow_ui_pack_registers_route_and_decision_terms() {
        let c = get_vocabulary("workflow-ui").expect("workflow-ui registered");
        assert_eq!(c.name, "workflow-ui");
        assert_eq!(c.primary_prefix, "wfui");
        assert_eq!(
            c.primary_namespace(),
            crate::emporium::vocabs::WORKFLOW_UI_NS
        );
        assert!(
            c.write_target.is_none(),
            "workflow-ui is publication-only; retained decisions currently load through user RDF"
        );

        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "AuthorizationFlag",
                "EvidenceArtifact",
                "NavigationRoute",
                "PageTurnDecision",
                "WorkflowAdventurePacket",
            ]
        );

        let decision = &c.classes["PageTurnDecision"];
        assert_eq!(
            decision.rdf_types,
            vec!["wfui:PageTurnDecision", "prov:Entity"]
        );
        assert!(decision.predicates["wfui:schemaVersion"].required);
        assert!(decision.predicates["wfui:intent"].required);
        assert!(decision.predicates["wfui:executionAuthorized"].required);
        assert_eq!(
            decision.predicates["wfui:recommendedRouteId"].datatype,
            Datatype::string
        );
        assert_eq!(
            decision.predicates["wfui:followedRouteId"].datatype,
            Datatype::string
        );
        assert_eq!(
            decision.predicates["wfui:followedChoiceId"].datatype,
            Datatype::string
        );
        assert_eq!(
            decision.predicates["wfui:hasAuthorizationFlag"].datatype,
            Datatype::uri
        );
        assert_eq!(
            decision.predicates["wfui:hasNavigationRoute"].datatype,
            Datatype::uri
        );
        assert!(decision.predicates["wfui:hasNavigationRoute"].multi);
        assert!(decision.predicates["prov:wasDerivedFrom"].multi);

        let route = &c.classes["NavigationRoute"];
        assert!(route.predicates["wfui:routeId"].required);
        assert!(route.predicates["wfui:choiceId"].required);
        assert_eq!(
            route.predicates["wfui:actionJson"].datatype,
            Datatype::string
        );

        assert!(c.classes["EvidenceArtifact"].predicates["wfui:path"].required);
        assert!(c.classes["AuthorizationFlag"].predicates["wfui:pageId"].required);
        let adventure = &c.classes["WorkflowAdventurePacket"];
        assert_eq!(
            adventure.rdf_types,
            vec!["wfui:WorkflowAdventurePacket", "prov:Entity"]
        );
        assert!(adventure.predicates["wfui:pageId"].required);
        assert!(adventure.predicates["wfui:pageTitle"].required);
        assert!(adventure.predicates["wfui:rawSparqlJson"].required);
        assert_eq!(
            adventure.predicates["wfui:hasNavigationRoute"].datatype,
            Datatype::uri
        );
        assert!(adventure.predicates["wfui:hasNavigationRoute"].multi);
        assert!(c.known_predicate_uris().contains(&format!(
            "{}recommendedRouteId",
            crate::emporium::vocabs::WORKFLOW_UI_NS
        )));
        assert!(c.known_predicate_uris().contains(&format!(
            "{}followedRouteId",
            crate::emporium::vocabs::WORKFLOW_UI_NS
        )));
    }

    #[test]
    fn wf_agent_session_projection_shapes_accept_and_reject_real_fixtures() {
        use oxigraph::model::{Literal, NamedNode};

        use crate::emporium::shacl_emit::vocab_to_shacl;
        use crate::emporium::shacl_validator::validate_against_shapes;
        use crate::emporium::terms::{Term, Triple};

        const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
        const AGT: &str = "http://mnemosyne.dev/agent#";
        const WF: &str = "http://mnemosyne.dev/workflow#";
        const PROV_AGENT: &str = "http://www.w3.org/ns/prov#Agent";
        const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

        fn uri(s: &str) -> Term {
            Term::Uri(NamedNode::new(s).expect("valid IRI"))
        }
        fn lit(s: &str) -> Term {
            Term::Lit(Literal::new_simple_literal(s))
        }
        fn int(value: i64) -> Term {
            Term::Lit(Literal::new_typed_literal(
                value.to_string(),
                NamedNode::new(format!("{XSD}integer")).unwrap(),
            ))
        }
        fn triple(s: &str, p: &str, o: Term) -> Triple {
            (s.to_string(), p.to_string(), o)
        }

        let contract = get_vocabulary("wf-agent-session-projection").unwrap();
        let shapes = vocab_to_shacl(contract);
        let sink = "urn:mnemosyne:local:graph:lab:projection:session";
        let agent = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b";
        let voice = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:voice:monovocal";
        let session = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:sess-fold";
        // §WS1 relevel: the run is a distinct agt:Run nested under the session, and
        // the turn nests under the run (…:session:{id}:run:{runId}:turn:{ordinal}).
        let run_episode = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:sess-fold:run:wfr-fold";
        let turn = "urn:sophia:agent:agent-1a2b3c4d5e6f7a8b:session:sess-fold:run:wfr-fold:turn:1";
        let wf_run = "urn:sophia:wf:run:wfr-fold";
        let wf_agent_run = "urn:sophia:wf:agent-run:wfr-fold:1";

        let good = vec![
            triple(agent, RDF_TYPE, uri(&format!("{AGT}Agent"))),
            triple(agent, RDF_TYPE, uri(PROV_AGENT)),
            triple(
                agent,
                &format!("{AGT}agentId"),
                lit("agent-1a2b3c4d5e6f7a8b"),
            ),
            triple(agent, &format!("{AGT}model"), lit("cheap")),
            triple(
                agent,
                &format!("{AGT}voicing"),
                uri(&format!("{AGT}Monovocal")),
            ),
            triple(agent, &format!("{AGT}hasVoice"), uri(voice)),
            triple(voice, RDF_TYPE, uri(&format!("{AGT}Voice"))),
            triple(voice, &format!("{AGT}voiceOf"), uri(agent)),
            triple(voice, &format!("{AGT}voiceId"), lit("monovocal")),
            triple(wf_run, RDF_TYPE, uri(&format!("{WF}Run"))),
            triple(wf_agent_run, RDF_TYPE, uri(&format!("{WF}AgentRun"))),
            // §WS1: the run-grain Session no longer carries agt:realizedBy (breaker).
            triple(session, RDF_TYPE, uri(&format!("{AGT}Session"))),
            triple(session, &format!("{AGT}ofAgent"), uri(agent)),
            triple(session, &format!("{AGT}sessionId"), lit("sess-fold")),
            triple(session, &format!("{AGT}ownerUserId"), lit("owner-1")),
            triple(session, &format!("{AGT}turnCount"), int(1)),
            // the agt:Run carries the realization edge (→ the wf:Run anchor).
            triple(run_episode, RDF_TYPE, uri(&format!("{AGT}Run"))),
            triple(run_episode, &format!("{AGT}ofAgent"), uri(agent)),
            triple(run_episode, &format!("{AGT}ofSession"), uri(session)),
            triple(run_episode, &format!("{AGT}realizedBy"), uri(wf_run)),
            triple(run_episode, &format!("{AGT}runId"), lit("wfr-fold")),
            triple(run_episode, &format!("{AGT}ownerUserId"), lit("owner-1")),
            triple(run_episode, &format!("{AGT}graphId"), lit("lab")),
            triple(run_episode, &format!("{AGT}turnCount"), int(1)),
            triple(turn, RDF_TYPE, uri(&format!("{AGT}Turn"))),
            triple(turn, &format!("{AGT}inSession"), uri(session)),
            triple(turn, &format!("{AGT}inRun"), uri(run_episode)),
            triple(turn, &format!("{AGT}ordinal"), int(1)),
            triple(turn, &format!("{AGT}turnGrain"), lit("node")),
            triple(turn, &format!("{AGT}realizedBy"), uri(wf_agent_run)),
        ];
        validate_against_shapes(&shapes, &good, sink, contract.name.as_str())
            .expect("well-formed WS-7 relevel Agent/Session/Run/Turn projection conforms");

        // (1) a missing agt:turnGrain still violates RunTurnGrainShape.
        let missing_grain: Vec<Triple> = good
            .iter()
            .filter(|(_, p, _)| p != &format!("{AGT}turnGrain"))
            .cloned()
            .collect();
        let violations =
            validate_against_shapes(&shapes, &missing_grain, sink, contract.name.as_str())
                .expect_err("missing agt:turnGrain must violate");
        let turn_grain_predicate = format!("{AGT}turnGrain");
        assert!(
            violations.iter().any(|v| v.property_path.as_deref()
                == Some(turn_grain_predicate.as_str())
                || v.shape.as_deref() == Some("http://mnemosyne.dev/agent#RunTurnGrainShape")),
            "turnGrain violation present: {violations:?}"
        );

        // (2) a malformed (non-nested) Session IRI violates RunSessionIriShape — the
        // run-grain realizedBy breaker is gone, but the IRI invariant still has teeth.
        let malformed_session = "urn:sophia:agt:session:sess-fold";
        let malformed: Vec<Triple> = good
            .iter()
            .map(|(s, p, o)| {
                let subject = if s.as_str() == session {
                    malformed_session.to_string()
                } else {
                    s.clone()
                };
                (subject, p.clone(), o.clone())
            })
            .collect();
        let violations = validate_against_shapes(&shapes, &malformed, sink, contract.name.as_str())
            .expect_err("malformed flat session IRI must violate");
        assert!(
            violations.iter().any(
                |v| v.shape.as_deref() == Some("http://mnemosyne.dev/agent#RunSessionIriShape")
            ),
            "Session IRI shape violation present: {violations:?}"
        );
    }

    /// EA-3 §WS3: the wf-agent-world-runtime pack registers + parses via the real
    /// by-name resolver. CRITICAL asymmetry: each class KEY equals the wire `kind`
    /// (schemas.rs:572 `contract.classes.contains_key(&r.kind)`) while the rdf_type
    /// DIFFERS (SessionMessage→agt:Message, …). Predicate local names match the
    /// generic-record FIELD names so the no-silent-drop planner guard accepts them.
    #[test]
    fn wf_agent_world_runtime_registers_and_parses_natively() {
        let c =
            get_vocabulary("wf-agent-world-runtime").expect("wf-agent-world-runtime registered");
        assert_eq!(c.name, "wf-agent-world-runtime");
        assert_eq!(c.version, "1.0.0");
        assert_eq!(c.primary_prefix, "agt");
        assert_eq!(c.primary_namespace(), "http://mnemosyne.dev/agent#");
        assert_eq!(c.write_target.as_deref(), Some("projection:agent-world"));
        // Served (catalog + by-name find).
        assert!(
            crate::emporium::vocabs::find_contract("wf-agent-world-runtime", "latest").is_some()
        );

        // (assumption-confirm) the class KEY is the UNTRANSFORMED TS wire `kind`
        // (orchestrator.ts ships SessionMessage/SessionComment/… verbatim), so the
        // generic ingest gate (`contract.classes.contains_key(&r.kind)`) resolves it.
        for kind in [
            "SessionState",
            "SessionMessage",
            "SessionComment",
            "SessionApproval",
            "PromptBinding",
        ] {
            assert!(
                c.classes.contains_key(kind),
                "wire kind {kind} is a class key"
            );
        }
        // … but the rdf_type DIFFERS from the key (the §WS3 serializer projection).
        assert_eq!(
            c.classes["SessionState"].rdf_types,
            vec!["agt:SessionState"]
        );
        assert_eq!(c.classes["SessionMessage"].rdf_types, vec!["agt:Message"]);
        assert_eq!(c.classes["SessionComment"].rdf_types, vec!["agt:Comment"]);
        assert_eq!(c.classes["SessionApproval"].rdf_types, vec!["agt:Approval"]);
        assert_eq!(
            c.classes["PromptBinding"].rdf_types,
            vec!["agt:PromptBinding"]
        );

        // Predicate local names match the generic-record FIELD names, not the
        // serializer predicate names: SessionState carries agt:runId (field `runId`,
        // NOT agt:activeRunId); messages/comments carry agt:text (NOT messageText/
        // commentText); approvals carry agt:ts (NOT approvalTime); bindings carry
        // agt:documentId/snapshotId/renderedDigest.
        assert!(c.classes["SessionState"]
            .predicates
            .contains_key("agt:runId"));
        assert!(c.classes["SessionMessage"]
            .predicates
            .contains_key("agt:text"));
        assert!(c.classes["SessionComment"]
            .predicates
            .contains_key("agt:text"));
        assert!(c.classes["SessionApproval"]
            .predicates
            .contains_key("agt:ts"));
        assert!(c.classes["PromptBinding"]
            .predicates
            .contains_key("agt:documentId"));
        // The dateTime + count datatypes round-trip.
        assert_eq!(
            c.classes["SessionState"].predicates["agt:updatedAt"].datatype,
            Datatype::dateTime
        );
        assert_eq!(
            c.classes["SessionState"].predicates["agt:messageCount"].datatype,
            Datatype::integer
        );
        // All five classes materialize into the same projection:agent-world sink.
        for cls in c.classes.values() {
            assert_eq!(cls.store_target.as_deref(), Some("projection:agent-world"));
        }
    }

    #[test]
    fn protocol_has_empty_rdf_types() {
        let c = workflow_vocabulary();
        assert!(c.classes["Protocol"].rdf_types.is_empty());
        // Protocol still carries predicates (registry-only, never minted).
        assert!(c.classes["Protocol"].predicates.contains_key("wf:name"));
    }

    // ── EA-2b retrofit contracts (authored mirrors of code-defined spans) ──

    #[test]
    fn graph_vocabulary_mirrors_the_graph_metadata_span() {
        let c = graph_vocabulary();
        assert_eq!(c.name, "emporium-graph");
        assert_eq!(c.primary_prefix, "mnemo");
        // Single class, the mnemo:LocalGraph head, in the runtime MNEMO_NS.
        assert_eq!(c.primary_namespace(), "https://mnemosyne.local/ns#");
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["LocalGraph"]);
        let lg = &c.classes["LocalGraph"];
        assert_eq!(lg.rdf_types, vec!["mnemo:LocalGraph"]);
        // 7 predicates (the 8-triple span minus rdf:type) — all required.
        assert_eq!(lg.predicates.len(), 7);
        assert!(lg.predicates.values().all(|p| p.required));
        // created/modified are SIMPLE string literals (NOT dateTime) — matches the
        // materializer's sparql_string_literal; an xsd:dateTime shape would reject
        // the real projection.
        assert_eq!(lg.predicates["dcterms:created"].datatype, Datatype::string);
        assert_eq!(lg.predicates["dcterms:modified"].datatype, Datatype::string);
    }

    #[test]
    fn salience_vocabulary_mirrors_the_blockvaluation_span() {
        let c = salience_vocabulary();
        assert_eq!(c.name, "emporium-salience");
        assert_eq!(c.primary_prefix, "mnemo");
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["BlockValuation"]);
        let bv = &c.classes["BlockValuation"];
        assert_eq!(bv.rdf_types, vec!["mnemo:BlockValuation"]);
        // The dual-namespace fan-out: 20 declared predicates (mnemo:/mdoc: mirrors
        // + namespace-specific ones), the full span salience_value_triples mints.
        assert_eq!(bv.predicates.len(), 20);
        // The float predicates use the EA-2b Datatype::float variant (push_float_triple
        // emits ^^xsd:float; xsd:double would NOT match — proven by the validator probe).
        assert_eq!(
            bv.predicates["mdoc:rawImportanceSum"].datatype,
            Datatype::float
        );
        assert_eq!(
            bv.predicates["mnemo:cumulativeImportance"].datatype,
            Datatype::float
        );
        // Counts are xsd:integer (push_integer_triple), tags are multi.
        assert_eq!(
            bv.predicates["mnemo:valuationCount"].datatype,
            Datatype::integer
        );
        assert!(bv.predicates["mnemo:tag"].multi);
        assert!(bv.predicates["mdoc:tag"].multi);
        // The optional guards are NOT required (sparse/guarded emission).
        assert!(!bv.predicates["mnemo:lastValuatedAt"].required);
        assert!(!bv.predicates["mdoc:userImportance"].required);
    }

    // ── EA-2b+ structural-fork retrofit contracts ──

    #[test]
    fn wires_vocabulary_mirrors_the_wire_span() {
        let c = wires_vocabulary();
        assert_eq!(c.name, "emporium-wires");
        assert_eq!(c.primary_prefix, "wire");
        // wire: is the runtime WIRE_NS (the gardend MNEMO vocab prefix).
        assert_eq!(c.primary_namespace(), "http://mnemosyne.ai/vocab#");
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["Wire"]);
        let w = &c.classes["Wire"];
        assert_eq!(w.rdf_types, vec!["wire:Wire"]);
        // targetGraph is the ONE always-emitted (required) non-type predicate.
        assert!(w.predicates["wire:targetGraph"].required);
        // Endpoints are object properties (uri) but NOT required (construction-driven)
        // — the shapes thus do NOT force them present (dangling refs allowed by design).
        assert_eq!(w.predicates["wire:targetDocument"].datatype, Datatype::uri);
        assert!(!w.predicates["wire:targetDocument"].required);
        assert_eq!(
            w.predicates["wire:bidirectional"].datatype,
            Datatype::boolean
        );
    }

    #[test]
    fn song_vocabulary_mirrors_the_three_song_classes() {
        let c = song_vocabulary();
        assert_eq!(c.name, "emporium-song");
        assert_eq!(c.primary_prefix, "mnemo");
        assert_eq!(c.primary_namespace(), "https://mnemosyne.local/ns#");
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        // BTreeMap order: Song, SongCoda, SongVerse.
        assert_eq!(keys, vec!["Song", "SongCoda", "SongVerse"]);
        assert_eq!(c.classes["Song"].rdf_types, vec!["mnemo:Song"]);
        assert_eq!(c.classes["SongVerse"].rdf_types, vec!["mnemo:SongVerse"]);
        assert_eq!(c.classes["SongCoda"].rdf_types, vec!["mnemo:SongCoda"]);
        // verseIndex is xsd:integer; created/modified are SIMPLE string literals.
        assert_eq!(
            c.classes["SongVerse"].predicates["mnemo:verseIndex"].datatype,
            Datatype::integer
        );
        assert_eq!(
            c.classes["SongVerse"].predicates["dcterms:created"].datatype,
            Datatype::string
        );
    }

    #[test]
    fn document_vocabulary_mirrors_the_node_tree_classes() {
        let c = document_vocabulary();
        assert_eq!(c.name, "emporium-document");
        assert_eq!(c.primary_prefix, "mdoc");
        assert_eq!(c.primary_namespace(), "http://mnemosyne.dev/doc#");
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["Paragraph", "TextNode", "XmlFragment"]);
        assert_eq!(c.classes["XmlFragment"].rdf_types, vec!["mdoc:XmlFragment"]);
        // childNode is the recursive tree edge — multi, object property (uri).
        let cn = &c.classes["XmlFragment"].predicates["mdoc:childNode"];
        assert_eq!(cn.datatype, Datatype::uri);
        assert!(cn.multi);
        // documentId is required on every node class.
        assert!(c.classes["TextNode"].predicates["mnemo:documentId"].required);
    }

    #[test]
    fn workspace_vocabulary_mirrors_the_entity_classes() {
        let c = workspace_vocabulary();
        assert_eq!(c.name, "emporium-workspace");
        assert_eq!(c.primary_prefix, "mdoc");
        let keys: Vec<&str> = c.classes.keys().map(String::as_str).collect();
        // The 3 mdoc-namespaced entity classes (Wire is in its own contract).
        assert_eq!(keys, vec!["Artifact", "Folder", "TipTapDocument"]);
        assert_eq!(c.classes["Folder"].rdf_types, vec!["mdoc:Folder"]);
        // order is xsd:float (push_workspace_number_triple).
        assert_eq!(
            c.classes["Folder"].predicates["mdoc:order"].datatype,
            Datatype::float
        );
        assert_eq!(
            c.classes["Artifact"].predicates["nie:mimeType"].datatype,
            Datatype::string
        );
        // Workspace producers store byte counts as JSON/Yjs numbers and the
        // RDF projection emits exact xsd:integer literals for both faces.
        assert_eq!(
            c.classes["TipTapDocument"].predicates["mdoc:sourceContentSize"].datatype,
            Datatype::integer
        );
        assert_eq!(
            c.classes["Artifact"].predicates["nfo:fileSize"].datatype,
            Datatype::integer
        );
    }

    #[test]
    fn float_and_double_are_distinct_datatypes() {
        // The EA-2b extension: float is NOT double. SHACL sh:datatype is an exact
        // IRI match, so conflating them would derive a shape that rejects the real
        // ^^xsd:float salience projection.
        assert_ne!(Datatype::float, Datatype::double);
    }

    #[test]
    fn expand_resolves_curies_and_passes_through_full_uris() {
        let c = workflow_vocabulary();
        assert_eq!(
            c.expand("wf:name").unwrap(),
            "http://mnemosyne.dev/workflow#name"
        );
        assert_eq!(
            c.expand("dcterms:title").unwrap(),
            "http://purl.org/dc/terms/title"
        );
        assert_eq!(
            c.expand("rdf:type").unwrap(),
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
        );
        // Full URIs and urns pass through unchanged.
        assert_eq!(
            c.expand("http://example.org/x").unwrap(),
            "http://example.org/x"
        );
        assert_eq!(c.expand("urn:sophia:wf:x").unwrap(), "urn:sophia:wf:x");
        // Unknown prefix errors.
        assert!(c.expand("nope:thing").is_err());
    }

    #[test]
    fn primary_namespace_is_wf() {
        assert_eq!(
            workflow_vocabulary().primary_namespace(),
            "http://mnemosyne.dev/workflow#"
        );
    }

    #[test]
    fn workflow_required_predicates_match_the_golden() {
        let c = workflow_vocabulary();
        let wf = &c.classes["Workflow"];
        let required: std::collections::BTreeSet<&str> = wf
            .predicates
            .iter()
            .filter(|(_, p)| p.required)
            .map(|(k, _)| k.as_str())
            .collect();
        let expected: std::collections::BTreeSet<&str> = [
            "wf:name",
            "wf:description",
            "wf:phase",
            "wf:scriptBlock",
            "wf:scriptSha256",
        ]
        .into_iter()
        .collect();
        assert_eq!(required, expected);
        // wf:whenToUse is optional.
        assert!(!wf.predicates["wf:whenToUse"].required);
        // wf:description is a `wf:` predicate (NOT dcterms) and is multi=false.
        assert_eq!(wf.predicates["wf:description"].datatype, Datatype::string);
        assert!(wf.predicates["wf:phase"].multi);
    }

    #[test]
    fn known_predicate_uris_are_all_wf_or_prov_or_dcterms() {
        let c = workflow_vocabulary();
        let uris = c.known_predicate_uris();
        assert!(uris.contains("http://mnemosyne.dev/workflow#name"));
        assert!(uris.contains("http://www.w3.org/ns/prov#used"));
        assert!(uris.contains("http://purl.org/dc/terms/title"));
        // Every minted predicate resolves under a declared namespace.
        for uri in &uris {
            assert!(uri.contains("://"), "predicate URI not expanded: {uri}");
        }
    }

    // ── S6: `identity_kind` made LOAD-BEARING (was parsed but consumed nowhere) ──
    //
    // The appraisal's finding #2: `identity_kind` is dead metadata, and its drift
    // already shipped (chamber declared `content-hash` but minted a versioned logical
    // id). This test makes the field a CHECKABLE invariant against what the minting
    // code actually does, derived from the `subject_rule` shape. Two biconditionals,
    // both grounded in the code and enforced over EVERY class of EVERY SERVED pack:
    //
    //   (A) `content-hash` ⟺ the subject_rule is HASH-BASED — it interpolates a
    //       content hash (`{hash}`/`{contentHash}`/`{memId}` — sophia-memory-core mints
    //       `memory_record_id` = a content hash, planner.rs ~1570-1590) or is
    //       documented `content-hash`. A class that CLAIMS content identity must
    //       actually mint one; a class that mints one must not hide behind another kind.
    //   (B) `resolve-by-query` ⟺ `store_mode` is Virtual — a resolve-by-query subject
    //       is never materialized (it is computed on read from a SPARQL rule).
    //
    // Anything else (`urn-template`/`logical-id`/`doc-uri`/`event-id`) is a
    // caller-stable template/URI convention — NOT hash-based, NOT virtual — and the
    // two biconditionals pin exactly that by exclusion.

    /// Does a class's `subject_rule` describe HASH-BASED minting? (invariant A's RHS)
    /// True iff it interpolates a content-hash slot or is documented `content-hash`.
    /// The marker set is derived EXHAUSTIVELY from the real content-hash minting rules
    /// across the served goldens — nothing broader, so an unrelated class is never
    /// mis-read as content-addressed:
    ///   - memory EvidenceLink/SourceReference: `…:{hash}`
    ///   - memory MemoryRecord: `…:{memId}` (`memId` = `memory_record_id`, a content
    ///     hash — planner.rs ~1570-1590) + the documented `content-hash` phrase
    ///   - workflow PageView: `…:{contentHash}`
    /// NB the markers are CLOSED (`{hash}`, not a `{sha` PREFIX) precisely so a
    /// caller-stable field like `{shardId}` (event-id) is not swept up as a hash.
    fn subject_rule_is_hash_based(rule: &str) -> bool {
        let lower = rule.to_ascii_lowercase();
        lower.contains("{hash}")
            || lower.contains("{contenthash}")
            || lower.contains("{memid}")
            || lower.contains("content-hash")
    }

    #[test]
    fn identity_kind_is_consistent_with_subject_rule_for_every_served_pack() {
        for (pack_name, json, _sha) in crate::emporium::vocabs::VOCAB_REGISTRY.iter() {
            let contract: VocabularyContract = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("served golden '{pack_name}' must parse: {e}"));
            for (class_name, class) in &contract.classes {
                let where_ = format!("{pack_name}.{class_name}");
                // Every SERVED class must declare BOTH so the test can check them
                // (the parse defaults them to None; a served class that omits either
                // is itself a golden defect this test surfaces).
                let identity_kind = class
                    .identity_kind
                    .as_deref()
                    .unwrap_or_else(|| panic!("{where_}: served class must declare identity_kind"));
                let store_mode = class
                    .store_mode
                    .as_deref()
                    .unwrap_or_else(|| panic!("{where_}: served class must declare store_mode"));
                let identity = IdentityKind::parse(identity_kind)
                    .unwrap_or_else(|| panic!("{where_}: unknown identity_kind '{identity_kind}'"));
                let store = StoreMode::parse(store_mode)
                    .unwrap_or_else(|| panic!("{where_}: unknown store_mode '{store_mode}'"));
                let rule = class.subject_rule.as_deref().unwrap_or_else(|| {
                    panic!("{where_}: served class must declare a subject_rule")
                });
                let hash_based = subject_rule_is_hash_based(rule);

                // (A) content-hash ⟺ hash-based subject_rule.
                assert_eq!(
                    identity == IdentityKind::ContentHash,
                    hash_based,
                    "{where_}: identity_kind='{identity_kind}' but subject_rule={rule:?} \
                     — a content-hash class MUST mint a hash and only a content-hash class may \
                     (invariant A). This is the exact chamber drift the appraisal found."
                );

                // (B) resolve-by-query ⟺ store_mode Virtual.
                assert_eq!(
                    identity == IdentityKind::ResolveByQuery,
                    store == StoreMode::Virtual,
                    "{where_}: identity_kind='{identity_kind}' vs store_mode='{store_mode}' \
                     — a resolve-by-query subject is computed-on-read (Virtual) and only those \
                     (invariant B)."
                );
            }
        }
    }
}

//! Identity-fenced source synchronization for fully-offline Shrubbery clients.
//!
//! The protocol is deliberately above HTTP caching and below Surfaces.  A
//! client mirrors authoritative sources, authors stable intents while
//! partitioned, and later submits those intents at least once.  This module
//! records the intent before updating any disposable projection and gives
//! every accepted operation one of three durable outcomes:
//!
//! * applied exactly once (duplicate delivery returns the same receipt);
//! * retained as an explicit [`SyncConflict`] with every candidate;
//! * rejected before acceptance because its graph/object incarnation is stale.
//!
//! Current-state objects fold from a causal operation DAG, event-log objects
//! fold by set union over stable event identity, Y.Docs merge through yrs, and
//! derived RDF is rebuilt from those sources.  The per-graph ledger is an
//! atomic JSON file under the graph lifetime, so deleting/recreating a graph
//! also creates a fresh source authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{Literal, NamedNode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::app_error::{AppError, AppResult};
use crate::app_error_codes;
use crate::app_runtime::AppHandle;
use crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator;
use crate::crdt_engine::rooms::RoomRegistry;
use crate::document_incarnation_store::ensure_document_incarnation_id;
use crate::document_service::{read_graph_documents_cold, read_workspace_record};
use crate::emporium::contract::{
    get_vocabulary, ReconciliationStrategy, SourceKind, VocabularyContract,
};
use crate::emporium::planner::plan_generic_compute;
use crate::emporium::reconcile::{
    apply_diff, reconcile_class_validated, reconcile_classes, ClassScope, Placement, SpanKey,
};
use crate::emporium::schemas::{GenericRecordIn, MemoryRecordIn};
use crate::emporium::terms::{Term, Triple, TripleDiff};
use crate::graph_record_store::{ensure_graph_incarnation, read_graph_record_no_heal};
use crate::paths::{document_ydoc_state_path, existing_graph_dir, workspace_ydoc_state_path};
use crate::rdf::graph_subject;
use crate::rdf_authority::user_rdf_graph_iri;
use crate::rdf_service::{ensure_graph_store_seeded, open_graph_store};
use crate::runtime_config::RDF_TYPE;
use crate::semantic_search_projection::semantic_block_sources;
use crate::storage::{read_json, write_json};

const SOURCE_SYNC_SCHEMA_VERSION: u32 = 1;
const SOURCE_SYNC_DIR: &str = "source-sync";
const SOURCE_SYNC_LEDGER_FILE: &str = "ledger.json";
const ROOT_VERSION: &str = "root";
const SYNC_NS: &str = "http://mnemosyne.dev/sync#";
const SYNC_CONFLICT_TYPE: &str = "http://mnemosyne.dev/sync#SyncConflict";
const SYNC_CONFLICT_CANDIDATE_TYPE: &str = "http://mnemosyne.dev/sync#SyncConflictCandidate";
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";
const MAX_OPERATION_ID_LEN: usize = 160;
const MAX_SOURCE_BATCH: usize = 1_000;

static SOURCE_GATES: OnceLock<StdMutex<BTreeMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();

pub(crate) struct SourceGateGuard {
    _guard: OwnedMutexGuard<()>,
}

pub(crate) async fn acquire_source_gate(graph_id: &str) -> SourceGateGuard {
    let gate = {
        let mut gates = SOURCE_GATES
            .get_or_init(|| StdMutex::new(BTreeMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            gates
                .entry(graph_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    };
    SourceGateGuard {
        _guard: gate.lock_owned().await,
    }
}

pub(crate) fn source_authority_active(app: &AppHandle, graph_id: &str) -> AppResult<bool> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    Ok(ledger_path(&graph_dir).is_file())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceLedger {
    schema_version: u32,
    graph_id: String,
    graph_incarnation: String,
    revision: u64,
    #[serde(default)]
    operations: BTreeMap<String, LedgerOperation>,
    #[serde(default)]
    workflow_fold_triples: Vec<TripleWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<SourceCheckpoint>,
}

impl SourceLedger {
    fn empty(graph_id: &str, graph_incarnation: &str) -> Self {
        Self {
            schema_version: SOURCE_SYNC_SCHEMA_VERSION,
            graph_id: graph_id.to_string(),
            graph_incarnation: graph_incarnation.to_string(),
            revision: 0,
            operations: BTreeMap::new(),
            workflow_fold_triples: Vec::new(),
            checkpoint: None,
        }
    }
}

/// A content-addressed, identity-fenced checkpoint of authorities that predate
/// the source ledger plus the disposable RDF face at one ledger revision.
///
/// The RDF bytes are returned to clients as an epoch-bound read cache and are
/// also the migration floor for projections whose historical source existed
/// only in the legacy store. New source operations always replay on top of
/// this floor. Value-store files are current-state authorities (not merely RDF
/// faces), so they are checkpointed separately and replayed before new stable
/// valuation events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceCheckpoint {
    ledger_revision: u64,
    rdf_digest: String,
    rdf_quad_count: usize,
    value_stores_digest: String,
    captured_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerOperation {
    digest: String,
    accepted_revision: u64,
    status: ReceiptStatus,
    operation: SourceOperation,
    outcome: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effect_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum ReceiptStatus {
    Accepted,
    Applied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum SourceOperation {
    CurrentState {
        operation_id: String,
        vocab: String,
        class: String,
        object_id: String,
        base_version: String,
        record: Value,
        #[serde(default)]
        causal_order: Option<i64>,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        evidence_weight: Option<f64>,
    },
    ResolveCurrent {
        operation_id: String,
        object_key: String,
        conflict_id: String,
        #[serde(default)]
        chosen_operation_id: Option<String>,
        #[serde(default)]
        record: Option<Value>,
    },
    EventLog {
        operation_id: String,
        event_id: String,
        vocab: String,
        class: String,
        record: Value,
    },
    Memory {
        operation_id: String,
        #[serde(default)]
        observer: String,
        #[serde(default)]
        publish: bool,
        at_ms: i64,
        records: Vec<Value>,
    },
    Valuation {
        operation_id: String,
        valuation_event_id: String,
        #[serde(default)]
        observer: String,
        document_id: String,
        block_id: String,
        #[serde(default)]
        importance: Option<i64>,
        #[serde(default)]
        valence: Option<i64>,
        #[serde(default)]
        tags: Vec<String>,
        at_ms: i64,
    },
    Retraction {
        operation_id: String,
        retraction_event_id: String,
        subject: String,
        rationale: String,
        #[serde(default = "default_retraction_kind")]
        retraction_kind: String,
        #[serde(default)]
        observer: Option<String>,
        at_ms: i64,
    },
    DocumentLifecycle {
        operation_id: String,
        action: DocumentLifecycleAction,
        document_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        new_document_incarnation: Option<String>,
        #[serde(default)]
        expected_document_incarnation: Option<String>,
        #[serde(default)]
        initial_update_base64: Option<String>,
    },
    WorkspaceUpdate {
        operation_id: String,
        update_base64: String,
    },
    DocumentUpdate {
        operation_id: String,
        document_id: String,
        document_incarnation: String,
        update_base64: String,
    },
    CrdtCommand {
        operation_id: String,
        command_kind: String,
        #[serde(default)]
        document_id: Option<String>,
        #[serde(default)]
        payload: Value,
    },
    GraphMetadata {
        operation_id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
    },
}

fn default_retraction_kind() -> String {
    "retract".to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum DocumentLifecycleAction {
    Create,
    Delete,
    Recreate,
}

impl SourceOperation {
    fn operation_id(&self) -> &str {
        match self {
            Self::CurrentState { operation_id, .. }
            | Self::ResolveCurrent { operation_id, .. }
            | Self::EventLog { operation_id, .. }
            | Self::Memory { operation_id, .. }
            | Self::Valuation { operation_id, .. }
            | Self::Retraction { operation_id, .. }
            | Self::DocumentLifecycle { operation_id, .. }
            | Self::WorkspaceUpdate { operation_id, .. }
            | Self::DocumentUpdate { operation_id, .. }
            | Self::CrdtCommand { operation_id, .. }
            | Self::GraphMetadata { operation_id, .. } => operation_id,
        }
    }

    fn requires_individual_effect(&self) -> bool {
        matches!(
            self,
            Self::DocumentLifecycle { .. }
                | Self::WorkspaceUpdate { .. }
                | Self::DocumentUpdate { .. }
                | Self::CrdtCommand { .. }
                | Self::GraphMetadata { .. }
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourcePushInput {
    #[serde(alias = "graph_id")]
    graph_id: String,
    #[serde(alias = "graph_incarnation")]
    graph_incarnation: String,
    operations: Vec<SourceOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourcePullInput {
    #[serde(alias = "graph_id")]
    graph_id: String,
    #[serde(default, alias = "graph_incarnation")]
    graph_incarnation: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceRebuildInput {
    #[serde(alias = "graph_id")]
    graph_id: String,
    #[serde(alias = "graph_incarnation")]
    graph_incarnation: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationReceipt {
    operation_id: String,
    digest: String,
    accepted_revision: u64,
    status: ReceiptStatus,
    duplicate: bool,
    outcome: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    effect_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentObjectFace {
    object_key: String,
    vocab: String,
    class: String,
    object_id: String,
    source_version: String,
    record: Value,
    reconciliation_strategy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_id: Option<String>,
    /// The operation whose record is projected as this head. For a resolved
    /// head this is the `ResolveCurrent` operation id, which is how a reader
    /// learns the head was chosen rather than merely projected.
    operation_id: String,
    /// Verbatim `client_id` of the operation that minted the head. `None`
    /// for a resolved head (`ResolveCurrent` carries no client id) and for
    /// pre-attribution ledger rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentCandidateFace {
    operation_id: String,
    source_version: String,
    base_version: String,
    record: Value,
    /// All three below are verbatim copies of the operation's own optional
    /// fields — delivery-order independent by construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    causal_order: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_weight: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncConflict {
    conflict_id: String,
    object_key: String,
    base_version: String,
    reconciliation_strategy: String,
    reason: String,
    candidates: Vec<CurrentCandidateFace>,
    projected_operation_id: String,
}

#[derive(Debug, Clone)]
struct CurrentCandidate {
    operation_id: String,
    source_version: String,
    base_version: String,
    record: Value,
    causal_order: Option<i64>,
    client_id: Option<String>,
    evidence_weight: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentFold {
    face: CurrentObjectFace,
    conflict: Option<SyncConflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
struct TripleWire {
    subject: String,
    predicate: String,
    object_nt: String,
}

impl TripleWire {
    fn from_triple((subject, predicate, object): &Triple) -> Self {
        Self {
            subject: subject.clone(),
            predicate: predicate.clone(),
            object_nt: object.as_nt(),
        }
    }

    fn to_triple(&self) -> Triple {
        (
            self.subject.clone(),
            self.predicate.clone(),
            crate::emporium::survey::parse_term(&self.object_nt),
        )
    }
}

fn ledger_path(graph_dir: &Path) -> PathBuf {
    graph_dir
        .join(SOURCE_SYNC_DIR)
        .join(SOURCE_SYNC_LEDGER_FILE)
}

fn checkpoint_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join(SOURCE_SYNC_DIR).join("checkpoints")
}

fn rdf_checkpoint_path(graph_dir: &Path, digest: &str) -> PathBuf {
    checkpoint_dir(graph_dir).join(format!("{digest}.nq"))
}

fn values_checkpoint_path(graph_dir: &Path, digest: &str) -> PathBuf {
    checkpoint_dir(graph_dir).join(format!("{digest}.values.json"))
}

fn capture_value_store_files(graph_dir: &Path) -> AppResult<BTreeMap<String, String>> {
    capture_value_store_files_inner(graph_dir, false)
}

fn capture_value_store_files_inner(graph_dir: &Path, pull_limited: bool) -> AppResult<BTreeMap<String, String>> {
    let values_dir = graph_dir.join("values");
    let mut files = BTreeMap::new();
    let mut bytes_read = 0;
    if !values_dir.is_dir() {
        return Ok(files);
    }
    let mut pending = vec![values_dir.clone()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|error| {
            AppError::storage(format!(
                "read value-store directory {}: {error}",
                directory.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                AppError::storage(format!(
                    "read value-store entry {}: {error}",
                    directory.display()
                ))
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| {
                AppError::storage(format!("read value-store type {}: {error}", path.display()))
            })?;
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file()
                || path.file_name().and_then(|name| name.to_str()) != Some("block-values.json")
            {
                continue;
            }
            let relative = path
                .strip_prefix(&values_dir)
                .map_err(|error| AppError::storage(format!("value-store relative path: {error}")))?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = if pull_limited { crate::source_pull_budget::read_bytes(&path, &mut bytes_read)? }
                else { crate::storage::read_bytes(&path)? };
            let data = String::from_utf8(bytes).map_err(|error| {
                AppError::storage(format!("read value store encoding: {error}"))
            })?;
            // Parse before accepting the file into an authoritative checkpoint.
            serde_json::from_str::<Value>(&data).map_err(|error| {
                AppError::serialization(format!("parse value store {}: {error}", path.display()))
            })?;
            files.insert(relative, data);
        }
    }
    Ok(files)
}

fn checkpoint_files(
    graph_dir: &Path,
    checkpoint: &SourceCheckpoint,
) -> AppResult<(String, BTreeMap<String, String>)> {
    checkpoint_files_inner(graph_dir, checkpoint, false)
}

fn checkpoint_files_inner(graph_dir: &Path, checkpoint: &SourceCheckpoint, pull_limited: bool) -> AppResult<(String, BTreeMap<String, String>)> {
    let rdf_path = rdf_checkpoint_path(graph_dir, &checkpoint.rdf_digest);
    let mut bytes_read = 0;
    let rdf_bytes = if pull_limited { crate::source_pull_budget::read_bytes(&rdf_path, &mut bytes_read)? }
        else { crate::storage::read_bytes(&rdf_path)? };
    let rdf = String::from_utf8(rdf_bytes).map_err(|error| {
        AppError::serialization(format!("checkpoint RDF is not UTF-8: {error}"))
    })?;
    if sha256_bytes(rdf.as_bytes()) != checkpoint.rdf_digest {
        return Err(AppError::conflict(
            "source checkpoint RDF digest does not match its ledger identity",
        )
        .with_code(app_error_codes::LEDGER_INTEGRITY));
    }
    let values_path = values_checkpoint_path(graph_dir, &checkpoint.value_stores_digest);
    let values = if pull_limited { crate::source_pull_budget::read_bytes(&values_path, &mut bytes_read)? }
        else { crate::storage::read_bytes(&values_path)? };
    if sha256_bytes(&values) != checkpoint.value_stores_digest {
        return Err(AppError::conflict(
            "source checkpoint value-store digest does not match its ledger identity",
        )
        .with_code(app_error_codes::LEDGER_INTEGRITY));
    }
    let values = serde_json::from_slice::<BTreeMap<String, String>>(&values).map_err(|error| {
        AppError::serialization(format!("parse source checkpoint value stores: {error}"))
    })?;
    Ok((rdf, values))
}

fn capture_checkpoint(
    graph_dir: &Path,
    store: &oxigraph::store::Store,
    ledger_revision: u64,
) -> AppResult<SourceCheckpoint> {
    capture_checkpoint_inner(graph_dir, store, ledger_revision, false)
}

fn capture_checkpoint_inner(graph_dir: &Path, store: &oxigraph::store::Store, ledger_revision: u64, pull_limited: bool) -> AppResult<SourceCheckpoint> {
    let dump = if pull_limited { source_rdf_dump(store)? }
        else { crate::rdf_query_service::dump_rdf_from_store(store, "nquads", None, None).map_err(AppError::rdf)? };
    let rdf = canonicalize_nquads(&dump.data);
    let rdf_digest = sha256_bytes(rdf.as_bytes());
    let value_stores = capture_value_store_files_inner(graph_dir, pull_limited)?;
    let value_bytes = serde_json::to_vec(&value_stores).map_err(|error| {
        AppError::serialization(format!("serialize checkpoint value stores: {error}"))
    })?;
    let value_stores_digest = sha256_bytes(&value_bytes);
    if pull_limited && rdf.len().saturating_add(value_bytes.len()) > crate::source_pull_budget::MAX_SOURCE_PULL_BYTES as usize {
        return Err(crate::source_pull_budget::capacity_error());
    }
    crate::storage::write_bytes(&rdf_checkpoint_path(graph_dir, &rdf_digest), rdf.as_bytes())?;
    crate::storage::write_bytes(
        &values_checkpoint_path(graph_dir, &value_stores_digest),
        &value_bytes,
    )?;
    Ok(SourceCheckpoint {
        ledger_revision,
        rdf_digest,
        rdf_quad_count: dump.quad_count,
        value_stores_digest,
        captured_at_ms: chrono::Utc::now().timestamp_millis(),
    })
}

/// Establish the immutable migration floor exactly once, before this ledger
/// accepts its first new source operation. It captures pre-source-sync
/// authorities so upgraded graphs remain rebuildable, but it must never
/// advance to include projections produced by later ledger operations: doing
/// so would let a "rebuild" pass by restoring its own derived answer.
fn ensure_checkpoint(
    graph_dir: &Path,
    store: &oxigraph::store::Store,
    ledger: &mut SourceLedger,
) -> AppResult<SourceCheckpoint> {
    if let Some(checkpoint) = &ledger.checkpoint {
        return Ok(checkpoint.clone());
    }
    let checkpoint = capture_checkpoint(graph_dir, store, ledger.revision)?;
    ledger.checkpoint = Some(checkpoint.clone());
    Ok(checkpoint)
}

fn rdf_dataset_identity(store: &oxigraph::store::Store) -> AppResult<Value> {
    let dump = crate::rdf_query_service::dump_rdf_from_store(store, "nquads", None, None).map_err(AppError::rdf)?;
    let canonical = canonicalize_nquads(&dump.data);
    Ok(json!({
        "digest": sha256_bytes(canonical.as_bytes()),
        "quadCount": dump.quad_count,
    }))
}

fn source_rdf_dump(store: &oxigraph::store::Store) -> AppResult<crate::rdf_query_service::RdfDumpResult> {
    crate::rdf_query_service::dump_rdf_from_store_limited(store, "nquads", None, None,
        Some(crate::source_pull_budget::MAX_SOURCE_PULL_BYTES as usize))
        .map_err(|error| if error.contains("source_bundle_too_large") {
            crate::source_pull_budget::capacity_error()
        } else { AppError::rdf(error) })
}

fn restore_checkpoint(
    graph_id: &str,
    graph_dir: &Path,
    store: &oxigraph::store::Store,
    checkpoint: &SourceCheckpoint,
) -> AppResult<()> {
    let (rdf, values) = checkpoint_files(graph_dir, checkpoint)?;
    // This is the digest-verified engine checkpoint, not a caller-supplied RDF
    // import. Its legitimate reserved projections must replay, while public
    // dataset imports keep their existing reserved-graph refusal. Fully parse
    // both authorities before any clear: malformed input cannot erase state.
    let quads = RdfParser::from_format(RdfFormat::NQuads)
        .for_slice(rdf.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::rdf(format!("parse source checkpoint RDF: {error}")))?;
    for (relative, data) in &values {
        let relative_path = Path::new(relative);
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || relative_path.file_name().and_then(|name| name.to_str()) != Some("block-values.json")
        {
            return Err(AppError::conflict(format!(
                "unsafe value-store path in source checkpoint: {relative}"
            ))
            .with_code(app_error_codes::LEDGER_INTEGRITY));
        }
        serde_json::from_str::<Value>(data).map_err(|error| {
            AppError::serialization(format!("parse source checkpoint value store {relative}: {error}"))
        })?;
    }
    crate::rdf_seed_service::with_invalidated_seed_cache(graph_dir, || {
        let mut transaction = store.start_transaction()
            .map_err(|error| format!("start source checkpoint replay: {error}"))?;
        transaction.clear()
            .map_err(|error| format!("clear source checkpoint target: {error}"))?;
        for quad in &quads {
            transaction.insert(quad);
        }
        // A restored checkpoint is not a completed current projection. Remove
        // only the derived seed-completion graph atomically with replacement;
        // final replay reconciliation recreates it. The checkpoint bytes and
        // all canonical/source RDF remain unchanged.
        let marker_graph = NamedNode::new(crate::rdf_authority::seed_marker_graph_iri(graph_id))
            .map_err(|error| format!("source checkpoint seed graph: {error}"))?;
        transaction.clear_graph(marker_graph.as_ref())
            .map_err(|error| format!("invalidate restored source seed marker: {error}"))?;
        transaction.commit()
            .map_err(|error| format!("commit source checkpoint replay: {error}"))?;
        Ok(())
    }).map_err(AppError::rdf)?;
    crate::cell_durability::mark_rdf_store_written(store);

    // RDF replacement is transactional. These value files are not in that
    // transaction; I/O failure here remains an explicit replay failure, not a
    // claim of cross-filesystem atomicity.
    let values_dir = graph_dir.join("values");
    if values_dir.exists() {
        crate::storage::remove_dir_all(&values_dir)?;
    }
    for (relative, data) in values {
        let relative_path = Path::new(&relative);
        crate::storage::write_bytes(&values_dir.join(relative_path), data.as_bytes())?;
    }
    Ok(())
}

/// Remove only the RDF faces whose complete authorities are the workspace and
/// document Y.Docs. The immutable migration floor may contain old copies of
/// these faces; restoring it must not resurrect a deleted document or stale
/// workspace namespace.
fn clear_ydoc_projection_graphs(
    store: &oxigraph::store::Store,
    graph_id: &str,
) -> AppResult<usize> {
    let workspace = crate::rdf_authority::workspace_projection_graph_iri(graph_id);
    let document_prefix = format!("{}:projection:document:", graph_subject(graph_id));
    let query = format!(
        "SELECT DISTINCT ?g WHERE {{ GRAPH ?g {{ ?s ?p ?o }} \
         FILTER(?g = <{workspace}> || STRSTARTS(STR(?g), {prefix})) }}",
        prefix = Literal::new_simple_literal(&document_prefix),
    );
    let results = oxigraph::sparql::SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|error| AppError::rdf(format!("parse Y.Doc projection inventory: {error}")))?
        .on_store(store)
        .execute()
        .map_err(|error| AppError::rdf(format!("query Y.Doc projection inventory: {error}")))?;
    let oxigraph::sparql::QueryResults::Solutions(solutions) = results else {
        return Err(AppError::rdf(
            "Y.Doc projection inventory did not return solutions",
        ));
    };
    let mut graphs = BTreeSet::from([workspace]);
    for solution in solutions {
        let solution = solution
            .map_err(|error| AppError::rdf(format!("read Y.Doc projection graph: {error}")))?;
        if let Some(oxigraph::model::Term::NamedNode(graph)) = solution.get("g") {
            graphs.insert(graph.as_str().to_string());
        }
    }
    for graph in &graphs {
        clear_named_graph(store, graph)?;
    }
    Ok(graphs.len())
}

fn read_ledger(
    graph_dir: &Path,
    graph_id: &str,
    graph_incarnation: &str,
) -> AppResult<SourceLedger> {
    let path = ledger_path(graph_dir);
    if !path.is_file() {
        return Ok(SourceLedger::empty(graph_id, graph_incarnation));
    }
    let ledger = read_json::<SourceLedger>(&path)?;
    if ledger.schema_version != SOURCE_SYNC_SCHEMA_VERSION {
        return Err(AppError::conflict(format!(
            "unsupported source ledger schema {} (expected {})",
            ledger.schema_version, SOURCE_SYNC_SCHEMA_VERSION
        ))
        .with_code(app_error_codes::LEDGER_INTEGRITY));
    }
    if ledger.graph_id != graph_id || ledger.graph_incarnation != graph_incarnation {
        return Err(AppError::conflict(
            "source ledger identity does not match the active graph incarnation",
        )
        .with_code(app_error_codes::STALE_GRAPH_INCARNATION));
    }
    Ok(ledger)
}

fn write_ledger(graph_dir: &Path, ledger: &SourceLedger) -> AppResult<()> {
    write_json(&ledger_path(graph_dir), ledger)
}

fn canonical_json_bytes(value: &Value) -> AppResult<Vec<u8>> {
    serde_json_canonicalizer::to_vec(value)
        .map_err(|error| AppError::serialization(format!("canonical source JSON: {error}")))
}

/// N-Quads is one complete quad per physical line. Oxigraph's iteration order
/// is deliberately unspecified and may change after RocksDB compaction or a
/// cold reopen, so wire digests must sort those complete records first. This
/// makes a source snapshot identity a property of the RDF set rather than the
/// storage engine's traversal order.
fn canonicalize_nquads(data: &str) -> String {
    let mut lines = data
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    lines.sort_unstable();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn value_digest(value: &Value) -> AppResult<String> {
    let bytes = canonical_json_bytes(value)?;
    Ok(sha256_bytes(&bytes))
}

fn operation_digest(operation: &SourceOperation) -> AppResult<String> {
    let value = serde_json::to_value(operation)
        .map_err(|error| AppError::serialization(format!("serialize source operation: {error}")))?;
    value_digest(&value)
}

fn source_epoch(graph_incarnation: &str, revision: u64, manifest_hash: &str) -> String {
    // The source ledger revision advances only for source operations. Direct
    // authoritative changes (for example a live Y.Doc/document mutation or an
    // original-file write) can change the complete source set without
    // advancing that ledger. Epoch consumers invalidate derived state by this
    // identity, so bind it to the complete manifest rather than publishing two
    // different source sets under the same `incarnation:revision` label.
    format!("{graph_incarnation}:{revision}:{manifest_hash}")
}

fn validate_stable_id(value: &str, label: &str) -> AppResult<()> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AppError::validation(format!("{label} must not be empty")));
    }
    if value.len() > MAX_OPERATION_ID_LEN
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(AppError::validation(format!(
            "{label} must be <={MAX_OPERATION_ID_LEN} characters of [A-Za-z0-9._:-]"
        )));
    }
    Ok(())
}

fn object_key(vocab: &str, class: &str, object_id: &str) -> String {
    format!("{vocab}\u{1f}{class}\u{1f}{object_id}")
}

fn reconciliation_strategy_label(strategy: ReconciliationStrategy) -> &'static str {
    match strategy {
        ReconciliationStrategy::ProducerDirected => "producerDirected",
        ReconciliationStrategy::Contested => "contested",
        ReconciliationStrategy::CausalLww => "causalLww",
        ReconciliationStrategy::EvidenceWeighted => "evidenceWeighted",
        ReconciliationStrategy::CodeBacked => "codeBacked",
    }
}

fn normalized_generic_record(class: &str, object_id: &str, record: &Value) -> AppResult<Value> {
    let mut record = record
        .as_object()
        .cloned()
        .ok_or_else(|| AppError::validation("source object record must be a JSON object"))?;
    if let Some(kind) = record.get("kind").and_then(Value::as_str) {
        if kind != class {
            return Err(AppError::validation(format!(
                "record kind '{kind}' does not match declared class '{class}'"
            )));
        }
    }
    record.insert("kind".to_string(), json!(class));
    match record.get("localId").and_then(Value::as_str) {
        Some(local_id) if local_id != object_id => {
            return Err(AppError::validation(format!(
                "record localId '{local_id}' does not match objectId '{object_id}'"
            )))
        }
        Some(_) => {}
        None => {
            record.insert("localId".to_string(), json!(object_id));
        }
    }
    Ok(Value::Object(record))
}

fn generic_record(value: &Value) -> AppResult<GenericRecordIn> {
    serde_json::from_value(value.clone())
        .map_err(|error| AppError::validation(format!("invalid generic source record: {error}")))
}

fn validate_generic_source(
    graph_id: &str,
    vocab: &str,
    class: &str,
    object_id: &str,
    record: &Value,
    expected_kind: SourceKind,
) -> AppResult<()> {
    validate_stable_id(object_id, "objectId/eventId")?;
    let contract = get_vocabulary(vocab)
        .ok_or_else(|| AppError::validation(format!("unknown embedded vocabulary '{vocab}'")))?;
    let signature = contract
        .materialization_signature(class)
        .map_err(AppError::validation)?;
    if signature.source_kind != expected_kind {
        return Err(AppError::validation(format!(
            "{vocab}.{class} is {:?}, not {:?}",
            signature.source_kind, expected_kind
        )));
    }
    if expected_kind == SourceKind::Derived {
        return Err(AppError::validation(
            "derived Meaningful Objects are projections and cannot be authored",
        ));
    }
    let normalized = normalized_generic_record(class, object_id, record)?;
    let record = generic_record(&normalized)?;
    plan_generic_compute(contract, graph_id, &[record])
        .map_err(|error| AppError::validation(error.0))?;
    Ok(())
}

fn candidate_version(
    object_key: &str,
    operation_id: &str,
    base_version: &str,
    record: &Value,
) -> AppResult<String> {
    let record_digest = value_digest(record)?;
    Ok(sha256_bytes(
        format!("source-current-v1\0{object_key}\0{operation_id}\0{base_version}\0{record_digest}")
            .as_bytes(),
    ))
}

fn conflict_id(object_key: &str, base_version: &str, candidates: &[CurrentCandidate]) -> String {
    let versions = candidates
        .iter()
        .map(|candidate| candidate.source_version.as_str())
        .collect::<Vec<_>>()
        .join("\0");
    format!(
        "conflict-{}",
        &sha256_bytes(format!("{object_key}\0{base_version}\0{versions}").as_bytes())[..32]
    )
}

fn deterministic_candidate(
    strategy: ReconciliationStrategy,
    candidates: &[CurrentCandidate],
) -> (usize, bool) {
    match strategy {
        ReconciliationStrategy::CausalLww => {
            let mut ranked = candidates.iter().enumerate().collect::<Vec<_>>();
            ranked.sort_by(|(_, left), (_, right)| {
                (
                    left.causal_order.unwrap_or(i64::MIN),
                    left.client_id.as_deref().unwrap_or(""),
                    left.source_version.as_str(),
                )
                    .cmp(&(
                        right.causal_order.unwrap_or(i64::MIN),
                        right.client_id.as_deref().unwrap_or(""),
                        right.source_version.as_str(),
                    ))
            });
            let complete_clock = candidates.iter().all(|candidate| {
                candidate.causal_order.is_some()
                    && candidate
                        .client_id
                        .as_deref()
                        .is_some_and(|client_id| !client_id.is_empty())
            });
            (
                ranked.last().map(|(index, _)| *index).unwrap_or(0),
                complete_clock,
            )
        }
        ReconciliationStrategy::EvidenceWeighted => {
            let mut ranked = candidates.iter().enumerate().collect::<Vec<_>>();
            ranked.sort_by(|(_, left), (_, right)| {
                left.evidence_weight
                    .unwrap_or(f64::NEG_INFINITY)
                    .total_cmp(&right.evidence_weight.unwrap_or(f64::NEG_INFINITY))
                    .then_with(|| left.source_version.cmp(&right.source_version))
            });
            let winner = ranked.last().map(|(index, _)| *index).unwrap_or(0);
            let unique = ranked.len() < 2
                || ranked[ranked.len() - 1]
                    .1
                    .evidence_weight
                    .unwrap_or(f64::NEG_INFINITY)
                    != ranked[ranked.len() - 2]
                        .1
                        .evidence_weight
                        .unwrap_or(f64::NEG_INFINITY);
            (winner, unique)
        }
        ReconciliationStrategy::ProducerDirected
        | ReconciliationStrategy::Contested
        | ReconciliationStrategy::CodeBacked => (0, false),
    }
}

fn current_operations(ledger: &SourceLedger) -> AppResult<BTreeMap<String, Vec<CurrentCandidate>>> {
    let mut objects: BTreeMap<String, Vec<CurrentCandidate>> = BTreeMap::new();
    for operation in ledger.operations.values() {
        let SourceOperation::CurrentState {
            operation_id,
            vocab,
            class,
            object_id,
            base_version,
            record,
            causal_order,
            client_id,
            evidence_weight,
        } = &operation.operation
        else {
            continue;
        };
        let key = object_key(vocab, class, object_id);
        objects
            .entry(key.clone())
            .or_default()
            .push(CurrentCandidate {
                operation_id: operation_id.clone(),
                source_version: candidate_version(&key, operation_id, base_version, record)?,
                base_version: base_version.clone(),
                record: normalized_generic_record(class, object_id, record)?,
                causal_order: *causal_order,
                client_id: client_id.clone(),
                evidence_weight: *evidence_weight,
            });
    }
    for candidates in objects.values_mut() {
        candidates.sort_by(|left, right| left.source_version.cmp(&right.source_version));
    }
    Ok(objects)
}

fn fold_current_objects(ledger: &SourceLedger) -> AppResult<Vec<CurrentFold>> {
    let grouped = current_operations(ledger)?;
    let mut out = Vec::new();
    for (key, candidates) in grouped {
        let all_candidates = candidates.clone();
        let first = candidates
            .first()
            .ok_or_else(|| AppError::internal("current candidate group is unexpectedly empty"))?;
        let SourceOperation::CurrentState {
            vocab,
            class,
            object_id,
            ..
        } = &ledger.operations[&first.operation_id].operation
        else {
            unreachable!("candidate came from current-state operation")
        };
        let contract = get_vocabulary(vocab)
            .ok_or_else(|| AppError::validation(format!("unknown vocabulary '{vocab}'")))?;
        let signature = contract
            .materialization_signature(class)
            .map_err(AppError::validation)?;
        let strategy = signature.reconciliation_strategy;
        let strategy_label = reconciliation_strategy_label(strategy).to_string();
        let mut by_base: BTreeMap<String, Vec<CurrentCandidate>> = BTreeMap::new();
        for candidate in candidates {
            by_base
                .entry(candidate.base_version.clone())
                .or_default()
                .push(candidate);
        }
        for siblings in by_base.values_mut() {
            siblings.sort_by(|left, right| left.source_version.cmp(&right.source_version));
        }

        let mut version = ROOT_VERSION.to_string();
        let mut head: Option<CurrentCandidate> = None;
        let mut conflict = None;
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(version.clone()) {
                return Err(AppError::conflict(format!(
                    "current-state causal cycle for {key} at {version}"
                ))
                .with_code(app_error_codes::CAUSAL_CYCLE));
            }
            let Some(siblings) = by_base.get(&version) else {
                break;
            };
            if siblings.len() == 1 {
                let candidate = siblings[0].clone();
                version = candidate.source_version.clone();
                head = Some(candidate);
                continue;
            }

            let (winner_index, automatically_resolved) =
                deterministic_candidate(strategy, siblings);
            let winner = siblings[winner_index].clone();
            if automatically_resolved {
                version = winner.source_version.clone();
                head = Some(winner);
                continue;
            }
            let id = conflict_id(&key, &version, siblings);
            let resolution = ledger.operations.values().find_map(|operation| {
                let SourceOperation::ResolveCurrent {
                    operation_id,
                    object_key,
                    conflict_id,
                    chosen_operation_id,
                    record,
                } = &operation.operation
                else {
                    return None;
                };
                (object_key == &key && conflict_id == &id).then(|| {
                    (
                        operation_id.clone(),
                        chosen_operation_id.clone(),
                        record.clone(),
                    )
                })
            });
            if let Some((operation_id, chosen_operation_id, merged_record)) = resolution {
                let resolved_record = match (chosen_operation_id.as_deref(), merged_record) {
                    (Some(chosen), _) => siblings
                        .iter()
                        .find(|candidate| candidate.operation_id == chosen)
                        .map(|candidate| candidate.record.clone())
                        .ok_or_else(|| {
                            AppError::validation(format!(
                                "resolution {operation_id} chooses unknown candidate {chosen}"
                            ))
                        })?,
                    (None, Some(record)) => normalized_generic_record(class, object_id, &record)?,
                    (None, None) => {
                        return Err(AppError::validation(format!(
                            "resolution {operation_id} has neither chosenOperationId nor record"
                        )))
                    }
                };
                let resolved_version = candidate_version(
                    &key,
                    &operation_id,
                    &format!("resolve:{id}"),
                    &resolved_record,
                )?;
                head = Some(CurrentCandidate {
                    operation_id,
                    source_version: resolved_version.clone(),
                    base_version: format!("resolve:{id}"),
                    record: resolved_record,
                    causal_order: None,
                    client_id: None,
                    evidence_weight: None,
                });
                version = resolved_version;
                continue;
            }

            head = Some(winner.clone());
            conflict = Some(SyncConflict {
                conflict_id: id,
                object_key: key.clone(),
                base_version: version.clone(),
                reconciliation_strategy: strategy_label.clone(),
                reason: "concurrent current-state candidates share one observed base".to_string(),
                candidates: siblings
                    .iter()
                    .map(|candidate| CurrentCandidateFace {
                        operation_id: candidate.operation_id.clone(),
                        source_version: candidate.source_version.clone(),
                        base_version: candidate.base_version.clone(),
                        record: candidate.record.clone(),
                        client_id: candidate.client_id.clone(),
                        causal_order: candidate.causal_order,
                        evidence_weight: candidate.evidence_weight,
                    })
                    .collect(),
                projected_operation_id: winner.operation_id.clone(),
            });
            break;
        }

        let known_versions = all_candidates
            .iter()
            .map(|candidate| candidate.source_version.clone())
            .chain(std::iter::once(ROOT_VERSION.to_string()))
            .collect::<BTreeSet<_>>();
        let missing_base_candidates = all_candidates
            .iter()
            .filter(|candidate| !known_versions.contains(&candidate.base_version))
            .cloned()
            .collect::<Vec<_>>();
        if !missing_base_candidates.is_empty() {
            let projected = head
                .clone()
                .unwrap_or_else(|| missing_base_candidates[0].clone());
            let missing_bases = missing_base_candidates
                .iter()
                .map(|candidate| candidate.base_version.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join("\0");
            let id = format!(
                "conflict-{}",
                &sha256_bytes(format!("{key}\0missing-base\0{missing_bases}").as_bytes())[..32]
            );
            let mut missing_faces = missing_base_candidates
                .iter()
                .map(|candidate| CurrentCandidateFace {
                    operation_id: candidate.operation_id.clone(),
                    source_version: candidate.source_version.clone(),
                    base_version: candidate.base_version.clone(),
                    record: candidate.record.clone(),
                    client_id: candidate.client_id.clone(),
                    causal_order: candidate.causal_order,
                    evidence_weight: candidate.evidence_weight,
                })
                .collect::<Vec<_>>();
            if let Some(existing) = &mut conflict {
                existing
                    .reason
                    .push_str("; one or more candidates reference an unavailable base");
                existing.candidates.extend(missing_faces);
                existing
                    .candidates
                    .sort_by(|left, right| left.source_version.cmp(&right.source_version));
                existing
                    .candidates
                    .dedup_by(|left, right| left.source_version == right.source_version);
            } else {
                // D19 — `conflict` is None entering this branch, so `missing_faces`
                // (built purely from `missing_base_candidates`) is about to become
                // the WHOLE candidate list. When `projected` is a legitimate chain
                // head from OUTSIDE `missing_base_candidates` (an unrelated orphan
                // coexists for the same object key), it must be added here or
                // `projected_operation_id` names an operation absent from
                // `candidates`.
                if !missing_faces
                    .iter()
                    .any(|face| face.operation_id == projected.operation_id)
                {
                    missing_faces.push(CurrentCandidateFace {
                        operation_id: projected.operation_id.clone(),
                        source_version: projected.source_version.clone(),
                        base_version: projected.base_version.clone(),
                        record: projected.record.clone(),
                        client_id: projected.client_id.clone(),
                        causal_order: projected.causal_order,
                        evidence_weight: projected.evidence_weight,
                    });
                }
                conflict = Some(SyncConflict {
                    conflict_id: id,
                    object_key: key.clone(),
                    base_version: format!("missing:{missing_bases}"),
                    reconciliation_strategy: strategy_label.clone(),
                    reason: "one or more current-state candidates reference an unavailable base"
                        .to_string(),
                    candidates: missing_faces,
                    projected_operation_id: projected.operation_id.clone(),
                });
            }
            head = Some(projected);
        }

        let Some(head) = head else {
            continue;
        };
        out.push(CurrentFold {
            face: CurrentObjectFace {
                object_key: key,
                vocab: vocab.clone(),
                class: class.clone(),
                object_id: object_id.clone(),
                source_version: head.source_version,
                record: head.record,
                reconciliation_strategy: strategy_label,
                conflict_id: conflict
                    .as_ref()
                    .map(|conflict| conflict.conflict_id.clone()),
                operation_id: head.operation_id.clone(),
                client_id: head.client_id.clone(),
            },
            conflict,
        });
    }
    out.sort_by(|left, right| left.face.object_key.cmp(&right.face.object_key));
    Ok(out)
}

fn event_operations(ledger: &SourceLedger) -> Vec<&LedgerOperation> {
    let mut events = ledger
        .operations
        .values()
        .filter(|operation| matches!(operation.operation, SourceOperation::EventLog { .. }))
        .collect::<Vec<_>>();
    events.sort_by(|left, right| match (&left.operation, &right.operation) {
        (
            SourceOperation::EventLog {
                event_id: left_id, ..
            },
            SourceOperation::EventLog {
                event_id: right_id, ..
            },
        ) => left_id.cmp(right_id),
        _ => std::cmp::Ordering::Equal,
    });
    events
}

fn semantic_event_digest(operation: &SourceOperation) -> AppResult<Option<String>> {
    let value = match operation {
        SourceOperation::EventLog {
            event_id,
            vocab,
            class,
            record,
            ..
        } => json!({
            "kind": "eventLog",
            "eventId": event_id,
            "vocab": vocab,
            "class": class,
            "record": record,
        }),
        SourceOperation::Valuation {
            valuation_event_id,
            observer,
            document_id,
            block_id,
            importance,
            valence,
            tags,
            at_ms,
            ..
        } => json!({
            "kind": "valuation",
            "valuationEventId": valuation_event_id,
            "observer": observer,
            "documentId": document_id,
            "blockId": block_id,
            "importance": importance,
            "valence": valence,
            "tags": tags,
            "atMs": at_ms,
        }),
        SourceOperation::Retraction {
            retraction_event_id,
            subject,
            rationale,
            retraction_kind,
            observer,
            at_ms,
            ..
        } => json!({
            "kind": "retraction",
            "retractionEventId": retraction_event_id,
            "subject": subject,
            "rationale": rationale,
            "retractionKind": retraction_kind,
            "observer": observer,
            "atMs": at_ms,
        }),
        _ => return Ok(None),
    };
    value_digest(&value).map(Some)
}

fn validate_event_identity(
    ledger: &SourceLedger,
    operation: &SourceOperation,
    event_id: &str,
) -> AppResult<()> {
    let digest = semantic_event_digest(operation)?
        .ok_or_else(|| AppError::internal("event semantic digest called for non-event"))?;
    for existing in ledger.operations.values() {
        if let SourceOperation::EventLog {
            event_id: existing_id,
            ..
        } = &existing.operation
        {
            if existing_id == event_id
                && semantic_event_digest(&existing.operation)?.as_deref() != Some(&digest)
            {
                return Err(AppError::conflict(format!(
                    "eventId '{event_id}' was already accepted with different content"
                ))
                .with_code(app_error_codes::EVENT_IDENTITY_REUSED));
            }
        }
    }
    Ok(())
}

fn validate_valuation_identity(
    ledger: &SourceLedger,
    operation: &SourceOperation,
    event_id: &str,
) -> AppResult<()> {
    let digest = semantic_event_digest(operation)?
        .ok_or_else(|| AppError::internal("valuation digest called for non-valuation"))?;
    for existing in ledger.operations.values() {
        if let SourceOperation::Valuation {
            valuation_event_id: existing_id,
            ..
        } = &existing.operation
        {
            if existing_id == event_id
                && semantic_event_digest(&existing.operation)?.as_deref() != Some(&digest)
            {
                return Err(AppError::conflict(format!(
                    "valuationEventId '{event_id}' was already accepted with different content"
                ))
                .with_code(app_error_codes::EVENT_IDENTITY_REUSED));
            }
        }
    }
    Ok(())
}

fn validate_retraction_identity(
    ledger: &SourceLedger,
    operation: &SourceOperation,
    event_id: &str,
) -> AppResult<()> {
    let digest = semantic_event_digest(operation)?
        .ok_or_else(|| AppError::internal("retraction digest called for non-retraction"))?;
    for existing in ledger.operations.values() {
        if let SourceOperation::Retraction {
            retraction_event_id: existing_id,
            ..
        } = &existing.operation
        {
            if existing_id == event_id
                && semantic_event_digest(&existing.operation)?.as_deref() != Some(&digest)
            {
                return Err(AppError::conflict(format!(
                    "retractionEventId '{event_id}' was already accepted with different content"
                ))
                .with_code(app_error_codes::EVENT_IDENTITY_REUSED));
            }
        }
    }
    Ok(())
}

fn stable_outcome(operation: &SourceOperation, ledger: &SourceLedger) -> AppResult<Value> {
    match operation {
        SourceOperation::CurrentState { operation_id, .. } => {
            for folded in fold_current_objects(ledger)? {
                let mut candidate_version = None;
                if let Some(conflict) = &folded.conflict {
                    candidate_version = conflict
                        .candidates
                        .iter()
                        .find(|candidate| candidate.operation_id == *operation_id)
                        .map(|candidate| candidate.source_version.clone());
                }
                if candidate_version.is_none() {
                    let current = current_operations(ledger)?;
                    candidate_version = current
                        .get(&folded.face.object_key)
                        .and_then(|candidates| {
                            candidates
                                .iter()
                                .find(|candidate| candidate.operation_id == *operation_id)
                        })
                        .map(|candidate| candidate.source_version.clone());
                }
                if let Some(source_version) = candidate_version {
                    return Ok(json!({
                        "sourceVersion": source_version,
                        "objectKey": folded.face.object_key,
                        "currentSourceVersion": folded.face.source_version,
                        "conflictId": folded.face.conflict_id,
                        "outcome": if folded.face.conflict_id.is_some() { "conflict" } else { "applied" },
                    }));
                }
            }
            Ok(json!({ "outcome": "accepted" }))
        }
        SourceOperation::ResolveCurrent {
            object_key,
            conflict_id,
            ..
        } => {
            let fold = fold_current_objects(ledger)?
                .into_iter()
                .find(|fold| fold.face.object_key == *object_key)
                .ok_or_else(|| AppError::validation(format!("unknown objectKey '{object_key}'")))?;
            Ok(json!({
                "outcome": "resolved",
                "conflictId": conflict_id,
                "sourceVersion": fold.face.source_version,
                "remainingConflictId": fold.face.conflict_id,
            }))
        }
        SourceOperation::EventLog { event_id, .. } => {
            Ok(json!({ "outcome": "unioned", "eventId": event_id }))
        }
        SourceOperation::Memory { .. } => Ok(json!({ "outcome": "accepted" })),
        SourceOperation::Valuation {
            valuation_event_id, ..
        } => Ok(json!({ "outcome": "unioned", "valuationEventId": valuation_event_id })),
        SourceOperation::Retraction {
            retraction_event_id,
            ..
        } => Ok(json!({ "outcome": "unioned", "retractionEventId": retraction_event_id })),
        SourceOperation::DocumentLifecycle {
            action,
            document_id,
            ..
        } => Ok(json!({ "outcome": "accepted", "action": action, "documentId": document_id })),
        SourceOperation::WorkspaceUpdate { .. } => {
            Ok(json!({ "outcome": "accepted", "sourceKind": "ydoc", "room": "workspace" }))
        }
        SourceOperation::DocumentUpdate { document_id, .. } => Ok(json!({
            "outcome": "accepted",
            "sourceKind": "ydoc",
            "documentId": document_id,
        })),
        SourceOperation::CrdtCommand {
            command_kind,
            document_id,
            ..
        } => Ok(json!({
            "outcome": "accepted",
            "sourceKind": "crdt-command",
            "commandKind": command_kind,
            "documentId": document_id,
        })),
        SourceOperation::GraphMetadata {
            title, description, ..
        } => Ok(json!({
            "outcome": "accepted",
            "sourceKind": "current-state",
            "graphMetadata": {
                "title": title,
                "description": description,
            },
        })),
    }
}

fn validate_source_operation(
    graph_id: &str,
    ledger: &SourceLedger,
    operation: &SourceOperation,
    digest: &str,
) -> AppResult<()> {
    validate_stable_id(operation.operation_id(), "operationId")?;
    if let Some(existing) = ledger.operations.get(operation.operation_id()) {
        if existing.digest != digest {
            return Err(AppError::conflict(format!(
                "operationId '{}' was reused with different content",
                operation.operation_id()
            ))
            .with_code(app_error_codes::OPERATION_ID_REUSED));
        }
        return Ok(());
    }
    match operation {
        SourceOperation::CurrentState {
            vocab,
            class,
            object_id,
            base_version,
            record,
            evidence_weight,
            causal_order,
            ..
        } => {
            if base_version.trim().is_empty() {
                return Err(AppError::validation("baseVersion must not be empty"));
            }
            if evidence_weight.is_some_and(|weight| !weight.is_finite()) {
                return Err(AppError::validation("evidenceWeight must be finite"));
            }
            if causal_order.is_some_and(|order| {
                !(-9_007_199_254_740_991_i64..=9_007_199_254_740_991_i64).contains(&order)
            }) {
                return Err(AppError::validation(
                    "causalOrder must be within the JS safe-integer range (±(2^53−1))",
                ));
            }
            validate_generic_source(
                graph_id,
                vocab,
                class,
                object_id,
                record,
                SourceKind::CurrentState,
            )
        }
        SourceOperation::ResolveCurrent {
            object_key,
            conflict_id,
            chosen_operation_id,
            record,
            ..
        } => {
            if object_key.trim().is_empty() || conflict_id.trim().is_empty() {
                return Err(AppError::validation(
                    "resolveCurrent requires objectKey and conflictId",
                ));
            }
            if chosen_operation_id.is_some() == record.is_some() {
                return Err(AppError::validation(
                    "resolveCurrent requires exactly one of chosenOperationId or record",
                ));
            }
            let conflict_exists = fold_current_objects(ledger)?
                .iter()
                .filter_map(|fold| fold.conflict.as_ref())
                .any(|conflict| {
                    conflict.object_key == *object_key && conflict.conflict_id == *conflict_id
                });
            if !conflict_exists {
                return Err(AppError::conflict(format!(
                    "sync conflict '{conflict_id}' is not current for object '{object_key}'"
                ))
                .with_code(app_error_codes::STALE_SYNC_CONFLICT));
            }
            Ok(())
        }
        SourceOperation::EventLog {
            event_id,
            vocab,
            class,
            record,
            ..
        } => {
            validate_stable_id(event_id, "eventId")?;
            validate_event_identity(ledger, operation, event_id)?;
            validate_generic_source(
                graph_id,
                vocab,
                class,
                event_id,
                record,
                SourceKind::EventLog,
            )
        }
        SourceOperation::Memory {
            observer,
            publish,
            records,
            at_ms,
            ..
        } => {
            if *at_ms < 0 || records.is_empty() {
                return Err(AppError::validation(
                    "memory source requires non-empty records and non-negative atMs",
                ));
            }
            crate::emporium::schemas::IngestRequest::scan_unknown_memory_keys(&Value::Array(
                records.clone(),
            ))
            .map_err(AppError::validation)?;
            let typed = records
                .iter()
                .map(|record| {
                    serde_json::from_value::<MemoryRecordIn>(record.clone()).map_err(|error| {
                        AppError::validation(format!("invalid memory source record: {error}"))
                    })
                })
                .collect::<AppResult<Vec<_>>>()?;
            crate::emporium::schemas::IngestRequest::validate_memory(&typed)
                .map_err(AppError::validation)?;
            if !publish && observer.trim().is_empty() {
                return Err(AppError::validation(
                    "private memory source requires a non-empty observer",
                ));
            }
            for record in &typed {
                if *publish
                    && record
                        .observer_agent_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty())
                {
                    return Err(AppError::validation(
                        "published memory may not carry observerAgentId",
                    ));
                }
                if !publish
                    && record
                        .observer_agent_id
                        .as_deref()
                        .is_some_and(|id| id != observer)
                {
                    return Err(AppError::validation(
                        "memory observerAgentId must match the source observer",
                    ));
                }
            }
            Ok(())
        }
        SourceOperation::Valuation {
            valuation_event_id,
            document_id,
            block_id,
            importance,
            valence,
            tags,
            at_ms,
            ..
        } => {
            validate_stable_id(valuation_event_id, "valuationEventId")?;
            validate_valuation_identity(ledger, operation, valuation_event_id)?;
            if document_id.trim().is_empty() || block_id.trim().is_empty() || *at_ms < 0 {
                return Err(AppError::validation(
                    "valuation requires documentId, blockId, and non-negative atMs",
                ));
            }
            if importance.is_none() && valence.is_none() && tags.is_empty() {
                return Err(AppError::validation(
                    "valuation requires importance, valence, or tags",
                ));
            }
            if importance.is_some_and(|value| !(0..=5).contains(&value))
                || valence.is_some_and(|value| !(-5..=5).contains(&value))
            {
                return Err(AppError::validation(
                    "valuation importance must be 0..5 and valence -5..5",
                ));
            }
            Ok(())
        }
        SourceOperation::Retraction {
            retraction_event_id,
            subject,
            rationale,
            retraction_kind,
            at_ms,
            ..
        } => {
            validate_stable_id(retraction_event_id, "retractionEventId")?;
            crate::pdf_source::require_not_authored_subject(graph_id, subject)
                .map_err(AppError::validation)?;
            validate_retraction_identity(ledger, operation, retraction_event_id)?;
            NamedNode::new(subject)
                .map_err(|error| AppError::validation(format!("invalid subject IRI: {error}")))?;
            if rationale.trim().is_empty()
                || !matches!(retraction_kind.as_str(), "retract" | "archive")
                || *at_ms < 0
            {
                return Err(AppError::validation(
                    "retraction requires rationale, retract|archive kind, and non-negative atMs",
                ));
            }
            Ok(())
        }
        SourceOperation::DocumentLifecycle {
            action,
            document_id,
            title,
            new_document_incarnation,
            expected_document_incarnation,
            initial_update_base64,
            ..
        } => {
            crate::ids::validate_local_id(document_id, "documentId")
                .map_err(AppError::validation)?;
            if matches!(
                action,
                DocumentLifecycleAction::Create | DocumentLifecycleAction::Recreate
            ) && title.as_deref().is_none_or(str::is_empty)
            {
                return Err(AppError::validation(
                    "document create/recreate requires title",
                ));
            }
            if matches!(
                action,
                DocumentLifecycleAction::Delete | DocumentLifecycleAction::Recreate
            ) && expected_document_incarnation
                .as_deref()
                .is_none_or(str::is_empty)
            {
                return Err(AppError::validation(
                    "document delete/recreate requires expectedDocumentIncarnation",
                ));
            }
            if let Some(incarnation) = new_document_incarnation {
                if matches!(action, DocumentLifecycleAction::Delete) {
                    return Err(AppError::validation(
                        "document delete may not carry newDocumentIncarnation",
                    ));
                }
                uuid::Uuid::parse_str(incarnation)
                    .map_err(|_| AppError::validation("newDocumentIncarnation must be a UUID"))?;
            }
            if let Some(update) = initial_update_base64 {
                if matches!(action, DocumentLifecycleAction::Delete) {
                    return Err(AppError::validation(
                        "document delete may not carry initialUpdateBase64",
                    ));
                }
                decode_update(update, "document lifecycle initial update")?;
            }
            Ok(())
        }
        SourceOperation::WorkspaceUpdate { update_base64, .. } => {
            decode_update(update_base64, "workspace update").map(|_| ())
        }
        SourceOperation::DocumentUpdate {
            document_id,
            document_incarnation,
            update_base64,
            ..
        } => {
            crate::ids::validate_local_id(document_id, "documentId")
                .map_err(AppError::validation)?;
            if document_incarnation.trim().is_empty() {
                return Err(AppError::validation(
                    "documentUpdate requires documentIncarnation",
                ));
            }
            decode_update(update_base64, "document update").map(|_| ())
        }
        SourceOperation::CrdtCommand {
            operation_id,
            command_kind,
            document_id,
            payload,
        } => {
            const OFFLINE_COMMAND_KINDS: &[&str] = &[
                "document.write",
                "document.editComment",
                "block.insert",
                "block.update",
                "block.editText",
                "block.delete",
                "workspace.createDocument",
                "workspace.updateDocument",
                "workspace.deleteDocument",
                "workspace.createFolder",
                "workspace.updateFolder",
                "workspace.deleteFolder",
                "workspace.moveFolder",
                "workspace.moveDocuments",
                "workspace.putArtifact",
                "workspace.deleteArtifact",
                "workspace.createWire",
                "workspace.refreshWire",
                "workspace.deleteWire",
            ];
            if !OFFLINE_COMMAND_KINDS.contains(&command_kind.as_str()) {
                return Err(AppError::validation(format!(
                    "crdtCommand kind '{command_kind}' is not an offline-authoring command"
                )));
            }
            let object = payload
                .as_object()
                .ok_or_else(|| AppError::validation("crdtCommand payload must be a JSON object"))?;
            if let Some(payload_operation_id) = object
                .get("operationId")
                .or_else(|| object.get("operation_id"))
            {
                if payload_operation_id.as_str() != Some(operation_id) {
                    return Err(AppError::validation(
                        "crdtCommand payload operationId must match the source operationId",
                    ));
                }
            }
            if let Some(document_id) = document_id {
                crate::ids::validate_local_id(document_id, "documentId")
                    .map_err(AppError::validation)?;
            }
            if matches!(
                command_kind.as_str(),
                "document.write"
                    | "document.editComment"
                    | "block.insert"
                    | "block.update"
                    | "block.editText"
                    | "block.delete"
                    | "workspace.deleteDocument"
            ) && document_id.as_deref().is_none_or(str::is_empty)
            {
                return Err(AppError::validation(format!(
                    "crdtCommand {command_kind} requires documentId"
                )));
            }
            Ok(())
        }
        SourceOperation::GraphMetadata {
            title, description, ..
        } => {
            if title.is_none() && description.is_none() {
                return Err(AppError::validation(
                    "graphMetadata requires title or description",
                ));
            }
            if title
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(AppError::validation(
                    "graphMetadata title must not be empty",
                ));
            }
            Ok(())
        }
    }
}

fn decode_update(value: &str, label: &str) -> AppResult<Vec<u8>> {
    BASE64_STANDARD
        .decode(value)
        .map_err(|error| AppError::validation(format!("decode {label} base64: {error}")))
}

fn active_graph_identity(
    app: &AppHandle,
    graph_id: &str,
    expected_incarnation: Option<&str>,
) -> AppResult<(PathBuf, String)> {
    let (graph_dir, record) = read_graph_record_no_heal(app, graph_id)?;
    let incarnation = match record.incarnation_id {
        Some(incarnation) => incarnation,
        None if expected_incarnation.is_some() => {
            return Err(AppError::conflict(format!("stale graph incarnation for {graph_id}: actual missing"))
                .with_code(app_error_codes::STALE_GRAPH_INCARNATION));
        }
        None => ensure_graph_incarnation(app, graph_id)?,
    };
    if let Some(expected) = expected_incarnation {
        if expected != incarnation {
            return Err(AppError::conflict(format!(
                "stale graph incarnation for {graph_id}: expected {expected}, actual {incarnation}"
            ))
            .with_code(app_error_codes::STALE_GRAPH_INCARNATION));
        }
    }
    Ok((graph_dir, incarnation))
}

/// D21 — classify a [`crate::document_incarnation_store::DocumentIncarnationFault`]
/// into an [`AppError`]. Only the `Mismatch` arm (a genuine cross-writer
/// identity race) is coded `stale_document_incarnation`; `Fault` (an invalid
/// id, a corrupt sidecar UUID, a vanished directory, I/O) is a local
/// durability fault, never a remote lifecycle fact, and stays uncoded.
/// Factored out of its one call site so it is unit-testable without an
/// `AppHandle`.
fn document_incarnation_fault_to_app_error(
    fault: crate::document_incarnation_store::DocumentIncarnationFault,
    document_id: &str,
) -> AppError {
    use crate::document_incarnation_store::DocumentIncarnationFault;
    match fault {
        DocumentIncarnationFault::Mismatch { expected, actual } => AppError::conflict(format!(
            "document incarnation conflict for {document_id}: expected {expected}, actual {actual}"
        ))
        .with_code(app_error_codes::STALE_DOCUMENT_INCARNATION),
        DocumentIncarnationFault::Fault(message) => AppError::storage(message),
    }
}

fn desired_for_record(
    graph_id: &str,
    vocab: &str,
    class: &str,
    object_id: &str,
    record: &Value,
) -> AppResult<(&'static VocabularyContract, String, String, Vec<Triple>)> {
    let contract = get_vocabulary(vocab)
        .ok_or_else(|| AppError::validation(format!("unknown vocabulary '{vocab}'")))?;
    let signature = contract
        .materialization_signature(class)
        .map_err(AppError::validation)?;
    let normalized = normalized_generic_record(class, object_id, record)?;
    let plan = plan_generic_compute(contract, graph_id, &[generic_record(&normalized)?])
        .map_err(|error| AppError::validation(error.0))?;
    let class_type = contract.classes[class]
        .rdf_types
        .iter()
        .filter_map(|rdf_type| contract.expand(rdf_type).ok())
        .find(|rdf_type| rdf_type.starts_with(contract.primary_namespace()))
        .ok_or_else(|| {
            AppError::validation(format!(
                "{vocab}.{class} has no targetable primary rdf:type"
            ))
        })?;
    let target = match signature.store_target {
        "user:rdf" => user_rdf_graph_iri(graph_id),
        target if target.starts_with("projection:") => {
            format!("{}:{target}", graph_subject(graph_id))
        }
        target => {
            return Err(AppError::validation(format!(
                "{vocab}.{class} declares unsupported materialization target '{target}'"
            )))
        }
    };
    Ok((contract, class_type, target, plan.desired_inserts))
}

fn materialize_record(
    store: &oxigraph::store::Store,
    graph_id: &str,
    vocab: &str,
    class: &str,
    object_id: &str,
    record: &Value,
) -> AppResult<(usize, Vec<TripleWire>)> {
    let (contract, class_type, target, desired) =
        desired_for_record(graph_id, vocab, class, object_id, record)?;
    let subjects = desired
        .iter()
        .filter(|(_, predicate, object)| {
            predicate == RDF_TYPE && object.as_nt() == format!("<{class_type}>")
        })
        .map(|(subject, _, _)| subject.clone())
        .collect::<BTreeSet<_>>();
    if subjects.len() != 1 {
        return Err(AppError::validation(format!(
            "{vocab}.{class} source record minted {} primary subjects, expected exactly one",
            subjects.len()
        )));
    }
    let scope = ClassScope {
        placement: Placement::Named(target),
        key: SpanKey::Fixed {
            rdf_type: class_type,
        },
        graph_id_conjunct: None,
        subjects: Some(subjects),
    };
    let diff = reconcile_class_validated(store, &scope, &desired, Some(contract))
        .map_err(AppError::rdf)?;
    Ok((
        diff.op_count(),
        desired.iter().map(TripleWire::from_triple).collect(),
    ))
}

/// The conflict projection, partitioned by subject rdf:type — the partition
/// `reconcile_classes` requires (`emporium/reconcile.rs:318-324`: each
/// `(scope, desired)` pair must carry ONLY the triples typed by that scope's
/// class).
struct ConflictProjection {
    conflicts: Vec<Triple>,
    candidates: Vec<Triple>,
}

fn conflict_triples(graph_id: &str, conflicts: &[SyncConflict]) -> AppResult<ConflictProjection> {
    let mut conflict_triples = Vec::new();
    let mut candidate_triples = Vec::new();
    let uri = |value: &str| -> AppResult<Term> {
        Ok(Term::Uri(NamedNode::new(value).map_err(|error| {
            AppError::validation(format!("sync conflict URI: {error}"))
        })?))
    };
    let string = |value: &str| Term::Lit(Literal::new_simple_literal(value));
    let typed = |value: String, datatype_local: &str| -> AppResult<Term> {
        Ok(Term::Lit(Literal::new_typed_literal(
            value,
            NamedNode::new(format!("{XSD_NS}{datatype_local}")).map_err(|error| {
                AppError::validation(format!("sync conflict datatype: {error}"))
            })?,
        )))
    };
    for conflict in conflicts {
        let subject = format!(
            "{}:projection:sync-conflicts:{}",
            graph_subject(graph_id),
            conflict.conflict_id
        );
        conflict_triples.push((
            subject.clone(),
            RDF_TYPE.to_string(),
            uri(SYNC_CONFLICT_TYPE)?,
        ));
        conflict_triples.push((
            subject.clone(),
            format!("{SYNC_NS}conflictId"),
            string(&conflict.conflict_id),
        ));
        conflict_triples.push((
            subject.clone(),
            format!("{SYNC_NS}objectKey"),
            string(&conflict.object_key),
        ));
        conflict_triples.push((
            subject.clone(),
            format!("{SYNC_NS}baseVersion"),
            string(&conflict.base_version),
        ));
        conflict_triples.push((
            subject.clone(),
            format!("{SYNC_NS}reconciliationStrategy"),
            string(&conflict.reconciliation_strategy),
        ));
        conflict_triples.push((
            subject.clone(),
            format!("{SYNC_NS}reason"),
            string(&conflict.reason),
        ));
        conflict_triples.push((
            subject.clone(),
            format!("{SYNC_NS}projectedOperationId"),
            string(&conflict.projected_operation_id),
        ));
        conflict_triples.push((
            subject.clone(),
            format!("{SYNC_NS}candidateCount"),
            typed(conflict.candidates.len().to_string(), "long")?,
        ));

        let mut projected_candidate_subject: Option<String> = None;
        for candidate in &conflict.candidates {
            let candidate_subject = format!("{subject}:candidate:{}", candidate.source_version);
            if candidate.operation_id == conflict.projected_operation_id {
                projected_candidate_subject = Some(candidate_subject.clone());
            }
            conflict_triples.push((
                subject.clone(),
                format!("{SYNC_NS}candidate"),
                uri(&candidate_subject)?,
            ));
            candidate_triples.push((
                candidate_subject.clone(),
                RDF_TYPE.to_string(),
                uri(SYNC_CONFLICT_CANDIDATE_TYPE)?,
            ));
            candidate_triples.push((
                candidate_subject.clone(),
                format!("{SYNC_NS}candidateOperationId"),
                string(&candidate.operation_id),
            ));
            candidate_triples.push((
                candidate_subject.clone(),
                format!("{SYNC_NS}candidateVersion"),
                string(&candidate.source_version),
            ));
            candidate_triples.push((
                candidate_subject.clone(),
                format!("{SYNC_NS}candidateBaseVersion"),
                string(&candidate.base_version),
            ));
            candidate_triples.push((
                candidate_subject.clone(),
                format!("{SYNC_NS}candidateRecordDigest"),
                string(&value_digest(&candidate.record)?),
            ));
            if let Some(client_id) = &candidate.client_id {
                candidate_triples.push((
                    candidate_subject.clone(),
                    format!("{SYNC_NS}candidateClientId"),
                    string(client_id),
                ));
            }
            if let Some(causal_order) = candidate.causal_order {
                candidate_triples.push((
                    candidate_subject.clone(),
                    format!("{SYNC_NS}candidateCausalOrder"),
                    typed(causal_order.to_string(), "long")?,
                ));
            }
            if let Some(evidence_weight) = candidate.evidence_weight {
                // Full round-trip `f64` precision (D20) — NOT the `{:.6}`
                // display convention `terms.rs:231` uses for materialized
                // record fields. The winner is chosen by exact
                // `f64::total_cmp`; two distinct weights that agree to six
                // decimal places must not render as an identical literal.
                candidate_triples.push((
                    candidate_subject.clone(),
                    format!("{SYNC_NS}candidateEvidenceWeight"),
                    typed(evidence_weight.to_string(), "double")?,
                ));
            }
            candidate_triples.push((
                candidate_subject,
                format!("{SYNC_NS}conflict"),
                uri(&subject)?,
            ));
        }
        // D19 guarantees `projected_operation_id` names a member of
        // `candidates` server-side, so this always resolves — but the
        // `sync#projectedCandidate` edge should not silently dangle if that
        // invariant is ever violated, so only emit it when it truly does.
        if let Some(projected_subject) = projected_candidate_subject {
            conflict_triples.push((
                subject,
                format!("{SYNC_NS}projectedCandidate"),
                uri(&projected_subject)?,
            ));
        }
    }
    Ok(ConflictProjection {
        conflicts: conflict_triples,
        candidates: candidate_triples,
    })
}

fn materialize_conflicts(
    store: &oxigraph::store::Store,
    graph_id: &str,
    conflicts: &[SyncConflict],
) -> AppResult<usize> {
    let projection = conflict_triples(graph_id, conflicts)?;
    let placement = Placement::Named(format!(
        "{}:projection:sync-conflicts",
        graph_subject(graph_id)
    ));
    let scopes = [
        (
            ClassScope {
                placement: placement.clone(),
                key: SpanKey::Fixed {
                    rdf_type: SYNC_CONFLICT_TYPE.to_string(),
                },
                graph_id_conjunct: None,
                subjects: None,
            },
            projection.conflicts,
        ),
        (
            ClassScope {
                placement,
                key: SpanKey::Fixed {
                    rdf_type: SYNC_CONFLICT_CANDIDATE_TYPE.to_string(),
                },
                graph_id_conjunct: None,
                subjects: None,
            },
            projection.candidates,
        ),
    ];
    reconcile_classes(store, &scopes)
        .map(|diff| diff.op_count())
        .map_err(AppError::rdf)
}

fn parse_ntriple(value: &str) -> AppResult<Triple> {
    let store = oxigraph::store::Store::new()
        .map_err(|error| AppError::rdf(format!("open N-Triples parser store: {error}")))?;
    let line = if value.trim_end().ends_with('.') {
        format!("{}\n", value.trim())
    } else {
        format!("{} .\n", value.trim())
    };
    store
        .load_from_slice(RdfParser::from_format(RdfFormat::NTriples), line.as_bytes())
        .map_err(|error| AppError::validation(format!("invalid canonical RDF triple: {error}")))?;
    let quads = store
        .iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::rdf(format!("read parsed RDF triple: {error}")))?;
    if quads.len() != 1 {
        return Err(AppError::validation(format!(
            "expected exactly one RDF triple, parsed {}",
            quads.len()
        )));
    }
    let quad = &quads[0];
    let subject = match &quad.subject {
        oxigraph::model::NamedOrBlankNode::NamedNode(node) => node.as_str().to_string(),
        oxigraph::model::NamedOrBlankNode::BlankNode(_) => {
            return Err(AppError::validation(
                "workflow fold triples may not use blank-node subjects",
            ))
        }
    };
    Ok((
        subject,
        quad.predicate.as_str().to_string(),
        crate::emporium::survey::parse_term(&quad.object.to_string()),
    ))
}

fn workflow_fold_desired(ledger: &SourceLedger) -> AppResult<Vec<TripleWire>> {
    let mut composition = Vec::<(i64, String, Vec<String>, Vec<String>)>::new();
    for operation in event_operations(ledger) {
        let SourceOperation::EventLog {
            event_id,
            vocab,
            class,
            record,
            ..
        } = &operation.operation
        else {
            continue;
        };
        if vocab != "workflow" || class != "CompositionEvent" {
            continue;
        }
        let field = |names: &[&str]| names.iter().find_map(|name| record.get(*name)).cloned();
        let order = field(&["eventOrder", "wf:eventOrder"])
            .and_then(|value| value.as_i64())
            .unwrap_or(0);
        let values = |names: &[&str]| -> Vec<String> {
            match field(names) {
                Some(Value::Array(values)) => values
                    .into_iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect(),
                Some(Value::String(value)) => vec![value],
                _ => Vec::new(),
            }
        };
        composition.push((
            order,
            event_id.clone(),
            values(&["insertTriple", "wf:insertTriple"]),
            values(&["deleteTriple", "wf:deleteTriple"]),
        ));
    }
    composition.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    let mut folded = BTreeSet::<TripleWire>::new();
    for (_, _, inserts, deletes) in composition {
        for raw in deletes {
            folded.remove(&TripleWire::from_triple(&parse_ntriple(&raw)?));
        }
        for raw in inserts {
            folded.insert(TripleWire::from_triple(&parse_ntriple(&raw)?));
        }
    }
    Ok(folded.into_iter().collect())
}

fn materialize_workflow_fold(
    store: &oxigraph::store::Store,
    graph_id: &str,
    ledger: &mut SourceLedger,
) -> AppResult<usize> {
    let desired = workflow_fold_desired(ledger)?;
    let old = ledger
        .workflow_fold_triples
        .iter()
        .map(TripleWire::to_triple)
        .collect::<Vec<_>>();
    let next = desired
        .iter()
        .map(TripleWire::to_triple)
        .collect::<Vec<_>>();
    // Delete every previously-owned fold triple and insert the complete new
    // fold.  This intentionally repairs a projection that was externally
    // wiped even when its source epoch did not change.
    let diff = TripleDiff {
        removes: old,
        adds: next,
    };
    apply_diff(
        store,
        &Placement::Named(user_rdf_graph_iri(graph_id)),
        &diff,
    )
    .map_err(AppError::rdf)?;
    ledger.workflow_fold_triples = desired;
    Ok(diff.op_count())
}

fn memory_event_batches(
    ledger: &SourceLedger,
) -> AppResult<Vec<crate::emporium::memory_events::MemoryEventBatch>> {
    let mut source = ledger
        .operations
        .values()
        .filter_map(|operation| {
            let SourceOperation::Memory {
                operation_id,
                observer,
                publish,
                at_ms,
                records,
            } = &operation.operation
            else {
                return None;
            };
            Some((
                operation_id.clone(),
                observer.clone(),
                *publish,
                *at_ms,
                records.clone(),
            ))
        })
        .collect::<Vec<_>>();
    source.sort_by(|left, right| (left.3, left.0.as_str()).cmp(&(right.3, right.0.as_str())));
    source
        .into_iter()
        .enumerate()
        .map(|(seq, (_, observer, publish, at_ms, records))| {
            let mut typed = records
                .into_iter()
                .map(|record| {
                    serde_json::from_value::<MemoryRecordIn>(record).map_err(|error| {
                        AppError::validation(format!("invalid memory source record: {error}"))
                    })
                })
                .collect::<AppResult<Vec<_>>>()?;
            for record in &mut typed {
                if publish {
                    record.observer_agent_id = None;
                } else if record
                    .observer_agent_id
                    .as_deref()
                    .is_none_or(str::is_empty)
                {
                    record.observer_agent_id = Some(observer.clone());
                }
            }
            let effective_observer = if publish {
                String::new()
            } else {
                typed
                    .first()
                    .and_then(|record| record.observer_agent_id.clone())
                    .unwrap_or(observer)
            };
            Ok(crate::emporium::memory_events::MemoryEventBatch {
                seq: seq as u64,
                at_ms,
                observer: effective_observer,
                records: typed,
            })
        })
        .collect()
}

fn import_external_memory_events(
    app: &AppHandle,
    graph_id: &str,
    ledger: &mut SourceLedger,
) -> AppResult<usize> {
    let batches = crate::emporium::memory_events::read_memory_events(app, graph_id)
        .map_err(AppError::storage)?;
    let mut imported = 0usize;
    for batch in batches {
        let identity = json!({
            "seq": batch.seq,
            "atMs": batch.at_ms,
            "observer": &batch.observer,
            "records": &batch.records,
        });
        let operation_id = format!("legacy-memory-{}", &value_digest(&identity)?[..32]);
        if ledger.operations.contains_key(&operation_id) {
            continue;
        }
        let records = batch
            .records
            .iter()
            .map(|record| {
                serde_json::to_value(record).map_err(|error| {
                    AppError::serialization(format!("serialize legacy memory event: {error}"))
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        let operation = SourceOperation::Memory {
            operation_id: operation_id.clone(),
            observer: batch.observer.clone(),
            publish: batch.observer.is_empty(),
            at_ms: batch.at_ms,
            records,
        };
        let digest = operation_digest(&operation)?;
        ledger.revision = ledger.revision.saturating_add(1);
        ledger.operations.insert(
            operation_id,
            LedgerOperation {
                digest,
                accepted_revision: ledger.revision,
                status: ReceiptStatus::Applied,
                operation,
                outcome: json!({
                    "outcome": "imported",
                    "authority": "emporium-memory-event-log",
                    "legacySeq": batch.seq,
                }),
                effect_error: None,
            },
        );
        imported += 1;
    }
    Ok(imported)
}

fn clear_named_graph(store: &oxigraph::store::Store, iri: &str) -> AppResult<()> {
    oxigraph::sparql::SparqlEvaluator::new()
        .parse_update(&format!("CLEAR SILENT GRAPH <{iri}>"))
        .map_err(|error| AppError::rdf(format!("parse projection clear: {error}")))?
        .on_store(store)
        .execute()
        .map_err(|error| AppError::rdf(format!("execute projection clear: {error}")))
}

fn materialize_memory_sources(
    store: &oxigraph::store::Store,
    graph_id: &str,
    ledger: &SourceLedger,
) -> AppResult<Value> {
    let batches = memory_event_batches(ledger)?;
    let expected = crate::emporium::memory_events::project_memory_events(graph_id, &batches)
        .map_err(AppError::rdf)?;
    for (graph, triples) in &expected {
        clear_named_graph(store, graph)?;
        let diff = TripleDiff {
            removes: Vec::new(),
            adds: triples.clone(),
        };
        apply_diff(store, &Placement::Named(graph.clone()), &diff).map_err(AppError::rdf)?;
    }
    Ok(json!({
        "events": batches.len(),
        "graphs": expected.keys().collect::<Vec<_>>(),
        "triples": expected.values().map(Vec::len).sum::<usize>(),
    }))
}

fn valuation_events(
    ledger: &SourceLedger,
    after_revision: u64,
) -> Vec<crate::salience_mcp_valuation::SourceValuationEvent> {
    let mut by_event =
        BTreeMap::<String, (u64, crate::salience_mcp_valuation::SourceValuationEvent)>::new();
    for operation in ledger.operations.values() {
        let SourceOperation::Valuation {
            valuation_event_id,
            observer,
            document_id,
            block_id,
            importance,
            valence,
            tags,
            at_ms,
            ..
        } = &operation.operation
        else {
            continue;
        };
        let event = crate::salience_mcp_valuation::SourceValuationEvent {
            event_id: valuation_event_id.clone(),
            observer: observer.clone(),
            document_id: document_id.clone(),
            block_id: block_id.clone(),
            importance: *importance,
            valence: *valence,
            tags: tags.clone(),
            at_ms: *at_ms,
        };
        by_event
            .entry(valuation_event_id.clone())
            .and_modify(|(revision, existing)| {
                if operation.accepted_revision < *revision {
                    *revision = operation.accepted_revision;
                    *existing = event.clone();
                }
            })
            .or_insert((operation.accepted_revision, event));
    }
    let mut events = by_event
        .into_values()
        .filter(|(revision, _)| *revision > after_revision)
        .map(|(_, event)| event)
        .collect::<Vec<_>>();
    events.sort_by(|left, right| {
        (left.at_ms, left.event_id.as_str()).cmp(&(right.at_ms, right.event_id.as_str()))
    });
    events
}

async fn materialize_retractions(
    app: &AppHandle,
    graph_id: &str,
    ledger: &SourceLedger,
) -> AppResult<usize> {
    let mut events = ledger
        .operations
        .values()
        .filter_map(|operation| {
            let SourceOperation::Retraction {
                retraction_event_id,
                subject,
                rationale,
                retraction_kind,
                observer,
                at_ms,
                ..
            } = &operation.operation
            else {
                return None;
            };
            Some((
                retraction_event_id,
                subject,
                rationale,
                retraction_kind,
                observer.as_deref(),
                *at_ms,
            ))
        })
        .collect::<Vec<_>>();
    events.sort_by(|left, right| left.0.cmp(right.0));
    for (event_id, subject, rationale, kind, observer, at_ms) in &events {
        crate::emporium::write::emporium_retract_with_identity(
            app,
            graph_id,
            subject,
            rationale,
            kind,
            *observer,
            Some(event_id),
            Some(*at_ms),
        )
        .await
        .map_err(|error| AppError::internal(error.message()))?;
    }
    Ok(events.len())
}

async fn rebuild_source_projections(
    app: &AppHandle,
    graph_id: &str,
    ledger: &mut SourceLedger,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let seed_replay = crate::rdf_seed_service::begin_seed_replay(&graph_dir).map_err(AppError::rdf)?;
    let checkpoint_revision = ledger
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.ledger_revision)
        .unwrap_or(0);
    if let Some(checkpoint) = &ledger.checkpoint {
        restore_checkpoint(graph_id, &graph_dir, &store, checkpoint)?;
    } else {
        seed_replay.invalidate().map_err(AppError::rdf)?;
    }
    let cleared_ydoc_projection_graphs = clear_ydoc_projection_graphs(&store, graph_id)?;
    drop(store);
    let ydoc = crate::crdt_engine::flush_ops::rebuild_all_ydoc_projections(
        app,
        graph_id,
        &graph_dir,
        &format!("source-rebuild-{}", ledger.revision),
    )
    .await
    .map_err(AppError::storage)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let folds = fold_current_objects(ledger)?;
    let mut current_operations = 0usize;
    let mut expected = BTreeSet::<TripleWire>::new();
    let conflicts = folds
        .iter()
        .filter_map(|fold| fold.conflict.clone())
        .collect::<Vec<_>>();
    for fold in &folds {
        let (operations, desired) = materialize_record(
            &store,
            graph_id,
            &fold.face.vocab,
            &fold.face.class,
            &fold.face.object_id,
            &fold.face.record,
        )?;
        current_operations += operations;
        expected.extend(desired);
    }

    let mut event_operations_applied = 0usize;
    let mut seen_events = BTreeSet::<String>::new();
    for operation in event_operations(ledger) {
        let SourceOperation::EventLog {
            event_id,
            vocab,
            class,
            record,
            ..
        } = &operation.operation
        else {
            continue;
        };
        if !seen_events.insert(event_id.clone()) {
            continue;
        }
        let (operations, desired) =
            materialize_record(&store, graph_id, vocab, class, event_id, record)?;
        event_operations_applied += operations;
        expected.extend(desired);
    }
    let conflict_operations = materialize_conflicts(&store, graph_id, &conflicts)?;
    let workflow_fold_operations = materialize_workflow_fold(&store, graph_id, ledger)?;
    let memory = materialize_memory_sources(&store, graph_id, ledger)?;
    let valuation = crate::salience_mcp_valuation::rebuild_value_store_from_source_events(
        app,
        graph_id,
        &valuation_events(ledger, checkpoint_revision),
        ledger.checkpoint.is_none(),
    )
    .map_err(AppError::internal)?;
    let retractions = materialize_retractions(app, graph_id, ledger).await?;
    // The graph record is a durable current-state authority outside Oxigraph.
    // Checkpoint restore intentionally rewinds the projection store, so finish
    // every replay by deriving this face from the current record. Do this last:
    // legacy retraction helpers may update graph metadata clocks.
    let (_, graph_record) = read_graph_record_no_heal(app, graph_id)?;
    let graph_record_diff =
        crate::rdf_record_materializer::reconcile_graph_record(&store, &graph_record)
            .map_err(AppError::rdf)?;
    crate::cell_durability::mark_rdf_store_written(&store);
    // Checkpoint replacement invalidated the process marker. Complete the
    // ordinary canonical seed reconciliation before this replay is successful,
    // so a fresh process will observe the same complete projection and epoch.
    seed_replay.finish().map_err(AppError::rdf)?;
    Ok(json!({
        "currentObjects": folds.len(),
        "currentRdfOperations": current_operations,
        "events": seen_events.len(),
        "eventRdfOperations": event_operations_applied,
        "conflicts": conflicts.len(),
        "conflictRdfOperations": conflict_operations,
        "workflowFoldRdfOperations": workflow_fold_operations,
        "clearedYdocProjectionGraphs": cleared_ydoc_projection_graphs,
        "ydoc": ydoc,
        "memory": memory,
        "valuation": valuation,
        "retractions": retractions,
        "graphRecordRdfOperations": graph_record_diff.op_count(),
        "expectedGenericTripleHash": value_digest(&serde_json::to_value(expected)
            .map_err(|error| AppError::serialization(format!("serialize expected triples: {error}")))?)?,
    }))
}

async fn validate_live_operation_identity(
    app: &AppHandle,
    graph_id: &str,
    ledger: &SourceLedger,
    operation: &SourceOperation,
) -> AppResult<()> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    match operation {
        SourceOperation::DocumentLifecycle {
            action: DocumentLifecycleAction::Create,
            document_id,
            ..
        } => {
            let document_dir = crate::document_paths::document_dir(&graph_dir, document_id)
                .map_err(AppError::validation)?;
            if document_dir.join("document.json").is_file() {
                return Err(AppError::conflict(format!(
                    "document {document_id} already exists"
                ))
                .with_code(app_error_codes::DOCUMENT_EXISTS));
            }
            if crate::document_tombstone_store::document_is_tombstoned(&graph_dir, document_id)
                .map_err(AppError::storage)?
            {
                return Err(AppError::conflict(format!(
                    "document {document_id} is tombstoned; use recreate with the prior document incarnation"
                ))
                .with_code(app_error_codes::DOCUMENT_TOMBSTONED));
            }
        }
        SourceOperation::DocumentUpdate {
            document_id,
            document_incarnation,
            ..
        } => {
            let actual = ensure_document_incarnation_id(&graph_dir, document_id)
                .map_err(AppError::storage)?;
            if actual != *document_incarnation {
                return Err(AppError::conflict(format!(
                    "stale document incarnation for {document_id}: expected {document_incarnation}, actual {actual}"
                ))
                .with_code(app_error_codes::STALE_DOCUMENT_INCARNATION));
            }
        }
        SourceOperation::DocumentLifecycle {
            action: DocumentLifecycleAction::Delete,
            document_id,
            expected_document_incarnation,
            ..
        } => {
            let actual = ensure_document_incarnation_id(&graph_dir, document_id)
                .map_err(AppError::storage)?;
            if expected_document_incarnation.as_deref() != Some(actual.as_str()) {
                return Err(AppError::conflict(format!(
                    "stale document incarnation for {document_id}"
                ))
                .with_code(app_error_codes::STALE_DOCUMENT_INCARNATION));
            }
        }
        SourceOperation::DocumentLifecycle {
            action: DocumentLifecycleAction::Recreate,
            document_id,
            expected_document_incarnation,
            ..
        } => {
            let expected = expected_document_incarnation
                .as_deref()
                .ok_or_else(|| AppError::validation("recreate requires an expected incarnation"))?;
            let manifest = crate::document_paths::document_dir(&graph_dir, document_id)
                .map_err(AppError::validation)?
                .join("document.json");
            if manifest.is_file() {
                let actual = ensure_document_incarnation_id(&graph_dir, document_id)
                    .map_err(AppError::storage)?;
                if actual != expected {
                    return Err(AppError::conflict(format!(
                        "stale document incarnation for {document_id}"
                    ))
                    .with_code(app_error_codes::STALE_DOCUMENT_INCARNATION));
                }
            } else {
                let tombstone = crate::document_tombstone_store::read_document_tombstone(
                    &graph_dir,
                    document_id,
                )
                .map_err(AppError::storage)?
                .ok_or_else(|| {
                    AppError::conflict(format!(
                        "document {document_id} is neither live nor tombstoned"
                    ))
                    .with_code(app_error_codes::RECREATE_BOUNDARY_MISMATCH)
                })?;
                let deletion_operation_id =
                    tombstone.deletion_operation_id.as_deref().ok_or_else(|| {
                        AppError::conflict(format!(
                            "document {document_id} was deleted outside the source ledger"
                        ))
                        .with_code(app_error_codes::RECREATE_BOUNDARY_MISMATCH)
                    })?;
                let deletion_matches = ledger.operations.get(deletion_operation_id).is_some_and(
                    |record| {
                        matches!(
                            &record.operation,
                            SourceOperation::DocumentLifecycle {
                                action: DocumentLifecycleAction::Delete,
                                document_id: deleted_document_id,
                                expected_document_incarnation: Some(deleted_incarnation),
                                ..
                            } if deleted_document_id == document_id && deleted_incarnation == expected
                        )
                    },
                );
                if !deletion_matches {
                    return Err(AppError::conflict(format!(
                        "recreate does not match the source-ledger deletion boundary for {document_id}"
                    ))
                    .with_code(app_error_codes::RECREATE_BOUNDARY_MISMATCH));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn apply_ydoc_update(
    app: &AppHandle,
    graph_id: &str,
    document_id: Option<&str>,
    update: &[u8],
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let registry = app.state::<RoomRegistry>();
    let (key, path) = match document_id {
        Some(document_id) => (
            format!("doc:{graph_id}:{document_id}"),
            document_ydoc_state_path(&graph_dir, document_id),
        ),
        None => (
            format!("workspace:{graph_id}"),
            workspace_ydoc_state_path(&graph_dir),
        ),
    };
    let room = registry
        .get_or_create(&key, path)
        .await
        .map_err(AppError::storage)?;
    room.configure_projection_flush(graph_id.to_string(), document_id.map(str::to_string))
        .map_err(AppError::internal)?;
    let changed = room
        .apply_client_update(update)
        .await
        .map_err(AppError::storage)?;
    room.schedule_projection_flush(app.clone());
    Ok(json!({ "changed": changed }))
}

async fn apply_lifecycle_operation(
    app: &AppHandle,
    graph_id: &str,
    graph_incarnation: &str,
    operation: &SourceOperation,
) -> AppResult<Value> {
    let SourceOperation::DocumentLifecycle {
        operation_id,
        action,
        document_id,
        title,
        new_document_incarnation,
        initial_update_base64,
        ..
    } = operation
    else {
        return Err(AppError::internal("expected document lifecycle operation"));
    };
    let enqueue =
        |kind: &str, child_operation_id: String, payload: Value, document: Option<String>| {
            crate::crdt_queue::enqueue_crdt_operation(
                app.clone(),
                crate::crdt_queue::EnqueueCrdtOperationInput {
                    kind: kind.to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: document,
                    payload: {
                        let mut payload = payload;
                        if let Some(object) = payload.as_object_mut() {
                            object.insert("operationId".to_string(), json!(child_operation_id));
                            object.insert("graphIncarnation".to_string(), json!(graph_incarnation));
                        }
                        payload
                    },
                },
            )
        };
    let empty_document = || {
        json!({
            "documentId": document_id,
            "title": title.as_deref().unwrap_or("Untitled"),
            "tiptapJson": {
                "type": "doc",
                "content": [],
            },
        })
    };
    let queue_result = match action {
        DocumentLifecycleAction::Create => enqueue(
            "document.write",
            operation_id.clone(),
            empty_document(),
            Some(document_id.clone()),
        )
        .await
        .map_err(AppError::internal)?,
        DocumentLifecycleAction::Delete => enqueue(
            "workspace.deleteDocument",
            operation_id.clone(),
            json!({ "documentId": document_id }),
            Some(document_id.clone()),
        )
        .await
        .map_err(AppError::internal)?,
        DocumentLifecycleAction::Recreate => {
            let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
            if !crate::document_tombstone_store::document_is_tombstoned(&graph_dir, document_id)
                .map_err(AppError::storage)?
            {
                enqueue(
                    "workspace.deleteDocument",
                    format!("{operation_id}:delete"),
                    json!({ "documentId": document_id }),
                    Some(document_id.clone()),
                )
                .await
                .map_err(AppError::internal)?;
            }
            enqueue(
                "document.write",
                format!("{operation_id}:create"),
                empty_document(),
                Some(document_id.clone()),
            )
            .await
            .map_err(AppError::internal)?
        }
    };
    if matches!(action, DocumentLifecycleAction::Delete) {
        return Ok(json!({
            "queue": queue_result,
            "documentId": document_id,
            "deleted": true,
        }));
    }

    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let lease = coordinator
        .acquire_hot_write(graph_id)
        .await
        .map_err(AppError::internal)?;
    let (graph_dir, _) = active_graph_identity(app, graph_id, Some(graph_incarnation))?;
    let document_incarnation =
        crate::document_incarnation_store::ensure_document_incarnation_id_with_requested(
            &graph_dir,
            document_id,
            new_document_incarnation.as_deref(),
        )
        .map_err(|fault| document_incarnation_fault_to_app_error(fault, document_id))?;
    let initial_update = match initial_update_base64 {
        Some(update) => {
            let update = decode_update(update, "document lifecycle initial update")?;
            Some(apply_ydoc_update(app, graph_id, Some(document_id), &update).await?)
        }
        None => None,
    };
    let registry = app.state::<RoomRegistry>();
    let room = registry
        .get_or_create(
            &format!("doc:{graph_id}:{document_id}"),
            document_ydoc_state_path(&graph_dir, document_id),
        )
        .await
        .map_err(AppError::storage)?;
    room.configure_projection_flush(graph_id.to_string(), Some(document_id.clone()))
        .map_err(AppError::internal)?;
    room.force_projection_rebuild();
    let projection_tail = crate::crdt_engine::document_ops::flush_room_document_if_dirty(
        app,
        graph_id,
        document_id,
        title.as_deref().unwrap_or("Untitled"),
        &room,
        operation_id,
    )
    .await
    .map_err(AppError::storage)?;
    drop(lease);
    Ok(json!({
        "queue": queue_result,
        "documentId": document_id,
        "documentIncarnation": document_incarnation,
        "initialUpdate": initial_update,
        "projectionTail": projection_tail,
    }))
}

async fn apply_individual_effect(
    app: &AppHandle,
    graph_id: &str,
    graph_incarnation: &str,
    operation: &SourceOperation,
) -> AppResult<Value> {
    match operation {
        SourceOperation::DocumentLifecycle { .. } => {
            apply_lifecycle_operation(app, graph_id, graph_incarnation, operation).await
        }
        SourceOperation::WorkspaceUpdate { update_base64, .. } => {
            let coordinator = app.state::<GraphPersistenceCoordinator>();
            let lease = coordinator
                .acquire_hot_write(graph_id)
                .await
                .map_err(AppError::internal)?;
            active_graph_identity(app, graph_id, Some(graph_incarnation))?;
            let update = decode_update(update_base64, "workspace update")?;
            let result = apply_ydoc_update(app, graph_id, None, &update).await?;
            let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
            let registry = app.state::<RoomRegistry>();
            let room = registry
                .peek(&format!("workspace:{graph_id}"))
                .await
                .ok_or_else(|| AppError::internal("workspace source room disappeared"))?;
            let projection_tail =
                crate::crdt_engine::workspace_ops::ensure_workspace_source_effect_persisted(
                    app,
                    graph_id,
                    &graph_dir,
                    &room,
                    operation.operation_id(),
                )
                .await
                .map_err(AppError::storage)?;
            drop(lease);
            Ok(json!({ "merge": result, "projectionTailChanged": projection_tail }))
        }
        SourceOperation::DocumentUpdate {
            document_id,
            document_incarnation,
            update_base64,
            ..
        } => {
            let coordinator = app.state::<GraphPersistenceCoordinator>();
            let lease = coordinator
                .acquire_hot_write(graph_id)
                .await
                .map_err(AppError::internal)?;
            let (graph_dir, _) = active_graph_identity(app, graph_id, Some(graph_incarnation))?;
            let actual = ensure_document_incarnation_id(&graph_dir, document_id)
                .map_err(AppError::storage)?;
            if actual != *document_incarnation {
                return Err(AppError::conflict(format!(
                    "stale document incarnation for {document_id}"
                ))
                .with_code(app_error_codes::STALE_DOCUMENT_INCARNATION));
            }
            let update = decode_update(update_base64, "document update")?;
            let result = apply_ydoc_update(app, graph_id, Some(document_id), &update).await?;
            let registry = app.state::<RoomRegistry>();
            let room = registry
                .peek(&format!("doc:{graph_id}:{document_id}"))
                .await
                .ok_or_else(|| AppError::internal("document source room disappeared"))?;
            room.force_projection_rebuild();
            let title = read_graph_documents_cold(&graph_dir)
                .map_err(AppError::storage)?
                .into_iter()
                .find(|record| record.document_id == *document_id)
                .map(|record| record.title)
                .unwrap_or_else(|| "Untitled".to_string());
            let projection_tail = crate::crdt_engine::document_ops::flush_room_document_if_dirty(
                app,
                graph_id,
                document_id,
                &title,
                &room,
                operation.operation_id(),
            )
            .await
            .map_err(AppError::storage)?;
            drop(lease);
            Ok(json!({ "merge": result, "projectionTail": projection_tail }))
        }
        SourceOperation::CrdtCommand {
            operation_id,
            command_kind,
            document_id,
            payload,
        } => {
            active_graph_identity(app, graph_id, Some(graph_incarnation))?;
            let mut payload = payload.clone();
            let object = payload
                .as_object_mut()
                .ok_or_else(|| AppError::validation("crdtCommand payload must be a JSON object"))?;
            object.insert("operationId".to_string(), json!(operation_id));
            object.insert("graphIncarnation".to_string(), json!(graph_incarnation));
            let outcome = crate::crdt_queue::enqueue_crdt_operation_outcome(
                app.clone(),
                crate::crdt_queue::EnqueueCrdtOperationInput {
                    kind: command_kind.clone(),
                    graph_id: graph_id.to_string(),
                    document_id: document_id.clone(),
                    payload,
                },
            )
            .await
            .map_err(AppError::internal)?;
            Ok(json!({
                "commandKind": command_kind,
                "operationId": outcome.operation_id,
                "value": outcome.value,
            }))
        }
        SourceOperation::GraphMetadata {
            title, description, ..
        } => {
            active_graph_identity(app, graph_id, Some(graph_incarnation))?;
            let graph = crate::graph_service::update_graph_metadata_service_async(
                app,
                graph_id.to_string(),
                crate::graph_service::UpdateGraphMetadataInput {
                    title: title.clone(),
                    description: description.clone(),
                },
            )
            .await?;
            Ok(json!({
                "graphId": graph_id,
                "title": graph.title,
                "description": graph.description,
            }))
        }
        _ => Err(AppError::internal(
            "non-individual source operation reached individual effect",
        )),
    }
}

async fn repair_individual_effects(
    app: &AppHandle,
    graph_id: &str,
    graph_incarnation: &str,
    graph_dir: &Path,
    ledger: &mut SourceLedger,
) -> AppResult<Value> {
    let mut pending = ledger
        .operations
        .iter()
        .filter(|(_, record)| {
            record.status == ReceiptStatus::Accepted
                && record.operation.requires_individual_effect()
        })
        .map(|(id, record)| {
            (
                record.accepted_revision,
                id.clone(),
                record.operation.clone(),
            )
        })
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));

    let mut applied = Vec::new();
    let mut failed = Vec::new();
    for (_, operation_id, operation) in pending {
        match apply_individual_effect(app, graph_id, graph_incarnation, &operation).await {
            Ok(effect) => {
                let record = ledger
                    .operations
                    .get_mut(&operation_id)
                    .expect("accepted operation remains");
                record.status = ReceiptStatus::Applied;
                record.effect_error = None;
                if let Some(outcome) = record.outcome.as_object_mut() {
                    outcome.insert("effect".to_string(), effect);
                }
                applied.push(operation_id);
            }
            Err(error) => {
                ledger
                    .operations
                    .get_mut(&operation_id)
                    .expect("accepted operation remains")
                    .effect_error = Some(error.to_string());
                failed.push(json!({
                    "operationId": operation_id,
                    "error": error.to_string(),
                }));
            }
        }
        write_ledger(graph_dir, ledger)?;
    }
    Ok(json!({
        "applied": applied,
        "failed": failed,
    }))
}

fn receipt_from(
    operation_id: &str,
    operation: &LedgerOperation,
    duplicate: bool,
) -> OperationReceipt {
    OperationReceipt {
        operation_id: operation_id.to_string(),
        digest: operation.digest.clone(),
        accepted_revision: operation.accepted_revision,
        status: operation.status,
        duplicate,
        outcome: operation.outcome.clone(),
        effect_error: operation.effect_error.clone(),
    }
}

/// Deterministic crash seam for the distributed-truth harness. The variable
/// names one exact operation ID; the process aborts only after the atomic
/// ledger commit and before any individual/projection effect. It is compiled
/// only into headless cells and is inert unless explicitly set.
#[cfg(feature = "headless")]
fn crash_after_commit_if_requested(operations: &[SourceOperation]) {
    let Ok(target) = std::env::var("GARDEN_SOURCE_SYNC_TEST_CRASH_AFTER_COMMIT_OPERATION_ID")
    else {
        return;
    };
    if operations
        .iter()
        .any(|operation| operation.operation_id() == target)
    {
        std::process::abort();
    }
}

pub(crate) async fn mcp_local_source_push(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let input: SourcePushInput = serde_json::from_value(arguments.clone())
        .map_err(|error| AppError::validation(format!("source_push input: {error}")))?;
    if input.operations.is_empty() || input.operations.len() > MAX_SOURCE_BATCH {
        return Err(AppError::validation(format!(
            "source_push requires 1..={MAX_SOURCE_BATCH} operations"
        )));
    }
    let _source_gate = acquire_source_gate(&input.graph_id).await;
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let identity_lease = coordinator
        .acquire_lifecycle_shared(&input.graph_id)
        .await
        .map_err(AppError::internal)?;
    let (graph_dir, graph_incarnation) =
        active_graph_identity(&app, &input.graph_id, Some(&input.graph_incarnation))?;
    let mut ledger = read_ledger(&graph_dir, &input.graph_id, &graph_incarnation)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let checkpoint_store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    ensure_checkpoint(&graph_dir, &checkpoint_store, &mut ledger)?;
    drop(checkpoint_store);
    import_external_memory_events(&app, &input.graph_id, &mut ledger)?;
    write_ledger(&graph_dir, &ledger)?;

    let mut staged = ledger.clone();
    let mut duplicates = BTreeSet::new();
    for operation in &input.operations {
        let digest = operation_digest(operation)?;
        validate_source_operation(&input.graph_id, &staged, operation, &digest)?;
        if staged.operations.contains_key(operation.operation_id()) {
            duplicates.insert(operation.operation_id().to_string());
            continue;
        }
        validate_live_operation_identity(&app, &input.graph_id, &staged, operation).await?;
        staged.revision = staged.revision.saturating_add(1);
        staged.operations.insert(
            operation.operation_id().to_string(),
            LedgerOperation {
                digest,
                accepted_revision: staged.revision,
                status: ReceiptStatus::Accepted,
                operation: operation.clone(),
                outcome: json!({ "outcome": "accepted" }),
                effect_error: None,
            },
        );
        let outcome = stable_outcome(operation, &staged)?;
        staged
            .operations
            .get_mut(operation.operation_id())
            .expect("just inserted")
            .outcome = outcome;
    }
    // Atomic source commit: after this write succeeds, every new intent is
    // durable even if the process dies before any projection effect.
    write_ledger(&graph_dir, &staged)?;
    ledger = staged;
    drop(identity_lease);
    #[cfg(feature = "headless")]
    crash_after_commit_if_requested(&input.operations);

    // Lifecycle and Y.Doc effects acquire their own graph authority and may
    // safely await the CRDT executor. Repair every accepted effect, not only
    // operations repeated in this request, so a pull/retry after process death
    // resumes the exact durable intent.
    let individual_repair = repair_individual_effects(
        &app,
        &input.graph_id,
        &graph_incarnation,
        &graph_dir,
        &mut ledger,
    )
    .await?;

    let projection_lease = coordinator
        .acquire_hot_write(&input.graph_id)
        .await
        .map_err(AppError::internal)?;
    active_graph_identity(&app, &input.graph_id, Some(&graph_incarnation))?;
    let projection_result = rebuild_source_projections(&app, &input.graph_id, &mut ledger).await;
    match &projection_result {
        Ok(_) => {
            for record in ledger.operations.values_mut() {
                if !record.operation.requires_individual_effect() {
                    record.status = ReceiptStatus::Applied;
                    record.effect_error = None;
                }
            }
        }
        Err(error) => {
            for record in ledger.operations.values_mut() {
                if !record.operation.requires_individual_effect()
                    && record.status == ReceiptStatus::Accepted
                {
                    record.effect_error = Some(error.to_string());
                }
            }
        }
    }
    write_ledger(&graph_dir, &ledger)?;
    drop(projection_lease);

    let receipts = input
        .operations
        .iter()
        .map(|operation| {
            let id = operation.operation_id();
            receipt_from(
                id,
                ledger.operations.get(id).expect("operation was accepted"),
                duplicates.contains(id),
            )
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "ok": receipts.iter().all(|receipt| receipt.status == ReceiptStatus::Applied),
        "graphId": input.graph_id,
        "graphIncarnation": graph_incarnation,
        "revision": ledger.revision,
        "receipts": receipts,
        "projection": projection_result.ok(),
        "individualRepair": individual_repair,
    }))
}

fn source_argument_string(arguments: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        arguments
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn source_invocation_id(arguments: &Value, prefix: &str) -> AppResult<String> {
    let id = source_argument_string(
        arguments,
        &[
            "sourceOperationId",
            "source_operation_id",
            "operationId",
            "operation_id",
        ],
    )
    .unwrap_or_else(|| format!("{prefix}-{}", Uuid::new_v4()));
    validate_stable_id(&id, "operationId")?;
    Ok(id)
}

fn source_child_operation_id(base: &str, index: usize, total: usize) -> String {
    if total == 1 {
        return base.to_string();
    }
    let suffix = format!(":{index}");
    if base.len() + suffix.len() <= MAX_OPERATION_ID_LEN {
        format!("{base}{suffix}")
    } else {
        format!("batch-{}:{index}", &sha256_bytes(base.as_bytes())[..32])
    }
}

fn source_now_ms(arguments: &Value) -> AppResult<i64> {
    if let Some(value) = arguments
        .get("atMs")
        .or_else(|| arguments.get("at_ms"))
        .and_then(Value::as_i64)
    {
        if value < 0 {
            return Err(AppError::validation("atMs must be non-negative"));
        }
        return Ok(value);
    }
    i64::try_from(crate::clock::epoch_millis())
        .map_err(|_| AppError::internal("system clock exceeded source timestamp range"))
}

fn source_object_error(error: crate::emporium::objects::ObjectError) -> AppError {
    use crate::emporium::objects::ObjectError;
    let message = error.message().to_string();
    match error {
        ObjectError::BadRequest(_) => AppError::validation(message),
        // Vocab/class misconfiguration — NEVER coded (this mapper must not
        // drift from `emporium_mcp_surface.rs`'s twin).
        ObjectError::NotFound(_) => AppError::not_found(message),
        // Genuine absence — the only NotFound-family variant coded.
        ObjectError::Absent(_) => {
            AppError::not_found(message).with_code(app_error_codes::OBJECT_NOT_FOUND)
        }
        ObjectError::Conflict(_) => AppError::conflict(message),
        ObjectError::Internal(_) => AppError::internal(message),
    }
}

fn normalize_legacy_generic_record(
    raw: &Value,
    index: usize,
) -> AppResult<(String, String, Value)> {
    let mut record = raw
        .as_object()
        .cloned()
        .ok_or_else(|| AppError::validation(format!("records[{index}]: expected a JSON object")))?;
    if record.contains_key("subject") || record.contains_key("vocab") {
        return Err(AppError::validation(format!(
            "records[{index}]: subject and vocab are not accepted inside a record"
        )));
    }
    if !record.contains_key("localId") {
        if let Some(value) = record
            .remove("clientRef")
            .or_else(|| record.remove("client_ref"))
        {
            record.insert("localId".to_string(), value);
        }
    }
    let class = record
        .get("kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::validation(format!("records[{index}]: kind is required")))?
        .to_string();
    let object_id = record
        .get("localId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::validation(format!(
                "records[{index}]: localId or clientRef is required"
            ))
        })?
        .to_string();
    if object_id.contains("://") || object_id.starts_with("urn:") {
        return Err(AppError::validation(format!(
            "records[{index}]: localId/clientRef must be a local token, not an IRI"
        )));
    }
    record.insert("kind".to_string(), json!(class));
    record.insert("localId".to_string(), json!(object_id));
    Ok((class, object_id, Value::Object(record)))
}

fn normalize_legacy_memory_records(
    records: &[Value],
    publish: bool,
    top_observer: Option<&str>,
) -> AppResult<BTreeMap<String, Vec<Value>>> {
    let mut groups = BTreeMap::<String, Vec<Value>>::new();
    for (index, raw) in records.iter().enumerate() {
        let mut record = raw.as_object().cloned().ok_or_else(|| {
            AppError::validation(format!("records[{index}]: expected a JSON object"))
        })?;
        if let Some(value) = record.remove("supersedes") {
            record.entry("supersedesRef".to_string()).or_insert(value);
        }
        if let Some(value) = record.remove("contradicts") {
            record.entry("contradictsRef".to_string()).or_insert(value);
        }
        if let Some(value) = record.remove("observer") {
            record.entry("observerAgentId".to_string()).or_insert(value);
        }
        let observer = if publish {
            record.remove("observerAgentId");
            String::new()
        } else {
            let observer = record
                .get("observerAgentId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    top_observer
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                })
                .ok_or_else(|| {
                    AppError::validation("private memory source requires observer or publish=true")
                })?
                .to_string();
            record.insert("observerAgentId".to_string(), json!(observer));
            observer
        };
        groups
            .entry(observer)
            .or_default()
            .push(Value::Object(record));
    }
    Ok(groups)
}

/// Source-authoritative implementation of the legacy Emporium write face.
///
/// The public tool retains its familiar request/response, but once a graph has
/// a source checkpoint it first validates through the exact legacy dry-run and
/// then commits stable current-state/event/memory intents. No post-checkpoint
/// write can live only in a disposable projection.
pub(crate) async fn mcp_source_emporium_write(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = source_argument_string(arguments, &["graphId", "graph_id"])
        .ok_or_else(|| AppError::validation("graphId is required"))?;
    let vocab = source_argument_string(arguments, &["vocab"])
        .ok_or_else(|| AppError::validation("vocab is required"))?;
    let records = arguments
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| AppError::validation("records is required"))?;
    if records.is_empty() {
        return Err(AppError::validation("records must not be empty"));
    }
    let publish = arguments
        .get("publish")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let observer = source_argument_string(arguments, &["observer"]);

    let expected = crate::graph_incarnation_admission::expected_incarnation(arguments)?;
    let preparation_lease = crate::graph_incarnation_admission::acquire_expected_lifetime(
        &app, &graph_id, expected.as_deref(),
    ).await?;

    let mut preview = crate::emporium::write::emporium_write(
        &app,
        &graph_id,
        &vocab,
        &records,
        true,
        publish,
        observer.as_deref(),
    )
    .await
    .map_err(source_object_error)?;
    if !preview.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        if let Some(object) = preview.as_object_mut() {
            object.insert("dryRun".to_string(), Value::Bool(false));
        }
        return Ok(preview);
    }

    // Chamber-proposed vocabularies (an ACTIVE `chm:DomainOntology` in
    // `:projection:chamber`, e.g. `cuentas`) have no embedded contract, so the
    // source authority below cannot type their records yet. The preview above
    // already validated this batch through the chamber-aware emporium lane
    // (`resolve_ingest_contract`: embedded first, then the chamber); apply
    // through that same lane instead of refusing a vocabulary the cell has
    // just accepted. Such writes are materialized into their projection sink
    // but are NOT source-ledgered — `sourceLedgered: false` says so on the
    // wire — until the source authority learns chamber ontologies (Despacho
    // bead `despacho-d-chamber-write-lane`, 2026-09-15). Before this branch,
    // every chamber vocabulary previewed clean and then failed apply with
    // "unknown embedded vocabulary" (observed on the canary cell 2026-09-04
    // and on Sirin 2026-09-15).
    if get_vocabulary(&vocab).is_none() {
        let mut applied = crate::emporium::write::emporium_write(
            &app,
            &graph_id,
            &vocab,
            &records,
            false,
            publish,
            observer.as_deref(),
        )
        .await
        .map_err(source_object_error)?;
        if let Some(object) = applied.as_object_mut() {
            object.insert("dryRun".to_string(), Value::Bool(false));
            object.insert("sourceLedgered".to_string(), Value::Bool(false));
            let warning = Value::String(format!(
                "vocab '{vocab}' is a chamber-proposed ontology: applied through the \
                 emporium lane, not source-ledgered"
            ));
            match object.get_mut("warnings").and_then(Value::as_array_mut) {
                Some(warnings) => warnings.push(warning),
                None => {
                    object.insert("warnings".to_string(), Value::Array(vec![warning]));
                }
            }
        }
        return Ok(applied);
    }

    let (_, graph_incarnation) = active_graph_identity(&app, &graph_id, expected.as_deref())?;
    let graph_dir = existing_graph_dir(&app, &graph_id).map_err(AppError::storage)?;
    let ledger = read_ledger(&graph_dir, &graph_id, &graph_incarnation)?;
    let invocation_id = source_invocation_id(arguments, "legacy-emporium")?;
    let at_ms = source_now_ms(arguments)?;
    let contract = get_vocabulary(&vocab)
        .ok_or_else(|| AppError::validation(format!("unknown embedded vocabulary '{vocab}'")))?;
    let mut operations = Vec::<SourceOperation>::new();

    if contract.write_target.as_deref() == Some("projection:memory") {
        let groups = normalize_legacy_memory_records(&records, publish, observer.as_deref())?;
        let total = groups.len();
        for (index, (effective_observer, records)) in groups.into_iter().enumerate() {
            operations.push(SourceOperation::Memory {
                operation_id: source_child_operation_id(&invocation_id, index, total),
                observer: effective_observer,
                publish,
                at_ms,
                records,
            });
        }
    } else {
        let folds = fold_current_objects(&ledger)?;
        let mut current_versions = folds
            .into_iter()
            .map(|fold| (fold.face.object_key, fold.face.source_version))
            .collect::<BTreeMap<_, _>>();
        let total = records.len();
        for (index, raw) in records.iter().enumerate() {
            let (class, object_id, record) = normalize_legacy_generic_record(raw, index)?;
            let signature = contract
                .materialization_signature(&class)
                .map_err(AppError::validation)?;
            let operation_id = source_child_operation_id(&invocation_id, index, total);
            match signature.source_kind {
                SourceKind::CurrentState => {
                    let key = object_key(&vocab, &class, &object_id);
                    let base_version = current_versions
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| ROOT_VERSION.to_string());
                    let source_version =
                        candidate_version(&key, &operation_id, &base_version, &record)?;
                    operations.push(SourceOperation::CurrentState {
                        operation_id,
                        vocab: vocab.clone(),
                        class,
                        object_id,
                        base_version,
                        record,
                        causal_order: Some(at_ms),
                        client_id: observer.clone().or_else(|| Some("legacy-mcp".to_string())),
                        evidence_weight: None,
                    });
                    current_versions.insert(key, source_version);
                }
                SourceKind::EventLog => operations.push(SourceOperation::EventLog {
                    operation_id,
                    event_id: object_id,
                    vocab: vocab.clone(),
                    class,
                    record,
                }),
                SourceKind::Derived => {
                    return Err(AppError::validation(format!(
                        "{vocab}.{class} is derived and cannot be authored"
                    )))
                }
            }
        }
    }

    // Do not nest lifecycle leases: a pending exclusive lifecycle writer may
    // block another shared acquisition. source_push rechecks this exact token
    // under its own admission lease before committing any operation.
    drop(preparation_lease);
    let graph_incarnation = expected.unwrap_or(graph_incarnation);
    let push = mcp_local_source_push(
        app,
        &json!({
            "graphId": graph_id,
            "graphIncarnation": graph_incarnation,
            "operations": operations,
        }),
    )
    .await?;
    if let Some(object) = preview.as_object_mut() {
        object.insert("dryRun".to_string(), Value::Bool(false));
        object.insert("ok".to_string(), push["ok"].clone());
        object.insert("sourceSync".to_string(), push);
    }
    Ok(preview)
}

/// Route the public valuation convenience tool through stable valuation events
/// after source activation. Before activation the legacy write runs while the
/// source gate is held, ensuring a concurrent first pull checkpoints it.
pub(crate) async fn mcp_authoritative_value(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id =
        crate::mcp_utils::mcp_graph_id_or_default(&app, arguments).map_err(AppError::validation)?;
    let gate = acquire_source_gate(&graph_id).await;
    if !source_authority_active(&app, &graph_id)? {
        let result = crate::salience_mcp_valuation::mcp_local_value(app, arguments)
            .map_err(AppError::validation);
        drop(gate);
        return result;
    }
    drop(gate);

    let (single, entries) =
        crate::salience_mcp_inputs::mcp_value_inputs(arguments).map_err(AppError::validation)?;
    let observer = source_argument_string(arguments, &["observerAgentId", "observer_agent_id"])
        .unwrap_or_default();
    let base_id = source_argument_string(
        arguments,
        &[
            "valuationEventId",
            "valuation_event_id",
            "sourceOperationId",
            "source_operation_id",
            "operationId",
            "operation_id",
        ],
    )
    .unwrap_or_else(|| format!("legacy-value-{}", Uuid::new_v4()));
    validate_stable_id(&base_id, "operationId")?;
    let default_at_ms = source_now_ms(arguments)?;
    let raw_batch = arguments.get("valuations").and_then(Value::as_array);
    let total = entries.len();
    let mut operations = Vec::<SourceOperation>::new();
    let mut accepted_entries = Vec::new();
    let mut errors = Vec::new();

    for entry in entries {
        let error = if entry.document_id.is_empty() || entry.block_id.is_empty() {
            Some("document_id and block_id are required")
        } else if entry.importance.is_none() && entry.valence.is_none() && entry.tags.is_empty() {
            Some("At least one of importance, valence, or tags required")
        } else if entry
            .importance
            .is_some_and(|importance| !(0..=5).contains(&importance))
        {
            Some("importance must be between 0 and 5")
        } else if entry
            .valence
            .is_some_and(|valence| !(-5..=5).contains(&valence))
        {
            Some("valence must be between -5 and +5")
        } else {
            None
        };
        if let Some(error) = error {
            errors.push(json!({ "index": entry.input_index, "error": error }));
            continue;
        }
        let raw = raw_batch.and_then(|batch| batch.get(entry.input_index));
        let operation_id = raw
            .and_then(|value| {
                source_argument_string(
                    value,
                    &[
                        "sourceOperationId",
                        "source_operation_id",
                        "operationId",
                        "operation_id",
                    ],
                )
            })
            .unwrap_or_else(|| source_child_operation_id(&base_id, entry.input_index, total));
        validate_stable_id(&operation_id, "operationId")?;
        let event_id = raw
            .and_then(|value| {
                source_argument_string(value, &["valuationEventId", "valuation_event_id"])
            })
            .unwrap_or_else(|| operation_id.clone());
        validate_stable_id(&event_id, "valuationEventId")?;
        let at_ms = raw
            .and_then(|value| {
                value
                    .get("atMs")
                    .or_else(|| value.get("at_ms"))
                    .and_then(Value::as_i64)
            })
            .unwrap_or(default_at_ms);
        operations.push(SourceOperation::Valuation {
            operation_id,
            valuation_event_id: event_id,
            observer: observer.clone(),
            document_id: entry.document_id.clone(),
            block_id: entry.block_id.clone(),
            importance: entry.importance,
            valence: entry.valence,
            tags: entry.tags.clone(),
            at_ms,
        });
        accepted_entries.push(entry);
    }

    if single {
        if let Some(error) = errors.first() {
            return Err(AppError::validation(
                error["error"].as_str().unwrap_or("valuation failed"),
            ));
        }
    }
    if operations.is_empty() {
        return Ok(json!({
            "results": [],
            "updated_count": 0,
            "updatedCount": 0,
            "errors": errors,
            "error_count": errors.len(),
            "errorCount": errors.len(),
        }));
    }

    let (_, graph_incarnation) = active_graph_identity(&app, &graph_id, None)?;
    let push = mcp_local_source_push(
        app.clone(),
        &json!({
            "graphId": graph_id,
            "graphIncarnation": graph_incarnation,
            "operations": operations,
        }),
    )
    .await?;

    let mut results = Vec::new();
    let mut tags_applied = 0usize;
    for entry in &accepted_entries {
        if !entry.tags.is_empty() {
            tags_applied += 1;
        }
        if entry.importance.is_some() || entry.valence.is_some() {
            let mut scores = crate::salience_score_projection::local_block_value_scores_for(
                &app,
                &graph_id,
                &observer,
                Some(&entry.document_id),
                Some(&entry.block_id),
                None,
                1,
                None,
                None,
            )
            .map_err(AppError::internal)?;
            if let Some(score) = scores.pop() {
                results.push(score);
            }
        }
    }

    if single {
        let mut result = if let Some(result) = results.into_iter().next() {
            result
        } else if tags_applied > 0 {
            json!({ "tags_applied": tags_applied, "tagsApplied": tags_applied })
        } else {
            json!({})
        };
        if let Some(object) = result.as_object_mut() {
            object.insert("sourceSync".to_string(), push);
        }
        return Ok(result);
    }

    let mut output = json!({
        "results": results,
        "updated_count": results.len(),
        "updatedCount": results.len(),
        "sourceSync": push,
    });
    if !errors.is_empty() {
        output["errors"] = Value::Array(errors.clone());
        output["error_count"] = json!(errors.len());
        output["errorCount"] = json!(errors.len());
    }
    if tags_applied > 0 {
        output["tags_applied"] = json!(tags_applied);
        output["tagsApplied"] = json!(tags_applied);
    }
    Ok(output)
}

pub(crate) async fn mcp_source_emporium_retract(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = source_argument_string(arguments, &["graphId", "graph_id"])
        .ok_or_else(|| AppError::validation("graphId is required"))?;
    let subject = source_argument_string(arguments, &["subject"])
        .ok_or_else(|| AppError::validation("subject is required"))?;
    let rationale = source_argument_string(arguments, &["rationale"])
        .ok_or_else(|| AppError::validation("rationale is required"))?;
    let retraction_kind =
        source_argument_string(arguments, &["kind"]).unwrap_or_else(|| "retract".to_string());
    let observer = source_argument_string(arguments, &["observer"]);
    let operation_id = source_invocation_id(arguments, "legacy-retraction")?;
    let event_id = source_argument_string(arguments, &["retractionEventId", "retraction_event_id"])
        .unwrap_or_else(|| operation_id.clone());
    let at_ms = source_now_ms(arguments)?;
    let (_, graph_incarnation) = active_graph_identity(&app, &graph_id, None)?;
    let push = mcp_local_source_push(
        app,
        &json!({
            "graphId": graph_id,
            "graphIncarnation": graph_incarnation,
            "operations": [{
                "kind": "retraction",
                "operationId": operation_id,
                "retractionEventId": event_id,
                "subject": subject,
                "rationale": rationale,
                "retractionKind": retraction_kind,
                "observer": observer,
                "atMs": at_ms,
            }],
        }),
    )
    .await?;
    Ok(json!({
        "graphId": graph_id,
        "subject": subject,
        "kind": retraction_kind,
        "rationale": rationale,
        "retractedAt": crate::emporium::terms::iso_from_ms(at_ms),
        "retractionRef": event_id,
        "alreadyRetracted": false,
        "warnings": [],
        "sourceSync": push,
    }))
}

fn source_registry() -> AppResult<Value> {
    let mut classes = Vec::new();
    for (vocab_name, _, _) in crate::emporium::vocabs::VOCAB_REGISTRY.iter() {
        let contract = get_vocabulary(vocab_name).ok_or_else(|| {
            AppError::internal(format!("registered vocab '{vocab_name}' missing"))
        })?;
        for class in contract.classes.keys() {
            let signature = contract
                .materialization_signature(class)
                .map_err(AppError::internal)?;
            classes.push(json!({
                "vocab": vocab_name,
                "class": class,
                "sourceKind": match signature.source_kind {
                    SourceKind::CurrentState => "current-state",
                    SourceKind::EventLog => "event-log",
                    SourceKind::Derived => "derived",
                },
                "identityKind": format!("{:?}", signature.identity_kind),
                "storeTarget": signature.store_target,
                "storeMode": format!("{:?}", signature.store_mode),
                "reconciliationStrategy": reconciliation_strategy_label(signature.reconciliation_strategy),
                "dispatchMode": format!("{:?}", signature.dispatch_mode),
            }));
        }
    }
    classes.sort_by(|left, right| {
        (
            left["vocab"].as_str().unwrap_or(""),
            left["class"].as_str().unwrap_or(""),
        )
            .cmp(&(
                right["vocab"].as_str().unwrap_or(""),
                right["class"].as_str().unwrap_or(""),
            ))
    });
    Ok(Value::Array(classes))
}

fn manifest_member(
    source_kind: &str,
    object_id: String,
    object_incarnation: Option<String>,
    source_version: String,
    bytes: &[u8],
) -> Value {
    json!({
        "sourceKind": source_kind,
        "objectId": object_id,
        "objectIncarnation": object_incarnation,
        "sourceVersion": source_version,
        "localDigest": sha256_bytes(bytes),
        "durable": true,
    })
}

fn source_resource_directory(relative: &Path) -> bool {
    let components = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    matches!(
        components.as_slice(),
        ["documents", _, "original"]
            | ["artifacts", _, "original"]
            | ["artifacts", _, "revisions", _]
            | ["images", _, "original"]
    )
}

fn collect_original_resources(graph_dir: &Path) -> AppResult<Vec<(Value, Vec<u8>)>> {
    let mut resources = Vec::new();
    let mut pending = vec![graph_dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| {
                AppError::storage(format!(
                    "read source resource directory {}: {error}",
                    directory.display()
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                AppError::storage(format!(
                    "read source resource entry {}: {error}",
                    directory.display()
                ))
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type().map_err(|error| {
                AppError::storage(format!(
                    "read source resource type {}: {error}",
                    entry.path().display()
                ))
            })?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(graph_dir).map_err(|error| {
                AppError::storage(format!(
                    "source resource path {} is outside graph: {error}",
                    path.display()
                ))
            })?;
            if source_resource_directory(relative) && path.join("manifest.json").is_file() {
                let (manifest, bytes) =
                    crate::original_file_storage::read_original_file_from_dir(&path)
                        .map_err(AppError::storage)?;
                let relative_key = relative.to_string_lossy().replace('\\', "/");
                let resource_id = format!("original:{relative_key}");
                let digest = sha256_bytes(&bytes);
                resources.push((
                    json!({
                        "resourceId": resource_id,
                        "kind": if relative_key.contains("/revisions/") {
                            "artifact-revision"
                        } else {
                            "original"
                        },
                        "path": relative_key,
                        "filename": manifest.filename,
                        "mediaType": manifest.mime_type,
                        "sizeBytes": bytes.len(),
                        "digest": digest,
                        "createdAt": manifest.created_at,
                        "updatedAt": manifest.updated_at,
                        "dataBase64": BASE64_STANDARD.encode(&bytes),
                    }),
                    bytes,
                ));
                continue;
            }
            pending.push(path);
        }
    }
    resources.sort_by(|left, right| {
        left.0["resourceId"]
            .as_str()
            .unwrap_or("")
            .cmp(right.0["resourceId"].as_str().unwrap_or(""))
    });
    Ok(resources)
}

fn collect_document_history_sources(
    graph_dir: &Path,
    graph_id: &str,
    document_ids: &[String],
) -> AppResult<Vec<(Value, Vec<u8>)>> {
    let mut history = Vec::new();
    for document_id in document_ids {
        let mut store = crate::document_history_persistence::read_document_history_store(
            graph_dir,
            graph_id,
            document_id,
        )
        .map_err(AppError::storage)?;
        store
            .snapshots
            .sort_by(|left, right| left.snapshot_id.cmp(&right.snapshot_id));
        for meta in store.snapshots {
            let payload = crate::document_history_persistence::read_document_snapshot_payload(
                graph_dir,
                document_id,
                &meta.snapshot_id,
            )
            .map_err(AppError::storage)?;
            let record = json!({
                "historyId": format!("{}:{}", document_id, meta.snapshot_id),
                "documentId": document_id,
                "snapshotId": meta.snapshot_id,
                "meta": meta,
                "payload": payload,
            });
            let bytes = canonical_json_bytes(&record).map_err(|error| {
                AppError::serialization(format!(
                    "serialize document history source {document_id}: {error}"
                ))
            })?;
            history.push((record, bytes));
        }
    }
    history.sort_by(|left, right| {
        left.0["historyId"]
            .as_str()
            .unwrap_or("")
            .cmp(right.0["historyId"].as_str().unwrap_or(""))
    });
    Ok(history)
}

pub(crate) async fn mcp_local_source_pull(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let input: SourcePullInput = serde_json::from_value(arguments.clone())
        .map_err(|error| AppError::validation(format!("source_pull input: {error}")))?;
    let _source_gate = acquire_source_gate(&input.graph_id).await;
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let identity_lease = coordinator
        .acquire_lifecycle_shared(&input.graph_id)
        .await
        .map_err(AppError::internal)?;
    // Reject a v1 complete bundle before even initializing an incarnation,
    // checkpointing, importing events or repairing projections. This does not
    // restrict ordinary document/sidebar/editor access or discard source data.
    let (budget_dir, _) = read_graph_record_no_heal(&app, &input.graph_id)?;
    crate::source_pull_budget::check_graph(&budget_dir)?;
    let (graph_dir, graph_incarnation) =
        active_graph_identity(&app, &input.graph_id, input.graph_incarnation.as_deref())?;
    let mut ledger = read_ledger(&graph_dir, &input.graph_id, &graph_incarnation)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let checkpoint_store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    if ledger.checkpoint.is_none() {
        ledger.checkpoint = Some(capture_checkpoint_inner(&graph_dir, &checkpoint_store, ledger.revision, true)?);
    }
    drop(checkpoint_store);
    let imported_memory_events = import_external_memory_events(&app, &input.graph_id, &mut ledger)?;
    write_ledger(&graph_dir, &ledger)?;
    drop(identity_lease);
    let individual_repair = repair_individual_effects(
        &app,
        &input.graph_id,
        &graph_incarnation,
        &graph_dir,
        &mut ledger,
    )
    .await?;
    let lease = coordinator
        .acquire_hot_write(&input.graph_id)
        .await
        .map_err(AppError::internal)?;
    active_graph_identity(&app, &input.graph_id, Some(&graph_incarnation))?;
    let pending = ledger
        .operations
        .values()
        .any(|operation| operation.status == ReceiptStatus::Accepted)
        || imported_memory_events > 0;
    let repair = if pending {
        match rebuild_source_projections(&app, &input.graph_id, &mut ledger).await {
            Ok(report) => {
                for operation in ledger.operations.values_mut() {
                    if !operation.operation.requires_individual_effect() {
                        operation.status = ReceiptStatus::Applied;
                        operation.effect_error = None;
                    }
                }
                write_ledger(&graph_dir, &ledger)?;
                Some(report)
            }
            Err(error) => Some(json!({ "error": error.to_string() })),
        }
    } else {
        None
    };

    // Native writers may have advanced files during the lifecycle-shared
    // preparation. Recheck under the hot-write lease before collecting bodies.
    crate::source_pull_budget::check_graph(&graph_dir)?;
    let workspace =
        read_workspace_record(&graph_dir, &input.graph_id).map_err(AppError::storage)?;
    let workspace_bytes = BASE64_STANDARD
        .decode(&workspace.ydoc_update_base64)
        .map_err(|error| AppError::storage(format!("decode workspace source: {error}")))?;
    let mut members = vec![manifest_member(
        "ydoc",
        "workspace".to_string(),
        Some(graph_incarnation.clone()),
        sha256_bytes(&workspace_bytes),
        &workspace_bytes,
    )];
    let (_, graph_record) = read_graph_record_no_heal(&app, &input.graph_id)?;
    let graph_metadata = json!({
        "title": graph_record.title,
        "description": graph_record.description,
        "status": graph_record.status,
        "createdAt": graph_record.created_at,
        "updatedAt": graph_record.updated_at,
        "createdByOperationId": graph_record.created_by_operation_id,
    });
    let graph_metadata_bytes = canonical_json_bytes(&graph_metadata).map_err(|error| {
        AppError::serialization(format!("serialize graph metadata source: {error}"))
    })?;
    members.push(manifest_member(
        "current-state",
        "graph-metadata".to_string(),
        Some(graph_incarnation.clone()),
        sha256_bytes(&graph_metadata_bytes),
        &graph_metadata_bytes,
    ));
    let documents = read_graph_documents_cold(&graph_dir).map_err(AppError::storage)?;
    let mut documents_json = Vec::new();
    let mut document_ids = Vec::new();
    for document in &documents {
        document_ids.push(document.document_id.clone());
        let incarnation = ensure_document_incarnation_id(&graph_dir, &document.document_id)
            .map_err(AppError::storage)?;
        // The enumeration above is a COLD read, which deliberately blanks
        // `ydoc_update_base64` (see read_document_record_cold: no all-documents
        // walk may hydrate every Y.Doc history into memory). A source pull is
        // one of the callers that genuinely needs the update payload, so read
        // each document's sidecar directly — one document at a time, never the
        // whole graph at once. Trusting the cold record's field here would emit
        // an empty body and an identical zero-byte digest for every document.
        let update_base64 = crate::document_sidecar_store::read_ydoc_update_base64(
            &document_ydoc_state_path(&graph_dir, &document.document_id),
        )
        .map_err(AppError::storage)?
        .unwrap_or_default();
        let bytes = BASE64_STANDARD
            .decode(&update_base64)
            .map_err(|error| {
                AppError::storage(format!(
                    "decode document {} source: {error}",
                    document.document_id
                ))
            })?;
        let digest = sha256_bytes(&bytes);
        members.push(manifest_member(
            "ydoc",
            format!("document:{}", document.document_id),
            Some(incarnation.clone()),
            digest.clone(),
            &bytes,
        ));
        documents_json.push(json!({
            "documentId": document.document_id,
            "documentIncarnation": incarnation,
            "revision": document.revision,
            "updateBase64": update_base64,
            "digest": digest,
        }));
    }

    // Semantic search is a disposable face over complete document sources.
    // Mirror the normalized corpus (not an opaque server-owned index) so a
    // browser can deterministically rebuild/query it while partitioned. The
    // corpus member is epoch-bound by the enclosing source revision and is
    // invalidated with every complete mirror commit.
    let semantic_corpus = semantic_block_sources(&documents)
        .into_iter()
        .map(|source| {
            json!({
                "semanticId": source.iri,
                "kind": source.kind,
                "graphId": source.graph_id,
                "documentId": source.document_id,
                "documentTitle": source.document_title,
                "blockId": source.block_id,
                "blockType": source.block_type,
                "content": source.content,
                "contentHash": source.content_hash,
                "order": source.order,
            })
        })
        .collect::<Vec<_>>();
    let semantic_corpus_bytes = canonical_json_bytes(&Value::Array(semantic_corpus.clone()))
        .map_err(|error| {
            AppError::serialization(format!("serialize semantic corpus source: {error}"))
        })?;
    members.push(manifest_member(
        "derived",
        "semantic-corpus".to_string(),
        None,
        sha256_bytes(&semantic_corpus_bytes),
        &semantic_corpus_bytes,
    ));

    let original_resources = collect_original_resources(&graph_dir)?;
    let mut resources_json = Vec::with_capacity(original_resources.len());
    for (resource, bytes) in original_resources {
        let resource_id = resource["resourceId"].as_str().unwrap_or("unknown");
        let digest = resource["digest"].as_str().unwrap_or_default();
        members.push(manifest_member(
            "current-state",
            format!("resource:{resource_id}"),
            None,
            digest.to_string(),
            &bytes,
        ));
        resources_json.push(resource);
    }
    let document_history =
        collect_document_history_sources(&graph_dir, &input.graph_id, &document_ids)?;
    let mut history_json = Vec::with_capacity(document_history.len());
    for (record, bytes) in document_history {
        let history_id = record["historyId"].as_str().unwrap_or("unknown");
        let digest = sha256_bytes(&bytes);
        members.push(manifest_member(
            "event-log",
            format!("document-history:{history_id}"),
            None,
            digest.clone(),
            &bytes,
        ));
        history_json.push(record);
    }

    let current = fold_current_objects(&ledger)?;
    let conflicts = current
        .iter()
        .filter_map(|fold| fold.conflict.clone())
        .collect::<Vec<_>>();
    for fold in &current {
        let bytes = canonical_json_bytes(&fold.face.record).map_err(|error| {
            AppError::serialization(format!("serialize current source: {error}"))
        })?;
        members.push(manifest_member(
            "current-state",
            fold.face.object_key.clone(),
            None,
            fold.face.source_version.clone(),
            &bytes,
        ));
    }
    let mut event_faces = BTreeMap::<String, Value>::new();
    for operation in event_operations(&ledger) {
        let SourceOperation::EventLog {
            event_id,
            vocab,
            class,
            record,
            ..
        } = &operation.operation
        else {
            continue;
        };
        event_faces.entry(event_id.clone()).or_insert_with(|| {
            json!({
                "eventId": event_id,
                "vocab": vocab,
                "class": class,
                "record": normalized_generic_record(class, event_id, record)
                    .unwrap_or_else(|_| record.clone()),
                "digest": semantic_event_digest(&operation.operation)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| operation.digest.clone()),
            })
        });
    }
    let events = event_faces.into_values().collect::<Vec<_>>();
    for event in &events {
        let bytes = canonical_json_bytes(event)
            .map_err(|error| AppError::serialization(format!("serialize event source: {error}")))?;
        members.push(manifest_member(
            "event-log",
            format!("event:{}", event["eventId"].as_str().unwrap_or_default()),
            None,
            event["digest"].as_str().unwrap_or_default().to_string(),
            &bytes,
        ));
    }
    let memory = ledger
        .operations
        .values()
        .filter(|operation| matches!(operation.operation, SourceOperation::Memory { .. }))
        .map(|operation| serde_json::to_value(&operation.operation).unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    let valuations = ledger
        .operations
        .values()
        .filter(|operation| matches!(operation.operation, SourceOperation::Valuation { .. }))
        .map(|operation| serde_json::to_value(&operation.operation).unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    let retractions = ledger
        .operations
        .values()
        .filter(|operation| matches!(operation.operation, SourceOperation::Retraction { .. }))
        .map(|operation| serde_json::to_value(&operation.operation).unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    for (kind, sources) in [
        ("event-log", &memory),
        ("event-log", &valuations),
        ("event-log", &retractions),
    ] {
        for source in sources {
            let bytes = canonical_json_bytes(source).map_err(|error| {
                AppError::serialization(format!("serialize source manifest member: {error}"))
            })?;
            let id = source
                .get("operationId")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            members.push(manifest_member(
                kind,
                format!("operation:{id}"),
                None,
                value_digest(source)?,
                &bytes,
            ));
        }
    }

    let checkpoint = ledger
        .checkpoint
        .clone()
        .ok_or_else(|| AppError::internal("source checkpoint was not established"))?;
    let (baseline_nquads, baseline_value_stores) = checkpoint_files_inner(&graph_dir, &checkpoint, true)?;
    members.push(manifest_member(
        "current-state",
        "legacy-rdf-baseline".to_string(),
        None,
        checkpoint.rdf_digest.clone(),
        baseline_nquads.as_bytes(),
    ));
    for (path, data) in &baseline_value_stores {
        members.push(manifest_member(
            "current-state",
            format!("legacy-valuation-store:{path}"),
            None,
            sha256_bytes(data.as_bytes()),
            data.as_bytes(),
        ));
    }

    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let projection = source_rdf_dump(&store)?;
    let projection_data = canonicalize_nquads(&projection.data);
    let projection_digest = sha256_bytes(projection_data.as_bytes());
    let current_value_stores = capture_value_store_files_inner(&graph_dir, true)?;
    members.push(manifest_member(
        "derived",
        "rdf-projection-snapshot".to_string(),
        None,
        projection_digest.clone(),
        projection_data.as_bytes(),
    ));
    for (path, data) in &current_value_stores {
        members.push(manifest_member(
            "current-state",
            format!("valuation-store:{path}"),
            None,
            sha256_bytes(data.as_bytes()),
            data.as_bytes(),
        ));
    }
    members.sort_by(|left, right| {
        left["objectId"]
            .as_str()
            .unwrap_or("")
            .cmp(right["objectId"].as_str().unwrap_or(""))
    });
    let manifest_hash = value_digest(&Value::Array(members.clone()))?;
    let receipts = ledger
        .operations
        .iter()
        .map(|(id, operation)| receipt_from(id, operation, false))
        .collect::<Vec<_>>();
    let epoch = source_epoch(&graph_incarnation, ledger.revision, &manifest_hash);
    // `conflicts` rides OUTSIDE the manifest digest closure (WS4 D6): the
    // client's manifest check is fail-closed on unknown/missing members in
    // both directions, so joining the closure is a flag day. A sibling
    // digest gives the same tamper-evidence without that break.
    let conflicts_digest = value_digest(&serde_json::to_value(&conflicts).map_err(|error| {
        AppError::serialization(format!("serialize sync conflicts: {error}"))
    })?)?;
    drop(lease);
    Ok(json!({
        "schemaVersion": SOURCE_SYNC_SCHEMA_VERSION,
        "graphId": input.graph_id,
        "graphIncarnation": graph_incarnation,
        "revision": ledger.revision,
        "epoch": epoch,
        "complete": true,
        "manifest": {
            "sourceManifestHash": manifest_hash,
            "members": members,
            "complete": true,
        },
        "workspace": {
            "updateBase64": workspace.ydoc_update_base64,
            "digest": sha256_bytes(&workspace_bytes),
        },
        "graphMetadata": graph_metadata,
        "documents": documents_json,
        "resources": resources_json,
        "documentHistory": history_json,
        "semanticCorpus": semantic_corpus,
        "derivedCapabilities": {
            "semanticSearch": {
                "algorithm": "source-derived-hybrid-v1",
                "sourceRevision": ledger.revision,
                "offline": true,
            },
        },
        "mutationCapabilities": {
            "expectedGraphIncarnation": {
                "version": 1,
                "tools": if cfg!(feature = "frontend-crdt") {
                    vec!["emporium_write"]
                } else {
                    vec!["emporium_write", "create_document_once", "create_wires"]
                },
                "input": "graphIncarnation",
                "alias": "graph_incarnation",
                "scope": "managed-graph-lifetime",
                "emporiumRequiresSourceAuthority": true,
                "multiOperationAtomic": false,
            },
        },
        "currentState": current.into_iter().map(|fold| fold.face).collect::<Vec<_>>(),
        "events": events,
        "memory": memory,
        "valuations": valuations,
        "retractions": retractions,
        "legacyBaseline": {
            "ledgerRevision": checkpoint.ledger_revision,
            "capturedAtMs": checkpoint.captured_at_ms,
            "rdfSnapshot": {
                "format": "application/n-quads",
                "data": baseline_nquads,
                "digest": checkpoint.rdf_digest,
                "quadCount": checkpoint.rdf_quad_count,
            },
            "valuationStores": baseline_value_stores,
        },
        "projectionSnapshot": {
            "format": "application/n-quads",
            "data": projection_data,
            "digest": projection_digest,
            "quadCount": projection.quad_count,
            "sourceRevision": ledger.revision,
        },
        "valuationStores": current_value_stores,
        "conflicts": conflicts,
        "conflictsDigest": conflicts_digest,
        "receipts": receipts,
        "sourceRegistry": source_registry()?,
        "repair": repair,
        "individualRepair": individual_repair,
        "importedLegacyMemoryEvents": imported_memory_events,
        "sourceCapabilities": {
            "typedErrorCodes": 1,
            "candidateAttribution": 1,
            "conflictsDigest": 1,
        },
    }))
}

pub(crate) async fn mcp_local_source_rebuild(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let input: SourceRebuildInput = serde_json::from_value(arguments.clone())
        .map_err(|error| AppError::validation(format!("source_rebuild input: {error}")))?;
    let _source_gate = acquire_source_gate(&input.graph_id).await;
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let identity_lease = coordinator
        .acquire_lifecycle_shared(&input.graph_id)
        .await
        .map_err(AppError::internal)?;
    let (graph_dir, graph_incarnation) =
        active_graph_identity(&app, &input.graph_id, Some(&input.graph_incarnation))?;
    let mut ledger = read_ledger(&graph_dir, &input.graph_id, &graph_incarnation)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let checkpoint_store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    ensure_checkpoint(&graph_dir, &checkpoint_store, &mut ledger)?;
    drop(checkpoint_store);
    import_external_memory_events(&app, &input.graph_id, &mut ledger)?;
    write_ledger(&graph_dir, &ledger)?;
    let before_store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let projection_before = rdf_dataset_identity(&before_store)?;
    drop(before_store);
    drop(identity_lease);
    let individual_repair = repair_individual_effects(
        &app,
        &input.graph_id,
        &graph_incarnation,
        &graph_dir,
        &mut ledger,
    )
    .await?;
    let lease = coordinator
        .acquire_hot_write(&input.graph_id)
        .await
        .map_err(AppError::internal)?;
    active_graph_identity(&app, &input.graph_id, Some(&graph_incarnation))?;
    let before_fold_hash = value_digest(
        &serde_json::to_value(fold_current_objects(&ledger)?).map_err(|error| {
            AppError::serialization(format!("serialize pre-rebuild fold: {error}"))
        })?,
    )?;
    let report = rebuild_source_projections(&app, &input.graph_id, &mut ledger).await?;
    let first_store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let projection_after_first = rdf_dataset_identity(&first_store)?;
    drop(first_store);
    let second_report = rebuild_source_projections(&app, &input.graph_id, &mut ledger).await?;
    let second_store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    let projection_after_second = rdf_dataset_identity(&second_store)?;
    for operation in ledger.operations.values_mut() {
        if !operation.operation.requires_individual_effect() {
            operation.status = ReceiptStatus::Applied;
            operation.effect_error = None;
        }
    }
    write_ledger(&graph_dir, &ledger)?;
    let after_fold_hash = value_digest(
        &serde_json::to_value(fold_current_objects(&ledger)?).map_err(|error| {
            AppError::serialization(format!("serialize post-rebuild fold: {error}"))
        })?,
    )?;
    drop(lease);
    Ok(json!({
        "ok": before_fold_hash == after_fold_hash
            && projection_before == projection_after_first
            && projection_after_first == projection_after_second,
        "graphId": input.graph_id,
        "graphIncarnation": graph_incarnation,
        "revision": ledger.revision,
        "sourceFoldHashBefore": before_fold_hash,
        "sourceFoldHashAfter": after_fold_hash,
        "sourceSetEqual": before_fold_hash == after_fold_hash,
        "projectionBefore": projection_before,
        "projectionAfter": projection_after_first,
        "projectionAfterSecondReplay": projection_after_second,
        "projectionSetEqual": projection_before == projection_after_first
            && projection_after_first == projection_after_second,
        "projection": report,
        "secondProjectionReplay": second_report,
        "individualRepair": individual_repair,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_value_failure_cannot_publish_restored_current_seed_marker() {
        let root = std::env::temp_dir().join(format!("garden-checkpoint-marker-failure-{}", Uuid::new_v4()));
        let graph_id = "checkpoint-marker-failure";
        let store = oxigraph::store::Store::new().unwrap();
        let marker_graph = crate::rdf_authority::seed_marker_graph_iri(graph_id);
        let rdf = format!("<{}> <{}seedKey> \"current-but-not-completed-replay\" <{marker_graph}> .\n<urn:kept> <urn:predicate> \"canonical\" <urn:authority> .\n", graph_subject(graph_id), crate::runtime_config::MNEMO_NS);
        let rdf_digest = sha256_bytes(rdf.as_bytes());
        let values = serde_json::to_vec(&BTreeMap::from([("item/block-values.json", "{}")])).unwrap();
        let value_stores_digest = sha256_bytes(&values);
        crate::storage::write_bytes(&rdf_checkpoint_path(&root, &rdf_digest), rdf.as_bytes()).unwrap();
        crate::storage::write_bytes(&values_checkpoint_path(&root, &value_stores_digest), &values).unwrap();
        // A file at the value-directory path makes the post-commit directory
        // removal fail. This is a real fixture I/O failure, not a relaxed oracle.
        crate::storage::write_bytes(&root.join("values"), b"not a directory").unwrap();
        let checkpoint = SourceCheckpoint { ledger_revision: 0, rdf_digest, rdf_quad_count: 2, value_stores_digest, captured_at_ms: 0 };
        assert!(restore_checkpoint(graph_id, &root, &store, &checkpoint).is_err());
        let actual = crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap().data;
        assert!(!actual.contains("seedKey"), "failed replay must not restore a completed marker");
        assert!(actual.contains("<urn:kept> <urn:predicate> \"canonical\" <urn:authority> ."));
        assert_eq!(fs::read(rdf_checkpoint_path(&root, &checkpoint.rdf_digest)).unwrap(), rdf.as_bytes());
        println!("SEED_CHECKPOINT_FAILURE_RETAINED_PROFILE={}", root.display());
    }

    #[test]
    fn ludus_checkpoint_replay_validates_before_effects_and_preserves_public_refusal() {
        let root = std::env::temp_dir().join(format!("ludus-checkpoint-{}", Uuid::new_v4()));
        let store = oxigraph::store::Store::new().unwrap();
        let graph_id = "ludus-checkpoint";
        let initial = "<urn:before> <urn:predicate> \"keep existing RDF\" <urn:before-graph> .\n";
        store.load_from_slice(RdfFormat::NQuads, initial.as_bytes()).unwrap();
        let old_values = root.join("values/old/block-values.json");
        crate::storage::write_bytes(&old_values, b"{\"keep\":true}").unwrap();
        let baseline = rdf_dataset_identity(&store).unwrap();
        let reserved = format!("<urn:owned> <urn:predicate> \"engine projection\" <{}:projection:graph> .\n", graph_subject(graph_id));
        let seal = |rdf: &str, values: BTreeMap<String, String>| {
            let rdf_digest = sha256_bytes(rdf.as_bytes());
            let data = serde_json::to_vec(&values).unwrap();
            let value_stores_digest = sha256_bytes(&data);
            crate::storage::write_bytes(&rdf_checkpoint_path(&root, &rdf_digest), rdf.as_bytes()).unwrap();
            crate::storage::write_bytes(&values_checkpoint_path(&root, &value_stores_digest), &data).unwrap();
            SourceCheckpoint { ledger_revision: 0, rdf_digest, rdf_quad_count: 1, value_stores_digest, captured_at_ms: 0 }
        };
        for checkpoint in [
            seal("not RDF", BTreeMap::new()),
            seal(&reserved, BTreeMap::from([("../escape/block-values.json".into(), "{}".into())])),
            seal(&reserved, BTreeMap::from([("safe/block-values.json".into(), "not JSON".into())])),
        ] {
            assert!(restore_checkpoint(graph_id, &root, &store, &checkpoint).is_err());
            assert_eq!(rdf_dataset_identity(&store).unwrap(), baseline);
            assert_eq!(std::fs::read(&old_values).unwrap(), b"{\"keep\":true}");
        }
        let checkpoint = seal(&reserved, BTreeMap::new());
        crate::storage::write_bytes(&rdf_checkpoint_path(&root, &checkpoint.rdf_digest), b"tampered").unwrap();
        assert!(restore_checkpoint(graph_id, &root, &store, &checkpoint).is_err());
        assert_eq!(rdf_dataset_identity(&store).unwrap(), baseline);
        assert_eq!(std::fs::read(&old_values).unwrap(), b"{\"keep\":true}");
        let error = crate::rdf_query_service::load_rdf_dataset_into_store(&store, graph_id, &reserved, "nquads", None).unwrap_err();
        assert!(error.contains("reserved projection graph"));
        assert_eq!(rdf_dataset_identity(&store).unwrap(), baseline);
        let checkpoint = seal(&reserved, BTreeMap::from([("new/block-values.json".into(), "{\"restored\":true}".into())]));
        restore_checkpoint(graph_id, &root, &store, &checkpoint).unwrap();
        let actual = crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap();
        assert_eq!(canonicalize_nquads(&actual.data), canonicalize_nquads(&reserved));
        assert!(!old_values.exists());
        assert_eq!(std::fs::read(root.join("values/new/block-values.json")).unwrap(), b"{\"restored\":true}");
        println!("LUDUS_CHECKPOINT_DISPOSABLE_FIXTURE={}", root.display());
    }

    #[test]
    fn pdf_source_independent_source_ledger_writes_are_refused() {
        let graph_id = "pdf-source-refusal";
        let ledger = SourceLedger::empty(graph_id,"incarnation-fixture");
        let retraction = SourceOperation::Retraction {
            operation_id:"pdf-retract-op".into(),retraction_event_id:"pdf-retract-event".into(),
            subject:format!("{}:document-owned",crate::pdf_source::sink(graph_id)),
            rationale:"forbidden independent write".into(),retraction_kind:"retract".into(),
            observer:None,at_ms:1,
        };
        let error = validate_source_operation(graph_id,&ledger,&retraction,"digest").unwrap_err();
        let message = error.message();
        assert!(message.contains("derived"),"{message}");
        let current = SourceOperation::CurrentState {
            operation_id:"pdf-current-op".into(),vocab:crate::pdf_source::PACK.into(),
            class:"PdfOriginal".into(),object_id:"forged".into(),base_version:"0".into(),
            record:json!({"kind":"PdfOriginal","localId":"forged"}),
            causal_order:None,client_id:None,evidence_weight:None,
        };
        assert!(validate_source_operation(graph_id,&ledger,&current,"digest").is_err());
        assert_eq!(ledger.revision,0);
        assert!(ledger.operations.is_empty());
    }

    fn bookmark(operation_id: &str, base_version: &str, title: &str) -> SourceOperation {
        SourceOperation::CurrentState {
            operation_id: operation_id.to_string(),
            vocab: "emporium-bookmark".to_string(),
            class: "Bookmark".to_string(),
            object_id: "same".to_string(),
            base_version: base_version.to_string(),
            record: json!({
                "kind": "Bookmark",
                "localId": "same",
                "url": "https://example.test",
                "title": title,
            }),
            causal_order: None,
            client_id: None,
            evidence_weight: None,
        }
    }

    /// [`bookmark`] with explicit writer attribution — the Ask A fields
    /// `bookmark` itself always leaves `None`.
    fn bookmark_attributed(
        operation_id: &str,
        base_version: &str,
        title: &str,
        client_id: Option<&str>,
        causal_order: Option<i64>,
        evidence_weight: Option<f64>,
    ) -> SourceOperation {
        match bookmark(operation_id, base_version, title) {
            SourceOperation::CurrentState {
                operation_id,
                vocab,
                class,
                object_id,
                base_version,
                record,
                ..
            } => SourceOperation::CurrentState {
                operation_id,
                vocab,
                class,
                object_id,
                base_version,
                record,
                causal_order,
                client_id: client_id.map(str::to_string),
                evidence_weight,
            },
            _ => unreachable!("bookmark always returns CurrentState"),
        }
    }

    fn ledger_with(operations: Vec<SourceOperation>) -> SourceLedger {
        let mut ledger = SourceLedger::empty("g", "inc");
        for (index, operation) in operations.into_iter().enumerate() {
            let digest = operation_digest(&operation).unwrap();
            ledger.operations.insert(
                operation.operation_id().to_string(),
                LedgerOperation {
                    digest,
                    accepted_revision: index as u64 + 1,
                    status: ReceiptStatus::Accepted,
                    operation,
                    outcome: json!({}),
                    effect_error: None,
                },
            );
        }
        ledger
    }

    #[test]
    fn canonical_digest_ignores_json_object_key_order() {
        let left = json!({ "z": 1, "a": { "y": 2, "b": 3 } });
        let right = json!({ "a": { "b": 3, "y": 2 }, "z": 1 });
        assert_eq!(value_digest(&left).unwrap(), value_digest(&right).unwrap());
    }

    #[test]
    fn canonical_json_matches_ecmascript_number_serialization() {
        let value = json!({ "order": 0.0, "fraction": 1.5 });
        assert_eq!(
            canonical_json_bytes(&value).unwrap(),
            br#"{"fraction":1.5,"order":0}"#,
        );
    }

    #[test]
    fn source_epoch_changes_when_manifest_changes_at_the_same_ledger_revision() {
        let before = source_epoch("incarnation", 7, "manifest-before");
        let after = source_epoch("incarnation", 7, "manifest-after");
        assert_ne!(before, after);
        assert_eq!(
            before, "incarnation:7:manifest-before",
            "epoch retains graph lifetime and source-ledger revision provenance",
        );
    }

    #[test]
    fn nquads_identity_ignores_store_iteration_order() {
        let ab = "<urn:a> <urn:p> \"a\" <urn:g> .\n<urn:b> <urn:p> \"b\" <urn:g> .\n";
        let ba = "<urn:b> <urn:p> \"b\" <urn:g> .\r\n<urn:a> <urn:p> \"a\" <urn:g> .\r\n";
        assert_eq!(canonicalize_nquads(ab), canonicalize_nquads(ba));
        assert_eq!(
            sha256_bytes(canonicalize_nquads(ab).as_bytes()),
            sha256_bytes(canonicalize_nquads(ba).as_bytes()),
        );
    }

    #[test]
    fn concurrent_current_state_is_delivery_order_independent_and_explicit() {
        let ab = ledger_with(vec![
            bookmark("client-a", ROOT_VERSION, "A"),
            bookmark("client-b", ROOT_VERSION, "B"),
        ]);
        let ba = ledger_with(vec![
            bookmark("client-b", ROOT_VERSION, "B"),
            bookmark("client-a", ROOT_VERSION, "A"),
        ]);
        let ab_fold = fold_current_objects(&ab).unwrap();
        let ba_fold = fold_current_objects(&ba).unwrap();
        assert_eq!(
            serde_json::to_value(&ab_fold[0].face).unwrap(),
            serde_json::to_value(&ba_fold[0].face).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(ab_fold[0].conflict.as_ref().unwrap()).unwrap(),
            serde_json::to_value(ba_fold[0].conflict.as_ref().unwrap()).unwrap(),
        );
        assert_eq!(ab_fold[0].conflict.as_ref().unwrap().candidates.len(), 2);
    }

    #[test]
    fn causal_chain_converges_even_when_successor_arrives_first() {
        let first = bookmark("client-a:1", ROOT_VERSION, "A1");
        let first_version = match &first {
            SourceOperation::CurrentState {
                operation_id,
                vocab,
                class,
                object_id,
                base_version,
                record,
                ..
            } => candidate_version(
                &object_key(vocab, class, object_id),
                operation_id,
                base_version,
                record,
            )
            .unwrap(),
            _ => unreachable!(),
        };
        let second = bookmark("client-a:2", &first_version, "A2");
        let ledger = ledger_with(vec![second, first]);
        let fold = fold_current_objects(&ledger).unwrap();
        assert_eq!(fold[0].face.record["title"], json!("A2"));
        assert!(fold[0].conflict.is_none());
    }

    #[test]
    fn missing_base_is_visible_until_predecessor_arrives() {
        let orphan = bookmark("client-a:2", "not-yet-present", "A2");
        let ledger = ledger_with(vec![orphan]);
        let fold = fold_current_objects(&ledger).unwrap();
        let conflict = fold[0].conflict.as_ref().expect("missing base is explicit");
        assert!(conflict.reason.contains("unavailable base"));
        assert_eq!(conflict.candidates.len(), 1);
        assert_eq!(fold[0].face.record["title"], json!("A2"));
    }

    #[test]
    fn causal_lww_without_complete_clock_remains_an_explicit_conflict() {
        let candidates = vec![
            CurrentCandidate {
                operation_id: "a".to_string(),
                source_version: "v-a".to_string(),
                base_version: ROOT_VERSION.to_string(),
                record: json!({}),
                causal_order: None,
                client_id: Some("a".to_string()),
                evidence_weight: None,
            },
            CurrentCandidate {
                operation_id: "b".to_string(),
                source_version: "v-b".to_string(),
                base_version: ROOT_VERSION.to_string(),
                record: json!({}),
                causal_order: Some(1),
                client_id: Some("b".to_string()),
                evidence_weight: None,
            },
        ];
        let (_, automatically_resolved) =
            deterministic_candidate(ReconciliationStrategy::CausalLww, &candidates);
        assert!(!automatically_resolved);
    }

    #[test]
    fn operation_id_reuse_with_different_content_is_rejected() {
        let mut ledger = ledger_with(vec![bookmark("same-id", ROOT_VERSION, "A")]);
        let replacement = bookmark("same-id", ROOT_VERSION, "B");
        let digest = operation_digest(&replacement).unwrap();
        let error = validate_source_operation("g", &ledger, &replacement, &digest).unwrap_err();
        assert_eq!(error.kind(), crate::app_error::AppErrorKind::Conflict);
        ledger.operations.clear();
    }

    // ── Slice 0 / WS4 Ask A — candidate attribution (05-master-spec.md §3,
    // 04-garden-gateway-contract.md §4.1, §6.1) ──

    #[test]
    fn candidate_faces_carry_writer_attribution() {
        let ledger = ledger_with(vec![
            bookmark_attributed(
                "client-a",
                ROOT_VERSION,
                "A",
                Some("device-a"),
                Some(10),
                Some(0.5),
            ),
            bookmark("client-b", ROOT_VERSION, "B"),
        ]);
        let fold = fold_current_objects(&ledger).unwrap();
        let conflict = fold[0].conflict.as_ref().unwrap();

        let attributed = conflict
            .candidates
            .iter()
            .find(|candidate| candidate.operation_id == "client-a")
            .unwrap();
        assert_eq!(attributed.client_id.as_deref(), Some("device-a"));
        assert_eq!(attributed.causal_order, Some(10));
        assert_eq!(attributed.evidence_weight, Some(0.5));

        let bare = conflict
            .candidates
            .iter()
            .find(|candidate| candidate.operation_id == "client-b")
            .unwrap();
        assert_eq!(bare.client_id, None);
        assert_eq!(bare.causal_order, None);
        assert_eq!(bare.evidence_weight, None);

        // `skip_serializing_if` — absence on the wire, not a null.
        let bare_json = serde_json::to_value(bare).unwrap();
        assert!(bare_json.get("clientId").is_none());
        assert!(bare_json.get("causalOrder").is_none());
        assert!(bare_json.get("evidenceWeight").is_none());
    }

    #[test]
    fn object_face_names_the_operation_that_minted_the_head() {
        // A single-writer chain: the head is attributed to its own operation.
        let ledger = ledger_with(vec![bookmark_attributed(
            "client-a:1",
            ROOT_VERSION,
            "A1",
            Some("device-a"),
            Some(1),
            None,
        )]);
        let fold = fold_current_objects(&ledger).unwrap();
        assert_eq!(fold[0].face.operation_id, "client-a:1");
        assert_eq!(fold[0].face.client_id.as_deref(), Some("device-a"));

        // After a ResolveCurrent, the head is attributed to the resolve
        // operation, and carries no client id (:1038-1073 needs no change).
        let a = bookmark("client-a", ROOT_VERSION, "A");
        let b = bookmark("client-b", ROOT_VERSION, "B");
        let a_operation_id = a.operation_id().to_string();
        let mut ledger = ledger_with(vec![a, b]);
        let conflict_id = fold_current_objects(&ledger).unwrap()[0]
            .conflict
            .as_ref()
            .unwrap()
            .conflict_id
            .clone();
        let resolve = SourceOperation::ResolveCurrent {
            operation_id: "resolve-1".to_string(),
            object_key: object_key("emporium-bookmark", "Bookmark", "same"),
            conflict_id,
            chosen_operation_id: Some(a_operation_id),
            record: None,
        };
        let digest = operation_digest(&resolve).unwrap();
        ledger.operations.insert(
            resolve.operation_id().to_string(),
            LedgerOperation {
                digest,
                accepted_revision: 3,
                status: ReceiptStatus::Accepted,
                operation: resolve,
                outcome: json!({}),
                effect_error: None,
            },
        );
        let fold = fold_current_objects(&ledger).unwrap();
        assert_eq!(fold[0].face.operation_id, "resolve-1");
        assert_eq!(fold[0].face.client_id, None);
        assert!(fold[0].face.conflict_id.is_none(), "resolved objects are no longer contested");
    }

    #[test]
    fn attribution_does_not_disturb_delivery_order_independence() {
        // The D2 guard: a future arrival-derived field would fail exactly
        // this assertion the way `accepted_revision` would.
        let ab = ledger_with(vec![
            bookmark_attributed("client-a", ROOT_VERSION, "A", Some("device-a"), Some(10), None),
            bookmark_attributed("client-b", ROOT_VERSION, "B", Some("device-b"), Some(20), None),
        ]);
        let ba = ledger_with(vec![
            bookmark_attributed("client-b", ROOT_VERSION, "B", Some("device-b"), Some(20), None),
            bookmark_attributed("client-a", ROOT_VERSION, "A", Some("device-a"), Some(10), None),
        ]);
        let ab_fold = fold_current_objects(&ab).unwrap();
        let ba_fold = fold_current_objects(&ba).unwrap();
        assert_eq!(
            serde_json::to_value(&ab_fold[0].face).unwrap(),
            serde_json::to_value(&ba_fold[0].face).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(ab_fold[0].conflict.as_ref().unwrap()).unwrap(),
            serde_json::to_value(ba_fold[0].conflict.as_ref().unwrap()).unwrap(),
        );
    }

    #[test]
    fn conflict_triples_emit_one_node_per_candidate() {
        let ledger = ledger_with(vec![
            bookmark("client-a", ROOT_VERSION, "A"),
            bookmark("client-b", ROOT_VERSION, "B"),
        ]);
        let fold = fold_current_objects(&ledger).unwrap();
        let conflicts: Vec<SyncConflict> = fold.into_iter().filter_map(|f| f.conflict).collect();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].candidates.len(), 2);
        let projection = conflict_triples("g", &conflicts).unwrap();

        let candidate_type_nt = format!("<{SYNC_CONFLICT_CANDIDATE_TYPE}>");
        let candidate_nodes: BTreeSet<&String> = projection
            .candidates
            .iter()
            .filter(|(_, p, o)| p == RDF_TYPE && o.as_nt() == candidate_type_nt)
            .map(|(s, _, _)| s)
            .collect();
        assert_eq!(candidate_nodes.len(), 2, "one SyncConflictCandidate node per candidate");

        for subject in &candidate_nodes {
            let has = |pred: &str| {
                projection
                    .candidates
                    .iter()
                    .any(|(s, p, _)| s == *subject && p == pred)
            };
            assert!(has(&format!("{SYNC_NS}candidateOperationId")));
            assert!(has(&format!("{SYNC_NS}candidateVersion")));
            assert!(has(&format!("{SYNC_NS}candidateBaseVersion")));
            assert!(has(&format!("{SYNC_NS}candidateRecordDigest")));
            assert!(has(&format!("{SYNC_NS}conflict")));
        }

        let candidate_edges = projection
            .conflicts
            .iter()
            .filter(|(_, p, _)| p == &format!("{SYNC_NS}candidate"))
            .count();
        assert_eq!(candidate_edges, 2);

        let count_triple = projection
            .conflicts
            .iter()
            .find(|(_, p, _)| p == &format!("{SYNC_NS}candidateCount"))
            .expect("candidateCount triple");
        assert_eq!(count_triple.2.as_nt(), format!("\"2\"^^<{XSD_NS}long>"));

        let projected_edges: Vec<&Triple> = projection
            .conflicts
            .iter()
            .filter(|(_, p, _)| p == &format!("{SYNC_NS}projectedCandidate"))
            .collect();
        assert_eq!(projected_edges.len(), 1);
        let projected_target = projected_edges[0].2.as_nt();
        assert!(
            candidate_nodes
                .iter()
                .any(|subject| format!("<{subject}>") == projected_target),
            "projectedCandidate must point at one of the emitted candidate nodes"
        );
    }

    #[test]
    fn conflict_triples_omit_absent_attribution() {
        let ledger = ledger_with(vec![
            bookmark("client-a", ROOT_VERSION, "A"),
            bookmark("client-b", ROOT_VERSION, "B"),
        ]);
        let fold = fold_current_objects(&ledger).unwrap();
        let conflict = fold[0].conflict.clone().unwrap();
        let projection = conflict_triples("g", std::slice::from_ref(&conflict)).unwrap();

        assert!(
            !projection
                .candidates
                .iter()
                .any(|(_, p, _)| p == &format!("{SYNC_NS}candidateClientId")),
            "absence must be confessed by absence, not an empty literal"
        );
        assert!(
            !projection
                .candidates
                .iter()
                .any(|(_, p, _)| p == &format!("{SYNC_NS}candidateCausalOrder"))
        );
        assert!(
            !projection
                .candidates
                .iter()
                .any(|(_, p, _)| p == &format!("{SYNC_NS}candidateEvidenceWeight"))
        );
    }

    #[test]
    fn conflict_projection_reclaims_resolved_candidates() {
        let store = oxigraph::store::Store::new().unwrap();
        let ledger = ledger_with(vec![
            bookmark("client-a", ROOT_VERSION, "A"),
            bookmark("client-b", ROOT_VERSION, "B"),
        ]);
        let conflicts: Vec<SyncConflict> = fold_current_objects(&ledger)
            .unwrap()
            .into_iter()
            .filter_map(|f| f.conflict)
            .collect();
        assert_eq!(conflicts.len(), 1);
        let first_pass = materialize_conflicts(&store, "g", &conflicts).unwrap();
        assert!(first_pass > 0, "the first materialize must write triples");

        let graph_name = format!("{}:projection:sync-conflicts", graph_subject("g"));
        let conflict_scope = ClassScope {
            placement: Placement::Named(graph_name.clone()),
            key: SpanKey::Fixed {
                rdf_type: SYNC_CONFLICT_TYPE.to_string(),
            },
            graph_id_conjunct: None,
            subjects: None,
        };
        let candidate_scope = ClassScope {
            placement: Placement::Named(graph_name),
            key: SpanKey::Fixed {
                rdf_type: SYNC_CONFLICT_CANDIDATE_TYPE.to_string(),
            },
            graph_id_conjunct: None,
            subjects: None,
        };
        assert!(!crate::emporium::reconcile::survey_class(&store, &conflict_scope)
            .unwrap()
            .is_empty());
        assert!(!crate::emporium::reconcile::survey_class(&store, &candidate_scope)
            .unwrap()
            .is_empty());

        // The post-resolution state: no live conflicts.
        let second_pass = materialize_conflicts(&store, "g", &[]).unwrap();
        assert!(second_pass > 0, "reclaiming a resolved conflict is itself an op");
        assert!(
            crate::emporium::reconcile::survey_class(&store, &conflict_scope)
                .unwrap()
                .is_empty(),
            "the SyncConflict span must be empty after resolution"
        );
        assert!(
            crate::emporium::reconcile::survey_class(&store, &candidate_scope)
                .unwrap()
                .is_empty(),
            "the SyncConflictCandidate span must be empty after resolution — this is the \
             reconcile-span regression guard for D4/reconcile_classes"
        );
    }

    #[test]
    fn conflict_projection_reclaims_legacy_flat_predicates() {
        let store = oxigraph::store::Store::new().unwrap();
        let graph_name = format!("{}:projection:sync-conflicts", graph_subject("g"));
        let legacy_subject = format!("{graph_name}:conflict-legacy");
        let seed = format!(
            "INSERT DATA {{ GRAPH <{graph_name}> {{ \
                <{legacy_subject}> a <{SYNC_CONFLICT_TYPE}> . \
                <{legacy_subject}> <{SYNC_NS}candidateOperationId> \"legacy-op\" . \
                <{legacy_subject}> <{SYNC_NS}candidateVersion> \"legacy-version\" . \
            }} }}"
        );
        oxigraph::sparql::SparqlEvaluator::new()
            .parse_update(&seed)
            .unwrap()
            .on_store(&store)
            .execute()
            .unwrap();

        let ledger = ledger_with(vec![
            bookmark("client-a", ROOT_VERSION, "A"),
            bookmark("client-b", ROOT_VERSION, "B"),
        ]);
        let conflicts: Vec<SyncConflict> = fold_current_objects(&ledger)
            .unwrap()
            .into_iter()
            .filter_map(|f| f.conflict)
            .collect();
        materialize_conflicts(&store, "g", &conflicts).unwrap();

        let conflict_scope = ClassScope {
            placement: Placement::Named(graph_name),
            key: SpanKey::Fixed {
                rdf_type: SYNC_CONFLICT_TYPE.to_string(),
            },
            graph_id_conjunct: None,
            subjects: None,
        };
        let survey = crate::emporium::reconcile::survey_class(&store, &conflict_scope).unwrap();
        assert!(
            !survey.iter().any(|(s, _, _)| *s == legacy_subject),
            "the legacy flat-predicate subject must be reclaimed: {survey:?}"
        );
        assert!(
            !survey
                .iter()
                .any(|(_, p, _)| p == &format!("{SYNC_NS}candidateOperationId")),
            "the flat candidateOperationId predicate must not survive a rebuild: {survey:?}"
        );
    }

    #[test]
    fn conflict_projection_is_convergent() {
        let store = oxigraph::store::Store::new().unwrap();
        // A synthetic conflict with plain-ASCII field values — deliberately
        // NOT routed through `fold_current_objects`/`object_key`, to isolate
        // THIS test's claim (the `reconcile_classes` two-scope composition
        // this materializer now uses converges to zero ops) from an
        // unrelated, PRE-EXISTING defect this test uncovered: `object_key`
        // (`source_sync.rs`) embeds a real U+001F separator, which
        // `emporium::survey::parse_term`'s `unescape_nt` does not decode
        // (`\uXXXX` is absent from its escape table) — so a REAL
        // `sync#objectKey` literal never round-trips byte-identically
        // through a survey, and a graph with a live Law IV conflict would
        // show nonzero `conflictRdfOperations` on every rebuild forever,
        // independent of anything Slice 0 touches. `emporium/survey.rs` is
        // outside this slice's file inventory; flagged in the build log,
        // not fixed here.
        let conflict = SyncConflict {
            conflict_id: "conflict-test".to_string(),
            object_key: "vocab-class-object".to_string(),
            base_version: ROOT_VERSION.to_string(),
            reconciliation_strategy: "contested".to_string(),
            reason: "concurrent current-state candidates share one observed base".to_string(),
            candidates: vec![
                CurrentCandidateFace {
                    operation_id: "op-a".to_string(),
                    source_version: "version-a".to_string(),
                    base_version: ROOT_VERSION.to_string(),
                    record: json!({"title": "A"}),
                    client_id: None,
                    causal_order: None,
                    evidence_weight: None,
                },
                CurrentCandidateFace {
                    operation_id: "op-b".to_string(),
                    source_version: "version-b".to_string(),
                    base_version: ROOT_VERSION.to_string(),
                    record: json!({"title": "B"}),
                    client_id: None,
                    causal_order: None,
                    evidence_weight: None,
                },
            ],
            projected_operation_id: "op-a".to_string(),
        };
        let conflicts = vec![conflict];
        let first = materialize_conflicts(&store, "g", &conflicts).unwrap();
        assert!(first > 0);
        let second = materialize_conflicts(&store, "g", &conflicts).unwrap();
        assert_eq!(second, 0, "a converged class emits zero ops on the next pass");
    }

    #[test]
    fn conflicts_digest_matches_canonical_conflicts() {
        let ab = ledger_with(vec![
            bookmark("client-a", ROOT_VERSION, "A"),
            bookmark("client-b", ROOT_VERSION, "B"),
        ]);
        let ba = ledger_with(vec![
            bookmark("client-b", ROOT_VERSION, "B"),
            bookmark("client-a", ROOT_VERSION, "A"),
        ]);
        let ab_conflicts: Vec<SyncConflict> = fold_current_objects(&ab)
            .unwrap()
            .into_iter()
            .filter_map(|f| f.conflict)
            .collect();
        let ba_conflicts: Vec<SyncConflict> = fold_current_objects(&ba)
            .unwrap()
            .into_iter()
            .filter_map(|f| f.conflict)
            .collect();
        let ab_digest = value_digest(&serde_json::to_value(&ab_conflicts).unwrap()).unwrap();
        let ba_digest = value_digest(&serde_json::to_value(&ba_conflicts).unwrap()).unwrap();
        assert_eq!(
            ab_digest, ba_digest,
            "conflictsDigest is the same authority statement regardless of arrival order"
        );

        let single_writer = ledger_with(vec![bookmark("client-a", ROOT_VERSION, "A")]);
        let no_conflicts: Vec<SyncConflict> = fold_current_objects(&single_writer)
            .unwrap()
            .into_iter()
            .filter_map(|f| f.conflict)
            .collect();
        let empty_digest = value_digest(&serde_json::to_value(&no_conflicts).unwrap()).unwrap();
        assert_ne!(ab_digest, empty_digest, "the digest is sensitive to the contested set");
    }

    // ── Slice 0 / WS4 Ask B — typed error codes ──

    #[test]
    fn app_error_codes_are_unique_and_snake_case() {
        let all = crate::app_error_codes::ALL;
        assert_eq!(all.len(), 16, "the closed taxonomy includes pull capacity and unavailable source bodies");
        assert!(all.contains(&crate::app_error_codes::SOURCE_BODY_UNAVAILABLE));
        let unique: BTreeSet<&str> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "app_error_codes::ALL must have no duplicates");
        for code in all {
            let snake_case = code
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_lowercase())
                && code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            assert!(snake_case, "code {code:?} must match ^[a-z][a-z0-9_]*$");
        }
    }

    #[test]
    fn coded_conflict_survives_message_rewrite() {
        let error = AppError::conflict("original message")
            .with_code(app_error_codes::STALE_GRAPH_INCARNATION)
            .with_message("rewritten message");
        assert_eq!(error.code(), Some(app_error_codes::STALE_GRAPH_INCARNATION));
        assert_eq!(error.message_ref(), "rewritten message");
        assert_eq!(error.kind(), crate::app_error::AppErrorKind::Conflict);
    }

    #[test]
    fn missing_base_conflict_always_names_a_candidate_it_projects() {
        // D19: a normal, fork-free chain head coexists with an unrelated
        // orphan candidate for the same object key. Verified defect at
        // (pre-fix) `:1101-1153` — `projected` named the chain head while
        // `candidates` held only the orphan.
        let chain = bookmark("chain-1", ROOT_VERSION, "chain-head");
        let orphan = bookmark("orphan-1", "not-yet-present", "orphan");
        let ledger = ledger_with(vec![chain, orphan]);
        let fold = fold_current_objects(&ledger).unwrap();
        assert_eq!(fold.len(), 1);
        let conflict = fold[0].conflict.as_ref().expect("missing base is explicit");
        assert!(
            conflict
                .candidates
                .iter()
                .any(|candidate| candidate.operation_id == conflict.projected_operation_id),
            "projectedOperationId must name a member of candidates: {conflict:?}"
        );
        assert_eq!(conflict.projected_operation_id, "chain-1");
        assert_eq!(conflict.candidates.len(), 2);
        assert_eq!(fold[0].face.record["title"], json!("chain-head"));
    }

    #[test]
    fn causal_order_outside_js_safe_integer_range_is_rejected() {
        let ledger = SourceLedger::empty("g", "inc");
        const BOUND: i64 = 9_007_199_254_740_991;

        let too_high = bookmark_attributed("op-1", ROOT_VERSION, "A", None, Some(BOUND + 1), None);
        let digest = operation_digest(&too_high).unwrap();
        let error = validate_source_operation("g", &ledger, &too_high, &digest).unwrap_err();
        assert_eq!(error.kind(), crate::app_error::AppErrorKind::Validation);
        assert_eq!(error.code(), None, "an out-of-range causalOrder is a client bug, not a coded fact");

        let too_low = bookmark_attributed("op-2", ROOT_VERSION, "A", None, Some(-(BOUND + 1)), None);
        let digest = operation_digest(&too_low).unwrap();
        assert!(validate_source_operation("g", &ledger, &too_low, &digest).is_err());

        let at_high_boundary = bookmark_attributed("op-3", ROOT_VERSION, "A", None, Some(BOUND), None);
        let digest = operation_digest(&at_high_boundary).unwrap();
        assert!(validate_source_operation("g", &ledger, &at_high_boundary, &digest).is_ok());

        let at_low_boundary = bookmark_attributed("op-4", ROOT_VERSION, "A", None, Some(-BOUND), None);
        let digest = operation_digest(&at_low_boundary).unwrap();
        assert!(validate_source_operation("g", &ledger, &at_low_boundary, &digest).is_ok());
    }

    #[test]
    fn candidate_evidence_weight_survives_round_trip_at_full_precision() {
        let a = bookmark_attributed("client-a", ROOT_VERSION, "A", Some("device-a"), None, Some(0.0000004));
        let b = bookmark_attributed("client-b", ROOT_VERSION, "B", Some("device-b"), None, Some(0.0000003));
        let ledger = ledger_with(vec![a, b]);
        let conflict = fold_current_objects(&ledger).unwrap()[0].conflict.clone().unwrap();
        let projection = conflict_triples("g", std::slice::from_ref(&conflict)).unwrap();

        let mut client_by_subject: std::collections::BTreeMap<&String, String> = Default::default();
        for (s, p, o) in &projection.candidates {
            if p == &format!("{SYNC_NS}candidateClientId") {
                client_by_subject.insert(s, o.as_nt());
            }
        }
        let mut weight_by_client: std::collections::BTreeMap<String, String> = Default::default();
        for (s, p, o) in &projection.candidates {
            if p == &format!("{SYNC_NS}candidateEvidenceWeight") {
                if let Some(client_nt) = client_by_subject.get(s) {
                    weight_by_client.insert(client_nt.clone(), o.as_nt());
                }
            }
        }
        assert_eq!(weight_by_client.len(), 2);
        let weight_a = weight_by_client
            .iter()
            .find(|(client, _)| client.contains("device-a"))
            .map(|(_, weight)| weight.clone())
            .expect("device-a's evidence weight is present");
        let weight_b = weight_by_client
            .iter()
            .find(|(client, _)| client.contains("device-b"))
            .map(|(_, weight)| weight.clone())
            .expect("device-b's evidence weight is present");
        assert_ne!(
            weight_a, weight_b,
            "two distinct evidence weights that agree to six decimals must not render as an \
             identical RDF literal (D20) — {{:.6}} would collapse both to \"0.000000\""
        );

        for (nt, expected) in [(&weight_a, 0.0000004_f64), (&weight_b, 0.0000003_f64)] {
            match crate::emporium::survey::parse_term(nt) {
                Term::Lit(literal) => {
                    let value: f64 = literal.value().parse().expect("literal value parses as f64");
                    assert_eq!(
                        value.to_bits(),
                        expected.to_bits(),
                        "round-trip through parse_term must preserve the exact f64 bit pattern"
                    );
                }
                other => panic!("expected a literal term, got {other:?}"),
            }
        }
    }

    #[test]
    fn stale_document_incarnation_excludes_local_storage_faults() {
        use crate::document_incarnation_store::ensure_document_incarnation_id_with_requested;

        let root = std::env::temp_dir().join(format!("sophia-d21-test-{}", Uuid::new_v4()));
        let graph_dir = root.join("graph-a");
        let document_id = "doc-a";
        let document_path = crate::paths::document_dir(&graph_dir, document_id).unwrap();
        std::fs::create_dir_all(&document_path).unwrap();

        // A genuine Fault: a corrupt on-disk sidecar UUID — never coded.
        std::fs::write(document_path.join(".incarnation-id"), b"not-a-uuid").unwrap();
        let fault = ensure_document_incarnation_id_with_requested(&graph_dir, document_id, None)
            .expect_err("a corrupt sidecar UUID must fail");
        let coded = document_incarnation_fault_to_app_error(fault, document_id);
        assert_eq!(
            coded.code(),
            None,
            "a local storage fault must never be coded stale_document_incarnation"
        );
        assert_eq!(coded.kind(), crate::app_error::AppErrorKind::Storage);

        // A genuine Mismatch: a real incarnation already exists and the
        // caller requests a different one — the ONE case that codes.
        std::fs::remove_file(document_path.join(".incarnation-id")).unwrap();
        let actual = ensure_document_incarnation_id_with_requested(&graph_dir, document_id, None)
            .expect("mint a fresh incarnation");
        let requested = Uuid::new_v4().to_string();
        assert_ne!(actual, requested);
        let mismatch =
            ensure_document_incarnation_id_with_requested(&graph_dir, document_id, Some(&requested))
                .expect_err("a different requested incarnation must be a genuine mismatch");
        let coded = document_incarnation_fault_to_app_error(mismatch, document_id);
        assert_eq!(coded.code(), Some(app_error_codes::STALE_DOCUMENT_INCARNATION));
        assert_eq!(coded.kind(), crate::app_error::AppErrorKind::Conflict);

        std::fs::remove_dir_all(&root).ok();
    }

    /// The twin of `emporium_mcp_surface.rs`'s
    /// `object_not_found_is_coded_only_for_absence` (R18) — this file's own
    /// `source_object_error` mapper (`:3166-3173` in the spec's line
    /// numbering) was flagged as "never listed" and must not drift from its
    /// sibling.
    #[test]
    fn source_object_error_codes_only_absence_not_misconfiguration() {
        use crate::emporium::objects::ObjectError;
        let misconfigured = source_object_error(ObjectError::NotFound(
            "vocab 'v': unknown vocabulary".to_string(),
        ));
        assert_eq!(misconfigured.code(), None);
        let absent =
            source_object_error(ObjectError::Absent("no object <urn:x> in <urn:sink>".to_string()));
        assert_eq!(absent.code(), Some(app_error_codes::OBJECT_NOT_FOUND));
    }
}

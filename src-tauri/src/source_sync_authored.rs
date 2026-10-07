//! Direct writers on graphs under source authority (ruled 2026-10-06 12:05
//! PDT: fix it in this release; notes kept outside this repository).
//!
//! Once a graph's source ledger exists, every ledger write rebuilds the
//! projections: it restores the checkpoint taken at activation and replays the
//! Y.Docs and the ledger. Writers that bypass the ledger (`sparql_update`,
//! `rdf_load`, `revaluate`, the salience user-value and configuration routes,
//! the emporium ingest and object routes, ...) used to be rewound by that
//! rebuild. Here they are recorded in the ledger instead, as one *authored
//! overlay* per graph (`SourceLedger::authored`):
//!
//! * **RDF**: the net quad delta, as canonical N-Quads lines, in the graphs the
//!   rebuild does not re-derive (see [`authored_graph`]). Each direct write is
//!   captured exactly: under the source gate the graphs it can reach are read
//!   before and after it, and the difference is composed into the overlay.
//!   Composition keeps only the net effect, so a value rewritten a thousand
//!   times (the web app writes its layout once per drag) costs one line, and a
//!   write that is undone leaves nothing behind.
//! * **Values**: the fields of each value store that only direct writers
//!   change (the configuration, its history, and user valuations), as last
//!   written. Valuation events stay ledger operations; these fields never
//!   come from them.
//!
//! A rebuild restores the checkpoint, replays the ledger, then applies the
//! overlay. As a last line of defence it compares those graphs and fields with
//! what they were, live, before the rebuild, and adopts any difference into
//! the overlay, except on subjects the ledger itself materializes (where the
//! ledger stays the authority). A writer that is not routed here, or a write
//! made before this change was deployed, is therefore kept and recorded by the
//! next rebuild instead of being erased.
//!
//! Graphs not under source authority are untouched: the wrapper runs the
//! writer exactly as before (holding the source gate, so a concurrent first
//! `source_pull` checkpoints either before the write or after it).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{GraphName, NamedOrBlankNode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(feature = "desktop")]
use tauri::Manager;

use super::{
    acquire_source_gate, active_graph_identity, capture_value_store_files, ledger_path,
    read_ledger, source_authority_active, write_ledger, SourceLedger,
};
use crate::app_error::{AppError, AppResult};
use crate::app_error_codes;
use crate::app_runtime::AppHandle;
use crate::crdt_engine::persistence_coordinator::{GraphPersistenceCoordinator, HotWriteLease};
use crate::rdf::graph_subject;
use crate::rdf_authority::is_reserved_rdf_graph_iri;
use crate::rdf_service::{ensure_graph_store_seeded, open_graph_store};
use crate::salience_value_store::{
    read_value_store_for, write_value_store, LocalBlockValueRecord, LocalValueStore,
};

/// What a direct writer can change, and so what is read around it.
#[derive(Debug, Clone)]
pub(crate) enum AuthoredScope {
    /// The graphs a SPARQL update or an RDF load may target: the default graph
    /// and every graph `rdf_authority` does not reserve for the engine.
    UserGraphs,
    /// Every graph a rebuild does not re-derive: the user graphs plus the
    /// engine projections that only direct writers fill (`:projection:chamber`,
    /// `:projection:violations`, the song and archive documents, the emporium
    /// sinks, ...). See [`authored_graph`].
    Authored,
    /// The configuration, its history and the user valuations of one value
    /// store (`""` is the shared commons).
    Values(String),
}

/// `{graph}:projection:{family}[:...]` families that a rebuild re-derives
/// from the Y.Docs, the ledger, the memory event log, the value stores or the
/// graph record (or that are disposable indexes rebuilt elsewhere, which the
/// overlay must not copy). Every other projection graph is preserved.
const DERIVED_PROJECTION_FAMILIES: &[&str] = &[
    "workspace",
    "document",
    "graph",
    "pdf-source",
    "seed",
    "flow",
    "memory",
    "salience",
    "sync-conflicts",
    "file-views",
    "ludus",
    "semantic",
    "kg-ultra",
    "lme-labeled-memory",
    "obs",
];

/// Document projections written out of band (song and memory-archive
/// documents have no Y.Doc to rebuild them from).
const PRESERVED_DOCUMENT_PREFIX: &str = "document:geist-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RdfReach {
    UserGraphs,
    Authored,
}

fn user_graph(graph_id: &str, graph: &NamedOrBlankNode) -> bool {
    match graph {
        NamedOrBlankNode::NamedNode(node) => !is_reserved_rdf_graph_iri(graph_id, node.as_str()),
        NamedOrBlankNode::BlankNode(_) => true,
    }
}

/// `true` for a named graph whose content a rebuild does not re-derive.
fn authored_graph(graph_id: &str, graph: &NamedOrBlankNode) -> bool {
    let NamedOrBlankNode::NamedNode(node) = graph else {
        return true;
    };
    let iri = node.as_str();
    if !is_reserved_rdf_graph_iri(graph_id, iri) {
        return true;
    }
    let projection_prefix = format!("{}:projection:", graph_subject(graph_id));
    let Some(suffix) = iri.strip_prefix(projection_prefix.as_str()) else {
        // The graph root itself and profile graphs.
        return false;
    };
    if suffix.starts_with(PRESERVED_DOCUMENT_PREFIX) {
        return true;
    }
    let family = suffix.split(':').next().unwrap_or(suffix);
    !DERIVED_PROJECTION_FAMILIES.contains(&family)
}

/// Every quad of the default graph and of the named graphs in `reach`, as
/// canonical N-Quads lines (the exact line Oxigraph's serializer writes).
fn rdf_lines(
    store: &oxigraph::store::Store,
    graph_id: &str,
    reach: RdfReach,
) -> AppResult<BTreeSet<String>> {
    let mut graphs = vec![GraphName::DefaultGraph];
    for graph in store.named_graphs() {
        let graph = graph.map_err(|error| AppError::rdf(format!("list named graphs: {error}")))?;
        let keep = match reach {
            RdfReach::UserGraphs => user_graph(graph_id, &graph),
            RdfReach::Authored => authored_graph(graph_id, &graph),
        };
        if keep {
            graphs.push(GraphName::from(graph));
        }
    }
    let mut lines = BTreeSet::new();
    for graph in &graphs {
        for quad in store.quads_for_pattern(None, None, None, Some(graph.as_ref())) {
            let quad =
                quad.map_err(|error| AppError::rdf(format!("read authored RDF: {error}")))?;
            lines.insert(format!("{quad} ."));
        }
    }
    Ok(lines)
}

/// `(removed, added)` from `before` to `after`.
fn delta(
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    (
        before.difference(after).cloned().collect(),
        after.difference(before).cloned().collect(),
    )
}

/// The subject of one N-Quads line: the bare IRI of `<iri>`, or the blank
/// node label as written.
fn line_subject(line: &str) -> &str {
    match line.strip_prefix('<') {
        Some(rest) => rest.split('>').next().unwrap_or(""),
        None => line.split(' ').next().unwrap_or(""),
    }
}

fn parse_lines(lines: &BTreeSet<String>) -> AppResult<Vec<oxigraph::model::Quad>> {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    RdfParser::from_format(RdfFormat::NQuads)
        .for_slice(text.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            AppError::conflict(format!(
                "authored RDF in the source ledger is not valid N-Quads: {error}"
            ))
            .with_code(app_error_codes::LEDGER_INTEGRITY)
        })
}

/// Remove then insert, in one transaction. The two sets are disjoint.
fn apply_rdf(
    store: &oxigraph::store::Store,
    removed: &BTreeSet<String>,
    added: &BTreeSet<String>,
) -> AppResult<()> {
    if removed.is_empty() && added.is_empty() {
        return Ok(());
    }
    let removes = parse_lines(removed)?;
    let adds = parse_lines(added)?;
    let mut transaction = store
        .start_transaction()
        .map_err(|error| AppError::rdf(format!("start authored RDF replay: {error}")))?;
    for quad in &removes {
        transaction.remove(quad);
    }
    for quad in &adds {
        transaction.insert(quad);
    }
    transaction
        .commit()
        .map_err(|error| AppError::rdf(format!("commit authored RDF replay: {error}")))?;
    crate::cell_durability::mark_rdf_store_written(store);
    Ok(())
}

/// The value-store fields only direct writers change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AuthoredValueFields {
    config: Value,
    config_history: Value,
    #[serde(default)]
    user: BTreeMap<String, AuthoredUserValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthoredUserValue {
    document_id: String,
    block_id: String,
    #[serde(default)]
    importance: Option<f64>,
    #[serde(default)]
    valence: Option<f64>,
}

/// A record that exists only because a user valuation created it and was then
/// cleared: no event ever touched it.
fn placeholder(record: &LocalBlockValueRecord) -> bool {
    record.raw_importance_sum == 0.0
        && record.raw_valence_sum == 0.0
        && record.cumulative_importance == 0.0
        && record.cumulative_valence == 0.0
        && record.importance_count == 0
        && record.valence_count == 0
        && record.valuation_count == 0
        && record.tags.is_empty()
        && record.last_valuated_at.is_empty()
}

impl AuthoredValueFields {
    fn capture(store: &LocalValueStore) -> AppResult<Self> {
        let user = store
            .blocks
            .iter()
            .filter(|(_, record)| {
                record.user_importance.is_some()
                    || record.user_valence.is_some()
                    || placeholder(record)
            })
            .map(|(key, record)| {
                (
                    key.clone(),
                    AuthoredUserValue {
                        document_id: record.document_id.clone(),
                        block_id: record.block_id.clone(),
                        importance: record.user_importance,
                        valence: record.user_valence,
                    },
                )
            })
            .collect();
        Ok(Self {
            config: serde_json::to_value(&store.config).map_err(|error| {
                AppError::serialization(format!("serialize value configuration: {error}"))
            })?,
            config_history: serde_json::to_value(&store.config_history).map_err(|error| {
                AppError::serialization(format!("serialize value configuration history: {error}"))
            })?,
            user,
        })
    }

    fn apply_to(&self, store: &mut LocalValueStore) -> AppResult<()> {
        let integrity = |what: &str, error: serde_json::Error| {
            AppError::conflict(format!("{what} in the source ledger: {error}"))
                .with_code(app_error_codes::LEDGER_INTEGRITY)
        };
        store.config = serde_json::from_value(self.config.clone())
            .map_err(|error| integrity("value configuration", error))?;
        store.config_history = serde_json::from_value(self.config_history.clone())
            .map_err(|error| integrity("value configuration history", error))?;
        for record in store.blocks.values_mut() {
            record.user_importance = None;
            record.user_valence = None;
        }
        for (key, user) in &self.user {
            let record = store
                .blocks
                .entry(key.clone())
                .or_insert_with(|| LocalBlockValueRecord {
                    document_id: user.document_id.clone(),
                    block_id: user.block_id.clone(),
                    ..LocalBlockValueRecord::default()
                });
            record.user_importance = user.importance;
            record.user_valence = user.valence;
        }
        Ok(())
    }
}

fn capture_values(
    graph_dir: &Path,
    graph_id: &str,
    observer: &str,
) -> AppResult<AuthoredValueFields> {
    let store = read_value_store_for(graph_dir, graph_id, observer).map_err(AppError::storage)?;
    AuthoredValueFields::capture(&store)
}

/// The authored fields of every value store present on disk, by observer.
fn capture_all_values(
    graph_dir: &Path,
    graph_id: &str,
) -> AppResult<BTreeMap<String, AuthoredValueFields>> {
    let mut all = BTreeMap::new();
    for data in capture_value_store_files(graph_dir)?.into_values() {
        let observer = serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|store| {
                store
                    .get("observer")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let fields = capture_values(graph_dir, graph_id, &observer)?;
        all.insert(observer, fields);
    }
    Ok(all)
}

/// Overwrite the authored fields of each named value store and reconcile its
/// salience projection.
fn apply_values(
    graph_dir: &Path,
    graph_id: &str,
    values: &BTreeMap<String, AuthoredValueFields>,
) -> AppResult<()> {
    for (observer, fields) in values {
        let mut store =
            read_value_store_for(graph_dir, graph_id, observer).map_err(AppError::storage)?;
        fields.apply_to(&mut store)?;
        write_value_store(graph_dir, &store).map_err(AppError::storage)?;
        crate::salience_rdf_materializer::reconcile_value_store(graph_dir, &store)
            .map_err(AppError::rdf)?;
    }
    Ok(())
}

/// The ledger's record of direct writes (`SourceLedger::authored`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AuthoredOverlay {
    /// N-Quads lines the replay removes after the ledger has been replayed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    removed: BTreeSet<String>,
    /// N-Quads lines the replay adds after the ledger has been replayed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    added: BTreeSet<String>,
    /// Authored value-store fields, by observer (`""` is the commons).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    values: BTreeMap<String, AuthoredValueFields>,
    /// Recorded writes by origin (audit only; replay never reads it).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    writes: BTreeMap<String, u64>,
    /// Rebuilds that had to adopt a difference they did not see recorded.
    #[serde(default)]
    adoptions: u64,
    #[serde(default)]
    updated_at_ms: i64,
}

impl AuthoredOverlay {
    /// Compose a further delta `(removed, added)`, taken from the state this
    /// overlay produces, into the overlay. The result is the net delta from
    /// the state before the first recorded write, so superseded states vanish:
    /// `A' = (A - R) ∪ (A_new - R_old)`, `R' = (R_old - A_new) ∪ (R - A_old)`.
    fn compose_rdf(&mut self, removed: &BTreeSet<String>, added: &BTreeSet<String>) {
        let next_added = self
            .added
            .difference(removed)
            .chain(added.difference(&self.removed))
            .cloned()
            .collect::<BTreeSet<_>>();
        let next_removed = self
            .removed
            .difference(added)
            .chain(removed.difference(&self.added))
            .cloned()
            .collect::<BTreeSet<_>>();
        self.added = next_added;
        self.removed = next_removed;
    }
}

async fn hot_write_lease(app: &AppHandle, graph_id: &str) -> AppResult<Option<HotWriteLease>> {
    match app.try_state::<GraphPersistenceCoordinator>() {
        Some(coordinator) => coordinator
            .acquire_hot_write(graph_id)
            .await
            .map(Some)
            .map_err(AppError::storage),
        None => Ok(None),
    }
}

fn scan(app: &AppHandle, graph_id: &str, reach: RdfReach) -> AppResult<BTreeSet<String>> {
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    ensure_graph_store_seeded(&graph_dir).map_err(AppError::rdf)?;
    let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
    rdf_lines(&store, graph_id, reach)
}

/// `true` once the ledger has accepted an operation. Until then nothing has
/// been replayed over its checkpoint, and the next ledger write re-takes the
/// checkpoint from the live store (`refloor_empty_ledger`), so a direct write
/// needs no record: exactly as on a graph that was never activated. This is
/// the Phanes shape (service graphs written only through SPARQL updates, in
/// large batches, whose ledgers have never held an operation): recording each
/// batch would scan the graph and rewrite the ledger per batch. An unreadable
/// ledger counts as holding operations, so the write is recorded (or its
/// recording fails loudly).
fn ledger_has_operations(app: &AppHandle, graph_id: &str) -> bool {
    let Ok(graph_dir) = crate::paths::existing_graph_dir(app, graph_id) else {
        return false;
    };
    match crate::storage::read_json::<SourceLedger>(&ledger_path(&graph_dir)) {
        Ok(ledger) => !ledger.operations.is_empty(),
        Err(_) => true,
    }
}

/// RDF-only writers bump the graph record's `updatedAt` without reconciling
/// its projection (`touch_graph_updated_at`); every rebuild reconciles it. On a
/// graph under source authority, reconcile it right after the write, so the
/// live projection is the one a rebuild reproduces and `source_rebuild` stays
/// `ok`. Best effort: the next rebuild reconciles it anyway.
async fn refresh_graph_record_projection(app: &AppHandle, graph_id: &str) {
    let Ok(lease) = hot_write_lease(app, graph_id).await else {
        return;
    };
    if let Ok((graph_dir, graph_record)) =
        crate::graph_record_store::read_graph_record_no_heal(app, graph_id)
    {
        if let Ok(store) = open_graph_store(&graph_dir) {
            if crate::rdf_record_materializer::reconcile_graph_record(&store, &graph_record).is_ok()
            {
                crate::cell_durability::mark_rdf_store_written(&store);
            }
        }
    }
    drop(lease);
}

fn reach_of(scope: &AuthoredScope) -> Option<RdfReach> {
    match scope {
        AuthoredScope::UserGraphs => Some(RdfReach::UserGraphs),
        AuthoredScope::Authored => Some(RdfReach::Authored),
        AuthoredScope::Values(_) => None,
    }
}

/// Run one direct writer. On a graph under source authority, record what it
/// changed in the ledger's authored overlay, so a rebuild replays it instead
/// of erasing it. On any other graph, run it exactly as before.
///
/// The writer runs while this holds the graph's source gate: no rebuild, pull
/// or push can run between the write and its record. The writer must not take
/// the source gate itself, and no lifecycle lease may be held by the caller.
pub(crate) async fn record_authored_write<T, E, Fut>(
    app: &AppHandle,
    graph_id: &str,
    origin: &'static str,
    scope: AuthoredScope,
    write: Fut,
) -> Result<T, E>
where
    Fut: Future<Output = Result<T, E>>,
    E: From<AppError>,
{
    let gate = acquire_source_gate(graph_id).await;
    if !source_authority_active(app, graph_id).unwrap_or(false) {
        let result = write.await;
        drop(gate);
        return result;
    }
    if !ledger_has_operations(app, graph_id) {
        let result = write.await;
        refresh_graph_record_projection(app, graph_id).await;
        drop(gate);
        return result;
    }
    let before = match reach_of(&scope) {
        Some(reach) => Some(scan(app, graph_id, reach)?),
        None => None,
    };
    let result = write.await;
    // Record whatever the write changed, even when it reports an error: a
    // partly applied write must replay exactly as it stands.
    let recorded = record(app, graph_id, origin, &scope, before.as_ref()).await;
    refresh_graph_record_projection(app, graph_id).await;
    drop(gate);
    let value = result?;
    recorded.map_err(|error| {
        E::from(AppError::internal(format!(
            "the write was applied but could not be recorded in the source ledger \
             (the next rebuild adopts it): {error}"
        )))
    })?;
    Ok(value)
}

async fn record(
    app: &AppHandle,
    graph_id: &str,
    origin: &str,
    scope: &AuthoredScope,
    before: Option<&BTreeSet<String>>,
) -> AppResult<()> {
    let (graph_dir, graph_incarnation) = active_graph_identity(app, graph_id, None)?;
    let mut ledger = read_ledger(&graph_dir, graph_id, &graph_incarnation)?;
    let mut overlay = ledger.authored.clone().unwrap_or_default();
    let changed = match (scope, reach_of(scope), before) {
        (AuthoredScope::Values(observer), _, _) => {
            let fields = capture_values(&graph_dir, graph_id, observer)?;
            if overlay.values.get(observer.as_str()) == Some(&fields) {
                false
            } else {
                overlay.values.insert(observer.clone(), fields);
                true
            }
        }
        (_, Some(reach), Some(before)) => {
            let store = open_graph_store(&graph_dir).map_err(AppError::rdf)?;
            let after = rdf_lines(&store, graph_id, reach)?;
            drop(store);
            let (removed, added) = delta(before, &after);
            if removed.is_empty() && added.is_empty() {
                false
            } else {
                overlay.compose_rdf(&removed, &added);
                true
            }
        }
        _ => {
            return Err(AppError::internal(
                "an authored RDF write was recorded without its prior state",
            ))
        }
    };
    if !changed {
        return Ok(());
    }
    *overlay.writes.entry(origin.to_string()).or_insert(0) += 1;
    overlay.updated_at_ms = chrono::Utc::now().timestamp_millis();
    ledger.authored = Some(overlay);
    let lease = hot_write_lease(app, graph_id).await?;
    write_ledger(&graph_dir, &ledger)?;
    drop(lease);
    Ok(())
}

/// What the authored graphs and value fields were, live, before a rebuild
/// restores the checkpoint.
pub(super) struct LiveAuthored {
    rdf: BTreeSet<String>,
    values: BTreeMap<String, AuthoredValueFields>,
}

pub(super) fn capture_live(
    store: &oxigraph::store::Store,
    graph_dir: &Path,
    graph_id: &str,
) -> AppResult<LiveAuthored> {
    Ok(LiveAuthored {
        rdf: rdf_lines(store, graph_id, RdfReach::Authored)?,
        values: capture_all_values(graph_dir, graph_id)?,
    })
}

/// The last step of a rebuild, after the ledger has been replayed: apply the
/// recorded overlay, then adopt what the live store had that the replay does
/// not reproduce. Lines whose subject the ledger materializes
/// (`owned_subjects`) are never adopted: there the ledger is the authority.
/// Deterministic: a second rebuild finds nothing to adopt and changes nothing.
pub(super) fn replay_and_adopt(
    store: &oxigraph::store::Store,
    graph_dir: &Path,
    graph_id: &str,
    ledger: &mut SourceLedger,
    live: &LiveAuthored,
    owned_subjects: &BTreeSet<String>,
) -> AppResult<Value> {
    let mut overlay = ledger.authored.clone().unwrap_or_default();
    apply_rdf(store, &overlay.removed, &overlay.added)?;
    apply_values(graph_dir, graph_id, &overlay.values)?;
    let recorded = json!({
        "removed": overlay.removed.len(),
        "added": overlay.added.len(),
        "valueStores": overlay.values.len(),
    });

    let replayed = rdf_lines(store, graph_id, RdfReach::Authored)?;
    let not_owned = |line: &&String| !owned_subjects.contains(line_subject(line));
    let adopt_removed = replayed
        .difference(&live.rdf)
        .filter(not_owned)
        .cloned()
        .collect::<BTreeSet<_>>();
    let adopt_added = live
        .rdf
        .difference(&replayed)
        .filter(not_owned)
        .cloned()
        .collect::<BTreeSet<_>>();
    apply_rdf(store, &adopt_removed, &adopt_added)?;
    overlay.compose_rdf(&adopt_removed, &adopt_added);

    let mut adopt_values = BTreeMap::new();
    for (observer, live_fields) in &live.values {
        if &capture_values(graph_dir, graph_id, observer)? != live_fields {
            adopt_values.insert(observer.clone(), live_fields.clone());
        }
    }
    apply_values(graph_dir, graph_id, &adopt_values)?;

    let adopted = !adopt_removed.is_empty() || !adopt_added.is_empty() || !adopt_values.is_empty();
    let adopted_value_stores = adopt_values.keys().cloned().collect::<Vec<String>>();
    let report = json!({
        "recorded": recorded,
        "adopted": adopted,
        "adoptedRemoved": adopt_removed.len(),
        "adoptedAdded": adopt_added.len(),
        "adoptedValueStores": adopted_value_stores,
    });
    if adopted {
        overlay.values.extend(adopt_values);
        overlay.adoptions += 1;
        overlay.updated_at_ms = chrono::Utc::now().timestamp_millis();
    }
    ledger.authored = if overlay == AuthoredOverlay::default() {
        None
    } else {
        Some(overlay)
    };
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(lines: &[&str]) -> BTreeSet<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    /// Apply `(removed, added)` to a set of lines, as the replay does.
    fn applied(
        state: &BTreeSet<String>,
        removed: &BTreeSet<String>,
        added: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        state
            .difference(removed)
            .chain(added.iter())
            .cloned()
            .collect()
    }

    /// Composition equals sequential application from the state the first
    /// write started from, over every small history drawn from a fixed pool
    /// (deterministic, no randomness), and never keeps a line both removed and
    /// added.
    #[test]
    fn authored_overlay_composition_equals_sequential_application() {
        let pool = [
            "<urn:a> <urn:p> \"1\" .",
            "<urn:a> <urn:p> \"2\" .",
            "<urn:b> <urn:p> \"3\" .",
        ];
        // Every state over the pool, as a bitmask.
        let state_of = |mask: u8| -> BTreeSet<String> {
            pool.iter()
                .enumerate()
                .filter(|(index, _)| mask & (1u8 << *index) != 0)
                .map(|(_, line)| line.to_string())
                .collect()
        };
        for start in 0..8u8 {
            for first in 0..8u8 {
                for second in 0..8u8 {
                    for third in 0..8u8 {
                        let states = [
                            state_of(start),
                            state_of(first),
                            state_of(second),
                            state_of(third),
                        ];
                        let mut overlay = AuthoredOverlay::default();
                        for window in states.windows(2) {
                            let (removed, added) = delta(&window[0], &window[1]);
                            overlay.compose_rdf(&removed, &added);
                        }
                        assert!(overlay.removed.is_disjoint(&overlay.added));
                        assert_eq!(
                            applied(&states[0], &overlay.removed, &overlay.added),
                            states[3],
                            "start {start:03b} -> {first:03b} -> {second:03b} -> {third:03b}"
                        );
                        // Net, not history: nothing outside the start/end difference.
                        let (net_removed, net_added) = delta(&states[0], &states[3]);
                        assert_eq!(overlay.removed, net_removed);
                        assert_eq!(overlay.added, net_added);
                    }
                }
            }
        }
    }

    #[test]
    fn authored_overlay_forgets_superseded_rewrites() {
        let mut overlay = AuthoredOverlay::default();
        let mut previous = set(&["<urn:layout> <urn:json> \"v0\" <urn:g> ."]);
        for round in 1..50 {
            let next = set(&[&format!("<urn:layout> <urn:json> \"v{round}\" <urn:g> .")]);
            let (removed, added) = delta(&previous, &next);
            overlay.compose_rdf(&removed, &added);
            previous = next;
        }
        assert_eq!(
            overlay.added,
            set(&["<urn:layout> <urn:json> \"v49\" <urn:g> ."])
        );
        assert_eq!(
            overlay.removed,
            set(&["<urn:layout> <urn:json> \"v0\" <urn:g> ."])
        );
    }

    #[test]
    fn authored_graphs_are_the_ones_a_rebuild_does_not_rederive() {
        let graph =
            |iri: &str| NamedOrBlankNode::NamedNode(oxigraph::model::NamedNode::new(iri).unwrap());
        let root = "urn:mnemosyne:local:graph:g";
        for kept in [
            format!("{root}:user:rdf"),
            format!("{root}:ux:config"),
            format!("{root}:derived:swarm"),
            "urn:custom:graph".to_string(),
            format!("{root}:projection:chamber"),
            format!("{root}:projection:violations"),
            format!("{root}:projection:song"),
            format!("{root}:projection:song:agent:x"),
            format!("{root}:projection:correo"),
            format!("{root}:projection:domain-manifest"),
            format!("{root}:projection:document:geist-memory-archive-1"),
            format!("{root}:projection:document:geist-song"),
        ] {
            assert!(
                authored_graph("g", &graph(&kept)),
                "{kept} must be preserved"
            );
        }
        for derived in [
            root.to_string(),
            format!("{root}:projection:workspace"),
            format!("{root}:projection:document:doc-1"),
            format!("{root}:projection:graph"),
            format!("{root}:projection:memory"),
            format!("{root}:projection:memory:agent:x"),
            format!("{root}:projection:salience"),
            format!("{root}:projection:salience:agent:x"),
            format!("{root}:projection:semantic"),
            format!("{root}:projection:flow"),
            format!("{root}:projection:sync-conflicts"),
            format!("{root}:projection:obs:raw"),
            "urn:mnemosyne:local:profile:p".to_string(),
        ] {
            assert!(
                !authored_graph("g", &graph(&derived)),
                "{derived} is re-derived"
            );
        }
        assert!(user_graph("g", &graph(&format!("{root}:user:rdf"))));
        assert!(!user_graph(
            "g",
            &graph(&format!("{root}:projection:chamber"))
        ));
    }

    #[test]
    fn line_subject_reads_iris_and_blank_nodes() {
        assert_eq!(line_subject("<urn:s> <urn:p> \"o\" <urn:g> ."), "urn:s");
        assert_eq!(line_subject("_:b1 <urn:p> \"o\" ."), "_:b1");
    }

    /// The overlay replays onto a real store exactly, blank nodes included,
    /// and lines read back from the store are the lines that were written.
    #[test]
    fn authored_rdf_lines_round_trip_through_a_store() {
        let store = oxigraph::store::Store::new().unwrap();
        let lines = set(&[
            "<urn:s> <urn:p> _:b1 <urn:mnemosyne:local:graph:g:user:rdf> .",
            "_:b1 <urn:q> \"inner \\\"quoted\\\"\"@en <urn:mnemosyne:local:graph:g:user:rdf> .",
            "<urn:d> <urn:p> \"default\" .",
            "<urn:x> <urn:p> \"skip\" <urn:mnemosyne:local:graph:g:projection:workspace> .",
        ]);
        apply_rdf(&store, &BTreeSet::new(), &lines).unwrap();
        let read = rdf_lines(&store, "g", RdfReach::Authored).unwrap();
        let mut expected = lines.clone();
        expected.remove(
            "<urn:x> <urn:p> \"skip\" <urn:mnemosyne:local:graph:g:projection:workspace> .",
        );
        assert_eq!(read, expected);
        apply_rdf(
            &store,
            &set(&["<urn:d> <urn:p> \"default\" ."]),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(!rdf_lines(&store, "g", RdfReach::UserGraphs)
            .unwrap()
            .contains("<urn:d> <urn:p> \"default\" ."));
    }

    #[test]
    fn authored_value_fields_round_trip_and_clear_what_live_cleared() {
        let graph_dir =
            std::env::temp_dir().join(format!("authored-values-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&graph_dir).unwrap();
        let mut live = read_value_store_for(&graph_dir, "g", "").unwrap();
        live.config.importance_prompt = "live prompt".to_string();
        live.blocks.insert(
            "d:rated".to_string(),
            LocalBlockValueRecord {
                document_id: "d".into(),
                block_id: "rated".into(),
                raw_importance_sum: 3.0,
                importance_count: 1,
                user_importance: Some(5.0),
                ..LocalBlockValueRecord::default()
            },
        );
        live.blocks.insert(
            "d:cleared".to_string(),
            LocalBlockValueRecord {
                document_id: "d".into(),
                block_id: "cleared".into(),
                ..LocalBlockValueRecord::default()
            },
        );
        let fields = AuthoredValueFields::capture(&live).unwrap();

        // A replayed store: the event-derived sums survived, the authored
        // fields are the checkpoint's (a stale user value on another block).
        let mut replayed = read_value_store_for(&graph_dir, "g", "").unwrap();
        replayed.blocks.insert(
            "d:rated".to_string(),
            LocalBlockValueRecord {
                document_id: "d".into(),
                block_id: "rated".into(),
                raw_importance_sum: 3.0,
                importance_count: 1,
                ..LocalBlockValueRecord::default()
            },
        );
        replayed.blocks.insert(
            "d:stale".to_string(),
            LocalBlockValueRecord {
                document_id: "d".into(),
                block_id: "stale".into(),
                raw_valence_sum: 1.0,
                valence_count: 1,
                user_valence: Some(4.0),
                ..LocalBlockValueRecord::default()
            },
        );
        fields.apply_to(&mut replayed).unwrap();
        assert_eq!(replayed.config.importance_prompt, "live prompt");
        assert_eq!(replayed.blocks["d:rated"].user_importance, Some(5.0));
        assert_eq!(replayed.blocks["d:rated"].raw_importance_sum, 3.0);
        assert!(replayed.blocks.contains_key("d:cleared"));
        assert_eq!(replayed.blocks["d:stale"].user_valence, None);
        assert_eq!(replayed.blocks["d:stale"].raw_valence_sum, 1.0);
        assert_eq!(AuthoredValueFields::capture(&replayed).unwrap(), fields);
        let _ = std::fs::remove_dir_all(&graph_dir);
    }
}

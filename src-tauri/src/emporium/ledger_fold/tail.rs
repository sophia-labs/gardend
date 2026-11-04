//! The fold host: one task per hosted ledger room that folds its frames into a graph sink
//! (W7 section 4, decisions 1.3 to 1.6).
//!
//! [`FoldTail`] keeps the head table of a `(graph, vocab)` ledger in memory and, whenever the room
//! announces a change ([`LedgerRoom::changes`]), waits out a 20 ms debounce, takes the barrier,
//! reads the frames past its watermark ([`LedgerRoom::events_since`]), plans them into units
//! and applies the units to the sink in store transactions that also write the watermark.
//!
//! - **Units.** A frame without a `txn` is a unit of its own. The frames of a transaction run are
//!   one unit that exists only once the run's `commit` frame has arrived; the whole unit applies in
//!   one store transaction, so a reader never sees half of it. A run with no commit is *open* while
//!   it is the last thing its origin wrote, and *abandoned* once a later frame of that origin
//!   follows it: an abandoned run is never applied, the watermark steps over it, and the status
//!   route lists it. A frame that does not decode is *quarantined* (listed, skipped, the watermark
//!   steps over it). Nothing halts the tail.
//! - **Watermark.** Per origin, in the sink's own store ([`super::watermark`]), written in the
//!   transaction that writes the delta. Replaying a frame at or below it is a no-op by construction;
//!   a crash between two transactions loses nothing and applies nothing twice.
//! - **Sinks.** A vocab with a contract folds into its `projection:*` write target, through the
//!   generation pointer a live rebuild moves; a vocab without one (and one whose contract names
//!   no projection) folds into `{graph_subject}:user:rdf`, the graph Nidra reads, touching only the
//!   subjects the ledger has frames for.
//! - **Hosting.** The registry starts a tail when a ledger room is opened for a request
//!   ([`ensure_tail`]); the tail holds the room *weakly*, so an evicted room releases its stream
//!   locks, and stops when the room's evicted flag is set (the room ticks its change channel then).
//!   Everything that must see a consistent fold (oracle, live rebuild, head rows) is a command the
//!   tail runs between cycles.

#![allow(dead_code)] // the routes, the registry hook and the commit seam are the consumers

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use garden_ledger::{Mode, OriginId, VersionVector};
use oxigraph::model::{GraphName, GraphNameRef, NamedNode, NamedOrBlankNode, Quad, Term as OxTerm};
use oxigraph::store::Store;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::instrument::WithSubscriber;

use super::watermark;
use super::{apply_delta, Delta, FoldEvent, FoldPolicy, HeadTable};
use crate::app_runtime::{AppHandle, Manager};
use crate::crdt_engine::convergent::Convergent;
use crate::crdt_engine::ledger_room::{FoldStep, LedgerRoom};
use crate::crdt_engine::persistence_coordinator::{GraphPersistenceCoordinator, HotWriteLease};
use crate::emporium::chamber_ontology::resolve_ingest_contract;
use crate::emporium::terms::{diff_triples, Term, Triple};
use crate::loopback_ledger_routes::ledger_room_key;
use crate::rdf::graph_subject;
use crate::rdf_authority::user_rdf_graph_iri;
use crate::rdf_store_service::open_graph_store;

/// How long the tail coalesces a burst of changes before folding.
const DEBOUNCE: Duration = Duration::from_millis(20);
/// Events per store transaction when catching up (a unit is never split).
const CHUNK_EVENTS: usize = 2_000;
/// Quarantined events remembered for the status route.
const QUARANTINE_CAP: usize = 1_000;
/// First retry delay after a failed cycle; doubles to [`BACKOFF_MAX`].
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// How long a retired sink generation stays readable for queries that already resolved it.
pub(crate) const RETIRE_GRACE: Duration = Duration::from_secs(60);
/// How long a route waits on the tail for a command.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// Sample triples listed in an oracle report.
const REPORT_SAMPLE: usize = 5;
/// A quarantined frame is logged for its (graph, vocab, origin) at most this often (W7b item 10).
const QUARANTINE_LOG_EVERY: Duration = Duration::from_secs(60);

/// Rate limit for the quarantine warning of one (graph, vocab, origin): the first note goes out
/// at once, later ones are counted and go out as one line per window. A tail that reloads re-plans
/// the whole ledger and meets the same bad frames again; without this each reload wrote one line per
/// frame (1,104 per gardend in the W7 smoke).
#[derive(Debug, Default)]
struct QuarantineLimiter {
    last: Option<std::time::Instant>,
    pending: u64,
}

impl QuarantineLimiter {
    /// Count `n` newly quarantined frames. `Some(total)` when a line is due: the frames counted since
    /// the last line, this call's included.
    fn note(&mut self, now: std::time::Instant, n: u64, every: Duration) -> Option<u64> {
        self.pending += n;
        let due = self
            .last
            .map_or(true, |at| now.saturating_duration_since(at) >= every);
        if !due {
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.pending))
    }

    /// A line owed for notes held back by an earlier window: `Some(count)` once the window has
    /// passed and something is pending, with no new frames needed (W7b-2 d). The tail asks on
    /// every cycle, so the trailing count of a burst is never left unreported until the next
    /// quarantine.
    fn flush(&mut self, now: std::time::Instant, every: Duration) -> Option<u64> {
        if self.pending == 0 {
            return None;
        }
        self.note(now, 0, every)
    }
}

fn quarantine_limiters() -> &'static StdMutex<HashMap<(String, String, String), QuarantineLimiter>>
{
    static LIMITERS: OnceLock<StdMutex<HashMap<(String, String, String), QuarantineLimiter>>> =
        OnceLock::new();
    LIMITERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn join_error(e: tokio::task::JoinError) -> String {
    format!("blocking task failed: {e}")
}

fn node(iri: &str) -> Result<NamedNode, String> {
    NamedNode::new(iri).map_err(|e| format!("invalid IRI {iri:?}: {e}"))
}

/// The cell's background-work lease: idle drain and shutdown wait for a cycle in flight. Tests have
/// no process lifecycle.
fn background_lease(name: &'static str) -> Option<crate::cell_lifecycle::BackgroundLease> {
    #[cfg(test)]
    {
        let _ = name;
        None
    }
    #[cfg(not(test))]
    {
        crate::cell_lifecycle::process_lifecycle()
            .and_then(|lifecycle| lifecycle.begin_background(name).ok())
    }
}

// ---------------------------------------------------------------------------------------------
// Planning: steps -> units. Pure over [`FoldStep`]s.
// ---------------------------------------------------------------------------------------------

/// A transaction run as the status route shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenTxn {
    pub(crate) txn: String,
    pub(crate) origin: String,
    pub(crate) first_seq: u64,
    pub(crate) last_seq: u64,
    /// True once a later frame of the origin follows the run: it can no longer be committed.
    pub(crate) abandoned: bool,
}

/// An event the fold skipped because its frame did not decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quarantined {
    pub(crate) origin: String,
    pub(crate) seq: u64,
    pub(crate) error: String,
}

/// The smallest thing applied atomically: one plain frame, or a closed transaction run. Applying
/// it moves its origin's watermark to `last_seq`. A unit with no events only advances the watermark
/// (a frame with no records, a quarantined one, an abandoned run, a commit).
pub(crate) struct Unit {
    pub(crate) origin: OriginId,
    pub(crate) last_seq: u64,
    pub(crate) events: Vec<FoldEvent>,
    /// Subject IRIs the frames' headers name.
    pub(crate) subjects: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Plan {
    pub(crate) units: Vec<Unit>,
    /// Runs still being written (the last thing their origin wrote).
    pub(crate) open_tail: Vec<OpenTxn>,
    /// Runs followed by a later frame of their origin and never committed.
    pub(crate) abandoned: Vec<OpenTxn>,
    pub(crate) quarantined: Vec<Quarantined>,
}

struct RunBuf {
    txn: String,
    first_seq: u64,
    last_seq: u64,
    events: Vec<FoldEvent>,
    subjects: Vec<String>,
}

struct OriginPlan {
    origin: OriginId,
    run: Option<RunBuf>,
}

impl Plan {
    /// Push a unit; a unit with no events folds into a directly preceding event-less unit of the
    /// same origin (watermark steps only), which keeps a ledger of commits and redactions small.
    fn push_unit(&mut self, unit: Unit) {
        if unit.events.is_empty() {
            if let Some(last) = self.units.last_mut() {
                if last.events.is_empty() && last.origin == unit.origin {
                    last.last_seq = unit.last_seq;
                    return;
                }
            }
        }
        self.units.push(unit);
    }

    fn quarantine(&mut self, origin: &OriginId, seq: u64, error: String) {
        self.quarantined.push(Quarantined {
            origin: origin.0.clone(),
            seq,
            error,
        });
    }
}

impl OriginPlan {
    fn abandon(&mut self, plan: &mut Plan) {
        if let Some(run) = self.run.take() {
            plan.abandoned.push(OpenTxn {
                txn: run.txn,
                origin: self.origin.0.clone(),
                first_seq: run.first_seq,
                last_seq: run.last_seq,
                abandoned: true,
            });
            plan.push_unit(Unit {
                origin: self.origin.clone(),
                last_seq: run.last_seq,
                events: Vec::new(),
                subjects: Vec::new(),
            });
        }
    }

    fn finish(mut self, plan: &mut Plan) {
        if let Some(run) = self.run.take() {
            plan.open_tail.push(OpenTxn {
                txn: run.txn,
                origin: self.origin.0.clone(),
                first_seq: run.first_seq,
                last_seq: run.last_seq,
                abandoned: false,
            });
        }
    }

    fn push(&mut self, step: FoldStep, plan: &mut Plan) {
        let FoldStep {
            origin,
            seq,
            txn,
            mode,
            subjects,
            event,
        } = step;
        let Some(txn) = txn else {
            // A frame with no txn ends any run in progress: that run can no longer be committed.
            self.abandon(plan);
            let mut unit = Unit {
                origin: origin.clone(),
                last_seq: seq,
                events: Vec::new(),
                subjects: Vec::new(),
            };
            match event {
                Some(Ok(event)) => {
                    unit.events.push(event);
                    unit.subjects = subjects;
                }
                Some(Err(error)) => plan.quarantine(&origin, seq, error),
                None => {}
            }
            plan.push_unit(unit);
            return;
        };
        let continues = self.run.as_ref().is_some_and(|run| run.txn == txn);
        if !continues {
            self.abandon(plan);
        }
        if matches!(mode, Mode::Commit) {
            // The commit frame closes the run it belongs to; with no frames before it, it closes a
            // run of one and applies nothing.
            let (events, subjects) = match self.run.take() {
                Some(run) if run.txn == txn => (run.events, run.subjects),
                _ => (Vec::new(), Vec::new()),
            };
            plan.push_unit(Unit {
                origin,
                last_seq: seq,
                events,
                subjects,
            });
            return;
        }
        let run = self.run.get_or_insert_with(|| RunBuf {
            txn,
            first_seq: seq,
            last_seq: seq,
            events: Vec::new(),
            subjects: Vec::new(),
        });
        run.last_seq = seq;
        match event {
            Some(Ok(event)) => {
                run.events.push(event);
                run.subjects.extend(subjects);
            }
            Some(Err(error)) => plan.quarantine(&origin, seq, error),
            None => {}
        }
    }
}

/// Plan steps (origin-major, seq ascending, as [`LedgerRoom::events_since`] returns them) into
/// units, in the same order. Deterministic: the same steps always plan the same way, so a restart
/// that re-reads the whole ledger reaches the boundaries the watermark was written at.
pub(crate) fn plan_steps(steps: Vec<FoldStep>) -> Plan {
    let mut plan = Plan::default();
    let mut current: Option<OriginPlan> = None;
    for step in steps {
        if current.as_ref().is_some_and(|c| c.origin != step.origin) {
            if let Some(done) = current.take() {
                done.finish(&mut plan);
            }
        }
        let origin_plan = current.get_or_insert_with(|| OriginPlan {
            origin: step.origin.clone(),
            run: None,
        });
        origin_plan.push(step, &mut plan);
    }
    if let Some(done) = current {
        done.finish(&mut plan);
    }
    plan
}

/// Split units into transactions of about [`CHUNK_EVENTS`] events, never inside a unit.
fn chunk_units(units: Vec<Unit>) -> Vec<Vec<Unit>> {
    let mut chunks = Vec::new();
    let mut current: Vec<Unit> = Vec::new();
    let mut events = 0usize;
    for unit in units {
        events += unit.events.len();
        current.push(unit);
        if events >= CHUNK_EVENTS {
            chunks.push(std::mem::take(&mut current));
            events = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// ---------------------------------------------------------------------------------------------
// Sinks and configuration.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum SinkKind {
    /// `{graph_subject}:user:rdf`, shared with other writers: the fold owns only its subjects.
    UserRdf,
    /// A contract's `projection:*` write target. `base` is `{graph_subject}:{write_target}`; the
    /// live graph is the generation the pointer in the control graph names.
    Projection { base: String },
}

/// The policy and the sink kind of `(graph, vocab)`, from the resolved contract or, with none,
/// from the op kinds. A contract is "none" only when the vocab is unknown to the registry and to
/// the graph's chamber; a contract that fails to parse is an error, not a fall-through to
/// `user:rdf`.
fn policy_and_kind(
    store: &Store,
    graph_id: &str,
    vocab: &str,
) -> Result<(FoldPolicy, SinkKind), String> {
    match resolve_ingest_contract(store, graph_id, vocab) {
        Ok(contract) => {
            let target = contract.write_target.as_deref().unwrap_or("");
            if target == "projection:memory" {
                return Err(format!(
                    "vocab '{vocab}' folds into projection:memory, which the memory lane owns; \
                     the ledger fold does not write there"
                ));
            }
            let kind = if target.starts_with("projection:") {
                SinkKind::Projection {
                    base: format!("{}:{}", graph_subject(graph_id), target),
                }
            } else {
                SinkKind::UserRdf
            };
            Ok((FoldPolicy::from_contract(&contract), kind))
        }
        Err(message) if message.starts_with("unknown vocab") => {
            Ok((FoldPolicy::from_ops(), SinkKind::UserRdf))
        }
        Err(message) => Err(format!("resolve contract of '{vocab}': {message}")),
    }
}

/// The graph a reader should query for the fold of `(graph_id, vocab)`: `user:rdf`, or the
/// current generation of the contract's projection sink (the pointer a live rebuild moves).
pub(crate) fn resolve_sink_graph(
    store: &Store,
    graph_id: &str,
    vocab: &str,
) -> Result<String, String> {
    let (_, kind) = policy_and_kind(store, graph_id, vocab)?;
    match kind {
        SinkKind::UserRdf => Ok(user_rdf_graph_iri(graph_id)),
        SinkKind::Projection { base } => {
            let control = node(&watermark::control_graph_iri(graph_id))?;
            let generation = watermark::read_generation(store, &control, &base)?;
            Ok(watermark::generation_graph_iri(&base, generation))
        }
    }
}

struct Resolved {
    policy: FoldPolicy,
    kind: SinkKind,
    generation: u64,
    watermark: VersionVector,
}

/// Everything the tail reads from the store at (re)load: the policy, the sink, the stored
/// watermark; and the housekeeping a load owes (orphan generations from a crashed rebuild).
fn resolve_config(
    store: &Store,
    graph_id: &str,
    vocab: &str,
    control: &NamedNode,
) -> Result<Resolved, String> {
    let (policy, kind) = policy_and_kind(store, graph_id, vocab)?;
    let generation = match &kind {
        SinkKind::UserRdf => 0,
        SinkKind::Projection { base } => {
            let generation = watermark::read_generation(store, control, base)?;
            watermark::sweep_orphan_generations(store, base, generation)?;
            generation
        }
    };
    let stored = watermark::read_watermarks(store, control, vocab)?;
    Ok(Resolved {
        policy,
        kind,
        generation,
        watermark: stored,
    })
}

fn sink_node(graph_id: &str, kind: &SinkKind, generation: u64) -> Result<NamedNode, String> {
    match kind {
        SinkKind::UserRdf => node(&user_rdf_graph_iri(graph_id)),
        SinkKind::Projection { base } => node(&watermark::generation_graph_iri(base, generation)),
    }
}

/// What a tail needs to run. Tests hand `store` in; the registry hook leaves it `None` and the
/// tail opens the graph's own store.
pub(crate) struct FoldConfig {
    pub(crate) graph_id: String,
    pub(crate) vocab: String,
    pub(crate) graph_dir: PathBuf,
    pub(crate) store: Option<Arc<Store>>,
    /// For the hot-write lease. `None` (tests): no lease.
    pub(crate) app: Option<AppHandle>,
}

// ---------------------------------------------------------------------------------------------
// Triples and quads.
// ---------------------------------------------------------------------------------------------

fn ox_to_term(term: OxTerm) -> Term {
    match term {
        OxTerm::NamedNode(n) => Term::Uri(n),
        OxTerm::Literal(l) => Term::Lit(l),
        // A blank node (or a quoted triple) on a fold-owned subject is someone's hand edit. It
        // cannot be a fold head, so it is shown as an IRI that no head will ever equal.
        other => Term::Uri(NamedNode::new_unchecked(format!(
            "urn:x-foreign-term:{}",
            other.to_string().replace(char::is_whitespace, "_")
        ))),
    }
}

fn to_quad(triple: &Triple, graph: &NamedNode) -> Option<Quad> {
    super::apply::quad_of(triple, &GraphName::NamedNode(graph.clone()))
}

/// Every triple of `subjects` in `graph`.
fn live_triples<'a>(
    store: &Store,
    graph: &NamedNode,
    subjects: impl IntoIterator<Item = &'a String>,
) -> Result<Vec<Triple>, String> {
    let mut out = Vec::new();
    for subject in subjects {
        let Ok(subject_node) = NamedNode::new(subject.as_str()) else {
            continue;
        };
        for quad in store.quads_for_pattern(
            Some(subject_node.as_ref().into()),
            None,
            None,
            Some(GraphNameRef::NamedNode(graph.as_ref())),
        ) {
            let quad = quad.map_err(|e| format!("read {subject}: {e}"))?;
            out.push((
                subject.clone(),
                quad.predicate.as_str().to_string(),
                ox_to_term(quad.object),
            ));
        }
    }
    Ok(out)
}

/// Every triple of a graph whose subject is an IRI.
fn dump_graph(store: &Store, graph: &NamedNode) -> Result<Vec<Triple>, String> {
    let mut out = Vec::new();
    for quad in store.quads_for_pattern(
        None,
        None,
        None,
        Some(GraphNameRef::NamedNode(graph.as_ref())),
    ) {
        let quad = quad.map_err(|e| format!("dump {graph}: {e}"))?;
        if let NamedOrBlankNode::NamedNode(subject) = quad.subject {
            out.push((
                subject.into_string(),
                quad.predicate.as_str().to_string(),
                ox_to_term(quad.object),
            ));
        }
    }
    Ok(out)
}

fn sample(triples: &[Triple]) -> Vec<String> {
    triples
        .iter()
        .take(REPORT_SAMPLE)
        .map(|(s, p, o)| format!("<{s}> <{p}> {}", o.as_nt()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The oracle report and the pure comparison.
// ---------------------------------------------------------------------------------------------

/// The result of an oracle comparison or a live rebuild. `missing`: in the replayed set but not in
/// the sink; `extra`: in the sink (on a fold-owned subject) but not in the replayed set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OracleReport {
    pub(crate) events: u64,
    pub(crate) triples: u64,
    pub(crate) equal: bool,
    pub(crate) missing: u64,
    pub(crate) extra: u64,
    pub(crate) ms: u64,
    pub(crate) missing_sample: Vec<String>,
    pub(crate) extra_sample: Vec<String>,
}

/// False-green guard: events that fold to nothing are a failure, never an empty "equal".
fn refuse_empty_fold(events: usize, replayed: &[Triple]) -> Result<(), String> {
    if events > 0 && replayed.is_empty() {
        return Err(format!(
            "ledger fold oracle: {events} event(s) folded to zero triples; refusing a vacuous result"
        ));
    }
    Ok(())
}

/// Fold `events` from nothing (head table and triples), refusing an empty fold.
fn fold_from_scratch<'a>(
    policy: &FoldPolicy,
    units: impl Iterator<Item = &'a Unit> + Clone,
) -> Result<(HeadTable, Vec<Triple>, usize), String> {
    let events = units.clone().map(|u| u.events.len()).sum::<usize>();
    let (table, triples) = HeadTable::rebuild(policy, units.flat_map(|u| u.events.iter()));
    refuse_empty_fold(events, &triples)?;
    Ok((table, triples, events))
}

/// Compare the replayed triples with the sink restricted to the fold-owned subjects.
fn compare_with_sink(
    store: &Store,
    sink: &NamedNode,
    owned: &BTreeSet<String>,
    replayed: &[Triple],
    events: usize,
    started: Instant,
) -> Result<OracleReport, String> {
    let mut subjects: BTreeSet<&String> = owned.iter().collect();
    subjects.extend(replayed.iter().map(|t| &t.0));
    let live = live_triples(store, sink, subjects)?;
    let diff = diff_triples(&live, replayed);
    Ok(OracleReport {
        events: events as u64,
        triples: replayed.len() as u64,
        equal: diff.is_empty(),
        missing: diff.adds.len() as u64,
        extra: diff.removes.len() as u64,
        ms: started.elapsed().as_millis() as u64,
        missing_sample: sample(&diff.adds),
        extra_sample: sample(&diff.removes),
    })
}

/// Round-trip the replayed triples through a throwaway in-memory store, so the oracle exercises
/// the same `apply_delta` path the live fold writes with.
fn through_memory_store(sink: &NamedNode, triples: &[Triple]) -> Result<Vec<Triple>, String> {
    let memory = Store::new().map_err(|e| format!("open throwaway store: {e}"))?;
    let mut txn = memory
        .start_transaction()
        .map_err(|e| format!("start throwaway transaction: {e}"))?;
    let delta = Delta {
        insert: triples.to_vec(),
        remove: Vec::new(),
        touched: BTreeSet::new(),
        contested: Vec::new(),
    };
    apply_delta(&mut txn, sink, &delta);
    txn.commit()
        .map_err(|e| format!("commit throwaway store: {e}"))?;
    dump_graph(&memory, sink)
}

fn oracle_blocking(
    store: &Store,
    sink: &NamedNode,
    policy: &FoldPolicy,
    units: &[Unit],
    owned: &BTreeSet<String>,
) -> Result<OracleReport, String> {
    let started = Instant::now();
    let (_table, triples, events) = fold_from_scratch(policy, units.iter())?;
    let replayed = through_memory_store(sink, &triples)?;
    let mut all_owned = owned.clone();
    all_owned.extend(units.iter().flat_map(|u| u.subjects.iter().cloned()));
    compare_with_sink(store, sink, &all_owned, &replayed, events, started)
}

// ---------------------------------------------------------------------------------------------
// Applying a chunk, and rebuilding.
// ---------------------------------------------------------------------------------------------

struct ChunkCtx {
    store: Arc<Store>,
    sink: NamedNode,
    control: NamedNode,
    vocab: String,
    policy: Arc<FoldPolicy>,
    /// Tests set it to fail the next chunk after its delta is staged and before it commits.
    fault: Arc<AtomicBool>,
}

#[derive(Default)]
struct ChunkOut {
    advance: BTreeMap<OriginId, u64>,
    touched: BTreeSet<String>,
    applied: u64,
    contested: u64,
    delta_ms: f64,
    commit_ms: f64,
}

/// One store transaction: every unit's delta, then the watermarks. Nothing is visible until the
/// commit, and the commit carries both.
fn apply_chunk_blocking(
    ctx: &ChunkCtx,
    heads: &mut HeadTable,
    units: &[Unit],
) -> Result<ChunkOut, String> {
    let mut txn = ctx
        .store
        .start_transaction()
        .map_err(|e| format!("start fold transaction: {e}"))?;
    let mut out = ChunkOut::default();
    let staging = Instant::now();
    for unit in units {
        for event in &unit.events {
            let delta = heads.apply(&ctx.policy, event);
            apply_delta(&mut txn, &ctx.sink, &delta);
            out.contested += delta.contested.len() as u64;
            out.touched.extend(delta.touched);
            out.applied += 1;
        }
        let slot = out.advance.entry(unit.origin.clone()).or_insert(0);
        *slot = (*slot).max(unit.last_seq);
    }
    out.delta_ms = ms_since(staging);
    let committing = Instant::now();
    for (origin, seq) in &out.advance {
        watermark::stage_watermark(&mut txn, &ctx.control, &ctx.vocab, origin, *seq)?;
    }
    if ctx.fault.load(Ordering::SeqCst) {
        return Err("injected fault between the delta and the commit".to_string());
    }
    txn.commit()
        .map_err(|e| format!("commit fold transaction: {e}"))?;
    crate::cell_durability::mark_rdf_store_written(&ctx.store);
    out.commit_ms = ms_since(committing);
    Ok(out)
}

struct RebuildOut {
    heads: HeadTable,
    sink: NamedNode,
    generation: u64,
    retire: Option<NamedNode>,
    owned: BTreeSet<String>,
    report: OracleReport,
}

/// A live rebuild (decision 1.6). `user:rdf`: one transaction that removes the sink triples of the
/// owned subjects that are not heads and inserts the heads that are not there (foreign subjects
/// are never read, let alone removed). `projection:*`: bulk-load the heads into the next
/// generation graph, then swap the pointer and the watermarks in one transaction.
#[allow(clippy::too_many_arguments)]
fn rebuild_blocking(
    store: &Store,
    kind: &SinkKind,
    control: &NamedNode,
    vocab: &str,
    graph_id: &str,
    generation: u64,
    policy: &FoldPolicy,
    units: &[Unit],
    advance: &BTreeMap<OriginId, u64>,
    owned_before: &BTreeSet<String>,
) -> Result<RebuildOut, String> {
    let started = Instant::now();
    let (heads, triples, events) = fold_from_scratch(policy, units.iter())?;
    let mut owned = owned_before.clone();
    owned.extend(units.iter().flat_map(|u| u.subjects.iter().cloned()));
    owned.extend(triples.iter().map(|t| t.0.clone()));

    let (sink, generation, retire) = match kind {
        SinkKind::UserRdf => {
            let sink = sink_node(graph_id, kind, 0)?;
            let desired: HashSet<Quad> = triples.iter().filter_map(|t| to_quad(t, &sink)).collect();
            let mut txn = store
                .start_transaction()
                .map_err(|e| format!("start rebuild transaction: {e}"))?;
            let mut current: HashSet<Quad> = HashSet::new();
            for subject in &owned {
                let Ok(subject_node) = NamedNode::new(subject.as_str()) else {
                    continue;
                };
                for quad in txn.quads_for_pattern(
                    Some(subject_node.as_ref().into()),
                    None,
                    None,
                    Some(GraphNameRef::NamedNode(sink.as_ref())),
                ) {
                    current.insert(quad.map_err(|e| format!("read {subject}: {e}"))?);
                }
            }
            for quad in current.difference(&desired) {
                txn.remove(quad);
            }
            for quad in desired.difference(&current) {
                txn.insert(quad);
            }
            for (origin, seq) in advance {
                watermark::stage_watermark(&mut txn, control, vocab, origin, *seq)?;
            }
            txn.commit()
                .map_err(|e| format!("commit rebuild transaction: {e}"))?;
            crate::cell_durability::mark_rdf_store_written(store);
            (sink, 0, None)
        }
        SinkKind::Projection { base } => {
            let next = generation + 1;
            let fresh = node(&watermark::generation_graph_iri(base, next))?;
            // A leftover of a crashed rebuild: the load below needs the graph empty.
            if store
                .contains_named_graph(&fresh)
                .map_err(|e| format!("check {fresh}: {e}"))?
            {
                store
                    .remove_named_graph(&fresh)
                    .map_err(|e| format!("clear {fresh}: {e}"))?;
            }
            let mut loader = store
                .bulk_loader()
                .with_num_threads(2)
                .with_max_memory_size_in_megabytes(256);
            loader
                .load_quads(triples.iter().filter_map(|t| to_quad(t, &fresh)))
                .map_err(|e| format!("bulk-load {fresh}: {e}"))?;
            loader
                .commit()
                .map_err(|e| format!("commit bulk load of {fresh}: {e}"))?;
            let mut txn = store
                .start_transaction()
                .map_err(|e| format!("start swap transaction: {e}"))?;
            watermark::stage_generation(&mut txn, control, base, next)?;
            for (origin, seq) in advance {
                watermark::stage_watermark(&mut txn, control, vocab, origin, *seq)?;
            }
            txn.commit()
                .map_err(|e| format!("commit swap transaction: {e}"))?;
            crate::cell_durability::mark_rdf_store_written(store);
            let old = node(&watermark::generation_graph_iri(base, generation))?;
            (fresh, next, Some(old))
        }
    };
    let report = compare_with_sink(store, &sink, &owned, &triples, events, started)?;
    Ok(RebuildOut {
        heads,
        sink,
        generation,
        retire,
        owned,
        report,
    })
}

/// Drop a retired generation after [`RETIRE_GRACE`], off the write path. A crash before it runs
/// leaves an orphan the next load sweeps.
fn schedule_retire(store: Arc<Store>, old: NamedNode) {
    crate::app_runtime::async_runtime::spawn(
        async move {
            tokio::time::sleep(RETIRE_GRACE).await;
            let _ = tokio::task::spawn_blocking(move || match store.remove_named_graph(&old) {
                Ok(()) => crate::cell_durability::mark_rdf_store_written(&store),
                Err(error) => log::warn!("retire of sink generation {old} failed: {error}"),
            })
            .await;
        }
        .with_current_subscriber(),
    );
}

// ---------------------------------------------------------------------------------------------
// The tail's shared state, commands and handle.
// ---------------------------------------------------------------------------------------------

/// Where the last fold cycle spent its time (W7b item 9): the barrier, the walk past the watermark
/// (`events_since`), planning, and the store transaction split into staging the deltas and the
/// commit that carries the watermark. Milliseconds; shown by the status route as `last_cycle`.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct CycleStats {
    pub(crate) persist_ms: f64,
    pub(crate) events_since_ms: f64,
    pub(crate) plan_ms: f64,
    pub(crate) apply_ms: f64,
    /// Inside `apply_ms`: head-table merges and delta staging.
    pub(crate) delta_ms: f64,
    /// Inside `apply_ms`: staging the watermark and committing the store transaction.
    pub(crate) commit_ms: f64,
    pub(crate) steps: usize,
    pub(crate) events: usize,
}

fn ms_since(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1e6).round() / 1e3
}

#[derive(Default)]
struct Shared {
    last_cycle: Option<CycleStats>,
    running: bool,
    ready: bool,
    sink: String,
    error: Option<String>,
    open_txns: Vec<OpenTxn>,
    quarantined: Vec<Quarantined>,
    quarantined_total: u64,
    contested_raised: u64,
    applied_events: u64,
    owned_subjects: usize,
}

/// What a replay route asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayInto {
    /// Fold the whole ledger in memory and compare it with the sink.
    Oracle,
    /// Rebuild the sink from the ledger (decision 1.6).
    Live,
}

enum Command {
    Heads {
        subject: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Replay {
        into: ReplayInto,
        reply: oneshot::Sender<Result<OracleReport, String>>,
    },
}

/// A handle on a running fold tail.
pub(crate) struct FoldTail {
    commands: mpsc::Sender<Command>,
    shared: Arc<StdMutex<Shared>>,
    watermark: watch::Receiver<VersionVector>,
    version: watch::Receiver<VersionVector>,
    room_id: usize,
    /// Test hook: while set, every chunk fails after its delta is staged and before it commits.
    pub(crate) fault: Arc<AtomicBool>,
}

fn vv_object(vv: &VersionVector) -> serde_json::Map<String, Value> {
    vv.0.iter()
        .map(|(origin, seq)| (origin.0.clone(), Value::from(*seq)))
        .collect()
}

fn lag_frames(version: &VersionVector, watermark: &VersionVector) -> u64 {
    version
        .0
        .iter()
        .map(|(origin, seq)| seq.saturating_sub(watermark.get(origin)))
        .sum()
}

impl FoldTail {
    /// Start a tail for `room`. The task holds the room weakly and ends when the room is evicted
    /// or dropped. [`ensure_tail`] is the registry's way in; this is for tests and for callers that
    /// manage the handle themselves.
    pub(crate) fn spawn(config: FoldConfig, room: &Arc<LedgerRoom>) -> Arc<FoldTail> {
        Self::spawn_registered(config, room, None)
    }

    fn spawn_registered(
        config: FoldConfig,
        room: &Arc<LedgerRoom>,
        registry_key: Option<String>,
    ) -> Arc<FoldTail> {
        let room_id = Arc::as_ptr(room) as usize;
        let (commands, command_rx) = mpsc::channel(8);
        let (wm_tx, wm_rx) = watch::channel(VersionVector::default());
        let changes = room.changes();
        let shared = Arc::new(StdMutex::new(Shared {
            running: true,
            ..Shared::default()
        }));
        let fault = Arc::new(AtomicBool::new(false));
        let tail = Arc::new(FoldTail {
            commands,
            shared: shared.clone(),
            watermark: wm_rx,
            version: changes.clone(),
            room_id,
            fault: fault.clone(),
        });
        let control = node(&watermark::control_graph_iri(&config.graph_id))
            .expect("the control graph IRI is valid");
        let core = Core {
            graph_id: config.graph_id,
            vocab: config.vocab,
            graph_dir: config.graph_dir,
            app: config.app,
            store: config.store,
            control,
            policy: Arc::new(FoldPolicy::default()),
            kind: SinkKind::UserRdf,
            sink: node("urn:sophia:ledger#unresolved").expect("a valid placeholder IRI"),
            generation: 0,
            heads: None,
            wm: VersionVector::default(),
            owned: BTreeSet::new(),
            quarantined: BTreeMap::new(),
            quarantined_total: 0,
            abandoned: BTreeMap::new(),
            open_tail: Vec::new(),
            contested: 0,
            applied: 0,
            shared,
            wm_tx,
            fault,
            chunk_ms: (0.0, 0.0),
        };
        let weak = Arc::downgrade(room);
        crate::app_runtime::async_runtime::spawn(
            run_tail(core, weak, changes, command_rx, registry_key, room_id)
                .with_current_subscriber(),
        );
        tail
    }

    /// The fold's watermark as a watch channel: lane S awaits `wait_for(|w| w.dominates(&target))`
    /// for `?wait=folded`.
    pub(crate) fn watermark(&self) -> watch::Receiver<VersionVector> {
        self.watermark.clone()
    }

    /// False once the task has ended (the room was evicted or dropped).
    pub(crate) fn is_running(&self) -> bool {
        lock(&self.shared).running
    }

    /// The status route's body (see [`fold_status_json`]).
    pub(crate) fn status_json(&self) -> Value {
        let watermark = self.watermark.borrow().clone();
        let version = self.version.borrow().clone();
        let shared = lock(&self.shared);
        json!({
            "watermark": vv_object(&watermark),
            "version": vv_object(&version),
            "lag_frames": lag_frames(&version, &watermark),
            "open_txns": shared.open_txns.iter().map(|t| json!({
                "txn": t.txn,
                "origin": t.origin,
                "first_seq": t.first_seq,
                "last_seq": t.last_seq,
                "abandoned": t.abandoned,
            })).collect::<Vec<_>>(),
            "quarantined": shared.quarantined.iter().map(|q| json!({
                "origin": q.origin,
                "seq": q.seq,
                "error": q.error,
            })).collect::<Vec<_>>(),
            "quarantined_total": shared.quarantined_total,
            "contested": shared.contested_raised,
            "sink": shared.sink,
            "running": shared.running,
            "ready": shared.ready,
            "error": shared.error,
            "applied_events": shared.applied_events,
            "owned_subjects": shared.owned_subjects,
            "last_cycle": shared.last_cycle,
        })
    }

    async fn ask<T>(
        &self,
        command: Command,
        reply: oneshot::Receiver<Result<T, String>>,
    ) -> Result<T, String> {
        self.commands
            .send(command)
            .await
            .map_err(|_| "the fold tail has stopped".to_string())?;
        match tokio::time::timeout(COMMAND_TIMEOUT, reply).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the fold tail stopped before it answered".to_string()),
            Err(_) => Err(format!(
                "the fold tail did not answer within {COMMAND_TIMEOUT:?}"
            )),
        }
    }

    /// The head rows of one subject, as of the tail's last cycle.
    pub(crate) async fn heads(&self, subject: &str) -> Result<Value, String> {
        let (reply, rx) = oneshot::channel();
        self.ask(
            Command::Heads {
                subject: subject.to_string(),
                reply,
            },
            rx,
        )
        .await
    }

    /// Run the oracle or a live rebuild between two cycles.
    pub(crate) async fn replay(&self, into: ReplayInto) -> Result<OracleReport, String> {
        let (reply, rx) = oneshot::channel();
        self.ask(Command::Replay { into, reply }, rx).await
    }
}

// ---------------------------------------------------------------------------------------------
// The registry of tails.
// ---------------------------------------------------------------------------------------------

fn tails() -> &'static StdMutex<HashMap<String, Arc<FoldTail>>> {
    static TAILS: OnceLock<StdMutex<HashMap<String, Arc<FoldTail>>>> = OnceLock::new();
    TAILS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Make sure `room` has a running fold tail and return it. Called by
/// `RoomRegistry::open_ledger` after it hands out a room; a room that already has one keeps it,
/// and a room opened after an eviction gets a new one (the old tail has seen the flag and left).
pub(crate) fn ensure_tail(
    app: &AppHandle,
    graph_id: &str,
    vocab: &str,
    graph_dir: &Path,
    room: &Arc<LedgerRoom>,
) -> Arc<FoldTail> {
    let key = ledger_room_key(graph_id, vocab);
    let room_id = Arc::as_ptr(room) as usize;
    let mut map = lock(tails());
    if let Some(existing) = map.get(&key) {
        if existing.room_id == room_id && existing.is_running() {
            return existing.clone();
        }
    }
    let tail = FoldTail::spawn_registered(
        FoldConfig {
            graph_id: graph_id.to_string(),
            vocab: vocab.to_string(),
            graph_dir: graph_dir.to_path_buf(),
            store: None,
            app: Some(app.clone()),
        },
        room,
        Some(key.clone()),
    );
    map.insert(key, tail.clone());
    tail
}

/// The running tail of `(graph_id, vocab)`, if there is one.
pub(crate) fn tail_for(graph_id: &str, vocab: &str) -> Option<Arc<FoldTail>> {
    let key = ledger_room_key(graph_id, vocab);
    lock(tails())
        .get(&key)
        .filter(|tail| tail.is_running())
        .cloned()
}

/// The fold's watermark of `(graph_id, vocab)` as a watch channel, for `?wait=folded`: await
/// `rx.wait_for(|w| w.dominates(&target))`. `None` when no tail runs for it (the room is not
/// hosted, or the tail has stopped); a caller then cannot wait and should say so.
pub(crate) fn fold_watermark(
    _app: &AppHandle,
    graph_id: &str,
    vocab: &str,
) -> Option<watch::Receiver<VersionVector>> {
    tail_for(graph_id, vocab).map(|tail| tail.watermark())
}

/// The status route's body: `{watermark, version, lag_frames, open_txns, quarantined, contested,
/// sink, running, ready, error, ...}`. The first two fields are what the commit seam's watermark
/// route returns. With no running tail: empty maps and `running: false`.
pub(crate) fn fold_status_json(_app: &AppHandle, graph_id: &str, vocab: &str) -> Value {
    match tail_for(graph_id, vocab) {
        Some(tail) => tail.status_json(),
        None => json!({
            "watermark": {},
            "version": {},
            "lag_frames": 0,
            "open_txns": [],
            "quarantined": [],
            "quarantined_total": 0,
            "contested": 0,
            "sink": null,
            "running": false,
            "ready": false,
            "error": null,
            "applied_events": 0,
            "owned_subjects": 0,
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// The task.
// ---------------------------------------------------------------------------------------------

struct Core {
    graph_id: String,
    vocab: String,
    graph_dir: PathBuf,
    app: Option<AppHandle>,
    store: Option<Arc<Store>>,
    control: NamedNode,
    policy: Arc<FoldPolicy>,
    kind: SinkKind,
    sink: NamedNode,
    generation: u64,
    /// `None` until loaded, and again after a failed chunk (the table may be ahead of the sink).
    heads: Option<HeadTable>,
    wm: VersionVector,
    /// Subjects the fold has frames for (header subjects and every delta's touched set).
    owned: BTreeSet<String>,
    quarantined: BTreeMap<(String, u64), String>,
    quarantined_total: u64,
    abandoned: BTreeMap<(String, u64), OpenTxn>,
    open_tail: Vec<OpenTxn>,
    contested: u64,
    applied: u64,
    shared: Arc<StdMutex<Shared>>,
    wm_tx: watch::Sender<VersionVector>,
    fault: Arc<AtomicBool>,
    /// Staging and commit time of the chunks applied since the last cycle report.
    chunk_ms: (f64, f64),
}

impl Core {
    fn store(&self) -> Result<Arc<Store>, String> {
        self.store
            .clone()
            .ok_or_else(|| "the sink store is not open".to_string())
    }

    fn set_error(&self, error: Option<String>) {
        lock(&self.shared).error = error;
    }

    /// Copy the task's state into what the routes read.
    fn publish(&self) {
        {
            let mut shared = lock(&self.shared);
            shared.sink = self.sink.as_str().to_string();
            let mut runs: Vec<OpenTxn> = self.abandoned.values().cloned().collect();
            runs.extend(self.open_tail.iter().cloned());
            runs.sort_by(|a, b| (&a.origin, a.first_seq).cmp(&(&b.origin, b.first_seq)));
            shared.open_txns = runs;
            shared.quarantined = self
                .quarantined
                .iter()
                .map(|((origin, seq), error)| Quarantined {
                    origin: origin.clone(),
                    seq: *seq,
                    error: error.clone(),
                })
                .collect();
            shared.quarantined_total = self.quarantined_total;
            shared.contested_raised = self
                .heads
                .as_ref()
                .map_or(shared.contested_raised, |h| h.contested_count() as u64);
            shared.applied_events = self.applied;
            shared.owned_subjects = self.owned.len();
        }
        self.wm_tx.send_if_modified(|current| {
            if *current == self.wm {
                false
            } else {
                *current = self.wm.clone();
                true
            }
        });
    }

    fn absorb_meta(&mut self, plan: &mut Plan) {
        self.open_tail = std::mem::take(&mut plan.open_tail);
        for run in std::mem::take(&mut plan.abandoned) {
            self.abandoned
                .entry((run.origin.clone(), run.first_seq))
                .or_insert(run);
        }
        // New quarantines, grouped by origin: one rate-limited line per origin, not one per frame.
        let mut fresh: BTreeMap<String, (u64, u64, String)> = BTreeMap::new();
        for q in std::mem::take(&mut plan.quarantined) {
            let key = (q.origin.clone(), q.seq);
            if self.quarantined.insert(key, q.error.clone()).is_none() {
                self.quarantined_total += 1;
                fresh
                    .entry(q.origin)
                    .and_modify(|(count, _, _)| *count += 1)
                    .or_insert((1, q.seq, q.error));
            }
        }
        for (origin, (count, seq, error)) in fresh {
            let due = lock(quarantine_limiters())
                .entry((self.graph_id.clone(), self.vocab.clone(), origin.clone()))
                .or_default()
                .note(std::time::Instant::now(), count, QUARANTINE_LOG_EVERY);
            if let Some(total) = due {
                log::warn!(
                    "ledger fold {}/{}: quarantined {total} frame(s) of {origin} (the first of them {origin}#{seq}: {error}); \
                     the status route lists them, and this note repeats at most once a minute",
                    self.graph_id,
                    self.vocab,
                );
            }
        }
        // Notes an earlier window held back: report them on this cycle, not at the next quarantine.
        let owed: Vec<(String, u64)> = lock(quarantine_limiters())
            .iter_mut()
            .filter(|((graph, vocab, _), _)| *graph == self.graph_id && *vocab == self.vocab)
            .filter_map(|((_, _, origin), limiter)| {
                limiter
                    .flush(std::time::Instant::now(), QUARANTINE_LOG_EVERY)
                    .map(|total| (origin.clone(), total))
            })
            .collect();
        for (origin, total) in owed {
            log::warn!(
                "ledger fold {}/{}: {total} more frame(s) of {origin} quarantined since the last note; \
                 the status route lists them",
                self.graph_id,
                self.vocab,
            );
        }
        while self.quarantined.len() > QUARANTINE_CAP {
            if let Some(oldest) = self.quarantined.keys().next().cloned() {
                self.quarantined.remove(&oldest);
            }
        }
    }

    async fn hot_lease(&self) -> Result<Option<HotWriteLease>, String> {
        let Some(app) = &self.app else {
            return Ok(None);
        };
        let Some(coordinator) = app.try_state::<GraphPersistenceCoordinator>() else {
            return Ok(None);
        };
        coordinator
            .acquire_hot_write(&self.graph_id)
            .await
            .map(Some)
    }

    /// Read the stored watermark and the whole ledger, rebuild the head table from what the
    /// watermark covers, and fold the rest.
    async fn load(&mut self, room: &Arc<LedgerRoom>) -> Result<(), String> {
        let _background = background_lease("ledger-fold");
        lock(&self.shared).ready = false;
        self.heads = None;
        if self.store.is_none() {
            let dir = self.graph_dir.clone();
            let store = tokio::task::spawn_blocking(move || open_graph_store(&dir))
                .await
                .map_err(join_error)??;
            self.store = Some(store);
        }
        let store = self.store()?;
        let resolved = {
            let (store, graph_id, vocab, control) = (
                store.clone(),
                self.graph_id.clone(),
                self.vocab.clone(),
                self.control.clone(),
            );
            tokio::task::spawn_blocking(move || resolve_config(&store, &graph_id, &vocab, &control))
                .await
                .map_err(join_error)??
        };
        self.policy = Arc::new(resolved.policy);
        self.kind = resolved.kind;
        self.generation = resolved.generation;
        self.sink = sink_node(&self.graph_id, &self.kind, self.generation)?;
        self.abandoned.clear();
        self.quarantined.clear();
        self.quarantined_total = 0;

        // `events_since` reads only what a barrier has covered; cover what is there first, so a
        // frame appended before this task started is not left waiting for the next append.
        room.persist()
            .await
            .map_err(|e| format!("ledger barrier: {e}"))?;
        let steps = room
            .events_since(&VersionVector::default())
            .await
            .map_err(|e| e.to_string())?;
        let mut last_seen: BTreeMap<OriginId, u64> = BTreeMap::new();
        for step in &steps {
            let slot = last_seen.entry(step.origin.clone()).or_insert(0);
            *slot = (*slot).max(step.seq);
        }
        let mut plan = plan_steps(steps);
        self.absorb_meta(&mut plan);

        let stored = resolved.watermark;
        let ahead = stored
            .0
            .iter()
            .find(|(origin, seq)| **seq > last_seen.get(*origin).copied().unwrap_or(0));
        if let Some((origin, seq)) = ahead {
            // The sink was folded from frames this ledger no longer has (a crash inside the
            // durability window, or a ledger replaced). Only a rebuild makes the two agree.
            log::warn!(
                "ledger fold {}/{}: watermark {} at {seq} is ahead of the ledger; rebuilding the sink",
                self.graph_id,
                self.vocab,
                origin.0
            );
            drop(plan);
            self.rebuild_live(room).await?;
            lock(&self.shared).ready = true;
            return Ok(());
        }

        let (below, above): (Vec<Unit>, Vec<Unit>) = plan
            .units
            .into_iter()
            .partition(|u| u.last_seq <= stored.get(&u.origin));
        let policy = self.policy.clone();
        let (heads, owned) = tokio::task::spawn_blocking(move || {
            let (heads, _triples) =
                HeadTable::rebuild(&policy, below.iter().flat_map(|u| u.events.iter()));
            let owned: BTreeSet<String> = below
                .iter()
                .flat_map(|u| u.subjects.iter().cloned())
                .collect();
            (heads, owned)
        })
        .await
        .map_err(join_error)?;
        self.heads = Some(heads);
        self.owned = owned;
        self.wm = stored;
        self.publish();
        self.apply_units(above).await?;
        self.publish();
        lock(&self.shared).ready = true;
        Ok(())
    }

    /// One fold cycle: barrier, read past the watermark, plan, apply.
    async fn cycle(&mut self, room: &Arc<LedgerRoom>) -> Result<(), String> {
        if self.heads.is_none() {
            return Err("the fold state is not loaded".to_string());
        }
        let _background = background_lease("ledger-fold");
        // The fold applies only what a barrier has covered (`events_since` stops at the version the
        // last barrier recorded). The barrier orders the ledger's writes; it does not force them to
        // the platter, so a power cut inside the accepted 1 s window can still lose the newest
        // frames, and the sink can then be ahead of the recovered ledger by that window. What the
        // rule does buy: the fold never reads a frame no barrier has been issued for, and the
        // watermark commits only after the ledger's own writes were ordered ahead of it. This
        // barrier covers everything appended so far.
        let started = Instant::now();
        room.persist()
            .await
            .map_err(|e| format!("ledger barrier: {e}"))?;
        let persist_ms = ms_since(started);
        let walking = Instant::now();
        let steps = room
            .events_since(&self.wm)
            .await
            .map_err(|e| e.to_string())?;
        let events_since_ms = ms_since(walking);
        let planning = Instant::now();
        let step_count = steps.len();
        let mut plan = plan_steps(steps);
        let plan_ms = ms_since(planning);
        self.absorb_meta(&mut plan);
        let event_count: usize = plan.units.iter().map(|u| u.events.len()).sum();
        self.chunk_ms = (0.0, 0.0);
        let applying = Instant::now();
        self.apply_units(plan.units).await?;
        let apply_ms = ms_since(applying);
        if step_count > 0 {
            lock(&self.shared).last_cycle = Some(CycleStats {
                persist_ms,
                events_since_ms,
                plan_ms,
                apply_ms,
                delta_ms: (self.chunk_ms.0 * 1e3).round() / 1e3,
                commit_ms: (self.chunk_ms.1 * 1e3).round() / 1e3,
                steps: step_count,
                events: event_count,
            });
        }
        self.publish();
        Ok(())
    }

    async fn apply_units(&mut self, units: Vec<Unit>) -> Result<(), String> {
        if units.is_empty() {
            return Ok(());
        }
        let _lease = self.hot_lease().await?;
        for chunk in chunk_units(units) {
            self.apply_chunk(chunk).await?;
        }
        Ok(())
    }

    async fn apply_chunk(&mut self, chunk: Vec<Unit>) -> Result<(), String> {
        let heads = self
            .heads
            .take()
            .ok_or_else(|| "the fold state is not loaded".to_string())?;
        let ctx = ChunkCtx {
            store: self.store()?,
            sink: self.sink.clone(),
            control: self.control.clone(),
            vocab: self.vocab.clone(),
            policy: self.policy.clone(),
            fault: self.fault.clone(),
        };
        let header_subjects: Vec<String> = chunk
            .iter()
            .flat_map(|u| u.subjects.iter().cloned())
            .collect();
        let (heads, out) = tokio::task::spawn_blocking(move || {
            let mut heads = heads;
            let out = apply_chunk_blocking(&ctx, &mut heads, &chunk);
            (heads, out)
        })
        .await
        .map_err(join_error)?;
        // Whatever happened, the table is back; but after a failed chunk it may hold heads the
        // sink never got, so it is dropped and the next cycle reloads from the store.
        let out = out?;
        self.heads = Some(heads);
        for (origin, seq) in out.advance {
            let slot = self.wm.0.entry(origin).or_insert(0);
            *slot = (*slot).max(seq);
        }
        self.chunk_ms.0 += out.delta_ms;
        self.chunk_ms.1 += out.commit_ms;
        self.owned.extend(out.touched);
        self.owned.extend(header_subjects);
        self.applied += out.applied;
        self.contested += out.contested;
        self.publish();
        Ok(())
    }

    fn heads_json(&self, subject: &str) -> Result<Value, String> {
        let heads = self
            .heads
            .as_ref()
            .ok_or_else(|| "the fold state is not loaded".to_string())?;
        let rows: Vec<Value> = heads
            .rows_of(subject)
            .iter()
            .map(|row| {
                let value = match &row.value {
                    Term::Uri(n) => json!({"kind": "iri", "value": n.as_str()}),
                    Term::Lit(l) => json!({
                        "kind": "literal",
                        "value": l.value(),
                        "datatype": l.datatype().as_str(),
                        "language": l.language(),
                    }),
                    Term::Placeholder(name) => json!({"kind": "placeholder", "value": name}),
                };
                json!({
                    "predicate": row.predicate,
                    "value": value,
                    "hlc": {"ms": row.hlc.ms, "count": row.hlc.count},
                    "frame": row.frame,
                    "origin": row.origin,
                    "contested": row.contested,
                })
            })
            .collect();
        Ok(json!({
            "subject": subject,
            "owned": heads.owned_subjects().contains(subject) || self.owned.contains(subject),
            "rows": rows,
        }))
    }

    /// Catch up, then fold the whole ledger in memory and compare it with the sink.
    async fn oracle(&mut self, room: &Arc<LedgerRoom>) -> Result<OracleReport, String> {
        self.cycle(room).await?;
        let steps = room
            .events_since(&VersionVector::default())
            .await
            .map_err(|e| e.to_string())?;
        let plan = plan_steps(steps);
        let covered = self.wm.clone();
        let units: Vec<Unit> = plan
            .units
            .into_iter()
            .filter(|u| u.last_seq <= covered.get(&u.origin))
            .collect();
        let (store, sink, policy, owned) = (
            self.store()?,
            self.sink.clone(),
            self.policy.clone(),
            self.owned.clone(),
        );
        tokio::task::spawn_blocking(move || oracle_blocking(&store, &sink, &policy, &units, &owned))
            .await
            .map_err(join_error)?
    }

    /// Rebuild the sink from the whole ledger (decision 1.6) and set the watermark to what was
    /// folded.
    async fn rebuild_live(&mut self, room: &Arc<LedgerRoom>) -> Result<OracleReport, String> {
        let _background = background_lease("ledger-fold");
        room.persist()
            .await
            .map_err(|e| format!("ledger barrier: {e}"))?;
        let steps = room
            .events_since(&VersionVector::default())
            .await
            .map_err(|e| e.to_string())?;
        let mut plan = plan_steps(steps);
        self.absorb_meta(&mut plan);
        let mut advance: BTreeMap<OriginId, u64> = BTreeMap::new();
        for unit in &plan.units {
            let slot = advance.entry(unit.origin.clone()).or_insert(0);
            *slot = (*slot).max(unit.last_seq);
        }
        let units = plan.units;
        let _lease = self.hot_lease().await?;
        let store = self.store()?;
        let out = {
            let (store, kind, control, vocab, graph_id, policy, owned) = (
                store.clone(),
                self.kind.clone(),
                self.control.clone(),
                self.vocab.clone(),
                self.graph_id.clone(),
                self.policy.clone(),
                self.owned.clone(),
            );
            let generation = self.generation;
            let advance = advance.clone();
            tokio::task::spawn_blocking(move || {
                rebuild_blocking(
                    &store, &kind, &control, &vocab, &graph_id, generation, &policy, &units,
                    &advance, &owned,
                )
            })
            .await
            .map_err(join_error)??
        };
        self.heads = Some(out.heads);
        self.sink = out.sink;
        self.generation = out.generation;
        self.owned = out.owned;
        self.wm = VersionVector(advance);
        self.publish();
        if let Some(old) = out.retire {
            schedule_retire(store, old);
        }
        Ok(out.report)
    }

    async fn handle(&mut self, command: Command, room: &Weak<LedgerRoom>, loaded: bool) {
        match command {
            Command::Heads { subject, reply } => {
                let result = if loaded {
                    self.heads_json(&subject)
                } else {
                    Err(self.not_ready())
                };
                let _ = reply.send(result);
            }
            Command::Replay { into, reply } => {
                let result = match (loaded, room.upgrade()) {
                    (true, Some(room)) => match into {
                        ReplayInto::Oracle => self.oracle(&room).await,
                        ReplayInto::Live => self.rebuild_live(&room).await,
                    },
                    _ => Err(self.not_ready()),
                };
                let _ = reply.send(result);
            }
        }
    }

    fn not_ready(&self) -> String {
        match &lock(&self.shared).error {
            Some(error) => format!("the fold is not ready: {error}"),
            None => "the fold is not ready: it is still loading".to_string(),
        }
    }

    fn stopped(&self) {
        let mut shared = lock(&self.shared);
        shared.running = false;
        shared.ready = false;
    }
}

async fn run_tail(
    mut core: Core,
    room: Weak<LedgerRoom>,
    mut changes: watch::Receiver<VersionVector>,
    mut commands: mpsc::Receiver<Command>,
    registry_key: Option<String>,
    room_id: usize,
) {
    let mut loaded = false;
    let mut dirty = true;
    let mut retry_at: Option<tokio::time::Instant> = None;
    let mut failures = 0u32;
    loop {
        let Some(strong) = room.upgrade() else {
            break;
        };
        if strong.is_evicted() {
            break;
        }
        let due = retry_at.map_or(true, |at| tokio::time::Instant::now() >= at);
        if dirty && due {
            // Mark the current version seen before reading, so a change that lands during the
            // cycle wakes the next iteration.
            changes.borrow_and_update();
            let result = if loaded {
                core.cycle(&strong).await
            } else {
                core.load(&strong).await
            };
            match result {
                Ok(()) => {
                    loaded = true;
                    dirty = false;
                    retry_at = None;
                    failures = 0;
                    core.set_error(None);
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let backoff = BACKOFF_MIN
                        .saturating_mul(1u32 << failures.min(6).saturating_sub(1))
                        .min(BACKOFF_MAX);
                    if !strong.is_evicted() {
                        log::warn!(
                            "ledger fold {}/{} failed (retry in {backoff:?}): {error}",
                            core.graph_id,
                            core.vocab
                        );
                    }
                    core.set_error(Some(error));
                    loaded = false;
                    dirty = true;
                    retry_at = Some(tokio::time::Instant::now() + backoff);
                }
            }
        }
        drop(strong);
        let retry = async {
            match retry_at {
                Some(at) if dirty => tokio::time::sleep_until(at).await,
                _ => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    break;
                }
                changes.borrow_and_update();
                tokio::time::sleep(DEBOUNCE).await;
                dirty = true;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    break;
                };
                core.handle(command, &room, loaded).await;
                if core.heads.is_none() {
                    loaded = false;
                }
                dirty = true;
            }
            _ = retry => {}
        }
    }
    core.stopped();
    if let Some(key) = registry_key {
        let mut map = lock(tails());
        if map.get(&key).is_some_and(|tail| tail.room_id == room_id) {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::ledger_fold::Hlc;
    use garden_ledger::{Draft, Subject};
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A scratch directory removed on drop (no tempfile dependency in the cell crate).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!(
                "ledger-fold-host-{}-{nanos}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn o(s: &str) -> OriginId {
        OriginId(s.to_string())
    }

    // ---- pure planning -------------------------------------------------------------------

    fn step(origin: &str, seq: u64, txn: Option<&str>, mode: Mode, bad: Option<&str>) -> FoldStep {
        // The planner never looks inside an event, so a synthetic one with no ops stands in; a
        // commit frame carries none, and a frame that does not decode carries its error.
        let event = match bad {
            Some(error) => Some(Err(error.to_string())),
            None if matches!(mode, Mode::Commit) => None,
            None => Some(Ok(FoldEvent::synthetic(
                origin,
                seq,
                Hlc { ms: seq, count: 0 },
                &[],
                Vec::new(),
            ))),
        };
        FoldStep {
            origin: o(origin),
            seq,
            txn: txn.map(str::to_string),
            mode,
            subjects: vec![format!("urn:s:{origin}:{seq}")],
            event,
        }
    }

    fn bounds(plan: &Plan) -> Vec<(String, u64)> {
        plan.units
            .iter()
            .map(|u| (u.origin.0.clone(), u.last_seq))
            .collect()
    }

    /// W7b item 10. The first note goes out at once; later ones are counted and released as one line
    /// per window, with the count. Mutation: log every call (drop the window test) and the second
    /// assertion goes red; drop the `pending` carry and the released count is wrong.
    #[test]
    fn quarantine_notes_are_rate_limited_per_window_and_carry_their_count() {
        let t0 = std::time::Instant::now();
        let every = Duration::from_secs(60);
        let mut limiter = QuarantineLimiter::default();
        assert_eq!(
            limiter.note(t0, 3, every),
            Some(3),
            "the first note is immediate"
        );
        assert_eq!(limiter.note(t0 + Duration::from_secs(1), 5, every), None);
        assert_eq!(limiter.note(t0 + Duration::from_secs(59), 2, every), None);
        assert_eq!(
            limiter.note(t0 + Duration::from_secs(60), 1, every),
            Some(8),
            "one line per window, carrying what was held back"
        );
        assert_eq!(limiter.note(t0 + Duration::from_secs(61), 4, every), None);
        assert_eq!(
            limiter.note(t0 + Duration::from_secs(121), 0, every),
            Some(4)
        );

        // The trailing count of a burst is flushed by a later cycle, with no new quarantine.
        let mut burst = QuarantineLimiter::default();
        assert_eq!(burst.note(t0, 1, every), Some(1));
        assert_eq!(burst.note(t0 + Duration::from_secs(5), 7, every), None);
        assert_eq!(
            burst.flush(t0 + Duration::from_secs(30), every),
            None,
            "window not over"
        );
        assert_eq!(burst.flush(t0 + Duration::from_secs(61), every), Some(7));
        assert_eq!(
            burst.flush(t0 + Duration::from_secs(200), every),
            None,
            "nothing pending"
        );
    }

    #[test]
    fn host_plan_a_committed_run_is_one_unit_ending_at_its_commit() {
        let plan = plan_steps(vec![
            step("a", 1, Some("t"), Mode::Assert, None),
            step("a", 2, Some("t"), Mode::Assert, None),
            step("a", 3, Some("t"), Mode::Commit, None),
            step("a", 4, None, Mode::Assert, None),
        ]);
        // Frames 1-3 are one unit that ends at the commit; frame 4 is a unit of its own.
        assert_eq!(bounds(&plan), vec![("a".into(), 3), ("a".into(), 4)]);
        assert!(plan.open_tail.is_empty() && plan.abandoned.is_empty());
    }

    #[test]
    fn host_plan_a_run_with_no_commit_is_open_then_abandoned_by_a_later_frame() {
        let open = plan_steps(vec![
            step("a", 1, None, Mode::Assert, None),
            step("a", 2, Some("t"), Mode::Assert, None),
            step("a", 3, Some("t"), Mode::Assert, None),
        ]);
        // The watermark may reach frame 1 and no further: the run is still being written.
        assert_eq!(bounds(&open), vec![("a".into(), 1)]);
        assert_eq!(
            open.open_tail,
            vec![OpenTxn {
                txn: "t".into(),
                origin: "a".into(),
                first_seq: 2,
                last_seq: 3,
                abandoned: false
            }]
        );

        let abandoned = plan_steps(vec![
            step("a", 1, Some("t"), Mode::Assert, None),
            step("a", 2, Some("t"), Mode::Assert, None),
            step("a", 3, None, Mode::Assert, None),
        ]);
        // A later frame follows: the run can never commit, the watermark steps over it (an
        // event-less unit ending at frame 2), then frame 3 is a unit of its own.
        assert_eq!(bounds(&abandoned), vec![("a".into(), 2), ("a".into(), 3)]);
        assert!(abandoned.units[0].events.is_empty());
        assert!(abandoned.open_tail.is_empty());
        assert_eq!(abandoned.abandoned.len(), 1);
        assert_eq!(abandoned.abandoned[0].first_seq, 1);
        assert_eq!(abandoned.abandoned[0].last_seq, 2);
        assert!(abandoned.abandoned[0].abandoned);

        // A different txn id also ends the run.
        let other = plan_steps(vec![
            step("a", 1, Some("t"), Mode::Assert, None),
            step("a", 2, Some("u"), Mode::Assert, None),
            step("a", 3, Some("u"), Mode::Commit, None),
        ]);
        assert_eq!(other.abandoned.len(), 1);
        assert_eq!(other.abandoned[0].txn, "t");
        assert!(other.open_tail.is_empty());
        assert_eq!(bounds(&other), vec![("a".into(), 1), ("a".into(), 3)]);
    }

    #[test]
    fn host_plan_origins_are_independent_and_a_bad_frame_is_quarantined_not_fatal() {
        let plan = plan_steps(vec![
            step("a", 1, None, Mode::Assert, Some("bad payload")),
            step("a", 2, Some("t"), Mode::Assert, None),
            step("b", 1, Some("t"), Mode::Assert, None),
            step("b", 2, Some("t"), Mode::Commit, None),
        ]);
        assert_eq!(
            plan.quarantined,
            vec![Quarantined {
                origin: "a".into(),
                seq: 1,
                error: "bad payload".into()
            }]
        );
        // a's open run holds a at frame 1; b's committed run is a unit.
        assert_eq!(bounds(&plan), vec![("a".into(), 1), ("b".into(), 2)]);
        assert_eq!(plan.open_tail.len(), 1);
        assert_eq!(plan.open_tail[0].origin, "a");
    }

    #[test]
    fn host_plan_a_commit_with_no_frames_before_it_closes_a_run_of_one() {
        let plan = plan_steps(vec![
            step("a", 1, Some("t"), Mode::Commit, None),
            step("a", 2, Some("t"), Mode::Assert, None),
        ]);
        assert_eq!(bounds(&plan), vec![("a".into(), 1)]);
        assert_eq!(plan.open_tail.len(), 1);
        assert_eq!(plan.open_tail[0].first_seq, 2);
    }

    #[test]
    fn host_chunks_never_split_a_unit_and_keep_order() {
        let units: Vec<Unit> = (1..=5)
            .map(|seq| Unit {
                origin: o("a"),
                last_seq: seq,
                events: Vec::new(),
                subjects: Vec::new(),
            })
            .collect();
        let chunks = chunk_units(units);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].iter().map(|u| u.last_seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }

    // ---- a real room, a real store, a real tail -------------------------------------------

    const GRAPH_PREFIX: &str = "fold-host";

    struct Rig {
        _dir: Scratch,
        graph_id: String,
        store: Arc<Store>,
        room: Arc<LedgerRoom>,
        tail: Arc<FoldTail>,
        vocab: String,
    }

    impl Rig {
        async fn new(vocab: &str) -> Rig {
            let dir = Scratch::new();
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let graph_id = format!("{GRAPH_PREFIX}-{}-{n}", std::process::id());
            let store = Arc::new(Store::open(dir.0.join("store.oxigraph")).unwrap());
            let room = LedgerRoom::open(&dir.0, vocab, o("cell-test")).unwrap();
            let tail = FoldTail::spawn(
                FoldConfig {
                    graph_id: graph_id.clone(),
                    vocab: vocab.to_string(),
                    graph_dir: dir.0.clone(),
                    store: Some(store.clone()),
                    app: None,
                },
                &room,
            );
            Rig {
                _dir: dir,
                graph_id,
                store,
                room,
                tail,
                vocab: vocab.to_string(),
            }
        }

        fn sink(&self) -> NamedNode {
            NamedNode::new(user_rdf_graph_iri(&self.graph_id)).unwrap()
        }

        fn control(&self) -> NamedNode {
            NamedNode::new(watermark::control_graph_iri(&self.graph_id)).unwrap()
        }

        /// Append one frame and return its seq.
        async fn append(&self, draft: Draft) -> u64 {
            self.room.append(draft).await.unwrap().seq
        }

        /// Wait until the fold's watermark covers `seq` of the cell origin.
        async fn folded(&self, seq: u64) {
            let mut rx = self.tail.watermark();
            let want = o("cell-test");
            tokio::time::timeout(
                Duration::from_secs(10),
                rx.wait_for(|w| w.get(&want) >= seq),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the fold did not reach seq {seq}: {}",
                    self.tail.status_json()
                )
            })
            .unwrap();
        }

        fn quads_in_sink(&self) -> Vec<Quad> {
            self.store
                .quads_for_pattern(
                    None,
                    None,
                    None,
                    Some(GraphNameRef::NamedNode(self.sink().as_ref())),
                )
                .map(Result::unwrap)
                .collect()
        }

        fn watermark_in_store(&self) -> u64 {
            watermark::read_watermarks(&self.store, &self.control(), &self.vocab)
                .unwrap()
                .get(&o("cell-test"))
        }
    }

    fn subject_iri(n: u64) -> String {
        format!("urn:ao:term/id={n}")
    }

    fn header_subject(n: u64) -> Subject {
        Subject {
            subject: subject_iri(n),
            class: "urn:ao:term".into(),
            parents: vec![],
        }
    }

    fn assert_draft(vocab: &str, subjects: &[u64], ops: Vec<Value>, txn: Option<&str>) -> Draft {
        Draft {
            witness: "urn:witness:fold-host-test".into(),
            vocab: vocab.into(),
            subjects: subjects.iter().map(|n| header_subject(*n)).collect(),
            mode: Mode::Assert,
            payload: serde_json::to_vec(&json!({"ops": ops})).unwrap(),
            txn: txn.map(str::to_string),
            extra: Default::default(),
        }
    }

    fn set_op(n: u64, predicate: &str, value: &str) -> Value {
        json!({
            "op": "set", "subject": subject_iri(n), "class": "urn:ao:term",
            "predicate": format!("urn:ao:term#{predicate}"), "value": {"@value": value},
        })
    }

    fn add_op(n: u64, predicate: &str, values: &[String]) -> Value {
        json!({
            "op": "add", "subject": subject_iri(n), "class": "urn:ao:term",
            "predicate": format!("urn:ao:term#{predicate}"),
            "values": values.iter().map(|v| json!({"@value": v})).collect::<Vec<_>>(),
        })
    }

    fn remove_op(n: u64, predicate: &str, values: &[String]) -> Value {
        json!({
            "op": "remove", "subject": subject_iri(n), "class": "urn:ao:term",
            "predicate": format!("urn:ao:term#{predicate}"),
            "values": values.iter().map(|v| json!({"@value": v})).collect::<Vec<_>>(),
        })
    }

    fn retract_op(n: u64) -> Value {
        json!({"op": "retract", "subject": subject_iri(n), "class": "urn:ao:term"})
    }

    #[tokio::test]
    async fn host_watermark_moves_in_the_same_transaction_as_the_delta() {
        let rig = Rig::new("ao").await;
        let seq = rig
            .append(assert_draft(
                "ao",
                &[1],
                vec![set_op(1, "name", "Customer")],
                None,
            ))
            .await;
        rig.folded(seq).await;
        assert!(
            rig.quads_in_sink()
                .iter()
                .any(|q| q.subject.to_string().contains("id=1")),
            "the delta did not reach user:rdf"
        );
        assert_eq!(rig.watermark_in_store(), seq);

        // A fault between the staged delta and the commit: neither the triple nor the watermark.
        rig.tail.fault.store(true, Ordering::SeqCst);
        let before = rig.quads_in_sink().len();
        let second = rig
            .append(assert_draft(
                "ao",
                &[2],
                vec![set_op(2, "name", "Order")],
                None,
            ))
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            rig.quads_in_sink().len(),
            before,
            "a delta committed without its watermark"
        );
        assert_eq!(
            rig.watermark_in_store(),
            seq,
            "the watermark moved without its delta"
        );
        assert!(rig.tail.status_json()["error"].is_string());

        // The fault clears; the tail reloads, finds the frame above the stored watermark,
        // applies it, and the sink is what the oracle says it should be.
        rig.tail.fault.store(false, Ordering::SeqCst);
        rig.folded(second).await;
        assert_eq!(rig.watermark_in_store(), second);
        let report = rig.tail.replay(ReplayInto::Oracle).await.unwrap();
        assert!(report.equal, "{report:?}");
        assert_eq!(report.events, 2);
    }

    /// W7b F8: the sink is never ahead of the barrier. With the next barrier parked in its fsync
    /// nothing is covered, so the fold applies nothing and the stored watermark stays; once the
    /// fsync finishes the frame folds. Mutation: let `events_since` read past the recorded barrier
    /// version and a frame appended between the cycle's barrier and its read is folded early.
    #[tokio::test]
    async fn host_the_fold_applies_a_frame_only_after_a_barrier_covers_it() {
        let rig = Rig::new("ao").await;
        let hook = rig.room.hold_next_barrier();
        let seq = rig
            .append(assert_draft(
                "ao",
                &[1],
                vec![set_op(1, "name", "Customer")],
                None,
            ))
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !hook.entered() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(hook.entered(), "no barrier reached its fsync");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(rig.watermark_in_store(), 0, "folded ahead of the barrier");
        assert!(
            rig.quads_in_sink().is_empty(),
            "folded ahead of the barrier"
        );

        hook.release();
        rig.folded(seq).await;
        assert_eq!(rig.watermark_in_store(), seq);
        assert!(!rig.quads_in_sink().is_empty());
    }

    #[tokio::test]
    async fn host_a_second_tail_starts_from_the_stored_watermark_and_applies_nothing_twice() {
        let rig = Rig::new("ao").await;
        let mut last = 0;
        for n in 1..=3 {
            last = rig
                .append(assert_draft(
                    "ao",
                    &[n],
                    vec![set_op(n, "name", &format!("t{n}"))],
                    None,
                ))
                .await;
        }
        rig.folded(last).await;
        let sink_before = rig.quads_in_sink();
        assert_eq!(rig.tail.status_json()["applied_events"], 3);

        let second = FoldTail::spawn(
            FoldConfig {
                graph_id: rig.graph_id.clone(),
                vocab: rig.vocab.clone(),
                graph_dir: rig._dir.0.clone(),
                store: Some(rig.store.clone()),
                app: None,
            },
            &rig.room,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !second.status_json()["ready"].as_bool().unwrap_or(false) {
            assert!(
                Instant::now() < deadline,
                "the second tail never became ready"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let status = second.status_json();
        assert_eq!(status["applied_events"], 0, "{status}");
        assert_eq!(status["watermark"]["cell-test"], last);
        assert_eq!(rig.quads_in_sink().len(), sink_before.len());
    }

    #[tokio::test]
    async fn host_a_txn_run_applies_atomically_and_only_at_its_commit() {
        let rig = Rig::new("ao").await;
        let runs = 3u64;
        let stop = Arc::new(AtomicBool::new(false));
        let observed: Arc<StdMutex<BTreeSet<usize>>> = Arc::new(StdMutex::new(BTreeSet::new()));
        let poller = {
            let (store, sink, stop, observed) = (
                rig.store.clone(),
                rig.sink(),
                stop.clone(),
                observed.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let n = store
                        .quads_for_pattern(
                            None,
                            None,
                            None,
                            Some(GraphNameRef::NamedNode(sink.as_ref())),
                        )
                        .filter(|q| {
                            q.as_ref()
                                .is_ok_and(|q| q.subject.to_string().contains("urn:ao:term/id="))
                        })
                        .count();
                    lock(&observed).insert(n);
                }
            })
        };

        // Three frames of one run, no commit yet: nothing reaches the sink.
        for n in 1..=runs {
            rig.append(assert_draft(
                "ao",
                &[n],
                vec![set_op(n, "name", &format!("n{n}")), set_op(n, "kind", "x")],
                Some("import-1"),
            ))
            .await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            rig.quads_in_sink().is_empty(),
            "an open run reached the sink"
        );
        let status = rig.tail.status_json();
        assert_eq!(status["open_txns"][0]["txn"], "import-1");
        assert_eq!(status["open_txns"][0]["abandoned"], false);
        assert_eq!(status["watermark"]["cell-test"].as_u64().unwrap_or(0), 0);
        // The room agrees: one open run of frames 1..=3.
        let open = rig.room.txn_runs_open().await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!((open[0].1.first_seq, open[0].1.last_seq), (1, 3));

        // The commit frame: all nine triples (rdf:type plus two values per subject) at once.
        let commit = rig
            .append(
                Draft::commit("import-1")
                    .with_witness("urn:witness:fold-host-test")
                    .with_vocab("ao"),
            )
            .await;
        rig.folded(commit).await;
        stop.store(true, Ordering::SeqCst);
        poller.join().unwrap();
        let whole = (runs * 3) as usize;
        assert_eq!(rig.quads_in_sink().len(), whole);
        let seen = lock(&observed).clone();
        assert!(
            seen.iter().all(|n| *n == 0 || *n == whole),
            "a reader saw a partial run: {seen:?}"
        );
        assert!(rig.tail.status_json()["open_txns"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn host_an_abandoned_run_is_listed_and_never_applied() {
        let rig = Rig::new("ao").await;
        rig.append(assert_draft(
            "ao",
            &[10],
            vec![set_op(10, "name", "ghost")],
            Some("dead"),
        ))
        .await;
        rig.append(assert_draft(
            "ao",
            &[11],
            vec![set_op(11, "name", "ghost-too")],
            Some("dead"),
        ))
        .await;
        // The writer moved on without committing.
        let live = rig
            .append(assert_draft(
                "ao",
                &[12],
                vec![set_op(12, "name", "real")],
                None,
            ))
            .await;
        rig.folded(live).await;
        let status = rig.tail.status_json();
        let runs = status["open_txns"].as_array().unwrap();
        assert_eq!(runs.len(), 1, "{status}");
        assert_eq!(runs[0]["txn"], "dead");
        assert_eq!(runs[0]["abandoned"], true);
        assert_eq!(
            (runs[0]["first_seq"].as_u64(), runs[0]["last_seq"].as_u64()),
            (Some(1), Some(2))
        );
        let subjects: BTreeSet<String> = rig
            .quads_in_sink()
            .iter()
            .map(|q| q.subject.to_string())
            .collect();
        assert!(subjects.iter().any(|s| s.contains("id=12")));
        assert!(!subjects
            .iter()
            .any(|s| s.contains("id=10") || s.contains("id=11")));
        // A restart plans the same way: still listed, still not applied.
        let report = rig.tail.replay(ReplayInto::Oracle).await.unwrap();
        assert!(report.equal, "{report:?}");
        assert_eq!(report.events, 1);
    }

    /// A small deterministic generator (xorshift), so a failure reproduces.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[tokio::test]
    async fn host_oracle_is_equal_after_a_thousand_random_events_and_not_after_a_hand_removal() {
        let rig = Rig::new("ao").await;
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut last = 0;
        for i in 0..1_000u64 {
            let subject = 1 + rng.below(40);
            let op = match rng.below(10) {
                0..=4 => set_op(
                    subject,
                    ["name", "kind", "note"][rng.below(3) as usize],
                    &format!("v{}", rng.below(30)),
                ),
                5..=7 => add_op(subject, "tag", &[format!("t{}", rng.below(8))]),
                8 => remove_op(subject, "tag", &[format!("t{}", rng.below(8))]),
                _ => retract_op(subject),
            };
            let txn = (i % 7 == 0).then(|| format!("batch-{i}"));
            last = rig
                .append(assert_draft("ao", &[subject], vec![op], txn.as_deref()))
                .await;
            if let Some(txn) = txn {
                last = rig
                    .append(
                        Draft::commit(txn)
                            .with_witness("urn:witness:fold-host-test")
                            .with_vocab("ao"),
                    )
                    .await;
            }
        }
        rig.folded(last).await;
        assert!(rig.tail.status_json()["quarantined"]
            .as_array()
            .unwrap()
            .is_empty());
        let equal = rig.tail.replay(ReplayInto::Oracle).await.unwrap();
        assert!(equal.equal, "{equal:?}");
        assert_eq!(equal.events, 1_000);
        assert!(equal.triples > 0);

        // Remove one fold-owned triple by hand: the oracle sees it.
        let victim = rig
            .quads_in_sink()
            .into_iter()
            .next()
            .expect("the sink holds something");
        rig.store.remove(&victim).unwrap();
        let unequal = rig.tail.replay(ReplayInto::Oracle).await.unwrap();
        assert!(!unequal.equal);
        assert_eq!(unequal.missing, 1, "{unequal:?}");
        assert_eq!(unequal.extra, 0);
        assert_eq!(unequal.missing_sample.len(), 1);

        // And a triple added by hand to an owned subject is `extra`.
        rig.store.insert(&victim).unwrap();
        let stray = Quad::new(
            victim.subject.clone(),
            NamedNode::new("urn:ao:term#stray").unwrap(),
            OxTerm::NamedNode(NamedNode::new("urn:elsewhere").unwrap()),
            GraphName::NamedNode(rig.sink()),
        );
        rig.store.insert(&stray).unwrap();
        let extra = rig.tail.replay(ReplayInto::Oracle).await.unwrap();
        assert_eq!((extra.missing, extra.extra), (0, 1), "{extra:?}");

        // A live rebuild repairs it.
        let rebuilt = rig.tail.replay(ReplayInto::Live).await.unwrap();
        assert!(rebuilt.equal, "{rebuilt:?}");
        assert!(rig.tail.replay(ReplayInto::Oracle).await.unwrap().equal);
    }

    #[tokio::test]
    async fn host_a_live_rebuild_of_user_rdf_keeps_foreign_subjects() {
        let rig = Rig::new("ao").await;
        let foreign = Quad::new(
            NamedNode::new("urn:foreign:vector-row/1").unwrap(),
            NamedNode::new("urn:foreign:text").unwrap(),
            OxTerm::NamedNode(NamedNode::new("urn:foreign:object").unwrap()),
            GraphName::NamedNode(rig.sink()),
        );
        rig.store.insert(&foreign).unwrap();
        let seq = rig
            .append(assert_draft(
                "ao",
                &[1],
                vec![set_op(1, "name", "Customer")],
                None,
            ))
            .await;
        rig.folded(seq).await;
        assert!(
            rig.store.contains(&foreign).unwrap(),
            "the fold touched a foreign subject"
        );
        let rebuilt = rig.tail.replay(ReplayInto::Live).await.unwrap();
        assert!(rebuilt.equal, "{rebuilt:?}");
        assert!(
            rig.store.contains(&foreign).unwrap(),
            "a live replay removed a foreign subject"
        );
        assert_eq!(rig.watermark_in_store(), seq);
    }

    #[tokio::test]
    async fn host_a_contract_vocab_folds_into_a_generation_graph_after_a_live_rebuild() {
        // `flow` is an embedded contract with write_target projection:flow.
        let rig = Rig::new("flow").await;
        let base = format!("{}:projection:flow", graph_subject(&rig.graph_id));
        let status_sink = |rig: &Rig| rig.tail.status_json()["sink"].as_str().unwrap().to_string();
        let mut last = 0;
        for n in 1..=3u64 {
            last = rig
                .append(assert_draft(
                    "flow",
                    &[n],
                    vec![set_op(n, "title", &format!("b{n}"))],
                    None,
                ))
                .await;
        }
        rig.folded(last).await;
        assert_eq!(
            status_sink(&rig),
            base,
            "generation 0 is the base graph itself"
        );
        assert_eq!(
            resolve_sink_graph(&rig.store, &rig.graph_id, "flow").unwrap(),
            base
        );
        let count = |iri: &str| {
            rig.store
                .quads_for_pattern(
                    None,
                    None,
                    None,
                    Some(GraphNameRef::NamedNode(
                        NamedNode::new(iri).unwrap().as_ref(),
                    )),
                )
                .count()
        };
        assert!(count(&base) > 0);

        let report = rig.tail.replay(ReplayInto::Live).await.unwrap();
        assert!(report.equal, "{report:?}");
        let gen1 = format!("{base}:gen-1");
        assert_eq!(status_sink(&rig), gen1);
        assert_eq!(
            resolve_sink_graph(&rig.store, &rig.graph_id, "flow").unwrap(),
            gen1
        );
        assert!(count(&gen1) > 0);
        // New frames fold into the new generation, and the base is left to the retirement task.
        let more = rig
            .append(assert_draft(
                "flow",
                &[4],
                vec![set_op(4, "title", "b4")],
                None,
            ))
            .await;
        rig.folded(more).await;
        let before = count(&gen1);
        assert!(before > 0);
        assert!(rig.tail.replay(ReplayInto::Oracle).await.unwrap().equal);
    }

    #[tokio::test]
    async fn host_the_tail_stops_at_eviction_and_does_not_keep_the_room_alive() {
        let rig = Rig::new("ao").await;
        let seq = rig
            .append(assert_draft("ao", &[1], vec![set_op(1, "name", "x")], None))
            .await;
        rig.folded(seq).await;
        assert!(rig.tail.is_running());
        rig.room.evict();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rig.tail.is_running() {
            assert!(
                Instant::now() < deadline,
                "the tail outlived its room's eviction"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The tail's task let go of the room: only the rig (and the eviction flush, briefly) holds it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&rig.room) > 1 {
            assert!(
                Instant::now() < deadline,
                "something still holds the evicted room"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(rig.tail.replay(ReplayInto::Oracle).await.is_err());
    }

    #[tokio::test]
    async fn host_a_frame_that_does_not_decode_is_quarantined_and_the_tail_goes_on() {
        let rig = Rig::new("ao").await;
        let mut bad = assert_draft("ao", &[1], vec![], None);
        bad.payload = b"{not json".to_vec();
        let bad_seq = rig.append(bad).await;
        let good = rig
            .append(assert_draft(
                "ao",
                &[2],
                vec![set_op(2, "name", "ok")],
                None,
            ))
            .await;
        rig.folded(good).await;
        let status = rig.tail.status_json();
        let q = status["quarantined"].as_array().unwrap();
        assert_eq!(q.len(), 1, "{status}");
        assert_eq!(q[0]["seq"], bad_seq);
        assert!(rig
            .quads_in_sink()
            .iter()
            .any(|q| q.subject.to_string().contains("id=2")));
    }

    #[tokio::test]
    async fn host_status_without_a_tail_is_an_empty_running_false_body() {
        // No app is needed for the unregistered case: nothing is registered under this key.
        assert!(tail_for("no-such-graph", "no-such-vocab").is_none());
        let rig = Rig::new("ao").await;
        let status = rig.tail.status_json();
        for field in [
            "watermark",
            "version",
            "lag_frames",
            "open_txns",
            "quarantined",
            "contested",
            "sink",
        ] {
            assert!(
                status.get(field).is_some(),
                "status lacks {field}: {status}"
            );
        }
    }
}

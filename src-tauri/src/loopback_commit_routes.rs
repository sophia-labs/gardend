//! The commit seam (W7, lane S): the HTTP write Nidra's single writer calls.
//!
//! - `POST /emporium/commit/{graph_id}/{vocab}` appends the request's ops to the `(graph, vocab)`
//!   ledger as one frame (or a transaction run of frames) and acks on the barrier. Scope `rdf.update`.
//! - `GET /emporium/commit/{graph_id}/{vocab}/watermark` the fold's watermark beside the ledger's
//!   version, cheap. Scope `rdf.query`. A read: it never creates a ledger or starts a fold.
//!
//! The write is a frame, not a plan (W7.md decision 1.7): validate, append, barrier, ack. There is no
//! survey, no planner, no applied journal and no write gate. The request's `ops` ARE the frame payload
//! (`{"ops":[...]}`, RFC 8785 canonical JSON, each op normalised through lane F's `Op`), so the fold
//! decodes exactly what the caller sent. Validation before any append:
//!
//! 1. structural (every IRI absolute, one class per subject, term shapes, typed literals well formed);
//! 2. for a vocab that resolves a contract ([`resolve_ingest_contract`]), the stateless SHACL gate
//!    ([`validate_desired_structured`]) on the desired triples of every subject a `set`/`add` leaves
//!    behind; a halt is 422 with the violations and nothing is appended. A contractless vocab (Nidra's
//!    `ao` until its shape sheet exists) gets the structural check only.
//!
//! Shape: [`crate::loopback_ledger_routes`] (same scope helpers, same graph check, admission through
//! [`RoomRegistry::open_ledger`]). Origin is the cell (decision 1.8): the server appends only as its
//! own writer; the caller's `writer` is validated, echoed, and recorded in the header's
//! `extra.writer` of every frame the request appends (see [`build_drafts`]).
//!
//! Transactions. A transaction run is consecutive frames of this origin sharing a `txn`, closed by a
//! `commit` frame ([`garden_ledger::Ledger::txn_runs`]); any other frame of the origin in between ends
//! the run. So while a caller's `txn` is open on a ledger, a request of a different transaction (or
//! none) to the same ledger waits [`txn_wait`] and then is refused 409; an open transaction that sees
//! no request for [`TXN_IDLE`] no longer blocks anyone.
//!
//! **A run is only ever continued or committed while it is intact** (W7b F1). The gate remembers, per
//! open transaction, the seq of the last frame it appended for it. Under the gate's mutex and before
//! any frame of transaction T is appended (a continuation or a commit-only request), the gate
//! compares that seq with the room's newest local seq: any frame in between (a stranger that passed
//! the idle gate) means the run is broken, and the request is `409 txnBroken {txn, broken_by_seq}`
//! with nothing appended. A commit-only request for a transaction with no open run is refused the
//! same way: a commit is never appended for a run that is not there. The gate is in memory, so after
//! a restart it is cold; the first request to a ledger in this process therefore learns the runs
//! the ledger already holds ([`LedgerRoom::local_txn_runs`]) before it decides anything. A restart
//! forgets that a transaction was recently active (it blocks no one), never that it is broken.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use garden_ledger::frame::{to_hex, MAX_BODY};
use garden_ledger::{Appended, Draft, Mode, Subject, VersionVector};
use oxigraph::model::NamedNode;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use crate::app_runtime::Manager;
use crate::crdt_engine::convergent::{ConvergeError, Convergent};
use crate::crdt_engine::ledger_room::LedgerRoom;
use crate::crdt_engine::rooms::{LedgerAdmissionError, RoomRegistry};
use crate::emporium::chamber_ontology::resolve_ingest_contract;
use crate::emporium::ledger_fold::{
    check_term_lexical, op_from_json, op_to_json, ops_to_value, FoldError, Op,
};
use crate::emporium::shacl_validator::{validate_desired_structured, violations_to_halt_string};
use crate::emporium::terms::{Term, Triple};
use crate::graph_paths::existing_graph_dir;
use crate::ids::validate_local_id;
use crate::loopback_http::{loopback_error, require_loopback_scope};
use crate::loopback_ledger_routes::{ledger_dir_in, ledger_room_key};
use crate::loopback_state::LoopbackState;
use crate::rdf_store_service::open_graph_store;

/// Scope of the commit route.
const COMMIT_SCOPE: &str = "rdf.update";
/// Scope of the watermark route.
const WATERMARK_SCOPE: &str = "rdf.query";

/// A request body over this is 413 (W7.md section 5).
pub(crate) const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// Target size of one frame's payload; ops are split into frames at this size.
pub(crate) const FRAME_PAYLOAD_TARGET: usize = 8 * 1024 * 1024;
/// Budget for the `subjects` of one frame header. The header is `u16`-prefixed (65,535 bytes), so a
/// frame of many small ops is cut by its subjects before it is cut by its payload.
pub(crate) const HEADER_SUBJECT_BUDGET: usize = 40_000;
/// What one `subjects` entry costs beyond its two IRIs (keys, quotes, braces, `parents`).
const SUBJECT_ENTRY_OVERHEAD: usize = 48;
/// One op, once canonical, may not need more than a frame can hold.
const MAX_OP_BYTES: usize = MAX_BODY - 128 * 1024;
/// Longest IRI accepted anywhere in an op.
const MAX_IRI_BYTES: usize = 4096;
/// `?wait=folded` waits at most this long, then answers 202.
const FOLD_WAIT: Duration = Duration::from_secs(10);
/// A transaction that has seen no request for this long no longer blocks other writers.
const TXN_IDLE: Duration = Duration::from_secs(60);
pub(crate) fn txn_idle() -> Duration {
    if cfg!(test) {
        Duration::from_millis(1000)
    } else {
        TXN_IDLE
    }
}
/// How long a request waits behind another caller's open transaction before it is refused 409.
fn txn_wait() -> Duration {
    if cfg!(test) {
        Duration::from_millis(250)
    } else {
        Duration::from_secs(5)
    }
}

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The prefix of the transaction ids this route mints for a multi-frame request that named none.
/// Reserved: a caller may not name a transaction with it (400, W7d), so the gate's rule that a
/// minted id is never remembered closed can never forget a caller's committed transaction.
const AUTO_TXN_PREFIX: &str = "commit-";

/// The two commit routes, merged into the loopback router.
pub(crate) fn loopback_commit_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/emporium/commit/{graph_id}/{vocab}", post(loopback_commit))
        .route(
            "/emporium/commit/{graph_id}/{vocab}/watermark",
            get(loopback_commit_watermark),
        )
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// A refusal: a status, a message, and (for a SHACL halt) the structured violations.
#[derive(Debug)]
pub(crate) struct CommitError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
    pub(crate) violations: Option<Value>,
    pub(crate) open_txn: Option<String>,
    /// `Some((txn, broken_by_seq))` for a 409 `txnBroken` (W7b F1).
    pub(crate) broken_txn: Option<(String, Option<u64>)>,
    /// `Some(txn)` for a 409 `txnClosed`: the ledger already holds a commit frame for it (W7b-2 c).
    pub(crate) closed_txn: Option<String>,
}

impl CommitError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        CommitError {
            status,
            message: message.into(),
            violations: None,
            open_txn: None,
            broken_txn: None,
            closed_txn: None,
        }
    }

    fn txn_closed(txn: &str) -> Self {
        CommitError {
            closed_txn: Some(txn.to_string()),
            ..Self::new(
                StatusCode::CONFLICT,
                format!(
                    "transaction {txn:?} is already committed on this ledger; nothing was appended. \
                     A retry after a lost response must read the outcome (the fold status or the \
                     version), not send the write again"
                ),
            )
        }
    }

    fn txn_broken(txn: &str, broken_by_seq: Option<u64>, message: String) -> Self {
        CommitError {
            broken_txn: Some((txn.to_string(), broken_by_seq)),
            ..Self::new(StatusCode::CONFLICT, message)
        }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for CommitError {
    fn into_response(self) -> Response {
        if self.violations.is_none()
            && self.open_txn.is_none()
            && self.broken_txn.is_none()
            && self.closed_txn.is_none()
        {
            return loopback_error(self.status, &self.message);
        }
        let mut body = json!({ "ok": false, "error": self.message });
        if let Some(violations) = self.violations {
            body["violations"] = violations;
        }
        if let Some(txn) = self.open_txn {
            body["openTxn"] = Value::String(txn);
        }
        if let Some(txn) = self.closed_txn {
            body["code"] = Value::String("txnClosed".to_string());
            body["txn"] = Value::String(txn);
        }
        if let Some((txn, broken_by_seq)) = self.broken_txn {
            body["code"] = Value::String("txnBroken".to_string());
            body["txn"] = Value::String(txn);
            body["broken_by_seq"] = broken_by_seq.map_or(Value::Null, |seq| json!(seq));
        }
        (self.status, Json(body)).into_response()
    }
}

fn from_converge(error: ConvergeError) -> CommitError {
    match error {
        ConvergeError::Locked(path) => CommitError::new(
            StatusCode::LOCKED,
            format!("the ledger is locked by another process ({path})"),
        ),
        ConvergeError::Refused(message) if message.contains("exceeds a frame limit") => {
            CommitError::new(StatusCode::PAYLOAD_TOO_LARGE, message)
        }
        ConvergeError::Refused(message) if message.contains("evicted") => {
            CommitError::new(StatusCode::SERVICE_UNAVAILABLE, message)
        }
        ConvergeError::Refused(message) | ConvergeError::Decode(message) => {
            CommitError::bad(message)
        }
        other => CommitError::internal(other.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Request body and plan (pure)
// ---------------------------------------------------------------------------------------------

/// The request body. Unknown keys are refused, so a typo fails loudly instead of being ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitBody {
    witness: Option<String>,
    writer: Option<String>,
    txn: Option<String>,
    commit: Option<bool>,
    #[serde(default)]
    ops: Vec<Value>,
}

/// Where the planner cuts frames. Production uses [`Limits::default`]; tests use small ones.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) frame_payload: usize,
    pub(crate) header_subjects: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            frame_payload: FRAME_PAYLOAD_TARGET,
            header_subjects: HEADER_SUBJECT_BUDGET,
        }
    }
}

/// One frame to append.
#[derive(Debug)]
pub(crate) struct PlannedFrame {
    pub(crate) mode: Mode,
    pub(crate) subjects: Vec<Subject>,
    pub(crate) payload: Vec<u8>,
}

/// A validated request, ready to append.
#[derive(Debug)]
pub(crate) struct CommitPlan {
    pub(crate) witness: String,
    /// The caller's `writer`, validated (see [`build_drafts`] for why it is not in the frame yet).
    pub(crate) writer: Option<String>,
    pub(crate) frames: Vec<PlannedFrame>,
    /// The transaction every frame carries (the caller's, or one minted for a request that needs
    /// several frames); `None` for a single frame that is its own atomic unit.
    pub(crate) txn: Option<String>,
    /// Append a `commit` frame for `txn` after the ops frames.
    pub(crate) close: bool,
    /// Whether the request leaves no transaction open.
    pub(crate) committed: bool,
    /// The triples every `set`/`add` subject is left with, for the SHACL gate.
    pub(crate) desired: Vec<Triple>,
}

fn check_token(what: &str, value: &str, max: usize) -> Result<(), CommitError> {
    if value.is_empty() || value.len() > max {
        return Err(CommitError::bad(format!(
            "{what} must be 1..={max} bytes, got {}",
            value.len()
        )));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(CommitError::bad(format!("{what} has a control character")));
    }
    Ok(())
}

fn check_txn(value: &str) -> Result<(), CommitError> {
    check_token("txn", value, 128)?;
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '=' | '@' | '+'))
    {
        return Err(CommitError::bad(
            "txn may contain only letters, digits and - _ . : = @ +",
        ));
    }
    if value.starts_with(AUTO_TXN_PREFIX) {
        return Err(CommitError::bad(format!(
            "txn ids starting with {AUTO_TXN_PREFIX:?} are reserved for the ids this route mints; \
             name the transaction otherwise"
        )));
    }
    Ok(())
}

/// What lane F's reader does not check: IRI length, empty `values`, and the lexical form of typed
/// literals ([`check_term_lexical`]).
fn check_op(op: &Op) -> Result<(), String> {
    for (what, iri) in [
        ("subject", Some(op.subject())),
        ("class", Some(op.class())),
        ("predicate", op.predicate()),
    ] {
        if iri.is_some_and(|iri| iri.len() > MAX_IRI_BYTES) {
            return Err(format!("{what} is over {MAX_IRI_BYTES} bytes"));
        }
    }
    let terms: Vec<&Term> = match op {
        // A set may carry any number of values, none clears; an add or remove needs at least one.
        Op::Set { values, .. } => values.iter().collect(),
        Op::Add { values, .. } | Op::Remove { values, .. } => {
            if values.is_empty() {
                return Err("values is empty".to_string());
            }
            values.iter().collect()
        }
        Op::Retract { .. } => Vec::new(),
    };
    for term in terms {
        check_term_lexical(term)?;
    }
    Ok(())
}

/// What the request leaves each subject with, applying ops in order (the last `set` wins, `remove`
/// takes a value out, `retract` drops the subject). Only subjects a `set`/`add` leaves values on are
/// validated.
#[derive(Default)]
struct Desired {
    subjects: BTreeMap<String, DesiredSubject>,
}

struct DesiredSubject {
    class: String,
    props: Vec<(String, Term)>,
    asserted: bool,
}

impl Desired {
    fn apply(&mut self, op: &Op) {
        let entry = match op {
            Op::Retract { subject, .. } => {
                self.subjects.remove(subject);
                return;
            }
            Op::Set { subject, class, .. }
            | Op::Add { subject, class, .. }
            | Op::Remove { subject, class, .. } => self
                .subjects
                .entry(subject.clone())
                .or_insert_with(|| DesiredSubject {
                    class: class.clone(),
                    props: Vec::new(),
                    asserted: false,
                }),
        };
        match op {
            Op::Set {
                predicate, values, ..
            } => {
                entry.props.retain(|(p, _)| p != predicate);
                for value in values {
                    let duplicate = entry
                        .props
                        .iter()
                        .any(|(p, v)| p == predicate && v.as_nt() == value.as_nt());
                    if !duplicate {
                        entry.props.push((predicate.clone(), value.clone()));
                    }
                    entry.asserted = true;
                }
            }
            Op::Add {
                predicate, values, ..
            } => {
                for value in values {
                    let duplicate = entry
                        .props
                        .iter()
                        .any(|(p, v)| p == predicate && v.as_nt() == value.as_nt());
                    if !duplicate {
                        entry.props.push((predicate.clone(), value.clone()));
                    }
                }
                entry.asserted = true;
            }
            Op::Remove {
                predicate, values, ..
            } => {
                for value in values {
                    entry
                        .props
                        .retain(|(p, v)| !(p == predicate && v.as_nt() == value.as_nt()));
                }
            }
            Op::Retract { .. } => {}
        }
    }

    fn triples(self) -> Vec<Triple> {
        let mut out = Vec::new();
        for (subject, desired) in self.subjects {
            if !desired.asserted || desired.props.is_empty() {
                continue;
            }
            if let Ok(class) = NamedNode::new(&desired.class) {
                out.push((subject.clone(), RDF_TYPE.to_string(), Term::Uri(class)));
            }
            for (predicate, value) in desired.props {
                out.push((subject.clone(), predicate, value));
            }
        }
        out
    }
}

/// `{"ops":[...]}` in the fold's stored form, canonicalized. Lane F's reader refuses any other
/// payload key, so nothing else may ride here.
fn assemble_payload(ops: &[Op]) -> Result<Vec<u8>, CommitError> {
    // W7b item 9: consecutive ops on one (subject, class) are stored once under a subject group
    // (`ops_to_value`), which is what takes the per-op subject and class out of the payload.
    serde_json_canonicalizer::to_vec(&ops_to_value(ops))
        .map_err(|e| CommitError::internal(format!("payload canonical form: {e}")))
}

/// Validate a request and cut it into frames. Pure: no I/O, so it is the unit under test.
pub(crate) fn plan_commit(body: CommitBody, limits: &Limits) -> Result<CommitPlan, CommitError> {
    let witness = body
        .witness
        .ok_or_else(|| CommitError::bad("witness is required"))?;
    check_token("witness", &witness, 1024)?;
    if let Some(writer) = &body.writer {
        check_token("writer", writer, 256)?;
    }
    if let Some(txn) = &body.txn {
        check_txn(txn)?;
    }

    // A commit-only request closes a run opened by earlier requests.
    if body.ops.is_empty() {
        return match (&body.txn, body.commit) {
            (Some(txn), Some(true)) => Ok(CommitPlan {
                witness,
                writer: body.writer.clone(),
                frames: Vec::new(),
                txn: Some(txn.clone()),
                close: true,
                committed: true,
                desired: Vec::new(),
            }),
            _ => Err(CommitError::bad(
                "ops is empty: send ops, or `txn` with `commit: true` to close an open transaction",
            )),
        };
    }
    if body.txn.is_none() && body.commit == Some(false) {
        return Err(CommitError::bad(
            "`commit: false` needs a `txn` to leave open; a request without one is atomic",
        ));
    }

    let mut classes: BTreeMap<String, String> = BTreeMap::new();
    let mut desired = Desired::default();
    let mut encoded: Vec<(Op, usize, Subject, bool)> = Vec::with_capacity(body.ops.len());
    for (index, raw) in body.ops.iter().enumerate() {
        let at = |message: String| CommitError::bad(format!("ops[{index}]: {message}"));
        // Lane F's reader is the arbiter of what the fold can decode: a request that passes here is
        // a payload the fold reads. Its normal form (`op_to_json`) is what goes in the frame.
        let op = op_from_json(index, raw).map_err(|e| match e {
            FoldError::Op { message, .. } => at(message),
            other => at(other.to_string()),
        })?;
        check_op(&op).map_err(at)?;
        let (subject, class) = (op.subject().to_string(), op.class().to_string());
        match classes.get(&subject) {
            Some(held) if *held != class => {
                return Err(CommitError::bad(format!(
                    "ops[{index}]: subject {subject} is given class {held} and {class}; a subject has one class"
                )))
            }
            Some(_) => {}
            None => {
                classes.insert(subject.clone(), class.clone());
            }
        }
        desired.apply(&op);
        let bytes = serde_json_canonicalizer::to_vec(&op_to_json(&op))
            .map_err(|e| CommitError::internal(format!("ops[{index}]: canonical form: {e}")))?;
        if bytes.len() > MAX_OP_BYTES {
            return Err(CommitError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "ops[{index}] is {} bytes; a frame holds at most {MAX_OP_BYTES}",
                    bytes.len()
                ),
            ));
        }
        let is_retract = matches!(op, Op::Retract { .. });
        encoded.push((
            op,
            bytes.len(),
            Subject {
                subject,
                class,
                parents: Vec::new(),
            },
            is_retract,
        ));
    }

    // Cut into frames by payload size and by header subjects.
    let mut frames: Vec<PlannedFrame> = Vec::new();
    let mut current: Vec<Op> = Vec::new();
    let mut subjects: Vec<Subject> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut payload_len = 0usize;
    let mut header_len = 0usize;
    let mut all_retract = true;
    let flush = |current: &mut Vec<Op>,
                 subjects: &mut Vec<Subject>,
                 seen: &mut BTreeSet<(String, String)>,
                 all_retract: &mut bool,
                 frames: &mut Vec<PlannedFrame>|
     -> Result<(), CommitError> {
        if current.is_empty() {
            return Ok(());
        }
        frames.push(PlannedFrame {
            mode: if *all_retract {
                Mode::Retract
            } else {
                Mode::Assert
            },
            subjects: std::mem::take(subjects),
            payload: assemble_payload(current)?,
        });
        current.clear();
        seen.clear();
        *all_retract = true;
        Ok(())
    };
    for (op, op_len, subject, is_retract) in encoded {
        let key = (subject.subject.clone(), subject.class.clone());
        let full_cost = subject.subject.len() + subject.class.len() + SUBJECT_ENTRY_OVERHEAD;
        let cost_now = if seen.contains(&key) { 0 } else { full_cost };
        if !current.is_empty()
            && (payload_len + op_len + 1 > limits.frame_payload
                || header_len + cost_now > limits.header_subjects)
        {
            flush(
                &mut current,
                &mut subjects,
                &mut seen,
                &mut all_retract,
                &mut frames,
            )?;
            payload_len = 0;
            header_len = 0;
        }
        if seen.insert(key) {
            subjects.push(subject);
            header_len += full_cost;
        }
        payload_len += op_len + 1;
        all_retract &= is_retract;
        current.push(op);
    }
    flush(
        &mut current,
        &mut subjects,
        &mut seen,
        &mut all_retract,
        &mut frames,
    )?;

    // One frame is its own atomic unit; several need a run, so the request stays all-or-nothing.
    let (txn, close, committed) = match body.txn {
        Some(txn) => {
            let close = body.commit.unwrap_or(false);
            (Some(txn), close, close)
        }
        None if frames.len() > 1 => (
            Some(format!(
                "{AUTO_TXN_PREFIX}{}",
                uuid::Uuid::new_v4().simple()
            )),
            true,
            true,
        ),
        None => (None, false, true),
    };
    Ok(CommitPlan {
        witness,
        writer: body.writer.clone(),
        frames,
        txn,
        close,
        committed,
        desired: desired.triples(),
    })
}

/// The drafts to append, in order.
///
/// The caller's `writer` (when it sent one) is recorded in the header's `extra.writer` of every frame
/// the request appends, the `commit` frame included (W7.md decision 1.8; W7b F2). It is not the
/// ledger origin, which is always the cell. The fold's payload reader is untouched: the writer rides
/// the header, never the payload.
pub(crate) fn build_drafts(plan: &CommitPlan, vocab: &str) -> Vec<Draft> {
    let with_writer = |draft: Draft| match &plan.writer {
        Some(writer) => draft.with_extra("writer", Value::String(writer.clone())),
        None => draft,
    };
    let mut drafts: Vec<Draft> = plan
        .frames
        .iter()
        .map(|frame| Draft {
            witness: plan.witness.clone(),
            vocab: vocab.to_string(),
            subjects: frame.subjects.clone(),
            mode: frame.mode.clone(),
            payload: frame.payload.clone(),
            txn: plan.txn.clone(),
            extra: Default::default(),
        })
        .map(with_writer)
        .collect();
    if plan.close {
        if let Some(txn) = &plan.txn {
            drafts.push(with_writer(
                Draft::commit(txn.clone())
                    .with_witness(plan.witness.clone())
                    .with_vocab(vocab),
            ));
        }
    }
    drafts
}

// ---------------------------------------------------------------------------------------------
// Transaction gate
// ---------------------------------------------------------------------------------------------

/// What the gate knows of one transaction run that has no commit frame yet.
#[derive(Debug, Clone)]
struct RunRec {
    /// Seq of the last frame of this cell's stream that belongs to the run.
    last_seq: u64,
    /// When a request of the transaction last passed the gate; `None` for a run learned from the
    /// ledger (a restart forgot it was active), which blocks nobody.
    since: Option<Instant>,
}

impl RunRec {
    /// True while the transaction's caller is presumed to be still writing it.
    fn blocks(&self) -> bool {
        self.since.is_some_and(|since| since.elapsed() < txn_idle())
    }
}

/// Per ledger: the open runs this cell's stream holds, as far as the gate has seen them.
#[derive(Debug, Default)]
struct GateState {
    /// The local origin the records were made under. Another origin (a ledger created anew under the
    /// same key) drops them and scans the new ledger.
    origin: Option<garden_ledger::OriginId>,
    runs: BTreeMap<String, RunRec>,
    /// Transactions the ledger holds a commit frame for: learned from the first scan, and added when
    /// this process commits one. A request naming one is refused `txnClosed` (W7b-2 c).
    closed: BTreeSet<String>,
}

/// Per ledger. The async mutex is held across a request's appends, so two requests' frames never
/// interleave, and the intact-run check below is made against a tip nobody else can move.
struct CommitGate {
    open: AsyncMutex<GateState>,
}

static GATES: OnceLock<StdMutex<BTreeMap<String, Arc<CommitGate>>>> = OnceLock::new();

fn gate_for(key: &str) -> Arc<CommitGate> {
    let gates = GATES.get_or_init(|| StdMutex::new(BTreeMap::new()));
    let mut gates = gates.lock().unwrap_or_else(|p| p.into_inner());
    gates
        .entry(key.to_string())
        .or_insert_with(|| {
            Arc::new(CommitGate {
                open: AsyncMutex::new(GateState::default()),
            })
        })
        .clone()
}

/// Forget every gate, as a process restart does.
#[cfg(test)]
pub(crate) fn forget_gates_for_test() {
    if let Some(gates) = GATES.get() {
        gates.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

/// Take the gate for a request of transaction `txn` (or none). Another caller's open transaction
/// blocks it for [`txn_wait`]; the error carries that transaction's id.
async fn enter_gate<'a>(
    gate: &'a CommitGate,
    txn: Option<&str>,
) -> Result<tokio::sync::MutexGuard<'a, GateState>, String> {
    let deadline = Instant::now() + txn_wait();
    loop {
        let guard = gate.open.lock().await;
        let blocker = guard
            .runs
            .iter()
            .find(|(open, rec)| txn != Some(open.as_str()) && rec.blocks())
            .map(|(open, _)| open.clone());
        match blocker {
            None => return Ok(guard),
            Some(other) => {
                drop(guard);
                if Instant::now() >= deadline {
                    return Err(other);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

/// A transaction id this route minted itself for a multi-frame request that named none (see
/// [`plan_commit`]). Nobody can retry one, so the gate does not remember it as closed. A caller
/// cannot name one: [`check_txn`] refuses the prefix (W7d).
fn is_auto_txn(txn: &str) -> bool {
    txn.strip_prefix(AUTO_TXN_PREFIX)
        .is_some_and(|rest| rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// With the gate held, bring its records in line with the ledger and return the cell's newest seq.
/// The first request to a ledger in this process (or to a ledger recreated under a new origin)
/// reads the runs the ledger already holds, so that a restart never makes a broken run look fresh.
async fn sync_gate(state: &mut GateState, room: &LedgerRoom) -> Result<u64, CommitError> {
    let (local, tip) = room.local_tip().await;
    if state.origin.as_ref() != Some(&local) {
        let (origin, runs) = room.local_txn_runs().await.map_err(from_converge)?;
        state.runs.clear();
        state.closed.clear();
        for run in runs {
            if run.closed {
                state.runs.remove(&run.txn);
                if !is_auto_txn(&run.txn) {
                    state.closed.insert(run.txn);
                }
            } else {
                state.runs.insert(
                    run.txn,
                    RunRec {
                        last_seq: run.last_seq,
                        since: None,
                    },
                );
            }
        }
        state.origin = Some(origin);
    }
    Ok(tip)
}

/// May a request of transaction `txn` append? Only into an intact run: the newest frame of this
/// cell's stream must be the last frame the run has (`tip`). A transaction the gate has no run for
/// is fresh, except for a commit-only request, which has nothing to commit.
fn check_run_intact(
    state: &GateState,
    txn: &str,
    tip: u64,
    commit_only: bool,
) -> Result<(), CommitError> {
    if state.closed.contains(txn) {
        return Err(CommitError::txn_closed(txn));
    }
    match state.runs.get(txn) {
        Some(run) if run.last_seq == tip => Ok(()),
        Some(run) => Err(CommitError::txn_broken(
            txn,
            (tip > run.last_seq).then_some(run.last_seq + 1),
            format!(
                "transaction {txn:?} is broken: its last frame is seq {} and the ledger is at seq {tip}; \
                 nothing was appended. Re-send the whole logical write under a new txn",
                run.last_seq
            ),
        )),
        None if commit_only => Err(CommitError::txn_broken(
            txn,
            None,
            format!(
                "transaction {txn:?} has no open run on this ledger (already committed, or never \
                 opened); no commit was appended"
            ),
        )),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// Validate both ids (400), then require the graph to exist (404), healing a first access the way
/// the ledger routes do. Returns the graph directory.
fn checked_graph(state: &LoopbackState, graph_id: &str, vocab: &str) -> Result<PathBuf, Response> {
    if let Err(message) = validate_local_id(graph_id, "graph_id") {
        return Err(loopback_error(StatusCode::BAD_REQUEST, &message));
    }
    if let Err(message) = validate_local_id(vocab, "vocab") {
        return Err(loopback_error(StatusCode::BAD_REQUEST, &message));
    }
    existing_graph_dir(&state.app, graph_id)
        .map_err(|message| loopback_error(StatusCode::NOT_FOUND, &message))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CommitQuery {
    wait: Option<String>,
}

async fn open_room(
    state: &LoopbackState,
    graph_id: &str,
    vocab: &str,
) -> Result<Arc<LedgerRoom>, CommitError> {
    let registry = state.app.state::<RoomRegistry>();
    registry
        .open_ledger(&state.app, graph_id, vocab)
        .await
        .map_err(|error| match error {
            LedgerAdmissionError::NotFound(message) => {
                CommitError::new(StatusCode::NOT_FOUND, message)
            }
            LedgerAdmissionError::Other(message)
                if message.contains("locked by another handle") =>
            {
                CommitError::new(StatusCode::LOCKED, message)
            }
            LedgerAdmissionError::Other(message) => CommitError::internal(message),
        })
}

/// The SHACL gate: resolve the vocab's contract and validate the desired triples. A vocab with no
/// contract passes (structural validation already ran); a halt is 422 and nothing is appended.
async fn shacl_gate(
    graph_dir: PathBuf,
    graph_id: String,
    vocab: String,
    desired: Vec<Triple>,
) -> Result<(), CommitError> {
    if desired.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        let store = open_graph_store(&graph_dir)
            .map_err(|e| CommitError::internal(format!("open the graph store: {e}")))?;
        let contract = match resolve_ingest_contract(&store, &graph_id, &vocab) {
            Ok(contract) => contract,
            Err(message) if message.starts_with("unknown vocab") => return Ok(()),
            Err(message) => return Err(CommitError::internal(message)),
        };
        match validate_desired_structured(&desired, &contract) {
            Ok(()) => Ok(()),
            Err(violations) => Err(CommitError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                message: violations_to_halt_string(&contract, &violations),
                violations: serde_json::to_value(&violations).ok(),
                open_txn: None,
                broken_txn: None,
                closed_txn: None,
            }),
        }
    })
    .await
    .map_err(|e| CommitError::internal(format!("SHACL gate task: {e}")))?
}

/// Append `drafts` in order. A fenced (evicted) room is reopened through the registry once per
/// frame. Returns the room that took the last frame and what was appended.
async fn append_all(
    state: &LoopbackState,
    graph_id: &str,
    vocab: &str,
    mut room: Arc<LedgerRoom>,
    drafts: Vec<Draft>,
    appended: &mut Vec<Appended>,
) -> Result<Arc<LedgerRoom>, CommitError> {
    for draft in drafts {
        let mut reopened = false;
        loop {
            match room.append(draft.clone()).await {
                Ok(done) => {
                    appended.push(done);
                    break;
                }
                Err(ConvergeError::Refused(message))
                    if !reopened && message.contains("evicted") =>
                {
                    reopened = true;
                    room = open_room(state, graph_id, vocab).await?;
                }
                Err(error) => return Err(from_converge(error)),
            }
        }
    }
    Ok(room)
}

fn vv_json(vv: &VersionVector) -> Value {
    let mut map = serde_json::Map::new();
    for (origin, seq) in &vv.0 {
        map.insert(origin.0.clone(), json!(seq));
    }
    Value::Object(map)
}

fn vv_from_json(value: &Value) -> VersionVector {
    let mut vv = VersionVector::default();
    if let Some(map) = value.as_object() {
        for (origin, seq) in map {
            if let Some(seq) = seq.as_u64() {
                vv.0.insert(garden_ledger::OriginId(origin.clone()), seq);
            }
        }
    }
    vv
}

/// Wait until the fold's watermark covers `target`, at most [`FOLD_WAIT`].
async fn wait_folded(
    state: &LoopbackState,
    graph_id: &str,
    vocab: &str,
    target: &VersionVector,
) -> bool {
    let deadline = Instant::now() + FOLD_WAIT;
    let mut watermark = loop {
        if let Some(receiver) =
            crate::emporium::ledger_fold::tail::fold_watermark(&state.app, graph_id, vocab)
        {
            break receiver;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    loop {
        let covered = watermark.borrow().dominates(target);
        if covered {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, watermark.changed()).await {
            Ok(Ok(())) => {}
            _ => return false,
        }
    }
}

/// `POST /emporium/commit/{graph_id}/{vocab}`: see the module docs and `COMMIT-API.md`.
pub(crate) async fn loopback_commit(
    Path((graph_id, vocab)): Path<(String, String)>,
    Query(query): Query<CommitQuery>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
    body: Body,
) -> Response {
    // The scope check stays in the handler, where the parity inventory looks for it.
    if let Err(response) = require_loopback_scope(&headers, &state, COMMIT_SCOPE) {
        return response;
    }
    match commit_inner(&state, &graph_id, &vocab, &query, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn commit_inner(
    state: &Arc<LoopbackState>,
    graph_id: &str,
    vocab: &str,
    query: &CommitQuery,
    body: Body,
) -> Result<Response, CommitError> {
    if state.lifecycle.is_draining() {
        return Err(CommitError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "graph cell is draining; retry through the gateway",
        ));
    }
    let wait_for_fold = match query.wait.as_deref() {
        None => false,
        Some("folded") => true,
        Some(other) => {
            return Err(CommitError::bad(format!(
                "wait={other:?}: the only supported value is `folded`"
            )))
        }
    };
    let graph_dir = match checked_graph(state, graph_id, vocab) {
        Ok(dir) => dir,
        Err(response) => return Ok(response),
    };

    let bytes = axum::body::to_bytes(body, MAX_REQUEST_BYTES + 1)
        .await
        .map_err(|_| {
            CommitError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the request body is over {MAX_REQUEST_BYTES} bytes"),
            )
        })?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(CommitError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("the request body is over {MAX_REQUEST_BYTES} bytes"),
        ));
    }
    let parsed: CommitBody = serde_json::from_slice(&bytes)
        .map_err(|e| CommitError::bad(format!("invalid commit body: {e}")))?;
    drop(bytes);

    let started = Instant::now();
    let plan = plan_commit(parsed, &Limits::default())?;
    shacl_gate(
        graph_dir,
        graph_id.to_string(),
        vocab.to_string(),
        plan.desired.clone(),
    )
    .await?;

    let validated = Instant::now();
    let drafts = build_drafts(&plan, vocab);
    let key = ledger_room_key(graph_id, vocab);
    let gate = gate_for(&key);
    let room = open_room(state, graph_id, vocab).await?;
    let room_opened = Instant::now();
    let mut gate_passed = room_opened;
    let mut appended: Vec<Appended> = Vec::new();
    let room = {
        let mut state_guard = enter_gate(&gate, plan.txn.as_deref())
            .await
            .map_err(|other| CommitError {
                status: StatusCode::CONFLICT,
                message: format!(
                    "transaction {other:?} is open on this ledger; send its commit, or retry once it closes"
                ),
                violations: None,
                open_txn: Some(other),
                broken_txn: None,
                closed_txn: None,
            })?;
        // A room evicted between the open and here (the registry's idle sweep) is reopened once,
        // as `append_all` does for an append.
        let mut room = room;
        let tip = match sync_gate(&mut state_guard, &room).await {
            Err(error) if error.status == StatusCode::SERVICE_UNAVAILABLE && room.is_evicted() => {
                room = open_room(state, graph_id, vocab).await?;
                sync_gate(&mut state_guard, &room).await?
            }
            other => other?,
        };
        if let Some(txn) = &plan.txn {
            let commit_only = plan.frames.is_empty() && plan.close;
            check_run_intact(&state_guard, txn, tip, commit_only)?;
        }
        gate_passed = Instant::now();
        let outcome = append_all(state, graph_id, vocab, room, drafts, &mut appended).await;
        // Whatever happened, the gate reflects what is now in the ledger: a transaction whose
        // newest frame is not its commit is an open run, and `last_seq` is where it ends.
        if let (Some(txn), Some(last)) = (&plan.txn, appended.last()) {
            if plan.committed && outcome.is_ok() {
                state_guard.runs.remove(txn);
                if !is_auto_txn(txn) {
                    state_guard.closed.insert(txn.clone());
                }
            } else {
                state_guard.runs.insert(
                    txn.clone(),
                    RunRec {
                        last_seq: last.seq,
                        // Only a caller who left the transaction open is presumed still writing.
                        since: (!plan.committed).then(Instant::now),
                    },
                );
            }
        }
        outcome?
    };

    let appended_at = Instant::now();
    // The ack rides the barrier, never before it.
    room.persist()
        .await
        .map_err(|e| CommitError::internal(format!("ledger barrier: {e}")))?;
    let barriered = Instant::now();

    let local = room.local_tip().await.0 .0;
    let frames: Vec<Value> = appended
        .iter()
        .map(|a| json!({ "origin": local, "seq": a.seq, "id": to_hex(&a.id), "hlc": a.hlc }))
        .collect();

    let acked = Instant::now();
    let mut folded = false;
    if wait_for_fold && plan.committed {
        if let Some(last) = appended.last() {
            let mut target = VersionVector::default();
            target
                .0
                .insert(garden_ledger::OriginId(local.clone()), last.seq);
            folded = wait_folded(state, graph_id, vocab, &target).await;
        }
    }
    let status = if wait_for_fold && plan.committed && !folded {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    let mut response = json!({
        "ok": true,
        "frames": frames,
        "committed": plan.committed,
        "folded": folded,
    });
    // Where the request spent its time, in milliseconds: validation (parse, plan, SHACL gate),
    // the gate and the appends, the barrier, and (with `?wait=folded`) the wait for the fold.
    let ms = |from: Instant, to: Instant| {
        (to.saturating_duration_since(from).as_secs_f64() * 1e5).round() / 1e2
    };
    response["timings"] = json!({
        "validateMs": ms(started, validated),
        "roomMs": ms(validated, room_opened),
        "gateMs": ms(room_opened, gate_passed),
        "appendMs": ms(gate_passed, appended_at),
        "barrierMs": ms(appended_at, barriered),
        "ackMs": ms(started, acked),
        "foldWaitMs": ms(acked, Instant::now()),
    });
    if let Some(txn) = &plan.txn {
        response["txn"] = Value::String(txn.clone());
    }
    if let Some(writer) = &plan.writer {
        response["writer"] = Value::String(writer.clone());
    }
    Ok((status, Json(response)).into_response())
}

/// `GET /emporium/commit/{graph_id}/{vocab}/watermark`: `{version, watermark, folded, lagFrames}`.
/// `watermark` is `null` while no fold is running for the ledger (nothing has opened it yet).
pub(crate) async fn loopback_commit_watermark(
    Path((graph_id, vocab)): Path<(String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<LoopbackState>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, WATERMARK_SCOPE) {
        return response;
    }
    let graph_dir = match checked_graph(&state, &graph_id, &vocab) {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let registry = state.app.state::<RoomRegistry>();
    let version = match registry
        .ledger_version(
            &ledger_room_key(&graph_id, &vocab),
            &ledger_dir_in(&graph_dir, &vocab),
        )
        .await
    {
        Ok(body) => body,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let origins = version.get("origins").cloned().unwrap_or_else(|| json!({}));
    let have = vv_from_json(&origins);
    let watermark = crate::emporium::ledger_fold::tail::fold_watermark(
        &state.app, &graph_id, &vocab,
    )
    .map(|receiver| {
        // Bind first: a temporary `Ref` in the tail would outlive `receiver`.
        let vv = receiver.borrow().clone();
        vv
    });
    let (folded, lag, watermark_json) = match &watermark {
        Some(vv) => {
            let lag: u64 = have
                .0
                .iter()
                .map(|(origin, seq)| seq.saturating_sub(vv.get(origin)))
                .sum();
            (vv.dominates(&have), Some(lag), vv_json(vv))
        }
        None => (false, None, Value::Null),
    };
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "graph": graph_id,
            "vocab": vocab,
            "local": version.get("local").cloned().unwrap_or(Value::Null),
            "version": origins,
            "durable": version.get("durable").cloned().unwrap_or_else(|| json!({})),
            "watermark": watermark_json,
            "folded": folded,
            "lagFrames": lag,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: &str = "urn:ao:term/id=1";
    const S2: &str = "urn:ao:term/id=2";
    const CLASS: &str = "urn:ao:term";
    const NAME: &str = "urn:ao:term#name";

    fn body(value: Value) -> CommitBody {
        serde_json::from_value(value).expect("a commit body")
    }

    fn set(subject: &str, value: Value) -> Value {
        json!({"op": "set", "subject": subject, "class": CLASS, "predicate": NAME, "value": value})
    }

    fn refusal(value: Value) -> CommitError {
        plan_commit(body(value), &Limits::default()).expect_err("a refusal")
    }

    #[test]
    fn a_witness_is_required() {
        let error = refusal(json!({"ops": [set(S1, json!("a"))]}));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("witness"));
        let error = refusal(json!({"witness": "", "ops": [set(S1, json!("a"))]}));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn unknown_body_keys_are_refused() {
        assert!(serde_json::from_value::<CommitBody>(json!({"witness": "w", "opz": []})).is_err());
    }

    #[test]
    fn one_frame_is_its_own_unit_with_no_txn_and_no_commit_frame() {
        let plan = plan_commit(
            body(json!({"witness": "w", "ops": [set(S1, json!("a")), set(S2, json!("b"))]})),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(plan.frames.len(), 1);
        assert_eq!(plan.txn, None);
        assert!(plan.committed && !plan.close);
        assert_eq!(plan.frames[0].subjects.len(), 2);
        let drafts = build_drafts(&plan, "ao");
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].mode, Mode::Assert);
        assert_eq!(drafts[0].vocab, "ao");
        let payload: Value = serde_json::from_slice(&drafts[0].payload).unwrap();
        assert_eq!(payload["ops"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_txn_stays_open_until_commit_and_a_commit_only_request_closes_it() {
        let open = plan_commit(
            body(json!({"witness": "w", "txn": "t1", "ops": [set(S1, json!("a"))]})),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(open.txn.as_deref(), Some("t1"));
        assert!(!open.committed && !open.close);
        assert_eq!(build_drafts(&open, "ao").len(), 1);

        let closing = plan_commit(
            body(
                json!({"witness": "w", "txn": "t1", "commit": true, "ops": [set(S2, json!("b"))]}),
            ),
            &Limits::default(),
        )
        .unwrap();
        assert!(closing.committed && closing.close);
        let drafts = build_drafts(&closing, "ao");
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[1].mode, Mode::Commit);
        assert_eq!(drafts[1].txn.as_deref(), Some("t1"));
        assert!(drafts[1].payload.is_empty() && drafts[1].subjects.is_empty());
        assert_eq!(drafts[1].witness, "w");
        assert_eq!(drafts[1].vocab, "ao");

        let only = plan_commit(
            body(json!({"witness": "w", "txn": "t1", "commit": true})),
            &Limits::default(),
        )
        .unwrap();
        assert!(only.frames.is_empty() && only.close && only.committed);
        assert_eq!(build_drafts(&only, "ao").len(), 1);
    }

    #[test]
    fn empty_requests_and_orphan_commit_false_are_refused() {
        assert_eq!(
            refusal(json!({"witness": "w", "ops": []})).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            refusal(json!({"witness": "w", "txn": "t", "ops": []})).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            refusal(json!({"witness": "w", "commit": false, "ops": [set(S1, json!("a"))]})).status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_request_that_needs_several_frames_gets_a_minted_txn_and_a_commit() {
        let limits = Limits {
            frame_payload: 400,
            header_subjects: HEADER_SUBJECT_BUDGET,
        };
        let ops: Vec<Value> = (0..12)
            .map(|i| set(&format!("urn:ao:term/id={i}"), json!("x".repeat(60))))
            .collect();
        let plan = plan_commit(body(json!({"witness": "w", "ops": ops})), &limits).unwrap();
        assert!(plan.frames.len() > 1, "{} frames", plan.frames.len());
        let txn = plan.txn.clone().expect("a minted txn");
        assert!(txn.starts_with("commit-"));
        assert!(plan.close && plan.committed);
        let drafts = build_drafts(&plan, "ao");
        assert_eq!(drafts.len(), plan.frames.len() + 1);
        assert!(drafts
            .iter()
            .all(|d| d.txn.as_deref() == Some(txn.as_str())));
        assert_eq!(drafts.last().unwrap().mode, Mode::Commit);
        // Every op lands in exactly one frame, in order.
        let seen: usize = plan
            .frames
            .iter()
            .map(|f| {
                serde_json::from_slice::<Value>(&f.payload).unwrap()["ops"]
                    .as_array()
                    .unwrap()
                    .len()
            })
            .sum();
        assert_eq!(seen, 12);
    }

    #[test]
    fn the_header_subject_budget_cuts_frames_before_the_payload_does() {
        let limits = Limits {
            frame_payload: FRAME_PAYLOAD_TARGET,
            header_subjects: 300,
        };
        let ops: Vec<Value> = (0..10)
            .map(|i| set(&format!("urn:ao:term/id={i}"), json!("a")))
            .collect();
        let plan = plan_commit(body(json!({"witness": "w", "ops": ops})), &limits).unwrap();
        assert!(plan.frames.len() > 1);
        for frame in &plan.frames {
            let cost: usize = frame
                .subjects
                .iter()
                .map(|s| s.subject.len() + s.class.len() + SUBJECT_ENTRY_OVERHEAD)
                .sum();
            assert!(cost <= 300 || frame.subjects.len() == 1);
        }
    }

    #[test]
    fn two_ops_on_one_subject_share_one_header_entry() {
        let other = json!({"op": "set", "subject": S1, "class": CLASS,
            "predicate": "urn:ao:term#description", "value": "d"});
        let plan = plan_commit(
            body(json!({"witness": "w", "ops": [set(S1, json!("a")), other]})),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(plan.frames[0].subjects.len(), 1);
    }

    #[test]
    fn a_subject_has_one_class() {
        let clash = json!({"op": "retract", "subject": S1, "class": "urn:ao:other"});
        let error = refusal(json!({"witness": "w", "ops": [set(S1, json!("a")), clash]}));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("one class"), "{}", error.message);
    }

    #[test]
    fn a_frame_of_retracts_only_is_a_retract_frame() {
        let plan = plan_commit(
            body(json!({"witness": "w", "ops": [
                {"op": "retract", "subject": S1, "class": CLASS},
                {"op": "retract", "subject": S2, "class": CLASS}]})),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(plan.frames[0].mode, Mode::Retract);
        let mixed = plan_commit(
            body(json!({"witness": "w", "ops": [
                {"op": "retract", "subject": S1, "class": CLASS}, set(S2, json!("a"))]})),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(mixed.frames[0].mode, Mode::Assert);
    }

    #[test]
    fn the_payload_is_canonical_and_exactly_what_lane_f_reads() {
        let plan = plan_commit(
            body(json!({"witness": "w", "writer": "nidra-worker-1", "ops": [
                set(S1, json!("a")),
                {"op": "add", "subject": S1, "class": CLASS, "predicate": "urn:ao:term#tag",
                 "values": [3, true, {"@id": "urn:ao:tag/id=1"}, {"@value": "hola", "@language": "es"}]},
                {"op": "retract", "subject": S2, "class": CLASS}]})),
            &Limits::default(),
        )
        .unwrap();
        let payload = &plan.frames[0].payload;
        let value: Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(
            payload,
            &serde_json_canonicalizer::to_vec(&value).unwrap(),
            "the assembled payload is already canonical"
        );
        assert_eq!(
            value.as_object().unwrap().len(),
            1,
            "only `ops` rides the payload"
        );
        // Lane F's reader accepts it, op for op.
        let ops =
            crate::emporium::ledger_fold::ops_from_payload(payload).expect("the fold reads it");
        assert_eq!(ops.len(), 3);
        assert_eq!(plan.writer.as_deref(), Some("nidra-worker-1"));
    }

    #[test]
    fn op_errors_name_the_op_by_index() {
        for bad in [
            json!({"op": "upsert", "subject": S1, "class": CLASS}),
            json!({"op": "set", "subject": "not an iri", "class": CLASS, "predicate": NAME, "value": "x"}),
            json!({"op": "set", "subject": S1, "class": CLASS}),
            json!({"op": "set", "subject": S1, "class": CLASS, "predicate": RDF_TYPE, "value": "x"}),
            json!({"op": "add", "subject": S1, "class": CLASS, "predicate": NAME, "value": "x"}),
            json!({"op": "retract", "subject": S1, "class": CLASS, "predicate": NAME}),
            json!({"op": "set", "subject": S1, "class": CLASS, "predicate": NAME, "value": [1]}),
            json!({"op": "set", "subject": S1, "class": CLASS, "predicate": NAME, "extra": 1}),
            json!({"op": "set", "subject": S1, "class": CLASS, "predicate": NAME,
                   "value": {"@value": "x", "color": 1}}),
            json!("not an object"),
        ] {
            let error =
                refusal(json!({"witness": "w", "ops": [set(S2, json!("ok")), bad.clone()]}));
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{bad}");
            assert!(
                error.message.starts_with("ops[1]:"),
                "{bad}: {}",
                error.message
            );
        }
    }

    #[test]
    fn empty_values_and_malformed_literals_are_refused() {
        let empty =
            json!({"op": "add", "subject": S1, "class": CLASS, "predicate": NAME, "values": []});
        let error = refusal(json!({"witness": "w", "ops": [empty]}));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("empty"), "{}", error.message);

        for (lexical, local) in [
            ("abc", "integer"),
            ("1.5", "long"),
            ("yes", "boolean"),
            ("not a number", "double"),
            ("yesterday", "dateTime"),
        ] {
            let op = json!({"op": "set", "subject": S1, "class": CLASS, "predicate": NAME,
                "value": {"@value": lexical,
                          "@type": format!("http://www.w3.org/2001/XMLSchema#{local}")}});
            let error = refusal(json!({"witness": "w", "ops": [op]}));
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{lexical} {local}");
            assert!(error.message.starts_with("ops[0]:"), "{}", error.message);
        }
        let fine = json!({"op": "set", "subject": S1, "class": CLASS, "predicate": NAME,
            "value": {"@value": "7", "@type": "http://www.w3.org/2001/XMLSchema#long"}});
        assert!(plan_commit(
            body(json!({"witness": "w", "ops": [fine]})),
            &Limits::default()
        )
        .is_ok());
    }

    #[test]
    fn a_long_iri_is_refused() {
        let long = format!("urn:ao:term/id={}", "x".repeat(MAX_IRI_BYTES));
        let error = refusal(json!({"witness": "w", "ops": [set(&long, json!("a"))]}));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(
            error.message.contains("subject is over"),
            "{}",
            error.message
        );
    }

    #[test]
    fn desired_triples_keep_the_last_set_and_drop_retracted_subjects() {
        let plan = plan_commit(
            body(json!({"witness": "w", "ops": [
                set(S1, json!("first")),
                set(S1, json!("second")),
                set(S2, json!("gone")),
                {"op": "retract", "subject": S2, "class": CLASS},
                {"op": "add", "subject": "urn:ao:term/id=3", "class": CLASS,
                 "predicate": "urn:ao:term#tag", "values": ["a", "b", "a"]},
                {"op": "remove", "subject": "urn:ao:term/id=3", "class": CLASS,
                 "predicate": "urn:ao:term#tag", "values": ["b"]},
                {"op": "set", "subject": "urn:ao:term/id=4", "class": CLASS,
                 "predicate": NAME, "value": null}]})),
            &Limits::default(),
        )
        .unwrap();
        let rendered: BTreeSet<String> = plan
            .desired
            .iter()
            .map(|(s, p, o)| format!("{s} {p} {}", o.as_nt()))
            .collect();
        let expected: BTreeSet<String> = [
            format!("{S1} {RDF_TYPE} <{CLASS}>"),
            format!("{S1} {NAME} \"second\""),
            format!("urn:ao:term/id=3 {RDF_TYPE} <{CLASS}>"),
            "urn:ao:term/id=3 urn:ao:term#tag \"a\"".to_string(),
        ]
        .into_iter()
        .collect();
        assert_eq!(rendered, expected);
    }

    #[test]
    fn a_remove_alone_asserts_nothing_to_validate() {
        let plan = plan_commit(
            body(json!({"witness": "w", "ops": [
                {"op": "remove", "subject": S1, "class": CLASS, "predicate": NAME, "values": ["a"]},
                {"op": "retract", "subject": S2, "class": CLASS}]})),
            &Limits::default(),
        )
        .unwrap();
        assert!(plan.desired.is_empty());
    }

    #[test]
    fn txn_ids_are_plain() {
        assert!(check_txn("import-7f3a.b:c_1").is_ok());
        assert!(check_txn("has space").is_err());
        assert!(check_txn("").is_err());
        assert!(check_txn(&"x".repeat(129)).is_err());
        // W7d: the prefix of the minted ids is reserved, whatever follows it; the word elsewhere is
        // fine. Mutation: drop the prefix check and the first two are accepted.
        for reserved in ["commit-0123456789abcdef0123456789abcdef", "commit-1"] {
            let refusal = check_txn(reserved).expect_err(reserved);
            assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
            assert!(refusal.message.contains("reserved"), "{}", refusal.message);
        }
        assert!(check_txn("import-commit-1").is_ok());
        assert!(check_txn("commit").is_ok());
    }

    #[test]
    fn converge_errors_map_to_the_error_table() {
        assert_eq!(
            from_converge(ConvergeError::Locked("/x".into())).status,
            StatusCode::LOCKED
        );
        assert_eq!(
            from_converge(ConvergeError::Refused("ledger room was evicted".into())).status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            from_converge(ConvergeError::Refused(
                "12 bytes exceeds a frame limit".into()
            ))
            .status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            from_converge(ConvergeError::Decode("x".into())).status,
            StatusCode::BAD_REQUEST
        );
    }

    fn live(last_seq: u64) -> RunRec {
        RunRec {
            last_seq,
            since: Some(Instant::now()),
        }
    }

    fn gate_with(runs: Vec<(&str, RunRec)>) -> CommitGate {
        CommitGate {
            open: AsyncMutex::new(GateState {
                origin: None,
                closed: BTreeSet::new(),
                runs: runs.into_iter().map(|(t, r)| (t.to_string(), r)).collect(),
            }),
        }
    }

    #[tokio::test]
    async fn another_callers_open_txn_blocks_until_it_idles_out_or_closes() {
        let gate = gate_with(vec![("t1", live(1))]);
        // The owner passes; a stranger and a txn-less request are refused after the wait.
        assert!(enter_gate(&gate, Some("t1")).await.is_ok());
        assert_eq!(enter_gate(&gate, Some("t2")).await.unwrap_err(), "t1");
        assert_eq!(enter_gate(&gate, None).await.unwrap_err(), "t1");
        // Once the owner closes it, everyone passes.
        gate.open.lock().await.runs.clear();
        assert!(enter_gate(&gate, None).await.is_ok());
        // An idle transaction no longer blocks, and neither does a run learned from the ledger.
        gate.open.lock().await.runs.insert(
            "t3".into(),
            RunRec {
                last_seq: 3,
                since: Some(Instant::now() - txn_idle() - Duration::from_secs(1)),
            },
        );
        gate.open.lock().await.runs.insert(
            "t4".into(),
            RunRec {
                last_seq: 4,
                since: None,
            },
        );
        assert!(enter_gate(&gate, None).await.is_ok());
    }

    /// W7b F1. The check is against the cell's newest seq: equal is intact, anything past it names
    /// the first stranger frame; a commit-only request for a run the gate does not hold is refused.
    /// Mutation: compare nothing (always `Ok`) and the broken and commit-only cases go red.
    #[test]
    fn a_run_is_continued_only_while_it_is_intact() {
        let state = GateState {
            origin: None,
            closed: BTreeSet::new(),
            runs: [("t1".to_string(), live(3))].into_iter().collect(),
        };
        assert!(check_run_intact(&state, "t1", 3, false).is_ok());
        assert!(check_run_intact(&state, "t1", 3, true).is_ok());
        for commit_only in [false, true] {
            let broken = check_run_intact(&state, "t1", 5, commit_only).unwrap_err();
            assert_eq!(broken.status, StatusCode::CONFLICT);
            assert_eq!(broken.broken_txn, Some(("t1".to_string(), Some(4))));
        }
        // A fresh transaction passes; a commit-only request for one has nothing to close.
        assert!(check_run_intact(&state, "t9", 5, false).is_ok());
        let nothing = check_run_intact(&state, "t9", 5, true).unwrap_err();
        assert_eq!(nothing.status, StatusCode::CONFLICT);
        assert_eq!(nothing.broken_txn, Some(("t9".to_string(), None)));
        // A ledger that lost frames (a tip below the run's end) is broken too, with no culprit seq.
        let lost = check_run_intact(&state, "t1", 2, false).unwrap_err();
        assert_eq!(lost.broken_txn, Some(("t1".to_string(), None)));
    }

    /// W7b-2 c. A transaction the ledger already committed is refused `txnClosed`, for an ops
    /// request and for a commit-only one; the id this route mints is never remembered. Mutation:
    /// drop the `closed` test in `check_run_intact` and the first two assertions go red.
    #[test]
    fn a_closed_transaction_is_refused_for_ops_and_for_commit_only() {
        let mut state = GateState::default();
        state.closed.insert("t7".to_string());
        for commit_only in [false, true] {
            let closed = check_run_intact(&state, "t7", 9, commit_only).unwrap_err();
            assert_eq!(closed.status, StatusCode::CONFLICT);
            assert_eq!(closed.closed_txn.as_deref(), Some("t7"));
            assert!(closed.broken_txn.is_none());
        }
        assert!(check_run_intact(&state, "t8", 9, false).is_ok());
        assert!(is_auto_txn("commit-0123456789abcdef0123456789abcdef"));
        assert!(!is_auto_txn("commit-1"));
        assert!(!is_auto_txn("my-txn"));
    }

    /// W7b F2. The writer rides every frame's header extra, the commit frame's included, and a
    /// request without one leaves the extra empty (so the frame bytes are what they were).
    #[test]
    fn the_writer_is_in_every_drafts_extra() {
        let with = plan_commit(
            body(
                json!({"witness": "w", "writer": "nidra-1", "txn": "t1", "commit": true,
                        "ops": [set(S1, json!("a"))]}),
            ),
            &Limits::default(),
        )
        .unwrap();
        let drafts = build_drafts(&with, "ao");
        assert_eq!(drafts.len(), 2, "the ops frame and the commit frame");
        for draft in &drafts {
            assert_eq!(draft.extra["writer"], "nidra-1");
        }
        let without = plan_commit(
            body(json!({"witness": "w", "ops": [set(S1, json!("a"))]})),
            &Limits::default(),
        )
        .unwrap();
        assert!(build_drafts(&without, "ao")[0].extra.is_empty());
    }

    /// W7c. A set takes `values` (any number, none clears) or the scalar `value`, never both; the
    /// frame carries the list form; a typed literal in the list is lexically checked; the SHACL
    /// gate sees every value of the set. Mutation: reject `values` on a set (the W7b reader) and the
    /// first assertion goes red; skip `check_term_lexical` over the list and the bad-literal case
    /// is accepted.
    #[test]
    fn a_set_takes_a_list_of_values_and_the_frame_carries_it() {
        let list = |values: Value| json!({"op": "set", "subject": S1, "class": CLASS, "predicate": NAME, "values": values});
        let plan = plan_commit(
            body(json!({"witness": "w", "ops": [list(json!(["a", "b", {"@id": "urn:o"}]))]})),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(plan.frames.len(), 1);
        let payload: Value = serde_json::from_slice(&plan.frames[0].payload).unwrap();
        assert_eq!(payload["ops"][0]["values"].as_array().unwrap().len(), 3);
        assert!(payload["ops"][0].get("value").is_none());
        // the SHACL gate is shown every value of the set
        let props: Vec<_> = plan
            .desired
            .iter()
            .filter(|(_, p, _)| p == NAME)
            .map(|(_, _, o)| o.as_nt())
            .collect();
        assert_eq!(props.len(), 3, "{props:?}");

        // [] clears (accepted); the scalar form is the one-element case; null clears
        for ops in [
            json!([list(json!([]))]),
            json!([set(S1, json!("x"))]),
            json!([set(S1, Value::Null)]),
        ] {
            plan_commit(
                body(json!({"witness": "w", "ops": ops})),
                &Limits::default(),
            )
            .unwrap();
        }
        // a set after a set replaces: the desired state holds the last list only
        let plan = plan_commit(
            body(json!({"witness": "w", "ops": [list(json!(["a", "b"])), list(json!(["c"]))]})),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(plan.desired.iter().filter(|(_, p, _)| p == NAME).count(), 1);

        // refused: both shapes, a non-array, a null member, a malformed typed literal
        let both = json!({"op": "set", "subject": S1, "class": CLASS, "predicate": NAME,
                          "value": "x", "values": ["y"]});
        for bad in [
            both,
            list(json!("x")),
            list(json!(["x", null])),
            list(json!([{"@value": "abc", "@type": "http://www.w3.org/2001/XMLSchema#integer"}])),
        ] {
            let error = refusal(json!({"witness": "w", "ops": [bad.clone()]}));
            assert_eq!(error.status, StatusCode::BAD_REQUEST, "{bad}");
        }
    }
}

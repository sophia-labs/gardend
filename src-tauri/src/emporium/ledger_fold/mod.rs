//! The ledger fold, core (W7 lane F): frames to heads to triples. Pure over frames.
//!
//! A frame's payload is a list of ops. [`decode_event`] turns a decoded ledger frame into a
//! [`FoldEvent`]; [`HeadTable::apply`] folds one event into the head table and returns the
//! [`Delta`] of triples that changes; [`apply_delta`] writes a Delta into a store transaction.
//! Nothing here touches an app handle, a registry or the file system.
//!
//! # Payload (mode `assert`, `supersede` or `retract`), canonical JSON
//!
//! ```json
//! {"ops":[
//!   {"op":"set","subject":"urn:ao:term/id=1","class":"urn:ao:term","predicate":"urn:ao:term#name","value":{"@value":"x"}},
//!   {"op":"add","subject":"...","class":"...","predicate":"...","values":[{"@id":"urn:ao:tag/id=2"}]},
//!   {"op":"remove","subject":"...","class":"...","predicate":"...","values":["x"]},
//!   {"op":"retract","subject":"...","class":"..."}
//! ]}
//! ```
//!
//! Terms: `{"@id": iri}`; `{"@value": "..", "@type": xsd-iri}`; `{"@value": "..", "@language": ".."}`;
//! a bare JSON string is a simple (xsd:string) literal, a bare integer an xsd:integer, any other
//! bare number an xsd:double, a bare boolean an xsd:boolean. Op and term (de)serialization lives in
//! this file only ([`op_from_json`], [`op_to_json`], [`ops_from_payload`], [`ops_to_value`],
//! [`term_from_json`], [`term_to_json`]); the commit route (lane S) validates requests with the
//! same functions so a request that passes is a payload the fold can read.
//!
//! # Semantics (decisions 1.1 to 1.3)
//!
//! * The grain is (subject, predicate). Policy per predicate comes from [`FoldPolicy`].
//! * `set` on an lww key writes the value (`value` absent or null clears the key); on an add-wins
//!   key it replaces what the frame observed (clear, then add). `add` on an lww key is `set` of the
//!   last value; `remove` on an lww key clears it.
//! * `add` on an add-wins key adds each value with a tag (the op's stamp). `remove` kills the tags
//!   of that value that it observed: tags of the same origin written earlier, and tags of frames it
//!   names as parent (the parents of the subject in the frame header). A concurrent add survives.
//! * `retract` removes every head of the subject written before it (by stamp); a later write
//!   revives the subject with only the newer heads.
//! * Stamp order is (HLC, origin id, seq, op index). Equality of values for the no-op check and for
//!   lww is `terms::canon_value`.

#![allow(dead_code, unused_imports)]

use std::collections::BTreeSet;
use std::fmt;

use garden_ledger::frame::{to_hex, Decoded, Header};
use garden_ledger::{Mode, PayloadState};
use oxigraph::model::{Literal, NamedNode};
use serde_json::{Map, Value};

use crate::emporium::terms::{Term, Triple, PLACEHOLDER_NS};

pub(crate) mod apply;
pub(crate) mod heads;
pub(crate) mod policy;
#[cfg(test)]
mod props;

pub(crate) use apply::{apply_delta, apply_delta_commit};
pub(crate) use garden_ledger::Hlc;
pub(crate) use heads::{CanonView, HeadKey, HeadRow, HeadTable, HeadValue, Stamp};
pub(crate) use policy::{FoldPolicy, Merge};

/// Lowercase hex id of a frame (blake3 of its header bytes).
pub(crate) type FrameId = String;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
pub(crate) const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A payload or op the fold cannot read. A fold error is never a panic: the host quarantines the
/// event and goes on with the next origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FoldError {
    /// The payload as a whole: not JSON, wrong envelope, damaged bytes.
    Payload(String),
    /// One op (by index in the payload) is malformed.
    Op {
        /// Index of the op in `ops`.
        index: usize,
        /// What is wrong with it.
        message: String,
    },
    /// A term outside any op (see [`term_from_json`]).
    Term(String),
}

impl fmt::Display for FoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FoldError::Payload(m) => write!(f, "fold payload: {m}"),
            FoldError::Op { index, message } => write!(f, "fold op {index}: {message}"),
            FoldError::Term(m) => write!(f, "fold term: {m}"),
        }
    }
}

impl std::error::Error for FoldError {}

// ---------------------------------------------------------------------------
// Events, ops, deltas
// ---------------------------------------------------------------------------

/// A subject a frame names in its header, with the frames it was written on top of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubjectRef {
    pub subject: String,
    pub class: String,
    pub parents: Vec<FrameId>,
}

/// One op of a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    /// Write one value (or clear the key when `value` is `None`).
    Set {
        subject: String,
        class: String,
        predicate: String,
        value: Option<Term>,
    },
    /// Add values to a set.
    Add {
        subject: String,
        class: String,
        predicate: String,
        values: Vec<Term>,
    },
    /// Remove values from a set.
    Remove {
        subject: String,
        class: String,
        predicate: String,
        values: Vec<Term>,
    },
    /// Retract the whole subject.
    Retract { subject: String, class: String },
}

impl Op {
    /// Subject IRI.
    pub(crate) fn subject(&self) -> &str {
        match self {
            Op::Set { subject, .. }
            | Op::Add { subject, .. }
            | Op::Remove { subject, .. }
            | Op::Retract { subject, .. } => subject,
        }
    }

    /// Class IRI.
    pub(crate) fn class(&self) -> &str {
        match self {
            Op::Set { class, .. }
            | Op::Add { class, .. }
            | Op::Remove { class, .. }
            | Op::Retract { class, .. } => class,
        }
    }

    /// Predicate IRI (none for `retract`).
    pub(crate) fn predicate(&self) -> Option<&str> {
        match self {
            Op::Set { predicate, .. }
            | Op::Add { predicate, .. }
            | Op::Remove { predicate, .. } => Some(predicate),
            Op::Retract { .. } => None,
        }
    }

    /// The wire name of the op.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Op::Set { .. } => "set",
            Op::Add { .. } => "add",
            Op::Remove { .. } => "remove",
            Op::Retract { .. } => "retract",
        }
    }
}

/// A decoded frame: the header facts the fold needs and the payload's ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoldEvent {
    pub origin: String,
    pub seq: u64,
    pub id: FrameId,
    pub hlc: Hlc,
    pub witness: String,
    pub vocab: String,
    pub mode: Mode,
    pub txn: Option<String>,
    pub subjects: Vec<SubjectRef>,
    pub ops: Vec<Op>,
    /// The payload was zeroed by a redaction: the event decodes with no ops.
    pub redacted: bool,
}

impl FoldEvent {
    /// Parents the header names for `subject` (empty when it names none).
    pub(crate) fn parents_of(&self, subject: &str) -> &[FrameId] {
        self.subjects
            .iter()
            .find(|s| s.subject == subject)
            .map(|s| s.parents.as_slice())
            .unwrap_or(&[])
    }

    /// An assert event built from parts, with a synthetic id (see [`synthetic_id`]). For tests and
    /// for hosts that fold events that did not come from a frame.
    pub(crate) fn synthetic(
        origin: &str,
        seq: u64,
        hlc: Hlc,
        parents: &[FrameId],
        ops: Vec<Op>,
    ) -> FoldEvent {
        let mut subjects: Vec<SubjectRef> = Vec::new();
        for op in &ops {
            if !subjects.iter().any(|s| s.subject == op.subject()) {
                subjects.push(SubjectRef {
                    subject: op.subject().to_string(),
                    class: op.class().to_string(),
                    parents: parents.to_vec(),
                });
            }
        }
        FoldEvent {
            origin: origin.to_string(),
            seq,
            id: synthetic_id(origin, seq),
            hlc,
            witness: "urn:witness:synthetic".to_string(),
            vocab: "synthetic".to_string(),
            mode: Mode::Assert,
            txn: None,
            subjects,
            ops,
            redacted: false,
        }
    }
}

/// The id [`FoldEvent::synthetic`] gives to frame `seq` of `origin`.
pub(crate) fn synthetic_id(origin: &str, seq: u64) -> FrameId {
    format!("{origin}#{seq}")
}

/// What one event changes in the sink: triples to remove, triples to insert, the subjects it
/// touched (the fold owns exactly the subjects it has frames for), and the keys it flagged as
/// contested.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Delta {
    pub insert: Vec<Triple>,
    pub remove: Vec<Triple>,
    /// Every subject named by an op of the event, including ops that changed nothing.
    pub touched: BTreeSet<String>,
    /// Keys the event flagged contested (a concurrent write under `Merge::Contested`).
    pub contested: Vec<HeadKey>,
}

impl Delta {
    /// True when the event changes no triple (a value-equal re-assert, a replay, a lost write).
    pub(crate) fn is_empty(&self) -> bool {
        self.insert.is_empty() && self.remove.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Terms and ops: JSON
// ---------------------------------------------------------------------------

fn iri(s: &str, what: &str) -> Result<NamedNode, String> {
    NamedNode::new(s).map_err(|e| format!("{what} {s:?} is not a valid IRI: {e}"))
}

fn typed(lexical: impl Into<String>, local: &str) -> Term {
    Term::Lit(Literal::new_typed_literal(
        lexical,
        NamedNode::new_unchecked(format!("{XSD}{local}")),
    ))
}

/// Read one term from its JSON form (see the module docs).
pub(crate) fn term_from_json(v: &Value) -> Result<Term, FoldError> {
    term_inner(v).map_err(FoldError::Term)
}

fn term_inner(v: &Value) -> Result<Term, String> {
    match v {
        Value::String(s) => Ok(Term::Lit(Literal::new_simple_literal(s.clone()))),
        Value::Bool(b) => Ok(typed(if *b { "true" } else { "false" }, "boolean")),
        Value::Number(n) => Ok(if n.is_i64() || n.is_u64() {
            typed(n.to_string(), "integer")
        } else {
            typed(n.to_string(), "double")
        }),
        Value::Object(m) => object_term(m),
        Value::Null => Err("null is not a term".to_string()),
        Value::Array(_) => Err("an array is not a term".to_string()),
    }
}

fn object_term(m: &Map<String, Value>) -> Result<Term, String> {
    for k in m.keys() {
        if !matches!(k.as_str(), "@id" | "@value" | "@type" | "@language") {
            return Err(format!("unknown term key {k:?}"));
        }
    }
    if let Some(id) = m.get("@id") {
        if m.len() != 1 {
            return Err("@id cannot be combined with other keys".to_string());
        }
        let s = id.as_str().ok_or("@id must be a string")?;
        return Ok(Term::Uri(iri(s, "@id")?));
    }
    let val = m.get("@value").ok_or("a term object needs @id or @value")?;
    let ty = m.get("@type");
    let lang = m.get("@language");
    if ty.is_some() && lang.is_some() {
        return Err("a term cannot have both @type and @language".to_string());
    }
    let lexical: String = match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return Err("@value must be a string, number or boolean".to_string()),
    };
    if let Some(l) = lang {
        let l = l.as_str().ok_or("@language must be a string")?;
        return Literal::new_language_tagged_literal(lexical, l)
            .map(Term::Lit)
            .map_err(|e| format!("bad language tag {l:?}: {e}"));
    }
    if let Some(t) = ty {
        let t = t.as_str().ok_or("@type must be a string")?;
        return Ok(Term::Lit(Literal::new_typed_literal(
            lexical,
            iri(t, "@type")?,
        )));
    }
    match val {
        Value::String(_) => Ok(Term::Lit(Literal::new_simple_literal(lexical))),
        other => term_inner(other),
    }
}

/// The JSON form of a term ([`term_from_json`] reads it back to an equal term).
pub(crate) fn term_to_json(t: &Term) -> Value {
    let mut m = Map::new();
    match t {
        Term::Uri(n) => {
            m.insert("@id".into(), Value::String(n.as_str().to_string()));
        }
        Term::Placeholder(name) => {
            m.insert(
                "@id".into(),
                Value::String(format!("{PLACEHOLDER_NS}{name}")),
            );
        }
        Term::Lit(l) => {
            m.insert("@value".into(), Value::String(l.value().to_string()));
            if let Some(lang) = l.language() {
                m.insert("@language".into(), Value::String(lang.to_string()));
            } else if l.datatype().as_str() != format!("{XSD}string") {
                m.insert(
                    "@type".into(),
                    Value::String(l.datatype().as_str().to_string()),
                );
            }
        }
    }
    Value::Object(m)
}

/// Check that a typed literal's lexical form is well-formed for the XSD types the fold compares
/// (integer family, decimal family, boolean, dateTime). Other datatypes pass. The fold itself
/// never calls this (it folds what the ledger holds); the commit route does, before it appends.
pub(crate) fn check_term_lexical(t: &Term) -> Result<(), String> {
    let Term::Lit(l) = t else {
        return Ok(());
    };
    if l.language().is_some() {
        return Ok(());
    }
    let dt = l.datatype().as_str();
    let Some(local) = dt.strip_prefix(XSD) else {
        return Ok(());
    };
    let v = l.value();
    let ok = match local {
        "integer" | "long" | "int" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "negativeInteger" | "nonPositiveInteger" | "unsignedLong"
        | "unsignedInt" => {
            let digits = v.strip_prefix(&['+', '-'][..]).unwrap_or(v);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        }
        "double" | "float" | "decimal" => {
            v.parse::<f64>().is_ok() || matches!(v, "INF" | "-INF" | "+INF" | "NaN")
        }
        "boolean" => matches!(v, "true" | "false" | "1" | "0"),
        "dateTime" => {
            chrono::DateTime::parse_from_rfc3339(v).is_ok()
                || chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S").is_ok()
                || chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
        }
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{v:?} is not a valid xsd:{local}"))
    }
}

fn iri_field(m: &Map<String, Value>, key: &str) -> Result<String, String> {
    let s = m
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("op needs a string {key:?}"))?;
    iri(s, key)?;
    Ok(s.to_string())
}

/// Read one op from its JSON form. `index` is its position in `ops`, for the error.
pub(crate) fn op_from_json(index: usize, v: &Value) -> Result<Op, FoldError> {
    op_inner(v).map_err(|message| FoldError::Op { index, message })
}

fn op_inner(v: &Value) -> Result<Op, String> {
    let m = v.as_object().ok_or("an op must be an object")?;
    let kind = m
        .get("op")
        .and_then(Value::as_str)
        .ok_or("an op needs a string \"op\"")?;
    let allowed: &[&str] = match kind {
        "set" => &["op", "subject", "class", "predicate", "value"],
        "add" | "remove" => &["op", "subject", "class", "predicate", "values"],
        "retract" => &["op", "subject", "class"],
        other => return Err(format!("unknown op {other:?}")),
    };
    for k in m.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(format!("op {kind:?} does not take {k:?}"));
        }
    }
    let subject = iri_field(m, "subject")?;
    let class = iri_field(m, "class")?;
    if kind == "retract" {
        return Ok(Op::Retract { subject, class });
    }
    let predicate = iri_field(m, "predicate")?;
    if predicate == RDF_TYPE {
        return Err("rdf:type is the class of the subject; use the class field".to_string());
    }
    if kind == "set" {
        let value = match m.get("value") {
            None | Some(Value::Null) => None,
            Some(v) => Some(term_inner(v)?),
        };
        return Ok(Op::Set {
            subject,
            class,
            predicate,
            value,
        });
    }
    let arr = m
        .get("values")
        .and_then(Value::as_array)
        .ok_or("an add or remove op needs a \"values\" array")?;
    let values = arr
        .iter()
        .map(term_inner)
        .collect::<Result<Vec<Term>, String>>()?;
    Ok(if kind == "add" {
        Op::Add {
            subject,
            class,
            predicate,
            values,
        }
    } else {
        Op::Remove {
            subject,
            class,
            predicate,
            values,
        }
    })
}

/// The JSON form of an op ([`op_from_json`] reads it back to an equal op).
pub(crate) fn op_to_json(op: &Op) -> Value {
    let mut m = Map::new();
    m.insert("op".into(), Value::String(op.kind().to_string()));
    m.insert("subject".into(), Value::String(op.subject().to_string()));
    m.insert("class".into(), Value::String(op.class().to_string()));
    if let Some(p) = op.predicate() {
        m.insert("predicate".into(), Value::String(p.to_string()));
    }
    match op {
        Op::Set { value, .. } => {
            if let Some(t) = value {
                m.insert("value".into(), term_to_json(t));
            }
        }
        Op::Add { values, .. } | Op::Remove { values, .. } => {
            m.insert(
                "values".into(),
                Value::Array(values.iter().map(term_to_json).collect()),
            );
        }
        Op::Retract { .. } => {}
    }
    Value::Object(m)
}

/// `{"ops": [...]}` as a JSON value.
pub(crate) fn ops_to_value(ops: &[Op]) -> Value {
    let mut m = Map::new();
    m.insert(
        "ops".into(),
        Value::Array(ops.iter().map(op_to_json).collect()),
    );
    Value::Object(m)
}

/// `{"ops": [...]}` as JSON bytes (not canonicalized: the writer canonicalizes, see
/// `garden_ledger`).
pub(crate) fn ops_to_payload(ops: &[Op]) -> Vec<u8> {
    serde_json::to_vec(&ops_to_value(ops)).unwrap_or_default()
}

/// Read the ops of a payload.
pub(crate) fn ops_from_payload(bytes: &[u8]) -> Result<Vec<Op>, FoldError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| FoldError::Payload(format!("payload is not JSON: {e}")))?;
    ops_from_value(&v)
}

/// Read the ops of an already parsed payload (`{"ops": [...]}`).
pub(crate) fn ops_from_value(v: &Value) -> Result<Vec<Op>, FoldError> {
    let m = v
        .as_object()
        .ok_or_else(|| FoldError::Payload("payload must be a JSON object".to_string()))?;
    for k in m.keys() {
        if k != "ops" {
            return Err(FoldError::Payload(format!("unknown payload key {k:?}")));
        }
    }
    let arr = m
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| FoldError::Payload("payload needs an \"ops\" array".to_string()))?;
    arr.iter()
        .enumerate()
        .map(|(i, o)| op_from_json(i, o))
        .collect()
}

// ---------------------------------------------------------------------------
// Frames to events
// ---------------------------------------------------------------------------

/// Decode a ledger frame into a [`FoldEvent`].
///
/// `assert`, `supersede` and `retract` frames carry ops; their payload must parse. `commit`,
/// `redact` and modes this build does not know decode with no ops (a reader never refuses a frame
/// over its mode). A redacted payload decodes with no ops and `redacted: true`; a damaged payload is
/// a [`FoldError::Payload`].
pub(crate) fn decode_event(frame: &Decoded) -> Result<FoldEvent, FoldError> {
    decode_parts(
        &frame.header,
        &to_hex(&frame.id),
        &frame.payload,
        frame.state,
    )
}

/// [`decode_event`] over the parts of a frame, for a caller that holds them apart.
pub(crate) fn decode_parts(
    header: &Header,
    id: &str,
    payload: &[u8],
    state: PayloadState,
) -> Result<FoldEvent, FoldError> {
    let carries_ops = matches!(header.mode, Mode::Assert | Mode::Supersede | Mode::Retract);
    let mut redacted = false;
    let ops = if carries_ops {
        match state {
            PayloadState::Intact => ops_from_payload(payload)?,
            PayloadState::Redacted => {
                redacted = true;
                Vec::new()
            }
            PayloadState::Damaged => {
                return Err(FoldError::Payload(
                    "payload does not match its hash and is not redacted".to_string(),
                ))
            }
        }
    } else {
        Vec::new()
    };
    Ok(FoldEvent {
        origin: header.origin.0.clone(),
        seq: header.seq,
        id: id.to_string(),
        hlc: header.hlc,
        witness: header.witness.clone(),
        vocab: header.vocab.clone(),
        mode: header.mode.clone(),
        txn: header.txn.clone(),
        subjects: header
            .subjects
            .iter()
            .map(|s| SubjectRef {
                subject: s.subject.clone(),
                class: s.class.clone(),
                parents: s.parents.clone(),
            })
            .collect(),
        ops,
        redacted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use garden_ledger::OriginId;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn header(mode: Mode) -> Header {
        Header {
            v: 1,
            origin: OriginId("cell-0001".to_string()),
            seq: 7,
            hlc: Hlc { ms: 1000, count: 2 },
            witness: "urn:w:1".to_string(),
            vocab: "ao".to_string(),
            subjects: vec![garden_ledger::Subject {
                subject: "urn:ao:term/id=1".to_string(),
                class: "urn:ao:term".to_string(),
                parents: vec!["ab".repeat(32)],
            }],
            mode,
            payload_hash: "00".repeat(32),
            payload_len: 0,
            txn: Some("t1".to_string()),
            sig: None,
            extra: BTreeMap::new(),
        }
    }

    fn decoded(mode: Mode, payload: &[u8], state: PayloadState) -> Decoded {
        Decoded {
            header: header(mode),
            id: [7u8; 32],
            payload: payload.to_vec(),
            state,
            frame_len: payload.len() + 100,
        }
    }

    fn all_ops_payload() -> Vec<u8> {
        serde_json::to_vec(&json!({"ops": [
            {"op": "set", "subject": "urn:ao:term/id=1", "class": "urn:ao:term",
             "predicate": "urn:ao:term#name", "value": {"@value": "x"}},
            {"op": "set", "subject": "urn:ao:term/id=1", "class": "urn:ao:term",
             "predicate": "urn:ao:term#note"},
            {"op": "add", "subject": "urn:ao:term/id=1", "class": "urn:ao:term",
             "predicate": "urn:ao:term#tag", "values": [{"@id": "urn:ao:tag/id=2"}, "plain"]},
            {"op": "remove", "subject": "urn:ao:term/id=1", "class": "urn:ao:term",
             "predicate": "urn:ao:term#tag", "values": [7]},
            {"op": "retract", "subject": "urn:ao:term/id=9", "class": "urn:ao:term"}
        ]}))
        .unwrap()
    }

    #[test]
    fn decode_reads_every_op_kind_and_the_header_facts() {
        let d = decoded(Mode::Assert, &all_ops_payload(), PayloadState::Intact);
        let ev = decode_event(&d).expect("decodes");
        assert_eq!(ev.origin, "cell-0001");
        assert_eq!(ev.seq, 7);
        assert_eq!(ev.id, "07".repeat(32));
        assert_eq!(ev.hlc, Hlc { ms: 1000, count: 2 });
        assert_eq!(ev.witness, "urn:w:1");
        assert_eq!(ev.vocab, "ao");
        assert_eq!(ev.mode, Mode::Assert);
        assert_eq!(ev.txn.as_deref(), Some("t1"));
        assert!(!ev.redacted);
        assert_eq!(ev.subjects.len(), 1);
        assert_eq!(ev.parents_of("urn:ao:term/id=1"), &["ab".repeat(32)][..]);
        assert!(ev.parents_of("urn:other").is_empty());
        assert_eq!(ev.ops.len(), 5);
        assert!(matches!(&ev.ops[0], Op::Set { value: Some(_), .. }));
        assert!(matches!(&ev.ops[1], Op::Set { value: None, .. }));
        match &ev.ops[2] {
            Op::Add { values, .. } => {
                assert_eq!(values.len(), 2);
                assert!(matches!(&values[0], Term::Uri(_)));
                assert!(matches!(&values[1], Term::Lit(_)));
            }
            other => panic!("expected add, got {other:?}"),
        }
        match &ev.ops[3] {
            Op::Remove { values, .. } => {
                assert_eq!(values, &vec![typed("7", "integer")]);
            }
            other => panic!("expected remove, got {other:?}"),
        }
        assert_eq!(
            ev.ops[4],
            Op::Retract {
                subject: "urn:ao:term/id=9".into(),
                class: "urn:ao:term".into()
            }
        );
    }

    #[test]
    fn modes_without_ops_decode_empty() {
        for mode in [
            Mode::Commit,
            Mode::Redact,
            Mode::Other("future".to_string()),
        ] {
            let d = decoded(mode.clone(), b"not json at all", PayloadState::Intact);
            let ev = decode_event(&d).expect("a mode without ops never reads the payload");
            assert!(ev.ops.is_empty(), "{mode:?}");
        }
    }

    #[test]
    fn supersede_and_retract_modes_read_their_ops() {
        for mode in [Mode::Supersede, Mode::Retract] {
            let d = decoded(mode, &all_ops_payload(), PayloadState::Intact);
            assert_eq!(decode_event(&d).unwrap().ops.len(), 5);
        }
    }

    #[test]
    fn redacted_payload_decodes_empty_and_flagged_damaged_is_an_error() {
        let d = decoded(Mode::Assert, &[0u8; 16], PayloadState::Redacted);
        let ev = decode_event(&d).unwrap();
        assert!(ev.redacted);
        assert!(ev.ops.is_empty());
        let d = decoded(Mode::Assert, b"{\"ops\":[", PayloadState::Damaged);
        assert!(matches!(decode_event(&d), Err(FoldError::Payload(_))));
    }

    #[test]
    fn term_shapes() {
        let t = |v: Value| term_from_json(&v).expect("term parses");
        assert_eq!(
            t(json!({"@id": "urn:x:1"})),
            Term::Uri(NamedNode::new("urn:x:1").unwrap())
        );
        assert_eq!(
            t(json!({"@value": "7", "@type": "http://www.w3.org/2001/XMLSchema#long"})),
            typed("7", "long")
        );
        assert_eq!(
            t(json!({"@value": "hola", "@language": "ES"})),
            Term::Lit(Literal::new_language_tagged_literal("hola", "es").unwrap())
        );
        assert_eq!(
            t(json!({"@value": "plain"})),
            Term::Lit(Literal::new_simple_literal("plain"))
        );
        assert_eq!(t(json!("plain")), t(json!({"@value": "plain"})));
        assert_eq!(t(json!(42)), typed("42", "integer"));
        assert_eq!(t(json!(-3)), typed("-3", "integer"));
        assert_eq!(t(json!(1.5)), typed("1.5", "double"));
        assert_eq!(t(json!(true)), typed("true", "boolean"));
        assert_eq!(t(json!({"@value": 5})), typed("5", "integer"));
        // xsd:string is the simple literal
        assert_eq!(
            t(json!({"@value": "s", "@type": "http://www.w3.org/2001/XMLSchema#string"})),
            Term::Lit(Literal::new_simple_literal("s"))
        );
    }

    #[test]
    fn terms_round_trip_through_json() {
        let samples = vec![
            Term::Uri(NamedNode::new("urn:x:1").unwrap()),
            typed("7", "long"),
            typed("1.5", "double"),
            typed("true", "boolean"),
            Term::Lit(Literal::new_simple_literal("plain \"quoted\"")),
            Term::Lit(Literal::new_language_tagged_literal("hola", "es").unwrap()),
        ];
        for s in samples {
            let back = term_from_json(&term_to_json(&s)).expect("round trip");
            assert_eq!(back, s);
        }
    }

    #[test]
    fn ops_round_trip_through_json() {
        let ops = ops_from_payload(&all_ops_payload()).unwrap();
        let again = ops_from_payload(&ops_to_payload(&ops)).unwrap();
        assert_eq!(ops, again);
    }

    #[test]
    fn bad_terms_are_errors_not_panics() {
        let bad = vec![
            json!(null),
            json!([1, 2]),
            json!({}),
            json!({"@id": 5}),
            json!({"@id": "not an iri"}),
            json!({"@id": "urn:x:1", "@value": "y"}),
            json!({"@value": "x", "@type": "urn:t", "@language": "es"}),
            json!({"@value": "x", "@language": "not a tag!!"}),
            json!({"@value": "x", "@type": "nope"}),
            json!({"@value": {"nested": 1}}),
            json!({"@value": "x", "@extra": 1}),
            json!({"@language": "es"}),
        ];
        for b in bad {
            assert!(
                matches!(term_from_json(&b), Err(FoldError::Term(_))),
                "{b} should be rejected"
            );
        }
    }

    #[test]
    fn bad_payloads_are_fold_errors_not_panics() {
        let s = "urn:ao:term/id=1";
        let c = "urn:ao:term";
        let p = "urn:ao:term#name";
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("not json", b"{{{".to_vec()),
            ("empty", Vec::new()),
            ("array envelope", b"[]".to_vec()),
            ("no ops key", b"{}".to_vec()),
            ("ops not array", br#"{"ops": 3}"#.to_vec()),
            (
                "unknown envelope key",
                br#"{"ops": [], "extra": 1}"#.to_vec(),
            ),
            ("op not object", br#"{"ops": [3]}"#.to_vec()),
            ("no op name", br#"{"ops": [{}]}"#.to_vec()),
            (
                "unknown op",
                serde_json::to_vec(&json!({"ops":[{"op":"merge","subject":s,"class":c}]})).unwrap(),
            ),
            (
                "no subject",
                serde_json::to_vec(&json!({"ops":[{"op":"retract","class":c}]})).unwrap(),
            ),
            (
                "bad subject iri",
                serde_json::to_vec(&json!({"ops":[{"op":"retract","subject":"x y","class":c}]}))
                    .unwrap(),
            ),
            (
                "no class",
                serde_json::to_vec(&json!({"ops":[{"op":"retract","subject":s}]})).unwrap(),
            ),
            (
                "set without predicate",
                serde_json::to_vec(&json!({"ops":[{"op":"set","subject":s,"class":c}]})).unwrap(),
            ),
            (
                "predicate is rdf:type",
                serde_json::to_vec(&json!({"ops":[{"op":"set","subject":s,"class":c,
                    "predicate":RDF_TYPE,"value":"x"}]}))
                .unwrap(),
            ),
            (
                "set with values",
                serde_json::to_vec(&json!({"ops":[{"op":"set","subject":s,"class":c,
                    "predicate":p,"values":["x"]}]}))
                .unwrap(),
            ),
            (
                "add without values",
                serde_json::to_vec(&json!({"ops":[{"op":"add","subject":s,"class":c,
                    "predicate":p}]}))
                .unwrap(),
            ),
            (
                "add with null value",
                serde_json::to_vec(&json!({"ops":[{"op":"add","subject":s,"class":c,
                    "predicate":p,"values":[null]}]}))
                .unwrap(),
            ),
            (
                "retract with predicate",
                serde_json::to_vec(&json!({"ops":[{"op":"retract","subject":s,"class":c,
                    "predicate":p}]}))
                .unwrap(),
            ),
            (
                "bad term in set",
                serde_json::to_vec(&json!({"ops":[{"op":"set","subject":s,"class":c,
                    "predicate":p,"value":{"@id":"no iri"}}]}))
                .unwrap(),
            ),
        ];
        for (name, payload) in cases {
            let d = decoded(Mode::Assert, &payload, PayloadState::Intact);
            let r = decode_event(&d);
            assert!(r.is_err(), "{name} must be a FoldError, got {r:?}");
            // Display never panics either
            let _ = r.unwrap_err().to_string();
        }
    }

    #[test]
    fn an_op_error_names_its_index() {
        let payload = serde_json::to_vec(&json!({"ops": [
            {"op":"retract","subject":"urn:a","class":"urn:c"},
            {"op":"retract","subject":"urn:a"}
        ]}))
        .unwrap();
        match ops_from_payload(&payload) {
            Err(FoldError::Op { index, .. }) => assert_eq!(index, 1),
            other => panic!("expected an op error, got {other:?}"),
        }
    }

    #[test]
    fn lexical_check_catches_malformed_numbers_booleans_and_dates() {
        assert!(check_term_lexical(&typed("12", "integer")).is_ok());
        assert!(check_term_lexical(&typed("-12", "long")).is_ok());
        assert!(check_term_lexical(&typed("abc", "integer")).is_err());
        assert!(check_term_lexical(&typed("1.5", "integer")).is_err());
        assert!(check_term_lexical(&typed("1.5", "double")).is_ok());
        assert!(check_term_lexical(&typed("x", "double")).is_err());
        assert!(check_term_lexical(&typed("true", "boolean")).is_ok());
        assert!(check_term_lexical(&typed("yes", "boolean")).is_err());
        assert!(check_term_lexical(&typed("2026-11-04T10:00:00Z", "dateTime")).is_ok());
        assert!(check_term_lexical(&typed("2026-11-04T10:00:00", "dateTime")).is_ok());
        assert!(check_term_lexical(&typed("yesterday", "dateTime")).is_err());
        assert!(check_term_lexical(&Term::Lit(Literal::new_simple_literal("anything"))).is_ok());
    }
}

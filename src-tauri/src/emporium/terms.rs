//! Typed RDF term handling for the ingest planner — the deterministic
//! literal-formatting + value-canonical-diff foundation.
//!
//! Port of `app/services/emporium/ingest/terms.py`. Triples are
//! `(subject_uri, predicate_uri, Term)` end-to-end: N-Triples escaping is the
//! library's job (we wrap oxigraph model terms whose `Display` matches
//! pyoxigraph byte-for-byte), and diffing compares *values* via [`canon`], so
//! store round-trip normalization (xsd:long → xsd:integer, dateTime
//! re-serialization) never reads as drift.
//!
//! Placeholders: some object URIs (a code block minted by a doc write, e.g.)
//! do not exist at plan time. They are represented as NamedNodes in the
//! reserved `urn:wf-emit:placeholder:<NAME>` scheme and substituted by the
//! applier after the write that mints them. (The applier is HELD — P3.)

use chrono::{DateTime, SecondsFormat, Utc};
use oxigraph::model::{Literal, NamedNode, Term as OxTerm};
use sha2::{Digest, Sha256};

use crate::emporium::contract::{Datatype, SlugRule};

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

pub(crate) const PLACEHOLDER_NS: &str = "urn:wf-emit:placeholder:";

/// Managed-document provenance namespace (infrastructure; deliberately NOT
/// part of any vocabulary contract). Written by the applier after each doc
/// write; read by the survey to decide rewrite-vs-skip.
#[allow(dead_code)]
pub(crate) const EMPORIUM_NS: &str = "http://mnemosyne.dev/emporium#";

/// T-W Law 4 (retraction is an event, not an erasure) — the cross-vocab
/// retraction predicates [`crate::emporium::write::emporium_retract`] mints
/// direct-on-store on the retracted subject, IN the same named graph the
/// subject already lives in. Deliberately NOT part of any vocab's SHACL shape
/// (retraction is an engine-level annotation any class can carry, not a
/// declared predicate a producer writes) and deliberately vocab-agnostic (one
/// shared convention, not a per-vocab reinvention) — so the SAME `FILTER NOT
/// EXISTS` clause excludes a retracted subject from
/// [`crate::emporium::sweep::current_heads_by_lineage`] /
/// [`crate::emporium::sweep::heads_as_of`] (T2's unified head reader),
/// [`crate::emporium::object_query::run_object_query`]'s criteria match, and
/// the generic object surface's `list_objects` / `assert_subject_is_class`
/// (`emporium/objects.rs`) — "faces hide it, the log keeps it" is ONE
/// predicate, read the same way everywhere.
pub(crate) const RETRACTED_AT_PRED: &str = "http://mnemosyne.dev/emporium#retractedAt";
/// The retraction event's rationale (Law 4: who/when/rationale). Multi-valued
/// in principle (a subject retracted more than once carries one per event).
pub(crate) const RETRACTION_RATIONALE_PRED: &str =
    "http://mnemosyne.dev/emporium#retractionRationale";
/// The retraction event's kind — `"retract"` or `"archive"` (a plain literal,
/// not a closed SHACL enum; this predicate lives outside every vocab shape).
pub(crate) const RETRACTION_KIND_PRED: &str = "http://mnemosyne.dev/emporium#retractionKind";
/// The witness who retracted the subject (an IRI object), when an `observer`
/// was supplied to `emporium_retract`. Omitted for an unwitnessed retraction.
pub(crate) const RETRACTED_BY_PRED: &str = "http://mnemosyne.dev/emporium#retractedBy";
/// Stable identity of the retraction event.  Replaying the same event inserts
/// the same RDF set; a second, intentional retraction uses a different IRI.
pub(crate) const RETRACTION_EVENT_PRED: &str = "http://mnemosyne.dev/emporium#retractionEvent";

/// A typed RDF object term. `Uri`/`Lit` wrap oxigraph model terms so `Display`
/// produces N-Triples escaping byte-identical to pyoxigraph; `Placeholder`
/// is a deferred URI (resolved by the applier, HELD).
#[derive(Debug, Clone)]
pub(crate) enum Term {
    Uri(NamedNode),
    Lit(Literal),
    Placeholder(String),
}

impl Term {
    /// The N-Triples object serialization used inside INSERT/DELETE DATA bodies.
    /// For placeholders this is `<urn:wf-emit:placeholder:NAME>` (substituted
    /// later by the applier).
    pub(crate) fn as_nt(&self) -> String {
        match self {
            Term::Uri(n) => n.to_string(),
            Term::Lit(l) => l.to_string(),
            Term::Placeholder(name) => format!("<{PLACEHOLDER_NS}{name}>"),
        }
    }
}

impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_nt())
    }
}

/// A triple as `(subject_uri, predicate_uri, object_term)`.
pub(crate) type Triple = (String, String, Term);

/// Raised when ingest inputs cannot produce a valid plan. Mirrors the Python
/// `PlanError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanError(pub(crate) String);

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PlanError {}

fn xsd(local: &str) -> NamedNode {
    NamedNode::new(format!("{XSD}{local}")).expect("xsd datatype IRI is valid")
}

/// sha256 hex of UTF-8 text. Mirrors `terms.sha256_text`.
pub(crate) fn sha256_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Canonical form for provenance shas: LF newlines, no trailing whitespace per
/// line, exactly one trailing newline. Mirrors `terms.canonical_md`.
///
/// NOTE: distinct from the `string` datatype's `\r`-strip in [`term_for`] —
/// this normalizes CRLF/CR → LF and trims line/trailing whitespace; that one
/// removes the literal `\r` character only. Conflating them breaks both term
/// parity and provenance shas.
#[allow(dead_code)]
pub(crate) fn canonical_md(md: &str) -> String {
    let unified = md.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    for (i, line) in unified.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    let trimmed = out.trim_end_matches('\n');
    format!("{trimmed}\n")
}

/// Epoch-ms → ISO-8601 UTC, byte-identical to Python
/// `datetime.fromtimestamp(ms/1000, tz=utc).isoformat()`.
///
/// The parity trap: Python's `isoformat()` uses `+00:00` (NOT `Z`), omits the
/// fraction on whole seconds, and emits six-digit microseconds on fractional
/// ms. No chrono `SecondsFormat` preset matches across both cases, so we branch
/// on whether the sub-second component is zero: `Secs` (no fraction) vs `Micros`
/// (six digits), always with `use_z = false` (→ `+00:00`).
pub(crate) fn iso_from_ms(ms: i64) -> String {
    let dt: DateTime<Utc> =
        DateTime::from_timestamp_millis(ms).expect("epoch-ms within representable range");
    let fmt = if dt.timestamp_subsec_nanos() == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    dt.to_rfc3339_opts(fmt, false)
}

/// Apply a slug rule (regex sub → strip → optional lowercase). Mirrors
/// `terms.apply_slug`. The pattern is a fixed contract value, so a compile
/// failure is a contract bug — we surface it as the un-slugged text rather than
/// panicking.
pub(crate) fn apply_slug(rule: &SlugRule, text: &str) -> String {
    let out = match regex::Regex::new(&rule.pattern) {
        Ok(re) => re.replace_all(text, rule.replacement.as_str()).into_owned(),
        Err(_) => text.to_string(),
    };
    let strip_chars: Vec<char> = rule.strip.chars().collect();
    let out = out
        .trim_start_matches(|c| strip_chars.contains(&c))
        .trim_end_matches(|c| strip_chars.contains(&c))
        .to_string();
    if rule.lowercase {
        out.to_lowercase()
    } else {
        out
    }
}

/// A placeholder term (deferred URI resolved by the applier).
#[allow(dead_code)]
pub(crate) fn placeholder(name: &str) -> Term {
    Term::Placeholder(name.to_string())
}

/// Build the typed term for a value per a vocab datatype. Reproduces the exact
/// literal forms from `terms.term_for`:
/// - `uri` → NamedNode
/// - `double` → `format!("{:.6}")` xsd:double (byte-identical to Python `%.6f`)
/// - `integer`/`long` → the integer string, xsd:integer / xsd:long
/// - `dateTime` → epoch-ms numeric → ISO, else pass-through string, xsd:dateTime
/// - `boolean` → `"true"`/`"false"`, xsd:boolean
/// - `string` → simple literal, `\r` characters stripped
#[derive(Debug, Clone)]
pub(crate) enum Value {
    /// A URI object (already an absolute IRI/urn).
    Uri(String),
    /// A placeholder object (deferred URI).
    Placeholder(String),
    /// A floating-point value (for `double`).
    Float(f64),
    /// An integer value (for `integer`/`long`, or epoch-ms for `dateTime`).
    Int(i64),
    /// A boolean value.
    Bool(bool),
    /// A string value (for `string`, or a pre-formatted ISO `dateTime`).
    Str(String),
}

/// Coerce a [`Value`] into a typed [`Term`] per a datatype. The caller is
/// responsible for matching `Value` shape to the declared datatype (the DSL
/// layer, HELD, will own that mapping); this mirrors Python's duck-typed
/// `term_for` for the shapes the mint actually feeds it.
pub(crate) fn term_for(value: &Value, datatype: Datatype) -> Term {
    match datatype {
        Datatype::uri => match value {
            Value::Placeholder(name) => Term::Placeholder(name.clone()),
            Value::Uri(s) | Value::Str(s) => Term::Uri(NamedNode::new(s).unwrap_or_else(|_| {
                NamedNode::new("urn:wf-emit:invalid-uri").expect("fallback IRI is valid")
            })),
            other => Term::Uri(
                NamedNode::new(value_to_plain_string(other))
                    .unwrap_or_else(|_| NamedNode::new("urn:wf-emit:invalid-uri").unwrap()),
            ),
        },
        Datatype::double => {
            let f = match value {
                Value::Float(f) => *f,
                Value::Int(i) => *i as f64,
                Value::Str(s) => s.parse().unwrap_or(0.0),
                _ => 0.0,
            };
            Term::Lit(Literal::new_typed_literal(format!("{f:.6}"), xsd("double")))
        }
        // Mirrors `double` but mints `xsd:float` — the datatype the salience
        // materializer's `push_float_triple` emits. `term_for` is the DSL-planner
        // mint path (not the salience path), so this arm exists for exhaustiveness
        // + future DSL use; the EA-2b salience contract only feeds `vocab_to_shacl`.
        Datatype::float => {
            let f = match value {
                Value::Float(f) => *f,
                Value::Int(i) => *i as f64,
                Value::Str(s) => s.parse().unwrap_or(0.0),
                _ => 0.0,
            };
            Term::Lit(Literal::new_typed_literal(format!("{f:.6}"), xsd("float")))
        }
        Datatype::integer => {
            let i = value_to_i64(value);
            Term::Lit(Literal::new_typed_literal(i.to_string(), xsd("integer")))
        }
        Datatype::long => {
            let i = value_to_i64(value);
            Term::Lit(Literal::new_typed_literal(i.to_string(), xsd("long")))
        }
        Datatype::dateTime => {
            let iso = match value {
                Value::Int(ms) => iso_from_ms(*ms),
                Value::Float(ms) => iso_from_ms(*ms as i64),
                Value::Str(s) => s.clone(),
                other => value_to_plain_string(other),
            };
            Term::Lit(Literal::new_typed_literal(iso, xsd("dateTime")))
        }
        Datatype::boolean => {
            let truthy = match value {
                Value::Bool(b) => *b,
                Value::Str(s) => s == "true",
                Value::Int(i) => *i == 1,
                _ => false,
            };
            Term::Lit(Literal::new_typed_literal(
                if truthy { "true" } else { "false" },
                xsd("boolean"),
            ))
        }
        Datatype::string => {
            let s = value_to_plain_string(value).replace('\r', "");
            Term::Lit(Literal::new_simple_literal(s))
        }
    }
}

fn value_to_i64(value: &Value) -> i64 {
    match value {
        Value::Int(i) => *i,
        Value::Float(f) => *f as i64,
        Value::Bool(b) => {
            if *b {
                1
            } else {
                0
            }
        }
        Value::Str(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn value_to_plain_string(value: &Value) -> String {
    match value {
        Value::Uri(s) | Value::Str(s) | Value::Placeholder(s) => s.clone(),
        Value::Float(f) => format!("{f}"),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
    }
}

/// Value-canonical key for diffing (store round-trips must not read as drift).
/// Mirrors `terms.canon_value`: the five numeric xsd datatypes (integer, long,
/// double, float, decimal) all key as `NUM`
/// rounded to 6dp (so xsd:long ≡ xsd:integer ≡ xsd:float), dateTime keys as epoch-seconds,
/// boolean as a bool, placeholders by name, URIs by value, else the literal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CanonValue {
    Placeholder(String),
    Uri(String),
    /// 6dp-rounded numeric, stored as the bit pattern of the rounded f64 so it
    /// is `Eq`/`Hash`-able and matches Python's `round(x, 6)` equality.
    Num(u64),
    /// dateTime as epoch-seconds (Python uses `timestamp()`, a float), stored
    /// as bits for `Eq`.
    Dt(u64),
    Bool(bool),
    Lit(String),
}

fn num_to_bits(x: f64) -> u64 {
    // round to 6 decimal places, mirroring Python round(x, 6) for diff keys.
    let rounded = (x * 1_000_000.0).round() / 1_000_000.0;
    // normalize -0.0 to 0.0 so the key is canonical.
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    rounded.to_bits()
}

/// Compute the value-canonical key for a term.
pub(crate) fn canon_value(term: &Term) -> CanonValue {
    match term {
        Term::Placeholder(name) => CanonValue::Placeholder(name.clone()),
        Term::Uri(n) => CanonValue::Uri(n.as_str().to_string()),
        Term::Lit(l) => {
            let dt = l.datatype().as_str().to_string();
            let value = l.value();
            if dt == format!("{XSD}integer")
                || dt == format!("{XSD}long")
                || dt == format!("{XSD}double")
                || dt == format!("{XSD}float")
                || dt == format!("{XSD}decimal")
            {
                if let Ok(x) = value.parse::<f64>() {
                    return CanonValue::Num(num_to_bits(x));
                }
            } else if dt == format!("{XSD}dateTime") {
                if let Some(secs) = parse_iso_to_epoch_secs(value) {
                    return CanonValue::Dt(secs.to_bits());
                }
            } else if dt == format!("{XSD}boolean") {
                let b = matches!(value.to_lowercase().as_str(), "true" | "1");
                return CanonValue::Bool(b);
            }
            CanonValue::Lit(value.to_string())
        }
    }
}

fn parse_iso_to_epoch_secs(value: &str) -> Option<f64> {
    let normalized = value.replace('Z', "+00:00");
    DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|dt| dt.timestamp() as f64 + (dt.timestamp_subsec_nanos() as f64) / 1_000_000_000.0)
}

/// Value-canonical key for a whole triple: `(subject, predicate, canon(object))`.
/// Mirrors `terms.canon`.
pub(crate) fn canon(triple: &Triple) -> (String, String, CanonValue) {
    let (s, p, o) = triple;
    (s.clone(), p.clone(), canon_value(o))
}

/// Render chunked INSERT DATA / DELETE DATA bodies from typed triples, NO GRAPH
/// clause (the applier wraps in `GRAPH <{root}:user:rdf>`). Mirrors
/// `terms.render_updates` (size=60).
pub(crate) fn render_updates(verb: &str, triples: &[Triple], size: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < triples.len() {
        let end = (i + size).min(triples.len());
        let body = triples[i..end]
            .iter()
            .map(|(s, p, o)| format!("<{s}> <{p}> {}", o.as_nt()))
            .collect::<Vec<_>>()
            .join(" .\n");
        out.push(format!("{verb} {{\n{body} .\n}}"));
        i = end;
    }
    out
}

// ---------------------------------------------------------------------------
// The canonical triple DIFF — the idempotency core.
// ---------------------------------------------------------------------------

/// The add/remove ops between two triple sets, keyed by [`canon`] so store
/// round-trips never read as drift. `adds = desired − current`,
/// `removes = current − desired`. A converged graph (desired == current by
/// value) yields zero ops.
#[derive(Debug, Default)]
pub(crate) struct TripleDiff {
    pub(crate) adds: Vec<Triple>,
    pub(crate) removes: Vec<Triple>,
}

impl TripleDiff {
    pub(crate) fn is_empty(&self) -> bool {
        self.adds.is_empty() && self.removes.is_empty()
    }

    /// One face of the diff: the store-op count (`removes + adds`). A converged
    /// reconcile returns 0. The reconciles return the structured `TripleDiff`;
    /// this re-projects the historical `usize` op-count at the consumer.
    pub(crate) fn op_count(&self) -> usize {
        self.removes.len() + self.adds.len()
    }
}

/// Diff two triple sets by value-canonical key. `current` is what the graph
/// holds; `desired` is what the mint wants. Returns the INSERT (`adds`) and
/// DELETE (`removes`) op sets. Identical-by-value sets → zero ops.
pub(crate) fn diff_triples(current: &[Triple], desired: &[Triple]) -> TripleDiff {
    use std::collections::BTreeSet;
    let cur_keys: BTreeSet<_> = current.iter().map(canon).collect();
    let des_keys: BTreeSet<_> = desired.iter().map(canon).collect();

    let adds = desired
        .iter()
        .filter(|t| !cur_keys.contains(&canon(t)))
        .cloned()
        .collect();
    let removes = current
        .iter()
        .filter(|t| !des_keys.contains(&canon(t)))
        .cloned()
        .collect();
    TripleDiff { adds, removes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(t: &Term) -> String {
        t.as_nt()
    }

    #[test]
    fn double_format_matches_python_percent_6f() {
        assert_eq!(
            lit(&term_for(&Value::Float(0.5), Datatype::double)),
            "\"0.500000\"^^<http://www.w3.org/2001/XMLSchema#double>"
        );
        assert_eq!(
            lit(&term_for(&Value::Float(1e20), Datatype::double)),
            "\"100000000000000000000.000000\"^^<http://www.w3.org/2001/XMLSchema#double>"
        );
        assert_eq!(
            lit(&term_for(&Value::Float(-1.5), Datatype::double)),
            "\"-1.500000\"^^<http://www.w3.org/2001/XMLSchema#double>"
        );
        assert_eq!(
            lit(&term_for(&Value::Float(123.456789), Datatype::double)),
            "\"123.456789\"^^<http://www.w3.org/2001/XMLSchema#double>"
        );
        // -0.0 matches Python's "-0.000000".
        assert_eq!(
            lit(&term_for(&Value::Float(-0.0), Datatype::double)),
            "\"-0.000000\"^^<http://www.w3.org/2001/XMLSchema#double>"
        );
    }

    #[test]
    fn integer_and_long_render_int_strings() {
        assert_eq!(
            lit(&term_for(&Value::Int(42), Datatype::integer)),
            "\"42\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );
        assert_eq!(
            lit(&term_for(&Value::Int(1750000000000), Datatype::long)),
            "\"1750000000000\"^^<http://www.w3.org/2001/XMLSchema#long>"
        );
    }

    #[test]
    fn iso_from_ms_byte_goldens() {
        // Whole seconds: no fraction, +00:00 (not Z).
        assert_eq!(iso_from_ms(1750000000000), "2025-06-15T15:06:40+00:00");
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00+00:00");
        // Fractional ms: six-digit microseconds.
        assert_eq!(
            iso_from_ms(1749605313305),
            "2025-06-11T01:28:33.305000+00:00"
        );
        assert_eq!(
            iso_from_ms(1749605313001),
            "2025-06-11T01:28:33.001000+00:00"
        );
    }

    #[test]
    fn datetime_term_uses_iso() {
        assert_eq!(
            lit(&term_for(&Value::Int(1750000000000), Datatype::dateTime)),
            "\"2025-06-15T15:06:40+00:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"
        );
    }

    #[test]
    fn boolean_truthy_set() {
        assert_eq!(
            lit(&term_for(&Value::Bool(true), Datatype::boolean)),
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        );
        assert_eq!(
            lit(&term_for(&Value::Bool(false), Datatype::boolean)),
            "\"false\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        );
        assert_eq!(
            lit(&term_for(&Value::Str("true".into()), Datatype::boolean)),
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        );
        assert_eq!(
            lit(&term_for(&Value::Int(1), Datatype::boolean)),
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        );
    }

    #[test]
    fn string_strips_carriage_return_chars_only() {
        // \r CHARACTERS removed, \n preserved. This is distinct from canonical_md
        // (which folds CRLF/CR → LF): "a\r\nb\rc" → "a\nbc" → N-Triples "a\nbc".
        let t = term_for(&Value::Str("a\r\nb\rc".into()), Datatype::string);
        assert_eq!(lit(&t), "\"a\\nbc\"");
    }

    #[test]
    fn string_serializes_as_simple_literal_no_datatype() {
        let t = term_for(&Value::Str("hello".into()), Datatype::string);
        assert_eq!(lit(&t), "\"hello\"");
    }

    #[test]
    fn uri_term_renders_angle_brackets() {
        let t = term_for(&Value::Uri("urn:sophia:wf:demo".into()), Datatype::uri);
        assert_eq!(lit(&t), "<urn:sophia:wf:demo>");
    }

    #[test]
    fn canon_long_equals_integer() {
        let int_term = term_for(&Value::Int(5), Datatype::integer);
        let long_term = term_for(&Value::Int(5), Datatype::long);
        assert_eq!(canon_value(&int_term), canon_value(&long_term));
        // double of the same value also keys equal (NUM, 6dp).
        let dbl_term = term_for(&Value::Float(5.0), Datatype::double);
        assert_eq!(canon_value(&int_term), canon_value(&dbl_term));
    }

    #[test]
    fn canon_datetime_keys_by_epoch_seconds() {
        // Same instant rendered two ways keys equal.
        let from_ms = term_for(&Value::Int(1750000000000), Datatype::dateTime);
        let from_iso = term_for(
            &Value::Str("2025-06-15T15:06:40Z".into()),
            Datatype::dateTime,
        );
        assert_eq!(canon_value(&from_ms), canon_value(&from_iso));
    }

    #[test]
    fn render_updates_chunks_130_triples_into_60_60_10() {
        let triples: Vec<Triple> = (0..130)
            .map(|i| {
                (
                    format!("urn:s:{i}"),
                    "urn:p".to_string(),
                    term_for(&Value::Int(i), Datatype::integer),
                )
            })
            .collect();
        let bodies = render_updates("INSERT DATA", &triples, 60);
        assert_eq!(bodies.len(), 3);
        assert_eq!(bodies[0].matches(" .\n").count(), 60);
        assert_eq!(bodies[2].matches("<urn:s:").count(), 10);
        // No GRAPH clause — the applier wraps.
        assert!(!bodies[0].to_uppercase().contains("GRAPH"));
        assert!(bodies[0].starts_with("INSERT DATA {\n"));
    }

    #[test]
    fn diff_of_identical_sets_is_zero_ops() {
        let triples: Vec<Triple> = vec![
            (
                "urn:s".into(),
                "urn:p".into(),
                term_for(&Value::Int(1), Datatype::integer),
            ),
            (
                "urn:s".into(),
                "urn:q".into(),
                term_for(&Value::Str("x".into()), Datatype::string),
            ),
        ];
        let diff = diff_triples(&triples, &triples);
        assert!(diff.is_empty(), "identical sets must diff to zero ops");
    }

    #[test]
    fn diff_treats_long_and_integer_as_converged() {
        // The store round-trips xsd:long → xsd:integer; the diff must NOT churn.
        let desired: Vec<Triple> = vec![(
            "urn:s".into(),
            "urn:p".into(),
            term_for(&Value::Int(5), Datatype::long),
        )];
        let current: Vec<Triple> = vec![(
            "urn:s".into(),
            "urn:p".into(),
            term_for(&Value::Int(5), Datatype::integer),
        )];
        let diff = diff_triples(&current, &desired);
        assert!(
            diff.is_empty(),
            "xsd:long ≡ xsd:integer must converge to zero ops"
        );
    }

    #[test]
    fn diff_detects_add_and_remove() {
        let current: Vec<Triple> = vec![(
            "urn:s".into(),
            "urn:p".into(),
            term_for(&Value::Str("old".into()), Datatype::string),
        )];
        let desired: Vec<Triple> = vec![(
            "urn:s".into(),
            "urn:p".into(),
            term_for(&Value::Str("new".into()), Datatype::string),
        )];
        let diff = diff_triples(&current, &desired);
        assert_eq!(diff.adds.len(), 1);
        assert_eq!(diff.removes.len(), 1);
    }

    #[test]
    fn canonical_md_normalizes_newlines_and_trailing_ws() {
        // Per-line rstrip only: the leading space on " c" is preserved.
        assert_eq!(canonical_md("a  \r\nb \r c"), "a\nb\n c\n");
        assert_eq!(canonical_md("x\n\n\n"), "x\n");
    }

    #[test]
    fn apply_slug_matches_contract_rule() {
        let rule = SlugRule {
            pattern: "[^A-Za-z0-9_-]+".into(),
            replacement: "-".into(),
            strip: "-".into(),
            lowercase: false,
        };
        assert_eq!(apply_slug(&rule, "Hello World!"), "Hello-World");
        assert_eq!(apply_slug(&rule, "  spaced  "), "spaced");
        let lower = SlugRule {
            lowercase: true,
            ..rule
        };
        assert_eq!(apply_slug(&lower, "Mixed Case"), "mixed-case");
    }
}

// ===========================================================================
// DIFFERENTIAL ORACLE (recon#1) — the Rust evaluator of the model<->runtime
// differential oracle. See prototypes/ontology-to-lean/differential/.
//
// It runs the REAL `diff_triples` / `canon` (and the REAL STRSTARTS-subtree +
// bare-predicate face scope for the L5 axis) over a SHARED corpus, and emits
// each scenario's canon-delta in the shared tagged-JSON format so the proved
// Lean reconcile model can be set-compared against it. NO MOCKS — every value
// flows through the production `parse_term` -> `canon_value`.
// ===========================================================================
#[cfg(test)]
mod diff_oracle {
    use super::*;
    use crate::emporium::survey::parse_term;
    use serde_json::{json, Value as J};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn corpus_dir() -> PathBuf {
        // src-tauri/ is the manifest dir; the corpus lives at the repo root.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../prototypes/ontology-to-lean/differential")
    }

    /// Decode the SHARED tagged CV object into a production `Term`. Prefers the
    /// `rdf` N-Triples field (exercises `parse_term` end-to-end, proving the
    /// datatype-collapse); falls back to constructing from the tag when absent.
    fn term_of_cv(cv: &J) -> Term {
        if let Some(rdf) = cv.get("rdf").and_then(|x| x.as_str()) {
            return parse_term(rdf);
        }
        let k = cv.get("k").and_then(|x| x.as_str()).expect("cv.k");
        match k {
            "uri" => parse_term(&format!("<{}>", cv["v"].as_str().unwrap())),
            "placeholder" => Term::Placeholder(
                cv.get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("p")
                    .to_string(),
            ),
            "lit" => Term::Lit(Literal::new_simple_literal(cv["v"].as_str().unwrap())),
            "num" => Term::Lit(Literal::new_typed_literal(
                cv["v"].as_i64().unwrap().to_string(),
                xsd("integer"),
            )),
            "dt" => Term::Lit(Literal::new_typed_literal(
                iso_from_ms(cv["v"].as_i64().unwrap() * 1000),
                xsd("dateTime"),
            )),
            "bool" => Term::Lit(Literal::new_typed_literal(
                if cv["v"].as_bool().unwrap() {
                    "true"
                } else {
                    "false"
                },
                xsd("boolean"),
            )),
            other => panic!("unknown cv tag {other}"),
        }
    }

    /// Parse a SHARED triple `[s, p, CV]` into a production `(s, p, Term)`.
    fn triple_of(j: &J) -> Triple {
        let arr = j.as_array().expect("triple is an array");
        (
            arr[0].as_str().expect("subject").to_string(),
            arr[1].as_str().expect("predicate").to_string(),
            term_of_cv(&arr[2]),
        )
    }

    fn triples_of(j: &J) -> Vec<Triple> {
        j.as_array()
            .expect("triple list")
            .iter()
            .map(triple_of)
            .collect()
    }

    /// Re-encode the REAL `CanonValue` into the SHARED tagged form. Num/Dt are
    /// f64-BITS in the runtime; we decode back to the corpus-restricted INTEGER
    /// key space (the faithfulness bridge: v1 corpus is integer-valued).
    fn canon_to_json(cv: &CanonValue) -> J {
        match cv {
            CanonValue::Uri(s) => json!({"k": "uri", "v": s}),
            CanonValue::Lit(s) => json!({"k": "lit", "v": s}),
            CanonValue::Bool(b) => json!({"k": "bool", "v": b}),
            CanonValue::Placeholder(_) => json!({"k": "placeholder"}),
            CanonValue::Num(bits) => {
                let f = f64::from_bits(*bits);
                json!({"k": "num", "v": f as i64})
            }
            CanonValue::Dt(bits) => {
                let f = f64::from_bits(*bits);
                json!({"k": "dt", "v": f as i64})
            }
        }
    }

    fn canon_triple_to_json(t: &(String, String, CanonValue)) -> J {
        json!([t.0, t.1, canon_to_json(&t.2)])
    }

    /// The compared artifact: REMOVES and ADDS as canon-triple SETS (dedup'd,
    /// sorted — the comparison gate's normal form).
    fn delta_json(removes: &[Triple], adds: &[Triple]) -> J {
        let r: BTreeSet<_> = removes.iter().map(canon).collect();
        let a: BTreeSet<_> = adds.iter().map(canon).collect();
        json!({
            "removes": r.iter().map(canon_triple_to_json).collect::<Vec<_>>(),
            "adds": a.iter().map(canon_triple_to_json).collect::<Vec<_>>(),
        })
    }

    fn read_corpus(name: &str) -> J {
        let p = corpus_dir().join("corpus").join(name);
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read corpus {p:?}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse corpus {p:?}: {e}"))
    }

    fn write_out(name: &str, value: &J) {
        let dir = corpus_dir().join("out");
        std::fs::create_dir_all(&dir).expect("mkdir out");
        let p = dir.join(name);
        let body = serde_json::to_vec_pretty(value).unwrap();
        crate::storage_file_ops::write_bytes(&p, &body)
            .unwrap_or_else(|e| panic!("write {p:?}: {e}"));
        eprintln!("[diff_oracle] wrote {p:?}");
    }

    /// The plain DIFF axis — REAL `diff_triples` over each scenario's
    /// (current, desired), emitted as canon-deltas.
    #[test]
    fn emit_rust_diff_delta() {
        let corpus = read_corpus("diff.json");
        let mut out = Vec::new();
        for sc in corpus.as_array().expect("scenario array") {
            let name = sc["name"].as_str().unwrap().to_string();
            let current = triples_of(&sc["current"]);
            let desired = triples_of(&sc["desired"]);
            let diff = diff_triples(&current, &desired);
            let delta = delta_json(&diff.removes, &diff.adds);
            out.push(json!({"name": name, "delta": delta}));
        }
        write_out("rust-diff-delta.json", &json!(out));
    }

    /// The L5 face axis — drives the PRODUCTION span-SQL over a SEEDED Oxigraph
    /// [`Store`]. This is the honest close: NO hand-written mirror of the scope
    /// filter. Per scenario:
    ///   1. `Store::new()` — an in-memory store (no EFS/disk).
    ///   2. Seed the scenario's `current` triples into the per-doc projection
    ///      graph via the REAL `run_document_update` INSERT DATA (the same
    ///      GRAPH-wrap + `SparqlEvaluator` path production uses).
    ///   3. For each corpus face, build the production [`OwnedSpan`] from its
    ///      `fragmentSubject` / scenario `subject` / `barePredicates`, then call
    ///      the PRODUCTION `survey_owned_span` — which renders the REAL
    ///      `OwnedSpan::filter_clause` (`STRSTARTS(STR(?s),"{frag}#")` OR
    ///      `(?s=<bare> && ?p in barePredicates)`) and runs it as a SELECT FILTER
    ///      ON THE STORE. The in-scope `current` is therefore the REAL span-SQL
    ///      result over the seeded quads, NOT a `starts_with`/`==` re-impl.
    ///   4. Union the per-face desireds, run the REAL `diff_triples`, emit the
    ///      canon-delta in the shared tagged-JSON form.
    ///
    /// The corpus face IRIs (e.g. `http://mnemosyne.ai/vocab#body`) are NOT the
    /// runtime MNEMO_NS — but that is fine: the SAME IRIs seed the store AND
    /// populate the span's `bare_predicates`, so the SPARQL `?p = <iri>` equality
    /// matches byte-for-byte. The corpus scopes are exercised against the REAL
    /// filter, which is the whole point.
    #[test]
    fn emit_rust_l5_delta() {
        use crate::document_meaningful_object::{
            run_document_update, survey_owned_span, OwnedSpan,
        };
        use oxigraph::store::Store;

        // Per-doc projection graph the seed lands in and the survey reads from.
        const GRAPH_ID: &str = "l5-oracle";
        const DOC_ID: &str = "l5-doc";

        let corpus = read_corpus("l5-face.json");
        let mut out = Vec::new();
        for sc in corpus.as_array().expect("scenario array") {
            let name = sc["name"].as_str().unwrap().to_string();
            let bare = sc["subject"]
                .as_str()
                .expect("scenario subject")
                .to_string();
            let all_current = triples_of(&sc["current"]);

            // 1+2. Seed the scenario's `current` into the per-doc projection graph
            // via the REAL production write path (GRAPH-wrap + SparqlEvaluator).
            let store = Store::new().expect("in-memory oxigraph store");
            for body in render_updates("INSERT DATA", &all_current, 60) {
                run_document_update(&store, GRAPH_ID, DOC_ID, &body)
                    .unwrap_or_else(|e| panic!("seed {name}: {e}"));
            }

            // 3. Per-face: build the PRODUCTION OwnedSpan and survey via the REAL
            // span-SQL (filter_clause -> survey_with_filter on the seeded store).
            let mut current: Vec<Triple> = Vec::new();
            let mut desired: Vec<Triple> = Vec::new();
            for face in sc["faces"].as_array().expect("faces") {
                let fragment_subject = face["fragmentSubject"].as_str().map(|s| s.to_string());
                let bare_predicates: Vec<String> = face["barePredicates"]
                    .as_array()
                    .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default();
                let span = OwnedSpan {
                    fragment_subject,
                    bare_subject: bare.clone(),
                    bare_predicates,
                };
                current.extend(
                    survey_owned_span(&store, GRAPH_ID, DOC_ID, &span)
                        .unwrap_or_else(|e| panic!("survey {name}: {e}")),
                );
                desired.extend(triples_of(&face["desired"]));
            }

            // 4. The REAL value-canonical diff over the REAL-surveyed current.
            let diff = diff_triples(&current, &desired);
            let delta = delta_json(&diff.removes, &diff.adds);
            out.push(json!({"name": name, "delta": delta}));
        }
        write_out("rust-l5-delta.json", &json!(out));
    }

    /// Files scope: real class survey/apply, intersected with exact subjects,
    /// inside a named projection. This exercises the materializer primitive;
    /// it does not certify golden validation, source receipts or crash recovery.
    #[test]
    fn emit_rust_files_delta() {
        use crate::emporium::reconcile::{
            apply_diff, class_diff, survey_class, ClassScope, Placement, SpanKey,
        };
        use oxigraph::store::Store;
        let corpus = read_corpus("files-scope.json");
        let scenarios = corpus.as_array().expect("Files scenario array");
        assert_eq!(scenarios.len(), 4, "Files corpus cardinality changed");
        let mut names = BTreeSet::new();
        let mut out = Vec::new();
        for sc in scenarios {
            let name = sc["name"].as_str().expect("Files scenario name");
            assert!(names.insert(name), "duplicate Files scenario");
            let target = Placement::Named(sc["target"].as_str().unwrap().to_string());
            let other_target = Placement::Named(sc["otherTarget"].as_str().unwrap().to_string());
            assert_ne!(sc["target"], sc["otherTarget"], "foreign graph must differ");
            let current = triples_of(&sc["current"]);
            let desired = triples_of(&sc["desired"]);
            let other_current = triples_of(&sc["otherCurrent"]);
            let store = Store::new().expect("Files in-memory store");
            apply_diff(
                &store,
                &target,
                &TripleDiff {
                    adds: current,
                    removes: vec![],
                },
            )
            .expect("seed Files target");
            apply_diff(
                &store,
                &other_target,
                &TripleDiff {
                    adds: other_current,
                    removes: vec![],
                },
            )
            .expect("seed foreign graph");
            let class = sc["cls"].as_str().unwrap().to_string();
            let scope = ClassScope {
                placement: target,
                key: SpanKey::Fixed {
                    rdf_type: class.clone(),
                },
                graph_id_conjunct: None,
                subjects: Some(
                    [sc["subject"].as_str().unwrap().to_string()]
                        .into_iter()
                        .collect(),
                ),
            };
            let other_scope = ClassScope {
                placement: other_target,
                key: SpanKey::Fixed { rdf_type: class },
                graph_id_conjunct: None,
                subjects: None,
            };
            let sibling_scope = ClassScope {
                placement: scope.placement.clone(),
                key: scope.key.clone(),
                graph_id_conjunct: None,
                subjects: Some(
                    triples_of(&sc["current"])
                        .into_iter()
                        .map(|t| t.0)
                        .filter(|s| s != sc["subject"].as_str().unwrap())
                        .collect(),
                ),
            };
            let siblings_before =
                survey_class(&store, &sibling_scope).expect("survey siblings before");
            let before = survey_class(&store, &other_scope).expect("survey foreign before");
            let delta = class_diff(&store, &scope, &desired).expect("native Files class diff");
            let encoded = delta_json(&delta.removes, &delta.adds);
            apply_diff(&store, &scope.placement, &delta).expect("apply native Files delta");
            let after = survey_class(&store, &other_scope).expect("survey foreign after");
            let siblings_after =
                survey_class(&store, &sibling_scope).expect("survey siblings after");
            assert_eq!(
                delta_json(&[], &siblings_before),
                delta_json(&[], &siblings_after),
                "sibling changed in {name}"
            );
            assert_eq!(
                delta_json(&[], &before),
                delta_json(&[], &after),
                "foreign graph changed in {name}"
            );
            let again = class_diff(&store, &scope, &desired).expect("repeat native Files diff");
            // The wrong-type fixture deliberately retains old off-class facts;
            // adding the Files type then brings them into scope on the next run.
            if name != "files-wrong-type-is-outside-survey" {
                assert!(again.is_empty(), "Files fixed point failed in {name}");
            } else {
                assert!(
                    !again.is_empty(),
                    "wrong-type fixture must expose the closure precondition"
                );
            }
            out.push(json!({"name": name, "delta": encoded}));
        }
        assert_eq!(
            names,
            BTreeSet::from([
                "files-create",
                "files-update-preserves-sibling-and-other-graph",
                "files-converged-zero-ops",
                "files-wrong-type-is-outside-survey",
            ])
        );
        write_out("rust-files-delta.json", &json!(out));
    }

    /// The required-sweep axis — the RUNTIME `check` closure presence test
    /// (asserts.rs:269-288): for each required curie, is it among the
    /// instance's keys? Empty failure-list == complete.
    #[test]
    fn emit_rust_required_delta() {
        let corpus = read_corpus("required-sweep.json");
        let mut out = Vec::new();
        for sc in corpus.as_array().expect("scenario array") {
            let name = sc["name"].as_str().unwrap().to_string();
            let cls = sc["class"].as_str().unwrap();
            let required: Vec<&str> = sc["requiredCuries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap())
                .collect();
            let keys: BTreeSet<&str> = sc["instanceKeys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap())
                .collect();
            // Mirror asserts.rs `check`: push a failure for each missing required.
            let mut failures: Vec<String> = Vec::new();
            for curie in &required {
                if !keys.contains(curie) {
                    failures.push(format!("{cls} missing required {curie}"));
                }
            }
            out.push(json!({
                "name": name,
                "complete": failures.is_empty(),
                "failures": failures,
            }));
        }
        write_out("rust-required-delta.json", &json!(out));
    }
}

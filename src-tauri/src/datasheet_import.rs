//! Datasheets in Sophia, slice 1: the importer.
//!
//! Maps NomDeTom's `autonotes.data/2` sidecar (`<doc>.data.json`, written by
//! `sidecars.py` of `NomDeTom/datasheet-parser` at `ee680a6`) into the
//! **light tier** `datasheet-light/1` of the document-evidence ontology
//! (`plans/2026-10-08-datasheets-in-sophia.md` section 2):
//!
//! * custody nodes (`de:ByteContent`, `de:FileSourceBinding`, renditions,
//!   `de:ExtractionAttempt`, `de:ImportRecord`) and one shared `de:ToolProfile`;
//! * **one node per statement**: a reader's statement is one `de:Assertion`
//!   carrying its Proposition and SpecificationValue fields inline
//!   (review fix #3); conditions are inline values;
//! * **per-slot conditions and guarantees** (`eng:<slot>Conditions`,
//!   `eng:<slot>Guarantee`); absent means *not stated*, never unconditional
//!   (review fix #1);
//! * **typed literals with fixed lexical forms** for small values
//!   (`de:Digest`, `de:RectList`, `de:ProseCondition`, `eng:BitRange`,
//!   `eng:BitPattern`; review fix #2); numbers are `xsd:decimal`;
//! * deterministic IRIs `ds-<sha16>-<json path>`, so a re-import is
//!   idempotent;
//! * one named graph per import, replaced whole through Oxigraph's bulk
//!   loader;
//! * admission checks ported from `verify_library.py` (no page, no value;
//!   min <= max; closed enums), a per-document budget (<= 1,800 nodes,
//!   <= 20,000 triples; the overflow stays in the envelope and is recorded as
//!   `de:diagnostic "on demand: N rows"`), and part strings resolved to
//!   `eng:ComponentFamily` handles.
//!
//! The envelope is the source of truth: the graph can always be rebuilt from
//! it. Nothing here is a typed-tier expansion (`evidence_expand` is slice 10).
//!
//! Tom's vocabularies (source roles, reader tools, the corroboration scale and
//! flags) are imported as the `dsp:` concept scheme in
//! `datasheet_light_1.ttl`, credited to him there.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{GraphName, Literal, NamedNode, NamedOrBlankNode, Quad, Term, TermRef};
use oxigraph::store::Store;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app_error::{AppError, AppResult};
use crate::app_runtime::AppHandle;
use crate::mcp_arg_utils::{mcp_arg_string, mcp_required_graph_id};

// ---------------------------------------------------------------- vocabulary

pub(crate) const DE: &str = "https://sophia-labs.ai/ontology/document-evidence#";
pub(crate) const ENG: &str = "https://sophia-labs.ai/ontology/engineering#";
pub(crate) const EP: &str = "https://sophia-labs.ai/concept/engineering-parameter#";
pub(crate) const EU: &str = "https://sophia-labs.ai/concept/engineering-unit#";
pub(crate) const DSP: &str = "https://sophia-labs.ai/concept/datasheet-parser#";
pub(crate) const PROFILE_SPEC: &str =
    "https://sophia-labs.ai/ontology/profiles/engineering-specification/1";
pub(crate) const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

pub(crate) const UPSTREAM_SCHEMA: &str = "autonotes.data/2";
pub(crate) const MAPPING_PROFILE: &str = "datasheet-light/1";
pub(crate) const MAPPING_VERSION: u32 = 1;
pub(crate) const VOCAB_TTL: &str = include_str!("datasheet_light_1.ttl");

// ------------------------------------------------------------------- budget

pub(crate) const MAX_NODES: usize = 1_800;
pub(crate) const MAX_TRIPLES: usize = 20_000;
const MAX_FAMILIES: usize = 12;
const MAX_PARAMS: usize = 40;
const MAX_READINGS: usize = 5;
const MAX_ROWS: usize = 600;
const MAX_REGISTERS: usize = 100;
const MAX_FIELDS: usize = 1_000;
/// Kept free for the closing diagnostics, so an overflow can always say so.
const RESERVE_NODES: usize = 4;
const RESERVE_TRIPLES: usize = 80;
const MAX_ENVELOPE_BYTES: u64 = 64 * 1024 * 1024;

const LEVELS: [&str; 4] = ["high", "medium", "low", "none"];
const DOC_TYPES: [&str; 6] = [
    "datasheet",
    "user_manual",
    "errata",
    "reference_manual",
    "app_note",
    "paper",
];
const SOURCES: [&str; 4] = ["page1", "roc", "prose", "ti_export"];
const PARAM_KEYS: [&str; 4] = ["vin", "vout", "iout", "fsw"];

pub(crate) fn de(local: &str) -> String {
    format!("{DE}{local}")
}
pub(crate) fn eng(local: &str) -> String {
    format!("{ENG}{local}")
}
pub(crate) fn dsp(local: &str) -> String {
    format!("{DSP}{local}")
}

// ------------------------------------------------------------------ triples

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Obj {
    Iri(String),
    /// A plain (`xsd:string`) literal.
    Text(String),
    /// Lexical form and datatype IRI.
    Typed(String, String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Tr {
    pub(crate) s: String,
    pub(crate) p: String,
    pub(crate) o: Obj,
}

#[derive(Default)]
struct Grp(Vec<Tr>);

impl Grp {
    fn push(&mut self, s: &str, p: String, o: Obj) {
        self.0.push(Tr {
            s: s.to_string(),
            p,
            o,
        });
    }
    fn ty(&mut self, s: &str, class: String) {
        self.push(s, RDF_TYPE.to_string(), Obj::Iri(class));
    }
    fn iri(&mut self, s: &str, p: String, o: &str) {
        self.push(s, p, Obj::Iri(o.to_string()));
    }
    fn text(&mut self, s: &str, p: String, o: &str) {
        self.push(s, p, Obj::Text(o.to_string()));
    }
    fn typed(&mut self, s: &str, p: String, lexical: String, datatype: &str) {
        self.push(s, p, Obj::Typed(lexical, datatype.to_string()));
    }
    fn dec(&mut self, s: &str, p: String, v: f64) {
        self.typed(s, p, decimal_lexical(v), &format!("{XSD}decimal"));
    }
    fn int(&mut self, s: &str, p: String, v: u64) {
        self.typed(s, p, v.to_string(), &format!("{XSD}integer"));
    }
    fn boolean(&mut self, s: &str, p: String, v: bool) {
        self.typed(s, p, v.to_string(), &format!("{XSD}boolean"));
    }
    fn diag(&mut self, s: &str, message: &str) {
        self.text(s, de("diagnostic"), message);
    }
}

/// `xsd:decimal` lexical form: Rust's `Display` for `f64` never uses an
/// exponent, so `1800.0` is `1800`, `1.8` is `1.8`, `1e-7` is `0.0000001`.
fn decimal_lexical(v: f64) -> String {
    format!("{v}")
}

// ---------------------------------------------------------------- lexicals

fn slug(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "_".into()
    } else {
        out
    }
}

/// Normalized identifier: lower-case ASCII alphanumerics only.
pub(crate) fn norm_ident(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `Quiescent current` -> `quiescentCurrent`.
fn camel(s: &str) -> String {
    let mut out = String::new();
    let mut first = true;
    for word in s.split(|c: char| !c.is_ascii_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if first {
            out.push_str(&lower);
            first = false;
        } else {
            let mut chars = lower.chars();
            if let Some(c) = chars.next() {
                out.push(c.to_ascii_uppercase());
                out.push_str(chars.as_str());
            }
        }
        if out.len() >= 60 {
            break;
        }
    }
    if out.is_empty() {
        "unnamed".into()
    } else {
        out
    }
}

/// The `eu:` local name of a printed unit, when the scheme knows it.
fn unit_local(unit: &str) -> Option<&'static str> {
    Some(match unit.trim() {
        "V" => "V",
        "mV" => "mV",
        "\u{b5}V" => "uV",
        "A" => "A",
        "mA" => "mA",
        "\u{b5}A" => "uA",
        "nA" => "nA",
        "pA" => "pA",
        "Hz" => "Hz",
        "kHz" => "kHz",
        "MHz" => "MHz",
        "GHz" => "GHz",
        "\u{3a9}" => "ohm",
        "m\u{3a9}" => "mohm",
        "k\u{3a9}" => "kohm",
        "M\u{3a9}" => "Mohm",
        "\u{b0}C" => "degC",
        "%" => "percent",
        "W" => "W",
        "mW" => "mW",
        "s" => "s",
        "ms" => "ms",
        "\u{b5}s" => "us",
        "ns" => "ns",
        "pF" => "pF",
        "nF" => "nF",
        "\u{b5}F" => "uF",
        "\u{b5}H" => "uH",
        "mH" => "mH",
        "dB" => "dB",
        _ => return None,
    })
}

fn xsd_date(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let ok = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    ok.then(|| s.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn page_of(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return (n >= 1).then_some(n);
    }
    let f = v.as_f64()?;
    (f >= 1.0 && f.fract() == 0.0 && f < 1e9).then_some(f as u64)
}

/// `high:low` with LSB zero, from the printed bit spec (`7`, `5:0`, `[7:5]`,
/// `7-4`). `None` when it cannot be normalized without guessing.
fn bit_range(bits: &str) -> Option<(u32, u32)> {
    let t: String = bits
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '[' && *c != ']')
        .collect();
    let (hi, lo) = match t.split_once(|c| c == ':' || c == '-' || c == '\u{2013}') {
        Some((a, b)) => (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?),
        None => {
            let n = t.parse::<u32>().ok()?;
            (n, n)
        }
    };
    (hi >= lo && hi < 4096).then_some((hi, lo))
}

/// `width:valueBits:knownMask` from a printed reset value, MSB first. Only
/// bits the source states enter the mask; anything else stays lexical.
fn bit_pattern(width: u32, reset: &str) -> Option<String> {
    let r = reset.trim();
    if r.is_empty() || width == 0 || width > 64 {
        return None;
    }
    let w = width as usize;
    let mut lower = r.to_ascii_lowercase();
    // Trailing radix letters as the TI tables print them: `00b`, `8000h`.
    if !lower.starts_with("0x")
        && !lower.starts_with("0b")
        && lower.len() > 1
        && lower.ends_with('b')
        && lower[..lower.len() - 1].chars().all(|c| "01x".contains(c))
    {
        lower = format!("0b{}", &lower[..lower.len() - 1]);
    } else if !lower.starts_with("0x")
        && lower.len() > 1
        && lower.ends_with('h')
        && lower[..lower.len() - 1]
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    {
        lower = format!("0x{}", &lower[..lower.len() - 1]);
    }
    // Binary with possible unknown bits: 0b01x1
    if let Some(bits) = lower.strip_prefix("0b") {
        if bits.is_empty() || bits.len() > w || !bits.chars().all(|c| "01x-?".contains(c)) {
            return None;
        }
        let pad = w - bits.len();
        let mut value = "0".repeat(pad);
        let mut mask = "1".repeat(pad);
        for c in bits.chars() {
            match c {
                '0' | '1' => {
                    value.push(c);
                    mask.push('1');
                }
                _ => {
                    value.push('0');
                    mask.push('0');
                }
            }
        }
        return Some(format!("{width}:{value}:{mask}"));
    }
    // A lone unknown marker.
    if lower == "x" || lower == "-" || lower == "?" {
        return Some(format!("{width}:{}:{}", "0".repeat(w), "0".repeat(w)));
    }
    let n = if let Some(hex) = lower.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()?
    } else {
        lower.parse::<u64>().ok()?
    };
    if width < 64 && n >> width != 0 {
        return None;
    }
    let value = format!("{:0w$b}", n, w = w);
    Some(format!("{width}:{value}:{}", "1".repeat(w)))
}

fn parse_int_lexical(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    match t.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    }
}

fn rect_list(boxes: &[[f64; 4]]) -> String {
    boxes
        .iter()
        .map(|b| {
            b.iter()
                .map(|v| decimal_lexical(*v))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn boxes_of(row: &Value) -> Vec<[f64; 4]> {
    let one = |v: &Value| -> Option<[f64; 4]> {
        let a = v.as_array()?;
        if a.len() != 4 {
            return None;
        }
        let mut out = [0.0; 4];
        for (i, x) in a.iter().enumerate() {
            let f = x.as_f64()?;
            if !f.is_finite() {
                return None;
            }
            out[i] = f;
        }
        Some(out)
    };
    if let Some(list) = row.get("boxes").and_then(Value::as_array) {
        return list.iter().filter_map(one).collect();
    }
    row.get("box").and_then(one).into_iter().collect()
}

// ------------------------------------------------------------------- input

pub(crate) struct PdfCheck {
    pub(crate) artifact_id: Option<String>,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}

pub(crate) struct ImportInput {
    pub(crate) envelope: Value,
    pub(crate) envelope_sha256: String,
    pub(crate) envelope_len: u64,
    pub(crate) envelope_artifact_id: Option<String>,
    pub(crate) pdf: Option<PdfCheck>,
    /// Other outputs of the read job, by role: `text`, `regions`, `docling`,
    /// `audit` -> artifact id.
    pub(crate) renditions: BTreeMap<String, String>,
    /// The `text` rendition's content (the read job's `.text.txt`), already bounded; read for identity only.
    pub(crate) text: Option<String>,
    /// True when `text` was supplied inline in the call, not read from a rendition artifact.
    pub(crate) text_inline: bool,
}

impl ImportInput {
    #[allow(dead_code)]
    pub(crate) fn from_inline(envelope: Value) -> Self {
        let bytes = serde_json::to_vec(&envelope).unwrap_or_default();
        Self {
            envelope_sha256: sha256_hex(&bytes),
            envelope_len: bytes.len() as u64,
            envelope,
            envelope_artifact_id: None,
            pdf: None,
            renditions: BTreeMap::new(),
            text: None,
            text_inline: false,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ImportError {
    /// Admission refused the envelope; every reason is listed.
    Refused(Vec<String>),
    Store(String),
}

impl ImportError {
    pub(crate) fn into_app_error(self) -> AppError {
        match self {
            ImportError::Refused(reasons) => AppError::validation(format!(
                "datasheet import refused ({} reason{}): {}",
                reasons.len(),
                if reasons.len() == 1 { "" } else { "s" },
                reasons.join("; ")
            )),
            ImportError::Store(message) => AppError::storage(message),
        }
    }
}

// --------------------------------------------------------------- admission

fn admit(env: &Value, pdf: Option<&PdfCheck>) -> Vec<String> {
    let mut errs: Vec<String> = Vec::new();
    let Some(root) = env.as_object() else {
        return vec!["the envelope is not a JSON object".into()];
    };
    match root.get("schema").and_then(Value::as_str) {
        Some(UPSTREAM_SCHEMA) => {}
        other => errs.push(format!(
            "wrong schema {:?}: expected {UPSTREAM_SCHEMA}",
            other.unwrap_or("(missing)")
        )),
    }
    let doc = root.get("document").and_then(Value::as_object);
    let mut pages = 0u64;
    match doc {
        None => errs.push("document is missing".into()),
        Some(d) => {
            match d.get("sha256").and_then(Value::as_str) {
                Some(s) if is_hex64(s) => {
                    if let Some(p) = pdf {
                        if p.sha256 != s {
                            errs.push(format!(
                                "document.sha256 {s} does not match the PDF artifact's bytes {}",
                                p.sha256
                            ));
                        }
                    }
                }
                _ => errs.push("document.sha256 is not a lower-case hex SHA-256".into()),
            }
            match page_of(d.get("pages")) {
                Some(n) => pages = n,
                None => errs.push("document.pages is not a positive integer".into()),
            }
            match d.get("doc_type").and_then(Value::as_str) {
                Some(t) if DOC_TYPES.contains(&t) => {}
                other => errs.push(format!(
                    "document.doc_type {:?} is outside the closed set",
                    other.unwrap_or("(missing)")
                )),
            }
        }
    }
    let check_page = |errs: &mut Vec<String>, path: &str, v: Option<&Value>| -> Option<u64> {
        let p = page_of(v)?;
        if pages > 0 && p > pages {
            errs.push(format!(
                "{path}: page {p} is beyond the document's {pages} pages"
            ));
        }
        Some(p)
    };
    let num = |errs: &mut Vec<String>, path: &str, v: Option<&Value>| -> Option<f64> {
        match v {
            None | Some(Value::Null) => None,
            Some(x) => match x.as_f64() {
                Some(f) if f.is_finite() => Some(f),
                _ => {
                    errs.push(format!("{path}: not a finite number"));
                    None
                }
            },
        }
    };
    for (i, p) in root
        .get("params")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let key = p.get("key").and_then(Value::as_str).unwrap_or("");
        let path = format!("params[{i}] ({key})");
        if !PARAM_KEYS.contains(&key) {
            errs.push(format!(
                "{path}: key outside the closed set vin/vout/iout/fsw"
            ));
        }
        match p.get("confidence").and_then(Value::as_str) {
            Some(c) if LEVELS.contains(&c) => {}
            _ => errs.push(format!("{path}: bad confidence")),
        }
        let lo = num(&mut errs, &format!("{path}.min"), p.get("min"));
        let hi = num(&mut errs, &format!("{path}.max"), p.get("max"));
        num(&mut errs, &format!("{path}.typ"), p.get("typ"));
        if let (Some(lo), Some(hi)) = (lo, hi) {
            if lo > hi {
                errs.push(format!("{path}: impossible range {lo} > {hi}"));
            }
        }
        let readings = p.get("readings").and_then(Value::as_array);
        let mut external = false;
        for (j, r) in readings.into_iter().flatten().enumerate() {
            let rpath = format!("{path}.readings[{j}]");
            match r.get("source").and_then(Value::as_str) {
                Some(s) if SOURCES.contains(&s) => {}
                other => errs.push(format!(
                    "{rpath}: source {:?} is outside the closed set",
                    other.unwrap_or("(missing)")
                )),
            }
            let has_file = r
                .get("file")
                .and_then(Value::as_str)
                .is_some_and(|f| !f.is_empty());
            external |= has_file;
            let page = check_page(&mut errs, &rpath, r.get("page"));
            if page.is_none() && !has_file {
                errs.push(format!(
                    "{rpath}: no page and no source file (no page, no value)"
                ));
            }
            num(&mut errs, &format!("{rpath}.min"), r.get("min"));
            num(&mut errs, &format!("{rpath}.max"), r.get("max"));
        }
        let page = check_page(&mut errs, &path, p.get("page"));
        let verified = p.get("verified").is_some_and(|v| !v.is_null());
        if page.is_none() && !verified && !external {
            errs.push(format!("{path}: value without page or source file"));
        }
    }
    for (i, r) in root
        .get("spec_rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let path = format!("spec_rows[{i}]");
        match r.get("confidence").and_then(Value::as_str) {
            Some(c) if LEVELS.contains(&c) => {}
            _ => errs.push(format!("{path}: bad confidence")),
        }
        if check_page(&mut errs, &path, r.get("page")).is_none() {
            errs.push(format!("{path}: value without page"));
        }
        for slot in ["min", "typ", "max"] {
            num(&mut errs, &format!("{path}.{slot}"), r.get(slot));
        }
    }
    for (i, r) in root
        .get("registers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let path = format!("registers[{i}]");
        if r.get("name").and_then(Value::as_str).is_none() {
            errs.push(format!("{path}: name is missing"));
        }
        let rp = r
            .get("page")
            .filter(|v| !v.is_null())
            .or_else(|| r.get("source_page"));
        if rp.is_some_and(|v| !v.is_null()) {
            check_page(&mut errs, &path, rp);
        }
    }
    errs.truncate(60);
    errs
}

// ----------------------------------------------------------------- mapping

#[derive(Debug)]
pub(crate) struct Mapped {
    pub(crate) sha16: String,
    pub(crate) graph_iri: String,
    pub(crate) triples: Vec<Tr>,
    pub(crate) nodes: usize,
    pub(crate) counts: BTreeMap<&'static str, usize>,
    pub(crate) diagnostics: Vec<String>,
}

struct Ctx {
    graph_id: String,
    sha16: String,
    tool: String,
    subject: String,
    edition: String,
    content: String,
    reader_tools: Vec<String>,
}

impl Ctx {
    fn rec(&self, local: &str) -> String {
        format!(
            "{}:record:{}",
            crate::rdf::graph_subject(&self.graph_id),
            local
        )
    }
    fn node(&self, path: &str) -> String {
        self.rec(&format!("ds-{}-{}", self.sha16, slug(path)))
    }
}

/// Collects groups under the per-document cap.
struct Budget {
    nodes: BTreeSet<String>,
    triples: BTreeSet<Tr>,
}

impl Budget {
    fn try_commit(&mut self, group: Grp, reserve: bool) -> bool {
        let (rn, rt) = if reserve {
            (RESERVE_NODES, RESERVE_TRIPLES)
        } else {
            (0, 0)
        };
        let fresh: BTreeSet<Tr> = group
            .0
            .into_iter()
            .filter(|t| !self.triples.contains(t))
            .collect();
        let new_nodes = fresh
            .iter()
            .map(|t| &t.s)
            .filter(|s| !self.nodes.contains(*s))
            .collect::<BTreeSet<_>>()
            .len();
        if self.nodes.len() + new_nodes > MAX_NODES - rn
            || self.triples.len() + fresh.len() > MAX_TRIPLES - rt
        {
            return false;
        }
        for t in fresh {
            self.nodes.insert(t.s.clone());
            self.triples.insert(t);
        }
        true
    }
}

fn s_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

fn f_of(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

fn tool_ident(env: &Value) -> (String, String, String) {
    let tools = env.pointer("/extraction/tools");
    let get = |k: &str| {
        tools
            .and_then(|t| t.get(k))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    };
    let commit = get("datasheet-parser");
    let docling = get("docling");
    (
        commit.clone(),
        docling.clone(),
        format!("tool-dsp-{}-{}", slug(&commit), slug(&docling)),
    )
}

fn reader_concepts(extractor: &str) -> Vec<String> {
    extractor
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| dsp(&slug(s)))
        .collect()
}

fn regime_for(section: &str) -> String {
    let s = section.to_ascii_lowercase();
    let local = if s.contains("recommended operating") {
        "recommendedOperating"
    } else if s.contains("absolute maximum") {
        "absoluteMaximum"
    } else if s.contains("electrical characteristics") {
        "electricalCharacteristic"
    } else {
        "otherRegime"
    };
    eng(local)
}

fn unit_triples(g: &mut Grp, node: &str, unit: &str) {
    let unit = unit.trim();
    if unit.is_empty() {
        return;
    }
    if let Some(local) = unit_local(unit) {
        g.iri(node, eng("unit"), &format!("{EU}{local}"));
    }
    g.text(node, eng("unitLexical"), unit);
}

fn page_triples(g: &mut Grp, ctx: &Ctx, node: &str, page: Option<u64>, boxes: &[[f64; 4]]) {
    let Some(page) = page else { return };
    g.iri(node, de("sourceContent"), &ctx.content);
    g.int(node, de("pageIndex"), page - 1);
    if boxes.is_empty() {
        g.iri(node, de("precision"), &de("page"));
    } else {
        g.typed(node, de("rectangles"), rect_list(boxes), &de("RectList"));
        g.iri(node, de("precision"), &de("cell"));
        g.iri(node, de("boundaryBasis"), &de("parser"));
        // No page frame travels in the sidecar, so the box cannot be called
        // checked: rotation and CropBox are unknown (S07).
        g.iri(node, de("geometryStatus"), &de("approximate"));
    }
}

fn flag_triples(g: &mut Grp, node: &str, v: &Value) {
    for f in v
        .get("flags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(f) = f.as_str() {
            g.diag(node, f);
        }
    }
}

fn map_param(ctx: &Ctx, idx: usize, p: &Value, diags: &mut Vec<String>) -> Grp {
    let key = s_of(p, "key").unwrap_or("param");
    let base = ctx.node(&format!("params-{key}"));
    let predicate = format!(
        "{EP}{}",
        match key {
            "vin" => "inputVoltage",
            "vout" => "outputVoltage",
            "iout" => "outputCurrent",
            _ => "switchingFrequency",
        }
    );
    let unit = s_of(p, "unit").unwrap_or("");
    let mut g = Grp::default();
    g.ty(&base, de("Assertion"));
    g.iri(&base, de("assertionKind"), &de("derivation"));
    g.iri(&base, de("speaker"), &ctx.tool);
    g.iri(&base, de("subject"), &ctx.subject);
    g.iri(&base, de("predicate"), &predicate);
    g.iri(&base, de("semanticProfile"), PROFILE_SPEC);
    g.text(&base, eng("parameterKey"), key);
    g.int(&base, de("ordinal"), idx as u64);
    g.iri(&base, eng("limitRegime"), &eng("recommendedOperating"));
    for (slot, k) in [
        ("lowerLimit", "min"),
        ("typical", "typ"),
        ("upperLimit", "max"),
    ] {
        if let Some(v) = f_of(p, k) {
            g.dec(&base, eng(slot), v);
        }
    }
    unit_triples(&mut g, &base, unit);
    g.iri(&base, de("conditionsCompleteness"), &de("unknown"));
    g.iri(&base, de("applicabilityCompleteness"), &de("unknown"));
    // The cards read one of high/medium/low/contested; Tom's own level is kept.
    let level = s_of(p, "confidence").unwrap_or("none");
    let contested = p.get("contested").and_then(Value::as_bool) == Some(true);
    g.iri(
        &base,
        de("confidence"),
        &dsp(if contested { "contested" } else { level }),
    );
    g.iri(&base, de("upstreamConfidence"), &dsp(level));
    g.int(&base, de("paramOrder"), idx as u64);
    if p.get("contested").and_then(Value::as_bool) == Some(true) {
        g.boolean(&base, de("contested"), true);
    }
    page_triples(&mut g, ctx, &base, page_of(p.get("page")), &[]);
    flag_triples(&mut g, &base, p);
    if let Some(d) = p.get("disagreement").filter(|d| !d.is_null()) {
        g.diag(&base, &format!("disagreement: {d}"));
    }
    let readings = p.get("readings").and_then(Value::as_array);
    let total = readings.map_or(0, Vec::len);
    for (j, r) in readings
        .into_iter()
        .flatten()
        .take(MAX_READINGS)
        .enumerate()
    {
        let node = format!("{base}-r{j}");
        g.iri(&base, de("groundingRecords"), &node);
        g.ty(&node, de("Assertion"));
        g.iri(&node, de("assertionKind"), &de("sourceReport"));
        g.iri(&node, de("speaker"), &ctx.tool);
        g.iri(&node, de("subject"), &ctx.subject);
        g.iri(&node, de("predicate"), &predicate);
        g.int(&node, de("ordinal"), j as u64);
        g.iri(&node, eng("limitRegime"), &eng("recommendedOperating"));
        if let Some(src) = s_of(r, "source") {
            g.iri(&node, de("sourceRole"), &dsp(src));
        }
        if let Some(ex) = s_of(r, "extractor") {
            for c in reader_concepts(ex) {
                g.iri(&node, de("readerTool"), &c);
            }
        }
        if let Some(v) = f_of(r, "min") {
            g.dec(&node, eng("lowerLimit"), v);
        }
        if let Some(v) = f_of(r, "max") {
            g.dec(&node, eng("upperLimit"), v);
        }
        unit_triples(&mut g, &node, unit);
        page_triples(&mut g, ctx, &node, page_of(r.get("page")), &[]);
        if let Some(f) = s_of(r, "file") {
            g.text(&node, de("externalFile"), f);
            if let Some(d) = s_of(r, "date") {
                g.text(&node, de("externalDate"), d);
            }
            if let Some(c) = s_of(r, "category") {
                g.text(&node, de("externalCategory"), c);
            }
        }
        if let (Some(lo), Some(hi)) = (f_of(r, "min"), f_of(r, "max")) {
            if lo > hi {
                g.diag(&node, "reading range inverted (min > max)");
            }
        }
    }
    if total > MAX_READINGS {
        diags.push(format!(
            "on demand: {} readings of params[{key}]",
            total - MAX_READINGS
        ));
    }
    // A person's check, as Tom's snapshot rule: the value they saw is pinned.
    if let Some(v) = p.get("verified").filter(|v| v.is_object()) {
        let node = format!("{base}-verified");
        g.ty(&node, de("Assessment"));
        g.iri(&node, de("assessmentFacet"), &de("sourceFidelity"));
        g.iri(&node, de("assessmentResult"), &de("accept"));
        g.iri(&node, de("assesses"), &base);
        g.iri(&node, de("scope"), &ctx.edition);
        g.boolean(&node, de("imported"), true);
        if let Some(by) = s_of(v, "by") {
            g.text(&node, de("assessorLabel"), by);
        }
        if let Some(on) = s_of(v, "on") {
            match xsd_date(on) {
                Some(d) => {
                    g.typed(&node, de("assessedOn"), d, &format!("{XSD}date"));
                    g.text(&node, de("datePrecision"), "day");
                }
                None => g.text(&node, de("assessedOnLexical"), on),
            }
        }
        if let Some(e) = s_of(v, "evidence") {
            g.text(&node, de("evidenceText"), e);
        }
        if let Some(snap) = v.get("value").and_then(Value::as_array) {
            if let Some(lo) = snap.first().and_then(Value::as_f64) {
                g.dec(&node, de("snapshotLower"), lo);
            }
            if let Some(hi) = snap.get(1).and_then(Value::as_f64) {
                g.dec(&node, de("snapshotUpper"), hi);
            }
        }
        let changed = p.get("flags").and_then(Value::as_array).is_some_and(|f| {
            f.iter()
                .any(|x| x.as_str() == Some("verified_value_changed"))
        });
        if changed {
            g.boolean(&node, de("changedSinceChecked"), true);
            if let Some(ev) = p.get("extracted_value").and_then(Value::as_array) {
                if let Some(lo) = ev.first().and_then(Value::as_f64) {
                    g.dec(&node, de("extractedLower"), lo);
                }
                if let Some(hi) = ev.get(1).and_then(Value::as_f64) {
                    g.dec(&node, de("extractedUpper"), hi);
                }
            }
        }
    }
    g
}

fn map_row(ctx: &Ctx, idx: usize, r: &Value) -> Grp {
    let node = ctx.node(&format!("spec_rows-{idx}"));
    let section = s_of(r, "section").unwrap_or("");
    let parameter = s_of(r, "parameter").unwrap_or("");
    let symbol = s_of(r, "symbol").unwrap_or("");
    let mut g = Grp::default();
    g.ty(&node, de("Assertion"));
    g.iri(&node, de("assertionKind"), &de("sourceReport"));
    g.iri(&node, de("speaker"), &ctx.tool);
    g.iri(&node, de("subject"), &ctx.subject);
    let named = if !parameter.is_empty() {
        parameter
    } else {
        symbol
    };
    g.iri(&node, de("predicate"), &format!("{EP}{}", camel(named)));
    g.iri(&node, de("semanticProfile"), PROFILE_SPEC);
    g.int(&node, de("ordinal"), idx as u64);
    g.text(&node, eng("section"), section);
    if let Some(grp) = s_of(r, "group") {
        g.text(&node, eng("group"), grp);
    }
    if !symbol.is_empty() {
        g.text(&node, eng("symbolLexical"), symbol);
    }
    if !parameter.is_empty() {
        g.text(&node, eng("parameterLexical"), parameter);
    }
    g.iri(&node, eng("limitRegime"), &regime_for(section));
    match s_of(r, "conditions") {
        Some(c) => {
            g.typed(
                &node,
                de("conditions"),
                c.to_string(),
                &de("ProseCondition"),
            );
            g.iri(&node, de("conditionsCompleteness"), &de("proseOnly"));
        }
        None => g.iri(&node, de("conditionsCompleteness"), &de("unknown")),
    }
    let slots = [
        ("min", "lower", "lowerLimit"),
        ("typ", "typical", "typical"),
        ("max", "upper", "upperLimit"),
    ];
    for (k, slot, pred) in slots {
        if let Some(v) = f_of(r, k) {
            g.dec(&node, eng(pred), v);
        }
        // Per-slot conditions and guarantees: absent means not stated.
        if let Some(c) = r
            .pointer(&format!("/slot_conditions/{k}"))
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
        {
            g.typed(
                &node,
                eng(&format!("{slot}Conditions")),
                c.to_string(),
                &de("ProseCondition"),
            );
        }
        if let Some(c) = r
            .pointer(&format!("/slot_guarantee/{k}"))
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
        {
            g.text(&node, eng(&format!("{slot}Guarantee")), c);
        }
    }
    if let Some(raw) = r.get("raw").and_then(Value::as_object) {
        for (k, v) in raw {
            if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
                g.text(&node, eng(&format!("raw{}", camel_cap(k))), s);
            }
        }
    }
    unit_triples(&mut g, &node, s_of(r, "unit").unwrap_or(""));
    page_triples(&mut g, ctx, &node, page_of(r.get("page")), &boxes_of(r));
    g.iri(
        &node,
        de("confidence"),
        &dsp(s_of(r, "confidence").unwrap_or("none")),
    );
    if let Some(ex) = s_of(r, "extractor") {
        for c in reader_concepts(ex) {
            g.iri(&node, de("readerTool"), &c);
        }
    }
    g.iri(&node, de("sourceRole"), &dsp("specTable"));
    flag_triples(&mut g, &node, r);
    if let (Some(lo), Some(hi)) = (f_of(r, "min"), f_of(r, "max")) {
        if lo > hi {
            g.diag(&node, "row range inverted (min > max)");
        }
    }
    g
}

fn camel_cap(s: &str) -> String {
    let c = camel(s);
    let mut chars = c.chars();
    match chars.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + chars.as_str(),
        None => c,
    }
}

/// One key value derived from a spec row (datasheets slice 1b): the same
/// derivation shape as a Tom key parameter, grounded in the row it came from.
fn map_key_value(
    ctx: &Ctx,
    order: usize,
    key: &str,
    row_idx: Option<usize>,
    row: Option<&Value>,
    text: Option<&str>,
) -> Grp {
    let node = ctx.node(&format!("key-{key}"));
    let mut g = Grp::default();
    g.ty(&node, de("Assertion"));
    g.iri(&node, de("assertionKind"), &de("derivation"));
    g.iri(&node, de("speaker"), &ctx.tool);
    g.iri(&node, de("subject"), &ctx.subject);
    g.iri(&node, de("predicate"), &format!("{EP}{key}"));
    g.iri(&node, de("semanticProfile"), PROFILE_SPEC);
    g.text(&node, eng("parameterKey"), key);
    g.int(&node, de("ordinal"), order as u64);
    g.int(&node, de("paramOrder"), order as u64);
    if let (Some(ri), Some(r)) = (row_idx, row) {
        let section = s_of(r, "section").unwrap_or("");
        g.iri(&node, eng("limitRegime"), &regime_for(section));
        for (slot, k) in [
            ("lowerLimit", "min"),
            ("typical", "typ"),
            ("upperLimit", "max"),
        ] {
            if let Some(v) = f_of(r, k) {
                g.dec(&node, eng(slot), v);
            }
        }
        unit_triples(&mut g, &node, s_of(r, "unit").unwrap_or(""));
        match s_of(r, "conditions") {
            Some(c) => {
                g.typed(
                    &node,
                    de("conditions"),
                    c.to_string(),
                    &de("ProseCondition"),
                );
                g.iri(&node, de("conditionsCompleteness"), &de("proseOnly"));
            }
            None => g.iri(&node, de("conditionsCompleteness"), &de("unknown")),
        }
        page_triples(&mut g, ctx, &node, page_of(r.get("page")), &boxes_of(r));
        g.iri(
            &node,
            de("groundingRecords"),
            &ctx.node(&format!("spec_rows-{ri}")),
        );
        g.iri(
            &node,
            de("confidence"),
            &dsp(s_of(r, "confidence").unwrap_or("none")),
        );
        g.diag(
            &node,
            &format!(
                "derived from spec_rows[{ri}] ({} / {})",
                s_of(r, "symbol").unwrap_or(""),
                s_of(r, "parameter").unwrap_or("")
            ),
        );
    } else if let Some(t) = text {
        g.text(&node, de("valueText"), t);
        page_triples(&mut g, ctx, &node, Some(1), &[]);
        g.iri(&node, de("confidence"), &dsp("low"));
        g.diag(
            &node,
            "read from the document title or part name, not from a table",
        );
    }
    g
}

fn map_register(ctx: &Ctx, idx: usize, r: &Value, reg_reader: Option<&str>) -> (Grp, usize) {
    let node = ctx.node(&format!("registers-{idx}"));
    let mut g = Grp::default();
    g.ty(&node, eng("RegisterDefinition"));
    g.iri(&node, de("speaker"), &ctx.tool);
    if let Some(rd) = reg_reader {
        g.iri(&node, de("readerTool"), &dsp(&slug(rd)));
    }
    g.int(&node, de("ordinal"), idx as u64);
    g.text(&node, eng("name"), s_of(r, "name").unwrap_or(""));
    if let Some(a) = s_of(r, "address") {
        g.text(&node, eng("addressLexical"), a);
        if let Some(n) = parse_int_lexical(a) {
            g.int(&node, eng("address"), n);
        }
    }
    if let Some(a) = s_of(r, "reset") {
        // Register width is not stated by the sidecar: the reset stays lexical.
        g.text(&node, eng("resetLexical"), a);
    }
    if let Some(d) = s_of(r, "description") {
        g.text(&node, eng("description"), d);
    }
    // Docling-read registers say `page`; the pdfplumber fallback says `source_page`.
    let rpage = page_of(r.get("page")).or_else(|| page_of(r.get("source_page")));
    page_triples(&mut g, ctx, &node, rpage, &[]);
    let fields = r.get("fields").and_then(Value::as_array);
    for (j, f) in fields.into_iter().flatten().enumerate() {
        let fnode = ctx.node(&format!("registers-{idx}-fields-{j}"));
        g.ty(&fnode, eng("BitFieldDefinition"));
        g.iri(&fnode, eng("register"), &node);
        g.int(&fnode, de("ordinal"), j as u64);
        g.text(&fnode, eng("name"), s_of(f, "name").unwrap_or(""));
        let bits = s_of(f, "bits").unwrap_or("");
        g.text(&fnode, eng("bitsLexical"), bits);
        let range = bit_range(bits);
        match range {
            Some((hi, lo)) => g.typed(&fnode, eng("bitRange"), format!("{hi}:{lo}"), &eng("BitRange")),
            None => g.diag(
                &fnode,
                &format!("bits not normalizable without a register width or numbering convention: {bits:?}"),
            ),
        }
        if let Some(acc) = s_of(f, "access") {
            g.text(&fnode, eng("accessLexical"), acc);
        }
        if let Some(reset) = s_of(f, "reset") {
            g.text(&fnode, eng("resetLexical"), reset);
            if let Some((hi, lo)) = range {
                if let Some(pat) = bit_pattern(hi - lo + 1, reset) {
                    g.typed(&fnode, eng("resetPattern"), pat, &eng("BitPattern"));
                }
            }
        }
        if let Some(d) = s_of(f, "description") {
            g.text(&fnode, eng("description"), d);
        }
    }
    let n = fields.map_or(0, Vec::len);
    (g, n)
}

fn count(counts: &mut BTreeMap<&'static str, usize>, k: &'static str, by: usize) {
    *counts.entry(k).or_insert(0) += by;
}

/// Map an admitted envelope to triples. `resolve_family` returns a handle the
/// graph already has for a normalized identifier.
pub(crate) fn map_envelope(
    graph_id: &str,
    input: &ImportInput,
    resolve_family: &dyn Fn(&str) -> Option<String>,
) -> Result<Mapped, ImportError> {
    let env = &input.envelope;
    let errs = admit(env, input.pdf.as_ref());
    if !errs.is_empty() {
        return Err(ImportError::Refused(errs));
    }
    let doc = &env["document"];
    let sha = doc["sha256"].as_str().unwrap_or_default().to_string();
    let sha16 = sha[..16].to_string();
    let pages = page_of(doc.get("pages")).unwrap_or(1);
    let gs = crate::rdf::graph_subject(graph_id);
    let graph_iri = format!("{gs}:projection:datasheet:ds-{sha16}:m{MAPPING_VERSION}");
    let (commit, docling_version, tool_local) = tool_ident(env);

    let mut ctx = Ctx {
        graph_id: graph_id.to_string(),
        sha16: sha16.clone(),
        tool: String::new(),
        subject: String::new(),
        edition: String::new(),
        content: String::new(),
        reader_tools: Vec::new(),
    };
    ctx.tool = ctx.rec(&tool_local);
    ctx.content = ctx.rec(&format!("ds-{sha16}"));
    ctx.edition = ctx.node("document");
    let mut budget = Budget {
        nodes: BTreeSet::new(),
        triples: BTreeSet::new(),
    };
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut diags: Vec<String> = Vec::new();

    // ---- custody
    let mut g = Grp::default();
    let content = ctx.content.clone();
    g.ty(&content, de("ByteContent"));
    g.text(&content, de("declaredSha256"), &sha);
    g.int(&content, de("pageCount"), pages);
    match &input.pdf {
        Some(p) => {
            g.typed(
                &content,
                de("digest"),
                format!("sha256:{}:{}", p.sha256, p.bytes),
                &de("Digest"),
            );
            g.iri(&content, de("byteCheck"), &de("matched"));
        }
        None => g.iri(&content, de("byteCheck"), &de("unchecked")),
    }
    let binding = ctx.node("binding");
    g.ty(&binding, de("FileSourceBinding"));
    g.iri(&binding, de("binds"), &content);
    if let Some(a) = input.pdf.as_ref().and_then(|p| p.artifact_id.as_deref()) {
        g.text(&binding, de("artifact"), a);
    }
    let mut renditions = vec![("dataJson", input.envelope_artifact_id.clone())];
    for role in ["text", "regions", "docling", "audit"] {
        if let Some(a) = input.renditions.get(role) {
            renditions.push((role, Some(a.clone())));
        }
    }
    if input.text_inline && !renditions.iter().any(|(r, _)| *r == "text") {
        renditions.push(("text", None));
    }
    let data_rendition = ctx.node("rendition-dataJson");
    for (role, artifact) in &renditions {
        let n = ctx.node(&format!("rendition-{role}"));
        g.ty(&n, de("Rendition"));
        g.iri(&n, de("renditionRole"), &dsp(role));
        g.iri(&n, de("renditionOf"), &content);
        if let Some(a) = artifact {
            g.text(&n, de("artifact"), a);
        }
        if *role == "text" {
            if let Some(t) = &input.text {
                // What identity was read from: the bounded text, by digest. Inline text has no artifact.
                g.typed(
                    &n,
                    de("digest"),
                    format!("sha256:{}:{}", sha256_hex(t.as_bytes()), t.len()),
                    &de("Digest"),
                );
                if input.text_inline {
                    g.boolean(&n, de("suppliedInline"), true);
                }
            }
        }
        if *role == "dataJson" {
            g.typed(
                &n,
                de("digest"),
                format!("sha256:{}:{}", input.envelope_sha256, input.envelope_len),
                &de("Digest"),
            );
        }
    }
    let attempt = ctx.node("attempt");
    g.ty(&attempt, de("ExtractionAttempt"));
    g.iri(&attempt, de("tool"), &ctx.tool);
    g.iri(&attempt, de("input"), &content);
    if let Some(gen) = s_of(&env["extraction"], "generated") {
        match xsd_date(gen) {
            Some(d) => g.typed(&attempt, de("generated"), d, &format!("{XSD}date")),
            None => g.text(&attempt, de("generatedLexical"), gen),
        }
    }
    if let Some(readers) = env.pointer("/extraction/readers").filter(|r| r.is_object()) {
        g.text(&attempt, de("readers"), &readers.to_string());
        for k in ["tables", "device_info", "prose", "registers"] {
            if let Some(r) = readers.get(k).and_then(Value::as_str) {
                let c = dsp(&slug(r));
                if !ctx.reader_tools.contains(&c) {
                    ctx.reader_tools.push(c);
                }
            }
        }
    }
    for (k, pred) in [
        ("docling_rows", "doclingRows"),
        ("pdfplumber_sections", "pdfplumberSections"),
        ("unit_repairs", "unitRepairs"),
    ] {
        if let Some(n) = env
            .pointer(&format!("/extraction/{k}"))
            .and_then(Value::as_u64)
        {
            g.int(&attempt, de(pred), n);
        }
    }
    if let Some(p) = env
        .pointer("/extraction/text_disagreement_pages")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        g.diag(
            &attempt,
            &format!("text readers disagree on pages {}", Value::Array(p.clone())),
        );
        g.text(
            &attempt,
            de("textDisagreementPages"),
            &Value::Array(p.clone()).to_string(),
        );
    }
    // Every other tool the run names (Docling and its version) is a ToolProfile of its own,
    // used by this attempt; `de:tool` stays the parser itself.
    if let Some(tools) = env.pointer("/extraction/tools").and_then(Value::as_object) {
        for (name, ver) in tools {
            let ver = ver.as_str().unwrap_or("unknown");
            if name == "datasheet-parser" {
                continue;
            }
            let t = ctx.rec(&format!("tool-{}-{}", slug(name), slug(ver)));
            g.ty(&t, de("ToolProfile"));
            g.text(&t, de("toolName"), name);
            g.text(&t, de("toolVersion"), ver);
            g.iri(&attempt, de("usedTool"), &t);
        }
    }
    let import = ctx.node("import");
    g.ty(&import, de("ImportRecord"));
    g.text(&import, de("upstreamSchema"), UPSTREAM_SCHEMA);
    g.text(&import, de("mappingProfile"), MAPPING_PROFILE);
    g.int(&import, de("mappingVersion"), MAPPING_VERSION as u64);
    g.iri(&import, de("importedFrom"), &data_rendition);
    g.iri(&import, de("attempt"), &attempt);
    g.iri(&import, de("edition"), &ctx.edition);
    if let Some(a) = &input.envelope_artifact_id {
        g.text(&import, de("artifact"), a);
    }
    let tool = ctx.tool.clone();
    g.ty(&tool, de("ToolProfile"));
    g.text(&tool, de("toolName"), "datasheet-parser");
    g.text(&tool, de("toolCommit"), &commit);
    g.text(&tool, de("doclingVersion"), &docling_version);
    g.text(
        &tool,
        de("sourceRepository"),
        "https://github.com/NomDeTom/datasheet-parser",
    );
    g.text(&tool, de("credit"), "NomDeTom");
    let custody =
        g.0.iter()
            .map(|t| t.s.clone())
            .collect::<BTreeSet<_>>()
            .len();
    count(&mut counts, "custodyNodes", custody);
    budget.try_commit(g, false);

    // ---- identity: several witnesses (the parser's guess, the title, the document's own first
    // pages, the file name) reconciled into one maker and one list of parts that says how it
    // knows. Disagreements are diagnostics. See datasheet_identity.rs.
    let raw_parts: Vec<String> = doc
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let file_names: Vec<String> = ["original_name", "file"]
        .iter()
        .filter_map(|k| s_of(doc, k))
        .map(str::to_string)
        .collect();
    let ident = crate::datasheet_identity::reconcile(&crate::datasheet_identity::Evidence {
        parser_parts: &raw_parts,
        parser_maker: s_of(doc, "manufacturer"),
        parser_maker_by: s_of(doc, "manufacturer_by"),
        title: s_of(doc, "title"),
        file_names: &file_names,
        text: input.text.as_deref(),
        use_filename: true,
    });
    let mut parts: Vec<String> = ident.parts.clone();
    diags.extend(ident.diagnostics.iter().cloned());
    if input.renditions.contains_key("text") && input.text.is_none() {
        diags.push(
            "the text rendition could not be read; identity is from the other witnesses".into(),
        );
    }
    if parts.len() > MAX_FAMILIES {
        diags.push(format!(
            "on demand: {} parts beyond {MAX_FAMILIES}",
            parts.len() - MAX_FAMILIES
        ));
        parts.truncate(MAX_FAMILIES);
    }
    let mut families: Vec<String> = Vec::new();
    let mut g = Grp::default();
    for part in &parts {
        let norm = norm_ident(part);
        if norm.is_empty() {
            continue;
        }
        let handle = resolve_family(&norm).unwrap_or_else(|| ctx.rec(&format!("family-{norm}")));
        if families.contains(&handle) {
            continue;
        }
        g.ty(&handle, eng("ComponentFamily"));
        g.text(&handle, eng("identifier"), part);
        g.text(&handle, eng("normalizedIdentifier"), &norm);
        g.iri(&ctx.edition, de("namesFamily"), &handle);
        families.push(handle);
    }
    ctx.subject = families
        .first()
        .cloned()
        .unwrap_or_else(|| ctx.edition.clone());
    if families.is_empty() {
        diags.push(
            "no part resolved to a component family; statements are about the edition".into(),
        );
    }
    count(&mut counts, "families", families.len());
    let ed = ctx.edition.clone();
    g.ty(&ed, de("DocumentEdition"));
    g.iri(&ed, de("sourceContent"), &content);
    g.iri(&ed, de("importRecord"), &import);
    if let Some(a) = input.pdf.as_ref().and_then(|p| p.artifact_id.as_deref()) {
        g.text(&ed, de("artifact"), a);
    }
    if let Some(first) = parts.first() {
        g.text(&ed, de("partNumber"), first);
    }
    {
        let rows_json = env.get("spec_rows").and_then(Value::as_array);
        let params_json = env.get("params").and_then(Value::as_array);
        let nrows = rows_json.map_or(0, Vec::len);
        let nparams_json = params_json.map_or(0, Vec::len);
        let nregs_json = env
            .get("registers")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        // To check: key values that are contested or weakly read and not cleanly
        // checked by a person, plus weakly read rows.
        let to_check = params_json
            .into_iter()
            .flatten()
            .filter(|p| {
                let weak = matches!(s_of(p, "confidence"), Some("low") | Some("none"))
                    || p.get("contested").and_then(Value::as_bool) == Some(true);
                let checked = p.get("verified").is_some_and(Value::is_object)
                    && !p.get("flags").and_then(Value::as_array).is_some_and(|f| {
                        f.iter()
                            .any(|x| x.as_str() == Some("verified_value_changed"))
                    });
                weak && !checked
            })
            .count()
            + rows_json
                .into_iter()
                .flatten()
                .filter(|r| matches!(s_of(r, "confidence"), Some("low")))
                .count();
        g.int(&ed, de("rowCount"), nrows as u64);
        g.int(&ed, de("toCheckCount"), to_check as u64);
        let has = |f: &str| {
            env.get("flags")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(f)))
        };
        g.boolean(&ed, de("scan"), has("text_from_ocr"));
        g.boolean(&ed, de("textOnly"), nrows + nparams_json + nregs_json == 0);
    }
    for (k, pred) in [
        ("title", "title"),
        ("revision", "revision"),
        ("doc_type", "documentType"),
        ("product_type", "productType"),
        ("classified_by", "classifiedBy"),
        ("topology_class", "topology"),
        ("topology_by", "topologyBy"),
        ("category", "category"),
        ("file", "fileName"),
        ("original_name", "alias"),
    ] {
        if let Some(v) = s_of(doc, k) {
            g.text(&ed, de(pred), v);
        }
    }
    // The reconciled identity; the parser's own guess stays in `identityWitnesses`.
    if let Some(m) = &ident.maker {
        g.text(&ed, de("maker"), m);
    }
    if let Some(b) = &ident.maker_by {
        g.text(&ed, de("makerBy"), b);
    }
    if !ident.parts_by.is_empty() {
        g.text(&ed, de("partsBy"), &ident.parts_by);
    }
    if let Some(l) = &ident.literature {
        g.text(&ed, de("literature"), l);
        g.text(&ed, de("identifier"), &format!("literature:{l}"));
    }
    g.text(&ed, de("identityWitnesses"), &ident.witnesses_json);
    for d in &ident.diagnostics {
        g.diag(&ed, d);
    }
    g.int(&ed, de("pageCount"), pages);
    if let Some(ids) = doc.get("ids").and_then(Value::as_object) {
        for (k, v) in ids {
            if let Some(v) = v.as_str().filter(|v| !v.is_empty()) {
                g.text(&ed, de("identifier"), &format!("{k}:{v}"));
            }
        }
    }
    if let Some(paper) = doc.get("paper").filter(|p| p.is_object()) {
        g.text(&ed, de("paperMetadata"), &paper.to_string());
    }
    for f in env
        .get("flags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(f) = f.as_str() {
            g.diag(&ed, f);
        }
    }
    for p in doc
        .get("correction_problems")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(p) = p.as_str() {
            g.diag(&ed, &format!("fix rejected: {p}"));
        }
    }
    budget.try_commit(g, false);

    // ---- key parameters (fused value plus readings)
    let params = env.get("params").and_then(Value::as_array);
    let nparams = params.map_or(0, Vec::len);
    let mut kept_params = 0;
    for (i, p) in params.into_iter().flatten().take(MAX_PARAMS).enumerate() {
        let g = map_param(&ctx, i, p, &mut diags);
        let readings = g.0.iter().filter(|t| t.p == de("groundingRecords")).count();
        let checks = g.0.iter().filter(|t| t.p == de("assessmentFacet")).count();
        if budget.try_commit(g, true) {
            kept_params += 1;
            count(&mut counts, "readings", readings);
            count(&mut counts, "checks", checks);
        } else {
            diags.push(format!(
                "on demand: params[{}]",
                s_of(p, "key").unwrap_or("?")
            ));
        }
    }
    if nparams > MAX_PARAMS {
        diags.push(format!("on demand: {} params", nparams - MAX_PARAMS));
    }
    count(&mut counts, "params", kept_params);

    // ---- registers and bit fields
    let regs = env.get("registers").and_then(Value::as_array);
    let nregs = regs.map_or(0, Vec::len);
    let reg_reader = env
        .pointer("/extraction/readers/registers")
        .and_then(Value::as_str);
    let (mut kept_regs, mut kept_fields, mut skipped_fields) = (0usize, 0usize, 0usize);
    for (i, r) in regs.into_iter().flatten().take(MAX_REGISTERS).enumerate() {
        let (g, nfields) = map_register(&ctx, i, r, reg_reader);
        if kept_fields + nfields > MAX_FIELDS || !budget.try_commit(g, true) {
            skipped_fields += nfields;
            continue;
        }
        kept_regs += 1;
        kept_fields += nfields;
    }
    if nregs > kept_regs {
        diags.push(format!("on demand: {} registers", nregs - kept_regs));
    }
    if skipped_fields > 0 {
        diags.push(format!("on demand: {skipped_fields} register fields"));
    }
    count(&mut counts, "registers", kept_regs);
    count(&mut counts, "registerFields", kept_fields);

    // ---- imported corrections (the card's `fix:` list, as stored in the sidecar)
    if let Some(fix) = doc.get("corrections").and_then(Value::as_object) {
        let mut g = Grp::default();
        for (field, value) in fix {
            let node = ctx.node(&format!("document-fix-{field}"));
            g.ty(&node, de("Assertion"));
            g.iri(&node, de("assertionKind"), &de("correction"));
            g.text(&node, de("speakerLabel"), "card (a person's fix)");
            g.iri(&node, de("subject"), &ctx.edition);
            g.iri(
                &node,
                de("predicate"),
                &dsp(&format!("field-{}", slug(field))),
            );
            let lexical = match value {
                Value::String(s) => s.clone(),
                Value::Array(a) => a
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| x.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            g.text(&node, de("value"), &lexical);
            g.iri(&node, de("corrects"), &ctx.edition);
            g.boolean(&node, de("imported"), true);
        }
        let n = fix.len();
        if budget.try_commit(g, true) {
            count(&mut counts, "corrections", n);
        }
    }

    // ---- specification rows (primary reader only)
    let rows = env.get("spec_rows").and_then(Value::as_array);
    let all_rows: Vec<(usize, &Value)> = rows.into_iter().flatten().enumerate().collect();
    let has_docling = all_rows
        .iter()
        .any(|(_, r)| s_of(r, "extractor").is_some_and(|e| e.starts_with("docling")));
    let (primary, secondary): (Vec<_>, Vec<_>) = all_rows.into_iter().partition(|(_, r)| {
        !has_docling || s_of(r, "extractor").is_some_and(|e| e.starts_with("docling"))
    });
    if !secondary.is_empty() {
        diags.push(format!(
            "on demand: {} alternative-reader rows",
            secondary.len()
        ));
    }
    // ---- key values for every part type, derived from the rows (slice 1b).
    // The rows they cite are committed first, so no key value dangles.
    let mut committed_rows: BTreeSet<usize> = BTreeSet::new();
    let mut key_values = 0usize;
    let mut key_kind: Option<(crate::datasheet_keyvalues::Kind, bool)> = None;
    if let Some((kind, inferred)) = crate::datasheet_keyvalues::classify(doc) {
        key_kind = Some((kind, inferred));
        let have: Vec<&str> = params
            .into_iter()
            .flatten()
            .filter_map(|p| match s_of(p, "key") {
                Some("vin") => Some("inputVoltage"),
                Some("iout") => Some("outputCurrent"),
                _ => None,
            })
            .collect();
        let candidates: Vec<(usize, &Value)> = primary.iter().take(MAX_ROWS).cloned().collect();
        for pick in crate::datasheet_keyvalues::select(kind, doc, &candidates, &have) {
            let order = nparams + key_values;
            let row = pick
                .row
                .and_then(|ri| candidates.iter().find(|(i, _)| *i == ri).map(|(_, r)| *r));
            if let (Some(ri), Some(r)) = (pick.row, row) {
                if !committed_rows.contains(&ri) {
                    if !budget.try_commit(map_row(&ctx, ri, r), true) {
                        diags.push(format!("on demand: key value {}", pick.key));
                        continue;
                    }
                    committed_rows.insert(ri);
                }
            }
            if budget.try_commit(
                map_key_value(&ctx, order, pick.key, pick.row, row, pick.text.as_deref()),
                true,
            ) {
                key_values += 1;
            } else {
                diags.push(format!("on demand: key value {}", pick.key));
            }
        }
    }
    count(&mut counts, "keyValues", key_values);
    let mut kept_rows = committed_rows.len();
    let mut dropped_rows = 0;
    for (n, (idx, r)) in primary.iter().enumerate() {
        if committed_rows.contains(idx) {
            continue;
        }
        if n >= MAX_ROWS || !budget.try_commit(map_row(&ctx, *idx, r), true) {
            dropped_rows += 1;
        } else {
            kept_rows += 1;
        }
    }
    if dropped_rows > 0 {
        diags.push(format!("on demand: {dropped_rows} rows"));
    }
    count(&mut counts, "rows", kept_rows);
    count(&mut counts, "alternativeReaderRows", secondary.len());

    // ---- closing diagnostics
    let mut g = Grp::default();
    if let Some((kind, inferred)) = key_kind {
        g.text(&ctx.edition, de("keyValueKind"), kind.name());
        if inferred {
            g.text(&ctx.edition, de("inferredProductType"), kind.name());
        }
    }
    for d in &diags {
        g.diag(&ctx.edition, d);
    }
    budget.try_commit(g, false);

    let nodes = budget.nodes.len();
    let triples: Vec<Tr> = budget.triples.into_iter().collect();
    count(&mut counts, "nodes", nodes);
    count(&mut counts, "triples", triples.len());
    Ok(Mapped {
        sha16,
        graph_iri,
        triples,
        nodes,
        counts,
        diagnostics: diags,
    })
}

// ------------------------------------------------------------------- store

fn node(iri: &str) -> Result<NamedNode, ImportError> {
    NamedNode::new(iri).map_err(|e| ImportError::Store(format!("invalid IRI {iri}: {e}")))
}

pub(crate) fn to_term(o: &Obj) -> Result<Term, ImportError> {
    Ok(match o {
        Obj::Iri(i) => Term::from(node(i)?),
        Obj::Text(t) => Term::from(Literal::new_simple_literal(t.clone())),
        Obj::Typed(l, d) => Term::from(Literal::new_typed_literal(l.clone(), node(d)?)),
    })
}

fn graph_prefix(graph_id: &str) -> String {
    format!(
        "{}:projection:datasheet:",
        crate::rdf::graph_subject(graph_id)
    )
}

/// A handle the graph already holds for a normalized identifier.
pub(crate) fn find_family(store: &Store, normalized: &str) -> Option<String> {
    let predicate = NamedNode::new(eng("normalizedIdentifier")).ok()?;
    let literal = Literal::new_simple_literal(normalized);
    let mut found: Vec<String> = store
        .quads_for_pattern(
            None,
            Some(predicate.as_ref()),
            Some(TermRef::Literal(literal.as_ref())),
            None,
        )
        .filter_map(Result::ok)
        .filter_map(|q| match q.subject {
            NamedOrBlankNode::NamedNode(n) => Some(n.into_string()),
            _ => None,
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

#[derive(Debug)]
pub(crate) struct ImportReport {
    pub(crate) graph_id: String,
    pub(crate) graph_iri: String,
    pub(crate) sha256: String,
    pub(crate) replaced: bool,
    pub(crate) mapped: Mapped,
}

impl ImportReport {
    pub(crate) fn to_json(&self) -> Value {
        let counts: serde_json::Map<String, Value> = self
            .mapped
            .counts
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        json!({
            "graphId": self.graph_id,
            "namedGraph": self.graph_iri,
            "sha256": self.sha256,
            "mappingProfile": MAPPING_PROFILE,
            "upstreamSchema": UPSTREAM_SCHEMA,
            "replaced": self.replaced,
            "nodes": self.mapped.nodes,
            "triples": self.mapped.triples.len(),
            "budget": {"maxNodes": MAX_NODES, "maxTriples": MAX_TRIPLES},
            "counts": counts,
            "diagnostics": self.mapped.diagnostics,
        })
    }
}

/// Admit, map and write one import. The import's named graph (and any earlier
/// mapping of the same document) is cleared and the new triples go in through
/// Oxigraph's bulk loader; the vocabulary graph is refreshed beside it.
pub(crate) fn import_into_store(
    store: &Store,
    graph_id: &str,
    input: &ImportInput,
) -> Result<ImportReport, ImportError> {
    let mapped = map_envelope(graph_id, input, &|norm| find_family(store, norm))?;
    let mut quads: Vec<Quad> = Vec::with_capacity(mapped.triples.len());
    let g_node = node(&mapped.graph_iri)?;
    for t in &mapped.triples {
        quads.push(Quad::new(
            node(&t.s)?,
            node(&t.p)?,
            to_term(&t.o)?,
            GraphName::NamedNode(g_node.clone()),
        ));
    }
    let _guard = crate::cell_durability::write_guard();
    let doc_prefix = format!("{}ds-{}:", graph_prefix(graph_id), mapped.sha16);
    let mut replaced = false;
    let existing: Vec<NamedNode> = store
        .named_graphs()
        .filter_map(Result::ok)
        .filter_map(|g| match g {
            NamedOrBlankNode::NamedNode(n) if n.as_str().starts_with(&doc_prefix) => Some(n),
            _ => None,
        })
        .collect();
    for g in &existing {
        store
            .clear_graph(g.as_ref())
            .map_err(|e| ImportError::Store(format!("clear {}: {e}", g.as_str())))?;
        replaced = true;
    }
    let mut loader = store.bulk_loader();
    loader
        .load_quads(quads)
        .map_err(|e| ImportError::Store(format!("bulk load: {e}")))?;
    loader
        .commit()
        .map_err(|e| ImportError::Store(format!("bulk commit: {e}")))?;
    load_vocabulary(store, graph_id)?;
    crate::cell_durability::mark_rdf_store_written(store);
    Ok(ImportReport {
        graph_id: graph_id.to_string(),
        graph_iri: mapped.graph_iri.clone(),
        sha256: input.envelope["document"]["sha256"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        replaced,
        mapped,
    })
}

pub(crate) fn vocabulary_graph_iri(graph_id: &str) -> String {
    format!("{}vocab:m{MAPPING_VERSION}", graph_prefix(graph_id))
}

fn load_vocabulary(store: &Store, graph_id: &str) -> Result<(), ImportError> {
    let g = node(&vocabulary_graph_iri(graph_id))?;
    store
        .clear_graph(g.as_ref())
        .map_err(|e| ImportError::Store(format!("clear vocabulary: {e}")))?;
    store
        .load_from_slice(
            RdfParser::from_format(RdfFormat::Turtle).with_default_graph(g),
            VOCAB_TTL.as_bytes(),
        )
        .map_err(|e| ImportError::Store(format!("load vocabulary: {e}")))
}

// --------------------------------------------------------------------- MCP

fn hash_file(path: &std::path::Path) -> AppResult<(String, u64)> {
    let mut file = std::fs::File::open(path).map_err(|e| AppError::storage(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| AppError::storage(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

fn artifact_arg(arguments: &Value, keys: &[&str]) -> AppResult<Option<String>> {
    match mcp_arg_string(arguments, keys) {
        Some(id) => {
            crate::ids::validate_local_id(&id, keys[0]).map_err(AppError::validation)?;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

/// The first `READ_BYTES` of a string, cut on a character boundary.
fn bound_text(t: &str) -> String {
    let mut cut = t.len().min(crate::datasheet_identity::READ_BYTES);
    while !t.is_char_boundary(cut) {
        cut -= 1;
    }
    t[..cut].to_string()
}

/// The head of the text rendition (the first `READ_BYTES`), windowed to the first pages by
/// the identity rules. An unreadable rendition is not an error: identity falls back to the
/// other witnesses and says so.
fn read_text_rendition(app: &AppHandle, graph_id: &str, id: &str) -> Option<String> {
    let (_, path) =
        crate::original_file_service::open_artifact_original_file(app, graph_id, id).ok()?;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(crate::datasheet_identity::READ_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `datasheet_import { graph_id, envelope_artifact_id | envelope, pdf_artifact_id?, renditions?, text? }`
///
/// `envelope_artifact_id` names an artifact holding the `.data.json`;
/// `envelope` is the same JSON inline (an object, or a JSON string). The PDF
/// the envelope describes is recorded when `pdf_artifact_id` is given, and
/// `document.sha256` is matched against that artifact's bytes. `renditions`
/// is `{text, regions, docling, audit}` -> artifact id for the read job's other
/// outputs. `text` is the read job's `.text.txt` inline (a string, first 256 KiB kept): the
/// document's own first pages, read for its parts and maker; recorded by digest on the text
/// rendition, marked as supplied inline. It wins over `renditions.text`.
pub(crate) fn mcp_local_datasheet_import(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    crate::ids::validate_local_id(&graph_id, "graph_id").map_err(AppError::validation)?;
    let envelope_artifact_id =
        artifact_arg(arguments, &["envelope_artifact_id", "envelopeArtifactId"])?;
    let pdf_artifact_id = artifact_arg(arguments, &["pdf_artifact_id", "pdfArtifactId"])?;
    let (envelope_bytes, envelope_artifact_id) =
        match (&envelope_artifact_id, arguments.get("envelope")) {
            (Some(id), None) => {
                let (_, bytes) =
                    crate::original_file_service::read_artifact_original_file(&app, &graph_id, id)?;
                (bytes, Some(id.clone()))
            }
            (None, Some(Value::String(s))) => (s.as_bytes().to_vec(), None),
            (None, Some(v)) if v.is_object() => (
                serde_json::to_vec(v).map_err(|e| AppError::serialization(e.to_string()))?,
                None,
            ),
            _ => {
                return Err(AppError::validation(
                    "give exactly one of envelope_artifact_id or envelope",
                ))
            }
        };
    if envelope_bytes.len() as u64 > MAX_ENVELOPE_BYTES {
        return Err(AppError::capacity("the envelope exceeds 64 MiB"));
    }
    let envelope: Value = serde_json::from_slice(&envelope_bytes)
        .map_err(|e| AppError::validation(format!("the envelope is not valid JSON: {e}")))?;
    let pdf = match &pdf_artifact_id {
        Some(id) => {
            let (_, path) =
                crate::original_file_service::open_artifact_original_file(&app, &graph_id, id)?;
            let (sha256, bytes) = hash_file(&path)?;
            Some(PdfCheck {
                artifact_id: Some(id.clone()),
                sha256,
                bytes,
            })
        }
        None => None,
    };
    let mut renditions = BTreeMap::new();
    if let Some(obj) = arguments.get("renditions").and_then(Value::as_object) {
        for (role, v) in obj {
            if !["text", "regions", "docling", "audit"].contains(&role.as_str()) {
                return Err(AppError::validation(format!(
                    "unknown rendition role {role}"
                )));
            }
            if let Some(id) = v.as_str() {
                crate::ids::validate_local_id(id, role).map_err(AppError::validation)?;
                renditions.insert(role.clone(), id.to_string());
            }
        }
    }
    // The text may come inline (`text`, a string) or from a `renditions.text` artifact. Inline wins.
    // Either way only the first READ_BYTES are kept.
    let inline_text = match arguments.get("text") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => Some(bound_text(t)),
        Some(_) => return Err(AppError::validation("text must be a string")),
    };
    let text_inline = inline_text.is_some();
    let text = match (inline_text, renditions.get("text")) {
        (Some(t), _) => Some(t),
        (None, Some(id)) => read_text_rendition(&app, &graph_id, id),
        (None, None) => None,
    };
    let input = ImportInput {
        envelope_sha256: sha256_hex(&envelope_bytes),
        envelope_len: envelope_bytes.len() as u64,
        envelope,
        envelope_artifact_id,
        pdf,
        renditions,
        text,
        text_inline,
    };
    let graph_dir =
        crate::paths::existing_graph_dir(&app, &graph_id).map_err(AppError::not_found)?;
    let store = crate::rdf_service::open_graph_store(&graph_dir).map_err(AppError::storage)?;
    let report =
        import_into_store(&store, &graph_id, &input).map_err(ImportError::into_app_error)?;
    Ok(report.to_json())
}

//! `agent_self_image` — an agent makes an image of itself, and a MODE dresses it.
//!
//! Spec: `plans/2026-09-22-agent-self-image.md` (sophia hub). Ontology:
//! `sophia-agent-core` 1.2.0 (`agt:SelfImage`, `agt:selfImage`, `agt:Mode`,
//! `agt:imageTransform`).
//!
//! - The BYTES live in an ordinary Garden artifact
//!   (`artifacts/{artifactId}/original` + `revisions/`), written through the same
//!   `create_artifact_revision` path `edit_artifact_image` uses, so `read_artifact`,
//!   `list_artifact_revisions` and `restore_artifact_revision` work on them as-is.
//! - The ONTOLOGY FACE is one `agt:SelfImage` record per image in the graph's
//!   `user:rdf` named graph, gated through the live `sophia-agent-core` SHACL
//!   (structural + `sh:select`) before it is written.
//! - A MODE VARIANT is derived from the base image under an `agt:Mode`'s
//!   `agt:imageTransform` and cached by `agt:derivationKey`; a changed base or
//!   transform changes the key, so the cached variant reads as stale and the next
//!   `mode_variant` call regenerates it.
//!
//! Self-only: every write names ONE agent (`agentId`) and touches only that
//! agent's own `#self-image…` subjects and its `agt:selfImage` link. The agent
//! must already exist in the graph (no roster creation). Like `status`, the
//! supplied agent id is a claim at the cell (the lease authenticates a principal,
//! not an agent); a hosted principal that did not write an image may not replace
//! it. Choreograph pins `agentId` to the session's own agent at its host gate.

use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
    emporium::terms::{Term, Triple},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{SecondsFormat, Utc};
use oxigraph::{
    model::{GraphNameRef, Literal, NamedNode, NamedNodeRef, Term as OxTerm},
    store::Store,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(crate) const TOOL_NAME: &str = "agent_self_image";
const AGT: &str = "http://mnemosyne.dev/agent#";
const PROV: &str = "http://www.w3.org/ns/prov#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const MAX_PROMPT: usize = 2000;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) const LOCAL_GENERATOR: &str = "local-sigil";
pub(crate) const LOCAL_MODEL: &str = "sigil-v1";
pub(crate) const OPENROUTER_GENERATOR: &str = "openrouter";

fn agt(local: &str) -> String {
    format!("{AGT}{local}")
}

/// Canonical `agent-<hex16>` — the same witness key `agent_status` accepts.
pub(crate) fn agent_id_valid(id: &str) -> bool {
    id.len() == 22
        && id.starts_with("agent-")
        && id.as_bytes()[6..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

pub(crate) fn agent_iri(agent_id: &str) -> String {
    format!("urn:sophia:agent:{agent_id}")
}
pub(crate) fn base_image_iri(agent_id: &str) -> String {
    format!("{}#self-image", agent_iri(agent_id))
}
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn mode_key(mode_iri: &str) -> String {
    sha256_hex(mode_iri.as_bytes())[..16].to_string()
}
pub(crate) fn variant_image_iri(agent_id: &str, mode_iri: &str) -> String {
    format!("{}:mode:{}", base_image_iri(agent_id), mode_key(mode_iri))
}
pub(crate) fn base_artifact_id(agent_id: &str) -> String {
    format!("agent-self-image-{}", &agent_id[6..])
}
pub(crate) fn variant_artifact_id(agent_id: &str, mode_iri: &str) -> String {
    format!("{}-mode-{}", base_artifact_id(agent_id), mode_key(mode_iri))
}

/// The variant cache key: any change to the base bytes, the mode's transform,
/// or the generator/model yields a different key (and so a stale variant).
pub(crate) fn derivation_key(
    base_sha256: &str,
    transform: &str,
    generator: &str,
    model: &str,
) -> String {
    let canonical = serde_json::to_vec(&json!([
        "agent-self-image-derivation-v1",
        base_sha256,
        transform,
        generator,
        model
    ]))
    .expect("JSON array serializes");
    sha256_hex(&canonical)
}

// ── Stored records (read back from RDF) ─────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SelfImageRecord {
    pub(crate) iri: String,
    pub(crate) fields: BTreeMap<String, String>,
}

impl SelfImageRecord {
    fn get(&self, local: &str) -> Option<&str> {
        self.fields.get(local).map(String::as_str)
    }
    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("iri".into(), json!(self.iri));
        for (key, value) in &self.fields {
            out.insert(key.clone(), json!(value));
        }
        Value::Object(out)
    }
}

fn user_graph(graph_id: &str) -> String {
    crate::rdf_authority::user_rdf_graph_iri(graph_id)
}

fn literal_or_iri(term: &OxTerm) -> Option<String> {
    match term {
        OxTerm::NamedNode(node) => Some(node.as_str().to_string()),
        OxTerm::Literal(literal) => Some(literal.value().to_string()),
        _ => None,
    }
}

/// Read one subject's predicates from ANY named graph (agents and modes may be
/// authored in any lane of the graph; self-images are always in `user:rdf`).
fn subject_fields(store: &Store, subject: &str) -> AppResult<Vec<(String, String)>> {
    let node = NamedNode::new(subject).map_err(|e| AppError::validation(e.to_string()))?;
    let mut out = Vec::new();
    for (index, quad) in store
        .quads_for_pattern(Some(node.as_ref().into()), None, None, None)
        .enumerate()
    {
        if index >= 512 {
            return Err(AppError::validation("subject exceeds 512 statements"));
        }
        let quad = quad.map_err(|e| AppError::rdf(e.to_string()))?;
        if let Some(value) = literal_or_iri(&quad.object) {
            out.push((quad.predicate.as_str().to_string(), value));
        }
    }
    Ok(out)
}

fn has_type(fields: &[(String, String)], class: &str) -> bool {
    fields.iter().any(|(p, o)| p == RDF_TYPE && o == class)
}

fn first(fields: &[(String, String)], predicate: &str) -> Option<String> {
    fields
        .iter()
        .find(|(p, _)| p == predicate)
        .map(|(_, o)| o.clone())
}

/// The agent's label if the agent exists in the graph (typed `agt:Agent`).
pub(crate) fn agent_in_graph(store: &Store, agent_id: &str) -> AppResult<Option<Option<String>>> {
    let fields = subject_fields(store, &agent_iri(agent_id))?;
    if !has_type(&fields, &agt("Agent")) {
        return Ok(None);
    }
    Ok(Some(first(&fields, RDFS_LABEL)))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModeView {
    pub(crate) iri: String,
    pub(crate) label: Option<String>,
    pub(crate) transform: String,
}

pub(crate) fn read_mode(store: &Store, mode_iri: &str) -> AppResult<ModeView> {
    let fields = subject_fields(store, mode_iri)?;
    if !has_type(&fields, &agt("Mode")) {
        return Err(AppError::validation(format!(
            "{mode_iri} is not an agt:Mode in this graph"
        )));
    }
    let transform = first(&fields, &agt("imageTransform"))
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| {
            AppError::validation(format!(
                "mode {mode_iri} has no agt:imageTransform; nothing to derive"
            ))
        })?;
    Ok(ModeView {
        iri: mode_iri.to_string(),
        label: first(&fields, RDFS_LABEL),
        transform,
    })
}

pub(crate) fn read_self_image(
    store: &Store,
    graph_id: &str,
    iri: &str,
) -> AppResult<Option<SelfImageRecord>> {
    let node = NamedNode::new(iri).map_err(|e| AppError::validation(e.to_string()))?;
    let graph = user_graph(graph_id);
    let mut record = SelfImageRecord {
        iri: iri.to_string(),
        fields: BTreeMap::new(),
    };
    let mut typed = false;
    for quad in store.quads_for_pattern(
        Some(node.as_ref().into()),
        None,
        None,
        Some(GraphNameRef::NamedNode(NamedNodeRef::new_unchecked(&graph))),
    ) {
        let quad = quad.map_err(|e| AppError::rdf(e.to_string()))?;
        let predicate = quad.predicate.as_str();
        if predicate == RDF_TYPE {
            typed |= matches!(&quad.object, OxTerm::NamedNode(n) if n.as_str() == agt("SelfImage"));
            continue;
        }
        let local = predicate
            .strip_prefix(AGT)
            .or_else(|| predicate.strip_prefix(PROV))
            .unwrap_or(predicate);
        if let Some(value) = literal_or_iri(&quad.object) {
            record.fields.insert(local.to_string(), value);
        }
    }
    Ok(typed.then_some(record))
}

/// The mode the agent names as its default, if any.
fn default_mode(store: &Store, agent_id: &str) -> AppResult<Option<String>> {
    Ok(first(
        &subject_fields(store, &agent_iri(agent_id))?,
        &agt("defaultMode"),
    ))
}

// ── Record construction + the SHACL gate + the write ────────────────────────

pub(crate) struct NewImage<'a> {
    pub(crate) agent_id: &'a str,
    pub(crate) artifact_id: &'a str,
    pub(crate) revision_id: &'a str,
    pub(crate) content_sha256: &'a str,
    pub(crate) mime_type: &'a str,
    pub(crate) prompt: &'a str,
    pub(crate) generator: &'a str,
    pub(crate) model: &'a str,
    pub(crate) generated_at: &'a str,
    pub(crate) principal: Option<&'a str>,
    /// `Some((base_iri, mode_iri, derivation_key))` for a mode variant.
    pub(crate) variant: Option<(&'a str, &'a str, &'a str)>,
}

fn iri_term(value: &str) -> AppResult<Term> {
    NamedNode::new(value)
        .map(Term::Uri)
        .map_err(|e| AppError::validation(e.to_string()))
}
fn str_term(value: &str) -> Term {
    Term::Lit(Literal::new_simple_literal(value))
}

pub(crate) fn record_triples(image: &NewImage<'_>) -> AppResult<(String, Vec<Triple>)> {
    let subject = match image.variant {
        Some((_, mode_iri, _)) => variant_image_iri(image.agent_id, mode_iri),
        None => base_image_iri(image.agent_id),
    };
    let s = || subject.clone();
    let mut triples: Vec<Triple> = vec![
        (s(), RDF_TYPE.into(), iri_term(&agt("SelfImage"))?),
        (s(), RDF_TYPE.into(), iri_term(&format!("{PROV}Entity"))?),
        (s(), agt("imageOf"), iri_term(&agent_iri(image.agent_id))?),
        (s(), agt("artifactId"), str_term(image.artifact_id)),
        (s(), agt("artifactRevision"), str_term(image.revision_id)),
        (s(), agt("contentSha256"), str_term(image.content_sha256)),
        (s(), agt("mimeType"), str_term(image.mime_type)),
        (s(), agt("imagePrompt"), str_term(image.prompt)),
        (s(), agt("generator"), str_term(image.generator)),
        (s(), agt("generatorModel"), str_term(image.model)),
        (s(), agt("generatedByTool"), str_term(TOOL_NAME)),
        (
            s(),
            format!("{PROV}generatedAtTime"),
            Term::Lit(Literal::new_typed_literal(
                image.generated_at,
                NamedNode::new_unchecked(XSD_DATETIME),
            )),
        ),
    ];
    if let Some(principal) = image.principal {
        triples.push((s(), agt("ingressPrincipal"), str_term(principal)));
    }
    if let Some((base_iri, mode_iri, key)) = image.variant {
        triples.push((s(), agt("derivedFromImage"), iri_term(base_iri)?));
        triples.push((s(), agt("underMode"), iri_term(mode_iri)?));
        triples.push((s(), agt("derivationKey"), str_term(key)));
    }
    Ok((subject, triples))
}

/// Gate a record through the LIVE `sophia-agent-core` contract (structural +
/// `sh:select`). `context` carries already-stored triples the shapes need to see
/// (the base record, for a variant's same-agent check).
pub(crate) fn validate_record(
    triples: &[Triple],
    context: &[Triple],
    graph_id: &str,
) -> AppResult<()> {
    let contract = crate::emporium::contract::get_vocabulary("sophia-agent-core")
        .ok_or_else(|| AppError::internal("sophia-agent-core vocabulary is not registered"))?;
    let mut all = context.to_vec();
    all.extend_from_slice(triples);
    crate::emporium::shacl_validator::validate_desired_structured_in_graph(
        &all,
        contract,
        &user_graph(graph_id),
    )
    .map_err(|violations| {
        let detail: Vec<String> = violations
            .iter()
            .map(|v| format!("{}: {}", v.focus_node, v.message))
            .collect();
        AppError::validation(format!(
            "self-image record violates sophia-agent-core: {}",
            detail.join("; ")
        ))
        .with_code("agent_self_image_shacl_violation")
    })
}

fn stored_triples(record: &SelfImageRecord) -> AppResult<Vec<Triple>> {
    let mut out = vec![(
        record.iri.clone(),
        RDF_TYPE.to_string(),
        iri_term(&agt("SelfImage"))?,
    )];
    out.push((
        record.iri.clone(),
        RDF_TYPE.to_string(),
        iri_term(&format!("{PROV}Entity"))?,
    ));
    for (local, value) in &record.fields {
        let (predicate, object) = match local.as_str() {
            "imageOf" | "derivedFromImage" | "underMode" => (agt(local), iri_term(value)?),
            "generatedAtTime" => (
                format!("{PROV}generatedAtTime"),
                Term::Lit(Literal::new_typed_literal(
                    value,
                    NamedNode::new_unchecked(XSD_DATETIME),
                )),
            ),
            _ => (agt(local), str_term(value)),
        };
        out.push((record.iri.clone(), predicate, object));
    }
    Ok(out)
}

/// Replace exactly this agent's own image subject (and, for a base, the agent's
/// `agt:selfImage` link) in `user:rdf` — one atomic SPARQL update.
pub(crate) fn write_record(
    store: &Store,
    graph_id: &str,
    agent_id: &str,
    subject: &str,
    triples: &[Triple],
    is_base: bool,
) -> AppResult<()> {
    let own_prefix = base_image_iri(agent_id);
    if !subject.starts_with(&own_prefix) || triples.iter().any(|(s, _, _)| s != subject) {
        return Err(AppError::validation(
            "agent_self_image writes only the calling agent's own self-image subjects",
        ));
    }
    let graph = user_graph(graph_id);
    let body: String = triples
        .iter()
        .map(|(s, p, o)| format!("<{s}> <{p}> {} .\n", o.as_nt()))
        .collect();
    let agent = agent_iri(agent_id);
    let link = if is_base {
        format!(
            "DELETE WHERE {{ GRAPH <{graph}> {{ <{agent}> <{}> ?old }} }};\n",
            agt("selfImage")
        )
    } else {
        String::new()
    };
    let link_insert = if is_base {
        format!("<{agent}> <{}> <{subject}> .\n", agt("selfImage"))
    } else {
        String::new()
    };
    let update = format!(
        "DELETE WHERE {{ GRAPH <{graph}> {{ <{subject}> ?p ?o }} }};\n{link}INSERT DATA {{ GRAPH <{graph}> {{\n{body}{link_insert}}} }}"
    );
    crate::rdf_query_service::execute_sparql_update(store, &update)
        .map(|_| ())
        .map_err(AppError::rdf)
}

/// A hosted principal may not overwrite an image another principal wrote.
pub(crate) fn guard_principal(
    existing: Option<&SelfImageRecord>,
    principal: Option<&str>,
) -> AppResult<()> {
    if let (Some(record), Some(caller)) = (existing, principal) {
        if let Some(owner) = record.get("ingressPrincipal") {
            if owner != caller {
                return Err(AppError::conflict(
                    "this self-image was written by a different principal; only its writer may replace it",
                )
                .with_code("agent_self_image_foreign_writer"));
            }
        }
    }
    Ok(())
}

// ── Generators ──────────────────────────────────────────────────────────────

pub(crate) struct Generated {
    pub(crate) mime_type: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) model: String,
    pub(crate) prompt: String,
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn hsl(seed: &[u8], index: usize, saturation: u8, lightness: u8) -> String {
    let hue = u32::from(seed[index]) * 360 / 256;
    format!("hsl({hue},{saturation}%,{lightness}%)")
}

/// The deterministic local generator: a small SVG creature whose colours and
/// features come from sha256(agentId, prompt). Same inputs → same bytes. It
/// exists so the ontology + artifact wiring is real and testable without a
/// provider, and so a cell with no image key still gets a face.
pub(crate) fn sigil_base(agent_id: &str, label: Option<&str>, prompt: &str) -> Generated {
    let seed = Sha256::digest(format!("{agent_id}\n{prompt}").as_bytes());
    let bg = hsl(&seed, 0, 45, 88);
    let body = hsl(&seed, 1, 55, 58);
    let cheek = hsl(&seed, 2, 70, 75);
    let eye_gap = 34 + (seed[3] % 20) as u32;
    let eye_r = 9 + (seed[4] % 6) as u32;
    let head_rx = 118 + (seed[5] % 30) as u32;
    let head_ry = 128 + (seed[6] % 30) as u32;
    let mouth_curve = 20 + (seed[7] % 24) as u32;
    let antenna = seed[8] % 2 == 0;
    let name = xml_escape(label.unwrap_or(agent_id));
    let antenna_svg = if antenna {
        format!(
            r#"<line x1="256" y1="{}" x2="256" y2="{}" stroke="{body}" stroke-width="8" stroke-linecap="round"/><circle cx="256" cy="{}" r="14" fill="{cheek}"/>"#,
            276 - head_ry,
            236 - head_ry,
            228 - head_ry
        )
    } else {
        String::new()
    };
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512" data-sophia-sigil="{LOCAL_MODEL}">
<rect width="512" height="512" fill="{bg}"/>
{antenna_svg}<ellipse cx="256" cy="276" rx="{head_rx}" ry="{head_ry}" fill="{body}"/>
<circle cx="{lx}" cy="250" r="{eye_r}" fill="#1d1b26"/><circle cx="{rx}" cy="250" r="{eye_r}" fill="#1d1b26"/>
<circle cx="{lc}" cy="300" r="16" fill="{cheek}" opacity="0.8"/><circle cx="{rc}" cy="300" r="16" fill="{cheek}" opacity="0.8"/>
<path d="M 226 318 Q 256 {my} 286 318" stroke="#1d1b26" stroke-width="7" fill="none" stroke-linecap="round"/>
<text x="256" y="490" text-anchor="middle" font-family="sans-serif" font-size="22" fill="#1d1b26">{name}</text>
</svg>
"##,
        lx = 256 - eye_gap,
        rx = 256 + eye_gap,
        lc = 256 - eye_gap - 36,
        rc = 256 + eye_gap + 36,
        my = 318 + mouth_curve,
    );
    Generated {
        mime_type: "image/svg+xml".into(),
        bytes: svg.into_bytes(),
        model: LOCAL_MODEL.into(),
        prompt: prompt.to_string(),
    }
}

/// Derive a mode variant locally: the BASE image (whatever its format) is
/// embedded unchanged and the transform is drawn over it. Recognised props are
/// drawn; anything else becomes a labelled ribbon — honest about what it can do.
pub(crate) fn sigil_variant(
    base_mime: &str,
    base_bytes: &[u8],
    mode_label: Option<&str>,
    transform: &str,
) -> Generated {
    let lower = transform.to_lowercase();
    let mut overlay = String::new();
    if lower.contains("glasses") || lower.contains("spectacles") {
        overlay.push_str(r##"<g data-prop="glasses" stroke="#2a2233" stroke-width="7" fill="rgba(255,255,255,0.25)"><circle cx="206" cy="250" r="34"/><circle cx="306" cy="250" r="34"/><path d="M 240 250 Q 256 238 272 250" fill="none"/><path d="M 172 246 L 136 232" fill="none"/><path d="M 340 246 L 376 232" fill="none"/></g>"##);
    }
    if lower.contains("hat") || lower.contains("cap") {
        overlay.push_str(r##"<g data-prop="hat" fill="#2a2233"><rect x="176" y="118" width="160" height="22" rx="8"/><rect x="206" y="52" width="100" height="72" rx="10"/></g>"##);
    }
    if lower.contains("crown") {
        overlay.push_str(r##"<path data-prop="crown" d="M 186 140 L 206 80 L 236 124 L 256 70 L 276 124 L 306 80 L 326 140 Z" fill="#e8b923" stroke="#8a6b00" stroke-width="4"/>"##);
    }
    if lower.contains("headphone") {
        overlay.push_str(r##"<g data-prop="headphones" stroke="#2a2233" stroke-width="12" fill="none"><path d="M 150 270 Q 150 130 256 130 Q 362 130 362 270"/></g><rect x="128" y="250" width="36" height="64" rx="12" fill="#2a2233"/><rect x="348" y="250" width="36" height="64" rx="12" fill="#2a2233"/>"##);
    }
    let ribbon = xml_escape(mode_label.unwrap_or(transform));
    let data_url = format!(
        "data:{base_mime};base64,{}",
        BASE64_STANDARD.encode(base_bytes)
    );
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="512" height="512" viewBox="0 0 512 512" data-sophia-sigil="{LOCAL_MODEL}" data-transform="{t}">
<image href="{data_url}" x="0" y="0" width="512" height="512"/>
{overlay}
<g data-prop="mode-ribbon"><rect x="0" y="0" width="512" height="40" fill="#2a2233" opacity="0.85"/><text x="256" y="27" text-anchor="middle" font-family="sans-serif" font-size="20" fill="#ffffff">{ribbon}</text></g>
</svg>
"##,
        t = xml_escape(transform),
    );
    Generated {
        mime_type: "image/svg+xml".into(),
        bytes: svg.into_bytes(),
        model: LOCAL_MODEL.into(),
        prompt: transform.to_string(),
    }
}

pub(crate) fn openrouter_base_prompt(label: Option<&str>, prompt: &str) -> String {
    format!(
        "A square self-portrait avatar for an AI agent{}. {prompt} Centered character, simple uncluttered background, friendly and legible at small sizes.",
        label.map(|l| format!(" called \"{l}\"")).unwrap_or_default()
    )
}

pub(crate) fn openrouter_variant_prompt(transform: &str) -> String {
    format!(
        "This is an AI agent's self-portrait. Keep the same character, identity, pose, palette and style. Change only this: {transform}."
    )
}

// ── The MCP handler ─────────────────────────────────────────────────────────

fn graph_id(args: &Value) -> AppResult<String> {
    if let (Some(a), Some(b)) = (args.get("graph_id"), args.get("graphId")) {
        if a != b {
            return Err(AppError::validation("conflicting graph aliases"));
        }
    }
    crate::mcp_utils::mcp_required_graph_id(args).map_err(AppError::validation)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

struct Call {
    graph: String,
    action: String,
    agent_id: String,
    prompt: Option<String>,
    mode_iri: Option<String>,
    generator: String,
    include_image: bool,
    force: bool,
}

fn parse(args: &Value) -> AppResult<Call> {
    const KNOWN: &[&str] = &[
        "graph_id",
        "graphId",
        "action",
        "agentId",
        "prompt",
        "modeIri",
        "generator",
        "includeImage",
        "force",
    ];
    if let Some(object) = args.as_object() {
        if let Some(extra) = object.keys().find(|k| !KNOWN.contains(&k.as_str())) {
            return Err(AppError::validation(format!("unknown argument {extra}")));
        }
    }
    let graph = graph_id(args)?;
    let action = arg_str(args, "action").unwrap_or("current").to_string();
    if !["generate", "mode_variant", "current", "describe"].contains(&action.as_str()) {
        return Err(AppError::validation(
            "action must be generate | mode_variant | current | describe",
        ));
    }
    let agent_id = arg_str(args, "agentId")
        .filter(|id| agent_id_valid(id))
        .ok_or_else(|| AppError::validation("agentId must be a canonical agent-<hex16> id"))?
        .to_string();
    let prompt = arg_str(args, "prompt").map(str::trim).map(str::to_string);
    if prompt.as_ref().is_some_and(|p| p.len() > MAX_PROMPT) {
        return Err(AppError::validation("prompt exceeds 2000 bytes"));
    }
    let mode_iri = arg_str(args, "modeIri").map(str::to_string);
    if let Some(iri) = &mode_iri {
        NamedNode::new(iri.as_str()).map_err(|_| AppError::validation("modeIri must be an IRI"))?;
    }
    let generator = arg_str(args, "generator")
        .unwrap_or(OPENROUTER_GENERATOR)
        .to_string();
    if ![OPENROUTER_GENERATOR, LOCAL_GENERATOR].contains(&generator.as_str()) {
        return Err(AppError::validation(
            "generator must be openrouter | local-sigil",
        ));
    }
    Ok(Call {
        graph,
        action,
        agent_id,
        prompt,
        mode_iri,
        generator,
        include_image: args.get("includeImage").and_then(Value::as_bool) == Some(true),
        force: args.get("force").and_then(Value::as_bool) == Some(true),
    })
}

async fn open_store(app: &AppHandle, graph: &str) -> AppResult<std::sync::Arc<Store>> {
    let (dir, _) = crate::graph_record_store::read_graph_record_no_heal(app, graph)?;
    crate::rdf_service::open_graph_store(&dir).map_err(AppError::rdf)
}

/// What the handler needs to know before generating (read under the lease).
struct Snapshot {
    label: Option<String>,
    base: Option<SelfImageRecord>,
    mode: Option<ModeView>,
    variant: Option<SelfImageRecord>,
}

/// Read under a read-only lease. `with_mode`: resolve `modeIri`, else the
/// agent's `agt:defaultMode`, and read the mode + this agent's variant under it.
async fn snapshot(app: &AppHandle, call: &Call, with_mode: bool) -> AppResult<Snapshot> {
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let lease = coordinator
        .acquire_hot_write(&call.graph)
        .await
        .map_err(AppError::storage)?;
    lease.declare_rdf_read_only();
    let store = open_store(app, &call.graph).await?;
    let label = agent_in_graph(&store, &call.agent_id)?.ok_or_else(|| {
        AppError::validation(
            "Agent is absent from this graph's ontology; a self-image does not create a roster",
        )
    })?;
    let base = read_self_image(&store, &call.graph, &base_image_iri(&call.agent_id))?;
    let mode_iri = match (&call.mode_iri, with_mode) {
        (Some(iri), true) => Some(iri.clone()),
        (None, true) => default_mode(&store, &call.agent_id)?,
        _ => None,
    };
    let (mode, variant) = match mode_iri {
        Some(iri) => (
            Some(read_mode(&store, &iri)?),
            read_self_image(
                &store,
                &call.graph,
                &variant_image_iri(&call.agent_id, &iri),
            )?,
        ),
        None => (None, None),
    };
    Ok(Snapshot {
        label,
        base,
        mode,
        variant,
    })
}

async fn generate_bytes_openrouter(
    app: &AppHandle,
    prompt: &str,
    input: Option<(&str, &[u8])>,
) -> AppResult<Generated> {
    let key = crate::local_provider_keys::provider_key(app, "openrouter").ok_or_else(|| {
        AppError::validation(
            "No OpenRouter API key: set SOPHIA_OPENROUTER_API_KEY (cells) or the macOS Keychain item dev.sophia.garden/openrouter; or use generator=local-sigil",
        )
        .with_code("agent_self_image_no_provider_key")
    })?;
    let (mime_type, b64) =
        crate::artifact_mcp_service::openrouter_image(&key, prompt, input).await?;
    let bytes = BASE64_STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| AppError::internal(format!("image model returned invalid base64: {e}")))?;
    Ok(Generated {
        mime_type,
        bytes,
        model: crate::artifact_mcp_service::IMAGE_MODEL.to_string(),
        prompt: prompt.to_string(),
    })
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "image/svg+xml" => "svg",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// Save bytes as a new artifact revision, then gate + write the RDF record.
async fn persist(
    app: &AppHandle,
    call: &Call,
    artifact_id: &str,
    generated: &Generated,
    variant: Option<(&str, &str, &str)>,
    context: &[Triple],
    label: String,
) -> AppResult<(
    SelfImageRecord,
    crate::artifact_revisions::ArtifactRevisionEntry,
)> {
    if generated.bytes.is_empty() || generated.bytes.len() > MAX_IMAGE_BYTES {
        return Err(AppError::validation(
            "generated image is empty or exceeds 8 MiB",
        ));
    }
    let principal = crate::cell_graph_boundary::current_cell_lease().map(|lease| lease.principal);
    let content_sha256 = sha256_hex(&generated.bytes);
    let generated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    // Build + gate the record BEFORE touching bytes, with a placeholder revision
    // (the revision id does not affect any shape), so a malformed record never
    // leaves an orphan revision behind.
    let probe = NewImage {
        agent_id: &call.agent_id,
        artifact_id,
        revision_id: "rev-pending",
        content_sha256: &content_sha256,
        mime_type: &generated.mime_type,
        prompt: &generated.prompt,
        generator: &call.generator,
        model: &generated.model,
        generated_at: &generated_at,
        principal: principal.as_deref(),
        variant,
    };
    let (_, probe_triples) = record_triples(&probe)?;
    validate_record(&probe_triples, context, &call.graph)?;

    let filename = format!("self-image.{}", extension(&generated.mime_type));
    let entry = crate::artifact_revisions::create_artifact_revision(
        app,
        &call.graph,
        artifact_id,
        &filename,
        &generated.mime_type,
        &BASE64_STANDARD.encode(&generated.bytes),
        Some(label),
    )?;
    let image = NewImage {
        revision_id: &entry.revision_id,
        ..probe
    };
    let (subject, triples) = record_triples(&image)?;

    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let _lease = coordinator
        .acquire_hot_write(&call.graph)
        .await
        .map_err(AppError::storage)?;
    crate::restore_guard::require_no_active_restore(app, &call.graph)
        .map_err(AppError::conflict)?;
    let store = open_store(app, &call.graph).await?;
    if agent_in_graph(&store, &call.agent_id)?.is_none() {
        return Err(AppError::conflict(
            "agent disappeared while its image was generated",
        ));
    }
    write_record(
        &store,
        &call.graph,
        &call.agent_id,
        &subject,
        &triples,
        variant.is_none(),
    )?;
    let (dir, _) = crate::graph_record_store::read_graph_record_no_heal(app, &call.graph)?;
    crate::graph_service::touch_graph_updated_at(&dir).map_err(AppError::storage)?;
    let record = read_self_image(&store, &call.graph, &subject)?
        .ok_or_else(|| AppError::internal("self-image record did not read back"))?;
    Ok((record, entry))
}

fn attach_image(
    app: &AppHandle,
    graph: &str,
    out: &mut Value,
    key: &str,
    record: &SelfImageRecord,
) -> AppResult<()> {
    let artifact_id = record
        .get("artifactId")
        .ok_or_else(|| AppError::internal("self-image record lacks artifactId"))?;
    let (manifest, bytes) =
        crate::original_file_service::read_artifact_original_file(app, graph, artifact_id)?;
    out[key] = json!({
        "mimeType": manifest.mime_type,
        "dataBase64": BASE64_STANDARD.encode(&bytes),
        "liveContentSha256": sha256_hex(&bytes),
    });
    Ok(())
}

fn variant_status(
    base: Option<&SelfImageRecord>,
    mode: &ModeView,
    variant: Option<&SelfImageRecord>,
) -> &'static str {
    let (Some(base), Some(variant)) = (base, variant) else {
        return "missing";
    };
    let generator = variant.get("generator").unwrap_or_default();
    let model = variant.get("generatorModel").unwrap_or_default();
    let expected = derivation_key(
        base.get("contentSha256").unwrap_or_default(),
        &mode.transform,
        generator,
        model,
    );
    if variant.get("derivationKey") == Some(expected.as_str()) {
        "fresh"
    } else {
        "stale"
    }
}

pub(crate) async fn mcp(app: AppHandle, args: &Value) -> AppResult<Value> {
    let call = parse(args)?;
    match call.action.as_str() {
        "generate" => generate(&app, &call).await,
        "mode_variant" => mode_variant(&app, &call).await,
        _ => read(&app, &call).await,
    }
}

async fn generate(app: &AppHandle, call: &Call) -> AppResult<Value> {
    let prompt = call
        .prompt
        .clone()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| AppError::validation("generate requires a prompt describing yourself"))?;
    let snap = snapshot(app, call, false).await?;
    let principal = crate::cell_graph_boundary::current_cell_lease().map(|lease| lease.principal);
    guard_principal(snap.base.as_ref(), principal.as_deref())?;
    let generated = match call.generator.as_str() {
        LOCAL_GENERATOR => sigil_base(&call.agent_id, snap.label.as_deref(), &prompt),
        _ => {
            let full = openrouter_base_prompt(snap.label.as_deref(), &prompt);
            generate_bytes_openrouter(app, &full, None).await?
        }
    };
    let artifact_id = base_artifact_id(&call.agent_id);
    let label = format!(
        "Self-image: {}",
        prompt.chars().take(48).collect::<String>()
    );
    let (record, revision) = persist(app, call, &artifact_id, &generated, None, &[], label).await?;
    Ok(json!({
        "action": "generate",
        "agentIri": agent_iri(&call.agent_id),
        "selfImage": record.to_json(),
        "revision": revision,
        "next": "Call action=mode_variant with a modeIri to see yourself under a mode.",
    }))
}

async fn mode_variant(app: &AppHandle, call: &Call) -> AppResult<Value> {
    let snap = snapshot(app, call, true).await?;
    let base = snap.base.clone().ok_or_else(|| {
        AppError::validation("no base self-image yet; call action=generate first")
            .with_code("agent_self_image_no_base")
    })?;
    let mode = snap.mode.clone().ok_or_else(|| {
        AppError::validation("mode_variant requires modeIri (the agent has no agt:defaultMode)")
    })?;
    let principal = crate::cell_graph_boundary::current_cell_lease().map(|lease| lease.principal);
    guard_principal(snap.variant.as_ref(), principal.as_deref())?;

    let model = match call.generator.as_str() {
        LOCAL_GENERATOR => LOCAL_MODEL,
        _ => crate::artifact_mcp_service::IMAGE_MODEL,
    };
    let key = derivation_key(
        base.get("contentSha256").unwrap_or_default(),
        &mode.transform,
        &call.generator,
        model,
    );
    if !call.force {
        if let Some(existing) = &snap.variant {
            if existing.get("derivationKey") == Some(key.as_str()) {
                return Ok(json!({
                    "action": "mode_variant",
                    "cached": true,
                    "mode": {"iri": mode.iri, "label": mode.label, "imageTransform": mode.transform},
                    "base": base.to_json(),
                    "selfImage": existing.to_json(),
                }));
            }
        }
    }
    let base_artifact = base
        .get("artifactId")
        .ok_or_else(|| AppError::internal("base record lacks artifactId"))?;
    let (base_manifest, base_bytes) =
        crate::original_file_service::read_artifact_original_file(app, &call.graph, base_artifact)?;
    if sha256_hex(&base_bytes) != base.get("contentSha256").unwrap_or_default() {
        return Err(AppError::conflict(
            "the base self-image artifact changed outside agent_self_image (e.g. a restore); regenerate the base first",
        )
        .with_code("agent_self_image_base_drift"));
    }
    let generated = match call.generator.as_str() {
        LOCAL_GENERATOR => sigil_variant(
            &base_manifest.mime_type,
            &base_bytes,
            mode.label.as_deref(),
            &mode.transform,
        ),
        _ => {
            let prompt = openrouter_variant_prompt(&mode.transform);
            generate_bytes_openrouter(app, &prompt, Some((&base_manifest.mime_type, &base_bytes)))
                .await?
        }
    };
    let artifact_id = variant_artifact_id(&call.agent_id, &mode.iri);
    let context = stored_triples(&base)?;
    let label = format!(
        "Self-image under mode {}",
        mode.label.as_deref().unwrap_or(&mode.iri)
    );
    let (record, revision) = persist(
        app,
        call,
        &artifact_id,
        &generated,
        Some((&base.iri, &mode.iri, &key)),
        &context,
        label,
    )
    .await?;
    Ok(json!({
        "action": "mode_variant",
        "cached": false,
        "mode": {"iri": mode.iri, "label": mode.label, "imageTransform": mode.transform},
        "base": base.to_json(),
        "selfImage": record.to_json(),
        "revision": revision,
    }))
}

async fn read(app: &AppHandle, call: &Call) -> AppResult<Value> {
    let snap = snapshot(app, call, true).await?;
    let mut out = json!({
        "action": call.action,
        "agentIri": agent_iri(&call.agent_id),
        "label": snap.label,
        "selfImage": snap.base.as_ref().map(SelfImageRecord::to_json),
    });
    if let Some(mode) = &snap.mode {
        out["mode"] =
            json!({"iri": mode.iri, "label": mode.label, "imageTransform": mode.transform});
        out["modeVariant"] = json!(snap.variant.as_ref().map(SelfImageRecord::to_json));
        out["modeVariantStatus"] = json!(variant_status(
            snap.base.as_ref(),
            mode,
            snap.variant.as_ref()
        ));
    }
    if call.action == "describe" {
        let mut lines = Vec::new();
        match &snap.base {
            Some(base) => lines.push(format!(
                "Base self-image of {}: made by {} with {} ({}) at {}, from the prompt: \"{}\".",
                snap.label.as_deref().unwrap_or(&call.agent_id),
                base.get("generatedByTool").unwrap_or("?"),
                base.get("generator").unwrap_or("?"),
                base.get("generatorModel").unwrap_or("?"),
                base.get("generatedAtTime").unwrap_or("?"),
                base.get("imagePrompt").unwrap_or("?"),
            )),
            None => lines.push("No self-image yet.".to_string()),
        }
        if let Some(mode) = &snap.mode {
            lines.push(format!(
                "Under mode {} the image is transformed by: \"{}\" (variant {}).",
                mode.label.as_deref().unwrap_or(&mode.iri),
                mode.transform,
                variant_status(snap.base.as_ref(), mode, snap.variant.as_ref()),
            ));
        }
        out["description"] = json!(lines.join(" "));
    }
    if call.action == "current" && call.include_image {
        if let Some(base) = &snap.base {
            attach_image(app, &call.graph, &mut out, "image", base)?;
        }
        if let Some(variant) = &snap.variant {
            attach_image(app, &call.graph, &mut out, "modeVariantImage", variant)?;
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "agent_self_image_tests.rs"]
mod tests;

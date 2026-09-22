//! Read-only Emporium PDF face. Document/Y.Doc metadata is the relationship
//! source; original bytes remain artifact authority. No independent writer.
use crate::document_service::DocumentRecord;
use crate::emporium::{
    contract::get_vocabulary,
    planner::plan_generic_compute,
    reconcile::{apply_diff, survey_class, ClassScope, Placement, SpanKey},
    schemas::GenericRecordIn,
    terms::{Term, Triple, TripleDiff},
};
use crate::pdf_source_wire::{self as wire, Inspection};
use oxigraph::store::Store;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const PACK: &str = "garden-pdf-source";
pub(crate) const NS: &str = "http://mnemosyne.dev/pdf-source#";
pub(crate) const PROJECTION_VERSION: &str = "pdf-source-v1";
pub(crate) const MAX_ANCHORS: usize = 4_096;
pub(crate) const MAX_DOCUMENT_RECTS: usize = 16_384;
const CLASSES: [&str; 4] = [
    "PdfOriginal",
    "TextSourceAnchor",
    "PdfRegion",
    "PdfPageSelector",
];

#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary {
    pub(crate) anchors_seen: usize,
    pub(crate) anchors_projected: usize,
    pub(crate) rectangles_projected: usize,
    pub(crate) omitted_anchors: usize,
    pub(crate) omitted_rectangles: usize,
    pub(crate) missing_block_ids: usize,
    pub(crate) complete: bool,
}

pub(crate) struct Projection {
    pub(crate) triples: Vec<Triple>,
    pub(crate) summary: Summary,
}

pub(crate) fn sink(graph_id: &str) -> String {
    format!(
        "{}:projection:pdf-source",
        crate::rdf::graph_subject(graph_id)
    )
}

pub(crate) fn require_not_authored_subject(graph_id: &str, candidate: &str) -> Result<(), String> {
    if candidate.starts_with(&format!("{}:", sink(graph_id))) {
        return Err("PDF source objects are derived; edit the source document instead".into());
    }
    Ok(())
}

fn local_prefix(document: &DocumentRecord) -> String {
    format!("document-{}", wire::sha256(&document.document_id))
}

fn subject(graph_id: &str, local_id: &str) -> String {
    format!("{}:{local_id}", sink(graph_id))
}

fn record(document: &DocumentRecord, kind: &str, local_id: &str) -> Value {
    json!({"kind":kind,"localId":local_id,"document":document.rdf_subject,
        "documentId":document.document_id})
}

fn payload_digest(value: &Value) -> String {
    // Strings hash their EXACT stored UTF-8 bytes, not JSON-quoted bytes.
    if let Some(raw) = value.as_str() {
        return wire::sha256(raw);
    }
    struct HashWriter(sha2::Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            use sha2::Digest;
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    use sha2::Digest;
    let mut writer = HashWriter(sha2::Sha256::new());
    // Value serialization into this infallible sink avoids a huge second copy
    // of malformed object-valued metadata. It does not replace the source.
    serde_json::to_writer(&mut writer, value).expect("JSON Value serializes into hash sink");
    format!("{:x}", writer.0.finalize())
}

pub(crate) fn project(document: &DocumentRecord) -> Result<Projection, String> {
    let contract = get_vocabulary(PACK).ok_or("PDF vocabulary is not registered")?;
    let mut summary = Summary {
        complete: true,
        ..Summary::default()
    };
    let mut records = Vec::new();
    let mut anchors = Vec::new();
    let mut ids = BTreeMap::<String, usize>::new();
    let mut stack: Vec<&Value> = document.tiptap_json.as_ref().into_iter().collect();
    // Traverse existing source without allocating typed geometry past budgets.
    while let Some(node) = stack.pop() {
        if let Some(children) = node.get("content").and_then(Value::as_array) {
            stack.extend(children.iter().rev());
        }
        let Some(raw) = node
            .get("attrs")
            .and_then(|a| a.get(wire::ATTRIBUTE))
            .filter(|v| !v.is_null())
        else {
            continue;
        };
        summary.anchors_seen = summary.anchors_seen.saturating_add(1);
        let Some(block_id) = node
            .get("attrs")
            .and_then(|a| a.get("data-block-id"))
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 200
                    && crate::emporium::query_engine::validate_sparql_iri("block id", id).is_ok()
            })
        else {
            summary.missing_block_ids += 1;
            summary.complete = false;
            continue;
        };
        if anchors.len() >= MAX_ANCHORS {
            summary.omitted_anchors += 1;
            summary.complete = false;
            continue;
        }
        *ids.entry(block_id.to_string()).or_default() += 1;
        anchors.push((node, raw, block_id));
    }
    let prefix = local_prefix(document);
    let mut emitted_originals = BTreeSet::new();
    let mut emitted_anchors = BTreeSet::new();
    for (node, raw, block_id) in anchors {
        let local = format!("{prefix}:anchor:{}", wire::sha256(block_id));
        if !emitted_anchors.insert(local.clone()) {
            summary.omitted_anchors += 1;
            summary.complete = false;
            continue;
        }
        let mut out = record(document, "TextSourceAnchor", &local);
        let current_hash = wire::text_sha256(node);
        out["textBlock"] = json!(format!("{}#block-{block_id}", document.rdf_subject));
        out["blockId"] = json!(block_id);
        out["payloadSha256"] = json!(payload_digest(raw));
        out["currentTextSha256"] = json!(current_hash);
        out["editedText"] = json!(false);
        let inspection = if ids.get(block_id).copied().unwrap_or(0) > 1 {
            Inspection::Invalid("duplicate block identity cannot identify one source passage")
        } else {
            wire::inspect(Some(raw))
        };
        match inspection {
            Inspection::Absent => continue,
            Inspection::Invalid(reason) | Inspection::Unsupported(reason) => {
                out["mappingStatus"] = json!(if matches!(inspection, Inspection::Unsupported(_)) {
                    "unsupported"
                } else {
                    "invalid"
                });
                out["reason"] = json!(reason);
            }
            Inspection::Mapped(anchor) => {
                let rectangle_count: usize = anchor.targets.iter().map(|t| t.rects.len()).sum();
                if summary.rectangles_projected.saturating_add(rectangle_count) > MAX_DOCUMENT_RECTS
                {
                    summary.omitted_rectangles =
                        summary.omitted_rectangles.saturating_add(rectangle_count);
                    summary.complete = false;
                    out["mappingStatus"] = json!("budget-exceeded");
                    out["reason"] =
                        json!("valid anchor exceeds the document projection rectangle budget");
                } else {
                    out["mappingStatus"] = json!("mapped");
                    out["oa:hasBody"] = out["textBlock"].clone();
                    out["textSha256"] = json!(anchor.text_sha256);
                    out["editedText"] = json!(current_hash != anchor.text_sha256);
                    out["approach"] = json!(anchor.extraction.approach);
                    out["extractorVersion"] = json!(anchor.extraction.version);
                    let original_key = wire::sha256(format!(
                        "{}\n{}\n{}\n{}",
                        anchor.source.sha256,
                        anchor.source.byte_length,
                        anchor.source.artifact_id.as_deref().unwrap_or(""),
                        anchor.source.revision_id.as_deref().unwrap_or("")
                    ));
                    let original_local = format!("{prefix}:original:{original_key}");
                    let original_iri = subject(&document.graph_id, &original_local);
                    out["prov:wasDerivedFrom"] = json!(original_iri);
                    if emitted_originals.insert(original_local.clone()) {
                        let mut original = record(document, "PdfOriginal", &original_local);
                        original["sha256"] = json!(anchor.source.sha256);
                        original["byteLength"] = json!(anchor.source.byte_length as i64);
                        original["attestation"] = json!("declared-source-unverified");
                        if let Some(id) = anchor.source.artifact_id {
                            original["artifactId"] = json!(id);
                        }
                        if let Some(id) = anchor.source.revision_id {
                            original["revisionId"] = json!(id);
                        }
                        records.push(original);
                    }
                    let mut targets = Vec::new();
                    for (target_index, target) in anchor.targets.iter().enumerate() {
                        for (rect_index, rect) in target.rects.iter().enumerate() {
                            // Generic Emporium's numeric diff is six-decimal
                            // canonical. A selector is immutable to its exact raw
                            // anchor, so sub-micro geometry edits get a new subject.
                            let region_local = format!(
                                "{local}:source-{}:region:{target_index}:{rect_index}",
                                out["payloadSha256"].as_str().expect("digest")
                            );
                            let selector_local = format!("{region_local}:selector");
                            let mut region = record(document, "PdfRegion", &region_local);
                            region["oa:hasSource"] = json!(original_iri);
                            region["oa:hasSelector"] =
                                json!(subject(&document.graph_id, &selector_local));
                            records.push(region);
                            let mut selector = record(document, "PdfPageSelector", &selector_local);
                            selector["pageIndex"] = json!(target.page_index as i64);
                            selector["space"] = json!(target.space);
                            selector["userUnit"] = json!(target.user_unit);
                            selector["rotation"] = json!(target.rotation as i64);
                            for (field, value) in ["cropXMin", "cropYMin", "cropXMax", "cropYMax"]
                                .iter()
                                .zip(target.view_box)
                            {
                                selector[*field] = json!(value);
                            }
                            for (field, value) in ["xMin", "yMin", "xMax", "yMax"].iter().zip(rect)
                            {
                                selector[*field] = json!(value);
                            }
                            records.push(selector);
                            targets.push(subject(&document.graph_id, &region_local));
                        }
                    }
                    out["oa:hasTarget"] = json!(targets);
                    summary.rectangles_projected += rectangle_count;
                }
            }
        }
        summary.anchors_projected += 1;
        records.push(out);
    }
    // Every retained anchor exposes document-wide completeness without inventing
    // a fifth class or an anchor on a nonexistent block. Missing identities are
    // also loud in the bounded summary (there may be no real block to identify).
    for record in &mut records {
        if record["kind"] == "TextSourceAnchor" {
            record["projectionComplete"] = json!(summary.complete);
            record["omittedAnchors"] = json!(summary.omitted_anchors + summary.missing_block_ids);
            record["omittedRectangles"] = json!(summary.omitted_rectangles);
        }
    }
    // The generic planner enforces declared fields and identity, but its legacy
    // string mint strips CR and its double mint rounds to six decimal places.
    // Keep native PDF literals exact without changing other vocabularies.
    let mut exact_literals = BTreeMap::new();
    for record in &records {
        let kind = record["kind"].as_str().ok_or("PDF record has no class")?;
        let id = record["localId"]
            .as_str()
            .ok_or("PDF record has no identity")?;
        for (curie, spec) in &contract.classes[kind].predicates {
            let local = curie.split_once(':').map(|(_, v)| v).unwrap_or(curie);
            let Some(value) = record.get(curie).or_else(|| record.get(local)) else {
                continue;
            };
            let literal = match spec.datatype {
                crate::emporium::contract::Datatype::double => {
                    Some(oxigraph::model::Literal::new_typed_literal(
                        value
                            .as_f64()
                            .ok_or("PDF double value is not numeric")?
                            .to_string(),
                        oxigraph::model::NamedNode::new("http://www.w3.org/2001/XMLSchema#double")
                            .expect("XSD"),
                    ))
                }
                crate::emporium::contract::Datatype::string => {
                    Some(oxigraph::model::Literal::new_simple_literal(
                        value.as_str().ok_or("PDF string value is not a string")?,
                    ))
                }
                _ => None,
            };
            if let Some(literal) = literal {
                exact_literals.insert(
                    (subject(&document.graph_id, id), contract.expand(curie)?),
                    literal,
                );
            }
        }
    }
    let records: Vec<GenericRecordIn> = records
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("PDF typed record: {e}"))?;
    let mut plan = plan_generic_compute(contract, &document.graph_id, &records).map_err(|e| e.0)?;
    for (s, p, o) in &mut plan.desired_inserts {
        if let Some(literal) = exact_literals.get(&(s.clone(), p.clone())) {
            *o = Term::Lit(literal.clone());
        }
    }
    // A custom diagnostic is not an OA Annotation. Conditional annotation
    // typing is emitted only where a real, nonempty target set exists.
    let mapped: BTreeSet<_> = plan
        .desired_inserts
        .iter()
        .filter_map(|(s, p, _)| (p == "http://www.w3.org/ns/oa#hasTarget").then(|| s.clone()))
        .collect();
    for subject in mapped {
        plan.desired_inserts.push((
            subject,
            crate::runtime_config::RDF_TYPE.into(),
            Term::Uri(
                oxigraph::model::NamedNode::new("http://www.w3.org/ns/oa#Annotation")
                    .expect("OA IRI"),
            ),
        ));
    }
    Ok(Projection {
        triples: plan.desired_inserts,
        summary,
    })
}

fn reconcile_desired(
    store: &Store,
    graph_id: &str,
    document_id: &str,
    desired: &[Triple],
) -> Result<TripleDiff, String> {
    let contract = get_vocabulary(PACK).ok_or("PDF vocabulary is not registered")?;
    let mut scopes = Vec::new();
    for class in CLASSES {
        let rdf_type = format!("{NS}{class}");
        scopes.push(ClassScope {
            placement: Placement::Named(sink(graph_id)),
            key: SpanKey::Fixed { rdf_type },
            graph_id_conjunct: Some((format!("{NS}documentId"), document_id.to_string())),
            subjects: None,
        });
    }
    // Reuse Emporium's declared validation, owned-span survey and write engine,
    // but NOT its six-decimal numeric equivalence. Exact PDF geometry must also
    // repair a sub-micro corruption on an unchanged selector subject.
    crate::emporium::shacl_validator::validate_desired(desired, contract)?;
    let mut current = Vec::new();
    for scope in &scopes {
        current.extend(survey_class(store, scope)?);
    }
    let before: BTreeMap<_, _> = current.into_iter().map(|t| (exact_key(&t), t)).collect();
    let after: BTreeMap<_, _> = desired
        .iter()
        .cloned()
        .map(|t| (exact_key(&t), t))
        .collect();
    let diff = TripleDiff {
        removes: before
            .iter()
            .filter(|(k, _)| !after.contains_key(*k))
            .map(|(_, v)| v.clone())
            .collect(),
        adds: after
            .iter()
            .filter(|(k, _)| !before.contains_key(*k))
            .map(|(_, v)| v.clone())
            .collect(),
    };
    if !diff.is_empty() {
        apply_diff(store, &Placement::Named(sink(graph_id)), &diff)?;
    }
    if !diff.is_empty() {
        crate::cell_durability::mark_rdf_store_written(store);
    }
    Ok(diff)
}

fn exact_key((s, p, o): &Triple) -> (String, String, String) {
    let value = match o {
        // Oxigraph canonicalizes xsd:long to xsd:integer on store round-trip
        // (the existing terms module documents this). Preserve exact integer
        // values without its float-based six-decimal numeric equivalence.
        Term::Lit(literal)
            if matches!(
                literal.datatype().as_str(),
                "http://www.w3.org/2001/XMLSchema#integer"
                    | "http://www.w3.org/2001/XMLSchema#long"
            ) =>
        {
            literal
                .value()
                .parse::<i128>()
                .map(|v| format!("integer:{v}"))
                .unwrap_or_else(|_| o.as_nt())
        }
        Term::Lit(literal)
            if literal.datatype().as_str() == "http://www.w3.org/2001/XMLSchema#double" =>
        {
            match literal.value().parse::<f64>() {
                Ok(value) if value.is_finite() => format!(
                    "double:{:016x}",
                    if value == 0.0 {
                        0.0f64.to_bits()
                    } else {
                        value.to_bits()
                    }
                ),
                _ => o.as_nt(),
            }
        }
        _ => o.as_nt(),
    };
    (s.clone(), p.clone(), value)
}

pub(crate) fn reconcile(store: &Store, document: &DocumentRecord) -> Result<Summary, String> {
    #[cfg(test)]
    fail_if_requested(&document.graph_id, &document.document_id)?;
    let projection = project(document)?;
    reconcile_desired(
        store,
        &document.graph_id,
        &document.document_id,
        &projection.triples,
    )?;
    if !projection.summary.complete {
        log::warn!(
            "pdf_source_projection_incomplete graph={} document={} summary={:?}",
            document.graph_id,
            document.document_id,
            projection.summary
        );
    }
    Ok(projection.summary)
}

pub(crate) fn remove_document(
    store: &Store,
    graph_id: &str,
    document_id: &str,
) -> Result<(), String> {
    reconcile_desired(store, graph_id, document_id, &[])?;
    Ok(())
}

#[cfg(test)]
static FAIL_ONCE: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub(crate) fn fail_next_for_test(graph_id: &str, document_id: &str) {
    *FAIL_ONCE.lock().unwrap() = Some((graph_id.into(), document_id.into()));
}
#[cfg(test)]
fn fail_if_requested(graph_id: &str, document_id: &str) -> Result<(), String> {
    let mut target = FAIL_ONCE.lock().unwrap();
    if target
        .as_ref()
        .is_some_and(|(g, d)| g == graph_id && d == document_id)
    {
        *target = None;
        return Err("injected PDF projection I/O failure".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "pdf_source_tests.rs"]
mod tests;

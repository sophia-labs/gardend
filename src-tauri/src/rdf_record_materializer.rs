use crate::{
    document_service::DocumentRecord,
    emporium::survey::parse_term,
    emporium::terms::{diff_triples, render_updates, Triple, TripleDiff},
    graph_service::GraphRecord,
    rdf::{
        format_rdf_triple, graph_subject, push_string_triple, push_uri_triple,
        sparql_string_literal, RdfTriple,
    },
    rdf_authority::{document_projection_graph_iri, graph_projection_graph_iri},
    rdf_document_tree::{document_tree_triples, DOCUMENT_LEVEL_PREDICATES},
    runtime_config::{DCTERMS_NS, MNEMO_NS, RDF_TYPE},
};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;

pub(super) fn materialize_graph_record(store: &Store, graph: &GraphRecord) -> Result<(), String> {
    let subject = graph_subject(&graph.graph_id);
    let authority_graph = graph_projection_graph_iri(&graph.graph_id);
    let update = format!(
        r#"
PREFIX dcterms: <{DCTERMS_NS}>
PREFIX mnemo: <{MNEMO_NS}>
DELETE {{ GRAPH <{authority_graph}> {{ <{subject}> ?p ?o . }} }}
INSERT {{
  GRAPH <{authority_graph}> {{
    <{subject}> a mnemo:LocalGraph ;
      dcterms:identifier {graph_id} ;
      dcterms:title {title} ;
      dcterms:created {created_at} ;
      dcterms:modified {updated_at} ;
      mnemo:origin {origin} ;
      mnemo:providerId {provider_id} ;
      mnemo:localPath {local_path} .
  }}
}}
WHERE {{ OPTIONAL {{ GRAPH <{authority_graph}> {{ <{subject}> ?p ?o . }} }} }}
"#,
        graph_id = sparql_string_literal(&graph.graph_id),
        title = sparql_string_literal(&graph.title),
        created_at = sparql_string_literal(&graph.created_at),
        updated_at = sparql_string_literal(&graph.updated_at),
        origin = sparql_string_literal(&graph.origin),
        provider_id = sparql_string_literal(&graph.provider_id),
        local_path = sparql_string_literal(&graph.local_path),
    );
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse graph materialization update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("materialize graph metadata: {error}"))
}

// ════════════════════════════════════════════════════════════════════════════
//  GRAPH METADATA as a Meaningful Object — the engine reconcile.
//
//  The first dividend of the `proto/meaningful-objects` Lean proof, cashed for
//  the SIMPLEST kind. Mirrors `reconcile_document_record`
//  (document_meaningful_object.rs:702): declare the kind's OWNED SPAN, survey it,
//  diff against the DESIRED set, apply only the delta, return the op count.
//
//  The graph-metadata span (Lean `Vocab.FileSystem.graphMetaSpan`) is the
//  SIMPLEST owned span in the whole ontology: a SINGLE subject
//  (`graph_subject(graph_id)`) over an UNRESTRICTED-bare predicate set, in the
//  per-graph projection graph (`graph_projection_graph_iri`). Unlike the
//  Document face (which co-manages a fixed bare-predicate allowlist on a SHARED
//  subject), the graph subject is OWNED OUTRIGHT — no other projection writes it
//  — so the survey/DELETE reclaims `<{subject}> ?p ?o` with a FREE `?p`,
//  exactly as the old wholesale `materialize_graph_record` DELETE does
//  (rdf_record_materializer.rs:18). That makes this a PURE PARITY swap: the
//  reconcile's DESIRED set is byte-for-byte the same 8 triples the old
//  materializer INSERTs, reached by value-diff instead of teardown-and-rebuild.
//  A converged save emits 0 ops; a metadata edit emits only the changed slots.
//
//  Reuses the proven engine: `diff_triples` → `render_updates` → graph-wrap +
//  `SparqlEvaluator` on the `&Store`. Like the materializer it BYPASSES the
//  authority gate (which reserves every `:projection:` graph).
// ════════════════════════════════════════════════════════════════════════════

/// project(source)->desired for the graph-metadata Meaningful Object: the EXACT
/// 8 triples `materialize_graph_record` INSERTs, built INDEPENDENTLY from the
/// `GraphRecord` fields + the namespace constants (the Rust image of Lean
/// `graphMetaTriples`), then adapted to engine `Triple`s through the SAME
/// `RdfTriple` → `format_rdf_triple` → `parse_term` round-trip the Document MO
/// uses — so term serialization is byte-identical to what the store round-trips
/// and the value-diff never reads parity as drift.
///
/// All 8 land on the SINGLE bare graph subject `graph_subject(graph_id)`:
/// `a mnemo:LocalGraph` (URI object) + the 7 metadata literals
/// (`dcterms:{identifier,title,created,modified}` + `mnemo:{origin,providerId,
/// localPath}`). `created`/`modified` are SIMPLE string literals (no
/// `^^xsd:dateTime`), matching the materializer's `sparql_string_literal`.
fn graph_desired_triples(graph: &GraphRecord) -> Vec<Triple> {
    let subject = graph_subject(&graph.graph_id);
    let mut raw: Vec<RdfTriple> = Vec::with_capacity(8);

    // a mnemo:LocalGraph  — rdf:type → the LocalGraph class IRI (URI object).
    push_uri_triple(
        &mut raw,
        &subject,
        RDF_TYPE,
        &format!("{MNEMO_NS}LocalGraph"),
    );
    // dcterms:identifier "{graph_id}"
    push_string_triple(
        &mut raw,
        &subject,
        &format!("{DCTERMS_NS}identifier"),
        &graph.graph_id,
    );
    // dcterms:title "{title}"
    push_string_triple(
        &mut raw,
        &subject,
        &format!("{DCTERMS_NS}title"),
        &graph.title,
    );
    // dcterms:created "{created_at}"  (simple literal, NOT xsd:dateTime)
    push_string_triple(
        &mut raw,
        &subject,
        &format!("{DCTERMS_NS}created"),
        &graph.created_at,
    );
    // dcterms:modified "{updated_at}"  (simple literal, NOT xsd:dateTime)
    push_string_triple(
        &mut raw,
        &subject,
        &format!("{DCTERMS_NS}modified"),
        &graph.updated_at,
    );
    // mnemo:origin "{origin}"
    push_string_triple(
        &mut raw,
        &subject,
        &format!("{MNEMO_NS}origin"),
        &graph.origin,
    );
    // mnemo:providerId "{provider_id}"
    push_string_triple(
        &mut raw,
        &subject,
        &format!("{MNEMO_NS}providerId"),
        &graph.provider_id,
    );
    // mnemo:localPath "{local_path}"
    push_string_triple(
        &mut raw,
        &subject,
        &format!("{MNEMO_NS}localPath"),
        &graph.local_path,
    );

    raw.iter().map(rdf_triple_to_term).collect()
}

/// Survey the graph subject's UNRESTRICTED bare span in the per-graph projection
/// graph: every `?p ?o` on `<{graph_subject}>`. The graph subject is owned
/// OUTRIGHT (no co-managed allowlist), so the survey scope is `?p` FREE — the
/// same span the old materializer's DELETE reclaims. Returns `current` via the
/// proven oxigraph-`term.to_string()` → `parse_term` round-trip.
fn survey_graph_metadata(store: &Store, graph_id: &str) -> Result<Vec<Triple>, String> {
    let authority_graph = graph_projection_graph_iri(graph_id);
    let subject = graph_subject(graph_id);
    let query = format!(
        r#"SELECT ?p ?o WHERE {{
  GRAPH <{authority_graph}> {{
    <{subject}> ?p ?o .
  }}
}}"#
    );

    let solutions = match SparqlEvaluator::new()
        .parse_query(&query)
        .map_err(|e| format!("parse graph survey: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute graph survey: {e}"))?
    {
        QueryResults::Solutions(s) => s,
        _ => return Err("graph survey expected SELECT solutions".to_string()),
    };

    let mut out = Vec::new();
    for sol in solutions {
        let sol = sol.map_err(|e| format!("graph survey row: {e}"))?;
        let p = match sol.get("p").ok_or("graph survey row missing ?p")? {
            oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
            other => return Err(format!("expected a NamedNode predicate, got {other}")),
        };
        let o = parse_term(
            &sol.get("o")
                .ok_or("graph survey row missing ?o")?
                .to_string(),
        );
        out.push((subject.clone(), p, o));
    }
    Ok(out)
}

/// Apply one already-rendered `INSERT DATA` / `DELETE DATA` body against the
/// per-graph projection graph, GRAPH-wrapping it into
/// `graph_projection_graph_iri` first. Port of the Document MO's
/// `run_document_update` + `graph_wrap_document`, retargeted to the graph
/// projection graph. DIRECT-ON-STORE; bypasses the authority gate.
fn run_graph_update(store: &Store, graph_id: &str, body: &str) -> Result<(), String> {
    let authority_graph = graph_projection_graph_iri(graph_id);
    let wrapped = graph_wrap_update(body, &authority_graph)?;
    SparqlEvaluator::new()
        .parse_update(&wrapped)
        .map_err(|e| format!("parse graph metadata update: {e}"))?
        .on_store(store)
        .execute()
        .map_err(|e| format!("execute graph metadata update: {e}"))
}

/// `INSERT/DELETE DATA { body }` → the same verb wrapped in
/// `GRAPH <{authority_graph}> { body }`. Port of the Document MO's
/// `graph_wrap_document`: match the leading verb, take the body between the
/// FIRST `{` and the LAST `}`, re-emit wrapped. `render_updates` satisfies this
/// shape.
fn graph_wrap_update(update: &str, authority_graph: &str) -> Result<String, String> {
    let trimmed = update.trim_start();
    let (verb_word, after) = if let Some(rest) = trimmed.strip_prefix("INSERT DATA") {
        ("INSERT DATA", rest)
    } else if let Some(rest) = trimmed.strip_prefix("DELETE DATA") {
        ("DELETE DATA", rest)
    } else {
        return Err(format!(
            "unexpected graph update shape: {}",
            &update.chars().take(80).collect::<String>()
        ));
    };
    let open = after
        .find('{')
        .ok_or_else(|| "graph update missing opening brace".to_string())?;
    let close = update
        .rfind('}')
        .ok_or_else(|| "graph update missing closing brace".to_string())?;
    let body_start = update.len() - after.len() + open + 1;
    if body_start > close {
        return Err("graph update has empty/invalid body span".to_string());
    }
    let body = &update[body_start..close];
    Ok(format!(
        "{verb_word} {{ GRAPH <{authority_graph}> {{\n{body}\n}} }}"
    ))
}

/// Adapt one [`RdfTriple`] → engine `(s, p, Term)` via the proven
/// `format_rdf_triple` → split → `parse_term` round-trip (the same adapter the
/// Document MO uses). `format_rdf_triple(&t)` emits `<s> <p> OBJ .`; we split off
/// `s`, `p` and feed `OBJ` to `parse_term`, which consumes `<uri>` / `"lit"`
/// exactly as `format_rdf_triple` emits them.
pub(crate) fn rdf_triple_to_term(t: &RdfTriple) -> Triple {
    let line = format_rdf_triple(t);
    let rest = line.trim();
    let (subject, rest) = split_angle_iri(rest)
        .unwrap_or_else(|| panic!("malformed serialized triple subject: {line}"));
    let (predicate, rest) = split_angle_iri(rest.trim_start())
        .unwrap_or_else(|| panic!("malformed serialized triple predicate: {line}"));
    let object_nt = rest.trim().strip_suffix('.').unwrap_or(rest).trim();
    let object = parse_term(object_nt);
    (subject, predicate, object)
}

/// Split a leading `<iri>` token, returning `(iri, remainder)`.
fn split_angle_iri(s: &str) -> Option<(String, &str)> {
    let s = s.strip_prefix('<')?;
    let close = s.find('>')?;
    Some((s[..close].to_string(), &s[close + 1..]))
}

/// ADDITIVE entry point: reconcile the per-graph projection graph to the
/// `GraphRecord`'s declared graph-metadata footprint by VALUE-DIFF — the
/// engine-reconcile counterpart of `materialize_graph_record`. Surveys the bare
/// graph subject's UNRESTRICTED span (`<{subject}> ?p ?o`), diffs against the 8
/// DESIRED graph-metadata triples ([`graph_desired_triples`]), and applies only
/// the delta direct-on-store via [`run_graph_update`]. Returns the structured
/// [`TripleDiff`] it applied — a converged reconcile returns an empty diff; a
/// metadata edit returns only the changed slots. The op count
/// (`removes + adds`, via [`TripleDiff::op_count`]) is one face of it, projected
/// at the consumer.
///
/// PURE PARITY (not a superset, unlike the Document MO): the desired set is
/// EXACTLY the 8 triples the old wholesale path INSERTs, and the survey span is
/// EXACTLY the span the old path DELETEs (`?p` free over the single owned
/// subject). So the converged net projection is byte-identical to
/// `materialize_graph_record`'s — the only difference is the MINIMAL delta.
pub(super) fn reconcile_graph_record(
    store: &Store,
    graph: &GraphRecord,
) -> Result<TripleDiff, String> {
    let current = survey_graph_metadata(store, &graph.graph_id)?;
    let desired = graph_desired_triples(graph);

    let diff = diff_triples(&current, &desired);

    // DELETE first, then INSERT (the materializer's order; graph-agnostic bodies).
    for body in render_updates("DELETE DATA", &diff.removes, 60) {
        run_graph_update(store, &graph.graph_id, &body)?;
    }
    for body in render_updates("INSERT DATA", &diff.adds, 60) {
        run_graph_update(store, &graph.graph_id, &body)?;
    }

    // `run_graph_update` executes directly against `&Store` (a low-level
    // materializer that "intentionally accepts only &Store", per
    // `GraphPersistenceLease`'s doc comment) and so never reaches the narrow
    // `mark_rdf_store_written` hook that `rdf_query_service::execute_sparql_update`
    // calls for ordinary SPARQL-update writes. Close that precision gap here —
    // mirroring the identical fix already made for the Observatory catch_up
    // materializer (`observatory::apply::catch_up`) — gated on a real diff so a
    // converged (no-op) reconcile stays flush-invisible at the narrow-hook
    // layer. This is purely ADDITIVE: the enclosing `GraphPersistenceLease`'s
    // coarse per-graph fallback (`mark_graph_rdf_stores_written_with_context`,
    // fired on `Drop` unless the caller proved the whole scope read-only)
    // still marks the store dirty regardless of this call, so
    // removing/misjudging this gate can only ever make the signal LESS
    // precise, never false-CLEAN.
    if !diff.is_empty() {
        crate::cell_durability::mark_rdf_store_written(store);
    }

    Ok(diff)
}

pub(super) fn materialize_document_record(
    store: &Store,
    document: &DocumentRecord,
) -> Result<(), String> {
    let tree_triples = document_tree_triples(document);
    materialize_document_record_with_triples(store, document, &tree_triples)
}

pub(super) fn materialize_document_record_with_triples(
    store: &Store,
    document: &DocumentRecord,
    tree_triples: &[RdfTriple],
) -> Result<(), String> {
    let subject = &document.rdf_subject;
    let authority_graph = document_projection_graph_iri(&document.graph_id, &document.document_id);
    let tree_insert = tree_triples
        .iter()
        .map(format_rdf_triple)
        .collect::<Vec<_>>()
        .join("\n  ");
    let insert_block = if tree_insert.is_empty() {
        String::new()
    } else {
        format!(
            r#";
INSERT DATA {{
  GRAPH <{authority_graph}> {{
  {tree_insert}
  }}
}}"#
        )
    };
    // The bare-`<subject>` predicate allowlist is the Document projection's
    // DECLARED vocabulary, owned by the projection itself
    // (`DOCUMENT_LEVEL_PREDICATES` in `rdf_document_tree`). Generate the
    // `VALUES ?local_p { … }` block from it so this DELETE span and the
    // Meaningful Object's owned-span share ONE source of truth.
    let local_p_values = DOCUMENT_LEVEL_PREDICATES
        .iter()
        .map(|local| format!("mnemo:{local}"))
        .collect::<Vec<_>>()
        .join("\n      ");
    // TWO SINGLE-PATTERN DELETE ops — NEVER one op with both spans. The old
    // shape put `OPTIONAL {{ tree span }} OPTIONAL {{ doc-level span }}` in
    // ONE WHERE with a DELETE template carrying BOTH patterns. The two
    // OPTIONALs bind DISJOINT variables, so SPARQL left-join semantics yield
    // the CARTESIAN PRODUCT: N tree rows x M doc-level rows (M up to the
    // full predicate allowlist), and oxigraph's update evaluator materializes
    // the whole thing eagerly — every one of the N*M solution rows PLUS
    // 2*N*M instantiated delete quads collected into one Vec before any
    // mutation applies (oxigraph src/sparql/update.rs eval_delete_insert).
    // That is ~7.6KB of transient allocation PER TREE TRIPLE ALREADY IN THE
    // STORE: a document projection graph that has accreted to ~1.7M triples
    // detonates ~13GB in a single tight collect loop — gigabytes per second,
    // zero log output; measured live killing a 14Gi cell in under 30s on the
    // canary graph's first post-hydration SPARQL query (2026-07-22). Split
    // into two ops the same walk deletes N + M rows / N + M quads instead —
    // a per-triple constant of ~450B, ~20-30x smaller — and reclaims exactly
    // the same spans: tree teardown and doc-level teardown never needed each
    // other's bindings in the first place.
    let update = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
DELETE {{
  GRAPH <{authority_graph}> {{
  ?tree_subject ?tree_p ?tree_o .
  }}
}}
WHERE {{
  GRAPH <{authority_graph}> {{
  ?tree_subject ?tree_p ?tree_o .
  FILTER(STRSTARTS(STR(?tree_subject), "{subject}#"))
  }}
}} ;
DELETE {{
  GRAPH <{authority_graph}> {{
  <{subject}> ?local_p ?local_o .
  }}
}}
WHERE {{
  GRAPH <{authority_graph}> {{
  VALUES ?local_p {{
      {local_p_values}
  }}
  <{subject}> ?local_p ?local_o .
  }}
}}{insert_block}
"#,
    );
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse document materialization update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("materialize document metadata: {error}"))?;
    // DURABILITY AUDIT FINDING (2026-07-18): this DELETE+INSERT always executes
    // directly against `&Store`, bypassing the narrow `mark_rdf_store_written`
    // hook entirely, with no upfront diff to gate on (unlike
    // `reconcile_graph_record`, this function has no survey-then-diff shape —
    // computing one safely would need a new survey query, deferred rather than
    // risked here). Some real production callers of this function reach it with
    // NO covering `GraphPersistenceLease`/write-gate at all (confirmed:
    // `rdf_seed_service::ensure_graph_store_seeded`'s document-reseed loop, and
    // Geist's memory/song archive-projection paths) — an acknowledged write that
    // was previously invisible to the durable flush. Mark unconditionally (the
    // safe direction: this executes a real DELETE+INSERT every call, so treating
    // it as dirty every time cannot be a false-CLEAN, only an occasional
    // no-op-content false-DIRTY).
    crate::cell_durability::mark_rdf_store_written(store);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // `RDF_TYPE` is brought in by `use super::*` (it is a module-level import now),
    // so it must NOT be re-imported here (that would be a duplicate-import error).
    use crate::{
        document_service::{DocumentTreeSnapshot, TreeNodeAttributes, TreeNodeSnapshot},
        rdf::document_subject,
        rdf_query_service::execute_sparql_query,
        runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    };

    fn graph_record(graph_id: &str) -> GraphRecord {
        GraphRecord {
            graph_id: graph_id.to_string(),
            title: "Graph A".to_string(),
            description: None,
            status: "active".to_string(),
            origin: "local".to_string(),
            provider_id: "local-profile".to_string(),
            local_path: format!("/tmp/{graph_id}"),
            created_at: "1".to_string(),
            incarnation_id: None,
            updated_at: "2".to_string(),
            capabilities: Vec::new(),
            created_by_operation_id: None,
            validation_policy: crate::runtime_config::ValidationPolicy::default(),
            content_revision: None,
        }
    }

    fn document_record(graph_id: &str, document_id: &str) -> DocumentRecord {
        DocumentRecord {
            document_id: document_id.to_string(),
            graph_id: graph_id.to_string(),
            title: "Document A".to_string(),
            revision: 1,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{document_id}"),
            rdf_subject: document_subject(document_id),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: Some(DocumentTreeSnapshot {
                doc_id: document_id.to_string(),
                root: TreeNodeSnapshot {
                    kind: "element".to_string(),
                    tag_name: Some("doc".to_string()),
                    text_content: None,
                    attributes: TreeNodeAttributes::default(),
                    children: vec![TreeNodeSnapshot {
                        kind: "element".to_string(),
                        tag_name: Some("paragraph".to_string()),
                        text_content: None,
                        attributes: TreeNodeAttributes {
                            block_id: Some("block-a".to_string()),
                            ..TreeNodeAttributes::default()
                        },
                        children: vec![TreeNodeSnapshot {
                            kind: "text".to_string(),
                            tag_name: None,
                            text_content: Some("Hello".to_string()),
                            attributes: TreeNodeAttributes::default(),
                            children: Vec::new(),
                        }],
                    }],
                },
            }),
            blocks: Vec::new(),
            rdf_triple_count: 0,
        }
    }

    /// Parity pin for the split teardown (the anti-cartesian rewrite): the
    /// two single-pattern DELETE ops must reclaim EXACTLY the spans the old
    /// combined OPTIONAL x OPTIONAL op reclaimed — the `{subject}#…` tree
    /// span and the allowlisted bare-subject predicates — while leaving (a)
    /// co-managed bare-subject predicates OUTSIDE the allowlist and (b)
    /// foreign subjects in the same projection graph untouched. Real store,
    /// real materializer, real SPARQL read-back.
    #[test]
    fn document_materialization_split_teardown_reclaims_same_spans_without_cartesian() {
        let store = Store::new().expect("store");
        let document = document_record("graph-a", "doc-a");
        let authority_graph = document_projection_graph_iri("graph-a", "doc-a");
        let subject = document_subject("doc-a");

        // Seed the graph with: a stale tree node, an allowlisted doc-level
        // predicate, a NON-allowlisted (co-managed) doc-level predicate, and
        // a foreign subject that merely lives in the same named graph.
        SparqlEvaluator::new()
            .parse_update(&format!(
                r#"
INSERT DATA {{
  GRAPH <{authority_graph}> {{
    <{subject}#stale-node> <http://mnemosyne.dev/doc#textContent> "stale tree text" .
    <{subject}> <{MNEMO_NS}documentId> "stale-doc-id" .
    <{subject}> <{MNEMO_NS}coManagedNotInAllowlist> "must survive" .
    <urn:foreign:subject> <urn:p> "also must survive" .
  }}
}}
"#
            ))
            .expect("parse seed insert")
            .on_store(&store)
            .execute()
            .expect("seed stale spans");

        // REAL production fn over the dirty graph.
        materialize_document_record(&store, &document).expect("materialize document");

        let read = |pattern: &str| -> bool {
            execute_sparql_query(
                &store,
                &format!("ASK {{ GRAPH <{authority_graph}> {{ {pattern} }} }}"),
            )
            .expect("ask")
            .boolean
                == Some(true)
        };

        // Tree span: stale node GONE, fresh tree present.
        assert!(
            !read(&format!(r#"<{subject}#stale-node> ?p ?o"#)),
            "the stale {{subject}}# tree span must be reclaimed"
        );
        assert!(
            read(&format!(
                r#"<{subject}#frag> <{MNEMO_NS}documentId> "doc-a""#
            )),
            "the fresh tree projection must be inserted"
        );
        // Allowlisted doc-level span: stale value GONE (documentId is in
        // DOCUMENT_LEVEL_PREDICATES).
        assert!(
            !read(&format!(
                r#"<{subject}> <{MNEMO_NS}documentId> "stale-doc-id""#
            )),
            "allowlisted bare-subject predicates must be reclaimed"
        );
        // Co-managed (non-allowlisted) predicate and foreign subject SURVIVE.
        assert!(
            read(&format!(
                r#"<{subject}> <{MNEMO_NS}coManagedNotInAllowlist> "must survive""#
            )),
            "non-allowlisted bare-subject predicates are co-managed and must survive"
        );
        assert!(
            read(r#"<urn:foreign:subject> <urn:p> "also must survive""#),
            "foreign subjects in the projection graph must survive"
        );
    }

    #[test]
    fn graph_record_materialization_uses_projection_named_graph() {
        let store = Store::new().expect("store");
        let graph = graph_record("graph-a");
        let authority_graph = graph_projection_graph_iri("graph-a");

        materialize_graph_record(&store, &graph).expect("materialize graph record");
        let result = execute_sparql_query(
            &store,
            &format!(
                r#"
PREFIX dcterms: <{DCTERMS_NS}>
SELECT ?title WHERE {{
  GRAPH <{authority_graph}> {{
    <{subject}> dcterms:title ?title .
  }}
}}
"#,
                subject = graph_subject("graph-a"),
            ),
        )
        .expect("query graph projection");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["title"], "\"Graph A\"");
    }

    #[test]
    fn document_materialization_replaces_only_its_projection_named_graph() {
        let store = Store::new().expect("store");
        let document = document_record("graph-a", "doc-a");
        let authority_graph = document_projection_graph_iri("graph-a", "doc-a");

        materialize_document_record(&store, &document).expect("materialize document");
        let result = execute_sparql_query(
            &store,
            &format!(
                r#"
PREFIX mnemo: <{MNEMO_NS}>
SELECT ?documentId WHERE {{
  GRAPH <{authority_graph}> {{
    <{subject}#frag> mnemo:documentId ?documentId .
  }}
}}
"#,
                subject = document_subject("doc-a"),
            ),
        )
        .expect("query document projection");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["documentId"], "\"doc-a\"");

        let default_graph = execute_sparql_query(
            &store,
            &format!(
                r#"
SELECT ?type WHERE {{
  <{subject}#frag> <{RDF_TYPE}> ?type .
}}
"#,
                subject = document_subject("doc-a"),
            ),
        )
        .expect("query default graph");
        assert_eq!(default_graph.rows.len(), 0);
    }

    // ════════════════════════════════════════════════════════════════════════
    //  P1 DIFFERENTIAL ORACLE — graph-metadata materializer (instance a)
    //
    //  Ties the Lean model `Vocab.FileSystem.graphMetaSpan` / `graphMetaTriples`
    //  (FileSystem.lean) to the REAL Rust `materialize_graph_record`. NO MOCKS:
    //  every assertion drives the actual production fn against a real Oxigraph
    //  Store, then reads the named graph back through the real SPARQL service.
    //
    //  ANTI-TAUTOLOGY GUARD (cf. the P0 faithfulness-oracle comment in
    //  document_meaningful_object.rs:2008): the "expected" projection
    //  (`model_predicted_projection`) is built INDEPENDENTLY of
    //  `materialize_graph_record` — from the GraphRecord fields + the namespace
    //  constants, mirroring the Lean `graphMetaTriples` template — NOT by parsing
    //  or re-running the materializer's own SPARQL string. The two paths only meet
    //  at the read-back set comparison, so a drift in either side FAILS the test.
    // ════════════════════════════════════════════════════════════════════════

    /// Model-predicted projection of `materialize_graph_record`, derived
    /// INDEPENDENTLY from the record fields + namespace constants. This is the
    /// Rust image of Lean `graphMetaTriples subject typ idn ttl crt mdf org prv lpt`
    /// (FileSystem.lean:127-136): exactly 8 triples on the SINGLE bare subject
    /// `graph_subject(graph_id)`, rendered as N-Triples terms the way the SPARQL
    /// service's `term.to_string()` yields them (IRIs as `<…>`, simple literals as
    /// `"…"`). It does NOT touch `materialize_graph_record`'s SPARQL string.
    fn model_predicted_projection(
        graph: &GraphRecord,
    ) -> std::collections::BTreeSet<(String, String)> {
        let subject = graph_subject(&graph.graph_id);
        // helpers: how `execute_sparql_query` renders a term via `term.to_string()`.
        let iri = |p: &str| format!("<{p}>");
        // simple (un-typed) literal — exactly what `sparql_string_literal` emits,
        // and what F3 flags: NO `^^xsd:dateTime` on created/modified.
        let lit = |v: &str| format!("\"{v}\"");
        // predicate IRIs, assembled from the SAME namespace constants the
        // materializer's PREFIX block expands (runtime_config.rs). Built here, not
        // read from the materializer.
        let dcterms = |local: &str| format!("{DCTERMS_NS}{local}");
        let mnemo = |local: &str| format!("{MNEMO_NS}{local}");

        let mut set = std::collections::BTreeSet::new();
        // `a mnemo:LocalGraph`  (rdf:type → mnemo:LocalGraph IRI object)
        set.insert((RDF_TYPE.to_string(), iri(&mnemo("LocalGraph"))));
        // dcterms:identifier "{graph_id}"
        set.insert((dcterms("identifier"), lit(&graph.graph_id)));
        // dcterms:title "{title}"
        set.insert((dcterms("title"), lit(&graph.title)));
        // dcterms:created "{created_at}"   (F3: simple literal, NOT xsd:dateTime)
        set.insert((dcterms("created"), lit(&graph.created_at)));
        // dcterms:modified "{updated_at}"  (F3: simple literal, NOT xsd:dateTime)
        set.insert((dcterms("modified"), lit(&graph.updated_at)));
        // mnemo:origin "{origin}"          (F2: free string literal, no enum)
        set.insert((mnemo("origin"), lit(&graph.origin)));
        // mnemo:providerId "{provider_id}"
        set.insert((mnemo("providerId"), lit(&graph.provider_id)));
        // mnemo:localPath "{local_path}"
        set.insert((mnemo("localPath"), lit(&graph.local_path)));
        let _ = subject; // subject is implicit in the query (we read ?p ?o for it)
        set
    }

    /// Read back the COMPLETE `?p ?o` set for the graph subject in the
    /// `:projection:graph` named graph, as N-Triples-rendered term strings — the
    /// real persisted projection, surveyed via the production SPARQL service.
    fn read_back_projection(
        store: &Store,
        graph_id: &str,
    ) -> std::collections::BTreeSet<(String, String)> {
        let authority_graph = graph_projection_graph_iri(graph_id);
        let subject = graph_subject(graph_id);
        let result = execute_sparql_query(
            store,
            &format!(
                r#"
SELECT ?p ?o WHERE {{
  GRAPH <{authority_graph}> {{
    <{subject}> ?p ?o .
  }}
}}
"#
            ),
        )
        .expect("query graph projection span");
        result
            .rows
            .iter()
            .map(|row| {
                let p = row.get("p").cloned().unwrap_or_default();
                let o = row.get("o").cloned().unwrap_or_default();
                // strip the `<…>` wrapper the term renderer puts around predicate
                // IRIs so the predicate matches our raw-IRI model keys.
                let p = p
                    .strip_prefix('<')
                    .and_then(|s| s.strip_suffix('>'))
                    .map(str::to_string)
                    .unwrap_or(p);
                (p, o)
            })
            .collect()
    }

    /// (1) REAL == MODEL. Run the REAL `materialize_graph_record` into a fresh
    /// in-memory store, read back the whole `:projection:graph` span for the
    /// subject, and assert it equals the independently-derived 8-triple model
    /// projection. This is the differential oracle tying `graphMetaTriples`
    /// (Lean) to the production materializer.
    #[test]
    fn p1_oracle_graph_metadata_real_equals_model() {
        let store = Store::new().expect("store");
        let graph = graph_record("graph-a");

        // REAL production fn — NOT a reimplementation.
        materialize_graph_record(&store, &graph).expect("materialize graph record");

        let real = read_back_projection(&store, &graph.graph_id);
        let model = model_predicted_projection(&graph);

        assert_eq!(
            real.len(),
            8,
            "the materializer emits EXACTLY 8 triples on the bare subject \
             (rdf:type + 7 metadata predicates); got {real:#?}"
        );
        assert_eq!(
            real, model,
            "REAL persisted projection must equal the model-predicted projection \
             (graphMetaTriples). real={real:#?} model={model:#?}"
        );

        // F4 (CEREMONY) tie-in: every emitted triple is on the SINGLE bare subject
        // — the Rust analogue of `graphMeta_single_subject`. Confirmed by asking
        // for the subject's span and getting back the FULL graph (no other subject
        // exists in the named graph).
        let all_subjects = execute_sparql_query(
            &store,
            &format!(
                r#"
SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{
  GRAPH <{authority_graph}> {{ ?s ?p ?o . }}
}}
"#,
                authority_graph = graph_projection_graph_iri(&graph.graph_id),
            ),
        )
        .expect("count distinct subjects");
        assert_eq!(
            all_subjects.rows[0]["n"], "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "F4: exactly ONE subject in the projection graph (one-subject-per-graph)"
        );
    }

    /// (2) F1 TEETH-CHECK — the span is UNRESTRICTED bare-subject. Seed a STRAY
    /// triple `<subject> mnemo:strayPredicateNotInProjection "x"` into the
    /// `:projection:graph` named graph FIRST, THEN run the REAL
    /// `materialize_graph_record`, and assert the stray is GONE.
    ///
    /// `mnemo:strayPredicateNotInProjection` is NOT one of the 8 projection
    /// predicates. A predicate-RESTRICTED DELETE (like the Document materializer's
    /// `VALUES ?local_p { … }` allowlist) would LEAVE it. The graph-metadata
    /// materializer's DELETE is `<subject> ?p ?o` with a FREE `?p`
    /// (rdf_record_materializer.rs:18), so it reclaims EVERY predicate — the stray
    /// MUST vanish. Its disappearance CONFIRMS F1 and validates the
    /// `bareAllPredicates := true` FaceScope extension (Reconcile.lean). If it
    /// SURVIVES, that REFUTES F1 — the assert will fail loudly.
    #[test]
    fn p1_oracle_f1_unrestricted_span_deletes_stray_predicate() {
        let store = Store::new().expect("store");
        let graph = graph_record("graph-a");
        let authority_graph = graph_projection_graph_iri(&graph.graph_id);
        let subject = graph_subject(&graph.graph_id);
        let stray_predicate = format!("{MNEMO_NS}strayPredicateNotInProjection");

        // SEED the stray FIRST, in the SAME named graph the materializer owns.
        SparqlEvaluator::new()
            .parse_update(&format!(
                r#"
INSERT DATA {{
  GRAPH <{authority_graph}> {{
    <{subject}> <{stray_predicate}> "x" .
  }}
}}
"#
            ))
            .expect("parse stray insert")
            .on_store(&store)
            .execute()
            .expect("seed stray triple");

        // Confirm the stray is actually present BEFORE the materializer runs
        // (otherwise the teeth-check would be vacuous).
        let before = read_back_projection(&store, &graph.graph_id);
        assert!(
            before.contains(&(stray_predicate.clone(), "\"x\"".to_string())),
            "PRECONDITION: the stray triple must be present before materialization; \
             before={before:#?}"
        );

        // RUN the REAL materializer over the now-dirty span.
        materialize_graph_record(&store, &graph).expect("materialize over stray");

        let after = read_back_projection(&store, &graph.graph_id);

        // THE TEETH: the stray predicate must be GONE — the unrestricted bare DELETE
        // reclaimed it. (A restricted allowlist span would have left it behind.)
        assert!(
            !after.contains(&(stray_predicate.clone(), "\"x\"".to_string())),
            "F1 REFUTED: the stray triple SURVIVED materialization — the DELETE is \
             NOT unrestricted bare-subject. after={after:#?}"
        );

        // …and the span is now EXACTLY the clean 8-triple model projection (the
        // stray is gone AND nothing else leaked): real == model after reconcile.
        assert_eq!(
            after,
            model_predicted_projection(&graph),
            "after wiping the stray, the span is exactly the model projection"
        );
        assert_eq!(after.len(), 8, "exactly 8 triples remain (no stray)");
    }

    // ════════════════════════════════════════════════════════════════════════
    //  GRAPH-RECONCILE DIFFERENTIAL ORACLE — `reconcile_graph_record` (the
    //  engine reconcile) vs `materialize_graph_record` (the old wholesale path).
    //
    //  Cashes the `proto/meaningful-objects` Lean proof for the graph-metadata
    //  kind: the engine reconcile reaches the SAME net projection (EQUIVALENCE),
    //  emits <= store ops (DOMINATION), and re-runs to 0 ops (CONVERGENCE). NO
    //  MOCKS: both production fns run against real Oxigraph stores; equality is
    //  read back through the real SPARQL service and compared by VALUE-CANONICAL
    //  key (`canon_value`), so a store round-trip (e.g. literal re-serialization)
    //  never reads as drift.
    //
    //  ANTI-TAUTOLOGY: the expected projection (`expected_graph_meta_canon`) is
    //  built INDEPENDENTLY from the `GraphRecord` fields + namespace constants
    //  (the Rust image of Lean `graphMetaTriples`) — NOT by parsing or re-running
    //  either materializer's SPARQL. The reconcile, the wholesale path, and the
    //  oracle only meet at the read-back canon-set comparison.
    // ════════════════════════════════════════════════════════════════════════

    use crate::emporium::survey::parse_term as oracle_parse_term;
    use crate::emporium::terms::{canon_value, CanonValue, Term};
    use oxigraph::sparql::QueryResults;

    /// The value-canonical net projection of the graph subject's bare span in the
    /// `:projection:graph` named graph, read back through the real SPARQL service.
    fn graph_canon_set(
        store: &Store,
        graph_id: &str,
    ) -> std::collections::BTreeSet<(String, String, CanonValue)> {
        let authority_graph = graph_projection_graph_iri(graph_id);
        let subject = graph_subject(graph_id);
        let query =
            format!("SELECT ?p ?o WHERE {{ GRAPH <{authority_graph}> {{ <{subject}> ?p ?o }} }}");
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse graph canon query")
            .on_store(store)
            .execute()
            .expect("execute graph canon query")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected SELECT solutions"),
        };
        let mut set = std::collections::BTreeSet::new();
        for sol in solutions {
            let sol = sol.expect("row");
            let p = match sol.get("p").expect("?p") {
                oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
                other => other.to_string(),
            };
            let o = oracle_parse_term(&sol.get("o").expect("?o").to_string());
            set.insert((subject.clone(), p, canon_value(&o)));
        }
        set
    }

    /// INDEPENDENT oracle: the 8 graph-metadata triples as a value-canonical set,
    /// built straight from the `GraphRecord` fields + namespace constants (the
    /// Rust image of Lean `graphMetaTriples`). Does NOT touch either materializer.
    fn expected_graph_meta_canon(
        graph: &GraphRecord,
    ) -> std::collections::BTreeSet<(String, String, CanonValue)> {
        let s = graph_subject(&graph.graph_id);
        let lit =
            |v: &str| canon_value(&Term::Lit(oxigraph::model::Literal::new_simple_literal(v)));
        let uri = |iri: &str| {
            canon_value(&Term::Uri(
                oxigraph::model::NamedNode::new(iri).expect("valid IRI"),
            ))
        };
        let mut set = std::collections::BTreeSet::new();
        set.insert((
            s.clone(),
            RDF_TYPE.to_string(),
            uri(&format!("{MNEMO_NS}LocalGraph")),
        ));
        set.insert((
            s.clone(),
            format!("{DCTERMS_NS}identifier"),
            lit(&graph.graph_id),
        ));
        set.insert((s.clone(), format!("{DCTERMS_NS}title"), lit(&graph.title)));
        set.insert((
            s.clone(),
            format!("{DCTERMS_NS}created"),
            lit(&graph.created_at),
        ));
        set.insert((
            s.clone(),
            format!("{DCTERMS_NS}modified"),
            lit(&graph.updated_at),
        ));
        set.insert((s.clone(), format!("{MNEMO_NS}origin"), lit(&graph.origin)));
        set.insert((
            s.clone(),
            format!("{MNEMO_NS}providerId"),
            lit(&graph.provider_id),
        ));
        set.insert((s, format!("{MNEMO_NS}localPath"), lit(&graph.local_path)));
        set
    }

    /// (1) EQUIVALENCE + DOMINATION + CONVERGENCE on a FRESH store: the engine
    /// reconcile reaches the SAME net projection as the wholesale materializer,
    /// matches the independent 8-triple oracle, and re-runs to 0 ops.
    #[test]
    fn reconcile_graph_fresh_equals_wholesale_and_oracle() {
        let graph = graph_record("graph-a");

        // OLD wholesale path on store A.
        let store_old = Store::new().expect("store A");
        materialize_graph_record(&store_old, &graph).expect("wholesale materialize");

        // NEW engine reconcile on store B (fresh → 8 adds, 0 removes).
        let store_new = Store::new().expect("store B");
        let d = reconcile_graph_record(&store_new, &graph).expect("reconcile");
        assert_eq!(
            d.op_count(),
            8,
            "a fresh reconcile emits exactly the 8 graph-metadata adds"
        );

        let old_set = graph_canon_set(&store_old, &graph.graph_id);
        let new_set = graph_canon_set(&store_new, &graph.graph_id);
        let oracle = expected_graph_meta_canon(&graph);

        // EQUIVALENCE: reconcile net state == wholesale net state == oracle. Graph
        // metadata is PURE PARITY (not a superset like Document): the desired set is
        // EXACTLY the 8 triples the old path writes.
        assert_eq!(
            new_set, old_set,
            "EQUIVALENCE: reconcile reaches the SAME net projection as the wholesale path"
        );
        assert_eq!(
            new_set, oracle,
            "EQUIVALENCE: reconcile net projection == the independent 8-triple oracle"
        );
        assert_eq!(
            new_set.len(),
            8,
            "exactly 8 triples on the bare graph subject"
        );

        // CONVERGENCE: a second reconcile to the SAME record emits zero ops and the
        // graph is byte-stable.
        let before = graph_canon_set(&store_new, &graph.graph_id);
        let d_converge = reconcile_graph_record(&store_new, &graph).expect("converge");
        assert_eq!(
            d_converge.op_count(),
            0,
            "a converged reconcile emits zero ops"
        );
        assert_eq!(
            before,
            graph_canon_set(&store_new, &graph.graph_id),
            "the graph is byte-stable across an identical re-reconcile"
        );
    }

    /// (2) UPDATE: DOMINATION + parity on a metadata edit. Seed both stores to v1,
    /// then change the title + modified timestamp. The wholesale path tears down
    /// all 8 and re-inserts 8 (16 store ops); the engine reconcile emits ONLY the
    /// changed slots (2 removes + 2 adds = 4). Both reach the same net projection.
    #[test]
    fn reconcile_graph_update_dominates_and_keeps_parity() {
        let v1 = graph_record("graph-a");
        let mut v2 = graph_record("graph-a");
        v2.title = "Graph A — renamed".to_string();
        v2.updated_at = "3".to_string();

        // Seed both stores to v1 (wholesale on A, reconcile on B — both land the
        // same 8 triples, verified by the fresh test above).
        let store_old = Store::new().expect("store A");
        materialize_graph_record(&store_old, &v1).expect("seed A v1");
        let store_new = Store::new().expect("store B");
        reconcile_graph_record(&store_new, &v1).expect("seed B v1");

        // OLD wholesale op count for v1→v2: DELETE the whole 8-triple span + INSERT
        // the 8 v2 triples = 16 store ops, regardless of how little changed.
        let old_span = read_back_projection(&store_old, &v1.graph_id).len(); // = 8
        let ops_old = old_span + 8;
        assert_eq!(ops_old, 16, "wholesale rebuild touches all 16 slots");

        // Apply v2: wholesale on A, engine reconcile on B.
        materialize_graph_record(&store_old, &v2).expect("wholesale v2");
        let ops_new = reconcile_graph_record(&store_new, &v2)
            .expect("reconcile v2")
            .op_count();

        // DOMINATION: the reconcile emits FEWER ops — exactly the 2 changed slots
        // (title + modified) as 2 removes + 2 adds.
        assert_eq!(
            ops_new, 4,
            "only title + modified changed → 2 removes + 2 adds"
        );
        assert!(
            ops_new < ops_old,
            "DOMINATION: reconcile emits fewer ops ({ops_new}) than wholesale ({ops_old})"
        );

        // EQUIVALENCE after the edit: both stores hold the SAME v2 net projection,
        // equal to the independent oracle.
        let old_set = graph_canon_set(&store_old, &v2.graph_id);
        let new_set = graph_canon_set(&store_new, &v2.graph_id);
        assert_eq!(
            new_set, old_set,
            "UPDATE EQUIVALENCE: reconcile == wholesale net projection after the edit"
        );
        assert_eq!(
            new_set,
            expected_graph_meta_canon(&v2),
            "UPDATE EQUIVALENCE: reconcile net projection == the v2 oracle"
        );
    }

    /// (3) RECLAIM: a stray bare predicate on the graph subject (NOT one of the 8)
    /// is removed by the reconcile — confirming the survey span is UNRESTRICTED
    /// bare (the graph subject is owned outright), so the engine reconcile inherits
    /// the wholesale path's F1 teeth.
    #[test]
    fn reconcile_graph_reclaims_stray_predicate() {
        let graph = graph_record("graph-a");
        let store = Store::new().expect("store");
        let authority_graph = graph_projection_graph_iri(&graph.graph_id);
        let subject = graph_subject(&graph.graph_id);
        let stray = format!("{MNEMO_NS}strayPredicateNotInProjection");

        // Reconcile the clean 8, then seed a stray onto the same bare subject.
        reconcile_graph_record(&store, &graph).expect("reconcile clean");
        SparqlEvaluator::new()
            .parse_update(&format!(
                "INSERT DATA {{ GRAPH <{authority_graph}> {{ <{subject}> <{stray}> \"x\" . }} }}"
            ))
            .expect("parse stray insert")
            .on_store(&store)
            .execute()
            .expect("seed stray");

        // Re-reconcile: the stray is NOT in `desired`, and the survey span is `?p`
        // free, so it surveys as `current` and diffs to a REMOVE.
        let d = reconcile_graph_record(&store, &graph).expect("reconcile over stray");
        assert_eq!(
            d.op_count(),
            1,
            "exactly one op: the stray triple is removed"
        );

        let after = graph_canon_set(&store, &graph.graph_id);
        assert_eq!(
            after,
            expected_graph_meta_canon(&graph),
            "after reclaiming the stray, the span is exactly the 8-triple model projection"
        );
        assert!(
            !after.iter().any(|(_, p, _)| p == &stray),
            "the stray predicate is gone — the reconcile span is unrestricted bare"
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    //  EA-2b SHACL CONFORMANCE ORACLE — graph-metadata kind.
    //
    //  The retrofit's CI consistency check: the REAL projection of the graph kind
    //  (`reconcile_graph_record` into a real Oxigraph store) CONFORMS to the SHACL
    //  shapes DERIVED from the `emporium-graph` vocab contract (`vocab_to_shacl`).
    //  A non-conformance would be a materializer↔vocab DRIFT BUG (the projection
    //  is a deterministic re-projection of trusted state, so the only way it can
    //  violate the contract-derived shapes is if the two have drifted apart).
    //
    //  NO MOCKS: real reconcile, real store, real rudof engine (the same
    //  `validate_desired` the memory/document appliers run live). The validated
    //  triples are READ BACK out of the persisted projection (not the in-memory
    //  `desired`), so the oracle covers the WHOLE materializer→store→shapes path,
    //  including any store round-trip normalization.
    //
    //  TEETH: a deliberately-bent projection (a required predicate DROPPED, the
    //  rdf:type swapped) is REJECTED — proving the oracle is not a tautology.
    // ════════════════════════════════════════════════════════════════════════

    use crate::emporium::contract::graph_vocabulary;
    use crate::emporium::shacl_validator::validate_desired;
    use crate::emporium::terms::Triple as EngineTriple;

    /// Read the REAL persisted graph-metadata projection back out of the store as
    /// engine `Triple`s — the exact input shape `validate_desired` consumes. Reads
    /// the full `<subject> ?p ?o` span in the `:projection:graph` named graph
    /// through the production SPARQL service, bridging each `?o` back to an engine
    /// `Term` via the proven `parse_term` round-trip (identical to the survey).
    fn read_back_graph_projection(store: &Store, graph_id: &str) -> Vec<EngineTriple> {
        let authority_graph = graph_projection_graph_iri(graph_id);
        let subject = graph_subject(graph_id);
        let query =
            format!("SELECT ?p ?o WHERE {{ GRAPH <{authority_graph}> {{ <{subject}> ?p ?o }} }}");
        let solutions = match SparqlEvaluator::new()
            .parse_query(&query)
            .expect("parse graph readback")
            .on_store(store)
            .execute()
            .expect("execute graph readback")
        {
            QueryResults::Solutions(s) => s,
            _ => panic!("expected SELECT solutions"),
        };
        let mut out = Vec::new();
        for sol in solutions {
            let sol = sol.expect("row");
            let p = match sol.get("p").expect("?p") {
                oxigraph::model::Term::NamedNode(n) => n.as_str().to_string(),
                other => other.to_string(),
            };
            let o = oracle_parse_term(&sol.get("o").expect("?o").to_string());
            out.push((subject.clone(), p, o));
        }
        out
    }

    /// CONFORMANCE: the REAL graph projection conforms to the vocab-derived shapes.
    #[test]
    fn shacl_oracle_graph_conforms() {
        let store = Store::new().expect("store");
        let graph = graph_record("graph-a");

        // REAL projection into a real store.
        reconcile_graph_record(&store, &graph).expect("reconcile graph");

        // Read the persisted projection back as engine triples.
        let projection = read_back_graph_projection(&store, &graph.graph_id);
        assert_eq!(
            projection.len(),
            8,
            "the graph projection is the 8-triple span"
        );

        // Validate against the SHACL shapes DERIVED from the emporium-graph contract.
        let result = validate_desired(&projection, graph_vocabulary());
        assert!(
            result.is_ok(),
            "the REAL graph projection must conform to the vocab-derived shapes \
             (a violation = materializer↔vocab drift): {result:?}"
        );
    }

    /// TEETH #1: a projection MISSING a required predicate (dcterms:title dropped)
    /// is REJECTED — the derived shape carries `sh:minCount 1` on title, so the
    /// oracle is not a pass-everything tautology.
    #[test]
    fn shacl_oracle_graph_teeth_missing_required_predicate() {
        let store = Store::new().expect("store");
        let graph = graph_record("graph-a");
        reconcile_graph_record(&store, &graph).expect("reconcile graph");

        let mut bent = read_back_graph_projection(&store, &graph.graph_id);
        let title_p = format!("{DCTERMS_NS}title");
        bent.retain(|(_, p, _)| p != &title_p);
        assert_eq!(bent.len(), 7, "title dropped → 7 triples");

        let result = validate_desired(&bent, graph_vocabulary());
        assert!(
            result.is_err(),
            "a projection missing the required dcterms:title must be rejected"
        );
        assert!(
            result.unwrap_err().starts_with("SHACL:"),
            "loud-halt prefix"
        );
    }

    /// TEETH #2: a CLOSED-shape violation — a rogue predicate OUTSIDE the contract
    /// on the graph subject is rejected (`sh:closed true`).
    #[test]
    fn shacl_oracle_graph_teeth_rogue_predicate() {
        let store = Store::new().expect("store");
        let graph = graph_record("graph-a");
        reconcile_graph_record(&store, &graph).expect("reconcile graph");

        let mut bent = read_back_graph_projection(&store, &graph.graph_id);
        let subject = graph_subject(&graph.graph_id);
        bent.push((
            subject,
            "http://example.org/not-in-the-graph-contract".to_string(),
            Term::Lit(oxigraph::model::Literal::new_simple_literal("rogue")),
        ));

        let result = validate_desired(&bent, graph_vocabulary());
        assert!(
            result.is_err(),
            "a predicate outside the closed graph contract must be rejected"
        );
        assert!(result.unwrap_err().starts_with("SHACL:"));
    }
}

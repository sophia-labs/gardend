use crate::{
    geist_memory_store::LocalMemoryStore,
    rdf::{
        format_rdf_triple, graph_subject, push_integer_triple, push_string_triple, push_uri_triple,
        sparql_string_literal,
    },
    rdf_service::open_graph_store,
    runtime_config::{DCTERMS_NS, MNEMO_NS, RDF_TYPE},
};
use oxigraph::sparql::SparqlEvaluator;
use std::path::Path;

fn memory_subject(graph_id: &str, number: u64) -> String {
    // Matches the codebase-wide graph-scoped subject convention (a `:`-joined
    // segment after `graph_subject(graph_id)` — see e.g.
    // `rdf_authority::memory_projection_graph_iri` `:projection:memory`,
    // `semantic_scaffold_rdf::semantic_edge_subject` `:semantic:edge:{id}`).
    // Previously this concatenated with NO separator at all
    // (`graph_subject(graph_id)` + `memory/{number}`), minting e.g.
    // `urn:mnemosyne:local:graph:vehicle-localmemory/87` — the graph id and
    // `memory/N` ran together with no boundary.
    format!("{}:memory:{number}", graph_subject(graph_id))
}

pub(crate) fn materialize_memory_store(
    graph_dir: &Path,
    store: &LocalMemoryStore,
) -> Result<(), String> {
    let oxi_store = open_graph_store(graph_dir)?;
    let graph_id_literal = sparql_string_literal(&store.graph_id);
    // SAFE against the `memory_subject` separator fix above: this DELETE is
    // keyed on `?memory a mnemo:Memory ; mnemo:graphId {graph_id_literal}`, NOT
    // on the subject URI's string shape. Any OLD-style (separator-less)
    // `...graph:{id}memory/{n}` subject still carries both that rdf:type and
    // that `mnemo:graphId` literal, so it still unifies with `?memory` here and
    // gets wholesale-deleted (all its `?p ?o` pairs) on the very next
    // materialize call — no orphaned old-shape subjects survive a rewrite.
    let delete = format!(
        r#"
PREFIX mnemo: <{MNEMO_NS}>
DELETE {{ ?memory ?p ?o . }}
WHERE {{
  ?memory a mnemo:Memory ;
    mnemo:graphId {graph_id_literal} ;
    ?p ?o .
}}
"#
    );
    SparqlEvaluator::new()
        .parse_update(&delete)
        .map_err(|error| format!("parse memory cleanup update: {error}"))?
        .on_store(&oxi_store)
        .execute()
        .map_err(|error| format!("clear memory store RDF: {error}"))?;
    // DURABILITY AUDIT FINDING (2026-07-18): direct-on-store wholesale
    // clear-then-rewrite, no narrow mark, reachable with NO covering lease at
    // all from `mcp_local_care_memories` (the `care` MCP tool) and the
    // memory-archive path (`geist_memory_service.rs`) — an acknowledged write
    // previously invisible to the durable flush. This DELETE always executes
    // once we get here, so mark now, unconditionally; the INSERT below marks
    // again when it runs (harmless — marking twice just bumps the epoch
    // twice, never a correctness issue).
    crate::cell_durability::mark_rdf_store_written(&oxi_store);

    let mut triples = Vec::new();
    for memory in store.memories.values() {
        let subject = memory_subject(&store.graph_id, memory.number);
        push_uri_triple(
            &mut triples,
            &subject,
            RDF_TYPE,
            &format!("{MNEMO_NS}Memory"),
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}graphId"),
            &store.graph_id,
        );
        push_integer_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}memoryNumber"),
            memory.number as i64,
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}blockId"),
            &memory.block_id,
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}content"),
            &memory.content,
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{DCTERMS_NS}created"),
            &memory.created_at,
        );
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{MNEMO_NS}lastActiveAt"),
            &memory.last_active,
        );
    }

    if triples.is_empty() {
        return Ok(());
    }
    let insert = triples
        .iter()
        .map(format_rdf_triple)
        .collect::<Vec<_>>()
        .join("\n  ");
    let update = format!("INSERT DATA {{\n  {insert}\n}}");
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse memory materialization update: {error}"))?
        .on_store(&oxi_store)
        .execute()
        .map_err(|error| format!("materialize memory store RDF: {error}"))?;
    crate::cell_durability::mark_rdf_store_written(&oxi_store);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{geist_memory_store::LocalMemoryRecord, rdf_query_service::execute_sparql_query};
    use std::collections::BTreeMap;
    use uuid::Uuid;

    /// A fresh, unique on-disk graph dir — the global store cache in
    /// `rdf_store_service` keys by `graph_dir/store.oxigraph`, so a unique dir
    /// per test is required to avoid cross-test bleed (mirrors the
    /// `salience_rdf_materializer` test helper).
    fn temp_graph_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sophia-geist-memory-rdf-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp graph dir");
        dir
    }

    fn memory_record(number: u64) -> LocalMemoryRecord {
        LocalMemoryRecord {
            number,
            block_id: format!("memory-{number}"),
            content: format!("content {number}"),
            created_at: "1000".to_string(),
            last_active: "2000".to_string(),
        }
    }

    #[test]
    fn memory_subject_is_colon_separated_not_bare_concatenated() {
        // The historical bug: `format!("{}memory/{number}", graph_subject(graph_id))`
        // with NO separator minted `urn:mnemosyne:local:graph:vehicle-localmemory/87`
        // — the graph id and `memory/N` ran together with no boundary at all.
        let subject = memory_subject("vehicle-local", 87);
        assert_eq!(subject, "urn:mnemosyne:local:graph:vehicle-local:memory:87");
        assert!(!subject.contains("vehicle-localmemory"));
    }

    #[test]
    fn materialize_memory_store_writes_colon_separated_subjects() {
        let graph_dir = temp_graph_dir();
        let mut memories = BTreeMap::new();
        memories.insert("87".to_string(), memory_record(87));
        let store = LocalMemoryStore {
            schema_version: 1,
            graph_id: "vehicle-local".to_string(),
            next_number: 88,
            memories,
            archives: Vec::new(),
        };

        materialize_memory_store(&graph_dir, &store).expect("materialize memory store");

        let oxi_store = open_graph_store(&graph_dir).expect("open store");
        let result = execute_sparql_query(
            &oxi_store,
            &format!(
                r#"
PREFIX mnemo: <{MNEMO_NS}>
SELECT ?memory WHERE {{ ?memory a mnemo:Memory }}
"#
            ),
        )
        .expect("query memory subjects");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0]["memory"],
            "<urn:mnemosyne:local:graph:vehicle-local:memory:87>"
        );

        let _ = std::fs::remove_dir_all(&graph_dir);
    }

    #[test]
    fn materialize_memory_store_sweeps_old_style_subjects_on_rewrite() {
        // Safety check for the separator fix: the DELETE is keyed on
        // `?memory a mnemo:Memory ; mnemo:graphId {literal}`, NOT on the subject
        // URI's string shape. Seed an OLD-style (separator-less) subject sharing
        // that type + graphId, then materialize — it must be swept away, leaving
        // only the new-style subject behind.
        let graph_dir = temp_graph_dir();
        let old_style_subject = "urn:mnemosyne:local:graph:vehicle-localmemory/87";
        let oxi_store = open_graph_store(&graph_dir).expect("open store");
        let seed = format!(
            r#"
PREFIX mnemo: <{MNEMO_NS}>
INSERT DATA {{
  <{old_style_subject}> a mnemo:Memory ;
    mnemo:graphId "vehicle-local" ;
    mnemo:memoryNumber 87 .
}}
"#
        );
        SparqlEvaluator::new()
            .parse_update(&seed)
            .expect("parse seed update")
            .on_store(&oxi_store)
            .execute()
            .expect("seed old-style subject");
        drop(oxi_store);

        let mut memories = BTreeMap::new();
        memories.insert("87".to_string(), memory_record(87));
        let store = LocalMemoryStore {
            schema_version: 1,
            graph_id: "vehicle-local".to_string(),
            next_number: 88,
            memories,
            archives: Vec::new(),
        };
        materialize_memory_store(&graph_dir, &store).expect("materialize memory store");

        let oxi_store = open_graph_store(&graph_dir).expect("open store");
        let result = execute_sparql_query(
            &oxi_store,
            &format!(
                r#"
PREFIX mnemo: <{MNEMO_NS}>
SELECT ?memory WHERE {{ ?memory a mnemo:Memory }}
"#
            ),
        )
        .expect("query memory subjects");

        assert_eq!(
            result.rows.len(),
            1,
            "old-style subject must not survive a rewrite"
        );
        assert_eq!(
            result.rows[0]["memory"],
            "<urn:mnemosyne:local:graph:vehicle-local:memory:87>"
        );

        let _ = std::fs::remove_dir_all(&graph_dir);
    }
}

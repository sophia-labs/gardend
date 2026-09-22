//! KG-ULTRA intuition as a Meaningful Object.
//!
//! KG-ULTRA's private runtime artifacts — integerized entity ids, relation
//! graphs, tensors, checkpoints, and caches — are service-private. The durable
//! Garden-facing object is the model's defeasible testimony: "given this graph
//! snapshot and query, here are ranked structural candidates." That testimony is
//! queryable RDF in the reserved `:projection:kg-ultra` graph, so agents can cite,
//! accept, reject, and evaluate it without treating model output as graph truth.

#![allow(dead_code)]

use oxigraph::sparql::SparqlEvaluator;
use oxigraph::store::Store;

use crate::{
    ids::url_component,
    rdf::{
        format_rdf_triple, graph_subject, push_float_triple, push_integer_triple,
        push_string_triple, push_typed_literal_triple, push_uri_triple, RdfTriple,
    },
    rdf_authority::kg_ultra_projection_graph_iri,
    runtime_config::{RDF_TYPE, XSD_NS},
};

pub(crate) const KG_ULTRA_NS: &str = "http://mnemosyne.dev/kg-ultra#";
pub(crate) const PROV_NS: &str = "http://www.w3.org/ns/prov#";
pub(crate) const DEFAULT_KG_ULTRA_OBSERVER: &str = "urn:sophia:observer:kg-ultra";

#[derive(Debug, Clone)]
pub(crate) struct KgUltraIntuitionRecord {
    pub(crate) graph_id: String,
    pub(crate) local_id: String,
    pub(crate) model_id: String,
    pub(crate) model_version: Option<String>,
    pub(crate) source_snapshot_id: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) task_kind: Option<String>,
    pub(crate) query_json: Option<String>,
    pub(crate) compiled_query_json: Option<String>,
    pub(crate) generated_at: String,
    pub(crate) observer: Option<String>,
    pub(crate) candidates: Vec<KgUltraIntuitionCandidate>,
    pub(crate) answer_candidates: Vec<KgUltraAnswerCandidate>,
}

#[derive(Debug, Clone)]
pub(crate) struct KgUltraIntuitionCandidate {
    pub(crate) local_id: String,
    pub(crate) head: String,
    pub(crate) relation: String,
    pub(crate) tail: String,
    pub(crate) rank: i64,
    pub(crate) score: Option<f64>,
    pub(crate) explanation: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct KgUltraAnswerCandidate {
    pub(crate) local_id: String,
    pub(crate) answer_json: String,
    pub(crate) rank: i64,
    pub(crate) score: Option<f64>,
    pub(crate) binding_json: Option<String>,
    pub(crate) score_parts_json: Option<String>,
    pub(crate) explanation: Option<String>,
}

pub(crate) fn intuition_subject(graph_id: &str, local_id: &str) -> String {
    format!(
        "{}:projection:kg-ultra:intuition:{}",
        graph_subject(graph_id),
        url_component(local_id)
    )
}

pub(crate) fn candidate_subject(graph_id: &str, intuition_id: &str, candidate_id: &str) -> String {
    format!(
        "{}:candidate:{}",
        intuition_subject(graph_id, intuition_id),
        url_component(candidate_id)
    )
}

pub(crate) fn answer_candidate_subject(
    graph_id: &str,
    intuition_id: &str,
    candidate_id: &str,
) -> String {
    format!(
        "{}:answer:{}",
        intuition_subject(graph_id, intuition_id),
        url_component(candidate_id)
    )
}

pub(crate) fn intuition_record_triples(record: &KgUltraIntuitionRecord) -> Vec<RdfTriple> {
    let mut triples = Vec::new();
    let subject = intuition_subject(&record.graph_id, &record.local_id);
    let observer = record
        .observer
        .as_deref()
        .unwrap_or(DEFAULT_KG_ULTRA_OBSERVER);

    push_uri_triple(
        &mut triples,
        &subject,
        RDF_TYPE,
        &format!("{KG_ULTRA_NS}Intuition"),
    );
    push_uri_triple(
        &mut triples,
        &subject,
        RDF_TYPE,
        &format!("{PROV_NS}Entity"),
    );
    push_uri_triple(
        &mut triples,
        &subject,
        &format!("{PROV_NS}wasAttributedTo"),
        observer,
    );
    push_string_triple(
        &mut triples,
        &subject,
        &format!("{KG_ULTRA_NS}graphId"),
        &record.graph_id,
    );
    push_string_triple(
        &mut triples,
        &subject,
        &format!("{KG_ULTRA_NS}intuitionId"),
        &record.local_id,
    );
    push_string_triple(
        &mut triples,
        &subject,
        &format!("{KG_ULTRA_NS}modelId"),
        &record.model_id,
    );
    push_typed_literal_triple(
        &mut triples,
        &subject,
        &format!("{KG_ULTRA_NS}generatedAt"),
        &record.generated_at,
        &format!("{XSD_NS}dateTime"),
    );
    if let Some(model_version) = &record.model_version {
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}modelVersion"),
            model_version,
        );
    }
    if let Some(source_snapshot_id) = &record.source_snapshot_id {
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}sourceSnapshotId"),
            source_snapshot_id,
        );
    }
    if let Some(task_id) = &record.task_id {
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}taskId"),
            task_id,
        );
    }
    if let Some(task_kind) = &record.task_kind {
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}taskKind"),
            task_kind,
        );
    }
    if let Some(query_json) = &record.query_json {
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}queryJson"),
            query_json,
        );
    }
    if let Some(compiled_query_json) = &record.compiled_query_json {
        push_string_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}compiledQueryJson"),
            compiled_query_json,
        );
    }

    for candidate in &record.candidates {
        let candidate_subject =
            candidate_subject(&record.graph_id, &record.local_id, &candidate.local_id);
        push_uri_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}hasCandidate"),
            &candidate_subject,
        );
        push_uri_triple(
            &mut triples,
            &candidate_subject,
            RDF_TYPE,
            &format!("{KG_ULTRA_NS}IntuitionCandidate"),
        );
        push_uri_triple(
            &mut triples,
            &candidate_subject,
            &format!("{KG_ULTRA_NS}partOfIntuition"),
            &subject,
        );
        push_string_triple(
            &mut triples,
            &candidate_subject,
            &format!("{KG_ULTRA_NS}candidateId"),
            &candidate.local_id,
        );
        push_uri_triple(
            &mut triples,
            &candidate_subject,
            &format!("{KG_ULTRA_NS}head"),
            &candidate.head,
        );
        push_uri_triple(
            &mut triples,
            &candidate_subject,
            &format!("{KG_ULTRA_NS}relation"),
            &candidate.relation,
        );
        push_uri_triple(
            &mut triples,
            &candidate_subject,
            &format!("{KG_ULTRA_NS}tail"),
            &candidate.tail,
        );
        push_integer_triple(
            &mut triples,
            &candidate_subject,
            &format!("{KG_ULTRA_NS}rank"),
            candidate.rank,
        );
        if let Some(score) = candidate.score {
            push_float_triple(
                &mut triples,
                &candidate_subject,
                &format!("{KG_ULTRA_NS}score"),
                score,
            );
        }
        if let Some(explanation) = &candidate.explanation {
            push_string_triple(
                &mut triples,
                &candidate_subject,
                &format!("{KG_ULTRA_NS}explanation"),
                explanation,
            );
        }
    }

    for candidate in &record.answer_candidates {
        let answer_subject =
            answer_candidate_subject(&record.graph_id, &record.local_id, &candidate.local_id);
        push_uri_triple(
            &mut triples,
            &subject,
            &format!("{KG_ULTRA_NS}hasAnswerCandidate"),
            &answer_subject,
        );
        push_uri_triple(
            &mut triples,
            &answer_subject,
            RDF_TYPE,
            &format!("{KG_ULTRA_NS}AnswerCandidate"),
        );
        push_uri_triple(
            &mut triples,
            &answer_subject,
            &format!("{KG_ULTRA_NS}partOfIntuition"),
            &subject,
        );
        push_string_triple(
            &mut triples,
            &answer_subject,
            &format!("{KG_ULTRA_NS}candidateId"),
            &candidate.local_id,
        );
        push_string_triple(
            &mut triples,
            &answer_subject,
            &format!("{KG_ULTRA_NS}answerJson"),
            &candidate.answer_json,
        );
        push_integer_triple(
            &mut triples,
            &answer_subject,
            &format!("{KG_ULTRA_NS}rank"),
            candidate.rank,
        );
        if let Some(score) = candidate.score {
            push_float_triple(
                &mut triples,
                &answer_subject,
                &format!("{KG_ULTRA_NS}score"),
                score,
            );
        }
        if let Some(binding_json) = &candidate.binding_json {
            push_string_triple(
                &mut triples,
                &answer_subject,
                &format!("{KG_ULTRA_NS}bindingJson"),
                binding_json,
            );
        }
        if let Some(score_parts_json) = &candidate.score_parts_json {
            push_string_triple(
                &mut triples,
                &answer_subject,
                &format!("{KG_ULTRA_NS}scorePartsJson"),
                score_parts_json,
            );
        }
        if let Some(explanation) = &candidate.explanation {
            push_string_triple(
                &mut triples,
                &answer_subject,
                &format!("{KG_ULTRA_NS}explanation"),
                explanation,
            );
        }
    }

    triples
}

pub(crate) fn append_intuition_record(
    store: &Store,
    record: &KgUltraIntuitionRecord,
) -> Result<usize, String> {
    let triples = intuition_record_triples(record);
    if triples.is_empty() {
        return Ok(0);
    }
    let projection_graph = kg_ultra_projection_graph_iri(&record.graph_id);
    let body = triples
        .iter()
        .map(format_rdf_triple)
        .collect::<Vec<_>>()
        .join("\n");
    let update = format!("INSERT DATA {{ GRAPH <{projection_graph}> {{\n{body}\n}} }}");
    SparqlEvaluator::new()
        .parse_update(&update)
        .map_err(|error| format!("parse kg-ultra intuition update: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute kg-ultra intuition update: {error}"))?;
    Ok(record.candidates.len() + record.answer_candidates.len() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::sparql::QueryResults;

    fn store() -> Store {
        Store::new().expect("in-memory store")
    }

    fn sample() -> KgUltraIntuitionRecord {
        KgUltraIntuitionRecord {
            graph_id: "lab".to_string(),
            local_id: "turn-1".to_string(),
            model_id: "ultra_4g".to_string(),
            model_version: Some("zero-shot".to_string()),
            source_snapshot_id: Some("workspace@42".to_string()),
            task_id: Some("agent-turn-1".to_string()),
            task_kind: Some("logical-query-answering".to_string()),
            query_json: Some(r#"{"relationWhitelist":["requires"]}"#.to_string()),
            compiled_query_json: Some(r#"{"kind":"path","maxHops":2}"#.to_string()),
            generated_at: "2026-06-24T12:00:00Z".to_string(),
            observer: None,
            candidates: vec![KgUltraIntuitionCandidate {
                local_id: "c1".to_string(),
                head: "urn:mnemosyne:local:document:a".to_string(),
                relation: "http://mnemosyne.ai/vocab#requires".to_string(),
                tail: "urn:mnemosyne:local:document:b".to_string(),
                rank: 1,
                score: Some(0.82),
                explanation: Some("structural cluster match".to_string()),
            }],
            answer_candidates: vec![KgUltraAnswerCandidate {
                local_id: "a1".to_string(),
                answer_json: r#"{"?target":"urn:mnemosyne:local:document:b"}"#.to_string(),
                rank: 1,
                score: Some(0.77),
                binding_json: Some(r#"{"target":"urn:mnemosyne:local:document:b"}"#.to_string()),
                score_parts_json: Some(r#"{"structural":0.77}"#.to_string()),
                explanation: Some("logical path support".to_string()),
            }],
        }
    }

    fn count(store: &Store, query: &str) -> usize {
        match SparqlEvaluator::new()
            .parse_query(query)
            .expect("parse")
            .on_store(store)
            .execute()
            .expect("execute")
        {
            QueryResults::Solutions(solutions) => solutions.count(),
            _ => panic!("expected solutions"),
        }
    }

    #[test]
    fn intuition_record_is_queryable_meaningful_object() {
        let store = store();
        let record = sample();
        let graph = kg_ultra_projection_graph_iri(&record.graph_id);
        append_intuition_record(&store, &record).expect("append intuition");

        let intuitions = count(
            &store,
            &format!(
                "SELECT ?i WHERE {{ GRAPH <{graph}> {{ ?i a <{KG_ULTRA_NS}Intuition> ; \
                 <{PROV_NS}wasAttributedTo> <{DEFAULT_KG_ULTRA_OBSERVER}> }} }}"
            ),
        );
        assert_eq!(intuitions, 1, "intuition head is queryable and attributed");

        let candidates = count(
            &store,
            &format!(
                "SELECT ?c WHERE {{ GRAPH <{graph}> {{ ?c a <{KG_ULTRA_NS}IntuitionCandidate> ; \
                 <{KG_ULTRA_NS}rank> ?rank ; <{KG_ULTRA_NS}score> ?score }} }}"
            ),
        );
        assert_eq!(candidates, 1, "candidate child is queryable");

        let answers = count(
            &store,
            &format!(
                "SELECT ?a WHERE {{ GRAPH <{graph}> {{ ?a a <{KG_ULTRA_NS}AnswerCandidate> ; \
                 <{KG_ULTRA_NS}answerJson> ?answer ; <{KG_ULTRA_NS}scorePartsJson> ?parts }} }}"
            ),
        );
        assert_eq!(answers, 1, "logical answer child is queryable");
    }

    #[test]
    fn appending_same_intuition_twice_is_idempotent() {
        let store = store();
        let record = sample();
        let graph = kg_ultra_projection_graph_iri(&record.graph_id);
        append_intuition_record(&store, &record).expect("append once");
        append_intuition_record(&store, &record).expect("append twice");
        let intuitions = count(
            &store,
            &format!("SELECT ?i WHERE {{ GRAPH <{graph}> {{ ?i a <{KG_ULTRA_NS}Intuition> }} }}"),
        );
        let candidates = count(
            &store,
            &format!(
                "SELECT ?c WHERE {{ GRAPH <{graph}> {{ ?c a <{KG_ULTRA_NS}IntuitionCandidate> }} }}"
            ),
        );
        let answers = count(
            &store,
            &format!(
                "SELECT ?a WHERE {{ GRAPH <{graph}> {{ ?a a <{KG_ULTRA_NS}AnswerCandidate> }} }}"
            ),
        );
        assert_eq!(intuitions, 1, "same intuition subject is set-idempotent");
        assert_eq!(candidates, 1, "same candidate subject is set-idempotent");
        assert_eq!(answers, 1, "same answer subject is set-idempotent");
    }

    #[test]
    fn subjects_are_stable_under_local_ids() {
        assert_eq!(
            intuition_subject("lab", "turn 1"),
            "urn:mnemosyne:local:graph:lab:projection:kg-ultra:intuition:turn%201"
        );
        assert_eq!(
            candidate_subject("lab", "turn 1", "c/1"),
            "urn:mnemosyne:local:graph:lab:projection:kg-ultra:intuition:turn%201:candidate:c%2F1"
        );
        assert_eq!(
            answer_candidate_subject("lab", "turn 1", "a/1"),
            "urn:mnemosyne:local:graph:lab:projection:kg-ultra:intuition:turn%201:answer:a%2F1"
        );
    }
}

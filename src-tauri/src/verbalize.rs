#![cfg_attr(not(test), allow(dead_code))]

use crate::{
    runtime_config::{MDOC_NS, MNEMO_NS, RDF_TYPE, WIRE_NS},
    wire_predicates::{predicate_label, predicate_short_name},
};
use oxigraph::{
    model::Term as OxTerm,
    sparql::{QueryResults, SparqlEvaluator},
    store::Store,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub(crate) struct VerbalizeCfg {
    pub(crate) relation_sample_limit: usize,
}

impl Default for VerbalizeCfg {
    fn default() -> Self {
        Self {
            relation_sample_limit: 2,
        }
    }
}

pub(crate) fn verbalize(iri: &str, store: &Store, cfg: &VerbalizeCfg) -> Option<String> {
    let iri = iri.trim();
    if iri.is_empty() {
        return None;
    }
    literal_object(store, iri, &format!("{MDOC_NS}textContent"))
        .or_else(|| literal_object(store, iri, &format!("{MNEMO_NS}body")))
        .or_else(|| verbalize_relation(iri, store, cfg))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn literal_object(store: &Store, subject: &str, predicate: &str) -> Option<String> {
    let rows = select_solutions(
        store,
        &format!("SELECT ?value WHERE {{ <{subject}> <{predicate}> ?value }} LIMIT 1"),
    )
    .ok()?;
    rows.into_iter().find_map(|row| match row.get("value") {
        Some(OxTerm::Literal(literal)) => Some(literal.value().to_string()),
        _ => None,
    })
}

fn verbalize_relation(iri: &str, store: &Store, cfg: &VerbalizeCfg) -> Option<String> {
    let rows = select_solutions(
        store,
        &format!(
            "SELECT ?predicate ?source ?target WHERE {{
  <{iri}> <{RDF_TYPE}> <{WIRE_NS}Wire> ;
         <{WIRE_NS}predicate> ?predicate .
  OPTIONAL {{ <{iri}> <{WIRE_NS}sourceBlock> ?source }}
  OPTIONAL {{ <{iri}> <{WIRE_NS}targetBlock> ?target }}
}} LIMIT 1"
        ),
    )
    .ok()?;
    let row = rows.first()?;
    let predicate = named_node(row.get("predicate"))?;
    let short = predicate_short_name(&predicate);
    let label = predicate_label(&predicate);
    let category = relation_category(&short).unwrap_or("Relation");
    let mut parts = vec![format!("relation {label}"), format!("category {category}")];
    if cfg.relation_sample_limit > 0 {
        if let Some(source) = named_node(row.get("source")) {
            if let Some(label) = verbalize(&source, store, cfg) {
                parts.push(format!("head {label}"));
            }
        }
        if cfg.relation_sample_limit > 1 {
            if let Some(target) = named_node(row.get("target")) {
                if let Some(label) = verbalize(&target, store, cfg) {
                    parts.push(format!("tail {label}"));
                }
            }
        }
    }
    Some(parts.join("; "))
}

fn relation_category(short_name: &str) -> Option<&'static str> {
    crate::wire_predicates::builtin_wire_predicates()
        .into_iter()
        .find(|(name, _, _)| *name == short_name)
        .and_then(|(_, _, category)| (!category.is_empty()).then_some(category))
}

fn named_node(term: Option<&OxTerm>) -> Option<String> {
    match term {
        Some(OxTerm::NamedNode(node)) => Some(node.as_str().to_string()),
        _ => None,
    }
}

fn select_solutions(store: &Store, query: &str) -> Result<Vec<BTreeMap<String, OxTerm>>, String> {
    let solutions = match SparqlEvaluator::new()
        .parse_query(query)
        .map_err(|error| format!("parse verbalize query: {error}"))?
        .on_store(store)
        .execute()
        .map_err(|error| format!("execute verbalize query: {error}"))?
    {
        QueryResults::Solutions(solutions) => solutions,
        _ => return Err("verbalize query expected SELECT solutions".to_string()),
    };

    let mut rows = Vec::new();
    for solution in solutions {
        let solution = solution.map_err(|error| format!("read verbalize row: {error}"))?;
        let mut row = BTreeMap::new();
        for (name, term) in solution.iter() {
            row.insert(
                name.to_string().trim_start_matches('?').to_string(),
                term.clone(),
            );
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::io::{RdfFormat, RdfParser};

    fn store_from_turtle(ttl: &str) -> Store {
        let store = Store::new().expect("in-memory store");
        store
            .load_from_slice(RdfParser::from_format(RdfFormat::Turtle), ttl.as_bytes())
            .expect("load turtle");
        store
    }

    #[test]
    fn verbalize_reads_block_and_document_text_faces() {
        let store = store_from_turtle(&format!(
            r#"@prefix mdoc: <{MDOC_NS}> .
@prefix mnemo: <{MNEMO_NS}> .

<urn:mnemosyne:local:document:doc#block-alpha> mdoc:textContent "alpha beta gamma" .
<urn:mnemosyne:local:document:doc> mnemo:body "document body text" .
"#
        ));

        assert_eq!(
            verbalize(
                "urn:mnemosyne:local:document:doc#block-alpha",
                &store,
                &VerbalizeCfg::default()
            ),
            Some("alpha beta gamma".to_string())
        );
        assert_eq!(
            verbalize(
                "urn:mnemosyne:local:document:doc",
                &store,
                &VerbalizeCfg::default()
            ),
            Some("document body text".to_string())
        );
        assert_eq!(
            verbalize(
                "urn:mnemosyne:local:document:doc#unlabeled",
                &store,
                &VerbalizeCfg::default()
            ),
            None
        );
    }

    #[test]
    fn verbalize_relation_uses_predicate_category_and_sampled_endpoints() {
        let store = store_from_turtle(&format!(
            r#"@prefix mdoc: <{MDOC_NS}> .
@prefix wire: <{WIRE_NS}> .

<urn:wire:1>
  a wire:Wire ;
  wire:predicate wire:supports ;
  wire:sourceBlock <urn:mnemosyne:local:document:doc#block-a> ;
  wire:targetBlock <urn:mnemosyne:local:document:doc#block-b> .

<urn:mnemosyne:local:document:doc#block-a> mdoc:textContent "head block text" .
<urn:mnemosyne:local:document:doc#block-b> mdoc:textContent "tail block text" .
"#
        ));

        let text =
            verbalize("urn:wire:1", &store, &VerbalizeCfg::default()).expect("relation verbalizes");

        assert!(text.contains("relation supports"), "{text}");
        assert!(text.contains("category Quality"), "{text}");
        assert!(text.contains("head head block text"), "{text}");
        assert!(text.contains("tail tail block text"), "{text}");
    }
}

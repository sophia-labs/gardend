//! Fixtures cut from the three datasets the design was measured on (the Uruguay
//! catalogue's DCAT, the cow-tools Data Cube, a Wikidata slice): real shapes,
//! a few quads each, in a real Oxigraph store.

use super::*;
use oxigraph::io::{RdfFormat, RdfParser};

fn store(trig: &str) -> Store {
    let store = Store::new().expect("in-memory store");
    store
        .load_from_slice(RdfParser::from_format(RdfFormat::TriG), trig.as_bytes())
        .expect("load trig");
    store
}

fn run(store: &Store, graph_id: &str, langs: &[&str]) -> (Vec<EntityRecord>, VerbalizeStats) {
    let cfg = VerbalizeCfg {
        langs: langs.iter().map(|l| l.to_string()).collect(),
        graph_id: graph_id.to_string(),
        ..VerbalizeCfg::default()
    };
    let ctx = Ctx::new(store, cfg);
    let mut stats = VerbalizeStats::default();
    let candidates = ctx.candidates();
    stats.candidates = candidates.len();
    let records = candidates
        .iter()
        .filter_map(|iri| ctx.verbalize(iri, &mut stats))
        .collect();
    (records, stats)
}

fn find<'a>(records: &'a [EntityRecord], iri: &str) -> &'a EntityRecord {
    records
        .iter()
        .find(|r| r.iri == iri)
        .unwrap_or_else(|| panic!("no record for {iri}: {:?}", records.iter().map(|r| &r.iri).collect::<Vec<_>>()))
}

const CATALOGUE: &str = r#"
@prefix dcat: <http://www.w3.org/ns/dcat#> . @prefix dct: <http://purl.org/dc/terms/> .
@prefix foaf: <http://xmlns.com/foaf/0.1/> . @prefix skos: <http://www.w3.org/2004/02/skos/core#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
GRAPH <urn:sophia:garden:uruguay-catalogue:dcat> {
  <https://catalogodatos.gub.uy/> a dcat:Catalog ; dct:title "Catálogo Nacional de Datos Abiertos"@es ;
      dcat:dataset <https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe> .
  <https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe> a dcat:Dataset ;
      dct:title "Precios de Combustibles - Poder Ejecutivo"@es ;
      dct:description "Precios máximos para combustibles del mercado interno aprobados por el Poder Ejecutivo."@es ;
      dct:identifier "3f2a9c1e-77aa-4c51-9e0b-4b3f2d1c0a99" ;
      dct:publisher <https://catalogodatos.gub.uy/organization/ancap> ;
      dcat:theme <https://catalogodatos.gub.uy/group/energia> ;
      dcat:keyword "Combustibles"@es ;
      dcat:landingPage <https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe> ;
      dcat:distribution <https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe/resource/r1> .
  <https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe/resource/r1> a dcat:Distribution ;
      dct:title "Datos de precios de combustibles"@es ; dct:format "CSV" ;
      dcat:downloadURL <https://catalogodatos.gub.uy/dataset/x/resource/r1/download/precios.csv> .
  <https://catalogodatos.gub.uy/organization/ancap> a foaf:Organization ; foaf:name "ANCAP"@es .
  <https://catalogodatos.gub.uy/group/energia> a skos:Concept ; skos:prefLabel "Energía"@es, "Energy"@en .
}
GRAPH <urn:mnemosyne:local:graph:uruguay-catalogue:projection:document:about> {
  <urn:mnemosyne:local:document:about> rdfs:label "About this graph" .
}
"#;

#[test]
fn dcat_dataset_reads_through_publisher_theme_and_parts() {
    let s = store(CATALOGUE);
    let (records, _) = run(&s, "uruguay-catalogue", &["es", "en"]);
    let ds = find(&records, "https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe");
    assert_eq!(ds.kind, "dataset");
    assert_eq!(ds.label, "Precios de Combustibles - Poder Ejecutivo");
    for needle in ["publisher: ANCAP", "theme: Energía", "Datos de precios de combustibles", "Precios máximos"] {
        assert!(ds.text.contains(needle), "{needle:?} missing from {:?}", ds.text);
    }
    // ids, URLs and housekeeping never reach the text (the UUID guard and the skip list)
    for absent in ["3f2a9c1e", "https://", "identifier", "landing page"] {
        assert!(!ds.text.contains(absent), "{absent:?} leaked into {:?}", ds.text);
    }
    // the catalog's member list is noise, not a relation
    let catalog = find(&records, "https://catalogodatos.gub.uy/");
    assert!(!catalog.text.contains("Precios"), "{}", catalog.text);
    // a distribution is a part: it rolls up to its dataset and says so
    let dist = find(&records, "https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe/resource/r1");
    assert_eq!(dist.kind, "distribution");
    assert_eq!(dist.parent.as_deref(), Some("https://catalogodatos.gub.uy/dataset/ancap-precio-combustible-pe"));
    assert!(dist.text.contains("part of: Precios de Combustibles"), "{}", dist.text);
    // the theme's other-language label is an alternate
    let theme = find(&records, "https://catalogodatos.gub.uy/group/energia");
    assert!(theme.text.starts_with("Energía (Energy)"), "{}", theme.text);
    // Sophia's own projection graphs belong to the block index, not here
    assert!(records.iter().all(|r| r.iri != "urn:mnemosyne:local:document:about"));
}

const CUBE: &str = r#"
@prefix cow: <https://sophia.dev/ontology/cattle-aggregates#> . @prefix qb: <http://purl.org/linked-data/cube#> .
@prefix skos: <http://www.w3.org/2004/02/skos/core#> . @prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
GRAPH <urn:sophia:garden:uruguay-cows:cow:ontology> {
  cow:AggregateObservation a owl:Class ; rdfs:subClassOf qb:Observation .
  cow:AnimalFlowObservation a owl:Class ; rdfs:subClassOf cow:AggregateObservation .
  cow:DatasetSnapshot a owl:Class ; rdfs:subClassOf qb:DataSet .
  cow:Department a owl:Class .
  cow:AnimalCategory a owl:Class .
  cow:department a qb:DimensionProperty, owl:ObjectProperty ; rdfs:range cow:Department .
  cow:animalsCount a qb:MeasureProperty, owl:DatatypeProperty ; rdfs:range xsd:integer .
  cow:dsd a qb:DataStructureDefinition ;
      qb:component [ qb:dimension cow:department ], [ qb:measure cow:animalsCount ] .
  cow:metric_bovine_count a cow:MetricDefinition ; skos:prefLabel "Existencias bovinas"@es, "Bovine count"@en .
}
GRAPH <urn:sophia:garden:uruguay-cows:cow:codelists:2025> {
  <urn:sophia:cow:animal-category/2/3> a cow:AnimalCategory, skos:Concept ;
      skos:prefLabel "3) VACAS SECAS"@es ; skos:notation "3" .
  <urn:sophia:cow:department/10> a cow:Department, skos:Concept ; skos:prefLabel "MONTEVIDEO"@es ; skos:notation "10" .
  <urn:sophia:cow:enumeration-area/1001001> a cow:EnumerationArea, skos:Concept ;
      skos:prefLabel "1001001" ; cow:department <urn:sophia:cow:department/10> .
}
GRAPH <urn:sophia:garden:uruguay-cows:cow:sources> {
  <urn:sophia:cow:snapshot/2025> a cow:DatasetSnapshot ; qb:structure cow:dsd ; cow:sourceYear "2025"^^xsd:gYear .
}
GRAPH <urn:sophia:garden:uruguay-cows:cow:observations:2025> {
  <urn:sophia:cow:obs/1> a cow:AnimalFlowObservation ; rdfs:label "an observation that happens to be labelled" ;
      cow:department <urn:sophia:cow:department/10> ; cow:animalsCount 2736 .
}
"#;

#[test]
fn data_cube_indexes_structure_and_code_lists_never_observations() {
    let s = store(CUBE);
    let (records, stats) = run(&s, "uruguay-cows", &["es", "en"]);
    // the graph's own subclass declarations deny the observation, labelled or not
    assert!(records.iter().all(|r| r.iri != "urn:sophia:cow:obs/1"));
    assert_eq!(stats.skipped_denied_type, 1);
    // shouting code-list labels are un-shouted in the text and lose their enumerator; the label is kept
    let dry = find(&records, "urn:sophia:cow:animal-category/2/3");
    assert_eq!(dry.label, "3) VACAS SECAS");
    assert!(dry.text.starts_with("Vacas secas [code 3] — animal category"), "{}", dry.text);
    // an unlabelled schema thing is named by its local name
    let class = find(&records, "https://sophia.dev/ontology/cattle-aggregates#Department");
    assert_eq!(class.label, "department");
    assert_eq!(class.kind, "class");
    // an unlabelled typed instance with a code reads as "<type> <code>"; a dataset says its dimensions and measures
    let snap = find(&records, "urn:sophia:cow:snapshot/2025");
    assert_eq!(snap.label, "dataset snapshot 2025");
    assert_eq!(snap.kind, "dataset");
    assert!(snap.text.contains("dimensions: department; measures: animals count"), "{}", snap.text);
    // a code-like label gets its type in front, and relations render through labels
    let area = find(&records, "urn:sophia:cow:enumeration-area/1001001");
    assert!(area.label.starts_with("enumeration area"), "{}", area.label);
    assert!(area.text.contains("department: Montevideo"), "{}", area.text);
    // language preference picks the label; the other language is an alternate
    let metric = find(&records, "https://sophia.dev/ontology/cattle-aggregates#metric_bovine_count");
    assert!(metric.text.starts_with("Existencias bovinas (Bovine count)"), "{}", metric.text);
}

const WIKIDATA: &str = r#"
@prefix wd: <http://www.wikidata.org/entity/> . @prefix wdt: <http://www.wikidata.org/prop/direct/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> . @prefix schema: <http://schema.org/> .
@prefix wikibase: <http://wikiba.se/ontology#> . @prefix skos: <http://www.w3.org/2004/02/skos/core#> .
GRAPH <urn:mnemosyne:local:graph:wd:user:rdf> {
  wd:Q167790 rdfs:label "Edinson Cavani"@en, "Edinson Cavani"@es ; skos:altLabel "Edinson Roberto Cavani Gomez"@en ;
      schema:description "Uruguayan association football player"@en, "futbolista uruguayo"@es ;
      wdt:P31 wd:Q5 ; wdt:P19 wd:Q1000 ; wdt:P54 wd:Q18656, wd:Q999999999 .
  wd:Q5 rdfs:label "human"@en .
  wd:Q1000 rdfs:label "Salto"@es .
  wd:Q18656 rdfs:label "Manchester United F.C."@en .
  wd:P19 wikibase:directClaim wdt:P19 ; rdfs:label "place of birth"@en .
  wd:P54 wikibase:directClaim wdt:P54 ; rdfs:label "member of sports team"@en .
  wd:Q77 schema:description "a description with no name"@en .
}
"#;

#[test]
fn wikidata_types_predicates_and_unnamed_objects() {
    let s = store(WIKIDATA);
    let (records, stats) = run(&s, "wd", &["en", "es"]);
    let cavani = find(&records, "http://www.wikidata.org/entity/Q167790");
    assert_eq!(cavani.types, vec!["human".to_string()]);
    assert!(cavani.text.starts_with("Edinson Cavani (Edinson Roberto Cavani Gomez) — human."), "{}", cavani.text);
    for needle in ["Uruguayan association football player", "futbolista uruguayo", "place of birth: Salto", "member of sports team: Manchester United F.C."] {
        assert!(cavani.text.contains(needle), "{needle:?} missing from {:?}", cavani.text);
    }
    // an object with no label is dropped, never shown as Q999999999
    assert!(!cavani.text.contains("Q999999999"), "{}", cavani.text);
    // described but unnamed: skipped (its local name is an id)
    assert!(records.iter().all(|r| r.iri != "http://www.wikidata.org/entity/Q77"));
    assert!(stats.skipped_no_label >= 1);
}

#[test]
fn the_uuid_guard() {
    for text in ["Q42", "P31", "3f2a9c1e-77aa-4c51-9e0b-4b3f2d1c0a99", "https://example.org/x", "urn:x:y", "12", "a1b2c3d4e5f6a7b8"] {
        assert!(!word_like(text), "{text} read as words");
    }
    for text in ["Salto", "Precios de combustibles", "Area 0", "ANCAP", "dataset snapshot 2025"] {
        assert!(word_like(text), "{text} not read as words");
    }
    assert_eq!(humanize("accrualPeriodicity"), "accrual periodicity");
    assert_eq!(humanize("scheme_land_tenures"), "scheme land tenures");
    assert_eq!(text_name("2) VACAS DE CRIA (ENTORADAS)"), "Vacas de cria (entoradas)");
    assert_eq!(text_name("1a. Praderas"), "Praderas");
    assert_eq!(text_name("ANCAP"), "ANCAP");
    assert_eq!(text_name("YEGUARIZOS"), "Yeguarizos");
    assert_eq!(text_name("Datos de precios"), "Datos de precios");
}

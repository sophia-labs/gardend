//! The faces one class derives (`type_catalog`, `shacl_emit`), pinned on a
//! realistic class: the dream report of plans/2026-10-20-typed-objects, ten
//! fields with one nested class. No store: the contract is built by the same
//! compact-form builder `propose_domain_ontology` uses, and checked through
//! the real gate, the real SHACL derivation and the real rudof validator.
use super::contract::VocabularyContract;
use super::shacl_emit::{class_to_shacl, vocab_to_shacl};
use super::shacl_validator::compile_check_contract;
use super::type_catalog::*;
use serde_json::{json, Value};

const GRAPH: &str = "freud-dreams";
const NS: &str = "urn:sophia:type:freud-dreams:workspace#";

/// The dream report, compact form (what an agent sends).
pub(super) fn dream_report_classes() -> Vec<Value> {
    vec![
        json!({
            "name": "DreamReport",
            "label": "Dream report",
            "plural": "Dream reports",
            "description": "A dream an experimenter induced in a sleeper, as the book reports it.",
            "fields": [
                { "key": "who", "label": "Who", "datatype": "string", "required": true,
                  "description": "The sleeper, as the book names them" },
                { "key": "procedure", "label": "What was done", "datatype": "string", "required": true,
                  "description": "What the experimenter did to the sleeper" },
                { "key": "dream", "label": "The dream", "datatype": "string", "required": true },
                { "key": "quote", "label": "Quote", "datatype": "string", "required": true,
                  "description": "The book's words, copied exactly" },
                { "key": "documentId", "label": "Document", "datatype": "string", "required": true },
                { "key": "blockId", "label": "Block", "datatype": "string" },
                { "key": "page", "label": "Page", "datatype": "integer" },
                { "key": "recurring", "label": "Recurring", "datatype": "boolean" },
                { "key": "source", "label": "Source", "datatype": "uri",
                  "description": "Where the book has it from" },
                { "key": "place", "label": "Where", "datatype": "object", "class": "Place",
                  "description": "Where the sleeper slept" }
            ],
            "presentation": {
                "title": "who",
                "subtitle": "procedure",
                "kinds": { "quote": "testimony", "page": "metric" },
                "roles": { "quote": "quote", "note": "dream", "document": "documentId", "block": "blockId" }
            }
        }),
        json!({
            "name": "Place",
            "label": "Place",
            "plural": "Places",
            "fields": [
                { "key": "city", "label": "City", "datatype": "string", "required": true },
                { "key": "house", "label": "House", "datatype": "string" }
            ]
        }),
    ]
}

pub(super) fn dream_report_contract_json(version: &str) -> Value {
    compact_types_contract(
        GRAPH,
        DEFAULT_TYPES_VOCAB,
        version,
        None,
        None,
        &dream_report_classes(),
        &[],
        None,
    )
    .expect("the compact dream report builds")
}

fn contract() -> VocabularyContract {
    serde_json::from_value(dream_report_contract_json("1")).expect("it parses as a contract")
}

#[test]
fn the_compact_form_fills_in_every_emporium_detail() {
    let built = dream_report_contract_json("1");
    assert_eq!(built["name"], "workspace");
    assert_eq!(built["primary_prefix"], "t-freud-dreams");
    assert_eq!(built["namespaces"]["t-freud-dreams"], NS);
    assert_eq!(built["write_target"], "projection:types");
    let class = &built["classes"]["DreamReport"];
    assert_eq!(class["rdf_types"], json!(["t-freud-dreams:DreamReport"]));
    assert_eq!(
        class["subject_rule"],
        "{graph_subject}:projection:types:dream-report:{localId}"
    );
    assert_eq!(class["identity_kind"], "logical-id");
    // The fields' order becomes the presentation's fieldOrder.
    assert_eq!(
        class["presentation"]["fieldOrder"],
        json!([
            "who",
            "procedure",
            "dream",
            "quote",
            "documentId",
            "blockId",
            "page",
            "recurring",
            "source",
            "place"
        ])
    );
    let parsed = contract();
    for class in ["DreamReport", "Place"] {
        parsed
            .materialization_signature(class)
            .unwrap_or_else(|e| panic!("{class} has a valid signature: {e}"));
    }
    validate_type_contract(&parsed).expect("the gate accepts it");
    compile_check_contract(&parsed).expect("its shapes compile");
    assert_eq!(
        class_iri(&parsed, "DreamReport").unwrap(),
        format!("{NS}DreamReport")
    );
}

/// The JSON Schema of the dream report, byte for byte: what a workflow step
/// with this itemClass hands its agents.
#[test]
fn the_dream_reports_json_schema_is_pinned_and_under_the_cap() {
    let schema = contract_class_to_json_schema(&contract(), "DreamReport");
    let expected = json!({
        "type": "object",
        "title": "Dream report",
        "description": "A dream an experimenter induced in a sleeper, as the book reports it.",
        "properties": {
            "who": { "type": "string", "description": "Who: The sleeper, as the book names them" },
            "procedure": { "type": "string", "description": "What was done: What the experimenter did to the sleeper" },
            "dream": { "type": "string", "description": "The dream" },
            "quote": { "type": "string", "description": "Quote: The book's words, copied exactly" },
            "documentId": { "type": "string", "description": "Document" },
            "blockId": { "type": "string", "description": "Block" },
            "page": { "type": "integer", "description": "Page" },
            "recurring": { "type": "boolean", "description": "Recurring" },
            "source": { "type": "string", "description": "Source: Where the book has it from (an absolute IRI)" },
            "place": {
                "type": "object",
                "title": "Place",
                "description": "Where: Where the sleeper slept",
                "properties": {
                    "city": { "type": "string", "description": "City" },
                    "house": { "type": "string", "description": "House" }
                },
                "required": ["city"],
                "additionalProperties": false
            }
        },
        "required": ["who", "procedure", "dream", "quote", "documentId"],
        "additionalProperties": false
    });
    assert_eq!(
        schema,
        expected,
        "{}",
        serde_json::to_string_pretty(&schema).unwrap()
    );
    // Key order is the presentation order (serde_json preserves it).
    let keys: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    assert_eq!(keys[0], "who");
    assert_eq!(keys[9], "place");
    let size = schema.to_string().len();
    assert!(size < ITEM_SCHEMA_MAX_CHARS, "{size} characters");
    assert!(schema_size_warnings(&contract()).is_empty());
    // Deterministic.
    assert_eq!(
        schema,
        contract_class_to_json_schema(&contract(), "DreamReport")
    );
}

#[test]
fn twelve_described_fields_still_fit_the_cap_and_a_huge_class_is_warned() {
    let fields: Vec<Value> = (0..12)
        .map(|i| json!({ "key": format!("field{i}"), "label": format!("Field number {i}"), "datatype": "string",
                         "required": true, "description": "A sentence of about sixty characters that says what it holds." }))
        .collect();
    let built = compact_types_contract(
        GRAPH,
        "wide",
        "1",
        None,
        None,
        &[json!({ "name": "Wide", "label": "Wide", "fields": fields })],
        &[],
        None,
    )
    .unwrap();
    let parsed: VocabularyContract = serde_json::from_value(built).unwrap();
    let size = contract_class_to_json_schema(&parsed, "Wide")
        .to_string()
        .len();
    assert!(
        size < ITEM_SCHEMA_MAX_CHARS,
        "12 described fields: {size} characters"
    );

    let fields: Vec<Value> = (0..40)
        .map(|i| {
            json!({ "key": format!("field{i}"), "datatype": "string",
                         "description": "x".repeat(100) })
        })
        .collect();
    let built = compact_types_contract(
        GRAPH,
        "huge",
        "1",
        None,
        None,
        &[json!({ "name": "Huge", "fields": fields })],
        &[],
        None,
    )
    .unwrap();
    let parsed: VocabularyContract = serde_json::from_value(built).unwrap();
    let warnings = schema_size_warnings(&parsed);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("cannot be a step's itemClass"),
        "{warnings:?}"
    );
}

#[test]
fn shacl_carries_the_labels_and_the_nested_shape() {
    let parsed = contract();
    let shacl = class_to_shacl(&parsed, "DreamReport");
    assert!(
        shacl.contains("<http://www.w3.org/2000/01/rdf-schema#label> \"Dream report\""),
        "{shacl}"
    );
    assert!(shacl.contains("sh:name \"Who\""), "{shacl}");
    assert!(
        shacl.contains("sh:description \"The sleeper, as the book names them\""),
        "{shacl}"
    );
    // The nested field: a node of Place, which must conform to Place's shape.
    let place = shacl
        .find(&format!("sh:path <{NS}place>"))
        .expect("place property");
    let block = &shacl[place..place + 300];
    assert!(block.contains("sh:nodeKind sh:BlankNodeOrIRI"), "{block}");
    // sh:class, not sh:node: see shacl_emit (rudof's closed-shape defect).
    assert!(block.contains(&format!("sh:class <{NS}Place>")), "{block}");
    assert!(!shacl.contains("sh:node "), "{shacl}");
    assert!(block.contains("sh:maxCount 1"), "{block}");
    // class_to_shacl brings the nested shape along; vocab_to_shacl has both too.
    assert!(
        shacl.contains("<urn:sophia:shacl:workspace#PlaceShape> a sh:NodeShape"),
        "{shacl}"
    );
    assert!(
        vocab_to_shacl(&parsed).contains("<urn:sophia:shacl:workspace#PlaceShape> a sh:NodeShape")
    );
    compile_check_contract(&parsed).expect("labels and sh:node compile");
}

#[test]
fn a_built_in_golden_derives_the_same_shapes_as_before() {
    // No golden carries a label: the derived SHACL of a built-in is unchanged
    // (no sh:name, no rdfs:label), and its catalogue label falls back.
    let bookmark = super::contract::get_vocabulary("emporium-bookmark").unwrap();
    let shacl = vocab_to_shacl(bookmark);
    assert!(!shacl.contains("sh:name"));
    assert!(!shacl.contains("rdf-schema#label"));
    assert!(shacl.starts_with("@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\n<urn:sophia:shacl:emporium-bookmark#BookmarkShape> a sh:NodeShape ;\n"));
    let entry = class_catalog_entry(bookmark, "Bookmark", "1.0.0");
    assert_eq!(entry["label"], "Bookmark");
    assert_eq!(entry["presentation"], json!({}));
    assert_eq!(entry["iri"], "http://mnemosyne.dev/bookmark#Bookmark");
    let url = entry["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "url")
        .unwrap();
    assert_eq!(url["datatype"], "uri");
    assert_eq!(url["label"], "url");
    assert_eq!(entry["jsonSchema"]["title"], "Bookmark");
}

#[test]
fn the_catalogue_entry_has_the_shape_shrubbery_reads() {
    let entry = class_catalog_entry(&contract(), "DreamReport", "1");
    assert_eq!(entry["iri"], format!("{NS}DreamReport"));
    assert_eq!(entry["name"], "DreamReport");
    assert_eq!(entry["vocab"], "workspace");
    assert_eq!(entry["version"], "1");
    assert_eq!(entry["label"], "Dream report");
    assert_eq!(entry["plural"], "Dream reports");
    assert_eq!(entry["presentation"]["title"], "who");
    assert_eq!(entry["presentation"]["roles"]["note"], "dream");
    let fields = entry["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 10);
    assert_eq!(
        fields[0],
        json!({
            "key": "who", "iri": format!("{NS}who"), "label": "Who",
            "description": "The sleeper, as the book names them",
            "datatype": "string", "required": true, "multi": false,
        })
    );
    let place = fields.iter().find(|f| f["key"] == "place").unwrap();
    assert_eq!(place["datatype"], "object");
    assert_eq!(place["class"], format!("{NS}Place"));
    assert!(fields
        .iter()
        .filter(|f| f["key"] != "place")
        .all(|f| f.get("class").is_none()));
}

fn gate(classes: Vec<Value>) -> Result<(), String> {
    let built = compact_types_contract(GRAPH, "workspace", "1", None, None, &classes, &[], None)?;
    let parsed: VocabularyContract =
        serde_json::from_value(built).map_err(|e| format!("parse: {e}"))?;
    validate_type_contract(&parsed)
}

#[test]
fn the_gate_refuses_a_cycle_a_missing_class_and_nesting_past_three() {
    let cycle = gate(vec![
        json!({ "name": "A", "fields": [{ "key": "b", "datatype": "object", "class": "B" }] }),
        json!({ "name": "B", "fields": [{ "key": "a", "datatype": "object", "class": "A" }] }),
    ])
    .expect_err("a cycle is refused");
    assert!(cycle.contains("cycle (A → B → A"), "{cycle}");

    let selfish = gate(vec![json!({ "name": "Node", "fields": [
        { "key": "next", "datatype": "object", "class": "Node", "multi": true }] })])
    .expect_err("a class that contains itself is refused");
    assert!(selfish.contains("cycle"), "{selfish}");

    let missing = gate(vec![json!({ "name": "A", "fields": [
        { "key": "b", "datatype": "object", "class": "Nowhere" }] })])
    .expect_err("an unknown nested class is refused");
    assert!(
        missing.contains("nests Nowhere, which is not a class of vocabulary workspace"),
        "{missing}"
    );

    let nameless = gate(vec![
        json!({ "name": "A", "fields": [{ "key": "b", "datatype": "object" }] }),
    ])
    .expect_err("an object with no class is refused");
    assert!(nameless.contains("names no class"), "{nameless}");

    let chain = |n: usize| -> Vec<Value> {
        (0..n)
            .map(|i| {
                if i + 1 < n {
                    json!({ "name": format!("L{i}"), "fields": [
                        { "key": "x", "datatype": "string" },
                        { "key": "down", "datatype": "object", "class": format!("L{}", i + 1) }] })
                } else {
                    json!({ "name": format!("L{i}"), "fields": [{ "key": "x", "datatype": "string" }] })
                }
            })
            .collect()
    };
    gate(chain(4)).expect("three levels below the top are allowed");
    let deep = gate(chain(5)).expect_err("four levels are refused");
    assert!(deep.contains("nests 4 levels deep"), "{deep}");
}

#[test]
fn the_gate_refuses_a_presentation_that_names_what_is_not_there() {
    let with = |presentation: Value| {
        gate(vec![json!({ "name": "A", "fields": [
            { "key": "text", "datatype": "string" }, { "key": "n", "datatype": "integer" }],
            "presentation": presentation })])
    };
    with(json!({ "title": "text", "roles": { "quote": "text" }, "kinds": { "n": "metric" } }))
        .expect("a good presentation");
    let title = with(json!({ "title": "nope" })).unwrap_err();
    assert!(
        title.contains("presentation.title names \"nope\""),
        "{title}"
    );
    let kind = with(json!({ "kinds": { "text": "sparkly" } })).unwrap_err();
    assert!(kind.contains("use one of identity, state"), "{kind}");
    let role = with(json!({ "roles": { "note": "n" } })).unwrap_err();
    assert!(role.contains("must be one text field"), "{role}");
    // An unknown presentation key is refused at parse (closed data).
    let unknown = with(json!({ "colour": "red" })).unwrap_err();
    assert!(unknown.contains("unknown field"), "{unknown}");
}

#[test]
fn a_fitting_item_projects_to_typed_triples_and_a_misfit_names_its_paths() {
    let parsed = contract();
    let run = "urn:sophia:wf:wf-1:run:r1";
    let item = format!("{run}#item-1");
    let fitting = json!({
        "who": "Meier", "procedure": "a blade of straw drawn between his toes",
        "dream": "he was being tortured by Arab pirates", "quote": "between his toes",
        "documentId": "freud-ch1", "page": "23", "recurring": false,
        "source": "urn:book:maury",
        "place": { "city": "Paris", "house": "the rue de l'Université" }
    });
    let triples = project_item(&parsed, "DreamReport", &item, &fitting, run).expect("it fits");
    let has = |s: &str, p: &str, o: &str| {
        triples
            .iter()
            .any(|(ts, tp, to)| ts == s && tp == p && to.as_nt() == o)
    };
    assert!(has(
        &item,
        "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
        &format!("<{NS}DreamReport>")
    ));
    assert!(has(&item, &format!("{NS}who"), "\"Meier\""));
    assert!(has(
        &item,
        &format!("{NS}page"),
        "\"23\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    ));
    assert!(has(
        &item,
        &format!("{NS}recurring"),
        "\"false\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
    ));
    assert!(has(
        &item,
        &format!("{NS}place"),
        &format!("<{item}/place>")
    ));
    assert!(has(
        &format!("{item}/place"),
        "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
        &format!("<{NS}Place>")
    ));
    assert!(has(
        &format!("{item}/place"),
        &format!("{NS}city"),
        "\"Paris\""
    ));
    assert!(triples
        .iter()
        .all(|(s, _, _)| s == &item || s.starts_with(&format!("{item}/"))));

    let misfit = json!({
        "who": "Meier", "procedure": "x", "quote": "y", "documentId": "d",
        "page": "twenty", "recurring": "yes", "source": "not an iri",
        "place": { "house": 7, "zip": "75007" },
        "mood": "anxious"
    });
    let problems =
        project_item(&parsed, "DreamReport", &item, &misfit, run).expect_err("it does not fit");
    let said = |text: &str| problems.iter().any(|p| p.contains(text));
    assert!(
        said("dream is missing; a Dream report needs it"),
        "{problems:?}"
    );
    assert!(
        said("page: expected a whole number, got \"twenty\""),
        "{problems:?}"
    );
    assert!(
        said("recurring: expected true or false, got \"yes\""),
        "{problems:?}"
    );
    assert!(said("source: expected an absolute IRI"), "{problems:?}");
    assert!(said("mood is not a field of Dream report"), "{problems:?}");
    assert!(
        said("place.city is missing; a Place needs it"),
        "{problems:?}"
    );
    assert!(said("place.zip is not a field of Place"), "{problems:?}");
    // A number is acceptable text (it is not repaired, it is said as text).
    assert!(!said("place.house"), "{problems:?}");
}

#[test]
fn the_generic_write_lane_refuses_a_nested_object_instead_of_minting_a_bad_iri() {
    let parsed = contract();
    let record: super::schemas::GenericRecordIn = serde_json::from_value(json!({
        "kind": "DreamReport", "localId": "meier", "who": "Meier", "procedure": "p",
        "dream": "d", "quote": "q", "documentId": "freud-ch1",
        "place": { "city": "Paris" }
    }))
    .unwrap();
    let error = super::planner::plan_generic_compute(&parsed, GRAPH, &[record])
        .expect_err("a nested object is refused by emporium_write");
    assert!(
        error.0.contains("place is a nested Place object"),
        "{}",
        error.0
    );
    // The IRI of an existing node is what this lane takes for it.
    let record: super::schemas::GenericRecordIn = serde_json::from_value(json!({
        "kind": "DreamReport", "localId": "meier", "who": "Meier", "procedure": "p",
        "dream": "d", "quote": "q", "documentId": "freud-ch1",
        "place": "urn:example:place:paris"
    }))
    .unwrap();
    super::planner::plan_generic_compute(&parsed, GRAPH, &[record]).expect("an IRI is taken");
}

#[test]
fn a_nested_list_is_one_typed_child_per_member_and_the_shape_judges_each() {
    let built = compact_types_contract(
        GRAPH,
        "workspace",
        "1",
        None,
        None,
        &[
            json!({ "name": "Report", "fields": [
            { "key": "who", "datatype": "string", "required": true },
            { "key": "witnesses", "datatype": "object", "class": "Witness", "multi": true }] }),
            json!({ "name": "Witness", "fields": [
            { "key": "name", "datatype": "string", "required": true },
            { "key": "age", "datatype": "integer" }] }),
        ],
        &[],
        None,
    )
    .unwrap();
    let parsed: VocabularyContract = serde_json::from_value(built).unwrap();
    validate_type_contract(&parsed).unwrap();
    let run = "urn:sophia:wf:wf-1:run:r1";
    let item = format!("{run}#item-3");
    let triples = project_item(
        &parsed,
        "Report",
        &item,
        &json!({
            "who": "Meier", "witnesses": [{ "name": "Ann", "age": 7 }, { "name": "Bo" }]
        }),
        run,
    )
    .expect("it fits");
    let children: Vec<&str> = triples
        .iter()
        .filter(|(s, p, _)| s == &item && p == &format!("{NS}witnesses"))
        .map(|(_, _, o)| match o {
            super::terms::Term::Uri(node) => node.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(
        children,
        vec![format!("{item}/witnesses/0"), format!("{item}/witnesses/1")]
    );
    assert!(triples
        .iter()
        .any(|(s, p, o)| s == &format!("{item}/witnesses/1")
            && p == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
            && o.as_nt() == format!("<{NS}Witness>")));
    let problems = project_item(
        &parsed,
        "Report",
        &item,
        &json!({
            "who": "Meier", "witnesses": [{ "name": "Ann" }, { "age": "old" }]
        }),
        run,
    )
    .expect_err("the second witness does not fit");
    assert!(
        problems.contains(&"witnesses[1].name is missing; a Witness needs it".to_string()),
        "{problems:?}"
    );
    assert!(
        problems.contains(&"witnesses[1].age: expected a whole number, got \"old\"".to_string()),
        "{problems:?}"
    );
    // A lone value for a list is one member; a list for a single field is not.
    project_item(
        &parsed,
        "Report",
        &item,
        &json!({ "who": "M", "witnesses": { "name": "Ann" } }),
        run,
    )
    .expect("a lone member");
    let error =
        project_item(&parsed, "Report", &item, &json!({ "who": ["M", "N"] }), run).unwrap_err();
    assert_eq!(
        error,
        vec!["who: expected one value, got a list".to_string()]
    );
}

#[test]
fn the_shape_itself_refuses_a_child_of_the_wrong_class() {
    // The derived shapes, run by the real validator over hand-made triples: a
    // nested node must be typed with the nested class (sh:class), and a
    // fitting item conforms (the regression the sh:node form failed).
    use super::terms::Term;
    use oxigraph::model::{Literal, NamedNode};
    let parsed = contract();
    let shapes = class_to_shacl(&parsed, "DreamReport");
    let run = "urn:sophia:wf:wf-1:run:r1";
    let item = format!("{run}#item-1");
    let uri = |s: &str| Term::Uri(NamedNode::new(s).unwrap());
    let text = |s: &str| Term::Lit(Literal::new_simple_literal(s));
    let rdf_type = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string();
    let mut triples = vec![(
        item.clone(),
        rdf_type.clone(),
        uri(&format!("{NS}DreamReport")),
    )];
    for key in ["who", "procedure", "dream", "quote", "documentId"] {
        triples.push((item.clone(), format!("{NS}{key}"), text("x")));
    }
    let child = format!("{item}/place");
    triples.push((item.clone(), format!("{NS}place"), uri(&child)));
    triples.push((child.clone(), format!("{NS}city"), text("Paris")));
    let untyped =
        super::shacl_validator::validate_against_shapes(&shapes, &triples, run, "workspace")
            .expect_err("an untyped child is refused");
    assert!(
        untyped
            .iter()
            .any(|v| v.message.contains("Class constraint")),
        "{untyped:?}"
    );
    triples.push((child.clone(), rdf_type, uri(&format!("{NS}Place"))));
    super::shacl_validator::validate_against_shapes(&shapes, &triples, run, "workspace")
        .expect("a typed child conforms");
}

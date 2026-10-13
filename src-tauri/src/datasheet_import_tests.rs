//! Datasheets slice 1: the importer and `datasheet_facts`, against REAL
//! Oxigraph stores (RocksDB in a disposable directory) and, for the dispatch
//! case, a real cell and the exact `tools/call` registry path. No mocks of our
//! own systems.
//!
//! The fixtures under `datasheet_fixtures/` are hand-written to NomDeTom's
//! `autonotes.data/2` schema (`sidecars.py` at `ee680a6`): invented devices,
//! invented values. Mounted under `emporium::` so the `cells` lane's
//! `emporium::` filter runs them.

use std::collections::BTreeSet;

use oxigraph::model::{GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Quad, Term};
use oxigraph::store::Store;
use serde_json::{json, Value};

use crate::datasheet_facts::{facts_from_store, obj_of, FactsQuery};
use crate::datasheet_import::{
    de, eng, find_family, import_into_store, norm_ident, vocabulary_graph_iri, ImportError,
    ImportInput, Obj, PdfCheck, MAX_NODES, MAX_TRIPLES, VOCAB_TTL,
};

const GID: &str = "datasheet-lab";
const BUCK: &str = include_str!("datasheet_fixtures/buck.data.json");
const CHARGER: &str = include_str!("datasheet_fixtures/charger.data.json");
const LDO: &str = include_str!("datasheet_fixtures/ldo-scan.data.json");
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture parses")
}

fn input(v: Value) -> ImportInput {
    ImportInput::from_inline(v)
}

fn with_store<R>(f: impl FnOnce(&Store) -> R) -> R {
    let dir = std::env::temp_dir().join(format!("garden-datasheet-import-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir).expect("open a real store");
    let out = f(&store);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn rec(local: &str) -> String {
    format!("urn:mnemosyne:local:graph:{GID}:record:{local}")
}

fn sha16(env: &Value) -> String {
    env["document"]["sha256"].as_str().unwrap()[..16].to_string()
}

fn node_of(env: &Value, path: &str) -> String {
    rec(&format!("ds-{}-{path}", sha16(env)))
}

fn nn(iri: &str) -> NamedNode {
    NamedNode::new(iri).expect("IRI")
}

fn objs(store: &Store, graph: &str, s: &str, p: &str) -> Vec<Obj> {
    let (s, p, g) = (nn(s), nn(p), nn(graph));
    let mut out: Vec<Obj> = store
        .quads_for_pattern(
            Some(s.as_ref().into()),
            Some(p.as_ref()),
            None,
            Some(GraphNameRef::NamedNode(g.as_ref())),
        )
        .map(|q| obj_of(&q.expect("quad").object))
        .collect();
    out.sort();
    out
}

fn graph_len(store: &Store, graph: &str) -> usize {
    let g = nn(graph);
    store
        .quads_for_pattern(None, None, None, Some(GraphNameRef::NamedNode(g.as_ref())))
        .count()
}

fn graph_dump(store: &Store, graph: &str) -> Vec<String> {
    let g = nn(graph);
    let mut v: Vec<String> = store
        .quads_for_pattern(None, None, None, Some(GraphNameRef::NamedNode(g.as_ref())))
        .map(|q| q.expect("quad").to_string())
        .collect();
    v.sort();
    v
}

fn refused(e: ImportError) -> Vec<String> {
    match e {
        ImportError::Refused(reasons) => reasons,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn dec(lexical: &str) -> Obj {
    Obj::Typed(lexical.into(), format!("{XSD}decimal"))
}
fn typed(lexical: &str, datatype: String) -> Obj {
    Obj::Typed(lexical.into(), datatype)
}

fn facts(store: &Store, part: Option<&str>, artifact: Option<&str>, parameter: Option<&str>) -> Value {
    facts_from_store(
        store,
        GID,
        &FactsQuery {
            part: part.map(str::to_string),
            artifact: artifact.map(str::to_string),
            parameter: parameter.map(str::to_string),
            limit: 200,
        },
    )
    .expect("facts")
}

fn claim<'a>(v: &'a Value, edition: usize, kind: &str, parameter: &str) -> &'a Value {
    v["editions"][edition]["claims"]
        .as_array()
        .expect("claims")
        .iter()
        .find(|c| c["kind"] == kind && (c["parameter"] == parameter || c["symbol"] == parameter))
        .unwrap_or_else(|| panic!("no {kind} claim {parameter} in {v}"))
}

// ------------------------------------------------------------------ import

#[test]
fn buck_fixture_maps_to_light_records_with_typed_literals() {
    with_store(|store| {
        let env = fixture(BUCK);
        let report = import_into_store(store, GID, &input(env.clone())).expect("import");
        assert!(!report.replaced);
        let c = &report.mapped.counts;
        // Custody: ByteContent, binding, one rendition (dataJson), attempt,
        // import record, ToolProfile.
        assert_eq!(c["custodyNodes"], 7);
        assert_eq!(c["families"], 1);
        assert_eq!(c["params"], 4);
        // vin 4 + vout 2 + iout 2 + fsw 1 readings.
        assert_eq!(c["readings"], 9);
        // 7 rows; the lone pdfplumber row is an alternative reading kept in
        // the envelope, not a node.
        assert_eq!(c["rows"], 6);
        assert_eq!(c["alternativeReaderRows"], 1);
        // 7 custody (6 + the Docling ToolProfile) + edition + family + (5+3+3+2) param nodes + 6 rows.
        assert_eq!(report.mapped.nodes, 28);
        assert!(report.mapped.triples.len() > 100, "{}", report.mapped.triples.len());
        assert!(report.mapped.nodes <= MAX_NODES && report.mapped.triples.len() <= MAX_TRIPLES);
        assert_eq!(graph_len(store, &report.graph_iri), report.mapped.triples.len());
        assert!(report
            .graph_iri
            .ends_with(&format!(":projection:datasheet:ds-{}:m1", sha16(&env))));
        assert!(report
            .mapped
            .diagnostics
            .contains(&"on demand: 1 alternative-reader rows".to_string()));

        let g = &report.graph_iri;
        // The fused value: one Assertion node, fields inline, typed decimals.
        let vin = node_of(&env, "params-vin");
        assert_eq!(objs(store, g, &vin, &eng("lowerLimit")), vec![dec("1.8")]);
        assert_eq!(objs(store, g, &vin, &eng("upperLimit")), vec![dec("6.5")]);
        assert_eq!(
            objs(store, g, &vin, &de("assertionKind")),
            vec![Obj::Iri(de("derivation"))]
        );
        assert_eq!(objs(store, g, &vin, &de("groundingRecords")).len(), 4);
        // Zero-based page index: Tom's page 4 is index 3.
        assert_eq!(
            objs(store, g, &vin, &de("pageIndex")),
            vec![typed("3", format!("{XSD}integer"))]
        );
        // A reading keeps its source role and reader tool apart (two readers
        // of one table are one source).
        let r2 = format!("{vin}-r2");
        assert_eq!(
            objs(store, g, &r2, &de("sourceRole")),
            vec![Obj::Iri("https://sophia-labs.ai/concept/datasheet-parser#roc".into())]
        );
        assert_eq!(
            objs(store, g, &r2, &de("readerTool")),
            vec![Obj::Iri("https://sophia-labs.ai/concept/datasheet-parser#docling".into())]
        );
        // The IQ row: typed rectangle, prose conditions, decimal with the exact lexical form.
        let iq = node_of(&env, "spec_rows-2");
        assert_eq!(
            objs(store, g, &iq, &de("rectangles")),
            vec![typed("54 402.3 558 414.9", de("RectList"))]
        );
        assert_eq!(
            objs(store, g, &iq, &de("conditions")),
            vec![typed("EN = VIN, IOUT = 0 mA, not switching", de("ProseCondition"))]
        );
        assert_eq!(objs(store, g, &iq, &eng("typical")), vec![dec("60")]);
        assert_eq!(
            objs(store, g, &iq, &eng("unit")),
            vec![Obj::Iri("https://sophia-labs.ai/concept/engineering-unit#nA".into())]
        );
        assert_eq!(
            objs(store, g, &iq, &de("geometryStatus")),
            vec![Obj::Iri(de("approximate"))]
        );
        // The unit the page printed is kept even where the scheme has an IRI.
        assert_eq!(
            objs(store, g, &iq, &eng("unitLexical")),
            vec![Obj::Text("nA".into())]
        );
        // The names the Files cards read, on the edition node itself.
        let ed = node_of(&env, "document");
        let int = |n: &str| typed(n, format!("{XSD}integer"));
        let boolean = |b: &str| typed(b, format!("{XSD}boolean"));
        assert_eq!(objs(store, g, &ed, &de("partNumber")), vec![Obj::Text("XPS62840".into())]);
        assert_eq!(objs(store, g, &ed, &de("revision")), vec![Obj::Text("B".into())]);
        assert_eq!(objs(store, g, &ed, &de("maker")), vec![Obj::Text("Example Semiconductor".into())]);
        assert_eq!(objs(store, g, &ed, &de("productType")), vec![Obj::Text("dc-dc".into())]);
        assert_eq!(objs(store, g, &ed, &de("rowCount")), vec![int("7")]);
        assert_eq!(objs(store, g, &ed, &de("toCheckCount")), vec![int("2")]);
        assert_eq!(objs(store, g, &ed, &de("scan")), vec![boolean("false")]);
        assert_eq!(objs(store, g, &ed, &de("textOnly")), vec![boolean("false")]);
        assert_eq!(objs(store, g, &vin, &de("paramOrder")), vec![int("0")]);
        assert_eq!(
            objs(store, g, &vin, &de("confidence")),
            vec![Obj::Iri("https://sophia-labs.ai/concept/datasheet-parser#high".into())]
        );
        // Custody: the declared digest, unchecked because no PDF was given.
        let content = rec(&format!("ds-{}", sha16(&env)));
        assert_eq!(
            objs(store, g, &content, &de("byteCheck")),
            vec![Obj::Iri(de("unchecked"))]
        );
        // The pipeline's own flags travel verbatim as diagnostics.
        let fsw = node_of(&env, "spec_rows-5");
        assert!(objs(store, g, &fsw, &de("diagnostic")).contains(&Obj::Text("value_not_numeric".into())));
        assert_eq!(
            objs(store, g, &fsw, &eng("rawMax")),
            vec![Obj::Text("2.2 (see note 2)".into())]
        );
    });
}

#[test]
fn per_slot_conditions_and_guarantees_are_preserved_and_absence_means_not_stated() {
    with_store(|store| {
        let env = fixture(BUCK);
        let report = import_into_store(store, GID, &input(env.clone())).expect("import");
        let g = &report.graph_iri;
        let iq = node_of(&env, "spec_rows-2");
        let prose = |s: &str| typed(s, de("ProseCondition"));
        assert_eq!(
            objs(store, g, &iq, &eng("typicalConditions")),
            vec![prose("TJ = 25 \u{b0}C")]
        );
        assert_eq!(
            objs(store, g, &iq, &eng("upperConditions")),
            vec![prose("TJ = \u{2212}40 \u{b0}C to 85 \u{b0}C")]
        );
        assert_eq!(
            objs(store, g, &iq, &eng("typicalGuarantee")),
            vec![Obj::Text("characterized".into())]
        );
        assert_eq!(
            objs(store, g, &iq, &eng("upperGuarantee")),
            vec![Obj::Text("tested".into())]
        );
        // No MIN stated: nothing says it is unconditional or guaranteed.
        assert!(objs(store, g, &iq, &eng("lowerConditions")).is_empty());
        assert!(objs(store, g, &iq, &eng("lowerGuarantee")).is_empty());
        // A row with no conditions column is "unknown", not unconditional.
        let vin_row = node_of(&env, "spec_rows-0");
        assert!(objs(store, g, &vin_row, &de("conditions")).is_empty());
        assert_eq!(
            objs(store, g, &vin_row, &de("conditionsCompleteness")),
            vec![Obj::Iri(de("unknown"))]
        );
        assert!(objs(store, g, &vin_row, &eng("typicalConditions")).is_empty());
        // And through the tool.
        let f = facts(store, Some("XPS62840"), None, Some("IQ"));
        let c = claim(&f, 0, "specRow", "IQ");
        let slots = &c["conditions"]["slots"];
        assert_eq!(slots["typical"]["conditions"], "TJ = 25 \u{b0}C");
        assert_eq!(slots["typical"]["guarantee"], "characterized");
        assert_eq!(slots["upper"]["conditions"], "TJ = \u{2212}40 \u{b0}C to 85 \u{b0}C");
        assert_eq!(slots["upper"]["guarantee"], "tested");
        assert!(slots["lower"]["conditions"].is_null() && slots["lower"]["guarantee"].is_null());
    });
}

#[test]
fn charger_registers_bit_fields_checks_and_contested_values() {
    with_store(|store| {
        let env = fixture(CHARGER);
        let report = import_into_store(store, GID, &input(env.clone())).expect("import");
        let c = &report.mapped.counts;
        assert_eq!(c["families"], 2);
        assert_eq!(c["params"], 2);
        assert_eq!(c["registers"], 2);
        assert_eq!(c["registerFields"], 6);
        assert_eq!(c["checks"], 2);
        assert_eq!(c["rows"], 2);
        // 7 custody + edition + 2 families + (6 + 3) params + (4 + 4) registers + 2 rows.
        assert_eq!(report.mapped.nodes, 29);
        let g = &report.graph_iri;
        let field = |reg: usize, j: usize| node_of(&env, &format!("registers-{reg}-fields-{j}"));
        let bit_range = |s: &str| typed(s, eng("BitRange"));
        let pattern = |s: &str| typed(s, eng("BitPattern"));
        // REG00: IINLIM 5:0 reset 0b001000; single bits; decimal resets.
        assert_eq!(objs(store, g, &field(0, 2), &eng("bitRange")), vec![bit_range("5:0")]);
        assert_eq!(
            objs(store, g, &field(0, 2), &eng("resetPattern")),
            vec![pattern("6:001000:111111")]
        );
        assert_eq!(objs(store, g, &field(0, 0), &eng("bitRange")), vec![bit_range("7:7")]);
        assert_eq!(objs(store, g, &field(0, 0), &eng("resetPattern")), vec![pattern("1:0:1")]);
        assert_eq!(objs(store, g, &field(0, 1), &eng("resetPattern")), vec![pattern("1:1:1")]);
        // REG0B: bracketed range; an unknown reset leaves the mask empty;
        // hex reset; a bit spec that cannot be normalized stays lexical.
        assert_eq!(objs(store, g, &field(1, 0), &eng("bitRange")), vec![bit_range("7:5")]);
        assert_eq!(
            objs(store, g, &field(1, 0), &eng("resetPattern")),
            vec![pattern("3:000:000")]
        );
        assert_eq!(objs(store, g, &field(1, 1), &eng("resetPattern")), vec![pattern("2:00:11")]);
        assert!(objs(store, g, &field(1, 2), &eng("bitRange")).is_empty());
        assert_eq!(
            objs(store, g, &field(1, 2), &eng("bitsLexical")),
            vec![Obj::Text("reserved".into())]
        );
        assert_eq!(objs(store, g, &field(1, 2), &de("diagnostic")).len(), 1);
        // The register keeps its printed address and a parsed one.
        let reg = node_of(&env, "registers-1");
        assert_eq!(objs(store, g, &reg, &eng("addressLexical")), vec![Obj::Text("0x0B".into())]);
        assert_eq!(objs(store, g, &reg, &eng("address")), vec![typed("11", format!("{XSD}integer"))]);

        // The contested value, its snapshot, and the changed-since-checked flag.
        let f = facts(store, Some("xbq25895"), None, Some("vin"));
        let vin = claim(&f, 0, "keyParameter", "vin");
        assert_eq!(vin["corroboration"]["contested"], true);
        assert_eq!(vin["value"]["upper"], 13.0);
        assert_eq!(vin["readings"].as_array().unwrap().len(), 4);
        let ext = vin["readings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["sourceRole"] == "ti_export")
            .expect("the vendor export reading");
        assert_eq!(ext["externalFile"], "TI Battery chargers 2026-09-04.xlsx");
        assert!(ext["page"].is_null());
        // The card mark is "contested"; Tom's level survives beside it, and the
        // check points at its assertion with de:assesses.
        let vin_node = node_of(&env, "params-vin");
        assert_eq!(
            objs(store, g, &vin_node, &de("confidence")),
            vec![Obj::Iri("https://sophia-labs.ai/concept/datasheet-parser#contested".into())]
        );
        assert_eq!(
            objs(store, g, &format!("{vin_node}-verified"), &de("assesses")),
            vec![Obj::Iri(vin_node.clone())]
        );
        assert_eq!(vin["corroboration"]["level"], "medium");
        // The shown value is the checked snapshot; no reading agrees with it.
        assert_eq!(vin["corroboration"]["independentSources"], 0);
        let check = &vin["checks"][0];
        assert_eq!(check["result"], "accept");
        assert_eq!(check["changedSinceChecked"], true);
        assert_eq!(check["snapshot"]["upper"], 13.0);
        assert_eq!(check["extracted"]["upper"], 13.5);
        assert_eq!(check["imported"], true);
        assert_eq!(vin["checkedByPerson"], false, "a changed value is not a clean check");
        assert!(vin["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "verified_value_changed"));
        // An unchanged check is a check.
        let iout = facts(store, Some("XBQ25895M"), None, Some("iout"));
        let iout = claim(&iout, 0, "keyParameter", "iout");
        assert_eq!(iout["checkedByPerson"], true);
        assert_eq!(iout["checks"][0]["by"], "ti_overrides.json");
        assert!(iout["checks"][0]["snapshot"]["lower"].is_null());
        // Document-level diagnostics ride on the edition.
        let all = facts(store, Some("XBQ25895"), None, None);
        assert!(all["editions"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "text_readers_disagree"));
    });
}

#[test]
fn scanned_ldo_carries_the_card_fix_flags_and_two_part_family() {
    with_store(|store| {
        let env = fixture(LDO);
        let report = import_into_store(store, GID, &input(env.clone())).expect("import");
        let c = &report.mapped.counts;
        let ed = node_of(&env, "document");
        assert_eq!(
            objs(store, &report.graph_iri, &ed, &de("scan")),
            vec![typed("true", format!("{XSD}boolean"))]
        );
        assert_eq!(c["corrections"], 3);
        assert_eq!(c["rows"], 3);
        assert_eq!(report.mapped.nodes, 18);
        let f = facts(store, Some("XLC1117-3.3"), None, None);
        assert_eq!(f["matched"], 1);
        let ed = &f["editions"][0];
        assert_eq!(ed["parts"].as_array().unwrap().len(), 2);
        let fixes = ed["corrections"].as_array().unwrap();
        assert!(fixes.iter().any(|x| x["field"] == "manufacturer" && x["value"] == "Example Semiconductor"));
        assert!(fixes.iter().any(|x| x["field"] == "parts" && x["value"] == "XLC1117, XLC1117-3.3"));
        let diags: Vec<&str> = ed["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(diags.contains(&"no_text_layer") && diags.contains(&"text_from_ocr"));
        assert!(diags.iter().any(|d| d.starts_with("fix rejected: `topology = ldo`")));
        let vdo = facts(store, Some("XLC1117"), None, Some("dropout"));
        let vdo = claim(&vdo, 0, "specRow", "VDO");
        assert_eq!(vdo["value"]["upper"], 1.3);
        assert_eq!(vdo["value"]["typical"], 1.1);
        assert_eq!(vdo["corroboration"]["level"], "low");
        assert_eq!(vdo["page"], 3);
        assert!(vdo["diagnostics"].as_array().unwrap().iter().any(|d| d == "ocr"));
    });
}

#[test]
fn reimport_is_idempotent_and_a_changed_envelope_replaces_the_graph_whole() {
    with_store(|store| {
        let env = fixture(BUCK);
        let first = import_into_store(store, GID, &input(env.clone())).expect("first");
        assert!(!first.replaced);
        let before = graph_dump(store, &first.graph_iri);
        let total = store.len().expect("len");
        let second = import_into_store(store, GID, &input(env.clone())).expect("second");
        assert!(second.replaced);
        assert_eq!(second.graph_iri, first.graph_iri);
        assert_eq!(graph_dump(store, &second.graph_iri), before, "same IRIs, same triples");
        assert_eq!(store.len().expect("len"), total, "no stacking");

        // The same document with the IQ row gone: the old row node must go.
        let mut edited = env.clone();
        edited["spec_rows"].as_array_mut().unwrap().remove(2);
        let third = import_into_store(store, GID, &input(edited)).expect("third");
        assert!(third.replaced);
        assert_eq!(third.mapped.counts["rows"], 5);
        let gone = node_of(&env, "spec_rows-2");
        // Index 2 is now the next row (VUVLO), not IQ: nothing of IQ remains.
        assert!(objs(store, &third.graph_iri, &gone, &eng("typical")).len() == 1);
        assert_eq!(objs(store, &third.graph_iri, &gone, &eng("typical")), vec![dec("1.65")]);
        assert_eq!(
            objs(store, &third.graph_iri, &gone, &eng("symbolLexical")),
            vec![Obj::Text("VUVLO".into())]
        );
        assert_eq!(graph_len(store, &third.graph_iri), third.mapped.triples.len());
        // Exactly one import graph for the document, plus the vocabulary.
        let graphs: BTreeSet<String> = store
            .named_graphs()
            .filter_map(Result::ok)
            .filter_map(|g| match g {
                NamedOrBlankNode::NamedNode(n) => Some(n.into_string()),
                _ => None,
            })
            .collect();
        assert_eq!(graphs.len(), 2, "{graphs:?}");
        assert!(graphs.contains(&vocabulary_graph_iri(GID)));
    });
}

#[test]
fn admission_refuses_what_verify_library_refuses_and_writes_nothing() {
    with_store(|store| {
        let mut env = fixture(BUCK);
        env["schema"] = json!("autonotes.data/1");
        env["document"]["doc_type"] = json!("brochure");
        env["params"][0]["page"] = Value::Null; // no page, no value ...
        env["params"][0]["readings"] = json!([
            {"source": "roc", "extractor": "docling", "page": null, "min": 1.8, "max": 6.5},
            {"source": "magic", "extractor": "docling", "page": 4, "min": 1.8, "max": 6.5},
            {"source": "prose", "extractor": "docling", "page": 400, "min": 1.8, "max": 6.5}
        ]);
        env["params"][1]["min"] = json!(7.0); // vout 7 > 3.3
        env["params"][2]["confidence"] = json!("certain");
        env["params"][3]["key"] = json!("vcc");
        env["spec_rows"][0]["page"] = Value::Null;
        env["spec_rows"][1]["max"] = json!("0.75");
        let reasons = refused(import_into_store(store, GID, &input(env)).expect_err("refused"));
        let all = reasons.join("\n");
        for needle in [
            "wrong schema",
            "doc_type",
            "no page and no source file",
            "outside the closed set",
            "beyond the document's 38 pages",
            "impossible range",
            "bad confidence",
            "key outside the closed set",
            "value without page",
            "not a finite number",
        ] {
            assert!(all.contains(needle), "missing {needle:?} in:\n{all}");
        }
        // Refusal is whole: not one graph was written.
        assert_eq!(store.named_graphs().count(), 0);
        assert_eq!(store.len().expect("len"), 0);
    });
}

#[test]
fn a_value_with_neither_page_nor_file_is_refused_unless_a_person_verified_it() {
    with_store(|store| {
        let mut env = fixture(BUCK);
        env["params"][0]["page"] = Value::Null;
        let reasons = refused(import_into_store(store, GID, &input(env.clone())).expect_err("refused"));
        assert_eq!(reasons, vec!["params[0] (vin): value without page or source file".to_string()]);
        // A person is their own source (verify_library.py).
        env["params"][0]["verified"] =
            json!({"by": "user", "on": "2026-10-01", "evidence": null, "value": [1.8, 6.5]});
        import_into_store(store, GID, &input(env)).expect("verified values are admitted");
    });
}

#[test]
fn the_pdf_artifact_is_recorded_and_its_bytes_must_match_the_declared_sha256() {
    with_store(|store| {
        let bytes = b"%PDF-1.7 fixture bytes for the custody check";
        let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(bytes));
        let mut env = fixture(LDO);
        env["document"]["sha256"] = json!(digest);
        let mut inp = input(env.clone());
        inp.pdf = Some(PdfCheck {
            artifact_id: Some("pdf-artifact-1".into()),
            sha256: digest.clone(),
            bytes: bytes.len() as u64,
        });
        inp.envelope_artifact_id = Some("envelope-artifact-1".into());
        inp.renditions.insert("text".into(), "text-artifact-1".into());
        let report = import_into_store(store, GID, &inp).expect("import");
        let g = &report.graph_iri;
        let content = rec(&format!("ds-{}", &digest[..16]));
        assert_eq!(
            objs(store, g, &content, &de("digest")),
            vec![typed(&format!("sha256:{digest}:{}", bytes.len()), de("Digest"))]
        );
        assert_eq!(objs(store, g, &content, &de("byteCheck")), vec![Obj::Iri(de("matched"))]);
        let binding = rec(&format!("ds-{}-binding", &digest[..16]));
        assert_eq!(
            objs(store, g, &binding, &de("artifact")),
            vec![Obj::Text("pdf-artifact-1".into())]
        );
        // custody now has the text rendition as well.
        assert_eq!(report.mapped.counts["custodyNodes"], 8);
        // The tool finds it by artifact id and by sha prefix, and links to it.
        let by_artifact = facts(store, None, Some("pdf-artifact-1"), Some("vout"));
        assert_eq!(by_artifact["matched"], 1);
        let c = claim(&by_artifact, 0, "keyParameter", "vout");
        let id = c["id"].as_str().unwrap();
        assert_eq!(c["link"], format!("/files/pdf-artifact-1#evidence={id}"));
        assert_eq!(facts(store, None, Some(&digest[..12]), None)["matched"], 1);
        assert_eq!(facts(store, None, Some("envelope-artifact-1"), None)["matched"], 1);

        // Other bytes under the same declared sha256: refused.
        let mut wrong = input(env);
        wrong.pdf = Some(PdfCheck {
            artifact_id: Some("pdf-artifact-2".into()),
            sha256: "0".repeat(64),
            bytes: 3,
        });
        let reasons = refused(import_into_store(store, GID, &wrong).expect_err("refused"));
        assert!(reasons[0].contains("does not match the PDF artifact's bytes"), "{reasons:?}");
    });
}

fn minimal_row(i: usize, rich: bool) -> Value {
    let mut r = json!({
        "section": "Electrical Characteristics", "page": 5, "group": "",
        "symbol": format!("S{i}"), "parameter": format!("Parameter {i}"),
        "conditions": if rich { "VIN = 3.6 V, IOUT = 100 mA" } else { "" },
        "min": 1.0, "typ": 2.0, "max": 3.0, "unit": "mA",
        "extractor": "docling", "confidence": "medium", "flags": []
    });
    if rich {
        r["slot_conditions"] = json!({"min": "TJ = -40 C", "typ": "TJ = 25 C", "max": "TJ = 85 C"});
        r["slot_guarantee"] = json!({"min": "tested", "typ": "characterized", "max": "tested"});
        r["box"] = json!([54.0, 100.0 + i as f64, 558.0, 112.0 + i as f64]);
    }
    r
}

#[test]
fn the_row_cap_overflows_into_a_diagnostic_and_the_envelope_keeps_the_rest() {
    with_store(|store| {
        let mut env = fixture(BUCK);
        env["spec_rows"] = Value::Array((0..700).map(|i| minimal_row(i, false)).collect());
        let report = import_into_store(store, GID, &input(env)).expect("import");
        assert_eq!(report.mapped.counts["rows"], 600);
        assert!(report.mapped.diagnostics.contains(&"on demand: 100 rows".to_string()));
        let f = facts(store, Some("XPS62840"), None, Some("S0"));
        assert!(f["editions"][0]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "on demand: 100 rows"));
        // Unlisted rows are "not found", not zero.
        let none = facts(store, Some("XPS62840"), None, Some("S650"));
        assert_eq!(none["editions"][0]["claims"].as_array().unwrap().len(), 0);
    });
}

#[test]
fn the_per_document_cap_holds_at_1800_nodes_and_20000_triples() {
    with_store(|store| {
        let mut env = fixture(CHARGER);
        // 100 registers of 11 fields, and 600 rich rows: far more than 20,000 triples.
        let regs: Vec<Value> = (0..100)
            .map(|r| {
                json!({"name": format!("REG{r:02X}"), "address": format!("0x{r:02X}"), "reset": "0x00", "page": 33,
                    "description": "bulk register",
                    "fields": (0..11).map(|j| json!({
                        "bits": format!("{j}"), "name": format!("F{r}_{j}"), "access": "R/W",
                        "reset": "0", "description": "bulk field"})).collect::<Vec<_>>()})
            })
            .collect();
        env["registers"] = Value::Array(regs);
        env["spec_rows"] = Value::Array((0..600).map(|i| minimal_row(i, true)).collect());
        let report = import_into_store(store, GID, &input(env)).expect("import still succeeds");
        let m = &report.mapped;
        assert!(m.nodes <= MAX_NODES, "nodes {}", m.nodes);
        assert!(m.triples.len() <= MAX_TRIPLES, "triples {}", m.triples.len());
        assert!(m.triples.len() > MAX_TRIPLES - 2_000, "the cap, not the data, ended it: {}", m.triples.len());
        // Fields cap at 1,000: 90 whole registers of 11 fields.
        assert_eq!(m.counts["registers"], 90);
        assert_eq!(m.counts["registerFields"], 990);
        assert!(m.diagnostics.contains(&"on demand: 10 registers".to_string()), "{:?}", m.diagnostics);
        assert!(m.diagnostics.contains(&"on demand: 110 register fields".to_string()));
        assert!(m.counts["rows"] < 600, "the triple budget must have stopped the rows");
        let dropped = 600 - m.counts["rows"];
        assert!(
            m.diagnostics.contains(&format!("on demand: {dropped} rows")),
            "{:?}",
            m.diagnostics
        );
        assert_eq!(graph_len(store, &report.graph_iri), m.triples.len());
    });
}

#[test]
fn part_strings_resolve_to_the_graphs_existing_family_handle() {
    with_store(|store| {
        // An earlier import (or a person) already minted this family.
        let existing = "urn:example:family/xps62840";
        let g = nn("urn:example:elsewhere");
        for (p, o) in [
            (eng("normalizedIdentifier"), Term::from(Literal::new_simple_literal("xps62840"))),
            (eng("identifier"), Term::from(Literal::new_simple_literal("XPS62840"))),
        ] {
            store
                .insert(&Quad::new(nn(existing), nn(&p), o, g.clone()))
                .expect("insert");
        }
        assert_eq!(find_family(store, "xps62840").as_deref(), Some(existing));
        assert_eq!(find_family(store, "nothere"), None);
        let env = fixture(BUCK);
        let report = import_into_store(store, GID, &input(env.clone())).expect("import");
        let edition = node_of(&env, "document");
        assert_eq!(
            objs(store, &report.graph_iri, &edition, &de("namesFamily")),
            vec![Obj::Iri(existing.into())]
        );
        let vin = node_of(&env, "params-vin");
        assert_eq!(objs(store, &report.graph_iri, &vin, &de("subject")), vec![Obj::Iri(existing.into())]);
        // The tool still finds the part by its normalized identifier.
        assert_eq!(facts(store, Some("xps-62840"), None, None)["matched"], 1);
    });
}

#[test]
fn facts_round_trip_buck_key_parameter_page_box_readings_and_corroboration() {
    with_store(|store| {
        let env = fixture(BUCK);
        import_into_store(store, GID, &input(env)).expect("import");
        let f = facts(store, Some("XPS62840"), None, Some("vin"));
        assert_eq!(f["matched"], 1);
        let ed = &f["editions"][0];
        assert_eq!(ed["title"], "XPS62840 1.8 V to 6.5 V Input, 750 mA Step-Down Converter");
        assert_eq!(ed["revision"], "B");
        assert_eq!(ed["maker"], "Example Semiconductor");
        let vin = claim(&f, 0, "keyParameter", "vin");
        assert_eq!(vin["value"]["lower"], 1.8);
        assert_eq!(vin["value"]["upper"], 6.5);
        assert_eq!(vin["value"]["unit"], "V");
        assert_eq!(vin["regime"], "recommendedOperating");
        assert_eq!(vin["page"], 4);
        assert_eq!(vin["corroboration"]["level"], "high");
        // page 1, the ROC table (read twice) and the prose: three sources, four readings.
        assert_eq!(vin["corroboration"]["independentSources"], 3);
        let mut roles: Vec<&str> = vin["corroboration"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        roles.sort();
        assert_eq!(roles, vec!["page1", "prose", "roc"]);
        assert_eq!(vin["readings"].as_array().unwrap().len(), 4);
        assert_eq!(vin["readings"][0]["page"], 1);
        assert_eq!(vin["checks"].as_array().unwrap().len(), 0, "no native checks yet");
        assert_eq!(vin["checkedByPerson"], false);
        assert!(vin["link"].is_null(), "no PDF artifact was bound");
        // Both the key parameter and the ROC row named VIN answer "vin".
        assert_eq!(ed["claims"].as_array().unwrap().len(), 2);
        // The IQ row: page and box.
        let f = facts(store, Some("XPS62840"), None, Some("IQ"));
        let iq = claim(&f, 0, "specRow", "IQ");
        assert_eq!(iq["page"], 5);
        assert_eq!(iq["box"], json!([[54.0, 402.3, 558.0, 414.9]]));
        assert_eq!(iq["precision"], "cell");
        assert_eq!(iq["geometryStatus"], "approximate");
        assert_eq!(iq["conditions"]["row"], "EN = VIN, IOUT = 0 mA, not switching");
        assert_eq!(iq["value"]["typical"], 60.0);
        assert!(iq["value"]["lower"].is_null());
        // Not found is not zero.
        let nothing = facts(store, Some("NOPE123"), None, None);
        assert_eq!(nothing["matched"], 0);
        assert!(nothing["note"].as_str().unwrap().contains("not found"));
        let no_param = facts(store, Some("XPS62840"), None, Some("junction temperature"));
        assert_eq!(no_param["editions"][0]["claims"].as_array().unwrap().len(), 0);
        // A query must name a part or an artifact.
        let err = facts_from_store(
            store,
            GID,
            &FactsQuery { part: None, artifact: None, parameter: Some("vin".into()), limit: 10 },
        )
        .expect_err("needs a part or artifact");
        assert!(err.message_ref().contains("part or an artifact"));
    });
}

#[test]
fn another_graphs_imports_are_not_visible() {
    with_store(|store| {
        import_into_store(store, GID, &input(fixture(BUCK))).expect("import");
        let other = facts_from_store(
            store,
            "some-other-graph",
            &FactsQuery { part: Some("XPS62840".into()), artifact: None, parameter: None, limit: 10 },
        )
        .expect("facts");
        assert_eq!(other["matched"], 0);
    });
}

#[test]
fn the_seed_vocabulary_parses_and_is_loaded_beside_every_import() {
    with_store(|store| {
        let report = import_into_store(store, GID, &input(fixture(BUCK))).expect("import");
        let vocab = vocabulary_graph_iri(GID);
        assert!(graph_len(store, &vocab) > 150, "{}", graph_len(store, &vocab));
        let dsp_high = "https://sophia-labs.ai/concept/datasheet-parser#high";
        let defs = objs(
            store,
            &vocab,
            dsp_high,
            "http://www.w3.org/2004/02/skos/core#definition",
        );
        assert_eq!(defs, vec![Obj::Text("Two or more independent sources agree.".into())]);
        assert!(!objs(
            store,
            &vocab,
            "https://sophia-labs.ai/concept/engineering-parameter#inputVoltage",
            "http://www.w3.org/2004/02/skos/core#notation"
        )
        .is_empty());
        // Tom is credited where his vocabulary lives.
        assert!(VOCAB_TTL.contains("NomDeTom"));
        // Every concept the importer can emit for a source role, reader or confidence is defined.
        for iri in [
            "page1", "roc", "prose", "ti_export", "specTable", "docling", "docling-ocr",
            "pdfplumber", "pdftotext", "high", "medium", "low", "none", "contested",
        ] {
            let full = format!("https://sophia-labs.ai/concept/datasheet-parser#{iri}");
            assert!(
                !objs(store, &vocab, &full, "http://www.w3.org/2004/02/skos/core#prefLabel").is_empty(),
                "dsp:{iri} undefined"
            );
        }
        // Re-import does not duplicate the vocabulary.
        let n = graph_len(store, &vocab);
        import_into_store(store, GID, &input(fixture(BUCK))).expect("again");
        assert_eq!(graph_len(store, &vocab), n);
        assert!(report.mapped.triples.len() > 100);
    });
}

// Real envelopes from the 2026-10-08 corpus read (Tom's pipeline at ee680a6;
// that run's Docling failed offline, so every value is from his fallback
// readers pdfplumber and pdftotext). Schema is his real one.
const REAL: [(&str, &str); 10] = [
    ("ti-lm5164_DS", include_str!("datasheet_fixtures/real/ti-lm5164_DS.data.json")),
    ("ti-tps54331_DS", include_str!("datasheet_fixtures/real/ti-tps54331_DS.data.json")),
    ("ti-ads1115_DS", include_str!("datasheet_fixtures/real/ti-ads1115_DS.data.json")),
    ("ti-tmp117_DS", include_str!("datasheet_fixtures/real/ti-tmp117_DS.data.json")),
    ("ti-lm358-family_DS", include_str!("datasheet_fixtures/real/ti-lm358-family_DS.data.json")),
    ("infineon-bsc016n06ns_DS_V2.6", include_str!("datasheet_fixtures/real/infineon-bsc016n06ns_DS_V2.6.data.json")),
    ("microchip-atmega328p_DS-revA", include_str!("datasheet_fixtures/real/microchip-atmega328p_DS-revA.data.json")),
    ("microchip-atmega328p_DS-revB", include_str!("datasheet_fixtures/real/microchip-atmega328p_DS-revB.data.json")),
    ("espressif-esp32-c3-trm", include_str!("datasheet_fixtures/real/espressif-esp32-c3-trm.data.json")),
    ("espressif-esp32-c3_DS", include_str!("datasheet_fixtures/real/espressif-esp32-c3_DS.data.json")),
];

/// Nodes the light tier must hold for an envelope, derived independently
/// from the JSON (all real rows are single-reader, so all are primary).
fn expected_nodes(env: &Value, families: usize) -> usize {
    let params: usize = env["params"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| 1 + p["readings"].as_array().unwrap().len().min(5) + usize::from(p.get("verified").is_some_and(Value::is_object)))
        .sum();
    let regs: usize = env["registers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| 1 + r["fields"].as_array().unwrap().len())
        .sum();
    let fixes = env["document"]["corrections"].as_object().map_or(0, |m| m.len());
    // 6 custody + one ToolProfile per non-parser tool the run names + edition + families.
    let tools = env["extraction"]["tools"].as_object().map_or(0, |t| t.keys().filter(|k| *k != "datasheet-parser").count());
    6 + tools + 1 + families + params + regs + env["spec_rows"].as_array().unwrap().len().min(600) + fixes
}

#[test]
fn real_corpus_envelopes_all_import_within_budget_and_pair_revisions_apart() {
    with_store(|store| {
        for (name, raw) in REAL {
            let env = fixture(raw);
            let report = import_into_store(store, GID, &input(env.clone()))
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let m = &report.mapped;
            assert!(m.nodes <= MAX_NODES && m.triples.len() <= MAX_TRIPLES, "{name}");
            assert_eq!(m.nodes, expected_nodes(&env, m.counts["families"]) + m.counts["keyValues"], "{name}");
            assert_eq!(m.counts["rows"], env["spec_rows"].as_array().unwrap().len(), "{name}");
            assert_eq!(m.counts["registers"], env["registers"].as_array().unwrap().len(), "{name}");
            assert_eq!(graph_len(store, &report.graph_iri), m.triples.len(), "{name}");
            // Idempotent on real data too.
            let before = graph_dump(store, &report.graph_iri);
            let again = import_into_store(store, GID, &input(env)).expect("again");
            assert!(again.replaced && graph_dump(store, &again.graph_iri) == before, "{name}");
        }
        // Rev A and rev B of the ATmega328P are two documents, two graphs.
        let graphs: BTreeSet<String> = store
            .named_graphs()
            .filter_map(Result::ok)
            .filter_map(|g| match g {
                NamedOrBlankNode::NamedNode(n) => Some(n.into_string()),
                _ => None,
            })
            .filter(|g| g.contains(":projection:datasheet:ds-"))
            .collect();
        assert_eq!(graphs.len(), REAL.len());
    });
}

#[test]
fn real_registers_and_key_parameters_round_trip() {
    with_store(|store| {
        let ads = fixture(REAL[2].1);
        let tmp = fixture(REAL[3].1);
        let lm = fixture(REAL[0].1);
        let rads = import_into_store(store, GID, &input(ads.clone())).expect("ads");
        let rtmp = import_into_store(store, GID, &input(tmp.clone())).expect("tmp");
        // The ATmega328P envelopes are empty (text-only): the card says so.
        let atm = fixture(REAL[6].1);
        let ratm = import_into_store(store, GID, &input(atm.clone())).expect("atmega");
        assert_eq!(
            objs(store, &ratm.graph_iri, &node_of(&atm, "document"), &de("textOnly")),
            vec![typed("true", format!("{XSD}boolean"))]
        );
        import_into_store(store, GID, &input(lm.clone())).expect("lm5164");
        let pat = |s: &str| typed(s, eng("BitPattern"));
        // ADS1115 register 0 (pdfplumber fallback: `source_page`, name UNKNOWN, address "?").
        let f0 = node_of(&ads, "registers-0-fields-0");
        assert_eq!(objs(store, &rads.graph_iri, &f0, &eng("resetPattern")), vec![pat("6:000000:111111")]);
        let f1 = node_of(&ads, "registers-0-fields-1");
        assert_eq!(objs(store, &rads.graph_iri, &f1, &eng("resetPattern")), vec![pat("2:00:11")]);
        let reg0 = node_of(&ads, "registers-0");
        assert_eq!(objs(store, &rads.graph_iri, &reg0, &eng("addressLexical")), vec![Obj::Text("?".into())]);
        assert!(objs(store, &rads.graph_iri, &reg0, &eng("address")).is_empty());
        assert_eq!(
            objs(store, &rads.graph_iri, &reg0, &de("pageIndex")),
            vec![typed("23", format!("{XSD}integer"))]
        );
        // TMP117: 16-bit hex reset printed `8000h`.
        let t = node_of(&tmp, "registers-0-fields-0");
        assert_eq!(
            objs(store, &rtmp.graph_iri, &t, &eng("resetPattern")),
            vec![pat("16:1000000000000000:1111111111111111")]
        );
        // The LM5164 buck: VIN 6-100 V on page 1, page 1 and the ROC table agree.
        let sha = lm["document"]["sha256"].as_str().unwrap();
        let f = facts(store, None, Some(&sha[..16]), Some("vin"));
        let vin = claim(&f, 0, "keyParameter", "vin");
        assert_eq!(vin["value"]["lower"], 6.0);
        assert_eq!(vin["value"]["upper"], 100.0);
        assert_eq!(vin["page"], 1);
        assert_eq!(vin["corroboration"]["level"], "high");
        assert_eq!(vin["corroboration"]["independentSources"], 2);
        // A key parameter the pipeline did not find is simply absent.
        let vout = facts(store, None, Some(&sha[..16]), Some("fsw"));
        let in_claims = vout["editions"][0]["claims"].as_array().unwrap().iter().any(|c| c["kind"] == "keyParameter");
        assert_eq!(in_claims, lm["params"].as_array().unwrap().iter().any(|p| p["key"] == "fsw"));
    });
}

// The Docling read of the same corpus (2026-10-08T232520Z).
const DOCLING: [(&str, &str); 10] = [
    ("ti-opa2134_DS", include_str!("datasheet_fixtures/docling/ti-opa2134_DS.data.json")),
    ("ti-lm358-family_DS", include_str!("datasheet_fixtures/docling/ti-lm358-family_DS.data.json")),
    ("infineon-bsc016n06ns_DS_V2.6", include_str!("datasheet_fixtures/docling/infineon-bsc016n06ns_DS_V2.6.data.json")),
    ("infineon-irlb8721_DS", include_str!("datasheet_fixtures/docling/infineon-irlb8721_DS.data.json")),
    ("vishay-si2302cds_DS", include_str!("datasheet_fixtures/docling/vishay-si2302cds_DS.data.json")),
    ("ti-tmp117_DS", include_str!("datasheet_fixtures/docling/ti-tmp117_DS.data.json")),
    ("bosch-bme280_DS", include_str!("datasheet_fixtures/docling/bosch-bme280_DS.data.json")),
    ("ti-ads1115_DS", include_str!("datasheet_fixtures/docling/ti-ads1115_DS.data.json")),
    ("espressif-esp32-c3_DS", include_str!("datasheet_fixtures/docling/espressif-esp32-c3_DS.data.json")),
    ("ti-tlv755p_DS", include_str!("datasheet_fixtures/docling/ti-tlv755p_DS.data.json")),
];

#[test]
fn key_values_are_derived_from_spec_rows_for_every_part_type_and_never_invented() {
    // (document, key -> spec_rows index it must be grounded in). Anything not
    // listed must be ABSENT: OPA2134 and BME280 state no parseable VOS /
    // accuracy number, the MCU and the 2008 MOSFET sheet have no matching row.
    let expected: [(&str, &[(&str, usize)]); 10] = [
        ("ti-opa2134_DS", &[("gainBandwidth", 25), ("supplyVoltage", 15)]),
        ("ti-lm358-family_DS", &[("gainBandwidth", 55), ("inputOffsetVoltage", 243), ("supplyVoltage", 5)]),
        ("infineon-bsc016n06ns_DS_V2.6", &[("drainSourceVoltage", 0), ("onResistance", 1), ("drainCurrent", 2)]),
        ("infineon-irlb8721_DS", &[("drainSourceVoltage", 0), ("onResistance", 25), ("drainCurrent", 3)]),
        ("vishay-si2302cds_DS", &[]),
        ("ti-tmp117_DS", &[("accuracy", 14), ("supplyVoltage", 6)]),
        ("bosch-bme280_DS", &[("range", 23), ("supplyVoltage", 0)]),
        ("ti-ads1115_DS", &[("resolution", 29), ("sampleRate", 37), ("supplyVoltage", 11)]),
        ("espressif-esp32-c3_DS", &[]),
        ("ti-tlv755p_DS", &[("dropout", 48)]),
    ];
    let all_keys = [
        "gainBandwidth", "inputOffsetVoltage", "supplyVoltage", "drainSourceVoltage", "onResistance",
        "drainCurrent", "range", "accuracy", "resolution", "sampleRate", "dropout", "core", "flash", "ram",
    ];
    with_store(|store| {
        for (i, (name, raw)) in DOCLING.iter().enumerate() {
            assert_eq!(expected[i].0, *name);
            let env = fixture(raw);
            let report = import_into_store(store, GID, &input(env.clone())).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let g = &report.graph_iri;
            let want = expected[i].1;
            assert_eq!(report.mapped.counts["keyValues"], want.len(), "{name}");
            assert_eq!(report.mapped.counts["rows"], env["spec_rows"].as_array().unwrap().len(), "{name}");
            assert_eq!(report.mapped.nodes, expected_nodes(&env, report.mapped.counts["families"]) + want.len(), "{name}");
            for key in all_keys {
                let node = node_of(&env, &format!("key-{key}"));
                match want.iter().find(|(k, _)| *k == key) {
                    Some((_, row)) => {
                        assert_eq!(
                            objs(store, g, &node, &de("groundingRecords")),
                            vec![Obj::Iri(node_of(&env, &format!("spec_rows-{row}")))],
                            "{name} {key}"
                        );
                        assert_eq!(
                            objs(store, g, &node, &de("assertionKind")),
                            vec![Obj::Iri(de("derivation"))]
                        );
                        assert_eq!(objs(store, g, &node, &de("paramOrder")).len(), 1);
                        // The confidence is the row's own.
                        let row_conf = env["spec_rows"][row]["confidence"].as_str().unwrap();
                        assert_eq!(
                            objs(store, g, &node, &de("confidence")),
                            vec![Obj::Iri(format!("https://sophia-labs.ai/concept/datasheet-parser#{row_conf}"))]
                        );
                        // And the value slots are the row's, not invented.
                        for (slot, k) in [("lowerLimit", "min"), ("typical", "typ"), ("upperLimit", "max")] {
                            let got = objs(store, g, &node, &eng(slot));
                            match env["spec_rows"][row][k].as_f64() {
                                Some(v) => assert_eq!(got.len(), 1, "{name} {key} {slot} {v}"),
                                None => assert!(got.is_empty(), "{name} {key} {slot}"),
                            }
                        }
                    }
                    None => assert!(
                        objs(store, g, &node, &de("assertionKind")).is_empty(),
                        "{name}: {key} must be absent"
                    ),
                }
            }
        }
        // Through the tool: RDS(on) is the lowest-VGS row with its conditions kept.
        let irlb = fixture(DOCLING[3].1);
        let sha = irlb["document"]["sha256"].as_str().unwrap();
        let f = facts(store, None, Some(&sha[..16]), Some("onResistance"));
        let rds = claim(&f, 0, "keyParameter", "onResistance");
        assert_eq!(rds["value"]["typical"], 13.1);
        assert_eq!(rds["value"]["upper"], 16.0);
        assert_eq!(rds["value"]["unit"], "m\u{3a9}");
        assert_eq!(rds["readings"].as_array().unwrap().len(), 1);
        assert!(rds["conditions"]["row"].as_str().unwrap().contains("VGS = 4.5V"));
        assert_eq!(rds["corroboration"]["level"], "low");
        // The other RDS(on) row (VGS = 10 V) stays reachable as its own row assertion.
        assert!(facts(store, None, Some(&sha[..16]), Some("RDS(on)"))["editions"][0]["claims"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "specRow" && c["conditions"]["row"].as_str().is_some_and(|s| s.contains("VGS = 10V"))));
        // The edition says what it was classified as.
        let ed = node_of(&irlb, "document");
        let g = format!("urn:mnemosyne:local:graph:{GID}:projection:datasheet:ds-{}:m1", &sha[..16]);
        assert_eq!(objs(store, &g, &ed, &de("inferredProductType")), vec![Obj::Text("mosfet".into())]);
    });
}

#[test]
fn a_multi_part_string_becomes_one_family_per_part_and_each_is_found() {
    with_store(|store| {
        let env = fixture(DOCLING[0].1);
        assert_eq!(env["document"]["parts"][0], "OPA134, OPA2134, OPA4134");
        let report = import_into_store(store, GID, &input(env.clone())).expect("import");
        assert_eq!(report.mapped.counts["families"], 3);
        let edition = node_of(&env, "document");
        assert_eq!(objs(store, &report.graph_iri, &edition, &de("namesFamily")).len(), 3);
        assert_eq!(
            objs(store, &report.graph_iri, &edition, &de("partNumber")),
            vec![Obj::Text("OPA134".into())]
        );
        for q in ["OPA2134", "opa134", "OPA4134", "opa-4134", "OPA 2134"] {
            let f = facts(store, Some(q), None, Some("gainBandwidth"));
            assert_eq!(f["matched"], 1, "{q}");
            assert_eq!(f["editions"][0]["parts"].as_array().unwrap().len(), 3);
            assert!(!f["editions"][0]["claims"].as_array().unwrap().is_empty(), "{q}");
        }
        // Nothing exact: a family prefix finds it; an unrelated part does not.
        assert_eq!(facts(store, Some("OPA21"), None, None)["matched"], 1);
        assert_eq!(facts(store, Some("OPA999"), None, None)["matched"], 0);
        // LM358 finds the LM358 sheet; LM358B too.
        let lm = fixture(DOCLING[1].1);
        import_into_store(store, GID, &input(lm)).expect("lm358");
        assert_eq!(facts(store, Some("LM358B"), None, None)["matched"], 1);
        assert_eq!(facts(store, Some("lm-358"), None, None)["matched"], 1);
        assert_eq!(facts(store, Some("LM2904"), None, None)["matched"], 1);
    });
}

#[test]
fn an_mcu_core_named_in_the_title_is_a_text_key_value() {
    with_store(|store| {
        let mut env = fixture(DOCLING[8].1);
        env["document"]["title"] = json!("ESP32-C3 Series, 32-bit RISC-V MCU");
        let report = import_into_store(store, GID, &input(env.clone())).expect("import");
        let node = node_of(&env, "key-core");
        assert_eq!(
            objs(store, &report.graph_iri, &node, &de("valueText")),
            vec![Obj::Text("RISC-V".into())]
        );
        let f = facts(store, None, Some(&env["document"]["sha256"].as_str().unwrap()[..16]), Some("core"));
        assert_eq!(claim(&f, 0, "keyParameter", "core")["value"]["text"], "RISC-V");
    });
}

#[test]
fn lexical_helpers_are_exact() {
    assert_eq!(norm_ident("XLC1117-3.3"), "xlc111733");
    assert_eq!(norm_ident(" TPS-62840 "), "tps62840");
}

// ------------------------------------------------------- dispatch (headless)

#[cfg(feature = "headless")]
mod dispatch {
    use super::*;
    use crate::app_error::AppResult;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::local_jobs::LocalJobRegistry;
    use crate::mcp_dispatch_registry::{lookup, McpCallCtx};
    use std::sync::Arc;

    fn call(ctx: &McpCallCtx<'_>, name: &str, args: Value) -> AppResult<Value> {
        let entry = lookup(name).unwrap_or_else(|| panic!("dispatch entry for {name}"));
        crate::app_runtime::async_runtime::block_on((entry.handler)(ctx, &args))
    }

    #[test]
    fn datasheet_tools_are_dispatched_and_serve_a_real_cell() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-datasheet-dispatch-profile-{nanos}"));
        let jobs_dir = std::env::temp_dir().join(format!("garden-datasheet-dispatch-jobs-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "datasheet-dispatch-lab";
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some(graph_id.to_string()),
                    title: "Datasheet dispatch lab".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let jobs = Arc::new(LocalJobRegistry::new(jobs_dir.clone()).expect("jobs"));
            let ctx = McpCallCtx { app: app.clone(), jobs: &jobs };

            // Inline envelope, as an object.
            let report = call(
                &ctx,
                "datasheet_import",
                json!({"graph_id": graph_id, "envelope": fixture(BUCK)}),
            )
            .expect("datasheet_import serves");
            assert_eq!(report["mappingProfile"], "datasheet-light/1");
            assert_eq!(report["replaced"], false);
            assert_eq!(report["nodes"], 28);
            // Inline, as a string, again: idempotent.
            let again = call(
                &ctx,
                "datasheet_import",
                json!({"graphId": graph_id, "envelope": BUCK}),
            )
            .expect("re-import");
            assert_eq!(again["replaced"], true);
            assert_eq!(again["triples"], report["triples"]);

            // The Writer's question: "max VIN of XPS62840, which page, how corroborated?"
            let f = call(
                &ctx,
                "datasheet_facts",
                json!({"graph_id": graph_id, "part": "XPS62840", "parameter": "vin"}),
            )
            .expect("datasheet_facts serves");
            let vin = f["editions"][0]["claims"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["kind"] == "keyParameter")
                .unwrap();
            assert_eq!(vin["value"]["upper"], 6.5);
            assert_eq!(vin["page"], 4);
            assert_eq!(vin["corroboration"]["level"], "high");

            // Refusals are loud and validated.
            let mut bad = fixture(BUCK);
            bad["schema"] = json!("nope");
            let err = call(&ctx, "datasheet_import", json!({"graph_id": graph_id, "envelope": bad}))
                .expect_err("refused");
            assert!(err.message_ref().contains("datasheet import refused"), "{}", err.message_ref());
            assert!(call(&ctx, "datasheet_import", json!({"graph_id": graph_id})).is_err());
            assert!(call(
                &ctx,
                "datasheet_import",
                json!({"graph_id": graph_id, "envelope_artifact_id": "no-such-artifact"})
            )
            .is_err());
            assert!(call(&ctx, "datasheet_facts", json!({"graph_id": graph_id})).is_err());
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        let _ = std::fs::remove_dir_all(&jobs_dir);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}

//! Datasheet identity (parts and maker by several witnesses), against REAL
//! first-page excerpts of the 2026-10-08 corpus documents and Tom's real
//! envelopes' identity fields (`datasheet_fixtures/identity/`, kept short: the
//! head of page 1 plus the lines that name a maker). Mounted under `emporium::`
//! so the `cells` lane runs them.
//!
//! The rules also exist in Node, as the corpus scorer
//! (`identity-score.mjs`, kept outside this repository). `expected*.tsv` are what
//! that script prints for these same excerpts; the first test makes this code
//! reproduce them line for line, so the two writings cannot drift silently.

use std::collections::BTreeMap;

use oxigraph::store::Store;
use serde_json::{json, Value};

use crate::datasheet_facts::{facts_from_store, FactsQuery};
use crate::datasheet_identity::{lead_parts, reconcile, Evidence, Identity};
use crate::datasheet_import::{import_into_store, ImportInput};

const GID: &str = "datasheet-lab";
const EXPECTED: &str = include_str!("datasheet_fixtures/identity/expected.tsv");
const EXPECTED_NOFILE: &str = include_str!("datasheet_fixtures/identity/expected-nofile.tsv");

/// (document, evidence lines, text excerpt)
const DOCS: &[(&str, &str, &str)] = &[
    ("bosch-bme280_DS", include_str!("datasheet_fixtures/identity/bosch-bme280_DS.ev"), include_str!("datasheet_fixtures/identity/bosch-bme280_DS.text.txt")),
    ("infineon-bsc016n06ns_DS_V2.6", include_str!("datasheet_fixtures/identity/infineon-bsc016n06ns_DS_V2.6.ev"), include_str!("datasheet_fixtures/identity/infineon-bsc016n06ns_DS_V2.6.text.txt")),
    ("infineon-irlb8721_DS", include_str!("datasheet_fixtures/identity/infineon-irlb8721_DS.ev"), include_str!("datasheet_fixtures/identity/infineon-irlb8721_DS.text.txt")),
    ("jst-ph-connector", include_str!("datasheet_fixtures/identity/jst-ph-connector.ev"), include_str!("datasheet_fixtures/identity/jst-ph-connector.text.txt")),
    ("microchip-atmega328p_DS-revA", include_str!("datasheet_fixtures/identity/microchip-atmega328p_DS-revA.ev"), include_str!("datasheet_fixtures/identity/microchip-atmega328p_DS-revA.text.txt")),
    ("murata-grm188r71c105ka12-mlcc-spec", include_str!("datasheet_fixtures/identity/murata-grm188r71c105ka12-mlcc-spec.ev"), include_str!("datasheet_fixtures/identity/murata-grm188r71c105ka12-mlcc-spec.text.txt")),
    ("ti-ads1115_DS", include_str!("datasheet_fixtures/identity/ti-ads1115_DS.ev"), include_str!("datasheet_fixtures/identity/ti-ads1115_DS.text.txt")),
    ("ti-lm358-family_DS", include_str!("datasheet_fixtures/identity/ti-lm358-family_DS.ev"), include_str!("datasheet_fixtures/identity/ti-lm358-family_DS.text.txt")),
    ("ti-opa2134_DS", include_str!("datasheet_fixtures/identity/ti-opa2134_DS.ev"), include_str!("datasheet_fixtures/identity/ti-opa2134_DS.text.txt")),
    ("ti-slva477b-appnote", include_str!("datasheet_fixtures/identity/ti-slva477b-appnote.ev"), include_str!("datasheet_fixtures/identity/ti-slva477b-appnote.text.txt")),
    ("ti-tmp117_DS", include_str!("datasheet_fixtures/identity/ti-tmp117_DS.ev"), include_str!("datasheet_fixtures/identity/ti-tmp117_DS.text.txt")),
    ("zilog-z80-cpu-productspec-mar78", include_str!("datasheet_fixtures/identity/zilog-z80-cpu-productspec-mar78.ev"), include_str!("datasheet_fixtures/identity/zilog-z80-cpu-productspec-mar78.text.txt")),
    ("zilog-z80-ctc-productspec-feb80", include_str!("datasheet_fixtures/identity/zilog-z80-ctc-productspec-feb80.ev"), include_str!("datasheet_fixtures/identity/zilog-z80-ctc-productspec-feb80.text.txt")),
];

struct Ev {
    parts: Vec<String>,
    maker: Option<String>,
    maker_by: Option<String>,
    title: Option<String>,
    files: Vec<String>,
}

fn parse_ev(raw: &str) -> Ev {
    let mut e = Ev { parts: vec![], maker: None, maker_by: None, title: None, files: vec![] };
    for line in raw.lines() {
        let Some((k, v)) = line.split_once('\t') else { continue };
        match k {
            "P" => e.parts.push(v.to_string()),
            "M" => e.maker = Some(v.to_string()),
            "B" => e.maker_by = Some(v.to_string()),
            "T" => e.title = Some(v.to_string()),
            "F" => e.files.push(v.to_string()),
            _ => {}
        }
    }
    e
}

fn run(ev: &Ev, text: Option<&str>, use_filename: bool) -> Identity {
    reconcile(&Evidence {
        parser_parts: &ev.parts,
        parser_maker: ev.maker.as_deref(),
        parser_maker_by: ev.maker_by.as_deref(),
        title: ev.title.as_deref(),
        file_names: &ev.files,
        text,
        use_filename,
    })
}

fn line(name: &str, id: &Identity) -> String {
    [
        name.to_string(),
        id.maker.clone().unwrap_or_default(),
        id.maker_by.clone().unwrap_or_default(),
        id.parts.join("|"),
        id.parts_by.clone(),
        id.diagnostics.join(" \u{a6} "),
    ]
    .join("\t")
}

fn identity_of(name: &str, use_filename: bool) -> Identity {
    let (_, ev, text) = DOCS.iter().find(|d| d.0 == name).expect("known document");
    run(&parse_ev(ev), Some(*text), use_filename)
}

#[test]
fn the_rust_reproduces_what_the_corpus_scorer_prints_for_the_same_excerpts() {
    for (expected, use_filename) in [(EXPECTED, true), (EXPECTED_NOFILE, false)] {
        let want: BTreeMap<&str, &str> = expected.lines().filter_map(|l| l.split_once('\t').map(|(k, _)| (k, l))).collect();
        assert_eq!(want.len(), DOCS.len());
        for (name, ev, text) in DOCS {
            let got = line(name, &run(&parse_ev(ev), Some(*text), use_filename));
            assert_eq!(got, want[name], "{name} (filename witness {use_filename})");
        }
    }
}

#[test]
fn ads1115_the_contested_maker_is_texas_instruments_and_the_disagreement_is_recorded() {
    let id = identity_of("ti-ads1115_DS", false);
    assert_eq!(id.maker.as_deref(), Some("Texas Instruments"));
    assert_eq!(id.maker_by.as_deref(), Some("text"));
    assert_eq!(id.parts, ["ADS1113", "ADS1114", "ADS1115"]);
    assert_eq!(id.literature.as_deref(), Some("SBAS444E"));
    assert!(
        id.diagnostics.iter().any(|d| d == "maker contested: parser=Analog Devices (part-prefix) vs text=Texas Instruments"),
        "{:?}",
        id.diagnostics
    );
}

#[test]
fn the_lm358_family_is_the_whole_family_in_document_order() {
    let id = identity_of("ti-lm358-family_DS", false);
    assert_eq!(
        id.parts,
        ["LM158", "LM158A", "LM258", "LM258A", "LM2904", "LM2904B", "LM2904BA", "LM2904V", "LM358", "LM358A", "LM358B", "LM358BA"]
    );
    assert_eq!(id.maker.as_deref(), Some("Texas Instruments"));
}

#[test]
fn bsc016n06ns_has_no_parser_part_and_still_has_one() {
    let (_, ev, _) = DOCS.iter().find(|d| d.0 == "infineon-bsc016n06ns_DS_V2.6").unwrap();
    let ev = parse_ev(ev);
    assert_eq!(ev.parts, ["infineon-bsc016n06ns_DS_V2.6"], "Tom fell back to the file name");
    let id = identity_of("infineon-bsc016n06ns_DS_V2.6", false);
    assert_eq!(id.parts, ["BSC016N06NS"]);
    assert_eq!(id.parts_by, "title");
    assert_eq!(id.maker.as_deref(), Some("Infineon"));
}

#[test]
fn atmega328p_names_all_eight_family_members() {
    let id = identity_of("microchip-atmega328p_DS-revA", false);
    assert_eq!(
        id.parts,
        ["ATmega48A", "ATmega48PA", "ATmega88A", "ATmega88PA", "ATmega168A", "ATmega168PA", "ATmega328", "ATmega328P"]
    );
    assert_eq!(id.maker.as_deref(), Some("Microchip Technology"));
    assert_eq!(id.maker_by.as_deref(), Some("text+parser"));
}

#[test]
fn murata_gets_its_part_number_not_a_fragment_of_it() {
    let id = identity_of("murata-grm188r71c105ka12-mlcc-spec", false);
    assert_eq!(id.parts, ["GRM188R71C105KA12"]);
    assert_eq!(id.maker.as_deref(), Some("Murata"));
    assert!(id.diagnostics.iter().any(|d| d.starts_with("parts contested: parser=[\"A12\"]")), "{:?}", id.diagnostics);
}

#[test]
fn opa2134_and_the_z80_cpu_keep_what_the_parser_got_right() {
    let id = identity_of("ti-opa2134_DS", false);
    assert_eq!(id.parts, ["OPA134", "OPA2134", "OPA4134"]);
    assert_eq!(id.maker.as_deref(), Some("Texas Instruments"));
    let id = identity_of("zilog-z80-cpu-productspec-mar78", false);
    assert_eq!(id.parts[0], "Z80");
    assert_eq!(id.maker.as_deref(), Some("Zilog"));
}

#[test]
fn the_file_name_is_a_weak_witness_and_can_be_switched_off() {
    // The CTC sheet prints no part number a heading can carry; only the file name has one.
    assert_eq!(identity_of("zilog-z80-ctc-productspec-feb80", true).parts, ["Z80-CTC"]);
    assert_eq!(identity_of("zilog-z80-ctc-productspec-feb80", true).parts_by, "filename");
    assert!(identity_of("zilog-z80-ctc-productspec-feb80", false).parts.is_empty());
    // A document that names no part names none: an app note, a connector drawing.
    assert!(identity_of("ti-slva477b-appnote", true).parts.is_empty());
    assert_eq!(identity_of("ti-slva477b-appnote", true).literature.as_deref(), Some("SLVA477B"));
    assert!(identity_of("jst-ph-connector", true).parts.is_empty());
    assert_eq!(identity_of("jst-ph-connector", false).maker.as_deref(), Some("JST"));
}

#[test]
fn headings_stop_at_prose() {
    assert_eq!(lead_parts("TMP117 High-Accuracy, Low-Power, Digital Temperature Sensor"), ["TMP117"]);
    assert!(lead_parts("ADS111x Ultra-Small, Low-Power, I 2 C-Compatible ADCs").is_empty());
}

fn facts(store: &Store, part: &str) -> Value {
    facts_from_store(
        store,
        GID,
        &FactsQuery { part: Some(part.to_string()), artifact: None, parameter: None, limit: 20 },
    )
    .expect("facts")
}

#[test]
fn an_import_with_the_text_rendition_records_and_reports_how_it_knows() {
    let mut env: Value = serde_json::from_str(include_str!("datasheet_fixtures/real/ti-ads1115_DS.data.json")).unwrap();
    env["extraction"]["text_disagreement_pages"] = json!([1, 18, 42]);
    // The identity fields exactly as Tom's 2026-10-08 read of this PDF printed them (the committed
    // envelope is an earlier run of the same file).
    env["document"]["title"] = json!("ADS1113, ADS1114, ADS1115 SBAS444E - MAY 2009 - REVISED DECEMBER 2024");
    env["document"]["parts"] = json!(["ADS1113"]);
    env["document"]["manufacturer"] = json!("Analog Devices");
    env["document"]["manufacturer_by"] = json!("part-prefix");
    let text = DOCS.iter().find(|d| d.0 == "ti-ads1115_DS").unwrap().2;
    let dir = std::env::temp_dir().join(format!("garden-datasheet-identity-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir).expect("store");

    // Without the text: Tom's guess is widened by the title, and the maker is the weak file-name reading
    // (the parser's own `part-prefix` guess says Analog Devices).
    let bare = ImportInput::from_inline(env.clone());
    let report = import_into_store(&store, GID, &bare).expect("import");
    let f = facts(&store, "ADS1115");
    let ed = &f["editions"][0];
    assert_eq!(ed["maker"], "Texas Instruments");
    assert_eq!(ed["makerBy"], "filename");
    assert_eq!(ed["parts"].as_array().unwrap().len(), 3);
    assert!(report.mapped.diagnostics.iter().any(|d| d.starts_with("maker contested: parser=Analog Devices (part-prefix) vs filename")));

    // With it: the document's own text decides, and says so.
    let mut with_text = ImportInput::from_inline(env.clone());
    with_text.text = Some(text.to_string());
    with_text.text_inline = true;
    import_into_store(&store, GID, &with_text).expect("again");
    let f = facts(&store, "ADS1115");
    assert_eq!(f["matched"], 1);
    let ed = &f["editions"][0];
    assert_eq!(ed["maker"], "Texas Instruments");
    assert_eq!(ed["makerBy"], "text+filename");
    assert_eq!(ed["partsBy"], "title+parser");
    assert_eq!(ed["literature"], "SBAS444E");
    let diags: Vec<&str> = ed["diagnostics"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(diags.contains(&"maker contested: parser=Analog Devices (part-prefix) vs text=Texas Instruments"), "{diags:?}");
    assert_eq!(ed["identity"]["parser"]["maker"], "Analog Devices");
    assert_eq!(ed["identity"]["parser"]["makerBy"], "part-prefix");
    assert_eq!(ed["identity"]["text"]["maker"], "Texas Instruments");
    // The reader run Tom's envelope reports.
    // Inline text has no artifact: its provenance is a digest and a length, marked as supplied inline.
    let quads = |pred: String| -> Vec<String> {
        let p = oxigraph::model::NamedNode::new(pred).unwrap();
        store.quads_for_pattern(None, Some(p.as_ref()), None, None).map(|q| q.unwrap().object.to_string()).collect()
    };
    assert_eq!(quads(crate::datasheet_import::de("suppliedInline")).len(), 1);
    let digests = quads(crate::datasheet_import::de("digest"));
    let want = format!("/{}", text.len());
    assert!(digests.iter().any(|d| d.contains("sha256:") && d.contains(&format!(":{}\"", text.len()))), "{digests:?} {want}");
    assert_eq!(ed["reader"]["tools"]["docling"], "2.135.0");
    assert_eq!(ed["reader"]["tools"]["datasheet-parser"], "ee680a6");
    assert_eq!(ed["reader"]["readers"]["tables"], "pdfplumber");
    assert_eq!(ed["reader"]["generated"], "2026-10-08");
    assert_eq!(ed["reader"]["pdfplumberSections"], 5);
    assert_eq!(ed["reader"]["textDisagreementPages"], json!([1, 18, 42]));
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

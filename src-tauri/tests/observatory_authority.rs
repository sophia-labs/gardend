//! Observatory two-writer authority proof harness (A0/A9, §A.6/§A.9 of
//! `plans/observatory-analysis-cell-spec-20260715.md` in the `sophia` hub
//! repo).
//!
//! Direction tests + one identity smoke, run against a REAL on-disk
//! gardend Oxigraph store (`garden_lib::observatory::authority_harness::open_real_store`
//! — the exact function `rdf_service.rs` opens for every RDF request) and
//! the REAL production gate functions
//! (`validate_sparql_update_authority`/`user_rdf_target_graph_iri`/
//! `validate_rdf_dataset_graph_targets`), reached read-only through the
//! `garden_lib::observatory` bridge — `src/rdf_authority.rs` only grew a
//! visibility change for A9 (`is_reserved_rdf_graph_iri` widened to
//! `pub(crate)`; no logic changed), never otherwise edited by this slice.
//!
//! Direction 1-5 prove the gate REFUSES every reserved-graph write shape
//! §A.6 names, each paired with a non-reserved CONTROL that proves the same
//! update/load SHAPE is syntactically real and genuinely writes — so a
//! refusal is provably about reserved-ness, not malformed SPARQL/RDF.
//!
//! Direction 6 (and its per-format siblings below it) close the ONE known
//! A9 gap: `load_rdf_dataset` (TriG/N-Quads/JSON-LD all carry named-graph
//! IRIs INLINE in the payload) now scans every named graph a dataset
//! carries and refuses the whole import if any is reserved. Covered, each
//! with a REFUSE + non-reserved-CONTROL pair: N-Quads (Direction 6), TriG,
//! JSON-LD (built via a REAL store round-trip, not hand-authored dataset
//! JSON-LD syntax); a MIXED payload with non-reserved quads both before AND
//! after the one reserved quad (still wholly refused, nothing partial
//! lands); a blank-node graph name (TriG `_:g{}`, N-Quads `… _:g .`, and an
//! N3 `{ … }` formula — all represented by oxigraph as `GraphName::BlankNode`,
//! a different term kind than a reserved `NamedNode` IRI, so ALWAYS ACCEPTED
//! and proven to actually land); and the "default/unnamed graph" mapping for
//! the four formats that carry NO named-graph syntax addressable via
//! `GRAPH <iri>` at all (Turtle/N-Triples/RDF-XML/N3's non-formula content)
//! — which must always be ACCEPTED, since they can never syntactically
//! express a reserved target.

use garden_lib::observatory::authority_harness::{
    execute_update_unchecked, graph_has_any_quad, load_rdf_dataset_unchecked, load_rdf_unchecked,
    open_real_store, resolve_load_rdf_target, validate_rdf_dataset_targets, validate_sparql_update,
};
use garden_lib::observatory::graph_identity::{
    bare_projection_obs_graph_iri, graph_subject, projection_obs_iris, raw_graph_iri,
    rollups_graph_iri, user_rdf_graph_iri, GRAPH_ID,
};
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::store::Store as ScratchStore;
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fresh temp dir per test/case — never shared, so each case's `Store` is
/// isolated and `quad_count`/`ASK` reads are unambiguous.
fn temp_graph_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sophia-observatory-authority-{label}-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create temp graph dir");
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Shared shape for Directions 1-3: an `INSERT DATA { GRAPH <reserved> {…} }`
/// must be refused by the real gate, AND the identical shape retargeted at a
/// non-reserved graph (`:user:rdf`) must be accepted by the gate AND
/// genuinely land a quad on a real store — proving the refusal is
/// reserved-ness-specific, not a syntax accident.
fn assert_insert_data_refused_but_shape_is_real(reserved_graph: &str, label: &str) {
    let update = format!(
        "INSERT DATA {{ GRAPH <{reserved_graph}> {{ <urn:sophia:observatory:probe:{label}> <http://mnemosyne.dev/observatory#objectType> \"Probe\" . }} }}"
    );
    assert!(
        validate_sparql_update(GRAPH_ID, &update).is_err(),
        "{label}: INSERT DATA targeting {reserved_graph} must be refused by validate_sparql_update_authority"
    );

    let control_graph = user_rdf_graph_iri();
    let control_update = update.replacen(reserved_graph, &control_graph, 1);
    assert!(
        validate_sparql_update(GRAPH_ID, &control_update).is_ok(),
        "{label}: the identical shape against the non-reserved :user:rdf graph must be accepted"
    );

    let dir = temp_graph_dir(label);
    let store = open_real_store(&dir).expect("open real store");
    execute_update_unchecked(&store, &control_update)
        .expect("control update must execute cleanly against a real store");
    assert!(
        graph_has_any_quad(&store, &control_graph).expect("query real store"),
        "{label}: control update must actually land a quad in :user:rdf"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Smoke — graph identity + reserved-IRI recognition
// ---------------------------------------------------------------------------

#[test]
fn observatory_authority_smoke_graph_subject_and_reserved_projection_iris() {
    assert_eq!(graph_subject(), "urn:mnemosyne:local:graph:observatory");

    // The three `:projection:obs*` IRIs must each be recognized reserved by
    // `is_reserved_rdf_graph_iri` — proven indirectly through
    // `user_rdf_target_graph_iri`, the real (pub(crate)) production
    // entrypoint that calls it directly (rdf_authority.rs:200-215,253-258).
    for iri in projection_obs_iris() {
        let result = resolve_load_rdf_target(GRAPH_ID, Some(iri.as_str()));
        assert!(
            result.is_err(),
            "{iri} must be recognized reserved by is_reserved_rdf_graph_iri"
        );
    }

    // Negative control: a non-reserved graph in the SAME graph_id resolves
    // cleanly and unchanged — proves the three refusals above are about
    // reserved-ness, not about graph_id "observatory" being special.
    let user_graph = user_rdf_graph_iri();
    assert_eq!(
        resolve_load_rdf_target(GRAPH_ID, Some(user_graph.as_str())).expect("user graph resolves"),
        user_graph
    );

    // And the package default (no explicit target) also resolves to :user:rdf.
    assert_eq!(
        resolve_load_rdf_target(GRAPH_ID, None).expect("default target resolves"),
        user_graph
    );
}

// ---------------------------------------------------------------------------
// Direction 1 — INSERT DATA into :projection:obs:raw refused
// ---------------------------------------------------------------------------

#[test]
fn observatory_authority_direction_1_insert_data_raw_graph_refused() {
    assert_insert_data_refused_but_shape_is_real(&raw_graph_iri(), "d1-raw");
}

// ---------------------------------------------------------------------------
// Direction 2 — INSERT DATA into :projection:obs:rollups refused
// ---------------------------------------------------------------------------

#[test]
fn observatory_authority_direction_2_insert_data_rollups_graph_refused() {
    assert_insert_data_refused_but_shape_is_real(&rollups_graph_iri(), "d2-rollups");
}

// ---------------------------------------------------------------------------
// Direction 3 — INSERT DATA into the bare :projection:obs root refused
// ("the bare base" in §A.6's enumeration)
// ---------------------------------------------------------------------------

#[test]
fn observatory_authority_direction_3_insert_data_bare_projection_obs_refused() {
    assert_insert_data_refused_but_shape_is_real(&bare_projection_obs_graph_iri(), "d3-bare");
}

// ---------------------------------------------------------------------------
// Direction 4 — CLEAR NAMED / DROP NAMED on the projection graphs refused
// ---------------------------------------------------------------------------

#[test]
fn observatory_authority_direction_4_clear_and_drop_named_refused() {
    let raw = raw_graph_iri();
    let rollups = rollups_graph_iri();
    for target in [raw.as_str(), rollups.as_str()] {
        assert!(
            validate_sparql_update(GRAPH_ID, &format!("CLEAR NAMED <{target}>")).is_err(),
            "CLEAR NAMED <{target}> must be refused"
        );
        assert!(
            validate_sparql_update(GRAPH_ID, &format!("DROP NAMED <{target}>")).is_err(),
            "DROP NAMED <{target}> must be refused"
        );
    }

    // Documented, not a gap: `validate_sparql_update_authority` forbids
    // CLEAR/DROP NAMED unconditionally (rdf_authority.rs:217-237 checks the
    // bare keyword substring before it ever inspects the target), so even a
    // non-reserved graph is refused. Recorded here so a future reader does
    // not mistake this for reserved-graph-specific behavior.
    let user_graph = user_rdf_graph_iri();
    assert!(
        validate_sparql_update(GRAPH_ID, &format!("CLEAR NAMED <{user_graph}>")).is_err(),
        "CLEAR NAMED is refused unconditionally, even for a non-reserved graph"
    );
}

// ---------------------------------------------------------------------------
// Direction 5 — WITH <:rollups> DELETE…INSERT…WHERE… refused;
// load_rdf targeting :rollups refused
// ---------------------------------------------------------------------------

#[test]
fn observatory_authority_direction_5_with_delete_insert_where_and_load_rdf_refused() {
    let rollups = rollups_graph_iri();
    let with_update = format!(
        "WITH <{rollups}> DELETE {{ ?s ?p ?o }} INSERT {{ ?s ?p ?o }} WHERE {{ ?s ?p ?o }}"
    );
    assert!(
        validate_sparql_update(GRAPH_ID, &with_update).is_err(),
        "WITH <:rollups> DELETE…INSERT…WHERE… must be refused"
    );

    assert!(
        resolve_load_rdf_target(GRAPH_ID, Some(rollups.as_str())).is_err(),
        "load_rdf targeting :rollups must be refused by user_rdf_target_graph_iri"
    );

    // Positive control: `load_rdf_into_store` (the exact write `load_rdf`
    // performs once its target has cleared the gate) genuinely writes when
    // given a non-reserved target — proving the refusal above is
    // reserved-ness-specific, not e.g. a malformed-data rejection.
    let control_graph = user_rdf_graph_iri();
    assert!(resolve_load_rdf_target(GRAPH_ID, Some(control_graph.as_str())).is_ok());

    let dir = temp_graph_dir("d5-load-rdf");
    let store = open_real_store(&dir).expect("open real store");
    let triple =
        "<urn:sophia:observatory:probe> <http://mnemosyne.dev/observatory#objectType> \"Probe\" .\n";
    load_rdf_unchecked(&store, triple, "nt", &control_graph).expect("control load_rdf writes");
    assert!(
        graph_has_any_quad(&store, &control_graph).expect("query real store"),
        "control load_rdf must actually land a quad in :user:rdf"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Direction 6 — load_rdf_dataset reserved-graph gap (A9 — CLOSED)
// ---------------------------------------------------------------------------

/// Build REAL dataset-format bytes naming `graph_iri` as the graph of a
/// single quad, by loading a real N-Quads quad into a fresh, throwaway
/// in-memory oxigraph `Store` and dumping it back out through oxigraph's OWN
/// serializer for `format` — a genuine round-trip through the same library
/// version `load_rdf_dataset` itself parses with, not hand-authored dataset
/// syntax. This is what makes the JSON-LD case trustworthy without having to
/// guess its named-graph dataset serialization shape by hand.
fn dataset_bytes_naming_graph(format: RdfFormat, graph_iri: &str, subject_label: &str) -> String {
    let seed_store = ScratchStore::new().expect("scratch store");
    let quad = format!(
        "<urn:sophia:observatory:capture:{subject_label}> <http://mnemosyne.dev/observatory#payloadJson> \"{{}}\" <{graph_iri}> .\n"
    );
    seed_store
        .load_from_slice(RdfParser::from_format(RdfFormat::NQuads), quad.as_bytes())
        .expect("seed scratch store with a real n-quads quad");
    let bytes = seed_store
        .dump_to_writer(format, Vec::new())
        .unwrap_or_else(|error| panic!("dump scratch store as {format:?}: {error}"));
    String::from_utf8(bytes).expect("dumped dataset is UTF-8")
}

/// Shared shape for the dataset-format Directions: a payload that
/// inline-names `reserved_graph` as its ONE named graph must be refused by
/// BOTH the service-layer pre-check (`validate_rdf_dataset_targets`, the
/// `AppError::validation`-mapped gate `load_rdf_dataset_service` runs) and
/// the store-write primitive itself (`load_rdf_dataset_unchecked`, so NO
/// quad lands even for a caller that reaches the primitive directly) — AND
/// the identical shape retargeted at non-reserved `:user:rdf` must be
/// accepted by both and genuinely land a quad, proving the refusal is
/// reserved-ness-specific, not e.g. `format` being broken.
fn assert_dataset_import_refused_but_shape_is_real(
    format_name: &str,
    format: RdfFormat,
    label: &str,
) {
    let reserved_graph = raw_graph_iri();
    let reserved_bytes = dataset_bytes_naming_graph(format, &reserved_graph, label);

    assert!(
        validate_rdf_dataset_targets(GRAPH_ID, &reserved_bytes, format_name).is_err(),
        "{label} ({format_name}): service-layer pre-check must refuse a payload inline-naming \
         {reserved_graph}"
    );

    let dir = temp_graph_dir(label);
    let store = open_real_store(&dir).expect("open real store");
    let write_result = load_rdf_dataset_unchecked(&store, GRAPH_ID, &reserved_bytes, format_name);
    let landed = graph_has_any_quad(&store, &reserved_graph).expect("query real store");
    cleanup(&dir);
    assert!(
        write_result.is_err(),
        "{label} ({format_name}): the store-write primitive itself must refuse, not just the \
         service pre-check"
    );
    assert!(
        !landed,
        "{label} ({format_name}): a refused dataset import must not land a quad in the \
         reserved graph"
    );

    // Non-reserved control: the identical shape, naming :user:rdf instead.
    let control_graph = user_rdf_graph_iri();
    let control_bytes =
        dataset_bytes_naming_graph(format, &control_graph, &format!("{label}-control"));
    assert!(
        validate_rdf_dataset_targets(GRAPH_ID, &control_bytes, format_name).is_ok(),
        "{label} ({format_name}): the identical shape against non-reserved :user:rdf must be \
         accepted by the pre-check"
    );

    let control_dir = temp_graph_dir(&format!("{label}-control"));
    let control_store = open_real_store(&control_dir).expect("open real store");
    load_rdf_dataset_unchecked(&control_store, GRAPH_ID, &control_bytes, format_name)
        .expect("control dataset import must execute cleanly against a real store");
    assert!(
        graph_has_any_quad(&control_store, &control_graph).expect("query real store"),
        "{label} ({format_name}): control dataset import must actually land a quad in :user:rdf"
    );
    cleanup(&control_dir);
}

/// N-Quads — the format the original A9 gap was discovered against
/// (`ba1a97eccc83:src-tauri/src/rdf_service.rs:226-250`). Un-ignored: this
/// is now GREEN.
#[test]
fn observatory_authority_direction_6_load_rdf_dataset_nquads_reserved_graph_refused() {
    assert_dataset_import_refused_but_shape_is_real("nq", RdfFormat::NQuads, "d6-nquads");
}

/// TriG — the other line-oriented dataset format `load_rdf_dataset` accepts.
#[test]
fn observatory_authority_direction_6_load_rdf_dataset_trig_reserved_graph_refused() {
    assert_dataset_import_refused_but_shape_is_real("trig", RdfFormat::TriG, "d6-trig");
}

/// JSON-LD — the third and last dataset-carrying format `load_rdf_dataset`
/// accepts (`RdfFormat::supports_datasets()` is `true` for exactly
/// `JsonLd | NQuads | TriG`). Built via the real store round-trip helper
/// above, so the fixture's exact JSON-LD dataset shape (an `@id`/`@graph`
/// wrapper) never has to be hand-guessed.
#[test]
fn observatory_authority_direction_6_load_rdf_dataset_jsonld_reserved_graph_refused() {
    let jsonld = RdfFormat::from_extension("jsonld").expect("JSON-LD format is available");
    assert!(jsonld.supports_datasets(), "JSON-LD must support datasets");
    assert_dataset_import_refused_but_shape_is_real("jsonld", jsonld, "d6-jsonld");
}

/// The "default/unnamed graph" mapping: Turtle, N-Triples, RDF-XML, and N3
/// (the fixtures below use none of them) carry NO named-graph syntax at all
/// (`RdfFormat::supports_datasets()` is `false` for all four) — every quad
/// THEY parse lands in the store's unnamed DEFAULT graph, which has no IRI
/// and can therefore never collide with a reserved `:projection:*` target.
/// `load_rdf_dataset` MUST still ACCEPT these: the reserved-graph refusal is
/// about reserved-ness, never a blanket ban on non-dataset formats.
///
/// CAVEAT (review r1 finding): N3 is the one exception to "no named-graph
/// syntax at all" — a `{ … }` FORMULA quote is NOT a dataset named graph
/// (`supports_datasets()` is still `false`, and a formula can't be reached
/// via `GRAPH <iri>` from the outside), but its CONTENTS parse with a
/// `GraphName::BlankNode`, not `DefaultGraph` — see
/// `rdf_query_service.rs`'s `oxigraph_represents_blank_node_graph_labels_as_graphname_blanknode`
/// unit test and this file's blank-node sibling
/// (`observatory_authority_direction_6_blank_node_graph_targets_are_accepted_on_a_real_store`)
/// for the N3-formula-specific proof. Still never reserved either way — a
/// blank node can't equal a reserved `NamedNode` IRI — just not literally
/// "DEFAULT graph" for that one shape.
#[test]
fn observatory_authority_direction_6_non_dataset_formats_map_to_default_graph_and_are_accepted() {
    let raw = raw_graph_iri();
    let cases: [(&str, RdfFormat, String); 4] = [
        (
            "turtle",
            RdfFormat::Turtle,
            r#"<urn:sophia:observatory:capture:d6-ttl> <http://mnemosyne.dev/observatory#payloadJson> "{}" ."#
                .to_string(),
        ),
        (
            "nt",
            RdfFormat::NTriples,
            r#"<urn:sophia:observatory:capture:d6-nt> <http://mnemosyne.dev/observatory#payloadJson> "{}" ."#
                .to_string(),
        ),
        (
            "rdf",
            RdfFormat::RdfXml,
            r#"<?xml version="1.0"?><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:obs="http://mnemosyne.dev/observatory#"><rdf:Description rdf:about="urn:sophia:observatory:capture:d6-rdfxml"><obs:payloadJson>{}</obs:payloadJson></rdf:Description></rdf:RDF>"#
                .to_string(),
        ),
        (
            "n3",
            RdfFormat::N3,
            r#"<urn:sophia:observatory:capture:d6-n3> <http://mnemosyne.dev/observatory#payloadJson> "{}" ."#
                .to_string(),
        ),
    ];

    for (format_name, format, body) in cases {
        assert!(
            !format.supports_datasets(),
            "{format_name}: expected a non-dataset format for this bucket"
        );
        assert!(
            validate_rdf_dataset_targets(GRAPH_ID, &body, format_name).is_ok(),
            "{format_name}: a format with no named-graph syntax can never carry a reserved \
             target; must be accepted"
        );

        let dir = temp_graph_dir(&format!("d6-default-graph-{format_name}"));
        let store = open_real_store(&dir).expect("open real store");
        let write_result = load_rdf_dataset_unchecked(&store, GRAPH_ID, &body, format_name);
        assert!(
            write_result.is_ok(),
            "{format_name}: dataset import must execute cleanly: {write_result:?}"
        );
        assert!(
            !graph_has_any_quad(&store, &raw).expect("query real store"),
            "{format_name}: a non-dataset format's quads must never appear under the reserved \
             raw graph"
        );
        cleanup(&dir);
    }
}

// ---------------------------------------------------------------------------
// Direction 6 (blank-node siblings) — a blank-node graph name is a
// DIFFERENT RDF term kind than a reserved `NamedNode` IRI, so it can never
// literally BE reserved. Proven against the REAL on-disk store + REAL gate
// (service-layer pre-check AND store-write primitive), not just accepted
// "because nothing crashed" — the quad(s) must actually land somewhere.
// ---------------------------------------------------------------------------

/// TriG `_:g { … }`, N-Quads `… _:g .`, and an N3 `{ … }` formula quote are
/// ALL represented by oxigraph as `GraphName::BlankNode` (pinned at the unit
/// level in `rdf_query_service.rs`'s
/// `oxigraph_represents_blank_node_graph_labels_as_graphname_blanknode`) — a
/// fresh, process-scoped identifier of a different term kind than
/// `NamedNode`, so it can never collide with a reserved `:projection:obs*`
/// IRI by construction. `load_rdf_dataset` must ACCEPT all three against the
/// REAL gate, and the quad(s) must genuinely be present in the real on-disk
/// store afterward.
#[test]
fn observatory_authority_direction_6_blank_node_graph_targets_are_accepted_on_a_real_store() {
    for (format_name, body) in [
        (
            "trig",
            r#"_:g1 { <urn:sophia:observatory:probe:d6-blank-trig> <http://mnemosyne.dev/observatory#objectType> "Probe" . }"#.to_string(),
        ),
        (
            "application/n-quads",
            r#"<urn:sophia:observatory:probe:d6-blank-nquads> <http://mnemosyne.dev/observatory#objectType> "Probe" _:g1 ."#.to_string(),
        ),
        (
            "n3",
            r#"<urn:sophia:observatory:probe:d6-blank-n3> <http://mnemosyne.dev/observatory#objectType> { <urn:sophia:observatory:probe:d6-blank-n3-inner> <http://mnemosyne.dev/observatory#objectType> "Probe" } ."#.to_string(),
        ),
    ] {
        assert!(
            validate_rdf_dataset_targets(GRAPH_ID, &body, format_name).is_ok(),
            "{format_name}: a blank-node graph target must never be treated as reserved by the \
             service-layer pre-check"
        );

        let dir = temp_graph_dir(&format!("d6-blank-{format_name}"));
        let store = open_real_store(&dir).expect("open real store");
        let write_result = load_rdf_dataset_unchecked(&store, GRAPH_ID, &body, format_name);
        assert!(
            write_result.is_ok(),
            "{format_name}: blank-node-graph import must execute cleanly against the real \
             store-write primitive: {write_result:?}"
        );
        assert!(
            store.len().expect("count real store quads") > 0,
            "{format_name}: the blank-node-graph quad(s) must actually land somewhere in the \
             real on-disk store"
        );
        cleanup(&dir);
    }
}

/// MIXED dataset against the REAL store: a non-reserved quad, THEN the one
/// reserved quad, THEN another non-reserved quad, for N-Quads and TriG. The
/// whole import must be refused by both the service-layer pre-check and the
/// store-write primitive, and — critically, on a REAL on-disk store, not the
/// in-memory `Store::new()` the unit-level twin of this test uses — NOTHING
/// may land, not even the two legitimate quads that sandwich the reserved
/// one.
#[test]
fn observatory_authority_direction_6_mixed_dataset_with_reserved_quad_is_wholly_refused_on_a_real_store(
) {
    let raw = raw_graph_iri();
    let user_graph = user_rdf_graph_iri();

    let nquads = format!(
        "<urn:sophia:observatory:probe:d6-mixed-1> <http://mnemosyne.dev/observatory#objectType> \"Probe\" <{user_graph}> .\n\
         <urn:sophia:observatory:probe:d6-mixed-2> <http://mnemosyne.dev/observatory#objectType> \"Probe\" <{raw}> .\n\
         <urn:sophia:observatory:probe:d6-mixed-3> <http://mnemosyne.dev/observatory#objectType> \"Probe\" <{user_graph}> .\n"
    );
    let trig = format!(
        "<{user_graph}> {{ <urn:sophia:observatory:probe:d6-mixed-trig-1> <http://mnemosyne.dev/observatory#objectType> \"Probe\" . }}\n\
         <{raw}> {{ <urn:sophia:observatory:probe:d6-mixed-trig-2> <http://mnemosyne.dev/observatory#objectType> \"Probe\" . }}\n\
         <{user_graph}> {{ <urn:sophia:observatory:probe:d6-mixed-trig-3> <http://mnemosyne.dev/observatory#objectType> \"Probe\" . }}\n"
    );

    for (format_name, body) in [("application/n-quads", nquads), ("trig", trig)] {
        assert!(
            validate_rdf_dataset_targets(GRAPH_ID, &body, format_name).is_err(),
            "{format_name}: a payload with ANY reserved-target quad must be refused by the \
             service-layer pre-check, regardless of the non-reserved quads around it"
        );

        let dir = temp_graph_dir(&format!("d6-mixed-{format_name}"));
        let store = open_real_store(&dir).expect("open real store");
        let write_result = load_rdf_dataset_unchecked(&store, GRAPH_ID, &body, format_name);
        assert!(
            write_result.is_err(),
            "{format_name}: the store-write primitive must also refuse the mixed payload"
        );
        assert_eq!(
            store.len().expect("count real store quads"),
            0,
            "{format_name}: NEITHER surrounding non-reserved quad may land on the real store — \
             the whole import is refused"
        );
        cleanup(&dir);
    }
}

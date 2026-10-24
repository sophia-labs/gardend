//! A real exported package only: no mock graph, vectors, or service. The data-backed test is ignored by default;
//! run it with GARDEN_SWARM_FIXTURE_DIR=<package with graph.nq + manifest.json> and `-- --ignored swarm_real`.
//! GARDEN_SWARM_MASK_GRAPH_PREFIX optionally limits which named graphs the masking checks pick.
use crate::{
    swarm_read::{self, Mask},
    swarm_store::{self, Snapshot, SourceSpec},
};
use oxigraph::{
    io::{RdfFormat, RdfParser},
    model::{GraphName, Quad},
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::BufReader,
    path::PathBuf,
    sync::{mpsc, Arc, OnceLock},
    time::Duration,
};

fn fixture_spec() -> SourceSpec {
    let package =
        PathBuf::from(std::env::var_os("GARDEN_SWARM_FIXTURE_DIR").expect(
            "set GARDEN_SWARM_FIXTURE_DIR to an exported package (graph.nq + manifest.json)",
        ));
    assert!(
        package.join("graph.nq").is_file() && package.join("manifest.json").is_file(),
        "GARDEN_SWARM_FIXTURE_DIR has no graph.nq + manifest.json: {}",
        package.display()
    );
    SourceSpec {
        rdf_path: package.join("graph.nq"),
        embeddings_manifest_path: package.join("manifest.json"),
        manifest_sha256: swarm_store::sha256_file(&package.join("manifest.json")).unwrap(),
        generation: "real-export".into(),
    }
}

fn fixture() -> Arc<Snapshot> {
    static FIXTURE: OnceLock<Arc<Snapshot>> = OnceLock::new();
    Arc::clone(
        FIXTURE.get_or_init(|| {
            Arc::new(Snapshot::load(&fixture_spec()).expect("real export must load"))
        }),
    )
}

fn count(value: &Value) -> usize {
    value["rows"][0]["count"]
        .as_str()
        .unwrap()
        .trim_start_matches('"')
        .split('"')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn edge(quad: &Quad) -> Value {
    let GraphName::NamedNode(graph) = &quad.graph_name else {
        panic!("real export requires named graphs")
    };
    json!({"subject": quad.subject.to_string(), "predicate": quad.predicate.as_str(),
           "object": quad.object.to_string(), "graph": graph.as_str()})
}

#[test]
#[ignore = "needs a real exported package: set GARDEN_SWARM_FIXTURE_DIR and run with --ignored"]
fn swarm_real_export_masking_and_publication() {
    let snapshot = fixture();
    let prefix = std::env::var("GARDEN_SWARM_MASK_GRAPH_PREFIX").unwrap_or_default();
    let first = snapshot
        .embeddings
        .iter()
        .find(|r| r.meta.graph.starts_with(&prefix))
        .unwrap();
    let second = snapshot
        .embeddings
        .iter()
        .find(|r| r.meta.graph.starts_with(&prefix) && r.meta.graph != first.meta.graph)
        .unwrap();
    let graphs = [first.meta.graph.clone(), second.meta.graph.clone()];
    // Independent streaming parse: retain just two real named graphs, not a second
    // full 1GB corpus. HashSet handles duplicate quads in the source exporter.
    let mut source: HashSet<Quad> = HashSet::new();
    for quad in RdfParser::from_format(RdfFormat::NQuads)
        .for_reader(BufReader::new(File::open(fixture_spec().rdf_path).unwrap()))
    {
        let quad = quad.unwrap();
        if matches!(&quad.graph_name, GraphName::NamedNode(g) if graphs.iter().any(|s| s == g.as_str()))
        {
            source.insert(quad);
        }
    }
    let unmasked = Mask::new(&[]).unwrap();
    for graph in &graphs {
        let hidden = Mask::new(std::slice::from_ref(graph)).unwrap();
        let expected = source
            .iter()
            .filter(|q| matches!(&q.graph_name, GraphName::NamedNode(g) if g.as_str() == graph))
            .count();
        assert!(expected > 0);
        for query in [
            format!("SELECT (COUNT(*) AS ?count) WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"),
            format!("SELECT (COUNT(*) AS ?count) FROM <{graph}> WHERE {{ ?s ?p ?o }}"),
            format!("SELECT (COUNT(*) AS ?count) FROM NAMED <{graph}> WHERE {{ GRAPH ?g {{ ?s ?p ?o }} }}"),
        ] {
            assert_eq!(count(&swarm_read::query(&snapshot, &unmasked, &query, 10).unwrap()), expected);
            assert_eq!(count(&swarm_read::query(&snapshot, &hidden, &query, 10).unwrap()), 0);
        }
        let examples: Vec<_> = snapshot
            .embeddings
            .iter()
            .filter(|r| r.meta.graph == *graph)
            .take(2)
            .collect();
        for row in examples {
            // A node's provenance pointer is present only in its source named graph.
            let ask = format!("ASK {{ <{}> ?p <{graph}> }}", row.meta.id);
            assert_eq!(
                swarm_read::query(&snapshot, &unmasked, &ask, 10).unwrap()["boolean"],
                true
            );
            assert_eq!(
                swarm_read::query(&snapshot, &hidden, &ask, 10).unwrap()["boolean"],
                false
            );
            let result = swarm_read::semantic(
                &snapshot,
                &hidden,
                &row.vector,
                Some(&row.meta.kind),
                10,
                None,
            )
            .unwrap();
            let expected_count = snapshot
                .embeddings
                .iter()
                .filter(|r| r.meta.kind == row.meta.kind && r.meta.graph != *graph)
                .count()
                .min(10);
            assert_eq!(result["results"].as_array().unwrap().len(), expected_count);
            for found in result["results"].as_array().unwrap() {
                let original = snapshot
                    .embeddings
                    .iter()
                    .find(|r| {
                        r.meta.id == found["id"].as_str().unwrap() && r.meta.kind == row.meta.kind
                    })
                    .unwrap();
                assert_ne!(original.meta.graph, *graph);
                assert_eq!(found["graph"], original.meta.graph);
            }
            let empty =
                swarm_read::semantic(&snapshot, &unmasked, &row.vector, None, 10, Some(&[]))
                    .unwrap();
            assert!(empty["results"].as_array().unwrap().is_empty());
            let only_hidden = vec![row.meta.id.clone()];
            let scoped = swarm_read::semantic(
                &snapshot,
                &hidden,
                &row.vector,
                None,
                10,
                Some(&only_hidden),
            )
            .unwrap();
            assert!(scoped["results"].as_array().unwrap().is_empty());
            assert!(
                swarm_read::traverse(&snapshot, &hidden, &row.meta.id, None, 6, 100).unwrap()
                    ["edges"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            let expected_edges: HashSet<String> = source
                .iter()
                .filter(|q| q.subject.to_string() == format!("<{}>", row.meta.id))
                .map(|q| edge(q).to_string())
                .collect();
            let walked =
                swarm_read::traverse(&snapshot, &unmasked, &row.meta.id, None, 1, 10_000).unwrap();
            let actual: HashSet<String> = walked["edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(Value::to_string)
                .collect();
            assert_eq!(actual, expected_edges);
        }
    }
    let statute = snapshot
        .embeddings
        .iter()
        .find(|row| row.meta.kind == "statutes")
        .unwrap();
    let hide_statutes = Mask::new(std::slice::from_ref(&statute.meta.graph)).unwrap();
    let statute_ask = format!("ASK {{ GRAPH <{}> {{ ?s ?p ?o }} }}", statute.meta.graph);
    assert_eq!(
        swarm_read::query(&snapshot, &unmasked, &statute_ask, 10).unwrap()["boolean"],
        true
    );
    assert_eq!(
        swarm_read::query(&snapshot, &hide_statutes, &statute_ask, 10).unwrap()["boolean"],
        false
    );
    assert!(swarm_read::semantic(
        &snapshot,
        &hide_statutes,
        &statute.vector,
        Some("statutes"),
        10,
        None
    )
    .unwrap()["results"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(
        swarm_read::traverse(&snapshot, &hide_statutes, &statute.meta.id, None, 2, 100).unwrap()
            ["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|edge| edge["graph"] != statute.meta.graph)
    );
    // Each thread masks its own source graph; the shared snapshot never changes.
    std::thread::scope(|scope| {
        for index in 0..8 {
            let snapshot = Arc::clone(&snapshot);
            let graph = graphs[index % 2].clone();
            scope.spawn(move || {
                let mask = Mask::new(std::slice::from_ref(&graph)).unwrap();
                let own = snapshot
                    .embeddings
                    .iter()
                    .find(|r| r.meta.graph == graph)
                    .unwrap();
                let q = format!(
                    "SELECT (COUNT(*) AS ?count) WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"
                );
                assert_eq!(
                    count(&swarm_read::query(&snapshot, &mask, &q, 10).unwrap()),
                    0
                );
                let search =
                    swarm_read::semantic(&snapshot, &mask, &own.vector, None, 20, None).unwrap();
                assert!(search["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["graph"] != graph));
                assert!(
                    swarm_read::traverse(&snapshot, &mask, &own.meta.id, None, 3, 100).unwrap()
                        ["edges"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            });
        }
    });
    // Typed saved-query binding uses the same policy; literal escaping cannot
    // turn a value into graph algebra or a second query.
    let template = "SELECT (COUNT(*) AS ?count) WHERE { GRAPH {{graph}} { ?s ?p ?o } }";
    swarm_read::validate_template(template).unwrap();
    let parameters = BTreeMap::from([("graph".into(), json!({"iri": graphs[0]}))]);
    let rendered = swarm_read::render(template, &parameters).unwrap();
    let hidden = Mask::new(&[graphs[0].clone()]).unwrap();
    assert_eq!(
        count(&swarm_read::query(&snapshot, &hidden, &rendered, 10).unwrap()),
        0
    );
    assert!(swarm_read::render(
        template,
        &BTreeMap::from([("graph".into(), json!({"iri": "urn:x> } UNION { ?s ?p ?o"}))])
    )
    .is_err());
    let quoted = BTreeMap::from([(
        "value".into(),
        json!({"literal": "\" } UNION { ?s ?p ?o } #"}),
    )]);
    let query = swarm_read::render("SELECT ?s WHERE { ?s ?p {{value}} }", &quoted).unwrap();
    for unsafe_context in [
        "SELECT ?s WHERE { ?s ?p ?label FILTER(?label = \"{{value}}\") }",
        "SELECT ?s WHERE { ?s ?p '''{{value}}''' }",
        "SELECT ?s WHERE { ?s ?p <urn:example:{{value}}> }",
        "SELECT ?s WHERE { ?s ?p ?o } # {{value}}",
    ] {
        assert!(swarm_read::validate_template(unsafe_context).is_err());
        assert!(swarm_read::render(unsafe_context, &quoted).is_err());
    }
    assert!(
        swarm_read::query(&snapshot, &hidden, &query, 10).unwrap()["rows"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Holding the import-side lock cannot stall a loaded cell's read path.
    let directory = std::env::temp_dir().join(format!("swarm-real-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let cell = swarm_store::cell(&directory, "real-fixture-incarnation").unwrap();
    cell.publish(Arc::clone(&snapshot)).unwrap();
    let writer = cell.writer.lock().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader_cell = Arc::clone(&cell);
    let reader = std::thread::spawn(move || {
        sender
            .send(reader_cell.snapshot().unwrap().generation.clone())
            .unwrap()
    });
    let read = receiver.recv_timeout(Duration::from_secs(3));
    drop(writer);
    reader.join().unwrap();
    assert_eq!(read.unwrap(), snapshot.generation);
    let old = cell.snapshot().unwrap();
    let mut next = (*snapshot).clone();
    next.generation = "republished-real-export".into();
    cell.publish(Arc::new(next)).unwrap();
    assert_eq!(old.generation, "real-export");
    assert_eq!(
        cell.snapshot().unwrap().generation,
        "republished-real-export"
    );
    assert!(cell
        .import(
            &directory.join("does-not-exist.nq"),
            &fixture_spec().embeddings_manifest_path
        )
        .is_err());
    assert_eq!(
        cell.snapshot().unwrap().generation,
        "republished-real-export"
    );
    let reincarnated = swarm_store::cell(&directory, "different-real-incarnation").unwrap();
    assert!(
        reincarnated.snapshot().is_err(),
        "a new graph incarnation must not inherit a cached snapshot"
    );
    assert!(Mask::new(&["not an iri".into()]).is_err());
    assert!(swarm_read::query(
        &snapshot,
        &unmasked,
        "SELECT * WHERE { SERVICE <https://example.invalid> { ?s ?p ?o } }",
        10
    )
    .is_err());
    let mut changed_source = fixture_spec();
    changed_source.manifest_sha256 = "changed-source".into();
    assert!(Snapshot::load(&changed_source)
        .err()
        .unwrap()
        .contains("manifest changed"));
    let mut nonfinite = first.vector.clone();
    nonfinite[0] = f32::NAN;
    assert!(swarm_store::normalize(&mut nonfinite).is_err());
}

#[test]
fn swarm_mcp_names_have_dispatch_catalog_scope_and_cell_policy() {
    let catalog: Value = serde_json::from_str(include_str!("mcp_tool_catalog.json")).unwrap();
    let policy: Value =
        serde_json::from_str(include_str!("cell_graph_boundary_policy.json")).unwrap();
    for name in [
        "swarm_load",
        "swarm_query",
        "swarm_save_query",
        "swarm_query_named",
        "swarm_semantic_search",
        "swarm_traverse",
    ] {
        assert!(crate::mcp_dispatch_registry::lookup(name).is_some());
        assert!(catalog["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == name));
        // Local profiles only: denied in cell mode (swarm_load reads host paths).
        assert!(policy["mcpProfileDenied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == name));
        assert!(!crate::loopback_scopes::mcp_tool_scopes(name).is_empty());
    }
}

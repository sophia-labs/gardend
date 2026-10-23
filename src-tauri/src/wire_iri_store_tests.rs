//! G3's store tests for wires that reach objects and remember their scene (INTERFACES.md). Owned by G3.
//!
//! A wire used to join documents and blocks. A scene wire may also end at an object IRI (a typed object on a
//! board as a card) and remembers the board and arrow it was drawn from. These tests drive the real `create_wires`,
//! `get_wires` and `traverse_wires` through the dispatch table (the path a live `tools/call` takes), the real
//! `scene_wire::create_scene_wires` writer the whiteboard's `wire` move calls, the real workspace Y.Doc snapshot
//! and the real RDF projection, against a disposable graph. No mocks.
use std::sync::Arc;

use serde_json::{json, Value};

use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};
use crate::scene_wire::{create_scene_wires, SceneWireDraft, SceneWireEnd, SceneWireProvenance};

const GRAPH: &str = "wb-graph";
const BOARD: &str = "wb-board";
const OBJECT: &str = "urn:mnemosyne:local:graph:wb-graph:dream:wolf-man";

// ---------------------------------------------------------------------------
// The disposable graph
// ---------------------------------------------------------------------------

struct Env {
    app: crate::app_runtime::AppHandle,
    jobs: Arc<LocalJobRegistry>,
}

impl Env {
    /// Dispatch through the real registry entry.
    fn call(&self, name: &str, mut args: Value) -> Result<Value, String> {
        if args.get("graphId").is_none() {
            args["graphId"] = json!(GRAPH);
        }
        let entry = lookup(name).unwrap_or_else(|| panic!("dispatch entry for {name}"));
        let ctx = McpCallCtx {
            app: self.app.clone(),
            jobs: &self.jobs,
        };
        run((entry.handler)(&ctx, &args)).map_err(|error| error.to_string())
    }
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    crate::app_runtime::async_runtime::block_on(future)
}

fn with_env(test: impl FnOnce(&Env)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-wire-iri-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir =
        std::env::temp_dir().join(format!("garden-wire-iri-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Wires that reach objects".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let jobs = Arc::new(LocalJobRegistry::new(jobs_dir.clone()).expect("job registry"));
        test(&Env { app, jobs });
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    let _ = std::fs::remove_dir_all(&jobs_dir);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn create(env: &Env, document_id: &str) {
    run(
        crate::mcp_workspace_crdt_service::mcp_local_create_document(
            env.app.clone(),
            &json!({"graphId": GRAPH, "documentId": document_id, "title": document_id}),
        ),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Reading back: the stored snapshot and the RDF
// ---------------------------------------------------------------------------

/// The `wires` of the workspace snapshot (what `get_wires` and every projection read).
fn snapshot_wires(env: &Env) -> Vec<Value> {
    let graph_dir = crate::paths::existing_graph_dir(&env.app, GRAPH).unwrap();
    let workspace = crate::document_service::read_workspace_record(&graph_dir, GRAPH).unwrap();
    workspace
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.get("wires"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn snapshot_wire(env: &Env, wire_id: &str) -> Value {
    snapshot_wires(env)
        .into_iter()
        .find(|wire| wire["id"] == wire_id)
        .unwrap_or_else(|| panic!("no wire {wire_id} in the snapshot"))
}

fn wire_subject(wire_id: &str) -> String {
    format!("urn:mnemosyne:local:graph:{GRAPH}:wire:{wire_id}")
}

/// `?p ?o` of the wire's subject, anywhere in the store (wrapped in `GRAPH ?g`: the cell's store is
/// partitioned into named graphs), as `(predicate IRI, object term with its N-Triples quoting stripped)`.
fn wire_triples(env: &Env, wire_id: &str) -> Vec<(String, String)> {
    let subject = wire_subject(wire_id);
    let result = run(crate::rdf_service::mcp_local_sparql_query(
        env.app.clone(),
        &json!({"graphId": GRAPH, "query": format!(
            "SELECT ?p ?o WHERE {{ GRAPH ?g {{ <{subject}> ?p ?o }} }}")}),
    ))
    .unwrap();
    let mut rows: Vec<(String, String)> = result["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("SELECT answered no rows array: {result}"))
        .iter()
        .map(|row| {
            let term = |key: &str| {
                row[key]
                    .as_str()
                    .unwrap()
                    .trim_matches(|c| matches!(c, '<' | '>' | '"'))
                    .to_string()
            };
            (term("p"), term("o"))
        })
        .collect();
    rows.sort();
    rows
}

fn objects_of(triples: &[(String, String)], predicate_name: &str) -> Vec<String> {
    let predicate = format!("http://mnemosyne.ai/vocab#{predicate_name}");
    triples
        .iter()
        .filter(|(p, _)| *p == predicate)
        .map(|(_, o)| o.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Drafts, as the whiteboard's `wire` move builds them
// ---------------------------------------------------------------------------

fn document_end(document_id: &str) -> SceneWireEnd {
    SceneWireEnd {
        document_id: Some(document_id.to_string()),
        block_id: None,
        iri: None,
        graph_id: None,
        label: None,
    }
}

fn object_end(iri: &str, label: &str) -> SceneWireEnd {
    SceneWireEnd {
        document_id: None,
        block_id: None,
        iri: Some(iri.to_string()),
        graph_id: None,
        label: Some(label.to_string()),
    }
}

fn draft(wire_id: &str, source: SceneWireEnd, target: SceneWireEnd) -> SceneWireDraft {
    SceneWireDraft {
        wire_id: wire_id.to_string(),
        predicate: "supports".to_string(),
        source,
        target,
        scene: SceneWireProvenance {
            graph_id: GRAPH.to_string(),
            document_id: BOARD.to_string(),
            element_id: format!("arw-{wire_id}"),
            source_element_id: "lnk0000000000000000001".to_string(),
            target_element_id: "crd0000000000000000001".to_string(),
            stable_key: format!("wb:{BOARD}:arw-{wire_id}"),
        },
    }
}

fn write_drafts(
    env: &Env,
    drafts: &[SceneWireDraft],
) -> Result<Vec<crate::scene_wire::SceneWireCreated>, String> {
    run(create_scene_wires(&env.app, GRAPH, drafts))
}

// ---------------------------------------------------------------------------
// G3
// ---------------------------------------------------------------------------

/// `create_wires` dropped every `scene*` field (only the REST route kept them). They are stored now: the
/// tool's answer, the stored snapshot and the RDF all carry them, and a board lists the wires drawn on it.
/// A plain wire stores and projects none of the new fields.
#[test]
fn create_wires_keeps_scene_provenance() {
    with_env(|env| {
        create(env, "doc-a");
        create(env, "doc-b");
        let created = env
            .call(
                "create_wires",
                json!({"wires": [{
                    "wireId": "wire-scene", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b",
                    "predicate": "supports",
                    "sceneGraphId": GRAPH, "sceneDocumentId": BOARD,
                    "sceneElementId": "arw0000000000000000002",
                    "sceneSourceElementId": "lnk0000000000000000001",
                    "sceneTargetElementId": "crd0000000000000000001",
                    "sceneStableKey": "wb:wb-board:arw0000000000000000002",
                }, {
                    "wire_id": "wire-snake", "source_document_id": "doc-b", "target_document_id": "doc-a",
                    "scene_document_id": BOARD, "scene_element_id": "arw-snake",
                }, {
                    "wireId": "wire-plain", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-b",
                    "predicate": "supports",
                }]}),
            )
            .unwrap();
        assert_eq!(created["count"], 3);

        // The stored snapshot carries all six, and the board.
        let stored = snapshot_wire(env, "wire-scene");
        for (key, value) in [
            ("sceneGraphId", GRAPH),
            ("sceneDocumentId", BOARD),
            ("sceneElementId", "arw0000000000000000002"),
            ("sceneSourceElementId", "lnk0000000000000000001"),
            ("sceneTargetElementId", "crd0000000000000000001"),
            ("sceneStableKey", "wb:wb-board:arw0000000000000000002"),
        ] {
            assert_eq!(stored[key], value, "snapshot {key}: {stored}");
        }
        // The snake_case twins are read too.
        let snake = snapshot_wire(env, "wire-snake");
        assert_eq!(snake["sceneDocumentId"], BOARD);
        assert_eq!(snake["sceneElementId"], "arw-snake");

        // get_wires answers the arrow and the board.
        let got = env
            .call(
                "get_wires",
                json!({"documentId": "doc-a", "direction": "outgoing"}),
            )
            .unwrap();
        let row = got["wires"]
            .as_array()
            .unwrap()
            .iter()
            .find(|wire| wire["id"] == "wire-scene")
            .unwrap_or_else(|| panic!("get_wires lost the wire: {got}"));
        assert_eq!(row["sceneDocumentId"], BOARD);
        assert_eq!(row["sceneElementId"], "arw0000000000000000002");

        // The RDF carries them under their wire: predicates.
        let triples = wire_triples(env, "wire-scene");
        assert_eq!(objects_of(&triples, "sceneGraphId"), vec![GRAPH]);
        assert_eq!(objects_of(&triples, "sceneDocumentId"), vec![BOARD]);
        assert_eq!(
            objects_of(&triples, "sceneElementId"),
            vec!["arw0000000000000000002"]
        );
        assert_eq!(
            objects_of(&triples, "sceneSourceElementId"),
            vec!["lnk0000000000000000001"]
        );
        assert_eq!(
            objects_of(&triples, "sceneTargetElementId"),
            vec!["crd0000000000000000001"]
        );
        assert_eq!(
            objects_of(&triples, "sceneStableKey"),
            vec!["wb:wb-board:arw0000000000000000002"]
        );

        // A board lists the wires drawn on it (under "both" only, which is the default).
        let board = env.call("get_wires", json!({"documentId": BOARD})).unwrap();
        let ids: Vec<&str> = board["wires"]
            .as_array()
            .unwrap()
            .iter()
            .map(|wire| wire["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids.len(),
            2,
            "the board lists its two arrows, not the plain wire: {board}"
        );
        assert!(ids.contains(&"wire-scene") && ids.contains(&"wire-snake"));
        let outgoing = env
            .call(
                "get_wires",
                json!({"documentId": BOARD, "direction": "outgoing"}),
            )
            .unwrap();
        assert_eq!(
            outgoing["count"], 0,
            "an arrow is not an edge out of the board"
        );

        // Regression: a document-to-document wire stores and projects none of the new fields.
        let plain = snapshot_wire(env, "wire-plain");
        for key in ["sourceIri", "targetIri", "sceneDocumentId"] {
            assert!(plain.get(key).is_none(), "{key} on a plain wire: {plain}");
        }
        assert_eq!(plain["sourceDocumentId"], "doc-a");
        assert_eq!(plain["targetDocumentId"], "doc-b");
        let triples = wire_triples(env, "wire-plain");
        for name in ["sourceResource", "targetResource", "sceneDocumentId"] {
            assert!(
                objects_of(&triples, name).is_empty(),
                "wire:{name} on a plain wire"
            );
        }
        assert_eq!(
            objects_of(&triples, "targetDocument"),
            vec!["urn:mnemosyne:local:document:doc-b"]
        );
    });
}

/// A scene wire with a document source and an object target: the snapshot has the IRI and no target document,
/// the RDF has `wire:targetResource` and no `wire:targetDocument`, and the REST summary shows the object as
/// `other_iri`. Replaying the same draft leaves one wire.
#[test]
fn wire_to_an_object_iri_round_trips() {
    with_env(|env| {
        create(env, "doc-traumdeutung");
        create(env, BOARD);
        let drafts = [draft(
            "wire-wb-0001",
            document_end("doc-traumdeutung"),
            object_end(OBJECT, "The Wolf Man dream"),
        )];
        let created = write_drafts(env, &drafts).unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].wire_id, "wire-wb-0001");
        assert_eq!(created[0].predicate, "supports");

        let stored = snapshot_wire(env, "wire-wb-0001");
        assert_eq!(stored["sourceDocumentId"], "doc-traumdeutung");
        assert_eq!(stored["targetIri"], OBJECT);
        assert!(
            stored["targetDocumentId"].is_null(),
            "an IRI end has no document id: {stored}"
        );
        assert!(stored["targetBlockId"].is_null());
        assert_eq!(stored["targetTitle"], "The Wolf Man dream");
        assert_eq!(stored["sceneDocumentId"], BOARD);
        assert_eq!(stored["sceneElementId"], "arw-wire-wb-0001");

        let triples = wire_triples(env, "wire-wb-0001");
        assert_eq!(objects_of(&triples, "targetResource"), vec![OBJECT]);
        assert!(
            objects_of(&triples, "targetDocument").is_empty()
                && objects_of(&triples, "targetBlock").is_empty(),
            "an IRI end never produces a wire:target document or block triple: {triples:?}"
        );
        assert_eq!(
            objects_of(&triples, "sourceDocument"),
            vec!["urn:mnemosyne:local:document:doc-traumdeutung"]
        );
        assert_eq!(objects_of(&triples, "sceneDocumentId"), vec![BOARD]);

        // The REST summary (the bundle the app reads) shows the object end.
        let outgoing = crate::hosted_wire_projection::hosted_wires_for_document(
            env.app.clone(),
            GRAPH,
            "doc-traumdeutung",
            "outgoing",
        )
        .unwrap();
        let entry = &outgoing[0];
        assert_eq!(entry["other_iri"], OBJECT);
        assert_eq!(entry["other_document_id"], "");
        assert_eq!(entry["other_title"], "The Wolf Man dream");
        assert_eq!(entry["scene_document_id"], BOARD);
        assert_eq!(entry["scene_element_id"], "arw-wire-wb-0001");

        // Replaying the same draft (same wire id) replaces, not duplicates.
        write_drafts(env, &drafts).unwrap();
        assert_eq!(snapshot_wires(env).len(), 1, "a replay leaves one wire");
        assert_eq!(
            objects_of(&wire_triples(env, "wire-wb-0001"), "targetResource"),
            vec![OBJECT]
        );

        // An object SOURCE: the board carries the queue operation and is not stored as the source.
        write_drafts(
            env,
            &[draft(
                "wire-wb-0002",
                object_end(OBJECT, "The Wolf Man dream"),
                document_end("doc-traumdeutung"),
            )],
        )
        .unwrap();
        let from_object = snapshot_wire(env, "wire-wb-0002");
        assert_eq!(from_object["sourceIri"], OBJECT);
        assert!(from_object["sourceDocumentId"].is_null(), "{from_object}");
        assert_eq!(from_object["sourceTitle"], "The Wolf Man dream");
        let triples = wire_triples(env, "wire-wb-0002");
        assert_eq!(objects_of(&triples, "sourceResource"), vec![OBJECT]);
        assert!(objects_of(&triples, "sourceDocument").is_empty());
        let incoming = crate::hosted_wire_projection::hosted_wires_for_document(
            env.app.clone(),
            GRAPH,
            "doc-traumdeutung",
            "incoming",
        )
        .unwrap();
        let entry = incoming
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == "wire-wb-0002")
            .expect("incoming from the object");
        assert_eq!(entry["other_iri"], OBJECT);
        assert_eq!(entry["other_document_id"], "");
    });
}

/// `get_wires {iri}` answers the wires that start or end at the object; exactly one of `documentId` and `iri`.
#[test]
fn get_wires_by_iri() {
    with_env(|env| {
        create(env, "doc-a");
        create(env, "doc-b");
        create(env, BOARD);
        write_drafts(
            env,
            &[
                draft(
                    "wire-into",
                    document_end("doc-a"),
                    object_end(OBJECT, "The Wolf Man dream"),
                ),
                draft(
                    "wire-out-of",
                    object_end(OBJECT, "The Wolf Man dream"),
                    document_end("doc-b"),
                ),
                draft(
                    "wire-elsewhere",
                    document_end("doc-a"),
                    document_end("doc-b"),
                ),
            ],
        )
        .unwrap();

        let ids = |result: &Value| -> Vec<String> {
            let mut ids: Vec<String> = result["wires"]
                .as_array()
                .unwrap()
                .iter()
                .map(|wire| wire["id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        let both = env.call("get_wires", json!({"iri": OBJECT})).unwrap();
        assert_eq!(ids(&both), vec!["wire-into", "wire-out-of"]);
        assert_eq!(both["iri"], OBJECT);
        assert_eq!(both["count"], 2);
        let incoming = env
            .call("get_wires", json!({"iri": OBJECT, "direction": "incoming"}))
            .unwrap();
        assert_eq!(ids(&incoming), vec!["wire-into"]);
        let row = &incoming["wires"][0];
        assert_eq!(row["targetIri"], OBJECT);
        assert!(row["targetDocumentId"].is_null());
        assert_eq!(row["sourceDocumentId"], "doc-a");
        assert_eq!(row["sceneDocumentId"], BOARD);
        let outgoing = env
            .call("get_wires", json!({"iri": OBJECT, "direction": "outgoing"}))
            .unwrap();
        assert_eq!(ids(&outgoing), vec!["wire-out-of"]);
        assert_eq!(outgoing["wires"][0]["sourceIri"], OBJECT);

        // An object nothing points at has no wires; a document is still asked the old way.
        let none = env
            .call(
                "get_wires",
                json!({"iri": "urn:mnemosyne:local:graph:wb-graph:dream:nobody"}),
            )
            .unwrap();
        assert_eq!(none["count"], 0);
        let doc = env
            .call("get_wires", json!({"documentId": "doc-a"}))
            .unwrap();
        assert_eq!(doc["document_id"], "doc-a");
        assert_eq!(ids(&doc), vec!["wire-elsewhere", "wire-into"]);

        // Exactly one of the two.
        let both_given = env
            .call("get_wires", json!({"iri": OBJECT, "documentId": "doc-a"}))
            .unwrap_err();
        assert!(
            both_given.contains("exactly one of documentId and iri"),
            "{both_given}"
        );
        let neither = env.call("get_wires", json!({})).unwrap_err();
        assert!(
            neither.contains("exactly one of documentId and iri"),
            "{neither}"
        );
    });
}

/// doc A -> object O -> doc B: a walk from A reaches O and B; a walk from O reaches A and B; a hop through the
/// object continues the walk.
#[test]
fn traverse_wires_crosses_an_object() {
    with_env(|env| {
        create(env, "doc-a");
        create(env, "doc-b");
        create(env, BOARD);
        write_drafts(
            env,
            &[
                draft(
                    "wire-a-o",
                    document_end("doc-a"),
                    object_end(OBJECT, "The Wolf Man dream"),
                ),
                draft(
                    "wire-o-b",
                    object_end(OBJECT, "The Wolf Man dream"),
                    document_end("doc-b"),
                ),
            ],
        )
        .unwrap();

        // From A, depth 2: O at depth 1, B at depth 2.
        let from_a = env
            .call(
                "traverse_wires",
                json!({"documentId": "doc-a", "direction": "outgoing", "maxDepth": 2}),
            )
            .unwrap();
        let depth_of = |result: &Value, key: &str, value: &str| -> Option<u64> {
            result["documents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry[key] == value)
                .and_then(|entry| entry["depth"].as_u64())
        };
        assert_eq!(depth_of(&from_a, "iri", OBJECT), Some(1), "{from_a}");
        assert_eq!(
            depth_of(&from_a, "documentId", "doc-b"),
            Some(2),
            "{from_a}"
        );
        assert_eq!(depth_of(&from_a, "documentId", "doc-a"), Some(0));
        let object_entry = from_a["documents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["iri"] == OBJECT)
            .unwrap();
        assert!(object_entry["documentId"].is_null());
        assert_eq!(object_entry["title"], "The Wolf Man dream");
        let path_to_b = from_a["paths"]
            .as_array()
            .unwrap()
            .iter()
            .find(|path| path["documentId"] == "doc-b")
            .expect("a path to B");
        let hops = path_to_b["hops"].as_array().unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0]["wireId"], "wire-a-o");
        assert_eq!(hops[0]["toIri"], OBJECT);
        assert_eq!(hops[1]["wireId"], "wire-o-b");
        assert_eq!(hops[1]["fromIri"], OBJECT);
        assert_eq!(hops[1]["toDocumentId"], "doc-b");

        // Depth 1 stops at the object.
        let shallow = env
            .call(
                "traverse_wires",
                json!({"documentId": "doc-a", "maxDepth": 1}),
            )
            .unwrap();
        assert_eq!(depth_of(&shallow, "iri", OBJECT), Some(1));
        assert_eq!(depth_of(&shallow, "documentId", "doc-b"), None);

        // From the object, both directions: A and B at depth 1.
        let from_o = env
            .call("traverse_wires", json!({"iri": OBJECT, "maxDepth": 2}))
            .unwrap();
        assert_eq!(from_o["iri"], OBJECT);
        assert!(from_o["documentId"].is_null());
        assert_eq!(
            depth_of(&from_o, "documentId", "doc-a"),
            Some(1),
            "{from_o}"
        );
        assert_eq!(
            depth_of(&from_o, "documentId", "doc-b"),
            Some(1),
            "{from_o}"
        );
        assert_eq!(depth_of(&from_o, "iri", OBJECT), Some(0));

        // Exactly one start.
        let both = env
            .call(
                "traverse_wires",
                json!({"iri": OBJECT, "documentId": "doc-a"}),
            )
            .unwrap_err();
        assert!(
            both.contains("exactly one of document_id and iri"),
            "{both}"
        );
        assert!(env.call("traverse_wires", json!({})).is_err());
    });
}

/// One bad draft among three writes none: the validation is of the whole set, before the first write.
#[test]
fn scene_wire_drafts_are_validated_whole() {
    with_env(|env| {
        create(env, "doc-a");
        create(env, "doc-b");
        create(env, BOARD);
        let good = |id: &str| draft(id, document_end("doc-a"), document_end("doc-b"));

        // An end with both a document and an IRI is the bad one, in the middle.
        let mut both = good("wire-bad");
        both.target.iri = Some(OBJECT.to_string());
        let error = write_drafts(env, &[good("wire-1"), both, good("wire-3")]).unwrap_err();
        assert!(
            error.contains("wire 2 (wire-bad)") && error.contains("not both"),
            "{error}"
        );
        assert!(
            snapshot_wires(env).is_empty(),
            "nothing was written: {:?}",
            snapshot_wires(env)
        );
        assert_eq!(
            env.call("get_wires", json!({"documentId": "doc-a"}))
                .unwrap()["count"],
            0
        );

        // The last one has no predicate.
        let mut mute = good("wire-3");
        mute.predicate = String::new();
        let error = write_drafts(env, &[good("wire-1"), good("wire-2"), mute]).unwrap_err();
        assert!(error.contains("predicate is empty"), "{error}");
        assert!(snapshot_wires(env).is_empty());

        // An id the projection could not name would be refused only after the earlier drafts were committed;
        // it is caught first.
        let mut spaced = good("wire 2");
        spaced.wire_id = "wire with a space".to_string();
        let error = write_drafts(env, &[good("wire-1"), spaced, good("wire-3")]).unwrap_err();
        assert!(error.contains("is not an IRI"), "{error}");
        assert!(snapshot_wires(env).is_empty());

        // An IRI that is not absolute, on the first.
        let mut relative = good("wire-1");
        relative.target = SceneWireEnd {
            document_id: None,
            block_id: None,
            iri: Some("wolf-man".to_string()),
            graph_id: None,
            label: None,
        };
        let error = write_drafts(env, &[relative, good("wire-2")]).unwrap_err();
        assert!(error.contains("not an absolute IRI"), "{error}");
        assert!(snapshot_wires(env).is_empty());

        // The same three, all good, are all written, in order.
        let created = write_drafts(env, &[good("wire-1"), good("wire-2"), good("wire-3")]).unwrap();
        assert_eq!(
            created
                .iter()
                .map(|c| c.wire_id.as_str())
                .collect::<Vec<_>>(),
            vec!["wire-1", "wire-2", "wire-3"]
        );
        assert_eq!(snapshot_wires(env).len(), 3);
        // And an empty set writes nothing and answers nothing.
        assert!(write_drafts(env, &[]).unwrap().is_empty());
    });
}

#[test]
fn create_wires_refuses_missing_ends_blocks_and_empty_predicates_before_writing() {
    with_env(|env| {
        create(env, "doc-a");
        create(env, "doc-b");
        let (provider, exporter, dispatch) = crate::telemetry::test_support::capture(true);
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!(target: "gardend_otel", "wire.validation");
            span.in_scope(|| {
                for extra in [
                    json!({"targetDocumentId": "doc-nope-000"}),
                    json!({"sourceDocumentId": "doc-nope-000"}),
                    json!({"sourceBlockId": "block-99999999"}),
                    json!({"targetBlockId": "block-99999999"}),
                    json!({"targetGraphId": "missing-graph"}),
                    json!({"predicate": ""}),
                    json!({"predicate": "   "}),
                ] {
                    let mut args =
                        json!({"sourceDocumentId": "doc-a", "targetDocumentId": "doc-b"});
                    for (key, value) in extra.as_object().unwrap() {
                        args[key] = value.clone();
                    }
                    let error = env.call("create_wires", args).unwrap_err();
                    assert!(
                        error.contains("not found")
                            || error.contains("does not exist")
                            || error.contains("predicate must be"),
                        "{error}"
                    );
                }
            });
        });
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        assert!(spans.iter().any(|span| span.name == "wire.validation"));
        assert_eq!(
            spans
                .iter()
                .flat_map(|span| span.events.events.iter())
                .filter(|event| event.name == "fsync")
                .count(),
            0
        );
        assert!(snapshot_wires(env).is_empty());
    });
}

#[test]
fn create_wires_checks_typed_objects_and_allows_vocabulary_self_wires() {
    with_env(|env| {
        create(env, "doc-a");
        let args = json!({"sourceDocumentId": "doc-a", "targetIri": OBJECT});
        assert!(env
            .call("create_wires", args.clone())
            .unwrap_err()
            .contains("typed object not found"));
        env.call("sparql_update", json!({"update": format!(
            "INSERT DATA {{ GRAPH <urn:wire-test:objects> {{ <{OBJECT}> a <urn:wire-test:Dream> }} }}"
        )})).unwrap();
        env.call("create_wires", args).unwrap();
        env.call(
            "create_wires",
            json!({"sourceDocumentId": "doc-a", "targetDocumentId": "doc-a"}),
        )
        .unwrap();
        let got = env
            .call("get_wires", json!({"documentId": "doc-a"}))
            .unwrap();
        assert_eq!(got["count"], 2, "{got}");
        assert_eq!(got["ghostWireCount"], 0, "{got}");
    });
}

#[test]
fn get_wires_counts_historical_ghosts_without_deleting_them() {
    with_env(|env| {
        create(env, "doc-a");
        // The old queue/storage shape still represents dangling wires; the
        // stricter authoring door must never rewrite that history on a read.
        run(crate::crdt_queue::enqueue_crdt_operation(env.app.clone(), crate::crdt_queue::EnqueueCrdtOperationInput {
            kind: "workspace.createWire".into(), graph_id: GRAPH.into(), document_id: Some("doc-a".into()),
            payload: json!({"wireId": "historical-ghost", "sourceDocumentId": "doc-a", "targetDocumentId": "doc-nope-000"}),
        })).unwrap();
        let before = snapshot_wires(env);
        for _ in 0..2 {
            let got = env
                .call("get_wires", json!({"documentId": "doc-a"}))
                .unwrap();
            assert_eq!(got["ghostWireCount"], 1, "{got}");
            assert_eq!(got["count"], 1, "{got}");
        }
        assert_eq!(snapshot_wires(env), before);
    });
}

#[test]
fn create_wires_accepts_real_blocks_and_checks_cross_graph_document_membership() {
    with_env(|env| {
        create(env, "doc-a");
        let other = "wire-target-graph";
        crate::graph_service::create_graph_service(
            &env.app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(other.into()),
                title: "Target graph".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        for (graph, doc, block) in [
            (GRAPH, "doc-a", "source-block"),
            (other, "doc-b", "target-block"),
        ] {
            run(crate::crdt_queue::enqueue_crdt_operation(env.app.clone(), crate::crdt_queue::EnqueueCrdtOperationInput {
                kind: "document.write".into(), graph_id: graph.into(), document_id: Some(doc.into()),
                payload: json!({"title": doc, "tiptapJson": {"type": "doc", "content": [{
                    "type": "paragraph", "attrs": {"data-block-id": block}, "content": [{"type": "text", "text": "Wired text."}]
                }]}}),
            })).unwrap();
        }
        let args = json!({"sourceDocumentId": "doc-a", "sourceBlockId": "source-block",
            "targetGraphId": other, "targetDocumentId": "doc-b", "targetBlockId": "target-block"});
        env.call("create_wires", args.clone()).unwrap();
        let mut wrong_graph = args.clone();
        wrong_graph["targetGraphId"] = json!(GRAPH);
        assert!(env.call("create_wires", wrong_graph).is_err());
        let mut wrong_block = args;
        wrong_block["sourceBlockId"] = json!("target-block");
        assert!(env.call("create_wires", wrong_block).is_err());
        let got = env
            .call("get_wires", json!({"documentId": "doc-a"}))
            .unwrap();
        assert_eq!(got["count"], 1, "{got}");
        assert_eq!(got["ghostWireCount"], 0, "{got}");
    });
}

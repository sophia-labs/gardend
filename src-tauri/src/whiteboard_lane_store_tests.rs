//! G2's store tests for the board lane (INTERFACES.md): a real graph, real boards, persistence tails, restore. Owned by G2.
//!
//! Each test builds a disposable graph in a temp profile with live `Room`s, draws the shared v2 fixture into a
//! board the way a browser's y-websocket client does (one stringified element per key of the `elements` map),
//! persists it through the same `persist_room_document` path a person's edit takes, and reads the graph's
//! `:projection:whiteboard` lane back from the real store. The delete, restore and cold-seed tests run the real
//! services. No mocks.
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxigraph::model::GraphName;
use serde_json::{json, Value};

use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::emporium::terms::Triple;
use crate::local_jobs::LocalJobRegistry;
use crate::mcp_dispatch_registry::{lookup, McpCallCtx};
use crate::whiteboard::lane;
use crate::whiteboard::ops::write_dirty;
use crate::whiteboard::scene::{Element, Scene};

const GRAPH: &str = "wb-graph";

const FIXTURE_V2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/whiteboard-fixture-v2-semantic.json"
));

const BOX: &str = "box0000000000000000001";
const CRD: &str = "crd0000000000000000001";
const NOTE_TEXT: &str = "txt0000000000000000002";

// ---------------------------------------------------------------------------
// The disposable graph
// ---------------------------------------------------------------------------

struct Env {
    app: crate::app_runtime::AppHandle,
    jobs: Arc<LocalJobRegistry>,
}

impl Env {
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

    fn graph_dir(&self) -> std::path::PathBuf {
        crate::graph_paths::existing_graph_dir(&self.app, GRAPH).unwrap()
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
    let profile = std::env::temp_dir().join(format!("garden-wb-lane-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let jobs_dir =
        std::env::temp_dir().join(format!("garden-wb-lane-jobs-{}", uuid::Uuid::new_v4()));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Whiteboard lane specimen".into(),
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

// ---------------------------------------------------------------------------
// Boards, drawing, reading the lane
// ---------------------------------------------------------------------------

fn fixture_elements() -> Vec<Element> {
    let fixture: Value = serde_json::from_str(FIXTURE_V2).expect("fixture parses");
    fixture["elements"]
        .as_array()
        .expect("elements")
        .iter()
        .map(|e| e.as_object().expect("element object").clone())
        .collect()
}

fn scene_of(elements: &[Element]) -> Scene {
    Scene::from_elements(
        elements
            .iter()
            .map(|el| (el["id"].as_str().expect("id").to_string(), el.clone()))
            .collect(),
    )
}

fn new_board(env: &Env, title: &str) -> String {
    env.call("whiteboard_create", json!({ "title": title }))
        .expect("whiteboard_create")["documentId"]
        .as_str()
        .expect("documentId")
        .to_string()
}

fn room_of(env: &Env, document_id: &str) -> Arc<Room> {
    let registry = env.app.state::<RoomRegistry>();
    run(registry.get_or_create(
        &format!("doc:{GRAPH}:{document_id}"),
        crate::ydoc_paths::document_ydoc_state_path(&env.graph_dir(), document_id),
    ))
    .unwrap()
}

/// Write `elements` into the board's live room (a person's edit) and persist it, which runs the persistence tail.
fn draw(env: &Env, document_id: &str, title: &str, elements: &[Element]) {
    let room = room_of(env, document_id);
    let mut scene = Scene::default();
    for el in elements {
        let id = el["id"].as_str().expect("id").to_string();
        scene.elements.insert(id.clone(), el.clone());
        scene.dirty.insert(id);
    }
    run(room.update_doc(|_doc, txn| {
        write_dirty(txn, &scene);
        Ok(())
    }))
    .unwrap();
    run(crate::crdt_engine::document_ops::persist_room_document(
        &env.app,
        GRAPH,
        document_id,
        title,
        &room,
        &format!("op-wb-lane-{}", uuid::Uuid::new_v4().simple()),
    ))
    .unwrap();
}

fn lines(triples: &[Triple]) -> BTreeSet<String> {
    triples
        .iter()
        .map(|(s, p, o)| format!("<{s}> <{p}> {}", o.as_nt()))
        .collect()
}

/// The lane's triples for one board, read from the real store.
fn lane_of(env: &Env, document_id: &str) -> BTreeSet<String> {
    let store = crate::rdf_store_service::open_graph_store(&env.graph_dir()).unwrap();
    lines(&lane::current_triples(&store, GRAPH, document_id).unwrap())
}

/// What the lane should hold for `elements` drawn on `document_id`.
fn expected(document_id: &str, title: &str, elements: &[Element]) -> BTreeSet<String> {
    lines(&lane::desired_triples(GRAPH, document_id, title, &scene_of(elements)).unwrap())
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn lane_graph_quads(env: &Env) -> Vec<oxigraph::model::Quad> {
    let store = crate::rdf_store_service::open_graph_store(&env.graph_dir()).unwrap();
    let iri = crate::rdf_authority::whiteboard_projection_graph_iri(GRAPH);
    store
        .iter()
        .map(Result::unwrap)
        .filter(|q| matches!(&q.graph_name, GraphName::NamedNode(n) if n.as_str() == iri))
        .collect()
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn persisting_a_board_fills_and_follows_its_lane() {
    with_env(|env| {
        let title = "Dream-work board";
        let doc = new_board(env, title);
        let mut elements = fixture_elements();
        draw(env, &doc, title, &elements);
        let lane_now = lane_of(env, &doc);
        assert_eq!(
            lane_now.len(),
            58,
            "the persistence tail ran the lane: {lane_now:#?}"
        );
        assert_eq!(lane_now, expected(&doc, title, &elements));

        // A person deletes the card and relabels the note: the tail reconciles the stale triples away.
        for el in elements.iter_mut() {
            if el["id"] == CRD {
                el.insert("isDeleted".into(), json!(true));
            }
            if el["id"] == NOTE_TEXT {
                el.insert("originalText".into(), json!("condensation (Verdichtung)"));
                el.insert("text".into(), json!("condensation (Verdichtung)"));
            }
        }
        draw(env, &doc, title, &elements);
        let after = lane_of(env, &doc);
        assert!(!after.iter().any(|l| l.contains(CRD)), "{after:#?}");
        assert!(after
            .iter()
            .any(|l| l.contains("condensation (Verdichtung)")));
        assert!(!after.iter().any(|l| l.ends_with("\"condensation\"")));
        assert_eq!(after, expected(&doc, title, &elements));
    });
}

#[test]
fn delete_board_empties_its_span() {
    with_env(|env| {
        let elements = fixture_elements();
        let gone = new_board(env, "Doomed board");
        let kept = new_board(env, "Kept board");
        draw(env, &gone, "Doomed board", &elements);
        draw(env, &kept, "Kept board", &elements);
        assert_eq!(lane_of(env, &gone).len(), 58);
        assert_eq!(lane_of(env, &kept).len(), 58);

        crate::document_delete_service::delete_document_for_operation(
            env.app.clone(),
            GRAPH.into(),
            gone.clone(),
            None,
        )
        .unwrap();

        assert!(
            lane_of(env, &gone).is_empty(),
            "the deleted board's span is empty"
        );
        let subjects: Vec<String> = lane_graph_quads(env)
            .iter()
            .map(|q| q.subject.to_string())
            .collect();
        assert!(
            !subjects
                .iter()
                .any(|s| s.contains(&format!("document:{gone}"))),
            "no lane subject of the deleted board survives: {subjects:?}"
        );
        assert!(
            subjects
                .iter()
                .any(|s| s.contains(&format!("document:{kept}"))),
            "the other board keeps its subjects"
        );
        assert_eq!(
            lane_of(env, &kept),
            expected(&kept, "Kept board", &elements),
            "the other board keeps its triples, exactly"
        );
    });
}

#[test]
fn restore_reprojects() {
    with_env(|env| {
        let title = "Restorable board";
        let doc = new_board(env, title);
        let mut elements = fixture_elements();
        draw(env, &doc, title, &elements);
        let at_the_point = lane_of(env, &doc);
        assert_eq!(at_the_point.len(), 58);

        let point = env
            .call("create_restore_point", json!({"label": "wb-lane"}))
            .unwrap();
        let restore_point = point["restorePointId"].as_str().unwrap().to_string();

        // After the point a second card goes up.
        let mut second = elements
            .iter()
            .find(|el| el["id"] == CRD)
            .expect("the fixture's card")
            .clone();
        second.insert("id".into(), json!("crd0000000000000000002"));
        second.insert("boundElements".into(), json!([]));
        second.insert(
            "link".into(),
            json!(crate::whiteboard::links::object_link(
                GRAPH,
                "urn:mnemosyne:local:graph:wb-graph:dream:rat-man"
            )),
        );
        second.insert(
            "customData".into(),
            json!({"sophia": {"by": "person", "at": 1_760_000_003_000_i64},
                   "mnemosyne": {"kind": "object", "graphId": GRAPH,
                                 "iri": "urn:mnemosyne:local:graph:wb-graph:dream:rat-man",
                                 "label": "The Rat Man dream", "classLabel": "DreamReport"}}),
        );
        elements.push(second);
        draw(env, &doc, title, &elements);
        let with_second = lane_of(env, &doc);
        assert!(
            with_second
                .iter()
                .any(|l| l.contains("crd0000000000000000002")),
            "the second card is in the lane before the restore"
        );
        assert!(with_second.len() > 58);

        crate::time_travel_restore_service::start_restore(
            &env.app,
            env.jobs.clone(),
            GRAPH,
            &restore_point,
            false,
        )
        .expect("start restore");
        wait_for("the restored lane to equal the point's", || {
            lane_of(env, &doc) == at_the_point
        });
        assert!(
            !lane_of(env, &doc)
                .iter()
                .any(|l| l.contains("crd0000000000000000002")),
            "the card drawn after the point is gone from the lane"
        );
    });
}

#[test]
fn restore_hook_runs_on_a_cold_seed() {
    with_env(|env| {
        let title = "Seeded board";
        let doc = new_board(env, title);
        let elements = fixture_elements();
        draw(env, &doc, title, &elements);
        let warm = lane_of(env, &doc);
        assert_eq!(warm.len(), 58);

        // A genuinely cold lane: no lane graph and no seed marker, as after a pod restart onto an older store.
        let dir = env.graph_dir();
        let store = crate::rdf_store_service::open_graph_store(&dir).unwrap();
        let seed = crate::rdf_authority::seed_marker_graph_iri(GRAPH);
        let lane_iri = crate::rdf_authority::whiteboard_projection_graph_iri(GRAPH);
        let to_remove: Vec<_> = store
            .iter()
            .map(Result::unwrap)
            .filter(|q| {
                matches!(&q.graph_name, GraphName::NamedNode(n)
                    if n.as_str() == lane_iri || n.as_str() == seed)
            })
            .collect();
        for quad in to_remove {
            store.remove(&quad).unwrap();
        }
        assert!(
            lane_of(env, &doc).is_empty(),
            "the lane is cold before the seed"
        );

        crate::rdf_seed_service::forget_process_seed_marker_for_test(&dir.join("store.oxigraph"));
        crate::rdf_seed_service::ensure_graph_store_seeded(&dir).unwrap();

        assert_eq!(
            lane_of(env, &doc),
            warm,
            "the seed's document walk refilled the lane from the Y.Doc sidecar"
        );
        assert_eq!(lane_of(env, &doc), expected(&doc, title, &elements));
    });
}

#[test]
fn a_user_write_into_the_lane_graph_is_refused_and_changes_nothing() {
    with_env(|env| {
        let title = "Guarded board";
        let doc = new_board(env, title);
        let elements = fixture_elements();
        draw(env, &doc, title, &elements);
        let before = lane_of(env, &doc);

        let lane_iri = crate::rdf_authority::whiteboard_projection_graph_iri(GRAPH);
        let update = format!(
            "INSERT DATA {{ GRAPH <{lane_iri}> {{ <urn:mnemosyne:local:document:{doc}#block-{BOX}> \
             <http://www.w3.org/2000/01/rdf-schema#label> \"forged\" }} }}"
        );
        let refused = env.call("sparql_update", json!({"update": update}));
        assert!(refused.is_err(), "the authority gate refuses: {refused:?}");
        assert_eq!(lane_of(env, &doc), before, "the lane is untouched");
    });
}
